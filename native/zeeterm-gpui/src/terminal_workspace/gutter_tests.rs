//! 行号 / 时间戳栏（V2-U49）：开关关着时终端几何和以前逐位一致；开着时列数、命中测试、输入法 /
//! 建议浮层锚点都按栏宽整体右移，点在栏上不选区也不上报，复制不带栏里的文字。

use super::gutter::{
    self, MIN_COLS, format_number, format_time, gutter_cells, row_parts, split_columns,
};
use super::{PANE_PAD, Pane, Tab, TerminalWorkspace};
use crate::settings_store::AppSettings;
use gpui::{
    AppContext as _, Entity, IntoElement, ParentElement, Render, Styled, TestAppContext,
    WindowHandle, div, px, size,
};
use std::sync::{Arc, Mutex, mpsc};
use zeeterm_core::ssh::flow::FlowGate;
use zeeterm_session::{CoreFilter, SessionBackend, SessionCommand, SessionPump};
use zeeterm_terminal::GutterRow;
use zeeterm_terminal::Size;

struct Recorder {
    sizes: Arc<Mutex<Vec<Size>>>,
    writes: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl SessionBackend for Recorder {
    fn send(&mut self, command: SessionCommand) {
        match command {
            SessionCommand::Resize(size) => self.sizes.lock().expect("sizes").push(size),
            SessionCommand::Write(bytes) => self.writes.lock().expect("writes").push(bytes),
            SessionCommand::Close => {}
        }
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
    sizes: Arc<Mutex<Vec<Size>>>,
    writes: Arc<Mutex<Vec<Vec<u8>>>>,
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
        let (events_tx, events) = mpsc::channel();
        let window = cx.add_window(|window, cx| {
            let workspace = cx.new(|cx| {
                let mut workspace = TerminalWorkspace::new(window, cx, "gutter-test".into());
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
                        sizes: Arc::clone(&sizes),
                        writes: Arc::clone(&writes),
                    }),
                    events,
                    None,
                    None,
                );
                workspace.tabs.push(Tab::new("gutter".into(), pane));
                workspace
            });
            Host { workspace }
        });
        let workspace = window
            .read_with(cx, |host, _| host.workspace.clone())
            .expect("host");
        let harness = Self {
            window,
            workspace,
            sizes,
            writes,
            events: events_tx,
        };
        cx.simulate_window_resize(window.into(), size(px(800.0), px(500.0)));
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

    /// 改显示开关（和设置页同一个读缓存），然后画两帧。
    fn set_switches(&self, cx: &mut TestAppContext, numbers: bool, times: bool) {
        self.workspace.update(cx, |ws, cx| {
            ws.settings.show_line_numbers = numbers;
            ws.settings.show_timestamps = times;
            cx.notify();
        });
        self.draw(cx);
        self.draw(cx);
    }

    fn feed(&self, cx: &mut TestAppContext, text: &str) {
        self.events
            .send(zeeterm_session::SessionEvent::Data {
                generation: 1,
                bytes: text.as_bytes().to_vec(),
            })
            .expect("output");
        self.workspace.update(cx, |ws, cx| {
            ws.poll_sessions();
            cx.notify();
        });
        self.draw(cx);
    }

    fn mouse(&self, cx: &mut TestAppContext, x: f32, y: f32, down: bool) {
        let position = gpui::point(px(x), px(y));
        cx.update_window(self.window.into(), |_, window, cx| {
            let input = if down {
                gpui::PlatformInput::MouseDown(gpui::MouseDownEvent {
                    button: gpui::MouseButton::Left,
                    position,
                    modifiers: Default::default(),
                    click_count: 1,
                    first_mouse: false,
                })
            } else {
                gpui::PlatformInput::MouseUp(gpui::MouseUpEvent {
                    button: gpui::MouseButton::Left,
                    position,
                    modifiers: Default::default(),
                    click_count: 1,
                })
            };
            window.dispatch_event(input, cx);
        })
        .expect("mouse");
        self.draw(cx);
    }

    fn geom(&self, cx: &TestAppContext) -> super::PaneGeom {
        self.workspace.read_with(cx, |ws, _| ws.pane_geom[0])
    }
}

/// 没有栏时的原始算式（改动前 `render_in_area` 里的写法），逐项和现在的结果比。
fn legacy_cols(width: f32, cell: f32) -> usize {
    (((width - PANE_PAD * 2.0).max(0.0) / cell).floor() as usize).max(20)
}

#[gpui::test]
fn with_both_switches_off_the_geometry_is_what_it_always_was(cx: &mut TestAppContext) {
    let harness = Harness::open(cx);
    for (width, height) in [(800.0, 500.0), (1003.7, 611.3), (360.0, 240.0)] {
        cx.simulate_window_resize(harness.window.into(), size(px(width), px(height)));
        harness.draw(cx);
        harness.draw(cx);
        harness.workspace.read_with(cx, |ws, _| {
            let geom = ws.pane_geom[0];
            let (area_x, area_y) = ws.area_origin;
            assert_eq!(geom.cols, legacy_cols(width, ws.cell_width), "{width}");
            assert_eq!(geom.gutter, 0.0);
            assert_eq!(geom.gutter_cells, 0);
            // 数值逐位相同：文字区左上角还是 区域原点 + 内边距。
            assert_eq!(geom.left, geom.rect.x + PANE_PAD);
            assert_eq!(geom.top, geom.rect.y + PANE_PAD);
            assert_eq!((geom.rect.x, geom.rect.y), (area_x, area_y));
            assert_eq!(ws.pane().expect("pane").snapshot.size.cols, geom.cols);
            // 命中测试、鼠标上报、输入法原点、建议浮层锚点都是以前的公式。
            let cell = |col: usize, row: usize| {
                gpui::point(
                    px(geom.left + (col as f32 + 0.5) * ws.cell_width),
                    px(geom.top + (row as f32 + 0.5) * ws.cell_height),
                )
            };
            assert_eq!(ws.cell_at(cell(0, 0)), Some((0, 0, 0)));
            assert_eq!(
                ws.cell_at(cell(geom.cols - 1, 2)),
                Some((0, 2, geom.cols - 1))
            );
            assert_eq!(ws.mouse_cell_at(cell(3, 1)), (1, 3));
            let snapshot = &ws.pane().expect("pane").snapshot;
            assert_eq!(
                ws.cursor_origin_in_pane(0, snapshot),
                (
                    PANE_PAD + snapshot.cursor_col as f32 * ws.cell_width,
                    PANE_PAD + snapshot.cursor_row as f32 * ws.cell_height
                )
            );
            assert_eq!(
                ws.ime_cursor_origin(),
                Some((
                    geom.left + snapshot.cursor_col as f32 * ws.cell_width,
                    geom.top + snapshot.cursor_row as f32 * ws.cell_height
                ))
            );
            assert!(!ws.in_gutter(gpui::point(px(geom.left - 4.0), px(geom.top + 4.0))));
        });
    }
}

#[gpui::test]
fn turning_a_gutter_on_takes_whole_columns_and_sends_one_resize(cx: &mut TestAppContext) {
    let harness = Harness::open(cx);
    let before = harness.geom(cx);
    let cell = harness.workspace.read_with(cx, |ws, _| ws.cell_width);
    for (numbers, times, cells) in [(true, false, 7usize), (false, true, 9), (true, true, 16)] {
        harness.set_switches(cx, false, false);
        harness.sizes.lock().expect("sizes").clear();
        harness.set_switches(cx, numbers, times);
        let after = harness.geom(cx);
        assert_eq!(after.gutter_cells, cells);
        assert_eq!(after.cols, before.cols - cells, "列数少栏宽对应的格数");
        assert_eq!(after.gutter, cells as f32 * cell);
        assert_eq!(after.left, before.left + after.gutter, "文字区右移一个栏宽");
        assert_eq!(after.top, before.top);
        let sizes = harness.sizes.lock().expect("sizes").clone();
        assert_eq!(sizes.len(), 1, "只发一次 Resize：{sizes:?}");
        assert_eq!(sizes[0].cols, after.cols);
        // 稳定后再画几帧不再发 Resize。
        harness.draw(cx);
        harness.draw(cx);
        assert_eq!(harness.sizes.lock().expect("sizes").len(), 1);
    }
    // 关回去：列数回到原来的，同样一次 Resize。
    harness.sizes.lock().expect("sizes").clear();
    harness.set_switches(cx, false, false);
    assert_eq!(harness.geom(cx), before);
    assert_eq!(harness.sizes.lock().expect("sizes").len(), 1);
}

#[gpui::test]
fn hit_testing_follows_the_gutter(cx: &mut TestAppContext) {
    let harness = Harness::open(cx);
    harness.feed(cx, "hello world\r\nsecond line");
    harness.set_switches(cx, true, true);
    harness.workspace.read_with(cx, |ws, _| {
        let geom = ws.pane_geom[0];
        let point = |x: f32, y: f32| gpui::point(px(x), px(y));
        let mid_row = |row: usize| geom.top + (row as f32 + 0.5) * ws.cell_height;
        let text_x = |col: usize| geom.left + (col as f32 + 0.5) * ws.cell_width;
        // 第 0 列、最后一列。
        assert_eq!(ws.cell_at(point(text_x(0), mid_row(0))), Some((0, 0, 0)));
        assert_eq!(
            ws.cell_at(point(text_x(geom.cols - 1), mid_row(1))),
            Some((0, 1, geom.cols - 1))
        );
        // 文字区右边界之外没有格子。
        assert_eq!(ws.cell_at(point(text_x(geom.cols) + 1.0, mid_row(0))), None);
        // 栏上：没有格子、命中栏、鼠标上报的坐标钳到第 0 列。
        let in_gutter = point(geom.left - geom.gutter / 2.0, mid_row(0));
        assert_eq!(ws.cell_at(in_gutter), None);
        assert!(ws.in_gutter(in_gutter));
        assert_eq!(ws.mouse_cell_at(in_gutter), (0, 0));
        // 栏和文字区的交界：左边一像素是栏，右边是第 0 列。
        assert!(ws.in_gutter(point(geom.left - 0.5, mid_row(0))));
        assert!(!ws.in_gutter(point(geom.left, mid_row(0))));
        // 输入法候选位置、测试钩子的文字中心都按栏宽右移。
        let snapshot = &ws.pane().expect("pane").snapshot;
        assert_eq!(
            ws.ime_cursor_origin(),
            Some((
                geom.left + snapshot.cursor_col as f32 * ws.cell_width,
                geom.top + snapshot.cursor_row as f32 * ws.cell_height
            ))
        );
        assert_eq!(
            ws.cursor_origin_in_pane(0, snapshot).0,
            PANE_PAD + geom.gutter + snapshot.cursor_col as f32 * ws.cell_width
        );
        #[cfg(debug_assertions)]
        {
            let center = ws.dev_text_center("second").expect("文字在屏幕上");
            assert_eq!(center.0, geom.left + 3.5 * ws.cell_width);
        }
    });
}

#[gpui::test]
fn pressing_on_the_gutter_neither_selects_nor_reports(cx: &mut TestAppContext) {
    let harness = Harness::open(cx);
    harness.feed(cx, "hello world\r\nsecond line");
    harness.set_switches(cx, true, false);
    let geom = harness.geom(cx);
    let (cell_w, cell_h) = harness
        .workspace
        .read_with(cx, |ws, _| (ws.cell_width, ws.cell_height));
    let y = geom.top + cell_h * 0.5;
    // 先在文字区点一下：有锚点（单击不留空选区，但记锚点）。
    harness.mouse(cx, geom.left + cell_w * 0.5, y, true);
    harness.mouse(cx, geom.left + cell_w * 0.5, y, false);
    harness.workspace.update(cx, |ws, _| {
        ws.selecting = false;
        ws.drag_anchor = None;
        ws.selection = None;
    });
    // 点在栏上：不记锚点、不进入拖选。
    harness.mouse(cx, geom.left - geom.gutter / 2.0, y, true);
    harness.workspace.read_with(cx, |ws, _| {
        assert!(!ws.selecting, "点在栏上不开始选区");
        assert_eq!(ws.drag_anchor, None);
        assert_eq!(ws.selection, None);
    });
    harness.mouse(cx, geom.left - geom.gutter / 2.0, y, false);

    // 远端接管鼠标（1000 + SGR 1006）：文字区的点击会上报，栏上的点击不上报。
    harness.feed(cx, "\x1b[?1000h\x1b[?1006h");
    harness.writes.lock().expect("writes").clear();
    harness.mouse(cx, geom.left - geom.gutter / 2.0, y, true);
    harness.mouse(cx, geom.left - geom.gutter / 2.0, y, false);
    assert!(
        harness.writes.lock().expect("writes").is_empty(),
        "栏上的点击不向远端上报"
    );
    harness.mouse(cx, geom.left + cell_w * 2.5, y, true);
    harness.mouse(cx, geom.left + cell_w * 2.5, y, false);
    let writes = harness.writes.lock().expect("writes").clone();
    assert!(!writes.is_empty(), "文字区的点击照常上报");
    // 上报的列号是文字区自己的列（第 3 列 = 1 起的 3），不含栏宽。
    let text = String::from_utf8_lossy(&writes[0]).into_owned();
    assert!(text.starts_with("\x1b[<0;3;1M"), "{text:?}");
}

#[gpui::test]
fn copying_a_selection_never_includes_the_gutter_text(cx: &mut TestAppContext) {
    let harness = Harness::open(cx);
    harness.feed(cx, "hello world\r\nsecond line\r\nthird");
    harness.set_switches(cx, true, true);
    let geom = harness.geom(cx);
    let (cell_w, cell_h) = harness
        .workspace
        .read_with(cx, |ws, _| (ws.cell_width, ws.cell_height));
    // 从栏里开始拖（按第 0 列算）一直拖到第三行：选中的文字只有终端里的字。
    harness.mouse(cx, geom.left + cell_w * 0.5, geom.top + cell_h * 0.5, true);
    cx.update_window(harness.window.into(), |_, window, cx| {
        window.dispatch_event(
            gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {
                position: gpui::point(
                    px(geom.left - geom.gutter / 2.0),
                    px(geom.top + cell_h * 2.5),
                ),
                pressed_button: Some(gpui::MouseButton::Left),
                modifiers: Default::default(),
            }),
            cx,
        );
    })
    .expect("move");
    harness.draw(cx);
    let text = harness.workspace.read_with(cx, |ws, _| ws.selected_text());
    // 从栏里拖到第 3 行：落点按第 0 列算，选区含第 3 行的第一个字。
    assert_eq!(text, "hello world\nsecond line\nt");
    assert!(
        !text.chars().any(|c| c.is_ascii_digit()),
        "不含栏里的行号 / 时间：{text:?}"
    );
}

#[gpui::test]
fn the_full_screen_gutter_is_blank_but_keeps_its_width(cx: &mut TestAppContext) {
    let harness = Harness::open(cx);
    harness.feed(cx, "prompt$ ls\r\nfile-a\r\nfile-b");
    harness.set_switches(cx, true, true);
    let main = harness.geom(cx);
    harness.workspace.read_with(cx, |ws, _| {
        let rows = ws.pane().expect("pane").pump().gutter_rows();
        assert!(rows[0].number.is_some() && rows[0].time.is_some());
    });
    harness.feed(cx, "\x1b[?1049h\x1b[2Jvim screen\r\nmore");
    assert_eq!(harness.geom(cx), main, "进全屏程序列数、栏宽不变");
    harness.workspace.read_with(cx, |ws, _| {
        let rows = ws.pane().expect("pane").pump().gutter_rows();
        assert_eq!(rows.len(), main.rows);
        assert!(
            rows.iter()
                .all(|row| row.number.is_none() && row.time.is_none())
        );
    });
    harness.feed(cx, "\x1b[?1049l");
    assert_eq!(harness.geom(cx), main, "退出全屏程序列数、栏宽不变");
    harness.workspace.read_with(cx, |ws, _| {
        let rows = ws.pane().expect("pane").pump().gutter_rows();
        assert!(rows[0].number.is_some(), "回到主屏幕，栏里的内容回来了");
    });
}

#[gpui::test]
fn each_split_pane_gets_its_own_gutter(cx: &mut TestAppContext) {
    let harness = Harness::open(cx);
    harness.set_switches(cx, true, true);
    harness.workspace.update(cx, |ws, cx| {
        ws.split_active(crate::split_tree::Axis::X, cx);
    });
    harness.draw(cx);
    harness.draw(cx);
    harness.workspace.read_with(cx, |ws, _| {
        assert_eq!(ws.pane_geom.len(), 2);
        for geom in &ws.pane_geom {
            assert_eq!(geom.gutter_cells, 16);
            assert_eq!(geom.left, geom.rect.x + PANE_PAD + geom.gutter);
        }
    });
}

#[gpui::test]
fn a_very_narrow_pane_goes_without_a_gutter_instead_of_overflowing(cx: &mut TestAppContext) {
    let harness = Harness::open(cx);
    harness.set_switches(cx, true, true);
    cx.simulate_window_resize(harness.window.into(), size(px(240.0), px(300.0)));
    harness.draw(cx);
    harness.draw(cx);
    let geom = harness.geom(cx);
    assert_eq!(
        geom.gutter_cells, 0,
        "扣掉栏不足 {MIN_COLS} 列时这个窗格不画栏"
    );
    assert!(geom.cols >= MIN_COLS);
    // 窗口再变宽，栏回来。
    cx.simulate_window_resize(harness.window.into(), size(px(1000.0), px(300.0)));
    harness.draw(cx);
    harness.draw(cx);
    assert_eq!(harness.geom(cx).gutter_cells, 16);
}

#[test]
fn the_gutter_helpers_agree_with_the_pane_math() {
    // 栏宽对应的格数不依赖像素：同一个宽度、不同格宽，扣掉的列数始终是整数格。
    for cell in [7.0f32, 8.4, 9.6] {
        let (cols, used) = gutter::split_columns(900.0, cell, 16);
        let raw = (900.0f32 / cell).floor() as usize;
        assert_eq!((cols, used), (raw - 16, 16));
    }
}

// ---- 纯函数 ----

#[test]
fn the_gutter_width_is_fixed_whole_cells() {
    assert_eq!(gutter_cells(false, false), 0);
    // 行号 6 位 + 一格间隔。
    assert_eq!(gutter_cells(true, false), 7);
    // 时间戳 8 个字符 + 一格间隔。
    assert_eq!(gutter_cells(false, true), 9);
    // 时间戳、一格、行号、一格间隔。
    assert_eq!(gutter_cells(true, true), 16);
}

#[test]
fn with_no_gutter_the_columns_are_exactly_what_they_always_were() {
    // 回归：没开栏时和原来的 `floor(宽 / 格宽).max(20)` 逐个比较（含小数宽度、窄窗格）。
    for tenths in 0..=40_000u32 {
        let width = tenths as f32 / 10.0;
        for cell in [6.0f32, 7.2, 8.4, 9.6001, 11.0] {
            let old = ((width / cell).floor() as usize).max(20);
            assert_eq!(split_columns(width, cell, 0), (old, 0), "{width} / {cell}");
        }
    }
}

#[test]
fn a_gutter_takes_whole_columns_out_of_the_text_area() {
    let cell = 8.0;
    for (inner, raw) in [(800.0, 100usize), (803.9, 100), (400.0, 50)] {
        for gutter in [7usize, 9, 16] {
            assert_eq!(
                split_columns(inner, cell, gutter),
                (raw - gutter, gutter),
                "{inner} 宽 {gutter} 格栏"
            );
        }
    }
}

#[test]
fn a_pane_too_narrow_for_the_gutter_goes_without_one() {
    // 扣掉栏之后不足 20 列：这个窗格不画栏，列数照旧。
    assert_eq!(split_columns(8.0 * 26.0, 8.0, 7), (26, 0));
    assert_eq!(split_columns(8.0 * 27.0, 8.0, 7), (20, 7));
    assert_eq!(split_columns(8.0 * 10.0, 8.0, 7), (20, 0));
}

#[test]
fn time_and_number_text_have_fixed_width() {
    assert_eq!(format_time(0, 0), "00:00:00");
    assert_eq!(format_time(3_723, 0), "01:02:03");
    // 时区偏移：UTC+8、UTC-5.5，跨日回绕。
    assert_eq!(format_time(3_723, 8 * 3600), "09:02:03");
    assert_eq!(format_time(3_723, -(5 * 3600 + 1800)), "19:32:03");
    assert_eq!(format_time(i64::MAX, i64::MAX).len(), 8);
    assert_eq!(format_number(7), "     7");
    assert_eq!(format_number(123_456), "123456");
    assert_eq!(format_number(1_234_567), "234567");
    assert_eq!(format_number(1_000_000), "     0");
}

#[test]
fn a_row_without_content_is_blank_but_keeps_its_width() {
    let blank = GutterRow::default();
    assert_eq!(
        row_parts(&blank, true, true, 0),
        (Some(" ".repeat(8)), Some(" ".repeat(6)))
    );
    let full = GutterRow {
        number: Some(42),
        time: Some(61),
    };
    assert_eq!(
        row_parts(&full, true, true, 0),
        (Some("00:01:01".to_owned()), Some("    42".to_owned()))
    );
    // 只开一个：另一栏不出现。
    assert_eq!(row_parts(&full, true, false, 0).0, None);
    assert_eq!(row_parts(&full, false, true, 0).1, None);
}
