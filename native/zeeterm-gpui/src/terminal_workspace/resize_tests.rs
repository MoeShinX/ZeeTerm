//! 首帧就按实际终端区域更新模型与 PTY：不能靠下一次输出或定时器补排版。

use super::{PANE_PAD, Pane, Tab, TerminalWorkspace};
use crate::settings_store::AppSettings;
use gpui::{
    AppContext as _, Entity, IntoElement, ParentElement, Render, Styled, TestAppContext,
    WindowHandle, div, px, size,
};
use std::sync::{Arc, Mutex, mpsc};
use zeeterm_core::ssh::flow::FlowGate;
use zeeterm_session::{CoreFilter, SessionBackend, SessionCommand, SessionPump};
use zeeterm_terminal::Size;

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
    events: mpsc::Sender<zeeterm_session::SessionEvent>,
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
            .send(zeeterm_session::SessionEvent::Data {
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

/// 改界面字号不能给远端发多余的窗口大小变化：终端的行列只取决于区域像素和终端自己的字号，
/// 界面字号换档（区域没变）既不换行重排，也不发 `Resize`。
#[gpui::test]
fn changing_the_interface_text_size_does_not_resend_the_window_size(cx: &mut TestAppContext) {
    let harness = Harness::open(cx);
    let settings = cx.update(AppSettings::global);
    let (cell, grid) = harness.workspace.read_with(cx, |ws, _| {
        (
            (ws.cell_width, ws.cell_height),
            ws.pane().expect("pane").snapshot.size,
        )
    });
    for level in [
        crate::typography::UiTextSize::Large,
        crate::typography::UiTextSize::Small,
        crate::typography::UiTextSize::Normal,
    ] {
        settings.update(cx, |model, mcx| {
            model.update_with(mcx, |s| s.ui_text_size = level);
        });
        harness.draw(cx);
        harness.draw(cx);
        assert!(
            harness.sizes.lock().expect("sizes").is_empty(),
            "{level:?}：界面字号换档不该触发 Resize"
        );
        harness.workspace.read_with(cx, |ws, _| {
            assert_eq!(
                (ws.cell_width, ws.cell_height),
                cell,
                "终端字符格不受界面字号影响"
            );
            assert_eq!(ws.pane().expect("pane").snapshot.size, grid, "{level:?}");
        });
    }
    crate::typography::set_ui_text_size(crate::typography::UiTextSize::Normal);
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
        .send(zeeterm_session::SessionEvent::Data {
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

// ---- V2-U45：服务器概览停靠在终端右边 ----
//
// 概览从「盖在终端上的浮层」改成 `终端 | 概览` 的左右排列。下面的测试用的是主窗口真正在画的那个
// `main_window::pages::terminal_row`，概览用一个同样尺寸规则（`flex_none` + `h_full` + 固定宽）的
// 占位块代替（真正的概览要整个 `MainWindow`）；`legacy` 把改动前的写法（相对定位的 `flex_1` 容器 +
// 绝对定位的浮层）原样留一份，开关关着时拿它做对照。

struct DockedHost {
    workspace: Entity<TerminalWorkspace>,
    overview: Option<f32>,
    legacy: bool,
}

impl Render for DockedHost {
    fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        let block = |width: f32| div().h_full().w(px(width));
        if self.legacy {
            // 改动前：概览是绝对定位的浮层，终端始终占满整个区域。
            return div()
                .flex()
                .flex_col()
                .size_full()
                .child(
                    div()
                        .relative()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .child(self.workspace.clone())
                        .children(
                            self.overview
                                .map(|width| block(width).absolute().top_0().right_0().bottom_0()),
                        ),
                )
                .into_any_element();
        }
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(crate::main_window::pages::terminal_row(
                self.workspace.clone(),
                None,
                self.overview
                    .map(|width| block(width).flex_none().into_any_element()),
            ))
            .into_any_element()
    }
}

/// 区域原点 + 每个窗格的（文字区左、上、列、行）。
type Geometry = ((f32, f32), Vec<(f32, f32, usize, usize)>);

struct DockedHarness {
    window: WindowHandle<DockedHost>,
    workspace: Entity<TerminalWorkspace>,
    sizes: Arc<Mutex<Vec<Size>>>,
}

impl DockedHarness {
    fn open(cx: &mut TestAppContext, legacy: bool, width: f32, height: f32) -> Self {
        cx.update(|cx| {
            gpui_kit::init(cx);
            AppSettings::init_for_test(cx);
        });
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let writes: Writes = Arc::new(Mutex::new(Vec::new()));
        let (_events_tx, events) = mpsc::channel();
        let window = cx.add_window(|window, cx| {
            let workspace = cx.new(|cx| {
                let mut workspace = TerminalWorkspace::new(window, cx, "docked-test".into());
                let pump = SessionPump::new(
                    Size { rows: 10, cols: 40 },
                    100,
                    Arc::new(FlowGate::new()),
                    CoreFilter::new(),
                    1,
                );
                let pane = Pane::with_parts(
                    pump,
                    Box::new(ResizeBackend(Arc::clone(&sizes), writes)),
                    events,
                    None,
                    None,
                );
                workspace.tabs.push(Tab::new("docked".into(), pane));
                workspace
            });
            DockedHost {
                workspace,
                overview: None,
                legacy,
            }
        });
        let workspace = window
            .read_with(cx, |host, _| host.workspace.clone())
            .expect("host");
        let harness = Self {
            window,
            workspace,
            sizes,
        };
        cx.simulate_window_resize(window.into(), size(px(width), px(height)));
        harness.draw(cx);
        harness.draw(cx);
        harness.sizes.lock().expect("sizes").clear();
        harness
    }

    fn draw(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .expect("draw");
    }

    fn set_overview(&self, cx: &mut TestAppContext, overview: Option<f32>) {
        self.window
            .update(cx, |host, _, cx| {
                host.overview = overview;
                cx.notify();
            })
            .expect("overview");
        self.draw(cx);
    }

    /// 终端现在的行列，以及按给定区域宽高算出来的期望值。
    fn grid(&self, cx: &TestAppContext, width: f32, height: f32) -> (Size, Size) {
        self.workspace.read_with(cx, |ws, _| {
            (
                ws.pane().expect("pane").snapshot.size,
                Size {
                    cols: (((width - PANE_PAD * 2.0) / ws.cell_width).floor() as usize).max(20),
                    rows: (((height - PANE_PAD * 2.0) / ws.cell_height).floor() as usize).max(2),
                },
            )
        })
    }

    /// 命中测试和输入法位置要用的几何：区域原点、窗格文字区左上角、行列。
    fn geometry(&self, cx: &TestAppContext) -> Geometry {
        self.workspace.read_with(cx, |ws, _| {
            (
                ws.area_origin,
                ws.pane_geom
                    .iter()
                    .map(|g| (g.left, g.top, g.cols, g.rows))
                    .collect(),
            )
        })
    }
}

/// 概览打开：终端的列数按「原宽 - 概览宽」重排并只发出一次 Resize；关掉恢复原列数（又一次 Resize）；
/// 稳定之后不再有多余的 Resize。
#[gpui::test]
fn opening_the_docked_overview_shrinks_the_terminal_and_closing_restores_it(
    cx: &mut TestAppContext,
) {
    let harness = DockedHarness::open(cx, false, 1000.0, 600.0);
    let (before, expected) = harness.grid(cx, 1000.0, 600.0);
    assert_eq!(before, expected, "概览关着：终端占满整个区域");

    harness.set_overview(cx, Some(360.0));
    let (docked, expected) = harness.grid(cx, 640.0, 600.0);
    assert_eq!(docked, expected, "终端宽度 = 1000 - 360");
    assert!(docked.cols < before.cols);
    assert_eq!(
        harness.sizes.lock().expect("sizes").as_slice(),
        &[expected],
        "远端只收到一次窗口大小变化"
    );

    harness.draw(cx);
    harness.draw(cx);
    assert_eq!(
        harness.sizes.lock().expect("sizes").len(),
        1,
        "稳定后不再重发"
    );

    harness.set_overview(cx, None);
    let (restored, expected) = harness.grid(cx, 1000.0, 600.0);
    assert_eq!(restored, before, "关掉后恢复原来的列数");
    assert_eq!(restored, expected);
    assert_eq!(
        harness.sizes.lock().expect("sizes").last(),
        Some(&before),
        "关掉时远端收到恢复后的大小"
    );
    assert_eq!(harness.sizes.lock().expect("sizes").len(), 2);
}

/// 终端区左上角不动：概览开着时选区、链接点击、鼠标上报用的窗口坐标换算（区域原点 + 窗格内边距）和
/// 关着时一致；只有列数变少。
#[gpui::test]
fn the_terminal_origin_does_not_move_when_the_overview_docks(cx: &mut TestAppContext) {
    let harness = DockedHarness::open(cx, false, 1000.0, 600.0);
    let (origin_off, geom_off) = harness.geometry(cx);
    harness.set_overview(cx, Some(420.0));
    let (origin_on, geom_on) = harness.geometry(cx);
    assert_eq!(origin_on, origin_off);
    assert_eq!(geom_on.len(), geom_off.len());
    for (on, off) in geom_on.iter().zip(&geom_off) {
        assert_eq!((on.0, on.1), (off.0, off.1), "文字区左上角不动");
        assert_eq!(on.3, off.3, "行数不变");
        assert!(on.2 < off.2, "列数变少");
    }
}

/// 开关关着时和改动前的写法（绝对定位浮层）逐项一致：几何、列数、发给远端的大小。
#[gpui::test]
fn with_the_overview_closed_the_layout_matches_the_old_floating_layout(cx: &mut TestAppContext) {
    for (width, height) in [(1000.0, 600.0), (720.0, 420.0), (1500.0, 900.0)] {
        let old = DockedHarness::open(cx, true, width, height);
        let new = DockedHarness::open(cx, false, width, height);
        assert_eq!(old.geometry(cx), new.geometry(cx), "{width}x{height}");
        assert_eq!(old.grid(cx, width, height), new.grid(cx, width, height));
        // 窗口变大变小之后也一致。
        for (w, h) in [(width + 200.0, height), (width, height - 100.0)] {
            for harness in [&old, &new] {
                cx.simulate_window_resize(harness.window.into(), size(px(w), px(h)));
                harness.draw(cx);
            }
            assert_eq!(old.geometry(cx), new.geometry(cx), "{w}x{h}");
            assert_eq!(
                *old.sizes.lock().expect("sizes"),
                *new.sizes.lock().expect("sizes"),
                "远端收到的窗口大小一致"
            );
        }
    }
}

/// 对照：改动前概览是浮层，终端列数不变（盖住了右边的内容）；改动后终端让出宽度。
#[gpui::test]
fn the_old_overlay_covered_the_terminal_and_the_docked_one_does_not(cx: &mut TestAppContext) {
    let old = DockedHarness::open(cx, true, 1000.0, 600.0);
    let before = old.grid(cx, 1000.0, 600.0).0;
    old.set_overview(cx, Some(360.0));
    assert_eq!(old.grid(cx, 1000.0, 600.0).0, before, "浮层不改变终端列数");
    let new = DockedHarness::open(cx, false, 1000.0, 600.0);
    new.set_overview(cx, Some(360.0));
    assert!(new.grid(cx, 640.0, 600.0).0.cols < before.cols);
}

/// 概览开着时在终端那一栏里拖选照常选中；在概览那一栏里按下不会开始终端选区。
#[gpui::test]
fn selecting_text_works_beside_a_docked_overview_and_the_overview_column_is_not_the_terminal(
    cx: &mut TestAppContext,
) {
    let harness = DockedHarness::open(cx, false, 1000.0, 600.0);
    harness.set_overview(cx, Some(360.0));
    let (left, top, cell_w, cell_h) = harness.workspace.read_with(cx, |ws, _| {
        let geom = &ws.pane_geom[0];
        (geom.left, geom.top, ws.cell_width, ws.cell_height)
    });
    let drag = |cx: &mut TestAppContext, from: gpui::Point<gpui::Pixels>, to: gpui::Point<_>| {
        cx.update_window(harness.window.into(), |_, window, cx| {
            window.dispatch_event(
                gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {
                    position: from,
                    pressed_button: None,
                    modifiers: Default::default(),
                }),
                cx,
            );
            window.dispatch_event(
                gpui::PlatformInput::MouseDown(gpui::MouseDownEvent {
                    button: gpui::MouseButton::Left,
                    position: from,
                    modifiers: Default::default(),
                    click_count: 1,
                    first_mouse: false,
                }),
                cx,
            );
            window.dispatch_event(
                gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {
                    position: to,
                    pressed_button: Some(gpui::MouseButton::Left),
                    modifiers: Default::default(),
                }),
                cx,
            );
            window.dispatch_event(
                gpui::PlatformInput::MouseUp(gpui::MouseUpEvent {
                    button: gpui::MouseButton::Left,
                    position: to,
                    modifiers: Default::default(),
                    click_count: 1,
                }),
                cx,
            );
        })
        .expect("drag");
        harness.draw(cx);
    };
    // 概览那一栏（x >= 640）里按下、拖到终端里：不该选中任何东西。
    drag(
        cx,
        gpui::point(px(700.0), px(top + cell_h * 0.5)),
        gpui::point(px(left + cell_w * 5.5), px(top + cell_h * 0.5)),
    );
    assert_eq!(
        harness.workspace.read_with(cx, |ws, _| ws.selection),
        None,
        "概览一栏里按下不开始终端选区"
    );
    // 终端那一栏里照常拖选。
    drag(
        cx,
        gpui::point(px(left + cell_w * 0.5), px(top + cell_h * 0.5)),
        gpui::point(px(left + cell_w * 5.5), px(top + cell_h * 0.5)),
    );
    assert!(
        harness
            .workspace
            .read_with(cx, |ws, _| ws.selection)
            .is_some(),
        "终端一栏里拖选照常"
    );
}
