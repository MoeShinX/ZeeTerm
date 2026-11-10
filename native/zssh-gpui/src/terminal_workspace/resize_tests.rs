//! 首帧就按实际终端区域更新模型与 PTY：不能靠下一次输出或定时器补排版。

use super::{PANE_PAD, Pane, Tab, TerminalWorkspace};
use crate::settings_store::AppSettings;
use gpui::{
    AppContext as _, Entity, IntoElement, ParentElement, Render, Styled, TestAppContext,
    WindowHandle, div, px, size,
};
use std::sync::{Arc, Mutex, mpsc};
use zssh_core::ssh::flow::FlowGate;
use zssh_session::{CoreFilter, SessionBackend, SessionCommand, SessionPump};
use zssh_terminal::Size;

type Writes = Arc<Mutex<Vec<Vec<u8>>>>;
struct ResizeBackend(Arc<Mutex<Vec<Size>>>, Writes);

impl SessionBackend for ResizeBackend {
    fn send(&mut self, command: SessionCommand) {
        match command {
            SessionCommand::Resize(size) => self.0.lock().expect("sizes").push(size),
            SessionCommand::Write(bytes) => self.1.lock().expect("writes").push(bytes),
            SessionCommand::Close => {}
        }
    }
}

struct ResizeHost {
    workspace: Entity<TerminalWorkspace>,
    sidebar: f32,
    frames: Arc<Mutex<Vec<(Size, Size)>>>,
}

impl Render for ResizeHost {
    fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        let workspace = self.workspace.clone();
        let frames = Arc::clone(&self.frames);
        div()
            .flex()
            .flex_row()
            .size_full()
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(self.workspace.clone())
                    .child(
                        gpui::canvas(
                            move |bounds, _, cx| {
                                let (actual, expected) = workspace.read_with(cx, |ws, _| {
                                    let expected = Size {
                                        cols: (((f32::from(bounds.size.width) - PANE_PAD * 2.0)
                                            / ws.cell_width)
                                            .floor()
                                            as usize)
                                            .max(20),
                                        rows: (((f32::from(bounds.size.height) - PANE_PAD * 2.0)
                                            / ws.cell_height)
                                            .floor()
                                            as usize)
                                            .max(2),
                                    };
                                    (ws.pane().expect("pane").snapshot.size, expected)
                                });
                                frames.lock().expect("frames").push((actual, expected));
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    ),
            )
            .child(div().w(px(self.sidebar)).h_full().flex_shrink_0())
    }
}

struct Harness {
    window: WindowHandle<ResizeHost>,
    workspace: Entity<TerminalWorkspace>,
    sizes: Arc<Mutex<Vec<Size>>>,
    writes: Writes,
    frames: Arc<Mutex<Vec<(Size, Size)>>>,
    events: mpsc::Sender<zssh_session::SessionEvent>,
}

impl Harness {
    fn open(cx: &mut TestAppContext) -> Self {
        cx.update(|cx| {
            gpui_kit::init(cx);
            AppSettings::init_for_test(cx);
        });
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let writes = Arc::new(Mutex::new(Vec::new()));
        let frames = Arc::new(Mutex::new(Vec::new()));
        let (events_tx, events) = mpsc::channel();
        let window = cx.add_window(|window, cx| {
            let workspace = cx.new(|cx| {
                let mut workspace = TerminalWorkspace::new(window, cx, "resize-test".into());
                let pump = SessionPump::new(
                    Size { rows: 10, cols: 40 },
                    100,
                    Arc::new(FlowGate::new()),
                    CoreFilter::new(),
                    1,
                );
                let pane = Pane::with_parts(
                    pump,
                    Box::new(ResizeBackend(Arc::clone(&sizes), Arc::clone(&writes))),
                    events,
                    None,
                    None,
                );
                workspace.tabs.push(Tab::new("resize".into(), pane));
                workspace
            });
            ResizeHost {
                workspace,
                sidebar: 240.0,
                frames: Arc::clone(&frames),
            }
        });
        let workspace = window
            .read_with(cx, |host, _| host.workspace.clone())
            .expect("host");
        let harness = Self {
            window,
            workspace,
            sizes,
            writes,
            frames,
            events: events_tx,
        };
        cx.simulate_window_resize(window.into(), size(px(1000.0), px(600.0)));
        harness.draw(cx);
        harness.draw(cx);
        harness.sizes.lock().expect("sizes").clear();
        harness.frames.lock().expect("frames").clear();
        harness
    }

    fn draw(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .expect("draw");
    }

    fn progress(&self, cx: &mut TestAppContext, percent: usize) {
        // 与测速进度相同的彩色中文 + CR 覆盖，不运行外部命令或联网。
        let output = format!(
            "\r\x1b[36;1m[node-1]# 正在检测国际互连：测试点之接收{} ⠋ {percent:02}% \x1b[0m",
            ".".repeat(20)
        );
        self.events
            .send(zssh_session::SessionEvent::Data {
                generation: 1,
                bytes: output.into_bytes(),
            })
            .expect("output");
        self.workspace.update(cx, |ws, cx| {
            ws.poll_sessions();
            cx.notify();
        });
        self.draw(cx);
        self.workspace.read_with(cx, |ws, _| {
            let snapshot = &ws.pane().expect("pane").snapshot;
            let text = snapshot
                .rows
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(text.matches("[node-1]").count(), 1, "进度应覆盖同一行");
            assert!(text.contains(&format!("{percent:02}%")));
            assert_eq!(snapshot.history_size, 0, "不应产生重复进度回滚");
        });
    }

    fn assert_current_size(&self, cx: &TestAppContext, width: f32, height: f32) {
        for (actual, expected) in self.frames.lock().expect("frames").drain(..) {
            assert_eq!(actual, expected, "不能绘制使用旧尺寸的中间帧");
        }
        let (actual, expected) = self.workspace.read_with(cx, |workspace, _| {
            (
                workspace.pane().expect("pane").snapshot.size,
                Size {
                    cols: (((width - PANE_PAD * 2.0) / workspace.cell_width).floor() as usize)
                        .max(20),
                    rows: (((height - PANE_PAD * 2.0) / workspace.cell_height).floor() as usize)
                        .max(2),
                },
            )
        });
        assert_eq!(actual, expected, "首帧的模型必须匹配当前可用区域");
        assert_eq!(
            self.sizes.lock().expect("sizes").last(),
            Some(&expected),
            "PTY 和本地终端必须收到同一尺寸"
        );
    }
}

#[gpui::test]
fn window_resize_updates_the_grid_and_pty_in_the_first_frame(cx: &mut TestAppContext) {
    let harness = Harness::open(cx);
    for (width, height) in [(720.0, 420.0), (1200.0, 720.0), (900.0, 550.0)] {
        cx.simulate_window_resize(harness.window.into(), size(px(width), px(height)));
        harness.draw(cx);
        harness.assert_current_size(cx, width - 240.0, height);
    }
}

#[gpui::test]
fn closing_and_reopening_a_sidebar_resizes_the_terminal_in_the_first_frame(
    cx: &mut TestAppContext,
) {
    let harness = Harness::open(cx);
    for sidebar in [0.0, 320.0, 0.0, 240.0] {
        harness
            .window
            .update(cx, |host, _, cx| {
                host.sidebar = sidebar;
                cx.notify();
            })
            .expect("sidebar");
        harness.draw(cx);
        harness.assert_current_size(cx, 1000.0 - sidebar, 600.0);
    }
}

#[gpui::test]
fn chinese_progress_keeps_overwriting_one_row_while_the_available_area_changes(
    cx: &mut TestAppContext,
) {
    let harness = Harness::open(cx);
    harness.progress(cx, 10);
    // 每个区域都足以容纳这条进度行，排除脚本自身长于终端时的正常软换行。
    for (index, (width, sidebar)) in [(900.0, 240.0), (1100.0, 0.0), (1000.0, 280.0)]
        .into_iter()
        .enumerate()
    {
        cx.simulate_window_resize(harness.window.into(), size(px(width), px(600.0)));
        harness
            .window
            .update(cx, |host, _, cx| {
                host.sidebar = sidebar;
                cx.notify();
            })
            .expect("sidebar");
        harness.draw(cx);
        harness.assert_current_size(cx, width - sidebar, 600.0);
        harness.progress(cx, 20 + index);
    }
}

#[gpui::test]
fn a_link_selected_after_chinese_is_copied_and_pasted_without_prefix_or_truncation(
    cx: &mut TestAppContext,
) {
    let harness = Harness::open(cx);
    let url = "https://example.com/r/Case09XY";
    harness
        .events
        .send(zssh_session::SessionEvent::Data {
            generation: 1,
            bytes: format!("\x1b[1m测试完成，点击查看你的结果链接：\x1b[0m{url}").into_bytes(),
        })
        .expect("output");
    harness.workspace.update(cx, |ws, cx| {
        ws.settings.copy_on_select = false;
        ws.settings.bracketed_paste = crate::settings::BracketedPaste::Off;
        ws.poll_sessions();
        cx.notify();
    });
    harness.draw(cx);
    let (start, end) = harness.workspace.read_with(cx, |ws, _| {
        let geom = &ws.pane_geom[0];
        let cell = |col: usize| {
            gpui::point(
                px(geom.left + (col as f32 + 0.5) * ws.cell_width),
                px(geom.top + ws.cell_height * 0.5),
            )
        };
        (cell(32), cell(32 + url.len() - 1))
    });
    cx.update_window(harness.window.into(), |_, window, cx| {
        window.dispatch_event(
            gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {
                position: start,
                pressed_button: None,
                modifiers: Default::default(),
            }),
            cx,
        );
        window.dispatch_event(
            gpui::PlatformInput::MouseDown(gpui::MouseDownEvent {
                button: gpui::MouseButton::Left,
                position: start,
                modifiers: Default::default(),
                click_count: 1,
                first_mouse: false,
            }),
            cx,
        );
    })
    .expect("mouse down");
    harness.draw(cx);
    cx.update_window(harness.window.into(), |_, window, cx| {
        window.dispatch_event(
            gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {
                position: end,
                pressed_button: Some(gpui::MouseButton::Left),
                modifiers: Default::default(),
            }),
            cx,
        );
        window.dispatch_event(
            gpui::PlatformInput::MouseUp(gpui::MouseUpEvent {
                button: gpui::MouseButton::Left,
                position: end,
                modifiers: Default::default(),
                click_count: 1,
            }),
            cx,
        );
    })
    .expect("mouse up");
    let copy = if cfg!(target_os = "macos") {
        "cmd-c"
    } else {
        "ctrl-shift-c"
    };
    let paste = if cfg!(target_os = "macos") {
        "cmd-v"
    } else {
        "ctrl-v"
    };
    cx.dispatch_keystroke(
        harness.window.into(),
        gpui::Keystroke::parse(copy).expect("copy"),
    );
    let copied = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
    assert_eq!(copied.as_deref(), Some(url));
    cx.dispatch_keystroke(
        harness.window.into(),
        gpui::Keystroke::parse(paste).expect("paste"),
    );
    assert_eq!(
        harness
            .writes
            .lock()
            .expect("writes")
            .last()
            .map(Vec::as_slice),
        Some(url.as_bytes())
    );
}
