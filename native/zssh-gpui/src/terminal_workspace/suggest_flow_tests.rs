//! 命令建议在工作区这一层的行为：真实的按键 / 鼠标 / 输入法分发、浮层模型和位置（含底部翻转）、
//! 绑定键、广播零生成、敏感条目不展示（含悬浮提示）。
//!
//! 用 GPUI 的测试平台把真实的 `TerminalWorkspace` 跑起来：按键走窗口的真实分发，后端是会回显的假 shell，
//! 输入行标记由测试通过会话事件注入（和真实远端发来的一模一样）。

// 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
use super::assist_ops::SuggestRefusal;
use super::suggest_ui::{
    HEADER_H, OverlayModel, ROW_H, binding_text, bound_key_matches, content_width, overlay_fits,
    overlay_height, overlay_model, place,
};
use super::{Pane, PaneState, Tab, TerminalWorkspace};
use crate::app_prefs::AppPrefs;
use crate::settings_store::AppSettings;
use ::gpui::{
    AppContext as _, KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, MouseButton, MouseDownEvent,
    PlatformInput, TestAppContext, WindowHandle, point, px, size,
};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zssh_core::ssh::flow::FlowGate;
use zssh_core::terminal_assist::suggest::CandidateSource;
use zssh_core::terminal_assist::{Phase, WriteOutcome};
use zssh_session::{CoreFilter, SessionBackend, SessionCommand, SessionEvent, SessionPump};
use zssh_terminal::Size;

type Writes = Arc<Mutex<Vec<Vec<u8>>>>;

/// 会回显的假 shell 后端：记录写入，并把写入原样回送成输出（同步回环）。
struct EchoShell {
    writes: Writes,
    events: Sender<SessionEvent>,
}

impl SessionBackend for EchoShell {
    fn send(&mut self, command: SessionCommand) {
        if let SessionCommand::Write(bytes) = command {
            let _ = self.send_write(bytes);
        }
    }

    fn send_write(&mut self, bytes: Vec<u8>) -> WriteOutcome {
        self.writes.lock().expect("lock").push(bytes.clone());
        let _ = self.events.send(SessionEvent::Data {
            generation: 1,
            bytes,
        });
        WriteOutcome::Queued
    }
}

/// 全进程共用的"本机设置"缓存会被这些测试改写，所以它们串行跑。
static SERIAL: Mutex<()> = Mutex::new(());

struct Harness {
    window: WindowHandle<TerminalWorkspace>,
    writes: Writes,
    remote: Sender<SessionEvent>,
    _guard: Option<std::sync::MutexGuard<'static, ()>>,
}

thread_local! {
    /// 这个线程已经拿着 `SERIAL`（同一个用例里开了不止一个 `Harness`）。
    static HOLDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl Drop for Harness {
    fn drop(&mut self) {
        if self._guard.is_some() {
            HOLDING.with(|held| held.set(false));
        }
    }
}

impl Harness {
    fn open(cx: &mut TestAppContext) -> Self {
        Self::open_with(cx, AppPrefs::default())
    }

    fn open_with(cx: &mut TestAppContext, prefs: AppPrefs) -> Self {
        let guard = if HOLDING.with(std::cell::Cell::get) {
            None
        } else {
            let guard = SERIAL
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            HOLDING.with(|held| held.set(true));
            Some(guard)
        };
        crate::app_prefs::refresh_cached(prefs);
        cx.update(|cx| {
            gpui_kit::init(cx);
            AppSettings::init_for_test(cx);
        });
        let window = cx
            .add_window(|window, cx| TerminalWorkspace::new(window, cx, "suggest-test".to_owned()));
        let writes: Writes = Arc::default();
        let (events_tx, events) = std::sync::mpsc::channel();
        let pump = SessionPump::new(
            Size { rows: 10, cols: 60 },
            100,
            Arc::new(FlowGate::new()),
            CoreFilter::new(),
            1,
        );
        let pane = Pane::with_parts(
            pump,
            Box::new(EchoShell {
                writes: Arc::clone(&writes),
                events: events_tx.clone(),
            }),
            events,
            Some(7),
            None,
        );
        window
            .update(cx, |workspace, window, cx| {
                workspace.tabs.push(Tab::new("t".to_owned(), pane));
                let focus = workspace.focus.clone();
                window.focus(&focus, cx);
                cx.notify();
            })
            .expect("window");
        let harness = Self {
            window,
            writes,
            remote: events_tx,
            _guard: guard,
        };
        harness.draw(cx);
        harness.output(cx, b"\x1b]133;A\x07$ \x1b]133;B\x07");
        harness
    }

    fn draw(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .expect("draw");
    }

    /// 远端发来输出，随后让工作区像每 60 ms 的节拍那样轮询一次。
    fn output(&self, cx: &mut TestAppContext, bytes: &[u8]) {
        self.remote
            .send(SessionEvent::Data {
                generation: 1,
                bytes: bytes.to_vec(),
            })
            .expect("send");
        self.poll(cx);
    }

    fn poll(&self, cx: &mut TestAppContext) {
        self.window
            .update(cx, |workspace, _, _| {
                workspace.poll_sessions();
            })
            .expect("poll");
        self.draw(cx);
    }

    fn with_pane<R>(&self, cx: &mut TestAppContext, f: impl FnOnce(&mut Pane) -> R) -> R {
        self.window
            .update(cx, |workspace, _, _| f(workspace.pane_mut().expect("pane")))
            .expect("window")
    }

    fn key(&self, cx: &mut TestAppContext, keystroke: Keystroke) {
        cx.dispatch_keystroke(self.window.into(), keystroke);
        self.draw(cx);
    }

    fn named(&self, cx: &mut TestAppContext, source: &'static str) {
        self.key(cx, Keystroke::parse(source).expect("keystroke"));
    }

    /// 按住一个键产生的重复事件（`is_held`）。
    fn held(&self, cx: &mut TestAppContext, source: &'static str) {
        let keystroke = Keystroke::parse(source).expect("keystroke");
        cx.update_window(self.window.into(), |_, window, cx| {
            window.dispatch_event(
                PlatformInput::KeyDown(KeyDownEvent {
                    keystroke,
                    is_held: true,
                    prefer_character_input: false,
                }),
                cx,
            );
        })
        .expect("held");
        self.draw(cx);
    }

    fn release(&self, cx: &mut TestAppContext, source: &'static str) {
        let keystroke = Keystroke::parse(source).expect("keystroke");
        cx.update_window(self.window.into(), |_, window, cx| {
            window.dispatch_event(PlatformInput::KeyUp(KeyUpEvent { keystroke }), cx);
        })
        .expect("release");
    }

    /// 逐字符敲一段文字。
    fn type_text(&self, cx: &mut TestAppContext, text: &str) {
        for ch in text.chars() {
            let key = if ch == ' ' {
                "space".to_owned()
            } else {
                ch.to_string()
            };
            self.key(
                cx,
                Keystroke {
                    modifiers: Modifiers::default(),
                    key,
                    key_char: Some(ch.to_string()),
                },
            );
        }
    }

    /// 敲一行、回车，远端给出命令开始和结束标记以及新的提示符。
    fn run(&self, cx: &mut TestAppContext, line: &str) {
        self.type_text(cx, line);
        self.named(cx, "enter");
        self.output(cx, b"\x1b]133;C\x07");
        self.output(cx, b"\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07");
    }

    /// 让输入稳定下来（跳过 80 ms 的真实等待）。
    fn settle(&self, cx: &mut TestAppContext) {
        self.poll(cx);
        for ms in [10_000u64, 20_000] {
            self.with_pane(cx, |pane| {
                pane.observe_input_at(Instant::now() + Duration::from_millis(ms));
            });
        }
        self.draw(cx);
    }

    /// 把窗口（也就是唯一一个窗格）改成这么大，排两次版让量到的区域生效。
    fn resize(&self, cx: &mut TestAppContext, w: f32, h: f32) {
        cx.simulate_window_resize(self.window.into(), size(px(w), px(h)));
        self.draw(cx);
        self.draw(cx);
    }

    /// 按工作区现在的几何，聚焦窗格的光标这里画不画得下浮层。
    fn overlay_has_room(&self, cx: &mut TestAppContext) -> bool {
        self.window
            .update(cx, |workspace, _, _| {
                let cell_h = workspace.cell_height;
                let pane = workspace.pane().expect("pane");
                let rect = workspace.layout.rect_of(pane.id).expect("rect");
                overlay_fits(
                    8.0 + pane.snapshot.cursor_row as f32 * cell_h,
                    cell_h,
                    rect.h,
                )
            })
            .expect("window")
    }

    fn sent(&self) -> Vec<Vec<u8>> {
        self.writes.lock().expect("lock").clone()
    }

    fn shown(&self, cx: &mut TestAppContext) -> Vec<String> {
        self.with_pane(cx, |pane| {
            pane.assist
                .suggest_view()
                .map(|view| view.items.iter().map(|item| item.text.clone()).collect())
                .unwrap_or_default()
        })
    }

    fn select_mode(&self, cx: &mut TestAppContext) -> bool {
        self.with_pane(cx, |pane| pane.assist.in_select_mode())
    }

    fn history_and_typed(&self, cx: &mut TestAppContext) {
        self.run(cx, "git status");
        self.run(cx, "git status");
        self.run(cx, "git stash");
        self.run(cx, "ls -la");
        self.type_text(cx, "gi");
        self.settle(cx);
    }
}

// ---- 位置：锚定光标，底部放不下向上翻转 ----

#[test]
fn the_overlay_sits_below_the_cursor_row_and_flips_up_when_the_bottom_has_no_room() {
    let (cell_h, pane_w, pane_h) = (18.0, 800.0, 400.0);
    let rows = 4;
    let height = overlay_height(rows);
    assert_eq!(height, HEADER_H + 4.0 * ROW_H + 8.0);

    // 光标靠上：画在光标那一行的下面。
    let near_top = place(80.0, 40.0, cell_h, rows, 400.0, pane_w, pane_h);
    assert!(!near_top.flipped);
    assert_eq!(near_top.top, 40.0 + cell_h + 2.0);
    assert_eq!(near_top.left, 80.0);
    assert_eq!(near_top.height, height);

    // 光标靠下：底部放不下，翻到光标那一行的上面，并且不盖住光标行。
    let near_bottom = place(80.0, pane_h - 40.0, cell_h, rows, 400.0, pane_w, pane_h);
    assert!(near_bottom.flipped);
    assert_eq!(near_bottom.top, pane_h - 40.0 - 2.0 - height);
    assert!(
        near_bottom.top + near_bottom.height <= pane_h - 40.0,
        "不压住光标行"
    );

    // 恰好放得下和差一像素放不下的边界。
    let below_edge = pane_h - height - cell_h - 2.0;
    assert!(!place(10.0, below_edge, cell_h, rows, 400.0, pane_w, pane_h).flipped);
    assert!(place(10.0, below_edge + 1.0, cell_h, rows, 400.0, pane_w, pane_h).flipped);
}

#[test]
fn the_overlay_stays_inside_the_pane_horizontally_and_in_tiny_panes() {
    // 光标在最右边：左移，右边缘留一点缝。
    let right = place(780.0, 40.0, 18.0, 3, 400.0, 800.0, 400.0);
    assert_eq!(right.width, 400.0);
    assert_eq!(right.left, 800.0 - 400.0 - 4.0);
    // 窗格比最小宽度还窄：宽度夹到窗格内。
    let narrow = place(20.0, 40.0, 18.0, 3, 900.0, 250.0, 400.0);
    assert_eq!(narrow.width, 250.0 - 8.0);
    assert!(narrow.left >= 4.0 && narrow.left + narrow.width <= 250.0);
    // 很长的命令：宽度有上限。
    let wide = place(0.0, 40.0, 18.0, 3, 5000.0, 2000.0, 400.0);
    assert_eq!(wide.width, 640.0);
    // 上下都放不下全部（窗格很矮）：取空间大的一侧，只画放得下的几行；整个浮层在窗格里，
    // 也不盖住光标那一行（复查 R3：原来高度不缩，120 高的窗格里画出 244 高的浮层）。
    let (cell_h, pane_h) = (18.0, 120.0);
    for cursor_y in [0.0, 20.0, 40.0, 60.0, 80.0, 100.0] {
        let at = place(10.0, cursor_y, cell_h, 8, 400.0, 800.0, pane_h);
        assert!(at.visible_rows <= 8);
        if at.visible_rows == 0 {
            continue;
        }
        assert!(at.top >= 0.0, "y={cursor_y}: {at:?}");
        assert!(
            at.top + at.height <= pane_h,
            "y={cursor_y}: 底边在窗格内 {at:?}"
        );
        assert_eq!(at.height, overlay_height(at.visible_rows));
        let covers_cursor_row = at.top < cursor_y + cell_h && at.top + at.height > cursor_y;
        assert!(!covers_cursor_row, "y={cursor_y}: 不盖住光标行 {at:?}");
    }
    let squat = place(10.0, 0.0, cell_h, 8, 400.0, 800.0, pane_h);
    assert!(
        squat.visible_rows >= 1 && squat.visible_rows < 8,
        "放得下几行就画几行：{squat:?}"
    );
    // 一行都放不下：不画。
    assert_eq!(
        place(10.0, 30.0, cell_h, 8, 400.0, 800.0, 70.0).visible_rows,
        0
    );
    // 高 DPI：单元格高度按设备像素取整后可以是小数，边界照样成立。
    let (cell_h, scale) = (19.5, 1.5);
    let height = overlay_height(5);
    let pane_h = 300.0 * scale;
    let cursor_y = pane_h - height - cell_h - 2.0;
    assert!(!place(0.0, cursor_y, cell_h, 5, 400.0, 900.0, pane_h).flipped);
    assert!(place(0.0, cursor_y + 0.5, cell_h, 5, 400.0, 900.0, pane_h).flipped);
}

/// 只画得下几行时，键盘选中的那一行始终在可见范围里。
#[test]
fn the_visible_window_keeps_the_selected_row_in_view() {
    use super::suggest_ui::visible_window_start as start;
    assert_eq!(start(None, 3, 8), 0, "没选中从头画");
    assert_eq!(start(Some(1), 3, 8), 0);
    assert_eq!(start(Some(2), 3, 8), 0);
    assert_eq!(start(Some(3), 3, 8), 1);
    assert_eq!(start(Some(7), 3, 8), 5, "最后一行");
    assert_eq!(start(Some(7), 8, 8), 0, "全放得下不滚");
    assert_eq!(start(Some(2), 0, 8), 0);
    for selected in 0..8 {
        let first = start(Some(selected), 3, 8);
        assert!((first..first + 3).contains(&selected));
    }
}

#[test]
fn the_width_follows_the_longest_candidate_in_cell_units() {
    let model = OverlayModel {
        rows: vec![
            super::suggest_ui::OverlayRow {
                text: "ls".into(),
                source: CandidateSource::History,
                selected: false,
            },
            super::suggest_ui::OverlayRow {
                text: "rsync -avz ./dist build:/srv".into(),
                source: CandidateSource::QuickCommand,
                selected: true,
            },
        ],
        select_mode: false,
    };
    let wide = content_width(&model, 9.0);
    assert_eq!(
        wide,
        "rsync -avz ./dist build:/srv".chars().count() as f32 * 9.0 + 150.0
    );
    assert!(
        content_width(
            &OverlayModel {
                rows: vec![],
                select_mode: false
            },
            9.0
        ) > 0.0
    );
}

// ---- 浮层模型：画什么、悬浮提示是什么、不含敏感条目 ----

#[gpui::test]
fn the_overlay_model_has_the_command_the_source_and_nothing_sensitive(cx: &mut TestAppContext) {
    let term = Harness::open(cx);
    term.window
        .update(cx, |workspace, _, _| {
            workspace.sync_quick_sources(&[
                ("git push --token=abc123", false),
                ("git remote -v", false),
                ("git log --oneline", true),
            ]);
        })
        .expect("window");
    term.history_and_typed(cx);
    let model = term.with_pane(cx, |pane| {
        overlay_model(&pane.assist.suggest_view().expect("view"))
    });
    let rows: Vec<(&str, CandidateSource, bool)> = model
        .rows
        .iter()
        .map(|row| (row.text.as_str(), row.source, row.selected))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("git status", CandidateSource::History, false),
            ("git stash", CandidateSource::History, false),
            ("git remote -v", CandidateSource::QuickCommand, false),
        ],
        "每行有命令和来源，不预选"
    );
    assert!(!model.select_mode);
    // 悬浮提示就是这一行自己的完整文字：敏感的、被排除的整条都不在模型里。
    let everything = format!("{model:?}");
    for secret in ["token", "abc123", "oneline"] {
        assert!(!everything.contains(secret), "{secret}");
    }
}

// ---- 按键：不显式进入选择模式时原样穿透 ----

#[gpui::test]
fn tab_enter_and_arrows_reach_the_shell_unchanged_and_never_accept_a_candidate(
    cx: &mut TestAppContext,
) {
    let keys: [(&str, &[u8]); 8] = [
        ("tab", b"\t"),
        ("enter", b"\r"),
        ("up", b"\x1b[A"),
        ("down", b"\x1b[B"),
        ("alt-up", b"\x1b[1;3A"),
        ("ctrl-alt-up", b"\x1b[1;7A"),
        ("alt-down", b"\x1b[1;3B"),
        ("ctrl-alt-down", b"\x1b[1;7B"),
    ];
    for (key, bytes) in keys {
        let term = Harness::open(cx);
        term.history_and_typed(cx);
        assert_eq!(term.shown(cx).len(), 2, "{key}：前提：浮层在显示");
        let before = term.sent().len();
        term.named(cx, key);
        assert_eq!(
            term.sent()[before..].to_vec(),
            vec![bytes.to_vec()],
            "{key}：恰好原样发出一份，没有接受第一条"
        );
        assert!(term.shown(cx).is_empty(), "{key}：交给 shell 之后浮层关闭");
        assert!(!term.select_mode(cx));
        let sent = String::from_utf8_lossy(&term.sent()[before..].concat()).into_owned();
        assert!(!sent.contains("t status"), "{key}");
    }
}

#[gpui::test]
fn escape_closes_a_shown_list_and_still_reaches_the_program(cx: &mut TestAppContext) {
    let term = Harness::open(cx);
    term.named(cx, "escape");
    assert_eq!(term.sent(), vec![vec![0x1b]], "没有列表：Esc 交给前台程序");
    term.history_and_typed(cx);
    let before = term.sent().len();
    term.named(cx, "escape");
    let sent = term.sent();
    assert_eq!(sent.len(), before + 1, "没进选择模式：Esc 照样发给 shell");
    assert_eq!(sent.last(), Some(&vec![0x1b]));
    assert!(term.shown(cx).is_empty(), "同时收起列表");
}

#[gpui::test]
fn explicit_select_mode_picks_with_arrows_and_tab_appends_only_the_suffix(cx: &mut TestAppContext) {
    let term = Harness::open(cx);
    term.history_and_typed(cx);
    // 点"选择建议"（浮层标题条 / 状态栏的入口走的就是这个方法）。
    term.window
        .update(cx, |workspace, _, cx| {
            assert!(workspace.suggest_toggle_select(cx));
        })
        .expect("window");
    assert!(term.select_mode(cx));
    assert_eq!(
        term.with_pane(cx, |p| p.assist.selected_index()),
        None,
        "仍不选中"
    );
    let before = term.sent().len();
    term.named(cx, "down");
    term.named(cx, "down");
    assert_eq!(term.with_pane(cx, |p| p.assist.selected_index()), Some(1));
    assert_eq!(term.sent().len(), before, "上下只选候选，不发给 shell");
    term.named(cx, "up");
    assert_eq!(term.with_pane(cx, |p| p.assist.selected_index()), Some(0));

    term.named(cx, "tab");
    assert_eq!(
        term.sent()[before..].to_vec(),
        vec![b"t status".to_vec()],
        "只追加后缀；没有 HT，也没有 CR"
    );
    assert!(!term.select_mode(cx));
    assert!(term.shown(cx).is_empty());
    term.poll(cx);
    term.with_pane(cx, |pane| {
        assert_eq!(pane.assist.tracker().state().confirmed_text, "git status");
        assert_eq!(pane.assist.phase(), Phase::Input);
    });
    // 之后另一次 Enter 才是执行。
    term.named(cx, "enter");
    assert_eq!(
        term.sent().last().map(Vec::as_slice),
        Some(b"\r".as_slice())
    );
}

#[gpui::test]
fn enter_in_select_mode_fills_in_without_running_and_a_held_enter_never_becomes_a_cr(
    cx: &mut TestAppContext,
) {
    let term = Harness::open(cx);
    term.history_and_typed(cx);
    term.window
        .update(cx, |workspace, _, cx| {
            workspace.suggest_toggle_select(cx);
        })
        .expect("window");
    term.named(cx, "down");
    let before = term.sent().len();
    term.named(cx, "enter");
    assert_eq!(term.sent()[before..].to_vec(), vec![b"t status".to_vec()]);
    // 按住 Enter 的重复事件：继续被吃掉，绝不能变成 CR（那会执行刚填入的命令）。
    for _ in 0..6 {
        term.held(cx, "enter");
    }
    assert_eq!(term.sent().len(), before + 1, "重复事件一个字节也没发");
    // 松开之后，另一次独立的按下按原终端规则处理：发 CR。
    term.release(cx, "enter");
    term.named(cx, "enter");
    assert_eq!(
        term.sent().last().map(Vec::as_slice),
        Some(b"\r".as_slice())
    );
}

#[gpui::test]
fn select_mode_without_a_selection_hands_tab_and_enter_back_to_the_terminal(
    cx: &mut TestAppContext,
) {
    for (key, bytes) in [("tab", b"\t".as_slice()), ("enter", b"\r".as_slice())] {
        let term = Harness::open(cx);
        term.history_and_typed(cx);
        term.window
            .update(cx, |workspace, _, cx| {
                workspace.suggest_toggle_select(cx);
            })
            .expect("window");
        let before = term.sent().len();
        term.named(cx, key);
        assert!(!term.select_mode(cx), "{key}：模式关闭");
        assert_eq!(
            term.sent()[before..].to_vec(),
            vec![bytes.to_vec()],
            "{key}：没选择就按原终端键处理，不偷偷选第一条"
        );
    }
}

#[gpui::test]
fn ordinary_typing_or_an_ime_commit_leaves_select_mode_and_is_processed_exactly_once(
    cx: &mut TestAppContext,
) {
    let term = Harness::open(cx);
    term.history_and_typed(cx);
    term.window
        .update(cx, |workspace, _, cx| {
            workspace.suggest_toggle_select(cx);
        })
        .expect("window");
    let before = term.sent().len();
    term.type_text(cx, "t");
    assert!(!term.select_mode(cx));
    assert_eq!(
        term.sent()[before..].to_vec(),
        vec![b"t".to_vec()],
        "这个字符恰好发一次"
    );

    // 输入法最终提交同样退出，并且只发一次。
    let term = Harness::open(cx);
    term.history_and_typed(cx);
    term.window
        .update(cx, |workspace, _, cx| {
            workspace.suggest_toggle_select(cx);
        })
        .expect("window");
    let before = term.sent().len();
    term.with_pane(cx, |pane| {
        pane.write_as("你".as_bytes(), super::WriteOrigin::ImeText);
    });
    assert!(!term.select_mode(cx));
    assert_eq!(
        term.sent()[before..].to_vec(),
        vec!["你".as_bytes().to_vec()]
    );
}

#[gpui::test]
fn clicking_the_terminal_or_scrolling_closes_the_list_and_the_event_is_handled_once(
    cx: &mut TestAppContext,
) {
    // 点终端：关闭，选区照常开始（原事件处理一次）。
    let term = Harness::open(cx);
    term.history_and_typed(cx);
    assert_eq!(term.shown(cx).len(), 2);
    cx.update_window(term.window.into(), |_, window, cx| {
        window.dispatch_event(
            PlatformInput::MouseDown(MouseDownEvent {
                position: point(px(300.0), px(200.0)),
                modifiers: Modifiers::default(),
                button: MouseButton::Left,
                click_count: 1,
                first_mouse: false,
            }),
            cx,
        );
    })
    .expect("click");
    assert!(term.shown(cx).is_empty(), "点终端：浮层关闭");
    assert!(
        term.window
            .update(cx, |workspace, _, _| workspace.drag_anchor.is_some())
            .expect("window"),
        "原事件照常处理：单击记了选区锚点"
    );

    // 选择模式下点终端：模式也退出。
    let term = Harness::open(cx);
    term.history_and_typed(cx);
    term.window
        .update(cx, |workspace, _, cx| {
            workspace.suggest_toggle_select(cx);
        })
        .expect("window");
    cx.update_window(term.window.into(), |_, window, cx| {
        window.dispatch_event(
            PlatformInput::MouseDown(MouseDownEvent {
                position: point(px(300.0), px(200.0)),
                modifiers: Modifiers::default(),
                button: MouseButton::Left,
                click_count: 1,
                first_mouse: false,
            }),
            cx,
        );
    })
    .expect("click");
    assert!(!term.select_mode(cx));

    // 滚动：关闭，不占滚轮。
    let term = Harness::open(cx);
    term.history_and_typed(cx);
    term.window
        .update(cx, |workspace, window, cx| {
            let event = ::gpui::ScrollEvent {
                position: point(px(300.0), px(200.0)),
                delta: ::gpui::ScrollDelta::Lines(point(0.0, 3.0)),
                ..Default::default()
            };
            workspace.on_wheel(&event, window, cx);
        })
        .expect("wheel");
    assert!(term.shown(cx).is_empty(), "滚轮：浮层关闭");
}

#[gpui::test]
fn clicking_fill_in_appends_only_the_suffix_and_returns_focus_to_the_terminal(
    cx: &mut TestAppContext,
) {
    let term = Harness::open(cx);
    term.history_and_typed(cx);
    let before = term.sent().len();
    let pane_id = term.with_pane(cx, |pane| pane.id);
    term.window
        .update(cx, |workspace, window, cx| {
            workspace.suggest_click_accept(pane_id, 1, window, cx);
        })
        .expect("window");
    assert_eq!(
        term.sent()[before..].to_vec(),
        vec![b"t stash".to_vec()],
        "只填入后缀，不发回车"
    );
    assert!(term.shown(cx).is_empty());
    let focused = term
        .window
        .update(cx, |workspace, window, _| {
            workspace.focus.is_focused(window)
        })
        .expect("window");
    assert!(focused, "焦点回到终端");
    // 点之前输入已经变了的旧浮层：什么也不发。
    let term = Harness::open(cx);
    term.history_and_typed(cx);
    term.type_text(cx, "t");
    let before = term.sent().len();
    let pane_id = term.with_pane(cx, |pane| pane.id);
    term.window
        .update(cx, |workspace, window, cx| {
            workspace.suggest_click_accept(pane_id, 0, window, cx);
        })
        .expect("window");
    assert_eq!(term.sent().len(), before);
}

// ---- 广播零生成、开关立即收起 ----

#[gpui::test]
fn a_broadcasting_tab_generates_nothing_even_after_a_long_wait(cx: &mut TestAppContext) {
    let term = Harness::open(cx);
    term.run(cx, "git status");
    term.window
        .update(cx, |workspace, _, cx| {
            workspace.tabs.active_mut().expect("tab").broadcast = true;
            cx.notify();
        })
        .expect("window");
    term.poll(cx);
    term.type_text(cx, "gi");
    term.settle(cx);
    assert!(term.shown(cx).is_empty());
    assert_eq!(
        term.with_pane(cx, |pane| pane.assist.searches_started()),
        0,
        "广播：一次检索都没发起"
    );
}

#[gpui::test]
fn switching_auto_suggestions_off_closes_the_overlay_and_select_mode_at_once(
    cx: &mut TestAppContext,
) {
    let term = Harness::open(cx);
    term.history_and_typed(cx);
    term.window
        .update(cx, |workspace, _, cx| {
            workspace.suggest_toggle_select(cx);
        })
        .expect("window");
    assert!(term.select_mode(cx));
    let off = AppPrefs {
        terminal_assist_enabled: false,
        ..AppPrefs::default()
    };
    crate::app_prefs::refresh_cached(off);
    term.poll(cx);
    assert!(term.shown(cx).is_empty());
    assert!(!term.select_mode(cx));
    // 关着时再打字也没有候选；Tab 照常是 Tab。
    term.type_text(cx, "t");
    term.settle(cx);
    assert!(term.shown(cx).is_empty());
    let before = term.sent().len();
    term.named(cx, "tab");
    assert_eq!(term.sent()[before..].to_vec(), vec![b"\t".to_vec()]);
}

// ---- 用户自己绑定的"选择建议"键 ----

#[test]
fn a_binding_needs_a_real_modifier_and_round_trips_through_its_text() {
    let ks = |modifiers: Modifiers, key: &str| Keystroke {
        modifiers,
        key: key.to_owned(),
        key_char: None,
    };
    let ctrl_alt = Modifiers {
        control: true,
        alt: true,
        ..Modifiers::default()
    };
    let text = binding_text(&ks(ctrl_alt, "s")).expect("可绑");
    assert_eq!(text, "ctrl-alt-s");
    assert!(bound_key_matches(&text, &ks(ctrl_alt, "s")));
    assert!(!bound_key_matches(&text, &ks(ctrl_alt, "d")));
    assert!(!bound_key_matches(&text, &ks(Modifiers::alt(), "s")));
    assert!(
        !bound_key_matches("", &ks(ctrl_alt, "s")),
        "没绑 = 不匹配任何键"
    );
    assert!(!bound_key_matches("garbage--", &ks(ctrl_alt, "s")));
    // 普通字母、Shift+字母、纯修饰键、Esc / Enter / Tab 不能绑。
    assert_eq!(binding_text(&ks(Modifiers::default(), "s")), None);
    assert_eq!(binding_text(&ks(Modifiers::shift(), "s")), None);
    assert_eq!(binding_text(&ks(Modifiers::control(), "control")), None);
    assert_eq!(binding_text(&ks(Modifiers::control(), "enter")), None);
    assert_eq!(binding_text(&ks(Modifiers::alt(), "tab")), None);
    assert_eq!(binding_text(&ks(Modifiers::alt(), "escape")), None);
}

#[gpui::test]
fn the_bound_key_enters_select_mode_when_candidates_exist_and_is_the_terminals_otherwise(
    cx: &mut TestAppContext,
) {
    let prefs = AppPrefs {
        terminal_assist_select_key: "ctrl-alt-s".to_owned(),
        ..AppPrefs::default()
    };
    let term = Harness::open_with(cx, prefs.clone());
    // 没有候选：绑定键不拦，按终端自己的规则发出去（Ctrl+Alt+S 编成 ^S）。
    term.named(cx, "ctrl-alt-s");
    assert_eq!(term.sent(), vec![vec![0x13]]);

    let term = Harness::open_with(cx, prefs);
    term.history_and_typed(cx);
    let before = term.sent().len();
    term.named(cx, "ctrl-alt-s");
    assert!(term.select_mode(cx), "有候选：拦下并进入选择模式");
    assert_eq!(
        term.sent().len(),
        before,
        "此上下文拦截该组合，不发给 shell"
    );
    // 按住的重复事件不会把模式退掉。
    term.held(cx, "ctrl-alt-s");
    assert!(term.select_mode(cx));
    // 默认（没绑）：同样的组合照常发给 shell。
    let term = Harness::open(cx);
    term.history_and_typed(cx);
    let before = term.sent().len();
    term.named(cx, "ctrl-alt-s");
    assert!(!term.select_mode(cx));
    assert_eq!(term.sent().len(), before + 1);
}

#[gpui::test]
fn a_pane_too_short_for_one_row_cannot_select_or_fill_in_hidden_candidates(
    cx: &mut TestAppContext,
) {
    let prefs = AppPrefs {
        terminal_assist_select_key: "ctrl-alt-s".to_owned(),
        ..AppPrefs::default()
    };
    let term = Harness::open_with(cx, prefs);
    // 窗格是分屏允许的最小高度。改尺寸之后要等新的提示符标记，输入跟踪才重新可靠。
    term.resize(cx, 800.0, split_tree_min_height());
    term.output(
        cx,
        b"
]133;A$ ]133;B",
    );
    term.history_and_typed(cx);
    assert!(!term.overlay_has_room(cx), "前提：这个高度一行候选都画不下");
    assert!(
        !term.shown(cx).is_empty(),
        "前提：候选本身是有的，只是画不下"
    );

    term.window
        .update(cx, |workspace, _, cx| {
            assert!(
                !workspace.suggest_toggle_select(cx),
                "状态栏入口：看不见列表时不能进入选择模式"
            );
        })
        .expect("window");
    assert!(!term.select_mode(cx));
    assert_eq!(
        term.with_pane(cx, |pane| pane.assist.plan_accept(0).err()),
        Some(SuggestRefusal::Hidden),
        "看不见的候选不能被填入"
    );
    let before = term.sent().len();
    term.named(cx, "ctrl-alt-s");
    assert!(!term.select_mode(cx), "绑定键：看不见列表时不进入选择模式");
    assert_eq!(
        term.sent()[before..].to_vec(),
        vec![vec![0x13]],
        "绑定键照常发给终端"
    );
    term.named(cx, "down");
    assert_eq!(term.sent().len(), before + 2, "方向键照常发给终端");
    assert_eq!(term.with_pane(cx, |p| p.assist.selected_index()), None);
    assert!(
        !term.sent()[before..]
            .iter()
            .any(|bytes| bytes == b"t status"),
        "没有填入看不见的候选"
    );

    // 窗格恢复：新的提示符上同样的输入，列表画得下，入口照常可用。
    term.resize(cx, 800.0, 600.0);
    term.output(
        cx,
        b"
]133;A$ ]133;B",
    );
    term.type_text(cx, "gi");
    term.settle(cx);
    assert!(term.overlay_has_room(cx));
    term.window
        .update(cx, |workspace, _, cx| {
            assert!(workspace.suggest_toggle_select(cx));
        })
        .expect("window");
    assert!(term.select_mode(cx));
}

fn split_tree_min_height() -> f32 {
    crate::split_tree::MIN_PANE_HEIGHT
}

#[gpui::test]
fn clearing_history_drops_the_current_target_or_everything_and_closes_the_overlay(
    cx: &mut TestAppContext,
) {
    let term = Harness::open(cx);
    term.history_and_typed(cx);
    assert_eq!(term.shown(cx).len(), 2);
    let cleared = term
        .window
        .update(cx, |workspace, _, _| workspace.clear_history_current())
        .expect("window");
    assert_eq!(cleared, 3, "三条不同的命令");
    assert!(term.shown(cx).is_empty(), "候选来自历史，一起收起");
    // 再打一个字，新的检索里不会再有被清掉的历史。
    term.type_text(cx, "t");
    term.settle(cx);
    assert!(term.shown(cx).is_empty());

    // 全部清除：包括别的目标。
    let term = Harness::open(cx);
    term.run(cx, "uptime");
    let other = term.with_pane(cx, |pane| {
        zssh_core::terminal_assist::history::lock(pane.assist.history()).record(
            "host:99",
            "df -h",
            zssh_core::terminal_assist::Provenance::Typed,
        );
        pane.assist.history().clone()
    });
    let cleared = term
        .window
        .update(cx, |workspace, _, _| workspace.clear_history_all())
        .expect("window");
    assert_eq!(cleared, 2);
    assert_eq!(
        zssh_core::terminal_assist::history::lock(&other)
            .stats()
            .entries,
        0
    );
}

#[gpui::test]
fn switching_history_off_clears_what_was_collected_and_stops_collecting(cx: &mut TestAppContext) {
    let term = Harness::open(cx);
    term.run(cx, "uptime");
    let entries = |term: &Harness, cx: &mut TestAppContext| {
        term.with_pane(cx, |pane| {
            zssh_core::terminal_assist::history::lock(pane.assist.history())
                .stats()
                .entries
        })
    };
    assert_eq!(entries(&term, cx), 1);
    let off = AppPrefs {
        terminal_assist_history: false,
        ..AppPrefs::default()
    };
    crate::app_prefs::refresh_cached(off);
    term.window
        .update(cx, |workspace, _, _| workspace.apply_assist_prefs())
        .expect("window");
    assert_eq!(entries(&term, cx), 0, "关掉即清空");
    term.run(cx, "df -h");
    assert_eq!(entries(&term, cx), 0, "关着时不采集");
}

#[test]
fn nothing_is_bound_by_default() {
    assert_eq!(AppPrefs::default().terminal_assist_select_key, "");
    assert_eq!(PaneState::Live, PaneState::Live);
}

#[gpui::test]
fn the_quick_command_snapshot_reaches_the_panes_and_only_changes_when_it_changed(
    cx: &mut TestAppContext,
) {
    let term = Harness::open(cx);
    term.window
        .update(cx, |workspace, _, _| {
            workspace.sync_quick_sources(&[("git remote -v", false), ("git log", true)]);
            let first = Arc::clone(&workspace.assist_quick);
            workspace.sync_quick_sources(&[("git remote -v", false), ("git log", true)]);
            assert!(Arc::ptr_eq(&first, &workspace.assist_quick), "没变就不换");
            workspace.sync_quick_sources(&[("git remote -v", false), ("git log", false)]);
            assert!(!Arc::ptr_eq(&first, &workspace.assist_quick));
        })
        .expect("window");
    term.run(cx, "ls -la");
    term.poll(cx);
    term.type_text(cx, "gi");
    term.settle(cx);
    assert_eq!(
        term.shown(cx),
        vec!["git remote -v".to_owned(), "git log".to_owned()]
    );
}
