//! 远端程序写入剪贴板（V2-U51，OSC 52，只写不读）：开关、锁屏、窗格状态 / 代次、后台标签和非聚焦窗格、
//! 取走即丢（开关关着时丢掉的请求不会在重新打开后补写）、读请求永远没有回复。
//!
//! 剪贴板写入走 `write_remote_clipboard` 的测试替身（本线程内存缓冲），不碰系统剪贴板。

use super::{
    Pane, PaneState, Tab, TerminalWorkspace, remote_clipboard_capture_start,
    remote_clipboard_capture_take,
};
use crate::settings_store::AppSettings;
use gpui::{
    AppContext as _, Entity, IntoElement, ParentElement, Render, Styled, TestAppContext,
    WindowHandle, div, px, size,
};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use zeeterm_core::ssh::flow::FlowGate;
use zeeterm_session::{CoreFilter, SessionBackend, SessionCommand, SessionEvent, SessionPump};
use zeeterm_terminal::Size;

/// 一条 OSC 52 写请求（选择符 `c`，BEL 结尾）。`text` 只用 ASCII 的 base64 字母表手编，免得为测试引依赖。
fn osc52(text: &str) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in text.as_bytes().chunks(3) {
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
    format!("\x1b]52;c;{out}\x07")
}

struct Recorder {
    writes: WriteLog,
}

impl SessionBackend for Recorder {
    fn send(&mut self, command: SessionCommand) {
        if let SessionCommand::Write(bytes) = command {
            self.writes.lock().expect("writes").push(bytes);
        }
    }

    fn send_reply(&mut self, _generation: u64, bytes: Vec<u8>) {
        self.writes.lock().expect("writes").push(bytes);
    }
}

struct Host {
    workspace: Entity<TerminalWorkspace>,
}

impl Render for Host {
    fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div().size_full().child(self.workspace.clone())
    }
}

struct Harness {
    window: WindowHandle<Host>,
    workspace: Entity<TerminalWorkspace>,
    /// 每个窗格往远端写的字节（含协议回复）。
    writes: Vec<WriteLog>,
    /// 每个窗格的事件入口（和 `workspace` 里窗格的顺序一致：先各标签，再标签里的分屏）。
    events: Vec<mpsc::Sender<SessionEvent>>,
}

/// 往远端写过的字节（含协议回复），所有窗格共用这种记录。
type WriteLog = Arc<Mutex<Vec<Vec<u8>>>>;

fn new_pane() -> (Pane, WriteLog, mpsc::Sender<SessionEvent>) {
    let writes = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = mpsc::channel();
    let pump = SessionPump::new(
        Size { rows: 10, cols: 40 },
        1_000,
        Arc::new(FlowGate::new()),
        CoreFilter::new(),
        1,
    );
    let pane = Pane::with_parts(
        pump,
        Box::new(Recorder {
            writes: Arc::clone(&writes),
        }),
        rx,
        None,
        None,
    );
    (pane, writes, tx)
}

impl Harness {
    /// `tabs` 个标签，第 0 个在前台；`extra_split` 在第 0 个标签里再放一个不聚焦的分屏窗格。
    fn open(cx: &mut TestAppContext, tabs: usize, extra_split: bool) -> Self {
        cx.update(|cx| {
            gpui_kit::init(cx);
            AppSettings::init_for_test(cx);
        });
        remote_clipboard_capture_start();
        crate::app_lock::set_locked(false);
        let mut writes = Vec::new();
        let mut events = Vec::new();
        let mut panes = Vec::new();
        for _ in 0..tabs {
            let (pane, w, e) = new_pane();
            panes.push(pane);
            writes.push(w);
            events.push(e);
        }
        let mut split = None;
        if extra_split {
            let (pane, w, e) = new_pane();
            split = Some(pane);
            writes.push(w);
            events.push(e);
        }
        let window = cx.add_window(|window, cx| {
            let workspace = cx.new(|cx| {
                let mut workspace = TerminalWorkspace::new(window, cx, "osc52-test".into());
                for (index, pane) in panes.into_iter().enumerate() {
                    workspace.tabs.push(Tab::new(format!("tab-{index}"), pane));
                }
                if let Some(pane) = split {
                    workspace.tabs.items_mut()[0].panes.push(pane);
                }
                workspace
            });
            Host { workspace }
        });
        let workspace = window
            .read_with(cx, |host, _| host.workspace.clone())
            .expect("host");
        cx.simulate_window_resize(window.into(), size(px(800.0), px(500.0)));
        let harness = Self {
            window,
            workspace,
            writes,
            events,
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

    fn set_osc52(&self, cx: &mut TestAppContext, on: bool) {
        self.workspace.update(cx, |ws, cx| {
            ws.settings.osc52_write = on;
            cx.notify();
        });
    }

    /// 远端（第 `pane` 个窗格）发来一段输出。
    fn remote_says(&self, pane: usize, text: &str) {
        self.events[pane]
            .send(SessionEvent::Data {
                generation: 1,
                bytes: text.as_bytes().to_vec(),
            })
            .expect("output");
    }

    /// 和每一拍节拍做的事一样：先收各窗格的会话事件，再处理剪贴板请求。
    fn tick(&self, cx: &mut TestAppContext) {
        self.workspace.update(cx, |ws, cx| {
            ws.poll_sessions();
            ws.flush_remote_clipboard(cx);
        });
    }

    fn written(&self) -> Vec<String> {
        remote_clipboard_capture_take()
    }

    fn sent_to_remote(&self, pane: usize) -> Vec<Vec<u8>> {
        self.writes[pane].lock().expect("writes").clone()
    }
}

#[gpui::test]
fn a_remote_write_reaches_the_clipboard_and_nothing_goes_back_to_the_remote(
    cx: &mut TestAppContext,
) {
    let h = Harness::open(cx, 1, false);
    assert!(
        h.workspace.read_with(cx, |ws, _| ws.settings.osc52_write),
        "默认开"
    );
    h.remote_says(0, &osc52("hello from remote"));
    h.tick(cx);
    assert_eq!(h.written(), ["hello from remote"]);
    assert!(h.sent_to_remote(0).is_empty(), "写请求没有回复");
    // 取走即丢：下一拍不会重复写。
    h.tick(cx);
    assert!(h.written().is_empty());
}

#[gpui::test]
fn reading_the_clipboard_is_never_answered_and_writes_nothing(cx: &mut TestAppContext) {
    let h = Harness::open(cx, 1, false);
    h.remote_says(
        0,
        "\x1b]52;c;?\x07\x1b]52;c;?\x1b\\\x1b]52;;?\x07\x1b]52;p;?\x07",
    );
    h.tick(cx);
    assert!(h.written().is_empty());
    assert!(
        h.sent_to_remote(0).is_empty(),
        "读请求没有任何回复，本机剪贴板的内容出不去"
    );
}

#[gpui::test]
fn with_the_switch_off_nothing_is_written_and_the_request_is_not_kept(cx: &mut TestAppContext) {
    let h = Harness::open(cx, 1, false);
    h.set_osc52(cx, false);
    h.remote_says(0, &osc52("secret token"));
    h.tick(cx);
    assert!(h.written().is_empty(), "开关关着不写");
    // 之后再打开也不会补写那条旧的。
    h.set_osc52(cx, true);
    h.tick(cx);
    h.tick(cx);
    assert!(h.written().is_empty(), "请求已经被丢掉，重新打开不补写");
    // 打开之后的新请求照常写。
    h.remote_says(0, &osc52("after switching on"));
    h.tick(cx);
    assert_eq!(h.written(), ["after switching on"]);
}

#[gpui::test]
fn the_real_toggle_flips_the_switch(cx: &mut TestAppContext) {
    let h = Harness::open(cx, 1, false);
    h.workspace.update(cx, |ws, cx| ws.toggle_osc52_write(cx));
    assert!(!h.workspace.read_with(cx, |ws, _| ws.settings.osc52_write));
    h.remote_says(0, &osc52("x"));
    h.tick(cx);
    assert!(h.written().is_empty());
    h.workspace.update(cx, |ws, cx| ws.toggle_osc52_write(cx));
    h.remote_says(0, &osc52("y"));
    h.tick(cx);
    assert_eq!(h.written(), ["y"]);
}

#[gpui::test]
fn a_locked_screen_writes_nothing_and_unlocking_does_not_replay(cx: &mut TestAppContext) {
    let h = Harness::open(cx, 1, false);
    crate::app_lock::set_locked(true);
    h.remote_says(0, &osc52("while locked"));
    h.tick(cx);
    assert!(h.written().is_empty(), "锁屏时不写");
    crate::app_lock::set_locked(false);
    h.tick(cx);
    assert!(h.written().is_empty(), "解锁后也不补写锁屏时的请求");
    h.remote_says(0, &osc52("after unlock"));
    h.tick(cx);
    assert_eq!(h.written(), ["after unlock"]);
}

#[gpui::test]
fn a_pane_that_is_not_live_or_has_a_stale_generation_writes_nothing(cx: &mut TestAppContext) {
    let h = Harness::open(cx, 2, false);
    // 第 0 个窗格断开了；第 1 个窗格的连接代次和泵里的对不上（旧连接的请求）。
    h.workspace.update(cx, |ws, _| {
        let tabs = ws.tabs.items_mut();
        tabs[0].panes[0].state = PaneState::Disconnected;
        tabs[1].panes[0].generation = 99;
    });
    h.remote_says(0, &osc52("from a dead pane"));
    h.remote_says(1, &osc52("from an old connection"));
    h.tick(cx);
    assert!(h.written().is_empty());
    // 窗格恢复正常之后，之前那两条也不会补写；新的请求才写。
    h.workspace.update(cx, |ws, _| {
        let tabs = ws.tabs.items_mut();
        tabs[0].panes[0].state = PaneState::Live;
        tabs[1].panes[0].generation = 1;
    });
    h.tick(cx);
    assert!(h.written().is_empty(), "不补写");
    h.remote_says(0, &osc52("alive again"));
    h.tick(cx);
    assert_eq!(h.written(), ["alive again"]);
}

#[gpui::test]
fn background_tabs_and_unfocused_splits_write_too(cx: &mut TestAppContext) {
    // 窗格 0 = 前台标签的聚焦窗格；1 = 后台标签；2 = 前台标签里不聚焦的分屏。
    let h = Harness::open(cx, 2, true);
    h.remote_says(1, &osc52("copied in a background tab"));
    h.tick(cx);
    assert_eq!(h.written(), ["copied in a background tab"]);
    h.remote_says(2, &osc52("copied in a split"));
    h.tick(cx);
    assert_eq!(h.written(), ["copied in a split"]);
    // 同一拍里几个窗格都有请求：都写，窗格内部只留最新一条。
    h.remote_says(0, &osc52("old one"));
    h.remote_says(0, &osc52("latest one"));
    h.remote_says(1, &osc52("other tab"));
    h.tick(cx);
    let mut written = h.written();
    written.sort();
    assert_eq!(written, ["latest one", "other tab"]);
}

#[gpui::test]
fn two_requests_in_a_row_leave_only_the_last_in_the_clipboard(cx: &mut TestAppContext) {
    let h = Harness::open(cx, 1, false);
    h.remote_says(0, &osc52("first"));
    h.remote_says(0, &osc52("second"));
    h.tick(cx);
    assert_eq!(h.written(), ["second"]);
}

/// 真正的节拍（60 ms 一拍的后台任务）也会处理剪贴板请求，不依赖界面重绘或手动调用。
#[gpui::test]
fn the_background_tick_flushes_the_request_by_itself(cx: &mut TestAppContext) {
    let h = Harness::open(cx, 1, false);
    h.remote_says(0, &osc52("via the tick"));
    for _ in 0..3 {
        cx.executor().advance_clock(Duration::from_millis(70));
        cx.run_until_parked();
    }
    assert_eq!(h.written(), ["via the tick"]);
}

/// 旧的复制路径没有被改：选区复制仍然直接写 GPUI 剪贴板（不经远端请求这条出口），空选区什么也不写。
#[gpui::test]
fn selection_copy_does_not_go_through_the_remote_request_path(cx: &mut TestAppContext) {
    let h = Harness::open(cx, 1, false);
    h.remote_says(0, "plain output\r\n");
    h.tick(cx);
    h.workspace.update(cx, |ws, cx| ws.copy_selection(cx));
    assert!(h.written().is_empty(), "没有选区：什么也不写");
    assert!(cx.read_from_clipboard().is_none(), "也没动 GPUI 剪贴板");
    // 选中第一行再复制：落在 GPUI 剪贴板里，不在远端请求的出口里。
    h.workspace.update(cx, |ws, cx| {
        ws.selection = ws
            .pane()
            .map(|pane| super::Selection::from_viewport(&pane.snapshot, (0, 0), (0, 4)));
        ws.copy_selection(cx);
    });
    assert!(h.written().is_empty());
    let copied = cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .unwrap_or_default();
    assert_eq!(copied, "plain");
}
