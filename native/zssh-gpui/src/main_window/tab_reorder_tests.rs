//! 主窗口标签栏里的拖动换位（`tab_move` + `tab_drag`）的窗口测试：真的画出主窗口的标题栏，用模拟的鼠标
//! 按下 / 移动 / 抬起和 Esc 走一遍。
//!
//! 要一个独立的数据目录（`MainWindow::new` 会打开主机库）：没设 `ZSSH_DATA_DIR` 就跳过并打印原因，免得
//! 碰到真实数据；门禁脚本会给它指一个临时目录。

use ::gpui::{
    AppContext as _, Modifiers, MouseButton, TestAppContext, VisualTestContext, WindowHandle,
    point, px,
};

use super::MainWindow;
use crate::settings_store::AppSettings;
use crate::tab_drag::{self, DragKey};

/// 指针落在标题栏正中（标签所在的高度）。
const BAR_MIDDLE: f32 = super::titlebar::BAR_HEIGHT / 2.0;

struct Harness {
    window: WindowHandle<MainWindow>,
    /// 三个终端标签的编号，按开始时的顺序。
    ids: [u64; 3],
}

impl Harness {
    /// 开一个有三个终端标签的主窗口，选中第 `active` 个。
    fn open(cx: &mut TestAppContext, active: usize) -> Option<Self> {
        if std::env::var_os("ZSSH_DATA_DIR").is_none() {
            eprintln!("跳过标签换位的窗口测试：需要 ZSSH_DATA_DIR 指向独立数据目录");
            return None;
        }
        cx.update(|cx| {
            gpui_kit::init(cx);
            AppSettings::init_for_test(cx);
        });
        let window = cx.add_window(MainWindow::new);
        let ids = window
            .update(cx, |main, window, cx| {
                window.focus(&main.focus, cx);
                main.workspace.update(cx, |ws, cx| {
                    let ids = [
                        ws.push_test_tab("web-1"),
                        ws.push_test_tab("web-2"),
                        ws.push_test_tab("web-3"),
                    ];
                    ws.select_tab(active, cx);
                    ids
                })
            })
            .expect("main window");
        let harness = Self { window, ids };
        harness.draw(cx);
        harness.draw(cx);
        Some(harness)
    }

    fn draw(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .expect("draw");
    }

    /// 标签现在的顺序（编号）。
    fn order(&self, cx: &mut TestAppContext) -> Vec<u64> {
        self.window
            .read_with(cx, |main, cx| main.workspace.read(cx).tab_ids())
            .expect("order")
    }

    /// 选中的标签编号。
    fn active(&self, cx: &mut TestAppContext) -> Option<u64> {
        self.window
            .read_with(cx, |main, cx| main.workspace.read(cx).active_tab_id())
            .expect("active")
    }

    /// 标签现在的横向中点（画出来的真实位置），按标签条上的顺序。
    fn centres(&self, cx: &mut TestAppContext) -> Vec<f32> {
        let keys: Vec<DragKey> = self.order(cx).into_iter().map(DragKey::Terminal).collect();
        let spans = self
            .window
            .read_with(cx, |main, _| main.tab_dragger.spans().collect(&keys))
            .expect("spans");
        spans
            .expect("每个标签都画过一帧")
            .iter()
            .map(|(left, right)| (left + right) / 2.0)
            .collect()
    }

    /// 窗口还有没有按下的标签（拖动没结束）。
    fn pressed(&self, cx: &mut TestAppContext) -> bool {
        self.window
            .read_with(cx, |main, _| main.tab_dragger.pending())
            .expect("pressed")
    }
}

fn at(x: f32) -> ::gpui::Point<::gpui::Pixels> {
    point(px(x), px(BAR_MIDDLE))
}

/// 在第一个标签上按下，往右拖到刚好越过第二个标签的中线。
fn press_first_and_drag_over_the_second(
    harness: &Harness,
    cx: &mut TestAppContext,
    vcx: &mut VisualTestContext,
) {
    let centres = harness.centres(cx);
    let (first, second) = (centres[0], centres[1]);
    vcx.simulate_mouse_down(at(first), MouseButton::Left, Modifiers::none());
    // 按下之后重画一帧，窗口级的鼠标监听才挂上（真窗口里按下会通知重画）。
    harness.draw(cx);
    // 先越过起拖距离，但还没到邻居的中线。
    vcx.simulate_mouse_move(at(first + 10.0), MouseButton::Left, Modifiers::none());
    harness.draw(cx);
    // 再越过邻居的中线。
    vcx.simulate_mouse_move(at(second + 4.0), MouseButton::Left, Modifiers::none());
    harness.draw(cx);
}

#[gpui::test]
fn dragging_a_tab_over_its_neighbour_swaps_them_and_releasing_keeps_the_order(
    cx: &mut TestAppContext,
) {
    let Some(harness) = Harness::open(cx, 0) else {
        return;
    };
    let [a, b, c] = harness.ids;
    assert_eq!(harness.order(cx), [a, b, c]);
    let mut vcx = VisualTestContext::from_window(harness.window.into(), cx);

    // 没越过起拖距离（手抖）：不换位。
    let first = harness.centres(cx)[0];
    vcx.simulate_mouse_down(at(first), MouseButton::Left, Modifiers::none());
    harness.draw(cx);
    vcx.simulate_mouse_move(at(first + 3.0), MouseButton::Left, Modifiers::none());
    harness.draw(cx);
    assert_eq!(harness.order(cx), [a, b, c]);
    vcx.simulate_mouse_up(at(first + 3.0), MouseButton::Left, Modifiers::none());
    harness.draw(cx);

    // 真的拖：越过相邻标签的中线才换。
    press_first_and_drag_over_the_second(&harness, cx, &mut vcx);
    assert_eq!(harness.order(cx), [b, a, c], "越过中线就换位");
    assert_eq!(
        harness.active(cx),
        Some(a),
        "选中的仍是原来那个标签，只是位置变了"
    );
    assert!(harness.pressed(cx));
    // 指针继续往右，越过第三个标签的中线：再换一次。
    let third = harness.centres(cx)[2];
    vcx.simulate_mouse_move(at(third + 4.0), MouseButton::Left, Modifiers::none());
    harness.draw(cx);
    assert_eq!(harness.order(cx), [b, c, a]);
    // 往回拖，只到第二个标签的中线右边一点（没越过它的中线）：不换回来。
    let second_now = harness.centres(cx)[1];
    vcx.simulate_mouse_move(at(second_now + 4.0), MouseButton::Left, Modifiers::none());
    harness.draw(cx);
    assert_eq!(harness.order(cx), [b, c, a], "没越过中线不换回来");

    // 松手：保留新顺序，选中不变，拖动状态（本窗口和全局的）都清掉。
    vcx.simulate_mouse_up(at(second_now + 4.0), MouseButton::Left, Modifiers::none());
    harness.draw(cx);
    assert_eq!(harness.order(cx), [b, c, a]);
    assert_eq!(harness.active(cx), Some(a));
    assert!(!harness.pressed(cx));
    assert!(cx.update(|cx| tab_drag::current(cx).is_none()));
}

#[gpui::test]
fn escape_puts_the_tabs_back_in_the_order_they_had_before_the_drag(cx: &mut TestAppContext) {
    let Some(harness) = Harness::open(cx, 1) else {
        return;
    };
    let [a, b, c] = harness.ids;
    let mut vcx = VisualTestContext::from_window(harness.window.into(), cx);

    press_first_and_drag_over_the_second(&harness, cx, &mut vcx);
    let third = harness.centres(cx)[2];
    vcx.simulate_mouse_move(at(third + 4.0), MouseButton::Left, Modifiers::none());
    harness.draw(cx);
    assert_eq!(harness.order(cx), [b, c, a], "拖动中已经换过两次位");
    assert_eq!(harness.active(cx), Some(b));

    vcx.simulate_keystrokes("escape");
    harness.draw(cx);
    assert_eq!(harness.order(cx), [a, b, c], "Esc 恢复拖动开始前的顺序");
    assert_eq!(harness.active(cx), Some(b), "选中的标签不受影响");
    assert!(!harness.pressed(cx));
    assert!(cx.update(|cx| tab_drag::current(cx).is_none()));

    // 之后再松手（鼠标那时还按着）也没有任何东西再动。
    vcx.simulate_mouse_up(at(third + 4.0), MouseButton::Left, Modifiers::none());
    harness.draw(cx);
    assert_eq!(harness.order(cx), [a, b, c]);
}

#[gpui::test]
fn a_drag_that_ends_on_the_tab_it_started_from_does_not_select_it(cx: &mut TestAppContext) {
    let Some(harness) = Harness::open(cx, 2) else {
        return;
    };
    let [a, b, c] = harness.ids;
    let mut vcx = VisualTestContext::from_window(harness.window.into(), cx);

    // 在没选中的第一个标签上按下，拖开几个像素又回到原处松手：算拖动，不是点击，所以不切到它。
    let first = harness.centres(cx)[0];
    vcx.simulate_mouse_down(at(first), MouseButton::Left, Modifiers::none());
    harness.draw(cx);
    vcx.simulate_mouse_move(at(first + 12.0), MouseButton::Left, Modifiers::none());
    harness.draw(cx);
    vcx.simulate_mouse_up(at(first), MouseButton::Left, Modifiers::none());
    harness.draw(cx);
    assert_eq!(harness.order(cx), [a, b, c]);
    assert_eq!(harness.active(cx), Some(c), "拖完不改变选中");

    // 单击（没有拖）照常选中。
    vcx.simulate_click(at(first), Modifiers::none());
    harness.draw(cx);
    assert_eq!(harness.active(cx), Some(a), "普通单击仍然切换标签");
}
