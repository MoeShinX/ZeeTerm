//! V2-U53：文件列表和传输队列之间拖动条的窗口测试。真的画出文件视图，用模拟的鼠标走一遍：
//! 没拖过时队列还是按内容、最高 224；往上 / 往下拖、拖到最低收起、双击恢复默认；
//! 高度存本机设置；窗口变矮时按列表优先夹取；点头部展开 / 收起照旧。
//!
//! 要一个独立的数据目录（视图启动时读设置库，松手时写）：没设 `ZSSH_DATA_DIR` 就跳过并打印原因。
//! 不连任何服务器：主机 7 在测试库里不存在，视图连接失败，只关心下半部的布局。
//! 不写 `use super::*`：父模块 glob 进来的 `gpui_kit::test` 会盖掉内置的 `#[test]`。

use std::sync::{Mutex, MutexGuard};

use ::gpui::{
    AppContext as _, Modifiers, MouseButton, MouseDownEvent, TestAppContext, VisualTestContext,
    WindowHandle, point, px, size,
};

use super::SftpDemo;
use super::queue_height::{self, CONTENT_MIN, LIST_MIN, NATURAL_MAX};
use super::queue_resize::{self, QueueBody};
use crate::settings_store::AppSettings;

/// 几个测试共用同一个设置库里的这一项，串行跑。
static STATE: Mutex<()> = Mutex::new(());

struct Harness {
    window: WindowHandle<SftpDemo>,
}

impl Harness {
    fn open(cx: &mut TestAppContext) -> Option<(MutexGuard<'static, ()>, Self)> {
        if std::env::var_os("ZSSH_DATA_DIR").is_none() {
            eprintln!("跳过传输队列拖动条的窗口测试：需要 ZSSH_DATA_DIR 指向独立数据目录");
            return None;
        }
        let guard = STATE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // 从「没拖过」开始。
        let db = crate::host_db::open_database().expect("库");
        queue_height::save(&db, None).expect("写");
        cx.update(|cx| {
            gpui_kit::init(cx);
            AppSettings::init_for_test(cx);
        });
        let window = cx.add_window(|window, cx| SftpDemo::new_embedded(window, cx, 7, None, true));
        let harness = Self { window };
        harness.draw(cx);
        harness.draw(cx);
        Some((guard, harness))
    }

    fn draw(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .expect("draw");
    }

    fn vcx(&self, cx: &mut TestAppContext) -> VisualTestContext {
        VisualTestContext::from_window(self.window.into(), cx)
    }

    fn read<R>(&self, cx: &mut TestAppContext, f: impl FnOnce(&SftpDemo) -> R) -> R {
        self.window.read_with(cx, |view, _| f(view)).expect("view")
    }

    fn with<R>(
        &self,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut SftpDemo, &mut ::gpui::Context<SftpDemo>) -> R,
    ) -> R {
        self.window
            .update(cx, |view, _, cx| f(view, cx))
            .expect("view")
    }

    /// 队列内容区现在画出来多高（画面量到的）。
    fn content(&self, cx: &mut TestAppContext) -> f32 {
        self.read(cx, |view| view.queue_content_height.get())
    }

    fn list(&self, cx: &mut TestAppContext) -> f32 {
        self.read(cx, |view| view.list_height.get())
    }

    /// 在拖动条上按下，挪 `dy`（负 = 往上），松手。
    fn drag(&self, cx: &mut TestAppContext, dy: f32) {
        let mut vcx = self.vcx(cx);
        let start = vcx
            .debug_bounds("sftp-queue-resize")
            .expect("拖动条")
            .center();
        vcx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
        self.draw(cx);
        let mut vcx = self.vcx(cx);
        let window_height = vcx.update(|window, _| f32::from(window.viewport_size().height));
        let moved = point(
            start.x,
            px((f32::from(start.y) + dy).clamp(2.0, window_height - 2.0)),
        );
        vcx.simulate_mouse_move(moved, MouseButton::Left, Modifiers::none());
        self.draw(cx);
        self.draw(cx);
        let mut vcx = self.vcx(cx);
        vcx.simulate_mouse_up(moved, MouseButton::Left, Modifiers::none());
        self.draw(cx);
        self.draw(cx);
    }

    fn saved() -> Option<f32> {
        queue_resize::load_saved()
    }
}

/// 回归：没拖过的视图队列内容区还是按内容、最高 224，设置里没有值。
#[gpui::test]
fn an_untouched_queue_keeps_its_natural_height(cx: &mut TestAppContext) {
    let Some((_guard, h)) = Harness::open(cx) else {
        return;
    };
    assert_eq!(h.read(cx, |v| v.queue_shown()), (true, QueueBody::Natural));
    let natural = h.content(cx);
    assert!(
        natural > 0.0 && natural <= NATURAL_MAX,
        "空队列按内容，不超过 224：{natural}"
    );
    assert_eq!(Harness::saved(), None);
}

/// 回归：点队列头部照旧展开 / 收起；点一下拖动条（没拖动）什么都不改。
#[gpui::test]
fn the_header_still_toggles_and_a_bare_click_on_the_handle_changes_nothing(
    cx: &mut TestAppContext,
) {
    let Some((_guard, h)) = Harness::open(cx) else {
        return;
    };
    let mut vcx = h.vcx(cx);
    let header = vcx
        .debug_bounds("sftp-queue-toggle")
        .expect("头部")
        .center();
    vcx.simulate_click(header, Modifiers::none());
    h.draw(cx);
    assert!(!h.read(cx, |v| v.queue_open), "收起");
    assert_eq!(h.content(cx), 0.0, "收起后没有内容区");
    let mut vcx = h.vcx(cx);
    let header = vcx
        .debug_bounds("sftp-queue-toggle")
        .expect("头部")
        .center();
    vcx.simulate_click(header, Modifiers::none());
    h.draw(cx);
    assert!(h.read(cx, |v| v.queue_open), "再点展开");

    let mut vcx = h.vcx(cx);
    let handle = vcx
        .debug_bounds("sftp-queue-resize")
        .expect("拖动条")
        .center();
    vcx.simulate_click(handle, Modifiers::none());
    h.draw(cx);
    assert!(h.read(cx, |v| v.queue_open));
    assert_eq!(h.read(cx, |v| v.queue_height), None);
    assert_eq!(Harness::saved(), None);
}

#[gpui::test]
fn dragging_up_makes_the_queue_taller_and_the_list_shorter_and_remembers_it(
    cx: &mut TestAppContext,
) {
    let Some((_guard, h)) = Harness::open(cx) else {
        return;
    };
    let (content0, list0) = (h.content(cx), h.list(cx));
    h.drag(cx, -100.0);
    let want = queue_height::clamp_content(content0 + 100.0, list0 + content0);
    let stored = h.read(cx, |v| v.queue_height).expect("拖过了");
    assert!((stored - want).abs() <= 2.0, "存的 {stored}，预期约 {want}");
    assert_eq!(Harness::saved(), Some(stored), "松手写进设置");
    // 固定高度：任务再少也是这么高；列表相应变矮，两者之和不变。
    h.draw(cx);
    let (content1, list1) = (h.content(cx), h.list(cx));
    assert!(
        (content1 - stored).abs() <= 1.0,
        "内容区 {content1}，存的 {stored}"
    );
    assert!(list1 < list0, "列表变矮：{list0} -> {list1}");
    assert!(
        ((content1 + list1) - (content0 + list0)).abs() <= 2.0,
        "总高度不变"
    );
    assert!(h.read(cx, |v| v.queue_open));
}

#[gpui::test]
fn dragging_down_shrinks_the_queue_but_never_below_the_minimum_until_it_collapses(
    cx: &mut TestAppContext,
) {
    let Some((_guard, h)) = Harness::open(cx) else {
        return;
    };
    h.drag(cx, -150.0);
    let tall = h.read(cx, |v| v.queue_height).expect("拖过了");
    h.drag(cx, 60.0);
    let shorter = h.read(cx, |v| v.queue_height).expect("还在");
    assert!(shorter < tall, "{tall} -> {shorter}");
    assert!(shorter >= CONTENT_MIN);
    // 一直拖到窗口最底：收起，高度仍是拖之前那个（不记 0）。
    h.drag(cx, 5000.0);
    assert!(!h.read(cx, |v| v.queue_open), "拖到最低收起");
    assert_eq!(h.read(cx, |v| v.queue_height), Some(shorter));
    assert_eq!(Harness::saved(), Some(shorter));
    assert_eq!(h.content(cx), 0.0);
    // 收起时往上拖：先展开，再按拖到的位置定高度。
    h.drag(cx, -120.0);
    assert!(h.read(cx, |v| v.queue_open), "往上拖会展开");
    let reopened = h.read(cx, |v| v.queue_height).expect("有值");
    assert!(reopened >= CONTENT_MIN);
    assert!(h.content(cx) > 0.0);
}

#[gpui::test]
fn double_clicking_the_handle_restores_the_default(cx: &mut TestAppContext) {
    let Some((_guard, h)) = Harness::open(cx) else {
        return;
    };
    h.drag(cx, -150.0);
    assert!(h.read(cx, |v| v.queue_height).is_some());
    h.with(cx, |view, cx| {
        let event = MouseDownEvent {
            button: MouseButton::Left,
            position: point(px(10.0), px(10.0)),
            modifiers: Modifiers::none(),
            click_count: 2,
            first_mouse: false,
        };
        view.queue_drag_start(&event, cx);
    });
    h.draw(cx);
    h.draw(cx);
    assert_eq!(h.read(cx, |v| v.queue_height), None);
    assert_eq!(Harness::saved(), None, "设置里回到没拖过");
    assert!(h.read(cx, |v| v.queue_drag.is_none()), "双击不开始拖动");
    assert_eq!(h.read(cx, |v| v.queue_shown()), (true, QueueBody::Natural));
}

/// 窗口 / 面板变矮：固定高度自动夹取，文件列表不被挤没、也不溢出。
#[gpui::test]
fn a_short_window_clamps_the_fixed_height_and_keeps_the_list(cx: &mut TestAppContext) {
    let Some((_guard, h)) = Harness::open(cx) else {
        return;
    };
    // 存一个很大的值（比如在大屏上拖出来的）。
    h.with(cx, |view, cx| {
        view.queue_height = Some(900.0);
        cx.notify();
    });
    for height in [900.0, 600.0, 450.0, 380.0] {
        cx.simulate_window_resize(h.window.into(), size(px(520.0), px(height)));
        for _ in 0..4 {
            h.draw(cx);
        }
        let (content, list) = (h.content(cx), h.list(cx));
        assert!(
            list >= LIST_MIN - 1.0,
            "窗口 {height}：列表 {list} 不能小于 {LIST_MIN}"
        );
        assert!(content <= 900.0);
        assert!(
            content + list <= height,
            "窗口 {height}：内容区 {content} + 列表 {list} 溢出"
        );
    }
    // 设置里的值不因为窗口矮就被改掉。
    assert_eq!(h.read(cx, |v| v.queue_height), Some(900.0));
}

/// 另一个视图（右侧停靠 / 左侧边栏 / 文件窗口 / 标签）拖过之后，这个视图在心跳里跟着换。
#[gpui::test]
fn another_view_dragging_updates_this_one(cx: &mut TestAppContext) {
    let Some((_guard, h)) = Harness::open(cx) else {
        return;
    };
    assert_eq!(h.read(cx, |v| v.queue_height), None);
    // 模拟别的视图松手：写库 + 代数加一（走它自己的写入口）。
    let other = cx.add_window(|window, cx| SftpDemo::new_embedded(window, cx, 7, None, false));
    other
        .update(cx, |view, _, cx| {
            let event_state = queue_height::QueueState {
                open: true,
                stored: Some(210.0),
            };
            view.queue_height = None;
            view.apply_queue_state(event_state, cx);
        })
        .expect("other");
    h.with(cx, |view, _| {
        assert!(view.refresh_shared_queue_height(), "发现代数变了，重读");
    });
    assert_eq!(h.read(cx, |v| v.queue_height), Some(210.0));
    // 再调一次：没有新变化。
    h.with(cx, |view, _| assert!(!view.refresh_shared_queue_height()));
}

/// 源码层面的回归：没拖过时的老写法（224 上限、头部 30 高、窄栏排法）原样还在。
#[test]
fn the_untouched_path_keeps_the_old_layout_code() {
    let source = include_str!("view.rs");
    let code = source.split("#[cfg(test)]").next().unwrap_or(source);
    assert!(code.contains(".when(fixed.is_none(), |d| d.max_h(px(224.0)))"));
    assert!(code.contains(".h(px(30.0))"));
    assert!(code.contains("let narrow = queue_narrow(dock, self.view_width.get());"));
    assert!(code.contains("Button::new(\"sftp-clear-finished\")"));
    assert_eq!(NATURAL_MAX, 224.0);
}

/// 审核修复（V2-U54）：矮窗口里队列内容区只能显示一点点（比最小值还矮）时拖动，存下来的仍是合格的值
/// （不小于最小高度，读设置时不会被当成坏值丢掉），重开后还在；窗口变高后按想要的高度显示。
#[gpui::test]
fn dragging_in_a_short_window_stores_a_valid_height_that_a_tall_window_then_shows(
    cx: &mut TestAppContext,
) {
    let Some((_guard, h)) = Harness::open(cx) else {
        return;
    };
    // 矮窗口：文件列表留 120 之后，队列内容区只剩不到最小高度的一点点。
    let height = 230.0;
    cx.simulate_window_resize(h.window.into(), size(px(520.0), px(height)));
    for _ in 0..4 {
        h.draw(cx);
    }
    let room = h.content(cx) + h.list(cx);
    let shown = queue_height::max_content(room);
    assert!(
        (1.0..CONTENT_MIN).contains(&shown),
        "窗口 {height} 的可用高度 {room} 要让内容区最多显示 {shown}（应在 1 到 {CONTENT_MIN} 之间）"
    );
    h.drag(cx, -40.0);
    let stored = h.read(cx, |v| v.queue_height).expect("拖过了");
    assert!(
        stored >= CONTENT_MIN,
        "存了 {stored}，窗口 {height}，当时显示 {shown}"
    );
    assert_eq!(Harness::saved(), Some(stored), "读设置时原样读回");
    // 窗口变高：按存的（想要的）高度显示。
    cx.simulate_window_resize(h.window.into(), size(px(520.0), px(900.0)));
    for _ in 0..4 {
        h.draw(cx);
    }
    assert!(
        (h.content(cx) - stored).abs() <= 1.0,
        "{} vs {stored}",
        h.content(cx)
    );
}
