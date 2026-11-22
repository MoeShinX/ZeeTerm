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
mod rowlog;

use rowlog::{FEED_SLICE, HISTORY_SLACK, RowLog};

pub use osc::{
    MAX_CWD_BYTES, MAX_OSC_PAYLOAD, MAX_OSC_STREAM_BYTES, MarkFamily, OscGuard, OscMark,
    OscScanner, ShellMarkKind,
};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::search::{RegexIter, RegexSearch};
use alacritty_terminal::term::{ClipboardType, Config, Osc52, Term, TermMode};
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
    /// 这一行是软折行的前半段（终端宽度放不下，延续到下一行）：复制跨行选区时，
    /// 折行处不该插换行——用户看到的一串字符就是一串，不是多行。
    pub wrapped: bool,
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

/// 远端用 OSC 52 一次最多能让我们写进剪贴板的文字（解码后的字节数）。
/// 超过的整条丢弃，不截断写入：半截命令 / 半截文字写进剪贴板比什么都不写更糟。
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;

/// 远端请求写入剪贴板的文字：只留最新一条没被取走的。
type ClipboardSlot = Arc<Mutex<Option<String>>>;

/// 把一条 `ClipboardStore` 放进待写槽（过滤规则见 [`Listener`]）。单独拿出来是为了能直接测限额。
fn store_clipboard(slot: &Mutex<Option<String>>, text: String) {
    // 空文本不清空用户的剪贴板：有的程序用空串表示"没有可复制的"。
    if text.is_empty() {
        return;
    }
    if text.len() > MAX_CLIPBOARD_BYTES {
        // 不打印内容（里面可能是密码、令牌）。
        tracing::warn!(
            bytes = text.len(),
            limit = MAX_CLIPBOARD_BYTES,
            "OSC 52 剪贴板写入超过上限，已丢弃"
        );
        return;
    }
    *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(text);
}

/// 把 Alacritty 的事件里"该回给远端的字节"收进队列，把"远端要写剪贴板"的请求收进待写槽。
///
/// `PtyWrite`（DSR 状态 / 光标位置、DA 设备属性、DECRQM 模式查询、`CSI 18 t` 窗口字符尺寸等）进回复队列。
/// 剪贴板（OSC 52）只认**写**：
/// - `ClipboardStore(Clipboard, 文字)`（选择符 `c`，空选择符也算 `c`）进待写槽，只留最新一条；
///   空文本、超过 [`MAX_CLIPBOARD_BYTES`] 的丢掉；主选区（`p` / `s`）不写，Windows 上没有这个概念，
///   Linux 上写进 PRIMARY 也不是用户选中的意思；
/// - `ClipboardLoad`（读剪贴板）永远不回：模型配成 [`Osc52::OnlyCopy`] 时 Alacritty 本来就不会发这个事件，
///   这里再挡一层，以后谁改了配置也不会把本机剪贴板内容回给远端。
///
/// 其余一律忽略：
/// - `ColorRequest`（OSC 10/11/12 颜色查询）：模型不知道主题色，答错比不答更糟，远端会按超时走默认；
/// - `TextAreaSizeRequest`（`CSI 14 t` 像素尺寸）：模型不知道字符格的像素大小，同上；
/// - 标题、响铃、光标闪烁等：与回复无关。
#[derive(Clone)]
struct Listener {
    replies: Arc<Mutex<ReplyQueue>>,
    clipboard: ClipboardSlot,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => {
                let mut queue = self
                    .replies
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                queue.push(text.into_bytes());
            }
            Event::ClipboardStore(ClipboardType::Clipboard, text) => {
                store_clipboard(&self.clipboard, text);
            }
            _ => {}
        }
    }
}

/// 模型的配置：OSC 52 只认"写剪贴板"（[`Osc52::OnlyCopy`]），不认"读剪贴板"——远端程序能往本机剪贴板放东西
/// （Codex、vim、tmux 的复制都靠它），但永远读不到本机剪贴板里的内容。
/// 写之前还要过界面层的开关、锁屏和会话状态检查，见 `TerminalModel::take_clipboard`。
///
/// Alacritty 自己的回滚上限比用户的多 [`HISTORY_SLACK`] 行：喂完每一小段输出后由模型把回滚裁回
/// 用户的上限，裁掉多少就是「丢弃了多少行」，行号靠它不回头（见 `rowlog`）。
fn model_config(history: usize) -> Config {
    Config {
        scrolling_history: history.saturating_add(HISTORY_SLACK),
        osc52: Osc52::OnlyCopy,
        ..Config::default()
    }
}

pub struct TerminalModel {
    term: Term<Listener>,
    processor: Processor,
    history_limit: usize,
    replies: Arc<Mutex<ReplyQueue>>,
    /// 远端用 OSC 52 请求写进剪贴板、还没被取走的文字（只留最新一条），见 [`Self::take_clipboard`]。
    clipboard: ClipboardSlot,
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
    /// 每一行的绝对行号和第一次收到输出的时间（左侧行号 / 时间戳栏用），见 `rowlog`。
    rowlog: RowLog,
    /// 主屏幕的回滚行数（最近一次在主屏幕上裁剪之后）。备用屏幕期间 Alacritty 不让我们看主屏幕的网格，
    /// 这时改回滚上限被悄悄裁掉的行数只能靠它算（见 [`Self::set_history`]）。
    main_history: usize,
}

/// 行号 / 时间戳栏里一行的内容（V2-U49）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GutterRow {
    /// 这一行在会话输出里是第几行（1 起）。软折行的续行、还没有内容的行是 `None`。
    pub number: Option<u64>,
    /// 这一行第一次收到输出的时间（Unix 秒）。续行、还没有内容的行、时间记不住的行是 `None`。
    pub time: Option<i64>,
}

/// 现在的 Unix 秒；系统时钟早于 1970 年之类读不出来时是 `None`（那时的行就不记时间）。
fn unix_now() -> Option<i64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
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
        let clipboard: ClipboardSlot = Arc::new(Mutex::new(None));
        let listener = Listener {
            replies: Arc::clone(&replies),
            clipboard: Arc::clone(&clipboard),
        };
        Self {
            term: Term::new(model_config(history), &size, listener),
            processor: Processor::new(),
            history_limit: history,
            replies,
            clipboard,
            revision: 0,
            content_revision: 0,
            osc: OscScanner::new(),
            osc_guard: OscGuard::new(),
            marks: VecDeque::new(),
            dropped_marks: 0,
            stream_pos: 0,
            rowlog: RowLog::default(),
            main_history: 0,
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

    /// 取走远端用 OSC 52 请求写进本机剪贴板的文字（只写不读；只有选择符 `c` / 空选择符的请求会到这里）。
    ///
    /// 只留最新一条没被取走的：后来的覆盖前面的。调用方每次喂完输出（含后台标签、非聚焦窗格）取一次，
    /// 写之前自己检查开关、锁屏、会话状态，不满足就直接丢掉这一条，不要留着以后再写。
    pub fn take_clipboard(&mut self) -> Option<String> {
        self.clipboard
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
        self.track_rows(unix_now());
        self.revision += 1;
        self.content_revision += 1;
        true
    }

    /// 喂界面自己的文字（"正在连接…"、断线提示）：不是远端的输出，就算里面有查询也不产生回复。
    pub fn feed_local(&mut self, bytes: &[u8]) {
        self.feed_local_at(bytes, unix_now());
    }

    /// [`Self::feed_local`]，但「现在几点」由调用方给（给每一行记收到的时间用；测试用它造时间）。
    pub fn feed_local_at(&mut self, bytes: &[u8], now: Option<i64>) {
        // 同步更新已经超时的话，先结束它：否则这段本地文字也会被攒进同步缓冲。
        self.expire_sync();
        let kept = {
            let mut queue = self
                .replies
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut queue.items)
        };
        // 本地文字里的 OSC 52 同样不是远端的请求：不能写本机剪贴板，也不能顶掉远端已经排队的那一条。
        let kept_clipboard = self.take_clipboard();
        self.advance_tracked(bytes, now);
        self.revision += 1;
        self.content_revision += 1;
        let mut queue = self
            .replies
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        queue.items = kept;
        drop(queue);
        *self
            .clipboard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = kept_clipboard;
    }

    /// 消费一批字节，返回实际交给状态机的字节数（供 ACK 账本使用）。
    ///
    /// 先对原始字节做一遍 OSC 扫描；有 shell 标记时在序列结尾处把批次切开，让 Alacritty 先处理到
    /// 那里，这样记下的光标位置就是标记发生那一刻的（`133;B` 之后同一批里的回显不会混进去）。
    /// 解析器是有状态的，切开喂和整批喂结果一样。
    pub fn feed(&mut self, raw: &[u8]) -> usize {
        self.feed_at(raw, unix_now())
    }

    /// [`Self::feed`]，但「现在几点」由调用方给（见 [`Self::feed_local_at`]）。
    pub fn feed_at(&mut self, raw: &[u8], now: Option<i64>) -> usize {
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
        let clipboard_before = self.take_clipboard();
        let overflowed_before = self.osc_guard.overflowed();
        let guarded = self.osc_guard.filter(raw);
        let truncated_here = self.osc_guard.overflowed() != overflowed_before;
        let bytes: &[u8] = &guarded;
        let mut found = Vec::new();
        self.osc.scan(bytes, &mut found);
        if found.is_empty() {
            self.advance_tracked(bytes, now);
        } else {
            let mut start = 0;
            for (end, mark) in found {
                self.advance_tracked(&bytes[start..end], now);
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
            self.advance_tracked(&bytes[start..], now);
        }
        self.stream_pos += raw.len() as u64;
        self.settle_clipboard(clipboard_before, truncated_here);
        if !raw.is_empty() {
            self.revision += 1;
            self.content_revision += 1;
        }
        raw.len()
    }

    /// 一批输出喂完后收拾待写剪贴板槽：`before` 是这批开始时槽里原有的（已先取走，免得被这批覆盖时丢失）。
    ///
    /// 这批里有 OSC 被 [`OscGuard`] 截断时，被截的那条 OSC 52 会以"前缀"派发——base64 前缀能解出来，
    /// 得到的是半截文字，不能写进剪贴板：这批新收到的请求一律作废，槽恢复成批前的样子
    /// （同一批里排在被截那条之前的正常请求也会一起丢，这种批次本来就不正常，宁可少写）。
    fn settle_clipboard(&mut self, before: Option<String>, truncated: bool) {
        let mut slot = self
            .clipboard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if truncated || slot.is_none() {
            *slot = before;
        }
    }

    /// 把一段字节交给状态机，每 [`FEED_SLICE`] 字节记一次账（见 `rowlog`）。
    fn advance_tracked(&mut self, bytes: &[u8], now: Option<i64>) {
        for chunk in bytes.chunks(FEED_SLICE) {
            self.processor.advance(&mut self.term, chunk);
            self.track_rows(now);
        }
    }

    /// 把主屏幕的回滚裁回用户设的上限，裁掉的行数记进 `dropped`。
    ///
    /// 只能在主屏幕上调用：备用屏幕的网格没有回滚。先收紧再放宽上限是 Alacritty 唯一公开的
    /// 「裁掉最老的行」的办法（`Grid::update_history` 只收不放，上限要再设回去）。
    fn trim_history(&mut self) {
        let limit = self.history_limit;
        let grid = self.term.grid_mut();
        let history = grid.history_size();
        if history > limit {
            self.rowlog.dropped += (history - limit) as u64;
            grid.update_history(limit);
            grid.update_history(limit.saturating_add(HISTORY_SLACK));
        }
        self.main_history = grid.history_size();
    }

    /// 喂完一小段输出后的记账：裁回滚、给新出现的行记收到的时间。备用屏幕上什么也不做
    /// （全屏程序的画面不编号；它进出时主屏幕的行不变）。
    fn track_rows(&mut self, now: Option<i64>) {
        if self.alt_screen() {
            return;
        }
        self.trim_history();
        let grid = self.term.grid();
        let history = grid.history_size() as u64;
        let rows = grid.screen_lines();
        let cursor_row = grid.cursor.point.line.0.max(0) as usize;
        let log = &mut self.rowlog;
        let screen_top = log.dropped + history;
        if log.stale {
            // 备用屏幕期间改过列数：旧记录对不上了，从现在起重新记，已有的行显示空。
            log.clear_times();
            log.stamped_upto = screen_top;
            log.stale = false;
        }
        log.fit(screen_top + rows as u64);
        // 刚滚进回滚的行不会再变了：没有时间的补上。
        for abs in log.stamped_upto.max(log.dropped)..screen_top {
            log.stamp(abs, now);
        }
        log.stamped_upto = screen_top;
        // 屏幕上：光标已经走过的行（在光标上方）、或者有内容的行算「收到过输出」；光标所在的空行还没有。
        for row_ix in 0..rows {
            let abs = screen_top + row_ix as u64;
            if log.time_of(abs).is_some() {
                continue;
            }
            if row_ix < cursor_row || !grid[Line(row_ix as i32)].is_clear() {
                log.stamp(abs, now);
            }
        }
    }

    /// 这一行是不是软折行的前半段（延续到下一行）：和 Alacritty 自己一样只看行尾格。
    fn row_wraps(&self, line: i32) -> bool {
        let grid = self.term.grid();
        let last = grid.columns().saturating_sub(1);
        grid[Line(line)][Column(last)]
            .flags
            .contains(Flags::WRAPLINE)
    }

    /// 回滚 + 屏幕里每个「有内容」的逻辑行（一串软折行的物理行）的起始物理行下标，只到最后一个有内容的逻辑行为止。
    ///
    /// 折行重排前后各取一次，两边**必须用同一套行边界**：重排只保证逻辑行不增不减，尾部的空行
    /// （光标下面的空屏、已经走过但没写字的行）在两边数量不一定一样，一边留着一边裁掉，
    /// 从后往前对齐就整体偏一位，老行的时间被别的行盖掉。所以边界只看内容，不看有没有时间。
    fn logical_line_starts(&self) -> Vec<usize> {
        let grid = self.term.grid();
        let history = grid.history_size();
        let total = history + grid.screen_lines();
        let mut starts: Vec<usize> = Vec::new();
        let mut starts_line = true;
        for idx in 0..total {
            if starts_line {
                starts.push(idx);
            }
            starts_line = !self.row_wraps(idx as i32 - history as i32);
        }
        let mut end = total;
        while let Some(&first) = starts.last() {
            let blank = (first..end).all(|idx| grid[Line(idx as i32 - history as i32)].is_clear());
            if !blank {
                break;
            }
            starts.pop();
            end = first;
        }
        starts
    }

    /// 每个逻辑行（见 [`Self::logical_line_starts`]）第一行的时间。折行重排前后各取一次，按顺序对上。
    fn logical_line_times(&self) -> Vec<Option<i64>> {
        self.logical_line_starts()
            .into_iter()
            .map(|idx| self.rowlog.time_of(self.rowlog.dropped + idx as u64))
            .collect()
    }

    /// 折行重排之后，把 [`Self::logical_line_times`] 记下的时间按逻辑行分回去。
    ///
    /// 重排不会增删逻辑行，只会在顶端丢掉超出回滚的老行，所以从最新的一行往回对齐；
    /// 对不上的（更老的行、旧记录里没有的行）显示空。续行不带时间。
    fn restore_logical_line_times(&mut self, old: &[Option<i64>]) {
        let starts = self.logical_line_starts();
        let grid = self.term.grid();
        let history = grid.history_size();
        let total = history + grid.screen_lines();
        let log = &mut self.rowlog;
        log.clear_times();
        log.fit(log.dropped + total as u64);
        let count = starts.len();
        for (k, &first) in starts.iter().enumerate() {
            // 第 k 个新逻辑行 <-> 旧记录里从后往前数同一位置的那个。
            let from_end = count - k;
            let time = old.len().checked_sub(from_end).and_then(|i| old[i]);
            log.set(log.dropped + first as u64, time);
        }
        log.stamped_upto = log.dropped + history as u64;
    }

    /// 屏幕上每一行的行号 / 时间戳栏内容（V2-U49）：和 [`Snapshot::rows`] 一一对应。
    ///
    /// - 行号 = 这一行之前被挤出回滚的行数 + 它在回滚 + 屏幕里的位置 + 1，所以回滚满了以后编号
    ///   继续往上走，不会回头；改列数（重新折行）后物理行数变了，编号按新的行位置重排。
    /// - 软折行的续行（上一行是软折行的前半段）不显示编号和时间，只有逻辑行的第一行显示。
    /// - 备用屏幕上整栏都是空的；还没有内容的行（光标下面的空屏）也是空的。
    pub fn gutter_rows(&self) -> Vec<GutterRow> {
        let grid = self.term.grid();
        let rows = grid.screen_lines();
        if self.alt_screen() {
            return vec![GutterRow::default(); rows];
        }
        let history = grid.history_size();
        let offset = grid.display_offset();
        let cursor_row = grid.cursor.point.line.0.max(0) as usize;
        let first = history.saturating_sub(offset);
        (0..rows)
            .map(|row_ix| {
                let idx = first + row_ix;
                let line = idx as i32 - history as i32;
                let continuation = idx > 0 && self.row_wraps(line - 1);
                if continuation {
                    return GutterRow::default();
                }
                let abs = self.rowlog.dropped + idx as u64;
                let time = self.rowlog.time_of(abs);
                let reached = time.is_some() || (idx >= history && idx - history <= cursor_row);
                GutterRow {
                    number: reached.then_some(abs + 1),
                    time,
                }
            })
            .collect()
    }

    /// 现在记着多少行的时间（测试 / 诊断用：内存有上限）。
    pub fn tracked_time_rows(&self) -> usize {
        self.rowlog.tracked()
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
        // 行号不回头：光标行（通常是提示符）挪到第一行后还是原来的编号，被清掉的行只是挤出了顶端。
        let history_before = self.term.grid().history_size();
        self.rowlog.dropped += (history_before + cursor_line) as u64;
        self.rowlog.stamped_upto = self.rowlog.dropped;
        let grid = self.term.grid_mut();
        grid.clear_history();
        self.main_history = 0;
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
        // 第一行以外都是空行了：旧时间不能留给它们。
        self.rowlog.fit(self.rowlog.dropped + 1);
        self.rowlog.truncate_to(self.rowlog.dropped + 1);
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
        // 旧会话请求写剪贴板、还没被取走的文字同样作废。
        let _ = self.take_clipboard();
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
        let reflows = size.cols != self.term.grid().columns() && size.cols > 0;
        // 改列数会让 Alacritty 重新折行，物理行的数量和位置都变；按逻辑行（一串软折行的行）把时间带过去。
        // 备用屏幕上看不到主屏幕的网格，没法带，回到主屏幕时记录作废。
        let carried = if reflows && !self.alt_screen() {
            Some(self.logical_line_times())
        } else {
            None
        };
        self.term.resize(size);
        if reflows && carried.is_none() {
            self.rowlog.stale = true;
        }
        if !self.alt_screen() {
            self.trim_history();
            if let Some(times) = carried {
                self.restore_logical_line_times(&times);
            }
        }
        self.revision += 1;
        self.content_revision += 1;
    }

    /// 改回滚上限，对已经在用的终端立刻生效（0.2.14 里改设置会同步给已开的 xterm）：
    /// 调小会丢掉最旧的多余行，调大只是以后能攒更多。
    pub fn set_history(&mut self, history: usize) {
        let alt = self.alt_screen();
        // `set_options` 会先让底层网格按「新上限 + 余量」丢掉老行，之后 `trim_history` 看到的已经是丢过之后的数量，
        // 所以要在改上限之前记下行数，按改前改后的实际差值记账，否则丢掉的这部分没人数，编号会倒退。
        // 备用屏幕上看不到主屏幕的网格，用最近一次在主屏幕上记下的行数。
        let before = if alt {
            self.main_history
        } else {
            self.term.grid().history_size()
        };
        self.term.set_options(model_config(history));
        self.history_limit = history;
        let after = if alt {
            before.min(history.saturating_add(HISTORY_SLACK))
        } else {
            self.term.grid().history_size()
        };
        self.rowlog.dropped += before.saturating_sub(after) as u64;
        if alt {
            self.main_history = after;
        } else {
            self.trim_history();
        }
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
            // 软折行标志（WRAPLINE）打在这一行的行尾格上；重排后可能在行内其他格，任一格有就算。
            let mut wrapped = false;
            for col in 0..cols {
                let cell = &grid[Line(row as i32 - offset)][Column(col)];
                wrapped |= cell.flags.contains(Flags::WRAPLINE);
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
                wrapped,
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
            let continues = row < end_row && snapshot.rows[row].wrapped;
            // 软折行接着往下选时，行尾的空格是那串字符本身的（折行正好断在空格上），不能去掉。
            out.push_str(if continues { &line } else { line.trim_end() });
            if row < end_row && !snapshot.rows[row].wrapped {
                // 行间换行只在真正的硬换行处插：软折行（上一行放不下延续下来）复制出来
                // 还是原来的一串字符，不带分行。
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
    fn a_soft_wrapped_line_copies_without_a_line_break() {
        // 40 个字符挤进 20 列：屏幕上是两行，但逻辑上是一串。
        let mut term = model(3, 20, 10);
        term.feed(b"0123456789ABCDEFGHIJKLMNOPQRSTUV");
        let snap = term.snapshot();
        assert!(snap.rows[0].wrapped, "第一行应是软折行的前半段");
        assert!(!snap.rows[1].wrapped);
        // 选满两行：复制出来是原来的一串，没有换行。
        assert_eq!(
            term.text_between((0, 0), (1, 19)),
            "0123456789ABCDEFGHIJKLMNOPQRSTUV"
        );
        // 只选到第一行结尾也一样没有换行。
        assert_eq!(term.text_between((0, 0), (0, 19)), "0123456789ABCDEFGHIJ");
        // 硬换行仍然保留换行。
        let mut hard = model(3, 20, 10);
        hard.feed(b"0123456789\r\nend");
        assert_eq!(hard.text_between((0, 0), (1, 3)), "0123456789\nend");
    }

    #[test]
    fn a_soft_wrap_that_breaks_on_a_space_keeps_the_space() {
        // 第 20 列（行尾）正好是空格：复制出来 `abcd` 和 `world` 之间的空格还在。
        let mut term = model(3, 20, 10);
        term.feed(b"echo 0123456789abcd world");
        assert!(term.snapshot().rows[0].wrapped);
        assert_eq!(
            term.text_between((0, 0), (1, 4)),
            "echo 0123456789abcd world"
        );
        // 选区只到软折行那一行就结束：行尾空格照旧去掉。
        assert_eq!(term.text_between((0, 0), (0, 19)), "echo 0123456789abcd");
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

    /// 测试用的最小 base64 编码（模型 crate 不为测试多引一个依赖）。
    fn b64(data: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let n = (u32::from(chunk[0]) << 16)
                | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
                | u32::from(*chunk.get(2).unwrap_or(&0));
            out.push(T[(n >> 18) as usize & 63] as char);
            out.push(T[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                T[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                T[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }

    /// 一条 OSC 52：`selector` 是选择符（可为空），`bel` 决定用 BEL 还是 `ESC \` 结尾。
    fn osc52(selector: &str, text: &str, bel: bool) -> Vec<u8> {
        let end = if bel { "\x07" } else { "\x1b\\" };
        format!("\x1b]52;{selector};{}{end}", b64(text.as_bytes())).into_bytes()
    }

    #[test]
    fn osc52_color_and_size_queries_produce_no_reply_and_clipboard_requests_never_do() {
        let mut term = model(4, 40, 10);
        // 写剪贴板（两种终止符）、读剪贴板、主选区读：全都不产生任何回复。
        term.feed(b"\x1b]52;c;aGVsbG8=\x07");
        term.feed(b"\x1b]52;c;aGVsbG8=\x1b\\");
        term.feed(b"\x1b]52;c;?\x07");
        term.feed(b"\x1b]52;p;?\x1b\\");
        term.feed(b"\x1b]52;;?\x07");
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
    fn a_remote_clipboard_write_is_picked_up_with_either_terminator() {
        let mut term = model(4, 40, 10);
        assert_eq!(term.take_clipboard(), None, "没有请求就没有东西");
        term.feed(&osc52("c", "hello", true));
        assert_eq!(term.take_clipboard().as_deref(), Some("hello"));
        assert_eq!(term.take_clipboard(), None, "取走之后没有了");
        term.feed(&osc52("c", "world", false));
        assert_eq!(term.take_clipboard().as_deref(), Some("world"));
        // 这些序列本身不上屏。
        assert_eq!(term.snapshot().rows[0].text, "");
        assert!(term.take_replies().is_empty(), "写不产生任何回复");
    }

    #[test]
    fn clipboard_text_keeps_chinese_and_line_breaks() {
        let mut term = model(4, 40, 10);
        let text = "第一行：你好，世界\n第二行\r\n  缩进的第三行\t制表符 🙂";
        term.feed(&osc52("c", text, true));
        assert_eq!(term.take_clipboard().as_deref(), Some(text));
    }

    /// 空选择符（tmux 的 `set-clipboard` 用法：`ESC ] 52 ; ; <base64>`）：Alacritty 把空选择符当 `c`，
    /// 所以落到剪贴板，不是主选区。`cp` 这类多字符选择符只看第一个字符。
    #[test]
    fn an_empty_selector_means_the_clipboard_like_alacritty_reads_it() {
        let mut term = model(4, 40, 10);
        term.feed(&osc52("", "from tmux", false));
        assert_eq!(term.take_clipboard().as_deref(), Some("from tmux"));
        term.feed(&osc52("cp", "first char wins", true));
        assert_eq!(term.take_clipboard().as_deref(), Some("first char wins"));
        term.feed(&osc52("pc", "primary first", true));
        assert_eq!(term.take_clipboard(), None, "第一个字符是 p：主选区，不写");
    }

    #[test]
    fn primary_selection_requests_are_ignored() {
        let mut term = model(4, 40, 10);
        term.feed(&osc52("p", "primary", true));
        term.feed(&osc52("s", "secondary", false));
        assert_eq!(term.take_clipboard(), None);
        // 不影响紧接着的正常请求。
        term.feed(&osc52("c", "ok", true));
        assert_eq!(term.take_clipboard().as_deref(), Some("ok"));
    }

    #[test]
    fn an_empty_text_never_clears_the_clipboard() {
        let mut term = model(4, 40, 10);
        term.feed(b"\x1b]52;c;\x07");
        term.feed(&osc52("c", "", true));
        assert_eq!(term.take_clipboard(), None);
        // 前面已经有一条没取走的，空请求也不会把它清掉。
        term.feed(&osc52("c", "keep", true));
        term.feed(&osc52("c", "", true));
        assert_eq!(term.take_clipboard().as_deref(), Some("keep"));
    }

    #[test]
    fn invalid_base64_or_text_that_is_not_utf8_is_ignored() {
        let mut term = model(4, 40, 10);
        term.feed(b"\x1b]52;c;not base64!!\x07");
        term.feed(format!("\x1b]52;c;{}\x07", b64(&[0xff, 0xfe, 0xfd])).as_bytes());
        assert_eq!(term.take_clipboard(), None);
    }

    /// 超过 1 MiB 的整条丢弃（不截断）。Alacritty 的事件在这一层之前已经解码，所以直接用监听器测限额；
    /// 刚好 1 MiB 的收下，多一个字节就丢。
    #[test]
    fn text_over_the_limit_is_dropped_whole_and_the_limit_itself_is_allowed() {
        let slot: ClipboardSlot = Arc::new(Mutex::new(None));
        let listener = Listener {
            replies: Arc::new(Mutex::new(ReplyQueue::default())),
            clipboard: Arc::clone(&slot),
        };
        let take = || slot.lock().unwrap().take();
        listener.send_event(Event::ClipboardStore(
            ClipboardType::Clipboard,
            "a".repeat(MAX_CLIPBOARD_BYTES),
        ));
        assert_eq!(take().map(|t| t.len()), Some(MAX_CLIPBOARD_BYTES));
        listener.send_event(Event::ClipboardStore(
            ClipboardType::Clipboard,
            "a".repeat(MAX_CLIPBOARD_BYTES + 1),
        ));
        assert_eq!(take(), None, "多一个字节就整条丢掉，不截断");
        // 超限的不会顶掉已经排着的那条。
        listener.send_event(Event::ClipboardStore(
            ClipboardType::Clipboard,
            "kept".into(),
        ));
        listener.send_event(Event::ClipboardStore(
            ClipboardType::Clipboard,
            "b".repeat(MAX_CLIPBOARD_BYTES + 1),
        ));
        assert_eq!(take().as_deref(), Some("kept"));
        // 主选区的事件即使真的送到这里也不收；读剪贴板的事件即使真的送到这里也不回。
        listener.send_event(Event::ClipboardStore(ClipboardType::Selection, "x".into()));
        listener.send_event(Event::ClipboardLoad(
            ClipboardType::Clipboard,
            Arc::new(|text| format!("leak:{text}")),
        ));
        assert_eq!(take(), None);
        assert!(listener.replies.lock().unwrap().items.is_empty());
    }

    /// 经 `feed` 的整条路径：大半 MiB 的真实 OSC 52 还在 [`OscGuard`] 的上限以内，原样收到；
    /// 超过护栏上限的那条被护栏截断，截出来的前缀能解出半截文字，绝不能当成完整内容写出去。
    #[test]
    fn large_clipboard_writes_pass_the_osc_guard_but_a_truncated_one_is_dropped() {
        let mut term = model(4, 40, 10);
        let medium = "中文abc\n".repeat(100_000 / 8); // 约 100 KB 的字节
        term.feed(&osc52("c", &medium, true));
        assert_eq!(term.take_clipboard().as_deref(), Some(medium.as_str()));
        assert_eq!(
            term.truncated_osc_count(),
            0,
            "100 KB 级的正常请求不被护栏碰"
        );

        let big = "x".repeat(700 * 1024); // base64 后约 933 KB，仍在 1 MiB 护栏内
        term.feed(&osc52("c", &big, false));
        assert_eq!(term.take_clipboard().as_deref(), Some(big.as_str()));
        assert_eq!(term.truncated_osc_count(), 0);

        // 超过护栏：base64 约 1.4 MB。这一条不产生请求，也不顶掉批前就排着的那条。
        // 选择符补 0～3 个 `c`，让被截断处的 base64 长度分别落在 4 的倍数的各个余数上：
        // 正好整除的那种截出来能解码成半截文字，是最危险的情形。
        let huge = "y".repeat(MAX_CLIPBOARD_BYTES);
        for (round, selector) in ["c", "cc", "ccc", "cccc"].into_iter().enumerate() {
            term.feed(&osc52("c", "queued before", true));
            term.feed(&osc52(selector, &huge, true));
            assert_eq!(term.truncated_osc_count(), round as u64 + 1);
            assert_eq!(
                term.take_clipboard().as_deref(),
                Some("queued before"),
                "被截断的不写，之前排着的那条不受影响（选择符 {selector}）"
            );
            assert_eq!(term.take_clipboard(), None);
        }
        // 之后的正常输出和请求照常。
        term.feed(b"after\x1b[1;1H");
        term.feed(&osc52("c", "fine", true));
        assert_eq!(term.take_clipboard().as_deref(), Some("fine"));
    }

    #[test]
    fn a_clipboard_write_split_across_two_feeds_is_picked_up() {
        let mut term = model(4, 40, 10);
        let bytes = osc52("c", "split across feeds", true);
        for cut in [3, 5, 8, bytes.len() - 1] {
            let (a, b) = bytes.split_at(cut);
            term.feed(a);
            assert_eq!(term.take_clipboard(), None, "没收完不能先写（cut={cut}）");
            term.feed(b);
            assert_eq!(
                term.take_clipboard().as_deref(),
                Some("split across feeds"),
                "cut={cut}"
            );
        }
        // 逐字节喂也一样。
        for byte in &bytes {
            term.feed(std::slice::from_ref(byte));
        }
        assert_eq!(term.take_clipboard().as_deref(), Some("split across feeds"));
    }

    #[test]
    fn two_requests_in_a_row_keep_only_the_last_one() {
        let mut term = model(4, 40, 10);
        term.feed(&osc52("c", "first", true));
        term.feed(&osc52("c", "second", true));
        assert_eq!(term.take_clipboard().as_deref(), Some("second"));
        assert_eq!(term.take_clipboard(), None);
        // 同一批里的两条也是后来的覆盖前面的。
        let mut both = osc52("c", "one", true);
        both.extend(osc52("c", "two", false));
        term.feed(&both);
        assert_eq!(term.take_clipboard().as_deref(), Some("two"));
        // 前一条没取走、后一批里没有请求：前一条还在。
        term.feed(&osc52("c", "pending", true));
        term.feed(b"plain output");
        assert_eq!(term.take_clipboard().as_deref(), Some("pending"));
    }

    #[test]
    fn reading_the_clipboard_never_replies_and_never_makes_a_request() {
        let mut term = model(4, 40, 10);
        term.feed(b"\x1b]52;c;?\x07");
        term.feed(b"\x1b]52;c;?\x1b\\");
        term.feed(b"\x1b]52;;?\x07");
        term.feed(b"\x1b]52;p;?\x07\x1b]52;s;?\x07");
        assert!(
            term.take_replies().is_empty(),
            "读请求没有回复，剪贴板内容出不去"
        );
        assert_eq!(term.take_clipboard(), None, "读请求也不是写请求");
        assert_eq!(term.dropped_replies(), 0);
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
    fn local_text_and_a_session_reset_never_make_clipboard_requests() {
        let mut term = model(4, 40, 10);
        // 界面自己的文字里的 OSC 52 不算远端请求，也不顶掉远端已经排着的那条。
        term.feed(&osc52("c", "remote", true));
        term.feed_local(&osc52("c", "local", true));
        assert_eq!(term.take_clipboard().as_deref(), Some("remote"));
        term.feed_local(&osc52("c", "local", true));
        assert_eq!(term.take_clipboard(), None);
        // 换会话：旧会话没取走的请求作废。
        term.feed(&osc52("c", "old session", true));
        term.reset_for_new_session();
        assert_eq!(term.take_clipboard(), None);
    }

    #[test]
    fn changing_the_history_limit_keeps_osc52_write_only() {
        let mut term = model(4, 20, 10);
        term.set_history(50);
        term.feed(b"\x1b]52;c;?\x07");
        assert!(term.take_replies().is_empty(), "改了回滚上限还是不回读请求");
        assert_eq!(term.take_clipboard(), None);
        term.feed(b"\x1b]52;c;aGk=\x07");
        assert_eq!(term.take_clipboard().as_deref(), Some("hi"), "仍然认写请求");
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

    // ---- 行号 / 时间戳栏（V2-U49）----

    fn numbers(term: &TerminalModel) -> Vec<Option<u64>> {
        term.gutter_rows().iter().map(|row| row.number).collect()
    }

    fn times(term: &TerminalModel) -> Vec<Option<i64>> {
        term.gutter_rows().iter().map(|row| row.time).collect()
    }

    fn row_texts(term: &TerminalModel) -> Vec<String> {
        term.snapshot()
            .rows
            .iter()
            .map(|row| row.text.clone())
            .collect()
    }

    #[test]
    fn line_numbers_start_at_one_and_skip_rows_that_have_no_output_yet() {
        let mut term = model(5, 20, 100);
        term.feed_at(b"one\r\ntwo", Some(1));
        assert_eq!(
            numbers(&term),
            [Some(1), Some(2), None, None, None],
            "光标下面还没有内容的行不编号"
        );
        term.feed_at(b"\r\nthree\r\nfour\r\nfive\r\nsix", Some(2));
        // 第 1 行已经滚进回滚：屏幕是 2~6 行。
        assert_eq!(
            numbers(&term),
            [Some(2), Some(3), Some(4), Some(5), Some(6)]
        );
    }

    #[test]
    fn a_soft_wrapped_continuation_has_no_number_or_time() {
        let mut term = model(4, 10, 100);
        term.feed_at(b"abcdefghijklmnopqrstuvwxy\r\nz", Some(7));
        // 25 个字符在 10 列里占 3 行，只有第一行有编号和时间；下一个逻辑行的编号接着物理行位置走。
        let rows = term.gutter_rows();
        assert_eq!(rows[0].number, Some(1));
        assert_eq!(rows[0].time, Some(7));
        assert_eq!(rows[1], GutterRow::default());
        assert_eq!(rows[2], GutterRow::default());
        assert_eq!(rows[3].number, Some(4));
    }

    #[test]
    fn line_numbers_keep_counting_after_the_scrollback_is_full() {
        let mut term = model(3, 20, 5);
        for i in 0..50 {
            term.feed_at(format!("line{i}\r\n").as_bytes(), Some(i));
        }
        // 50 行输出 + 末尾光标所在的空行：屏幕最后一行是第 51 行，回滚只剩 5 行，编号没有回头。
        assert!(term.history_size() <= 5);
        assert_eq!(numbers(&term), [Some(49), Some(50), Some(51)]);
        assert_eq!(row_texts(&term)[0], "line48");
    }

    #[test]
    fn one_big_feed_does_not_lose_count_of_the_dropped_rows() {
        // 一次喂进来的行数远超回滚上限：丢弃数仍然精确（按小段记账）。
        let mut term = model(4, 20, 50);
        let mut big = Vec::new();
        for i in 0..5000 {
            big.extend_from_slice(format!("L{i}\r\n").as_bytes());
        }
        term.feed_at(&big, Some(9));
        assert!(term.history_size() <= 50);
        let rows = term.gutter_rows();
        assert_eq!(rows[3].number, Some(5001), "5000 行输出 + 光标所在的空行");
        assert_eq!(row_texts(&term)[2], "L4999");
        assert_eq!(rows[2].number, Some(5000));
    }

    #[test]
    fn scrolled_back_rows_keep_their_own_numbers() {
        let mut term = model(3, 20, 100);
        for i in 0..20 {
            term.feed_at(format!("l{i}\r\n").as_bytes(), Some(i));
        }
        term.scroll(4);
        let snapshot = term.snapshot();
        let rows = term.gutter_rows();
        for (row, gutter) in snapshot.rows.iter().zip(&rows) {
            let n: u64 = row.text[1..].parse().expect("lN");
            assert_eq!(gutter.number, Some(n + 1), "行 {}", row.text);
        }
    }

    #[test]
    fn rows_get_the_time_of_the_feed_that_first_wrote_them() {
        let mut term = model(5, 20, 100);
        term.feed_at(b"a\r\n", Some(100));
        term.feed_local_at(b"local\r\n", Some(200));
        term.feed_at(b"b", Some(300));
        // 同一行后来被改写，时间还是第一次的。
        term.feed_at(b"\rB!", Some(400));
        assert_eq!(times(&term), [Some(100), Some(200), Some(300), None, None]);
    }

    #[test]
    fn rows_that_scroll_into_history_keep_their_time() {
        let mut term = model(3, 20, 100);
        for i in 0..10 {
            term.feed_at(format!("l{i}\r\n").as_bytes(), Some(1000 + i));
        }
        term.scroll_to_top();
        let rows = term.gutter_rows();
        assert_eq!(rows[0].time, Some(1000));
        assert_eq!(rows[1].time, Some(1001));
        assert_eq!(rows[2].time, Some(1002));
    }

    #[test]
    fn time_tracking_is_bounded_by_the_scrollback_limit() {
        let mut term = model(4, 20, 50);
        for i in 0..20_000 {
            term.feed_at(format!("l{i}\r\n").as_bytes(), Some(i));
        }
        let tracked = term.tracked_time_rows();
        assert!(tracked <= 50 + 4 + HISTORY_SLACK, "tracked={tracked}");
    }

    #[test]
    fn the_alternate_screen_gutter_is_blank_and_the_main_screen_numbers_come_back() {
        let mut term = model(4, 20, 100);
        term.feed_at(b"a\r\nb\r\nc", Some(5));
        let before = term.gutter_rows();
        term.feed_at(b"\x1b[?1049hvim\r\nstuff\r\n\r\n\r\n\r\n\r\n", Some(6));
        assert!(
            term.gutter_rows()
                .iter()
                .all(|row| *row == GutterRow::default())
        );
        assert_eq!(term.gutter_rows().len(), 4, "栏的行数和屏幕一致");
        term.feed_at(b"\x1b[?1049l", Some(7));
        assert_eq!(term.gutter_rows(), before);
    }

    #[test]
    fn widening_the_terminal_reflows_and_each_logical_line_keeps_its_time() {
        let mut term = model(10, 10, 100);
        term.feed_at(b"abcdefghijklmnopqrstuvwxy\r\n", Some(10));
        term.feed_at(b"short\r\n", Some(20));
        term.feed_at(b"tail", Some(30));
        term.resize(Size { rows: 10, cols: 40 });
        let rows = term.gutter_rows();
        let texts = row_texts(&term);
        assert_eq!(texts[0], "abcdefghijklmnopqrstuvwxy");
        assert_eq!(texts[1], "short");
        assert_eq!(texts[2], "tail");
        assert_eq!(
            (rows[0].time, rows[1].time, rows[2].time),
            (Some(10), Some(20), Some(30))
        );
        // 再窄回去：长行又折成几行，第一行带时间、续行不带，后面的行时间没有错位。
        term.resize(Size { rows: 10, cols: 10 });
        term.scroll_to_top();
        let rows = term.gutter_rows();
        let texts = row_texts(&term);
        let long = texts
            .iter()
            .position(|t| t == "abcdefghij")
            .expect("long line");
        assert_eq!(rows[long].time, Some(10));
        assert_eq!(rows[long + 1], GutterRow::default());
        let short = texts.iter().position(|t| t == "short").expect("short");
        assert_eq!(rows[short].time, Some(20));
        let tail = texts.iter().position(|t| t == "tail").expect("tail");
        assert_eq!(rows[tail].time, Some(30));
    }

    #[test]
    fn a_reflow_that_loses_the_old_lines_never_shifts_times_onto_other_lines() {
        // 回滚里的老行在窄屏下超出上限被丢掉：留下的行时间还是各自的，不会往上串。
        let mut term = model(3, 30, 6);
        for i in 0..12 {
            term.feed_at(format!("{i:0>25}\r\n").as_bytes(), Some(100 + i));
        }
        term.resize(Size { rows: 3, cols: 10 });
        assert!(term.history_size() <= 6);
        let texts = row_texts(&term);
        let rows = term.gutter_rows();
        for (text, gutter) in texts.iter().zip(&rows) {
            if let Some(time) = gutter.time {
                // 每个逻辑行 25 个字符 `000…0N`，只有逻辑行的第一段（10 个字符）才带时间。
                let n: i64 = time - 100;
                let want = format!("{n:0>25}");
                assert!(
                    want.starts_with(text.as_str()),
                    "{text} 带着第 {n} 行的时间"
                );
            }
        }
    }

    #[test]
    fn resizing_on_the_alternate_screen_drops_the_main_screen_times_but_never_misplaces_them() {
        let mut term = model(4, 10, 100);
        term.feed_at(b"abcdefghijklmnopqrstuvwxy\r\nz\r\n", Some(10));
        term.feed_at(b"\x1b[?1049h", Some(11));
        term.resize(Size { rows: 4, cols: 30 });
        term.feed_at(b"\x1b[?1049l", Some(12));
        term.feed_at(b"new", Some(13));
        let texts = row_texts(&term);
        let rows = term.gutter_rows();
        for (text, gutter) in texts.iter().zip(&rows) {
            if text == "new" {
                assert_eq!(gutter.time, Some(13));
            } else if let Some(time) = gutter.time {
                // 旧行要么空、要么是回到主屏幕那一刻补的，不能是别的行的旧时间。
                assert!(time >= 12, "{text} 的时间 {time}");
            }
        }
    }

    #[test]
    fn clearing_keeps_the_prompt_line_number_and_drops_stale_times() {
        let mut term = model(4, 20, 100);
        for i in 0..10 {
            term.feed_at(format!("l{i}\r\n").as_bytes(), Some(50));
        }
        term.feed_at(b"$ ", Some(60));
        let before = term.gutter_rows().last().copied().expect("rows");
        term.clear_keep_cursor_line();
        let rows = term.gutter_rows();
        assert_eq!(rows[0].number, before.number, "提示符行的编号不变");
        assert_eq!(rows[0].time, before.time);
        assert!(rows[1..].iter().all(|row| *row == GutterRow::default()));
        assert_eq!(term.history_size(), 0);
    }

    #[test]
    fn lowering_the_limit_counts_the_rows_it_drops() {
        let mut term = model(3, 20, 100);
        for i in 0..60 {
            term.feed_at(format!("l{i}\r\n").as_bytes(), Some(i));
        }
        let last = term.gutter_rows().last().and_then(|row| row.number);
        term.set_history(10);
        assert!(term.history_size() <= 10);
        assert_eq!(
            term.gutter_rows().last().and_then(|row| row.number),
            last,
            "裁掉老行不改变剩下的行的编号"
        );
    }

    #[test]
    fn tracking_does_not_change_what_the_screen_shows() {
        // 同一份输出按 1 字节一次喂和整批喂，屏幕、回滚完全一样，编号和时间也一样。
        let mut whole = model(5, 20, 30);
        let mut bytewise = model(5, 20, 30);
        let mut data = Vec::new();
        for i in 0..200 {
            data.extend_from_slice(format!("row {i} \x1b[31mred\x1b[0m 中文\r\n").as_bytes());
        }
        whole.feed_at(&data, Some(1));
        for byte in &data {
            bytewise.feed_at(std::slice::from_ref(byte), Some(1));
        }
        assert_eq!(whole.snapshot().rows, bytewise.snapshot().rows);
        assert_eq!(whole.history_size(), bytewise.history_size());
        assert_eq!(whole.gutter_rows(), bytewise.gutter_rows());
    }

    /// 回滚上限是模型自己裁的（Alacritty 的上限多留了余量）：用户翻在历史里时，来了新输出看到的画面必须和
    /// 直接用上限配置的 Alacritty 一模一样（包括满了以后视图被顶着走的那种）。
    #[test]
    fn trimming_the_scrollback_ourselves_shows_what_plain_alacritty_shows() {
        let limit = 40;
        let size = Size { rows: 6, cols: 24 };
        let replies = Arc::new(Mutex::new(ReplyQueue::default()));
        let mut plain: Term<Listener> = Term::new(
            Config {
                scrolling_history: limit,
                osc52: Osc52::OnlyCopy,
                ..Config::default()
            },
            &size,
            Listener {
                replies,
                clipboard: Arc::new(Mutex::new(None)),
            },
        );
        let mut processor: Processor = Processor::new();
        let mut ours = model(6, 24, limit);
        let screen = |term: &Term<Listener>| {
            let grid = term.grid();
            let offset = grid.display_offset() as i32;
            let rows: Vec<String> = (0..grid.screen_lines() as i32)
                .map(|row| {
                    (0..grid.columns())
                        .map(|col| grid[Line(row - offset)][Column(col)].c)
                        .collect::<String>()
                        .trim_end()
                        .to_owned()
                })
                .collect();
            (grid.display_offset(), grid.history_size(), rows)
        };
        let mut feed_both = |plain: &mut Term<Listener>, ours: &mut TerminalModel, text: &str| {
            processor.advance(plain, text.as_bytes());
            ours.feed_at(text.as_bytes(), Some(0));
        };
        for i in 0..100 {
            feed_both(&mut plain, &mut ours, &format!("first {i}\r\n"));
        }
        // 翻进历史，再让新输出把回滚顶满、继续往外挤。
        plain.scroll_display(Scroll::Delta(7));
        ours.scroll(7);
        for i in 0..300 {
            feed_both(&mut plain, &mut ours, &format!("second {i}\r\n"));
            let want = screen(&plain);
            let got = (
                ours.display_offset(),
                ours.history_size(),
                ours.snapshot()
                    .rows
                    .iter()
                    .map(|row| row.text.clone())
                    .collect::<Vec<_>>(),
            );
            assert_eq!(got, want, "第 {i} 行输出之后");
        }
    }

    // ---- 审核修复（V2-U54）：行号 / 时间戳记账 ----

    #[test]
    fn narrowing_the_window_keeps_each_old_time_when_a_blank_row_trails_the_output() {
        // A 在第 10 秒、B 在第 20 秒，后面留一个已经走过的空行，光标在再下一行。两行都很短，不会折行。
        let mut term = model(6, 20, 100);
        term.feed_at(b"A\r\n", Some(10));
        term.feed_at(b"B\r\n\r\n", Some(20));
        let time_of = |term: &TerminalModel, text: &str| {
            let position = row_texts(term)
                .iter()
                .position(|row| row == text)
                .expect("row");
            term.gutter_rows()[position].time
        };
        assert_eq!(time_of(&term, "A"), Some(10));
        assert_eq!(time_of(&term, "B"), Some(20));
        term.resize(Size { rows: 6, cols: 19 });
        assert_eq!(time_of(&term, "A"), Some(10), "A 的时间不能被 B 的盖掉");
        assert_eq!(time_of(&term, "B"), Some(20));
        // 来回改几次也一样。
        for cols in [25, 12, 20, 19, 40] {
            term.resize(Size { rows: 6, cols });
            assert_eq!(time_of(&term, "A"), Some(10), "{cols} 列");
            assert_eq!(time_of(&term, "B"), Some(20), "{cols} 列");
        }
    }

    #[test]
    fn reflow_keeps_times_with_blank_rows_between_and_after_the_output() {
        // 空行夹在中间（真实的行间空行）也要对得上；尾部空行有时间、没时间的各种组合都不能带偏对齐。
        let mut term = model(8, 20, 100);
        term.feed_at(b"A\r\n", Some(10));
        term.feed_at(b"\r\n", Some(15));
        term.feed_at(b"B\r\n", Some(20));
        term.feed_at(b"\r\n\r\n", Some(25));
        for cols in [19, 30, 11, 20] {
            term.resize(Size { rows: 8, cols });
            let texts = row_texts(&term);
            let rows = term.gutter_rows();
            let at = |text: &str| texts.iter().position(|row| row == text).expect("row");
            assert_eq!(rows[at("A")].time, Some(10), "{cols} 列");
            assert_eq!(rows[at("B")].time, Some(20), "{cols} 列");
            assert_eq!(
                rows[at("B") - 1].time,
                Some(15),
                "{cols} 列：A 和 B 之间的空行"
            );
        }
    }

    /// 屏幕上每个 `L<n>` 行：编号应该是 n + 1、时间应该是 n（测试里第 n 行的时间就是 n）。
    fn assert_rows_keep_their_identity(term: &TerminalModel, what: &str) {
        let texts = row_texts(term);
        let rows = term.gutter_rows();
        let mut checked = 0;
        for (text, gutter) in texts.iter().zip(&rows) {
            let Some(n) = text.strip_prefix('L').and_then(|n| n.parse::<u64>().ok()) else {
                continue;
            };
            assert_eq!(gutter.number, Some(n + 1), "{what}: {text} 的编号");
            assert_eq!(gutter.time, Some(n as i64), "{what}: {text} 的时间");
            checked += 1;
        }
        assert!(checked > 0, "{what}: 屏幕上没有可核对的行");
    }

    #[test]
    fn lowering_the_limit_from_a_deep_scrollback_does_not_move_the_line_numbers_back() {
        let mut term = model(4, 20, 6000);
        for i in 0..5000 {
            term.feed_at(format!("L{i}\r\n").as_bytes(), Some(i));
        }
        assert_rows_keep_their_identity(&term, "调小之前");
        let before = term.gutter_rows().last().and_then(|row| row.number);
        term.set_history(1000);
        assert!(term.history_size() <= 1000);
        assert_rows_keep_their_identity(&term, "6000 调到 1000 之后");
        assert_eq!(term.gutter_rows().last().and_then(|row| row.number), before);
        // 滚到回滚顶端，那里的行也是自己的编号和时间。
        term.scroll_to_top();
        assert_rows_keep_their_identity(&term, "翻到回滚顶端");
        // 再来新输出，编号接着往下走。
        term.scroll_to_bottom();
        term.feed_at(b"L5000\r\n", Some(5000));
        assert_rows_keep_their_identity(&term, "调小之后继续输出");
    }

    #[test]
    fn lowering_the_limit_while_on_the_alternate_screen_does_not_move_the_line_numbers_back() {
        let mut term = model(4, 20, 6000);
        for i in 0..5000 {
            term.feed_at(format!("L{i}\r\n").as_bytes(), Some(i));
        }
        term.feed_at(b"\x1b[?1049hvim", Some(6000));
        term.set_history(1000);
        term.feed_at(b"\x1b[?1049l", Some(6001));
        assert!(term.history_size() <= 1000);
        assert_rows_keep_their_identity(&term, "备用屏幕上调小、回到主屏幕之后");
    }
}
