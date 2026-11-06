//! 终端的文字输入处理器（LINUX-IME）：接住输入法提交 / 预编辑的文字。
//!
//! 为什么需要：X11 下 GPUI 无条件走 XIM，中文输入法开着时字母键被输入法拿走，拼好的字以"提交"
//! 事件回来；没有输入处理器就被丢掉，用户看到的是"打字没反应"。Wayland 下没有输入处理器时 GPUI
//! 根本不启用 text-input，输入法不介入。所以 Linux 上终端要像编辑框一样注册一个 `EntityInputHandler`。
//!
//! 事件顺序（来自 gpui-pre 0.3.10 / gpui-pre-linux 0.3.10 的源码），这决定了"同一个字不会发两次"：
//!
//! - 普通按键 → `KeyDownEvent` → 终端的 `on_key_down` 编码后直接发。X11 / Wayland 的窗口层在
//!   `KeyDown` 的监听器都没有拦下事件（`propagate` 仍为真）、修饰键只有 Shift 且带 `key_char` 时，
//!   **还会再调一次输入处理器的 `replace_text_in_range(None, key_char)`**。所以终端聚焦时，凡是带
//!   `key_char` 的按键，`on_key_down` 处理完必须 `stop_propagation()`，否则注册了输入处理器之后每个字符
//!   会发两遍。
//! - 输入法提交 → X11 `handle_ime_commit` / Wayland `ImeInput::InsertText` →
//!   `replace_text_in_range(None, 文字)`，不经过 `KeyDown`。例外：Wayland 提交的是单个 ASCII 字符时，
//!   GPUI 把它伪装成一次 `KeyDown`（走终端的按键路径），同样靠上面的拦截避免重复。
//! - 预编辑（拼音还没选字）→ `replace_and_mark_text_in_range`；清空预编辑 → 空串 / `unmark_text` /
//!   `replace_text_in_range(Some(标记范围), "")`。预编辑只存在本地、画在光标处，**不发给远端**。
//!
//! 只在 Linux 上注册（[`INPUT_HANDLER_ENABLED`]）：Windows 走"聚焦终端时把输入法切成英文"，行为不变。

use super::*;
use ::gpui::{ElementInputHandler, EntityInputHandler, UTF16Selection};
use std::ops::Range;
use unicode_width::UnicodeWidthChar as _;

/// 终端要不要注册文字输入处理器。只有 Linux 要：Windows 的输入法由 `crate::ime` 切成英文，不经过这里。
/// 写成常量而不是 `#[cfg]`，两个平台都编译同一份代码（Windows 上不注册，也就永远不会被调用）。
pub(super) const INPUT_HANDLER_ENABLED: bool = cfg!(target_os = "linux");

/// 输入法还没提交的文字（拼音等）。只记本地，不发给远端。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Preedit {
    text: String,
    /// 预编辑里的插入点，UTF-16 偏移；输入法候选框跟着它走。
    caret: usize,
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// UTF-16 偏移换成字节下标：越界取末尾，落在代理对中间取这个字符的开头。
fn byte_index(text: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (index, ch) in text.char_indices() {
        if units + ch.len_utf16() > utf16 {
            return index;
        }
        units += ch.len_utf16();
    }
    text.len()
}

impl Preedit {
    pub(super) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub(super) fn text(&self) -> &str {
        &self.text
    }

    fn utf16_len(&self) -> usize {
        utf16_len(&self.text)
    }

    pub(super) fn clear(&mut self) {
        self.text.clear();
        self.caret = 0;
    }

    /// `replace_and_mark_text_in_range`：`range` 为空是整段替换（X11 / Wayland 永远是这样）；
    /// 给了范围就只换预编辑里的这一段。`selected` 是新文字内部的选区，插入点取它的末端。
    fn mark(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        selected: Option<Range<usize>>,
    ) {
        let base = match range {
            Some(range) if !self.text.is_empty() => {
                let (start, end) = (range.start.min(range.end), range.start.max(range.end));
                let (start_byte, end_byte) =
                    (byte_index(&self.text, start), byte_index(&self.text, end));
                let base = utf16_len(&self.text[..start_byte]);
                self.text.replace_range(start_byte..end_byte, new_text);
                base
            }
            _ => {
                self.text = new_text.to_owned();
                0
            }
        };
        let end = self.utf16_len();
        self.caret = match selected {
            Some(selected) => (base + selected.end).min(end),
            None => (base + utf16_len(new_text)).min(end),
        };
    }

    /// `text_for_range`：返回这段预编辑文字，以及实际取到的范围。
    fn slice(&self, range: Range<usize>) -> Option<(String, Range<usize>)> {
        if self.text.is_empty() {
            return None;
        }
        let len = self.utf16_len();
        let (start, end) = (range.start.min(len), range.end.min(len));
        let (start, end) = (start.min(end), start.max(end));
        let text = self.text[byte_index(&self.text, start)..byte_index(&self.text, end)].to_owned();
        Some((text, start..end))
    }

    /// 预编辑里 UTF-16 偏移之前的文字占几个终端格子（中文占两格）。
    fn cells_before(&self, utf16: usize) -> usize {
        self.text[..byte_index(&self.text, utf16)]
            .chars()
            .map(|ch| ch.width().unwrap_or(0))
            .sum()
    }
}

/// 输入法提交的一段文字该怎么发。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Commit {
    /// 当作键入：和按键同一条路径（广播、回到当前行、选区作废、流控、锁屏都照旧）。
    Typed(Vec<u8>),
    /// 带换行的整段文字：按粘贴处理（多行确认、括号粘贴），不能让输入法绕过确认直接执行多行命令。
    Paste(String),
    /// 没有可发的内容。
    Nothing,
}

/// 提交的文字先去掉终端控制字符（ESC、DEL、C1 等：输入法 / 剪贴板类扩展不该往终端里注入转义序列；
/// Tab 保留），再决定走键入还是粘贴。只有换行的提交（例如某些输入法把回车当提交）是回车键：
/// 每个换行发一个 `\r`，和 Enter 键一致。
pub(super) fn classify_commit(text: &str) -> Commit {
    let cleaned: String = text
        .chars()
        .filter(|ch| !ch.is_control() || matches!(ch, '\t' | '\r' | '\n'))
        .collect();
    if cleaned.is_empty() {
        return Commit::Nothing;
    }
    if !cleaned.contains(['\r', '\n']) {
        return Commit::Typed(cleaned.into_bytes());
    }
    if cleaned.chars().all(|ch| matches!(ch, '\r' | '\n')) {
        // "\r\n" 是一个换行。
        let breaks = cleaned.replace("\r\n", "\n").chars().count();
        return Commit::Typed(vec![b'\r'; breaks]);
    }
    Commit::Paste(cleaned)
}

/// 绘制阶段调用：把终端登记成这一帧的文字输入目标（只有终端聚焦时 GPUI 才会真的登记）。
pub(super) fn register(
    focus: &FocusHandle,
    bounds: Bounds<Pixels>,
    entity: Entity<TerminalWorkspace>,
    window: &mut Window,
    cx: &mut App,
) {
    window.handle_input(focus, ElementInputHandler::new(bounds, entity), cx);
}

/// 预编辑文字盖在光标处：不透明底色盖住下面的格子，下划线表示"还没提交"。
/// `at` 是窗格内的左上角，`colors` 是（前景，底色）。
pub(super) fn preedit_overlay(
    text: &str,
    at: (f32, f32),
    height: f32,
    mono: &::gpui::SharedString,
    font_size: f32,
    colors: (Hsla, Hsla),
) -> AnyElement {
    div()
        .absolute()
        .left(::gpui::px(at.0))
        .top(::gpui::px(at.1))
        .h(::gpui::px(height))
        .flex()
        .items_center()
        .whitespace_nowrap()
        .bg(colors.1)
        .text_color(colors.0)
        .font_family(mono.clone())
        .text_size(::gpui::px(font_size))
        .underline()
        .child(text.to_owned())
        .into_any_element()
}

impl TerminalWorkspace {
    /// 活动窗格光标所在格子的左上角（窗口逻辑像素）；窗格还没排过版（没有几何信息）时是 `None`。
    pub(super) fn ime_cursor_origin(&self) -> Option<(f32, f32)> {
        let tab = self.tabs.active()?;
        let geom = self.pane_geom.get(tab.active_pane)?;
        let snapshot = &tab.active().snapshot;
        let col = snapshot.cursor_col.min(geom.cols.saturating_sub(1));
        Some((
            geom.left + col as f32 * self.cell_width,
            geom.top + snapshot.cursor_row as f32 * self.cell_height,
        ))
    }

    /// 输入法的东西来了先回到当前行（和普通按键一样：不能在历史里输入）。
    fn ime_scroll_to_bottom(&mut self) {
        if let Some(pane) = self.pane_mut()
            && pane.pump().display_offset() != 0
        {
            pane.pump_mut().scroll_to_bottom();
            pane.refresh();
        }
    }

    /// 现在能不能接输入法的文字：终端有焦点、有标签、没有模态的粘贴确认框。
    fn ime_ready(&self, window: &Window) -> bool {
        self.focus.is_focused(window) && !self.tabs.is_empty() && self.pending_paste.is_none()
    }

    /// 提交的文字发出去。走的是键入的那条路径（`send_bytes`），多行文字走粘贴。
    fn ime_commit(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        match classify_commit(text) {
            Commit::Nothing => {}
            Commit::Typed(bytes) => {
                zssh_platform_windows::activity::touch();
                self.cursor_blink.reset();
                self.ime_scroll_to_bottom();
                self.send_bytes(&bytes);
                cx.notify();
            }
            Commit::Paste(text) => {
                zssh_platform_windows::activity::touch();
                self.ime_scroll_to_bottom();
                self.begin_paste(text, window, cx);
            }
        }
    }
}

impl EntityInputHandler for TerminalWorkspace {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        // 终端对输入法暴露的"文档"只有预编辑文字：屏幕内容不是可编辑文本。
        let (text, actual) = self.ime_preedit.slice(range)?;
        *adjusted_range = Some(actual);
        Some(text)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let caret = self.ime_preedit.caret;
        Some(UTF16Selection {
            range: caret..caret,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        (!self.ime_preedit.is_empty()).then(|| 0..self.ime_preedit.utf16_len())
    }

    /// 输入法放弃预编辑（点了别处 / 重置）：预编辑文字丢掉，不发给远端——没选定的拼音不是用户要输入的内容。
    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.ime_preedit.is_empty() {
            self.ime_preedit.clear();
            cx.notify();
        }
    }

    /// 提交。键盘按键不会走到这里（`on_key_down` 已拦下），所以到这里的都是输入法的最终文字。
    /// 范围参数（X11 / Wayland 的"清掉标记范围"传的是 `Some(标记范围), ""`）只用来清预编辑。
    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let had_preedit = !self.ime_preedit.is_empty();
        self.ime_preedit.clear();
        if self.ime_ready(window) {
            self.ime_commit(text, window, cx);
        }
        if had_preedit {
            cx.notify();
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.ime_ready(window) {
            // 模态框开着 / 焦点已经走了：不接新的预编辑，已有的也清掉。
            if !self.ime_preedit.is_empty() {
                self.ime_preedit.clear();
                cx.notify();
            }
            return;
        }
        self.ime_preedit.mark(range, new_text, new_selected_range);
        if !self.ime_preedit.is_empty() {
            zssh_platform_windows::activity::touch();
            self.cursor_blink.reset();
            self.ime_scroll_to_bottom();
        }
        cx.notify();
    }

    /// 输入法候选框的位置：预编辑范围起点对应的格子（没有预编辑就是光标格）。
    /// 宽度按预编辑占的格数算，空范围（插入点）宽度为 0，X11 的 spot 取右下角即插入点的下沿。
    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let (x, y) = self.ime_cursor_origin()?;
        let start = self.ime_preedit.cells_before(range_utf16.start);
        let end = self
            .ime_preedit
            .cells_before(range_utf16.end.max(range_utf16.start));
        Some(Bounds::new(
            ::gpui::point(
                ::gpui::px(x + start as f32 * self.cell_width),
                ::gpui::px(y),
            ),
            ::gpui::size(
                ::gpui::px((end - start) as f32 * self.cell_width),
                ::gpui::px(self.cell_height),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: ::gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }

    /// 模态的粘贴确认框开着时不接文字（Wayland 上这会让输入法跟着关掉）。
    fn accepts_text_input(&self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        !self.tabs.is_empty() && self.pending_paste.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::{Commit, Preedit, byte_index, classify_commit, utf16_len};

    #[test]
    fn utf16_offsets_map_to_char_boundaries() {
        // 'a'(1) '你'(1) '😀'(2 个 UTF-16 单元) 'b'(1)
        let text = "a你😀b";
        assert_eq!(utf16_len(text), 5);
        assert_eq!(byte_index(text, 0), 0);
        assert_eq!(byte_index(text, 1), 1);
        assert_eq!(byte_index(text, 2), 4);
        assert_eq!(byte_index(text, 3), 4, "落在代理对中间取这个字符的开头");
        assert_eq!(byte_index(text, 4), 8);
        assert_eq!(byte_index(text, 5), 9);
        assert_eq!(byte_index(text, 99), text.len(), "越界取末尾");
    }

    #[test]
    fn marking_replaces_the_whole_preedit_unless_a_range_is_given() {
        let mut preedit = Preedit::default();
        preedit.mark(None, "ni", None);
        assert_eq!((preedit.text(), preedit.caret), ("ni", 2));
        preedit.mark(None, "ni'hao", Some(0..2));
        assert_eq!((preedit.text(), preedit.caret), ("ni'hao", 2));
        preedit.mark(Some(0..2), "你", None);
        assert_eq!(preedit.text(), "你'hao");
        assert_eq!(preedit.caret, 1);
        preedit.mark(None, "", None);
        assert!(preedit.is_empty());
        assert_eq!(preedit.caret, 0);
    }

    #[test]
    fn slice_clamps_to_the_preedit() {
        let mut preedit = Preedit::default();
        assert_eq!(preedit.slice(0..3), None, "没有预编辑就没有可读的文字");
        preedit.mark(None, "你好吗", None);
        assert_eq!(preedit.slice(1..3), Some(("好吗".to_owned(), 1..3)));
        assert_eq!(preedit.slice(2..99), Some(("吗".to_owned(), 2..3)));
    }

    #[test]
    fn chinese_characters_take_two_cells() {
        let mut preedit = Preedit::default();
        preedit.mark(None, "你好a", None);
        assert_eq!(preedit.cells_before(0), 0);
        assert_eq!(preedit.cells_before(1), 2);
        assert_eq!(preedit.cells_before(2), 4);
        assert_eq!(preedit.cells_before(3), 5);
    }

    #[test]
    fn plain_text_is_typed_as_is() {
        assert_eq!(
            classify_commit("你好"),
            Commit::Typed("你好".as_bytes().to_vec())
        );
        assert_eq!(classify_commit("a b\tc"), Commit::Typed(b"a b\tc".to_vec()));
        assert_eq!(classify_commit(""), Commit::Nothing);
    }

    #[test]
    fn control_characters_never_reach_the_terminal() {
        assert_eq!(
            classify_commit("\u{1b}[31m"),
            Commit::Typed(b"[31m".to_vec())
        );
        assert_eq!(
            classify_commit("a\u{7f}b\u{85}"),
            Commit::Typed(b"ab".to_vec())
        );
        assert_eq!(classify_commit("\u{1b}\u{7}"), Commit::Nothing);
    }

    #[test]
    fn a_commit_of_only_line_breaks_is_the_enter_key() {
        assert_eq!(classify_commit("\n"), Commit::Typed(vec![b'\r']));
        assert_eq!(classify_commit("\r\n"), Commit::Typed(vec![b'\r']));
        assert_eq!(classify_commit("\n\n"), Commit::Typed(vec![b'\r', b'\r']));
    }

    #[test]
    fn multi_line_text_goes_through_the_paste_path() {
        assert_eq!(
            classify_commit("ls\nrm -rf x\n"),
            Commit::Paste("ls\nrm -rf x\n".to_owned())
        );
    }
}

/// 用 GPUI 的测试平台把真实的 `TerminalWorkspace` 跑起来：按键走窗口的真实分发（`dispatch_keystroke`
/// 和 X11 / Wayland 窗口层的"KeyDown 没被拦下就再当文字插一遍"同一个规则），输入法的提交 / 预编辑
/// 走和平台同一组输入处理器方法，断言发给后端的字节恰好一份。
#[cfg(test)]
mod gpui_tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::super::{Pane, Tab, TerminalWorkspace};
    use super::INPUT_HANDLER_ENABLED;
    use crate::settings_store::AppSettings;
    use ::gpui::{
        App, AppContext as _, Bounds, ElementInputHandler, InputHandler as _, Keystroke,
        TestAppContext, Window, WindowHandle,
    };
    use std::sync::mpsc::Sender;
    use std::sync::{Arc, Mutex};
    use zssh_core::ssh::flow::FlowGate;
    use zssh_session::{CoreFilter, SessionBackend, SessionCommand, SessionEvent, SessionPump};
    use zssh_terminal::Size;

    type Writes = Arc<Mutex<Vec<Vec<u8>>>>;

    /// 记录每次写给会话的字节。
    struct Recorder(Writes);

    impl SessionBackend for Recorder {
        fn send(&mut self, command: SessionCommand) {
            if let SessionCommand::Write(bytes) = command {
                self.0.lock().expect("lock").push(bytes);
            }
        }
    }

    fn recording_pane(writes: &Writes, generation: u64) -> (Pane, Sender<SessionEvent>) {
        let (events_tx, events) = std::sync::mpsc::channel();
        let pump = SessionPump::new(
            Size { rows: 10, cols: 40 },
            100,
            Arc::new(FlowGate::new()),
            CoreFilter::new(),
            generation,
        );
        let pane = Pane::with_parts(
            pump,
            Box::new(Recorder(Arc::clone(writes))),
            events,
            None,
            None,
        );
        (pane, events_tx)
    }

    struct Harness {
        window: WindowHandle<TerminalWorkspace>,
        writes: Writes,
        /// 发送端不能丢：丢了会话通道就当作远端已经退出。
        _events: Vec<Sender<SessionEvent>>,
    }

    impl Harness {
        /// `ime_input` 为真时终端注册输入处理器（Linux 的默认）；Windows 的默认是假。
        fn open(cx: &mut TestAppContext, ime_input: bool) -> Self {
            cx.update(|cx| {
                gpui_kit::init(cx);
                AppSettings::init_for_test(cx);
            });
            let window = cx
                .add_window(|window, cx| TerminalWorkspace::new(window, cx, "ime-test".to_owned()));
            let writes: Writes = Arc::default();
            let (pane, events_tx) = recording_pane(&writes, 1);
            window
                .update(cx, |workspace, window, cx| {
                    workspace.ime_input = ime_input;
                    workspace.tabs.push(Tab::new("t".to_owned(), pane));
                    let focus = workspace.focus.clone();
                    window.focus(&focus, cx);
                    cx.notify();
                })
                .expect("window");
            let harness = Self {
                window,
                writes,
                _events: vec![events_tx],
            };
            harness.draw(cx);
            harness
        }

        fn draw(&self, cx: &mut TestAppContext) {
            cx.update_window(self.window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
            })
            .expect("draw");
        }

        /// 按一个键：和键盘事件走一样的分发（含"没被拦下就再当文字插一遍"）。
        fn key(&self, cx: &mut TestAppContext, keystroke: &'static str) {
            cx.dispatch_keystroke(
                self.window.into(),
                Keystroke::parse(keystroke).expect("keystroke"),
            );
            self.draw(cx);
        }

        /// 像平台那样调输入处理器（X11 的 `handle_ime_commit` / Wayland 的 `InsertText` 等）。
        fn ime<R>(
            &self,
            cx: &mut TestAppContext,
            f: impl FnOnce(&mut ElementInputHandler<TerminalWorkspace>, &mut Window, &mut App) -> R,
        ) -> R {
            let view = self.window.root(cx).expect("root");
            let result = cx
                .update_window(self.window.into(), |_, window, cx| {
                    let mut handler = ElementInputHandler::new(Bounds::default(), view);
                    f(&mut handler, window, cx)
                })
                .expect("ime");
            self.draw(cx);
            result
        }

        fn commit(&self, cx: &mut TestAppContext, text: &str) {
            self.ime(cx, |handler, window, cx| {
                handler.replace_text_in_range(None, text, window, cx);
            });
        }

        fn preedit(&self, cx: &mut TestAppContext, text: &str) {
            self.ime(cx, |handler, window, cx| {
                handler.replace_and_mark_text_in_range(None, text, None, window, cx);
            });
        }

        fn marked(&self, cx: &mut TestAppContext) -> Option<std::ops::Range<usize>> {
            self.ime(cx, |handler, window, cx| {
                handler.marked_text_range(window, cx)
            })
        }

        fn sent(&self) -> Vec<Vec<u8>> {
            self.writes.lock().expect("lock").clone()
        }

        fn sent_text(&self) -> String {
            String::from_utf8_lossy(&self.sent().concat()).into_owned()
        }
    }

    #[test]
    fn only_linux_registers_the_input_handler() {
        assert_eq!(INPUT_HANDLER_ENABLED, cfg!(target_os = "linux"));
    }

    #[gpui::test]
    fn the_default_follows_the_platform(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            AppSettings::init_for_test(cx);
        });
        let window = cx
            .add_window(|window, cx| TerminalWorkspace::new(window, cx, "ime-default".to_owned()));
        let registered = window
            .update(cx, |workspace, _, _| workspace.ime_input)
            .expect("window");
        assert_eq!(registered, cfg!(target_os = "linux"));
    }

    #[gpui::test]
    fn every_ordinary_key_is_sent_exactly_once(cx: &mut TestAppContext) {
        let term = Harness::open(cx, true);
        // 字母、数字、符号、空格、非 ASCII 字符：都带 key_char，窗口层在 KeyDown 没被拦下时会再插一遍。
        for key in ["a", "Z", "7", ".", "space", "好"] {
            term.key(cx, key);
        }
        assert_eq!(
            term.sent(),
            vec![
                b"a".to_vec(),
                b"Z".to_vec(),
                b"7".to_vec(),
                b".".to_vec(),
                b" ".to_vec(),
                "好".as_bytes().to_vec(),
            ]
        );
    }

    #[gpui::test]
    fn control_and_navigation_keys_keep_their_own_encoding(cx: &mut TestAppContext) {
        let term = Harness::open(cx, true);
        for key in [
            "enter",
            "tab",
            "backspace",
            "escape",
            "left",
            "ctrl-a",
            "ctrl-c",
        ] {
            term.key(cx, key);
        }
        assert_eq!(
            term.sent(),
            vec![
                b"\r".to_vec(),
                b"\t".to_vec(),
                vec![0x7f],
                vec![0x1b],
                b"\x1b[D".to_vec(),
                vec![0x01],
                vec![0x03],
            ]
        );
    }

    /// X11 / Wayland 给 Alt+字母的 `key_char` 是这个字母（模拟器会把它丢掉，所以手工构造）：
    /// 终端发 ESC 前缀加字母；窗口层虽然因为修饰键不只有 Shift 不会再插一遍，但模拟分发
    /// 不看修饰键（更严），这里也要只有一份。
    #[gpui::test]
    fn alt_letter_is_sent_once_with_an_escape_prefix(cx: &mut TestAppContext) {
        let term = Harness::open(cx, true);
        cx.dispatch_keystroke(
            term.window.into(),
            Keystroke {
                modifiers: ::gpui::Modifiers::alt(),
                key: "x".to_owned(),
                key_char: Some("x".to_owned()),
            },
        );
        assert_eq!(term.sent(), vec![b"\x1bx".to_vec()]);
    }

    /// Wayland 上输入法单独提交一个 `\n`：GPUI 伪装成 `key == key_char == "\n"` 的按键（真回车键的 `key`
    /// 是 "enter"）。终端要把它当 Enter 发 CR，而且只发一次。
    #[gpui::test]
    fn a_wayland_newline_commit_is_sent_as_enter(cx: &mut TestAppContext) {
        let term = Harness::open(cx, true);
        cx.dispatch_keystroke(
            term.window.into(),
            Keystroke {
                modifiers: ::gpui::Modifiers::default(),
                key: "\n".to_owned(),
                key_char: Some("\n".to_owned()),
            },
        );
        assert_eq!(term.sent(), vec![b"\r".to_vec()]);
    }

    #[gpui::test]
    fn a_commit_from_the_input_method_is_sent_exactly_once(cx: &mut TestAppContext) {
        let term = Harness::open(cx, true);
        term.commit(cx, "你好");
        assert_eq!(term.sent(), vec!["你好".as_bytes().to_vec()]);
    }

    #[gpui::test]
    fn typing_and_committing_interleave_without_duplicates(cx: &mut TestAppContext) {
        let term = Harness::open(cx, true);
        term.key(cx, "l");
        term.key(cx, "s");
        term.key(cx, "space");
        // 拼音：每个字母都是预编辑更新，最后选字提交。
        term.preedit(cx, "n");
        term.preedit(cx, "ni");
        term.preedit(cx, "ni'h");
        term.commit(cx, "你好");
        // 选完字之后的普通按键仍然是按键。
        term.key(cx, "enter");
        assert_eq!(term.sent_text(), "ls 你好\r");
        assert_eq!(term.sent().len(), 5, "每次输入各发一次");
    }

    #[gpui::test]
    fn preedit_text_is_never_sent_and_is_cleared_by_the_commit(cx: &mut TestAppContext) {
        let term = Harness::open(cx, true);
        term.preedit(cx, "nihao");
        assert!(term.sent().is_empty(), "预编辑不发给远端");
        assert_eq!(term.marked(cx), Some(0.."nihao".len()));
        term.commit(cx, "你好");
        assert_eq!(term.marked(cx), None);
        assert_eq!(term.sent(), vec!["你好".as_bytes().to_vec()]);
    }

    #[gpui::test]
    fn a_cancelled_preedit_sends_nothing(cx: &mut TestAppContext) {
        let term = Harness::open(cx, true);
        // Esc / 退格删光：预编辑被清成空串。
        term.preedit(cx, "ni");
        term.preedit(cx, "");
        assert_eq!(term.marked(cx), None);
        // Wayland 的 DeleteText、X11 失去焦点时：把标记范围替换成空串。
        term.preedit(cx, "ni");
        let marked = term.marked(cx);
        term.ime(cx, |handler, window, cx| {
            handler.replace_text_in_range(marked, "", window, cx);
        });
        assert_eq!(term.marked(cx), None);
        // 点鼠标等原因让输入法放弃预编辑。
        term.preedit(cx, "ni");
        term.ime(cx, |handler, window, cx| handler.unmark_text(window, cx));
        assert_eq!(term.marked(cx), None);
        assert!(term.sent().is_empty(), "取消的预编辑一个字节都不发");
        // 取消之后照常输入。
        term.key(cx, "a");
        assert_eq!(term.sent(), vec![b"a".to_vec()]);
    }

    #[gpui::test]
    fn the_candidate_box_follows_the_cursor_cell(cx: &mut TestAppContext) {
        let term = Harness::open(cx, true);
        let (origin, cell) = term
            .window
            .update(cx, |workspace, _, _| {
                (
                    workspace.ime_cursor_origin().expect("laid out"),
                    (workspace.cell_width, workspace.cell_height),
                )
            })
            .expect("window");
        let at_cursor = term
            .ime(cx, |handler, window, cx| {
                handler.bounds_for_range(0..0, window, cx)
            })
            .expect("bounds");
        assert_eq!(f32::from(at_cursor.origin.x), origin.0);
        assert_eq!(f32::from(at_cursor.origin.y), origin.1);
        assert_eq!(f32::from(at_cursor.size.height), cell.1);
        // 预编辑里每个汉字占两格：插入点在第二个字之后时，候选框右移四格。
        term.preedit(cx, "你好");
        let after_two = term
            .ime(cx, |handler, window, cx| {
                handler.bounds_for_range(2..2, window, cx)
            })
            .expect("bounds");
        assert_eq!(f32::from(after_two.origin.x), origin.0 + 4.0 * cell.0);
    }

    #[gpui::test]
    fn a_multi_line_commit_goes_through_the_paste_confirmation(cx: &mut TestAppContext) {
        let term = Harness::open(cx, true);
        term.commit(cx, "ls\nrm x\n");
        assert!(term.sent().is_empty(), "多行文字先等确认，不能直接执行");
        let pending = term
            .window
            .update(cx, |workspace, _, _| workspace.pending_paste.is_some())
            .expect("window");
        assert!(pending);
        // 确认框开着是模态的：输入法的文字和按键都进不了终端。
        term.commit(cx, "x");
        term.key(cx, "y");
        assert!(term.sent().is_empty());
        term.key(cx, "enter");
        assert_eq!(
            term.sent_text(),
            "ls\rrm x\r",
            "确认后整段按粘贴发出，换行转成回车"
        );
    }

    #[gpui::test]
    fn a_lone_line_break_commit_is_the_enter_key(cx: &mut TestAppContext) {
        let term = Harness::open(cx, true);
        term.commit(cx, "\n");
        assert_eq!(term.sent(), vec![b"\r".to_vec()]);
    }

    #[gpui::test]
    fn escape_sequences_in_a_commit_are_dropped(cx: &mut TestAppContext) {
        let term = Harness::open(cx, true);
        term.commit(cx, "\u{1b}]0;x\u{7}ok");
        assert_eq!(term.sent_text(), "]0;xok");
    }

    #[gpui::test]
    fn commits_reach_every_pane_in_broadcast_mode_once_each(cx: &mut TestAppContext) {
        let term = Harness::open(cx, true);
        let second: Writes = Arc::default();
        let (pane, _events) = recording_pane(&second, 2);
        term.window
            .update(cx, |workspace, _, _| {
                let tab = workspace.tabs.active_mut().expect("tab");
                tab.panes.push(pane);
                tab.broadcast = true;
            })
            .expect("window");
        term.commit(cx, "你");
        term.key(cx, "a");
        let expected = vec!["你".as_bytes().to_vec(), b"a".to_vec()];
        assert_eq!(term.sent(), expected, "第一个窗格各一份");
        assert_eq!(
            *second.lock().expect("lock"),
            expected,
            "第二个窗格也各一份"
        );
    }

    #[gpui::test]
    fn without_the_handler_keys_still_arrive_exactly_once(cx: &mut TestAppContext) {
        // Windows 的路径：不注册输入处理器，按键只走按键路径。
        let term = Harness::open(cx, false);
        for key in ["a", "好", "enter"] {
            term.key(cx, key);
        }
        assert_eq!(
            term.sent(),
            vec![b"a".to_vec(), "好".as_bytes().to_vec(), b"\r".to_vec()]
        );
    }
}
