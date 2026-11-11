//! 原生终端模型：把字节流交给 Alacritty 的状态机，产出有界快照。
//!
//! 这一层不碰 SSH、不碰 GPUI：调用方从 `ShellEventStream` 拿到字节，按批喂进来；
//! 绘制层按需取可见区快照。回滚、备用屏、UTF-8 跨块、宽字符都由 Alacritty 处理。
//!
//! 注意：Alacritty 的 `Term` 不是 `Send`，解析必须留在同一个线程/任务里；
//! 这也是后续专用解析线程的边界。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub mod osc;

pub use osc::{
    MAX_CWD_BYTES, MAX_OSC_PAYLOAD, MAX_OSC_STREAM_BYTES, MarkFamily, OscGuard, OscMark,
    OscScanner, ShellMarkKind,
};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::search::{RegexIter, RegexSearch};
use alacritty_terminal::term::{Config, Osc52, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, Processor};

pub use alacritty_terminal::vte::ansi::{Color as TerminalColor, NamedColor};

/// 远端的鼠标上报模式。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseMode {
    /// 1000：只报按下/抬起。
    pub click: bool,
    /// 1002：按住拖动也报。
    pub drag: bool,
    /// 1003：不按键的移动也报。
    pub motion: bool,
    /// 1006：SGR 扩展坐标。
    pub sgr: bool,
}

impl MouseMode {
    pub fn reports_buttons(&self) -> bool {
        self.click || self.drag || self.motion
    }

    pub fn reports_motion(&self) -> bool {
        self.drag || self.motion
    }
}

/// 终端尺寸（行、列）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub rows: usize,
    pub cols: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// 可见区的一行；`text` 已去掉尾部空格，`cells` 保留逐格属性。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowSnapshot {
    pub text: String,
    pub cells: Vec<CellSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellSnapshot {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub inverse: bool,
    pub hidden: bool,
    /// 组合字符（零宽），例如 `e` + U+0301。
    pub zerowidth: Option<Box<[char]>>,
    /// OSC 8 超链接的 URI。
    pub hyperlink: Option<Arc<str>>,
    /// 宽字符右侧的占位格：不参与文本复制。
    pub spacer: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub size: Size,
    pub rows: Vec<RowSnapshot>,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub history_size: usize,
    /// 可见内容的修订号：喂了输出 / 本地文字、改了尺寸或回滚上限、滚动，都会让它变大。同一个终端里
    /// 修订号相同就代表可见内容一样（异步高亮用它认出"扫描的是不是眼前这一屏"）。
    pub revision: u64,
    /// 快照取的时候滚回历史的行数。
    pub display_offset: usize,
}

/// 一条 shell 标记发生时的光标位置（屏幕坐标，不受滚回历史影响）。
///
/// 输入跟踪用它认出"Prompt 输入起点"：收到 `133;B` 那一刻的光标就是用户输入的第一个格子。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorAnchor {
    pub row: usize,
    pub col: usize,
    pub alt_screen: bool,
}

/// 终端模型识别出的一条 shell 标记，连同它在流里的位置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellEvent {
    pub mark: OscMark,
    /// 这条序列结尾之后，累计喂进来的远端字节数（`feed_local` 的文字不计）。单调递增，
    /// 用来把事件和"之后某一批字节"排先后。
    pub stream_pos: u64,
    /// 序列结尾那一刻的光标位置。
    pub anchor: CursorAnchor,
}

/// 标记队列的条数上限。调用方每批输出后取一次；满了丢最旧的并计数。
pub const MAX_PENDING_MARKS: usize = 256;

/// 一格输入行单元：保留尾部空格和组合字符，宽字符右侧的占位格单独标出。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineCell {
    pub ch: char,
    pub zerowidth: Option<Box<[char]>>,
    pub kind: LineCellKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineCellKind {
    Normal,
    /// 宽字符本体（占两格，右边紧跟一个 `WideSpacer`）。
    Wide,
    /// 宽字符右侧的占位格。
    WideSpacer,
    /// 行尾放不下宽字符时留的占位格：说明下一行是这一行的软换行延续。
    LeadingSpacer,
}

/// 光标和屏幕的轻量读数：输入跟踪每次都要取，所以不能像 [`Snapshot`] 那样整屏拷贝。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenProbe {
    pub rows: usize,
    pub cols: usize,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub alt_screen: bool,
    pub display_offset: usize,
    pub revision: u64,
    pub content_revision: u64,
}

/// 协议回复队列的条数上限。DSR / DA 这类应答都只有几个字节，远端正常使用时一批输出里
/// 不会超过个位数；上限只是防恶意输出刷爆内存和上行带宽。
pub const MAX_REPLIES: usize = 64;
/// 协议回复队列的总字节上限。
pub const MAX_REPLY_BYTES: usize = 4096;

/// 有界的协议回复队列：满了丢最旧的并计数（最新的查询最可能还有人在等）。
#[derive(Default)]
struct ReplyQueue {
    items: VecDeque<Vec<u8>>,
    bytes: usize,
    dropped: u64,
}

impl ReplyQueue {
    fn push(&mut self, reply: Vec<u8>) {
        if reply.is_empty() {
            return;
        }
        // 单条就超过总上限的不可能是正常应答。
        if reply.len() > MAX_REPLY_BYTES {
            self.dropped += 1;
            return;
        }
        while !self.items.is_empty()
            && (self.items.len() >= MAX_REPLIES || self.bytes + reply.len() > MAX_REPLY_BYTES)
        {
            if let Some(old) = self.items.pop_front() {
                self.bytes -= old.len();
                self.dropped += 1;
            }
        }
        self.bytes += reply.len();
        self.items.push_back(reply);
    }

    fn take(&mut self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.bytes);
        for item in self.items.drain(..) {
            out.extend_from_slice(&item);
        }
        self.bytes = 0;
        out
    }
}

/// 把 Alacritty 的事件里"该回给远端的字节"收进队列。
///
/// 只认 `PtyWrite`（DSR 状态 / 光标位置、DA 设备属性、DECRQM 模式查询、`CSI 18 t` 窗口字符尺寸等）。
/// 其余一律忽略：
/// - `ClipboardStore` / `ClipboardLoad`（OSC 52）：剪贴板读写要独立设计权限，这里不放开；
/// - `ColorRequest`（OSC 10/11/12 颜色查询）：模型不知道主题色，答错比不答更糟，远端会按超时走默认；
/// - `TextAreaSizeRequest`（`CSI 14 t` 像素尺寸）：模型不知道字符格的像素大小，同上；
/// - 标题、响铃、光标闪烁等：与回复无关。
#[derive(Clone)]
struct Listener {
    replies: Arc<Mutex<ReplyQueue>>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event {
            let mut queue = self
                .replies
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            queue.push(text.into_bytes());
        }
    }
}

/// 模型的配置：OSC 52 剪贴板读写始终关闭（Alacritty 默认放行"只写剪贴板"，这里不要）。
fn model_config(history: usize) -> Config {
    Config {
        scrolling_history: history,
        osc52: Osc52::Disabled,
        ..Config::default()
    }
}

pub struct TerminalModel {
    term: Term<Listener>,
    processor: Processor,
    history_limit: usize,
    replies: Arc<Mutex<ReplyQueue>>,
    /// 见 [`Snapshot::revision`]。
    revision: u64,
    /// 文字内容的修订号：和 `revision` 的区别是滚动不算。搜索靠它认出"该重新找一遍"。
    content_revision: u64,
    /// 远端输出里的 shell 标记（OSC 133 / 633 / 7）扫描器：只看不改，字节照常交给 Alacritty。
    osc: OscScanner,
    /// 在字节进解析器之前给"不终止的 OSC"封顶（见 [`OscGuard`]）。
    osc_guard: OscGuard,
    marks: VecDeque<ShellEvent>,
    dropped_marks: u64,
    /// 累计喂进来的远端字节数（见 [`ShellEvent::stream_pos`]）。
    stream_pos: u64,
}

/// 一处搜索命中：起止都是网格坐标（行号为负表示在回滚缓冲里，0 是当前屏幕第一行），
/// 起止都含。跨软换行的命中起止在不同行。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchMatch {
    pub start: (i32, usize),
    pub end: (i32, usize),
}

/// 搜索最多收集的命中数：再多就是 `.` 之类的无意义查询，界面也画不下。
pub const MAX_SEARCH_MATCHES: usize = 10_000;

/// 把输入转成正则里的字面量。
fn escape_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        if r"\.+*?()|[]{}^$#&-~".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

impl TerminalModel {
    pub fn new(size: Size, history: usize) -> Self {
        let replies = Arc::new(Mutex::new(ReplyQueue::default()));
        let listener = Listener {
            replies: Arc::clone(&replies),
        };
        Self {
            term: Term::new(model_config(history), &size, listener),
            processor: Processor::new(),
            history_limit: history,
            replies,
            revision: 0,
            content_revision: 0,
            osc: OscScanner::new(),
            osc_guard: OscGuard::new(),
            marks: VecDeque::new(),
            dropped_marks: 0,
            stream_pos: 0,
        }
    }

    /// 取走自上次以来识别出的 shell 标记（按出现顺序）。每批输出之后取一次。
    pub fn take_shell_events(&mut self) -> Vec<ShellEvent> {
        self.marks.drain(..).collect()
    }

    /// 因超过 [`MAX_OSC_STREAM_BYTES`] 而被截断的 OSC 条数（诊断用）。
    pub fn truncated_osc_count(&self) -> u64 {
        self.osc_guard.overflowed()
    }

    /// 因队列满或序列超长 / 损坏而丢掉的标记数（诊断用）。
    pub fn dropped_shell_events(&self) -> u64 {
        self.dropped_marks + self.osc.dropped()
    }

    /// 光标和屏幕模式的轻量读数（不拷贝屏幕内容）。光标取网格里的真实位置，不管远端有没有把它隐藏。
    pub fn screen_probe(&self) -> ScreenProbe {
        let grid = self.term.grid();
        ScreenProbe {
            rows: grid.screen_lines(),
            cols: grid.columns(),
            cursor_row: grid.cursor.point.line.0.max(0) as usize,
            cursor_col: grid.cursor.point.column.0,
            alt_screen: self.alt_screen(),
            display_offset: grid.display_offset(),
            revision: self.revision,
            content_revision: self.content_revision,
        }
    }

    /// 屏幕第 `row` 行（屏幕坐标，不受滚回历史影响）的全部格子，保留尾部空格。
    /// 行号越界返回 `None`。只读这一行，成本是列数，不扫整屏。
    pub fn line_cells(&self, row: usize) -> Option<Vec<LineCell>> {
        let grid = self.term.grid();
        if row >= grid.screen_lines() {
            return None;
        }
        let line = &grid[Line(row as i32)];
        let cells = (0..grid.columns())
            .map(|col| {
                let cell = &line[Column(col)];
                let kind = if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    LineCellKind::WideSpacer
                } else if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                    LineCellKind::LeadingSpacer
                } else if cell.flags.contains(Flags::WIDE_CHAR) {
                    LineCellKind::Wide
                } else {
                    LineCellKind::Normal
                };
                LineCell {
                    ch: cell.c,
                    zerowidth: cell
                        .zerowidth()
                        .map(|chars| chars.to_vec().into_boxed_slice()),
                    kind,
                }
            })
            .collect();
        Some(cells)
    }

    /// 取走远端查询产生的协议回复（按产生顺序拼成一段字节）。调用方每次喂完输出后取一次，
    /// 写回产生查询的那个会话；队列有上限，满了丢最旧的。
    pub fn take_replies(&mut self) -> Vec<u8> {
        self.replies
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    /// 因队列满（或单条过大）累计丢掉的回复条数。
    pub fn dropped_replies(&self) -> u64 {
        self.replies
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .dropped
    }

    /// 同步更新（`CSI ?2026h`）超时就结束它，把攒着的输出画出来；返回是不是真的结束了一次。
    ///
    /// 远端开了同步更新后断线或崩溃、不发 `CSI ?2026l` 时，解析器会一直把输出攒进缓冲，屏幕不再刷新，
    /// 直到攒满 2 MiB 或重连。结束同步的时机得由我们自己定（解析器只记下期限，不会自己到期）：
    /// 期限取解析器自己设的那个——每次 `?2026h` 起算 150 ms，和 Alacritty 本体的事件循环一致，
    /// 足够正常程序一帧内发完一屏，又不会让卡死的程序冻住画面。
    /// 调用方在每次轮询（含没有新数据的空闲轮询）时调用；`feed` / `feed_local` 开头也会先过一遍。
    pub fn expire_sync(&mut self) -> bool {
        self.expire_sync_at(Instant::now())
    }

    fn expire_sync_at(&mut self, now: Instant) -> bool {
        let Some(deadline) = self.processor.sync_timeout().sync_timeout() else {
            return false;
        };
        if now < deadline {
            return false;
        }
        self.processor.stop_sync(&mut self.term);
        self.revision += 1;
        self.content_revision += 1;
        true
    }

    /// 喂界面自己的文字（"正在连接…"、断线提示）：不是远端的输出，就算里面有查询也不产生回复。
    pub fn feed_local(&mut self, bytes: &[u8]) {
        // 同步更新已经超时的话，先结束它：否则这段本地文字也会被攒进同步缓冲。
        self.expire_sync();
        let kept = {
            let mut queue = self
                .replies
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut queue.items)
        };
        self.processor.advance(&mut self.term, bytes);
        self.revision += 1;
        self.content_revision += 1;
        let mut queue = self
            .replies
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        queue.items = kept;
    }

    /// 消费一批字节，返回实际交给状态机的字节数（供 ACK 账本使用）。
    ///
    /// 先对原始字节做一遍 OSC 扫描；有 shell 标记时在序列结尾处把批次切开，让 Alacritty 先处理到
    /// 那里，这样记下的光标位置就是标记发生那一刻的（`133;B` 之后同一批里的回显不会混进去）。
    /// 解析器是有状态的，切开喂和整批喂结果一样。
    pub fn feed(&mut self, raw: &[u8]) -> usize {
        // 空批次（过滤器的定时器空转）什么也不做：下面的"先结束超时的同步更新"会改屏幕，
        // 不能在调用方看不到变化的路径上悄悄做掉，空闲轮询靠 `expire_sync` 的返回值才知道要重绘。
        if raw.is_empty() {
            return 0;
        }
        // 上一轮开的同步更新要是已经超时（远端没发结束序列），先结束它，新来的字节才不会继续被攒着。
        self.expire_sync();
        // 先给"不终止的 OSC"封顶：解析器自己的 OSC 缓冲没有上限，远端一直不结束就会无限吃内存。
        // 没超限时这里零拷贝；超限时 `bytes` 是改写后的字节（插了 CAN、丢了超限的尾部），
        // 下面的扫描和切段都按它做，账本（返回值、`stream_pos`）仍按原始长度记。
        let guarded = self.osc_guard.filter(raw);
        let bytes: &[u8] = &guarded;
        let mut found = Vec::new();
        self.osc.scan(bytes, &mut found);
        if found.is_empty() {
            self.processor.advance(&mut self.term, bytes);
        } else {
            let mut start = 0;
            for (end, mark) in found {
                self.processor.advance(&mut self.term, &bytes[start..end]);
                start = end;
                let anchor = CursorAnchor {
                    row: self.term.grid().cursor.point.line.0.max(0) as usize,
                    col: self.term.grid().cursor.point.column.0,
                    alt_screen: self.alt_screen(),
                };
                if self.marks.len() >= MAX_PENDING_MARKS {
                    self.marks.pop_front();
                    self.dropped_marks += 1;
                }
                self.marks.push_back(ShellEvent {
                    mark,
                    stream_pos: self.stream_pos + end as u64,
                    anchor,
                });
            }
            self.processor.advance(&mut self.term, &bytes[start..]);
        }
        self.stream_pos += raw.len() as u64;
        if !raw.is_empty() {
            self.revision += 1;
            self.content_revision += 1;
        }
        raw.len()
    }

    /// 文字内容的修订号（滚动不算），见 [`SearchMatch`] 的用法：变了就该重新搜索。
    pub fn content_revision(&self) -> u64 {
        self.content_revision
    }

    /// 现在是不是在备用屏幕（vim、less、tmux 之类）。
    pub fn alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// 在屏幕 + 回滚缓冲里找 `query`（按字面量，不是正则），结果按位置从旧到新排。
    /// 跨软换行的命中也能找到；备用屏幕上只搜得到备用屏幕的内容（主屏幕的回滚此时不在网格里）。
    /// 命中数封顶 [`MAX_SEARCH_MATCHES`]。空查询没有命中。
    pub fn search(&self, query: &str, case_sensitive: bool) -> Vec<SearchMatch> {
        if query.is_empty() {
            return Vec::new();
        }
        // alacritty 默认"全小写就不分大小写"（智能大小写）；用内联标志把它改成由开关决定。
        let flag = if case_sensitive { "(?-i)" } else { "(?i)" };
        let pattern = format!("{flag}{}", escape_literal(query));
        let Ok(mut regex) = RegexSearch::new(&pattern) else {
            return Vec::new();
        };
        let grid = self.term.grid();
        let last_col = grid.columns().saturating_sub(1);
        let start = Point::new(grid.topmost_line(), Column(0));
        let end = Point::new(grid.bottommost_line(), Column(last_col));
        RegexIter::new(start, end, Direction::Right, &self.term, &mut regex)
            .take(MAX_SEARCH_MATCHES)
            .map(|found| SearchMatch {
                start: (found.start().line.0, found.start().column.0),
                end: (found.end().line.0, found.end().column.0),
            })
            .collect()
    }

    /// 让网格行 `line`（负数在回滚里）出现在可见区；本来就看得见就不动，否则把它放到大约居中的位置。
    pub fn scroll_to_line(&mut self, line: i32) {
        let rows = self.term.grid().screen_lines() as i32;
        let offset = self.term.grid().display_offset() as i32;
        let top = -offset;
        if line >= top && line < top + rows {
            return;
        }
        let history = self.term.grid().history_size() as i32;
        let wanted = (rows / 2 - line).clamp(0, history);
        self.term.scroll_display(Scroll::Delta(wanted - offset));
        self.revision += 1;
    }

    /// 清屏：丢掉整个回滚缓冲，屏幕只留光标所在的那一行（通常是提示符），挪到第一行，光标跟着它。
    /// 和老版本 xterm.js 的 `clear()` 一样；纯本地操作，不碰远端。备用屏幕上什么也不做
    /// （全屏程序下清掉本地画面只会留下一块空白，程序要等下次重绘才恢复）。
    pub fn clear_keep_cursor_line(&mut self) {
        if self.alt_screen() {
            return;
        }
        let rows = self.term.grid().screen_lines();
        let cursor_line = self.term.grid().cursor.point.line.0.max(0) as usize;
        let grid = self.term.grid_mut();
        grid.clear_history();
        if cursor_line > 0 {
            // 整屏上卷 cursor_line 行：光标行到了第一行，被卷走的行进回滚（下面一并清掉）。
            grid.scroll_up::<Color>(&(Line(0)..Line(rows as i32)), cursor_line);
            grid.clear_history();
        }
        // 第一行以外全部清空：上卷后下面残留的是原来光标行之后的旧内容。
        if rows > 1 {
            grid.reset_region::<Color, _>(Line(1)..Line(rows as i32));
        }
        grid.cursor.point.line = Line(0);
        self.revision += 1;
        self.content_revision += 1;
    }

    /// 接新会话之前把远端可能留下的终端状态复位：备用屏幕、光标隐藏、鼠标上报、括号粘贴、
    /// 应用光标键 / 小键盘、滚动区域、插入模式、SGR 颜色、字符集、光标样式、解析器的半截转义序列。
    /// 不清屏、不清回滚，光标留在原处（若在备用屏幕上则回到主屏幕的光标）。
    /// 还没取走的协议回复也作废：那是旧会话的查询产生的。
    pub fn reset_for_new_session(&mut self) {
        // 断线时可能正卡在半截转义序列（比如 OSC 字符串）里，不换掉解析器的话后面的文字都会被吞。
        self.processor = Processor::new();
        // 扫描器的半截序列和还没取走的标记也是旧会话的：一起作废。
        self.osc.reset();
        self.osc_guard.reset();
        self.marks.clear();
        let _ = self.take_replies();
        if self.alt_screen() {
            self.processor.advance(&mut self.term, b"\x1b[?1049l");
        }
        let cursor = self.term.grid().cursor.point;
        self.processor.advance(
            &mut self.term,
            concat!(
                "\x1b[?25h",   // 显示光标
                "\x1b[?1l",    // 光标键回到普通模式
                "\x1b[?1000l", // 鼠标上报全部关
                "\x1b[?1002l",
                "\x1b[?1003l",
                "\x1b[?1005l",
                "\x1b[?1006l",
                "\x1b[?1004l", // 焦点上报
                "\x1b[?2004l", // 括号粘贴
                "\x1b[?6l",    // 原点模式
                "\x1b[?7h",    // 自动换行
                "\x1b[?1007h", // 备用屏滚轮（默认开）
                "\x1b>",       // 小键盘回到数字模式
                "\x1b[4l",     // 插入模式
                "\x1b[20l",    // 换行模式
                "\x1b[<99u",   // 键盘增强协议栈清空
                "\x1b[0 q",    // 光标样式回默认
                "\x1b(B",      // 字符集回 ASCII
                "\x0f",        // SI：用 G0
                "\x1b[0m",     // 颜色和属性
                "\x1b[r",      // 滚动区域（会把光标挪到左上角，下面再放回去）
            )
            .as_bytes(),
        );
        let restore = format!("\x1b[{};{}H", cursor.line.0.max(0) + 1, cursor.column.0 + 1);
        self.processor.advance(&mut self.term, restore.as_bytes());
        self.revision += 1;
        self.content_revision += 1;
    }

    pub fn resize(&mut self, size: Size) {
        self.term.resize(size);
        self.revision += 1;
        self.content_revision += 1;
    }

    /// 改回滚上限，对已经在用的终端立刻生效（0.2.14 里改设置会同步给已开的 xterm）：
    /// 调小会丢掉最旧的多余行，调大只是以后能攒更多。
    pub fn set_history(&mut self, history: usize) {
        self.term.set_options(model_config(history));
        self.history_limit = history;
        self.revision += 1;
        self.content_revision += 1;
    }

    /// 当前的回滚上限。
    pub fn history_limit(&self) -> usize {
        self.history_limit
    }

    /// 正数向下滚（看历史），负数向上滚。
    /// 正数向上翻历史，负数向下回当前。
    pub fn scroll(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(delta));
        self.revision += 1;
    }

    pub fn scroll_to_top(&mut self) {
        self.term.scroll_display(Scroll::Top);
        self.revision += 1;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
        self.revision += 1;
    }

    /// 可见内容的修订号，见 [`Snapshot::revision`]。
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    pub fn size(&self) -> Size {
        let grid = self.term.grid();
        Size {
            rows: grid.screen_lines(),
            cols: grid.columns(),
        }
    }

    pub fn history_size(&self) -> usize {
        self.term.grid().history_size()
    }

    /// DECCKM：远端要求方向键用 SS3（`ESC O A`）。
    pub fn app_cursor(&self) -> bool {
        self.term.mode().contains(TermMode::APP_CURSOR)
    }

    /// 远端是否开启了 bracketed paste（`ESC [ 200 ~` / `201 ~`）。
    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// 远端的鼠标上报模式（DECSET 1000/1002/1003/1006）。
    pub fn mouse_mode(&self) -> MouseMode {
        let mode = self.term.mode();
        MouseMode {
            click: mode.contains(TermMode::MOUSE_REPORT_CLICK),
            drag: mode.contains(TermMode::MOUSE_DRAG),
            motion: mode.contains(TermMode::MOUSE_MOTION),
            sgr: mode.contains(TermMode::SGR_MOUSE),
        }
    }

    /// 光标可见时返回可见区坐标；滚回历史或远端隐藏光标时返回 `None`。
    pub fn cursor_visible(&self) -> Option<(usize, usize)> {
        let grid = self.term.grid();
        if grid.display_offset() != 0 || !self.term.mode().contains(TermMode::SHOW_CURSOR) {
            return None;
        }
        let point = grid.cursor.point;
        Some((point.line.0.max(0) as usize, point.column.0))
    }

    pub fn snapshot(&self) -> Snapshot {
        let grid = self.term.grid();
        let rows_count = grid.screen_lines();
        let cols = grid.columns();
        // `display_offset` 是滚回历史的行数；可见区第 0 行对应 Line(-offset)。
        let offset = grid.display_offset() as i32;
        let mut rows = Vec::with_capacity(rows_count);

        for row in 0..rows_count {
            let mut text = String::with_capacity(cols);
            let mut cells = Vec::with_capacity(cols);
            for col in 0..cols {
                let cell = &grid[Line(row as i32 - offset)][Column(col)];
                // 宽字符的占位格不进入文本，否则复制/搜索会多出空格。
                if !cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    text.push(cell.c);
                    if let Some(zerowidth) = cell.zerowidth() {
                        text.extend(zerowidth);
                    }
                }
                cells.push(CellSnapshot {
                    ch: cell.c,
                    fg: cell.fg,
                    bg: cell.bg,
                    bold: cell.flags.contains(Flags::BOLD),
                    italic: cell.flags.contains(Flags::ITALIC),
                    underline: cell.flags.contains(Flags::UNDERLINE),
                    strikethrough: cell.flags.contains(Flags::STRIKEOUT),
                    inverse: cell.flags.contains(Flags::INVERSE),
                    hidden: cell.flags.contains(Flags::HIDDEN),
                    zerowidth: cell
                        .zerowidth()
                        .map(|chars| chars.to_vec().into_boxed_slice()),
                    hyperlink: cell.hyperlink().map(|link| Arc::from(link.uri())),
                    spacer: cell
                        .flags
                        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER),
                });
            }
            rows.push(RowSnapshot {
                text: text.trim_end().to_owned(),
                cells,
            });
        }

        Snapshot {
            size: Size {
                rows: rows_count,
                cols,
            },
            rows,
            cursor_row: grid.cursor.point.line.0.max(0) as usize,
            cursor_col: grid.cursor.point.column.0,
            history_size: grid.history_size(),
            revision: self.revision,
            display_offset: grid.display_offset(),
        }
    }

    /// 取某一格的前景/背景，测试和后续高亮会用到。
    pub fn cell(&self, row: usize, col: usize) -> CellSnapshot {
        let grid = self.term.grid();
        let offset = grid.display_offset() as i32;
        let cell = &grid[Line(row as i32 - offset)][Column(col)];
        CellSnapshot {
            ch: cell.c,
            fg: cell.fg,
            bg: cell.bg,
            bold: cell.flags.contains(Flags::BOLD),
            italic: cell.flags.contains(Flags::ITALIC),
            underline: cell.flags.contains(Flags::UNDERLINE),
            strikethrough: cell.flags.contains(Flags::STRIKEOUT),
            inverse: cell.flags.contains(Flags::INVERSE),
            hidden: cell.flags.contains(Flags::HIDDEN),
            zerowidth: cell
                .zerowidth()
                .map(|chars| chars.to_vec().into_boxed_slice()),
            hyperlink: cell.hyperlink().map(|link| Arc::from(link.uri())),
            spacer: cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER),
        }
    }

    /// 取两个可见区坐标之间的文字（含端点），按行拼接，行尾空白去掉。
    pub fn text_between(&self, start: (usize, usize), end: (usize, usize)) -> String {
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        let snapshot = self.snapshot();
        if snapshot.rows.is_empty() {
            return String::new();
        }

        let last_row = snapshot.rows.len() - 1;
        let end_row = end.0.min(last_row);
        let cols = snapshot.size.cols;
        let mut out = String::new();

        for row in start.0.min(last_row)..=end_row {
            let from = if row == start.0 { start.1 } else { 0 };
            let to = if row == end_row { end.1 } else { cols - 1 };
            let mut line = String::new();
            for col in from..=to.min(cols.saturating_sub(1)) {
                let cell = &snapshot.rows[row].cells[col];
                if cell.spacer {
                    continue;
                }
                line.push(cell.ch);
                if let Some(zerowidth) = &cell.zerowidth {
                    line.extend(zerowidth.iter());
                }
            }
            out.push_str(line.trim_end());
            if row < end_row {
                out.push('\n');
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::vte::ansi::NamedColor;

    fn model(rows: usize, cols: usize, history: usize) -> TerminalModel {
        TerminalModel::new(Size { rows, cols }, history)
    }

    /// 远端发了不终止的 OSC：模型不能把它当无限大的缓冲吃下去，超限那条被截断，
    /// 后面的内容也不会当成普通文字打到屏幕上，OSC 结束后的正常输出照常显示。
    #[test]
    fn an_unterminated_osc_is_capped_and_does_not_leak_onto_the_screen() {
        let mut term = model(4, 40, 100);
        term.feed(b"\x1b]0;");
        let chunk = vec![b'a'; 64 * 1024];
        let mut total = 0;
        // 比上限多灌 8 MiB；反馈给调用方的"已消费字节数"仍是全部（流控账本要对得上）。
        while total < MAX_OSC_STREAM_BYTES + 8 * 1024 * 1024 {
            assert_eq!(term.feed(&chunk), chunk.len());
            total += chunk.len();
        }
        assert_eq!(term.truncated_osc_count(), 1);
        assert_eq!(term.snapshot().rows[0].text, "");
        term.feed(b"\x07hello");
        assert_eq!(term.snapshot().rows[0].text, "hello");
    }

    /// 远端开了同步更新就不再发结束序列（断线 / 崩溃）：到期后必须能把攒着的输出画出来，
    /// 不然屏幕冻住直到缓冲攒满 2 MiB。
    #[test]
    fn a_sync_update_that_never_ends_is_flushed_after_the_deadline() {
        let mut term = model(4, 40, 100);
        term.feed(b"\x1b[?2026hframe");
        assert_eq!(term.snapshot().rows[0].text, "", "同步期间输出被攒着");
        // 还没到期：什么也不做。
        assert!(!term.expire_sync_at(Instant::now()));
        assert_eq!(term.snapshot().rows[0].text, "");
        let revision = term.snapshot().revision;
        // 到期：攒着的输出画出来，修订号变（界面据此重绘）。
        let later = Instant::now() + std::time::Duration::from_secs(1);
        assert!(term.expire_sync_at(later));
        assert_eq!(term.snapshot().rows[0].text, "frame");
        assert_ne!(term.snapshot().revision, revision);
        // 已经结束，再检查不再有动作。
        assert!(!term.expire_sync_at(later));
    }

    /// 正常程序在期限内发了结束序列：不受影响，也没有多余的到期动作。
    #[test]
    fn a_sync_update_that_ends_in_time_is_untouched() {
        let mut term = model(4, 40, 100);
        term.feed(b"\x1b[?2026hframe\x1b[?2026l");
        assert_eq!(term.snapshot().rows[0].text, "frame");
        let later = Instant::now() + std::time::Duration::from_secs(1);
        assert!(!term.expire_sync_at(later));
    }

    /// 到期之后新来的字节不能继续被攒着。
    #[test]
    fn output_after_an_expired_sync_is_not_swallowed() {
        let mut term = model(4, 40, 100);
        term.feed(b"\x1b[?2026hfirst");
        std::thread::sleep(std::time::Duration::from_millis(200));
        term.feed(b" second");
        assert_eq!(term.snapshot().rows[0].text, "first second");
    }

    #[test]
    fn plain_text_and_cursor() {
        let mut term = model(4, 20, 100);
        term.feed(b"hello\r\nworld");
        let snap = term.snapshot();
        assert_eq!(snap.rows[0].text, "hello");
        assert_eq!(snap.rows[1].text, "world");
        assert_eq!((snap.cursor_row, snap.cursor_col), (1, 5));
    }

    /// 异步高亮靠修订号认出"扫描的是不是眼前这一屏"：任何会改变可见内容的操作都要让它变，
    /// 没改变内容的读取不能让它变。
    #[test]
    fn the_revision_moves_whenever_the_visible_screen_can_change() {
        let mut term = model(4, 20, 100);
        fn changed(term: &TerminalModel, last: &mut u64, what: &str) {
            let now = term.snapshot().revision;
            assert_ne!(now, *last, "{what} 之后修订号应该变");
            *last = now;
        }
        let mut last = term.snapshot().revision;

        term.feed(
            b"hello
",
        );
        changed(&term, &mut last, "喂输出");
        term.feed_local(b"local");
        changed(&term, &mut last, "喂本地文字");
        term.resize(Size { rows: 5, cols: 30 });
        changed(&term, &mut last, "改尺寸");
        for i in 0..20 {
            term.feed(
                format!(
                    "line {i}
"
                )
                .as_bytes(),
            );
        }
        last = term.snapshot().revision;
        term.scroll(3);
        changed(&term, &mut last, "滚动");
        assert_eq!(term.snapshot().display_offset, 3);
        term.scroll_to_bottom();
        changed(&term, &mut last, "回到底部");
        term.set_history(50);
        changed(&term, &mut last, "改回滚上限");

        // 取快照、取回复、喂空数据都不改变内容，修订号不动。
        let before = term.snapshot().revision;
        let _ = term.snapshot();
        let _ = term.take_replies();
        term.feed(b"");
        assert_eq!(term.snapshot().revision, before);
    }

    #[test]
    fn ansi_colors_are_parsed() {
        let mut term = model(2, 20, 10);
        term.feed(b"\x1b[31mred\x1b[0m plain");
        let cell = term.cell(0, 0);
        assert_eq!(cell.fg, Color::Named(NamedColor::Red));
        assert_eq!(cell.ch, 'r');
    }

    #[test]
    fn utf8_split_across_chunks_is_reassembled() {
        let mut term = model(2, 10, 10);
        let bytes = "中".as_bytes();
        term.feed(&bytes[..2]);
        term.feed(&bytes[2..]);
        assert_eq!(term.snapshot().rows[0].text, "中");
    }

    #[test]
    fn wide_cjk_takes_two_columns() {
        let mut term = model(2, 10, 10);
        term.feed("中文".as_bytes());
        let snap = term.snapshot();
        assert_eq!(snap.rows[0].text, "中文");
        assert_eq!(snap.cursor_col, 4);
    }

    #[test]
    fn alternate_screen_restores_primary() {
        let mut term = model(3, 20, 10);
        term.feed(b"main");
        term.feed(b"\x1b[?1049h\ralt");
        assert_eq!(term.snapshot().rows[0].text, "alt");
        term.feed(b"\x1b[?1049l");
        assert_eq!(term.snapshot().rows[0].text, "main");
    }

    #[test]
    fn scrollback_is_bounded() {
        let mut term = model(3, 20, 5);
        for i in 0..50 {
            term.feed(format!("line{i}\r\n").as_bytes());
        }
        assert!(term.history_size() <= 5, "history={}", term.history_size());
    }

    #[test]
    fn lowering_the_history_limit_trims_a_live_terminal() {
        let mut term = model(3, 20, 100);
        for i in 0..60 {
            term.feed(format!("line{i}\r\n").as_bytes());
        }
        assert!(term.history_size() > 40);
        term.set_history(10);
        assert_eq!(term.history_limit(), 10);
        assert!(term.history_size() <= 10, "history={}", term.history_size());
        // 之后继续输出也不会超过新的上限。
        for i in 0..60 {
            term.feed(format!("more{i}\r\n").as_bytes());
        }
        assert!(term.history_size() <= 10);
    }

    #[test]
    fn raising_the_history_limit_lets_a_live_terminal_keep_more() {
        let mut term = model(3, 20, 10);
        for i in 0..60 {
            term.feed(format!("line{i}\r\n").as_bytes());
        }
        assert!(term.history_size() <= 10);
        term.set_history(200);
        for i in 0..60 {
            term.feed(format!("more{i}\r\n").as_bytes());
        }
        let size = term.history_size();
        assert!(size > 10 && size <= 200, "history={size}");
    }

    #[test]
    fn the_history_limit_also_applies_while_on_the_alternate_screen() {
        let mut term = model(3, 20, 100);
        for i in 0..30 {
            term.feed(format!("line{i}\r\n").as_bytes());
        }
        term.feed(b"\x1b[?1049h");
        term.set_history(5);
        term.feed(b"\x1b[?1049l");
        assert!(term.history_size() <= 5, "history={}", term.history_size());
    }

    #[test]
    fn resize_updates_dimensions() {
        let mut term = model(4, 20, 10);
        term.resize(Size { rows: 8, cols: 40 });
        assert_eq!(term.size(), Size { rows: 8, cols: 40 });
        assert_eq!(term.snapshot().rows.len(), 8);
    }

    #[test]
    fn scroll_display_moves_the_view() {
        let mut term = model(2, 20, 100);
        for i in 0..10 {
            term.feed(format!("line{i}\r\n").as_bytes());
        }
        let bottom = term.snapshot().rows[0].text.clone();
        term.scroll(3);
        let scrolled = term.snapshot().rows[0].text.clone();
        assert_ne!(bottom, scrolled, "滚回历史后可见区应变化");
    }

    #[test]
    fn text_between_selects_partial_rows() {
        let mut term = model(3, 20, 10);
        term.feed(b"hello\r\nworld");
        assert_eq!(term.text_between((0, 1), (0, 4)), "ello");
        assert_eq!(term.text_between((0, 0), (1, 4)), "hello\nworld");
        assert_eq!(term.text_between((1, 4), (0, 0)), "hello\nworld");
    }

    #[test]
    fn scroll_to_top_and_bottom() {
        let mut term = model(2, 10, 100);
        for i in 0..10 {
            term.feed(format!("line{i}\r\n").as_bytes());
        }
        term.scroll_to_top();
        assert_eq!(term.display_offset(), term.history_size());
        assert_eq!(term.snapshot().rows[0].text, "line0");
        term.scroll_to_bottom();
        assert_eq!(term.display_offset(), 0);
    }

    #[test]
    fn cursor_is_visible_and_can_be_hidden() {
        let mut term = model(3, 10, 10);
        term.feed(b"hi");
        assert_eq!(term.cursor_visible(), Some((0, 2)));
        term.feed(b"\x1b[?25l");
        assert_eq!(term.cursor_visible(), None);
        term.feed(b"\x1b[?25h");
        assert_eq!(term.cursor_visible(), Some((0, 2)));

        // 滚回历史时不画光标。
        let mut term = model(2, 10, 100);
        for i in 0..10 {
            term.feed(format!("l{i}\r\n").as_bytes());
        }
        assert!(term.cursor_visible().is_some());
        term.scroll(2);
        assert_eq!(term.cursor_visible(), None);
    }

    #[test]
    fn style_flags_and_combining_chars_survive_the_snapshot() {
        let mut term = model(2, 20, 10);
        let input = "\x1b[4mU\x1b[0m\x1b[9mS\x1b[0m\x1b[8mH\x1b[0me\u{301}";
        term.feed(input.as_bytes());
        let snap = term.snapshot();
        assert!(snap.rows[0].cells[0].underline);
        assert!(snap.rows[0].cells[1].strikethrough);
        assert!(snap.rows[0].cells[2].hidden);
        assert_eq!(snap.rows[0].cells[3].ch, 'e');
        assert_eq!(
            snap.rows[0].cells[3]
                .zerowidth
                .as_deref()
                .map(|chars| chars.to_vec()),
            Some(vec!['\u{301}'])
        );
        assert!(snap.rows[0].text.contains("e\u{301}"));
    }

    #[test]
    fn osc8_links_are_preserved() {
        let mut term = model(2, 30, 10);
        let link = "\x1b]8;;https://example.com\x1b\\click\x1b]8;;\x1b\\";
        term.feed(link.as_bytes());
        let snap = term.snapshot();
        assert_eq!(snap.rows[0].text, "click");
        assert_eq!(
            snap.rows[0].cells[0].hyperlink.as_deref(),
            Some("https://example.com")
        );
        assert_eq!(
            snap.rows[0].cells[4].hyperlink.as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn status_report_query_is_answered() {
        let mut term = model(4, 20, 10);
        assert!(term.take_replies().is_empty(), "没有查询就没有回复");
        term.feed(b"\x1b[5n");
        assert_eq!(term.take_replies(), b"\x1b[0n");
        assert!(term.take_replies().is_empty(), "取走之后队列是空的");
    }

    #[test]
    fn cursor_position_report_follows_the_cursor() {
        let mut term = model(6, 30, 10);
        term.feed(b"\x1b[6n");
        assert_eq!(term.take_replies(), b"\x1b[1;1R");

        term.feed(b"\x1b[3;7H\x1b[6n");
        assert_eq!(term.take_replies(), b"\x1b[3;7R");

        term.feed(b"abc\x1b[6n");
        assert_eq!(term.take_replies(), b"\x1b[3;10R");

        // 查询被切成两批到达也要答（状态机跨批保留解析状态）。
        term.feed(b"\x1b[");
        assert!(term.take_replies().is_empty());
        term.feed(b"6n");
        assert_eq!(term.take_replies(), b"\x1b[3;10R");
    }

    #[test]
    fn device_attribute_queries_are_answered() {
        let mut term = model(4, 20, 10);
        term.feed(b"\x1b[c");
        assert_eq!(term.take_replies(), b"\x1b[?6c");

        term.feed(b"\x1b[0c");
        assert_eq!(term.take_replies(), b"\x1b[?6c");

        term.feed(b"\x1b[>c");
        let secondary = String::from_utf8(term.take_replies()).unwrap();
        assert!(
            secondary.starts_with("\x1b[>0;") && secondary.ends_with(";1c"),
            "{secondary:?}"
        );
    }

    #[test]
    fn several_queries_in_one_batch_are_answered_in_order() {
        let mut term = model(4, 20, 10);
        term.feed(b"\x1b[5n\x1b[2;4H\x1b[6n\x1b[c");
        assert_eq!(term.take_replies(), b"\x1b[0n\x1b[2;4R\x1b[?6c");
    }

    #[test]
    fn mode_and_window_character_size_queries_are_answered() {
        let mut term = model(7, 33, 10);
        // 括号粘贴（2004）当前关闭：状态 2 = 已重置。
        term.feed(b"\x1b[?2004$p");
        assert_eq!(term.take_replies(), b"\x1b[?2004;2$y");
        term.feed(b"\x1b[?2004h\x1b[?2004$p");
        assert_eq!(term.take_replies(), b"\x1b[?2004;1$y");
        // CSI 18 t：文本区的字符行列数。
        term.feed(b"\x1b[18t");
        assert_eq!(term.take_replies(), b"\x1b[8;7;33t");
    }

    #[test]
    fn osc52_clipboard_and_color_queries_produce_no_reply() {
        let mut term = model(4, 40, 10);
        // 写剪贴板（Alacritty 默认放行）、读剪贴板、两种终止符。
        term.feed(b"\x1b]52;c;aGVsbG8=\x07");
        term.feed(b"\x1b]52;c;?\x07");
        term.feed(b"\x1b]52;p;?\x1b\\");
        // 前景 / 背景 / 光标颜色查询：模型不知道主题色，不回答。
        term.feed(b"\x1b]10;?\x07\x1b]11;?\x07\x1b]12;?\x1b\\");
        // 像素尺寸查询：模型不知道字符格大小，不回答。
        term.feed(b"\x1b[14t");
        assert!(term.take_replies().is_empty());
        assert_eq!(term.dropped_replies(), 0);
        // 这些序列没有污染屏幕。
        assert_eq!(term.snapshot().rows[0].text, "");
    }

    #[test]
    fn reply_queue_is_bounded_and_drops_the_oldest() {
        let mut term = model(4, 20, 10);
        // 先让光标停在不同位置，便于从回复里认出是哪一条。
        let mut input = Vec::new();
        for i in 0..200usize {
            input.extend_from_slice(format!("\x1b[1;{}H\x1b[6n", (i % 19) + 1).as_bytes());
        }
        term.feed(&input);
        let bytes = term.take_replies();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert_eq!(text.matches('R').count(), MAX_REPLIES, "{text:?}");
        assert!(bytes.len() <= MAX_REPLY_BYTES);
        assert_eq!(term.dropped_replies(), 200 - MAX_REPLIES as u64);
        // 留下来的是最新的 64 条：最后一条对应 i = 199。
        assert!(
            text.ends_with(&format!("\x1b[1;{}R", (199 % 19) + 1)),
            "{text:?}"
        );
    }

    #[test]
    fn reply_queue_also_obeys_the_byte_limit() {
        let mut queue = ReplyQueue::default();
        for _ in 0..10 {
            queue.push(vec![b'x'; 1000]);
        }
        assert!(queue.bytes <= MAX_REPLY_BYTES);
        assert_eq!(queue.items.len(), 4);
        assert_eq!(queue.dropped, 6);
        // 单条超过总上限的直接丢，不挤掉别的。
        queue.push(vec![b'y'; MAX_REPLY_BYTES + 1]);
        assert_eq!(queue.items.len(), 4);
        assert_eq!(queue.dropped, 7);
        assert_eq!(queue.take().len(), 4000);
        assert_eq!(queue.bytes, 0);
    }

    #[test]
    fn local_text_never_produces_a_reply_and_keeps_pending_ones() {
        let mut term = model(4, 20, 10);
        term.feed(b"\x1b[5n");
        // 界面自己的提示里就算带了查询序列，也不能让它去问远端。
        term.feed_local(b"notice\x1b[6n\x1b[c\r\n");
        assert_eq!(
            term.take_replies(),
            b"\x1b[0n",
            "远端的回复还在，本地的没有"
        );
        assert_eq!(term.snapshot().rows[0].text, "notice");
    }

    #[test]
    fn changing_the_history_limit_keeps_osc52_off() {
        let mut term = model(4, 20, 10);
        term.set_history(50);
        term.feed(b"\x1b]52;c;?\x07\x1b]52;c;aGk=\x07");
        assert!(term.take_replies().is_empty());
    }

    fn screen_text(term: &TerminalModel) -> Vec<String> {
        term.snapshot()
            .rows
            .iter()
            .map(|row| row.text.clone())
            .collect()
    }

    #[test]
    fn search_finds_matches_in_the_scrollback_oldest_first() {
        let mut term = model(4, 40, 100);
        for i in 0..20 {
            term.feed(format!("row {i} needle\r\n").as_bytes());
        }
        let found = term.search("needle", false);
        assert_eq!(found.len(), 20);
        assert!(found.windows(2).all(|pair| pair[0].start < pair[1].start));
        // 最老的一个已经在回滚缓冲里（行号为负）。
        assert!(found[0].start.0 < 0);
        assert_eq!(found[0].start.1, 6);
        assert_eq!(found[0].end.1, 11);
        assert!(term.search("", false).is_empty());
        assert!(term.search("absent", false).is_empty());
    }

    #[test]
    fn the_case_switch_decides_not_the_smart_case_default() {
        let mut term = model(4, 40, 100);
        term.feed(b"Error error ERROR\r\n");
        assert_eq!(term.search("error", false).len(), 3);
        // 全小写的查询在 alacritty 里默认不分大小写；开关打开后必须分。
        assert_eq!(term.search("error", true).len(), 1);
        assert_eq!(term.search("Error", true).len(), 1);
        assert_eq!(term.search("Error", false).len(), 3);
    }

    #[test]
    fn regex_characters_in_the_query_are_literals() {
        let mut term = model(4, 40, 100);
        term.feed(b"a.b a+b (x) [y] a\\b\r\n");
        assert_eq!(term.search("a.b", false).len(), 1);
        assert_eq!(term.search("a+b", false).len(), 1);
        assert_eq!(term.search("(x)", false).len(), 1);
        assert_eq!(term.search("[y]", false).len(), 1);
        assert_eq!(term.search("a\\b", false).len(), 1);
        assert_eq!(term.search(".*", false).len(), 0);
    }

    #[test]
    fn a_match_that_wraps_over_the_soft_line_break_is_found() {
        let mut term = model(4, 10, 100);
        // 15 个字符在 10 列的屏幕上软换行，"fghijkl" 跨两行。
        term.feed(b"abcdefghijklmno\r\n");
        let found = term.search("fghijkl", false);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].start, (0, 5));
        assert_eq!(found[0].end, (1, 1));
    }

    #[test]
    fn wide_characters_can_be_searched() {
        let mut term = model(4, 20, 100);
        term.feed("你好世界 你好\r\n".as_bytes());
        assert_eq!(term.search("你好", false).len(), 2);
    }

    #[test]
    fn the_match_count_is_capped() {
        let mut term = model(4, 80, 100_000);
        let line = "a ".repeat(40);
        for _ in 0..400 {
            term.feed(format!("{line}\r\n").as_bytes());
        }
        assert_eq!(term.search("a", false).len(), MAX_SEARCH_MATCHES);
    }

    #[test]
    fn scrolling_to_a_line_in_the_history_makes_it_visible_and_leaves_visible_ones_alone() {
        let mut term = model(4, 20, 100);
        for i in 0..30 {
            term.feed(format!("line {i}\r\n").as_bytes());
        }
        let found = term.search("line 3", false);
        let first = found[0].start.0;
        assert!(first < 0);
        term.scroll_to_line(first);
        let offset = term.display_offset();
        assert!(term.snapshot().rows.iter().any(|row| row.text == "line 3"));
        // 已经看得见：不再动。
        term.scroll_to_line(first);
        assert_eq!(term.display_offset(), offset);
        // 当前屏幕上的行：不需要滚。
        term.scroll_to_bottom();
        term.scroll_to_line(0);
        assert_eq!(term.display_offset(), 0);
    }

    #[test]
    fn the_content_revision_ignores_scrolling_but_follows_text() {
        let mut term = model(4, 20, 100);
        for i in 0..20 {
            term.feed(format!("l{i}\r\n").as_bytes());
        }
        let before = term.content_revision();
        term.scroll(3);
        term.scroll_to_bottom();
        assert_eq!(term.content_revision(), before);
        term.feed(b"x");
        assert!(term.content_revision() > before);
    }

    #[test]
    fn clearing_keeps_only_the_cursor_line_on_the_first_row() {
        let mut term = model(5, 30, 100);
        for i in 0..20 {
            term.feed(format!("old {i}\r\n").as_bytes());
        }
        term.feed(b"user@web-1:~$ ls");
        assert!(term.history_size() > 0);
        assert!(term.snapshot().cursor_row > 0, "光标在屏幕下半部分");
        term.clear_keep_cursor_line();

        let after = term.snapshot();
        assert_eq!(term.history_size(), 0, "回滚清空");
        assert_eq!(after.rows[0].text, "user@web-1:~$ ls");
        assert!(after.rows[1..].iter().all(|row| row.text.is_empty()));
        assert_eq!((after.cursor_row, after.cursor_col), (0, 16));
        assert_eq!(after.display_offset, 0);

        // 之后的输出接在光标处。
        term.feed(b"\r\nnext line");
        let again = screen_text(&term);
        assert_eq!(again[0], "user@web-1:~$ ls");
        assert_eq!(again[1], "next line");
    }

    #[test]
    fn clearing_while_scrolled_back_returns_to_the_bottom() {
        let mut term = model(4, 20, 100);
        for i in 0..30 {
            term.feed(format!("l{i}\r\n").as_bytes());
        }
        term.feed(b"$ ");
        term.scroll(10);
        term.clear_keep_cursor_line();
        assert_eq!(term.display_offset(), 0);
        assert_eq!(term.snapshot().rows[0].text, "$");
        assert_eq!(term.history_size(), 0);
    }

    #[test]
    fn clearing_when_the_cursor_is_on_the_first_row_drops_everything_below() {
        let mut term = model(4, 20, 100);
        term.feed(b"prompt");
        term.clear_keep_cursor_line();
        assert_eq!(screen_text(&term)[0], "prompt");
        assert_eq!(term.snapshot().cursor_row, 0);
        // 光标在第一行、下面有内容（程序画过的）：下面的清掉。
        let mut term = model(4, 20, 100);
        term.feed(b"a\r\nb\r\nc\x1b[1;3H");
        term.clear_keep_cursor_line();
        assert_eq!(screen_text(&term), ["a", "", "", ""]);
    }

    #[test]
    fn clearing_does_nothing_on_the_alternate_screen() {
        let mut term = model(4, 20, 100);
        term.feed(b"main\r\n\x1b[?1049hfull screen");
        term.clear_keep_cursor_line();
        assert!(screen_text(&term).iter().any(|row| row == "full screen"));
        term.feed(b"\x1b[?1049l");
        assert_eq!(screen_text(&term)[0], "main");
    }

    #[test]
    fn a_new_session_resets_the_modes_a_dead_remote_left_behind() {
        let mut term = model(5, 30, 100);
        term.feed(b"history 1\r\nhistory 2\r\n");
        // 远端程序（vim 之类）开了一堆模式然后断线。
        term.feed(
            b"\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?2004h\x1b[?1h\x1b[2;4r\x1b[1;31mfull",
        );
        assert!(term.alt_screen());
        assert!(term.mouse_mode().reports_buttons() && term.mouse_mode().sgr);
        assert!(term.bracketed_paste() && term.app_cursor());
        assert!(term.cursor_visible().is_none());

        term.reset_for_new_session();

        assert!(!term.alt_screen());
        assert!(!term.mouse_mode().reports_buttons());
        assert!(!term.mouse_mode().sgr);
        assert!(!term.bracketed_paste());
        assert!(!term.app_cursor());
        assert!(term.cursor_visible().is_some(), "光标重新显示");
        // 回到主屏幕：历史还在。
        assert_eq!(screen_text(&term)[0], "history 1");
        assert_eq!(screen_text(&term)[1], "history 2");
        // 滚动区域和颜色也复位了：接着写的字是默认前景色，换行能一路到屏幕底。
        term.feed(b"\r\nplain\r\n\r\n\r\n\r\nlast");
        let rows = screen_text(&term);
        assert_eq!(rows[4], "last");
        let plain_at = rows.iter().position(|row| row == "plain");
        if let Some(at) = plain_at {
            assert_eq!(term.cell(at, 0).fg, Color::Named(NamedColor::Foreground));
        }
    }

    #[test]
    fn resetting_for_a_new_session_keeps_the_screen_and_the_scrollback() {
        let mut term = model(4, 20, 100);
        for i in 0..10 {
            term.feed(format!("line {i}\r\n").as_bytes());
        }
        term.feed(b"tail");
        let before = term.snapshot();
        term.reset_for_new_session();
        let after = term.snapshot();
        assert_eq!(screen_text(&term), {
            before
                .rows
                .iter()
                .map(|row| row.text.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(after.history_size, before.history_size);
        assert_eq!(
            (after.cursor_row, after.cursor_col),
            (before.cursor_row, before.cursor_col)
        );
    }

    #[test]
    fn a_half_finished_escape_sequence_does_not_eat_the_next_session() {
        let mut term = model(4, 40, 100);
        // 断线时正卡在 OSC 字符串中间：不换解析器，后面的文字都会被当成标题吞掉。
        term.feed(b"before\r\n\x1b]0;half a title");
        term.reset_for_new_session();
        term.feed(b"after");
        let rows = screen_text(&term);
        assert!(rows.iter().any(|row| row == "after"), "{rows:?}");
    }

    #[test]
    fn reset_drops_replies_owed_to_the_old_session() {
        let mut term = model(4, 20, 100);
        term.feed(b"\x1b[5n");
        term.reset_for_new_session();
        assert!(term.take_replies().is_empty());
    }

    /// 离线压力：100 MiB 确定性输出，验证解析不阻塞、回滚有界。
    /// 运行：`cargo test -p zssh-terminal replay_100mib -- --ignored --nocapture`
    #[test]
    #[ignore = "100 MiB 回放，手动/性能任务运行"]
    fn replay_100mib() {
        let mut term = model(50, 200, 10_000);
        let chunk = {
            let mut line = Vec::new();
            for i in 0..16 {
                line.extend_from_slice(
                    format!("line {i} 中文 emoji \u{1f600} \x1b[31mred\x1b[0m\r\n").as_bytes(),
                );
            }
            line
        };
        let total = 100 * 1024 * 1024;
        let started = std::time::Instant::now();
        let mut fed = 0usize;
        while fed < total {
            fed += term.feed(&chunk);
        }
        let elapsed = started.elapsed();
        println!("fed {fed} bytes in {elapsed:?}");
        assert!(term.history_size() <= 10_000);
        assert!(
            elapsed < std::time::Duration::from_secs(60),
            "回放太慢：{elapsed:?}"
        );
    }

    // ---- shell 标记（OSC 133 / 633 / 7）----

    fn marks(term: &mut TerminalModel) -> Vec<ShellEvent> {
        term.take_shell_events()
    }

    #[test]
    fn a_prompt_input_mark_records_the_cursor_where_it_arrived_not_where_the_chunk_ended() {
        let mut term = model(4, 40, 10);
        // 提示符、133;B、同一批里紧跟的回显：锚点必须是 B 那一刻的列 2，不是批末的列 4。
        term.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07ls");
        let events = marks(&mut term);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].mark.kind, ShellMarkKind::PromptStart);
        assert_eq!(events[0].anchor.col, 0);
        assert_eq!(events[1].mark.kind, ShellMarkKind::PromptEnd);
        assert_eq!((events[1].anchor.row, events[1].anchor.col), (0, 2));
        assert!(!events[1].anchor.alt_screen);
        let snap = term.snapshot();
        assert_eq!(snap.rows[0].text, "$ ls");
        assert_eq!(snap.cursor_col, 4);
        assert!(events[0].stream_pos < events[1].stream_pos);
    }

    #[test]
    fn splitting_at_marks_gives_the_same_screen_as_feeding_whole() {
        let data: &[u8] = b"\x1b[31mred\x1b[0m\r\n\x1b]133;D;0\x07\x1b]7;file:///tmp\x07\x1b]133;A\x07\x1b[1m$ \x1b[0m\x1b]133;B\x07echo \xe4\xb8\xad";
        let mut whole = model(4, 40, 10);
        whole.feed(data);
        let mut bytewise = model(4, 40, 10);
        for byte in data {
            bytewise.feed(&[*byte]);
        }
        assert_eq!(whole.snapshot().rows, bytewise.snapshot().rows);
        assert_eq!(
            (whole.snapshot().cursor_row, whole.snapshot().cursor_col),
            (
                bytewise.snapshot().cursor_row,
                bytewise.snapshot().cursor_col
            )
        );
        let a: Vec<_> = marks(&mut whole)
            .into_iter()
            .map(|e| (e.mark, e.anchor))
            .collect();
        let b: Vec<_> = marks(&mut bytewise)
            .into_iter()
            .map(|e| (e.mark, e.anchor))
            .collect();
        assert_eq!(a, b, "整批喂和逐字节喂得到同样的标记和锚点");
        assert_eq!(a.len(), 4);
    }

    #[test]
    fn a_mark_split_across_feeds_still_fires_once_at_the_end() {
        let mut term = model(2, 20, 10);
        term.feed(b"$ \x1b]133;");
        assert!(marks(&mut term).is_empty());
        term.feed(b"B\x07");
        let events = marks(&mut term);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].anchor.col, 2);
        assert!(marks(&mut term).is_empty(), "取走后队列是空的");
    }

    #[test]
    fn local_text_never_produces_shell_marks_even_if_it_contains_them() {
        let mut term = model(2, 40, 10);
        term.feed_local(b"connecting \x1b]133;B\x07 \x1b]133;A\x07 \x1b]7;file:///x\x07\r\n");
        assert!(marks(&mut term).is_empty(), "feed_local 不产生 Prompt 事件");
        // 本地文字里的半截序列也不能被后面的远端字节续上。
        term.feed_local(b"\x1b]133;");
        term.feed(b"B\x07");
        assert!(marks(&mut term).is_empty());
    }

    #[test]
    fn a_new_session_drops_half_marks_and_pending_marks() {
        let mut term = model(2, 40, 10);
        term.feed(b"\x1b]133;A\x07\x1b]133;");
        term.reset_for_new_session();
        assert!(marks(&mut term).is_empty(), "没取走的旧标记作废");
        term.feed(b"B\x07");
        assert!(
            marks(&mut term).is_empty(),
            "旧会话的半截序列不会被新会话续上"
        );
        term.feed(b"\x1b]133;A\x07");
        assert_eq!(marks(&mut term).len(), 1, "新会话照常识别");
    }

    #[test]
    fn the_mark_queue_is_bounded() {
        let mut term = model(2, 40, 10);
        for _ in 0..(MAX_PENDING_MARKS + 50) {
            term.feed(b"\x1b]133;A\x07");
        }
        assert_eq!(marks(&mut term).len(), MAX_PENDING_MARKS);
        assert_eq!(term.dropped_shell_events(), 50);
    }

    #[test]
    fn marks_on_the_alternate_screen_say_so() {
        let mut term = model(3, 20, 10);
        term.feed(b"\x1b[?1049h\x1b]133;B\x07");
        let events = marks(&mut term);
        assert!(events[0].anchor.alt_screen);
    }

    #[test]
    fn line_cells_keep_trailing_spaces_wide_chars_and_combining_marks() {
        let mut term = model(2, 10, 10);
        term.feed("a 你e\u{301}  ".as_bytes());
        let cells = term.line_cells(0).expect("第 0 行");
        assert_eq!(cells.len(), 10);
        assert_eq!(cells[0].ch, 'a');
        assert_eq!(cells[1].ch, ' ');
        assert_eq!(cells[2].ch, '你');
        assert_eq!(cells[2].kind, LineCellKind::Wide);
        assert_eq!(cells[3].kind, LineCellKind::WideSpacer);
        assert_eq!(cells[4].ch, 'e');
        assert_eq!(cells[4].zerowidth.as_deref(), Some(&['\u{301}'][..]));
        assert_eq!((cells[5].ch, cells[6].ch), (' ', ' '), "尾部空格保留");
        assert!(term.line_cells(2).is_none());
        // 裁过尾空格的 RowSnapshot.text 看不出这两个空格，这正是不能用它的原因。
        assert_eq!(term.snapshot().rows[0].text, "a 你e\u{301}");
    }

    #[test]
    fn the_screen_probe_reads_the_real_cursor_and_ignores_scrollback() {
        let mut term = model(3, 10, 50);
        for i in 0..10 {
            term.feed(format!("l{i}\r\n").as_bytes());
        }
        term.feed(b"ab\x1b[?25l");
        let probe = term.screen_probe();
        assert_eq!((probe.rows, probe.cols), (3, 10));
        assert_eq!(
            (probe.cursor_row, probe.cursor_col),
            (2, 2),
            "光标被远端隐藏也照读"
        );
        assert_eq!(term.cursor_visible(), None);
        term.scroll(4);
        let scrolled = term.screen_probe();
        assert_eq!(scrolled.display_offset, 4);
        assert_eq!(
            (scrolled.cursor_row, scrolled.cursor_col),
            (2, 2),
            "回滚不改网格光标"
        );
        assert!(scrolled.revision > probe.revision);
        assert_eq!(
            scrolled.content_revision, probe.content_revision,
            "滚动不算内容变化"
        );
    }
}
