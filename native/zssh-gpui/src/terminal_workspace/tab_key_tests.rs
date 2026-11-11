//! 带实际 Root 的按键路由测试；只把 TerminalWorkspace 当根会漏掉 Root 的 Tab 绑定。

use super::{Pane, Tab, TerminalWorkspace};
use crate::settings_store::AppSettings;
use gpui::{
    AppContext as _, Entity, FocusHandle, InteractiveElement as _, IntoElement, Keystroke,
    ParentElement as _, Render, Styled as _, TestAppContext, WindowHandle, div, px,
};
use gpui_kit::base::Root;
use std::sync::{Arc, Mutex, mpsc};
use zssh_core::ssh::flow::FlowGate;
use zssh_session::{CoreFilter, SessionBackend, SessionCommand, SessionEvent, SessionPump};
use zssh_terminal::Size;

type Writes = Arc<Mutex<Vec<Vec<u8>>>>;
struct Recorder(Writes);
impl SessionBackend for Recorder {
    fn send(&mut self, command: SessionCommand) {
        if let SessionCommand::Write(bytes) = command {
            self.0.lock().expect("writes").push(bytes);
        }
    }
}

struct Host {
    terminal: Entity<TerminalWorkspace>,
    before: FocusHandle,
    after: FocusHandle,
    overlay: bool,
}
impl Render for Host {
    fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        use gpui::prelude::FluentBuilder as _;
        div()
            .id("tab-test")
            .tab_group()
            .flex()
            .flex_col()
            .size_full()
            .when(self.overlay, |d| {
                d.key_context(super::TERMINAL_OVERLAY_CONTEXT)
            })
            .child(
                div()
                    .id("before")
                    .track_focus(&self.before)
                    .tab_index(0)
                    .h(px(24.0))
                    .child("Before"),
            )
            .child(div().flex_1().min_h_0().child(self.terminal.clone()))
            .child(
                div()
                    .id("after")
                    .track_focus(&self.after)
                    .tab_index(0)
                    .h(px(24.0))
                    .child("After"),
            )
    }
}

struct Harness {
    window: WindowHandle<Root>,
    host: Entity<Host>,
    terminal: Entity<TerminalWorkspace>,
    writes: Writes,
    _events: mpsc::Sender<SessionEvent>,
}
impl Harness {
    fn open(cx: &mut TestAppContext, ime: bool, with_pane: bool) -> Self {
        cx.update(|cx| {
            gpui_kit::init(cx);
            AppSettings::init_for_test(cx);
        });
        let writes: Writes = Arc::default();
        let (tx, rx) = mpsc::channel();
        let window = cx.add_window(|window, cx| {
            let terminal = cx.new(|cx| {
                let mut terminal = TerminalWorkspace::new(window, cx, "tab-test".into());
                terminal.ime_input = ime;
                if with_pane {
                    let pump = SessionPump::new(
                        Size { rows: 10, cols: 60 },
                        100,
                        Arc::new(FlowGate::new()),
                        CoreFilter::new(),
                        1,
                    );
                    terminal.tabs.push(Tab::new(
                        "local".into(),
                        Pane::with_parts(
                            pump,
                            Box::new(Recorder(Arc::clone(&writes))),
                            rx,
                            None,
                            None,
                        ),
                    ));
                }
                terminal
            });
            let focus = terminal.read(cx).focus.clone();
            window.focus(&focus, cx);
            let host = cx.new(|cx| Host {
                terminal,
                before: cx.focus_handle().tab_stop(true),
                after: cx.focus_handle().tab_stop(true),
                overlay: false,
            });
            Root::new(host, window, cx)
        });
        let host = window
            .read_with(cx, |root, _| {
                root.view().clone().downcast::<Host>().expect("host")
            })
            .expect("root");
        let terminal = host.read_with(cx, |host, _| host.terminal.clone());
        let harness = Self {
            window,
            host,
            terminal,
            writes,
            _events: tx,
        };
        harness.draw(cx);
        harness
    }
    fn draw(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.draw(cx).clear(cx)
        })
        .expect("draw");
    }
    fn key(&self, cx: &mut TestAppContext, key: &'static str) {
        cx.dispatch_keystroke(self.window.into(), Keystroke::parse(key).expect("key"));
        self.draw(cx);
    }
    fn terminal_focused(&self, cx: &mut TestAppContext) -> bool {
        cx.update_window(self.window.into(), |_, window, cx| {
            self.terminal.read(cx).focus.is_focused(window)
        })
        .expect("focus")
    }
}

#[gpui::test]
fn tab_and_shift_tab_reach_the_terminal_once_without_losing_focus(cx: &mut TestAppContext) {
    for ime in [false, true] {
        let h = Harness::open(cx, ime, true);
        for (key, expected) in [
            ("tab", b"\t".as_slice()),
            ("shift-tab", b"\x1b[Z".as_slice()),
        ] {
            h.key(cx, key);
            assert!(
                h.terminal_focused(cx),
                "{key} must not focus an outside button"
            );
            assert_eq!(
                h.writes.lock().expect("writes").last().map(Vec::as_slice),
                Some(expected)
            );
        }
        assert_eq!(h.writes.lock().expect("writes").len(), 2);
    }
}

#[gpui::test]
fn ordinary_controls_keep_tab_navigation_and_can_return_focus_to_terminal(cx: &mut TestAppContext) {
    let h = Harness::open(cx, false, true);
    let before = h.host.read_with(cx, |host, _| host.before.clone());
    cx.update_window(h.window.into(), |_, window, cx| window.focus(&before, cx))
        .expect("focus control");
    h.draw(cx);
    h.key(cx, "tab");
    cx.update_window(h.window.into(), |_, window, cx| {
        assert!(h.host.read(cx).after.is_focused(window));
    })
    .expect("next control");
    h.key(cx, "shift-tab");
    cx.update_window(h.window.into(), |_, window, cx| {
        assert!(before.is_focused(window));
        let focus = h.terminal.read(cx).focus.clone();
        window.focus(&focus, cx);
    })
    .expect("return focus");
    h.draw(cx);
    h.key(cx, "tab");
    assert!(h.terminal_focused(cx));
    assert_eq!(*h.writes.lock().expect("writes"), vec![b"\t".to_vec()]);
}

#[gpui::test]
fn overlays_leave_tab_to_root_without_writing_to_the_background_terminal(cx: &mut TestAppContext) {
    let h = Harness::open(cx, false, true);
    h.host.update(cx, |host, cx| {
        host.overlay = true;
        cx.notify();
    });
    h.draw(cx);
    h.key(cx, "tab");
    assert!(!h.terminal_focused(cx));
    assert!(h.writes.lock().expect("writes").is_empty());
}

#[gpui::test]
fn an_empty_workspace_keeps_root_tab_navigation(cx: &mut TestAppContext) {
    let h = Harness::open(cx, false, false);
    h.key(cx, "tab");
    assert!(!h.terminal_focused(cx));
    assert!(h.writes.lock().expect("writes").is_empty());
}

#[gpui::test]
fn paste_confirmation_keeps_tab_out_of_the_terminal(cx: &mut TestAppContext) {
    let h = Harness::open(cx, false, true);
    cx.update_window(h.window.into(), |_, window, cx| {
        h.terminal.update(cx, |terminal, cx| {
            terminal.settings.confirm_multiline = true;
            terminal.begin_paste("first\nsecond".into(), window, cx);
            assert!(terminal.pending_paste.is_some());
        });
    })
    .expect("paste confirmation");
    h.draw(cx);
    h.key(cx, "tab");
    assert!(!h.terminal_focused(cx));
    assert!(h.writes.lock().expect("writes").is_empty());
}
