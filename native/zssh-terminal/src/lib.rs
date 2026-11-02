//! 原生终端模型：把字节流交给 Alacritty 的状态机，产出有界快照。
//!
//! 这一层不碰 SSH、不碰 GPUI：调用方从 `ShellEventStream` 拿到字节，按批喂进来；
//! 绘制层按需取可见区快照。回滚、备用屏、UTF-8 跨块、宽字符都由 Alacritty 处理。
//!
//! 注意：Alacritty 的 `Term` 不是 `Send`，解析必须留在同一个线程/任务里；
//! 这也是后续专用解析线程的边界。

use std::sync::Arc;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
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
}

struct Listener;

impl EventListener for Listener {
    fn send_event(&self, _event: Event) {}
}

pub struct TerminalModel {
    term: Term<Listener>,
    processor: Processor,
    history_limit: usize,
}

impl TerminalModel {
    pub fn new(size: Size, history: usize) -> Self {
        let config = Config {
            scrolling_history: history,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &size, Listener),
            processor: Processor::new(),
            history_limit: history,
        }
    }

    /// 消费一批字节，返回实际交给状态机的字节数（供 ACK 账本使用）。
    pub fn feed(&mut self, bytes: &[u8]) -> usize {
        self.processor.advance(&mut self.term, bytes);
        bytes.len()
    }

    pub fn resize(&mut self, size: Size) {
        self.term.resize(size);
    }

    /// 改回滚上限，对已经在用的终端立刻生效（0.2.14 里改设置会同步给已开的 xterm）：
    /// 调小会丢掉最旧的多余行，调大只是以后能攒更多。
    pub fn set_history(&mut self, history: usize) {
        let config = Config {
            scrolling_history: history,
            ..Config::default()
        };
        self.term.set_options(config);
        self.history_limit = history;
    }

    /// 当前的回滚上限。
    pub fn history_limit(&self) -> usize {
        self.history_limit
    }

    /// 正数向下滚（看历史），负数向上滚。
    /// 正数向上翻历史，负数向下回当前。
    pub fn scroll(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(delta));
    }

    pub fn scroll_to_top(&mut self) {
        self.term.scroll_display(Scroll::Top);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
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

    #[test]
    fn plain_text_and_cursor() {
        let mut term = model(4, 20, 100);
        term.feed(b"hello\r\nworld");
        let snap = term.snapshot();
        assert_eq!(snap.rows[0].text, "hello");
        assert_eq!(snap.rows[1].text, "world");
        assert_eq!((snap.cursor_row, snap.cursor_col), (1, 5));
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
}
