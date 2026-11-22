//! V2-U57：按钮被键盘聚焦时要有清晰的焦点轮廓，输入框聚焦时仍然只有一层边框（没有外圈光晕）。
//!
//! 全局的 `Theme.focus_ring`（外圈光晕）保持关闭——那是输入框「粗蓝双框」的来源（`app_theme::FOCUS_HALO`）；
//! 按钮的轮廓来自 `native/vendor/gpui-component-0.8.1` 里的本地改动：光晕关着时按钮改画 2px 轮廓
//! （带 `focus-outline` 调试选择器，只在 debug + test-support 构建里记录），所以这里能直接量到它。

use ::gpui::{
    AppContext as _, Context, Entity, FocusHandle, Focusable as _, IntoElement, ParentElement as _,
    Render, Styled as _, TestAppContext, VisualTestContext, Window, WindowOptions,
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{IconName, v_flex};

use crate::settings_store::AppSettings;

/// 三种按钮各一个，外加一个输入框；焦点句柄由测试持有，用 `track_focus` 交给它们。
struct Probe {
    primary: FocusHandle,
    normal: FocusHandle,
    ghost_icon: FocusHandle,
    outline_icon: FocusHandle,
    input_state: Entity<InputState>,
}

impl Probe {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            primary: cx.focus_handle(),
            normal: cx.focus_handle(),
            ghost_icon: cx.focus_handle(),
            outline_icon: cx.focus_handle(),
            input_state: cx.new(|cx| InputState::new(window, cx)),
        }
    }
}

impl Render for Probe {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_4()
            .p_8()
            .child(
                Button::new("primary")
                    .primary()
                    .label("OK")
                    .track_focus(&self.primary),
            )
            .child(
                Button::new("normal")
                    .label("Cancel")
                    .track_focus(&self.normal),
            )
            .child(
                Button::new("ghost-icon")
                    .ghost()
                    .icon(IconName::Search)
                    .track_focus(&self.ghost_icon),
            )
            .child(
                Button::new("outline-icon")
                    .outline()
                    .icon(IconName::Search)
                    .track_focus(&self.outline_icon),
            )
            .child(Input::new(&self.input_state))
    }
}

fn open(cx: &mut TestAppContext, dark: bool) -> (::gpui::AnyWindowHandle, Entity<Probe>) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        AppSettings::init_for_test(cx);
        crate::app_theme::set_mode(dark, cx);
    });
    let (handle, view) = cx
        .update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Probe::new(window, cx))
            })
        })
        .expect("开 Root 窗口");
    draw(cx, handle);
    (handle, view)
}

fn draw(cx: &mut TestAppContext, handle: ::gpui::AnyWindowHandle) {
    // 聚焦变化要下一帧才反映到样式里，多画两帧。
    for _ in 0..2 {
        cx.update_window(handle, |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .expect("画一帧");
    }
}

fn has_outline(cx: &mut TestAppContext, handle: ::gpui::AnyWindowHandle) -> bool {
    VisualTestContext::from_window(handle, cx)
        .debug_bounds("focus-outline")
        .is_some()
}

fn focus_only(cx: &mut TestAppContext, handle: ::gpui::AnyWindowHandle, target: &FocusHandle) {
    cx.update_window(handle, |_, window, cx| window.focus(target, cx))
        .expect("聚焦");
    draw(cx, handle);
}

#[gpui::test]
fn focused_buttons_draw_an_outline_and_unfocused_ones_do_not(cx: &mut TestAppContext) {
    for dark in [false, true] {
        let (handle, view) = open(cx, dark);
        assert!(
            !cx.update(|cx| gpui_kit::component::ActiveTheme::theme(cx).focus_ring),
            "全局光晕必须保持关闭（输入框不能回到双层蓝框）"
        );
        assert!(!has_outline(cx, handle), "{dark}：没有聚焦的按钮不画轮廓");
        let handles = view.read_with(cx, |probe, _| {
            [
                ("主按钮", probe.primary.clone()),
                ("普通按钮", probe.normal.clone()),
                ("幽灵图标按钮", probe.ghost_icon.clone()),
                ("描边图标按钮", probe.outline_icon.clone()),
            ]
        });
        for (name, target) in handles {
            focus_only(cx, handle, &target);
            assert!(
                has_outline(cx, handle),
                "dark={dark}：聚焦的{name}要有焦点轮廓"
            );
        }
        // 焦点移走（回到输入框）以后轮廓消失；输入框自己没有按钮的轮廓。
        let input_focus =
            view.read_with(cx, |probe, cx| probe.input_state.read(cx).focus_handle(cx));
        focus_only(cx, handle, &input_focus);
        assert!(
            !has_outline(cx, handle),
            "dark={dark}：输入框聚焦时没有按钮轮廓，也不能出现外圈光晕"
        );
        cx.update_window(handle, |_, window, cx| window.blur(cx))
            .expect("取消聚焦");
        draw(cx, handle);
        assert!(!has_outline(cx, handle), "dark={dark}：没有焦点时不画轮廓");
    }
}

/// 走真实的 Tab 键路径（组件库 `Root` 的 Tab 绑定），而不是每次直接 `focus()`：焦点从第一个按钮出发，
/// 每按一次 Tab 轮廓跟着焦点走到下一个按钮，直到输入框接过焦点轮廓才消失。
#[gpui::test]
fn tabbing_through_buttons_moves_the_outline_with_the_focus(cx: &mut TestAppContext) {
    let (handle, view) = open(cx, true);
    let order = view.read_with(cx, |probe, _| {
        [
            probe.primary.clone(),
            probe.normal.clone(),
            probe.ghost_icon.clone(),
            probe.outline_icon.clone(),
        ]
    });
    focus_only(cx, handle, &order[0]);
    let mut vcx = VisualTestContext::from_window(handle, cx);
    assert!(vcx.debug_bounds("focus-outline").is_some());
    for (index, expected) in order.iter().enumerate().skip(1) {
        vcx.simulate_keystrokes("tab");
        let focused = vcx.update(|window, _| expected.is_focused(window));
        assert!(focused, "Tab 应该落在第 {} 个按钮上", index + 1);
        assert!(
            vcx.debug_bounds("focus-outline").is_some(),
            "Tab 到第 {} 个按钮后要有焦点轮廓",
            index + 1
        );
    }
    vcx.simulate_keystrokes("tab");
    assert!(
        vcx.debug_bounds("focus-outline").is_none(),
        "Tab 到输入框以后按钮轮廓消失，输入框不画按钮轮廓"
    );
}
