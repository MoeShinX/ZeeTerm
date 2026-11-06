//! 窗口外框：谁来画标题栏按钮、谁来响应拖动 / 缩放 / 关闭。
//!
//! Windows 上所有窗口都自绘标题栏，点击由系统按 `WindowControlArea` 做命中测试，这里不碰（外观和行为
//! 与以前逐像素一致）。Linux 上 GPUI 不处理 `WindowControlArea`，GNOME 的 Wayland 也不提供服务端装饰，
//! 所以要自己做三件事：
//!
//! 1. 请求客户端装饰（`WindowDecorations::Client`）。合成器坚持服务端装饰时（KDE、X11 窗口管理器自己画框）
//!    `window.window_decorations()` 会是 `Server`：这时不画自己的按钮、不留边距，避免两层标题栏。
//! 2. 标题栏拖动区：左键按下后第一次移动 → `start_window_move`，双击 → 最大化 / 还原，右键 → 系统窗口菜单。
//!    按下不动不会发起拖动，单击标签、按钮不受影响（可点元素照旧 `occlude()` 把拖动区在它们身上挖掉）。
//! 3. 关闭按钮必须和 Windows 走同一条路：Windows 上系统发 WM_CLOSE，GPUI 调窗口注册的
//!    `on_window_should_close`，返回 true 才真正关。GPUI 没有“应用自己发起关闭请求”的接口，所以所有带标题栏的
//!    窗口改用 [`on_should_close`] 注册同一个回调，它同时记在一张表里，关闭按钮从表里取出来调用。
//!
//! 窗口边缘的缩放、细边框、阴影由 GPUI 组件库的 `window_border` 负责：主窗口的 `Root` 已经包了一层，
//! 不走 `Root` 的窗口（文件窗口、终端窗口、更新窗口）用 [`frame`] 套上同一层。

use std::collections::HashMap;
use std::rc::Rc;

use ::gpui::{
    AnyElement, App, Decorations, Global, InteractiveElement, IntoElement, MouseButton, Window,
    WindowControlArea, WindowControls, WindowDecorations, WindowId, WindowOptions,
};

/// 所有窗口共用的应用标识：Wayland 的 `app_id`、X11 的 `WM_CLASS`。GNOME 的任务栏 / Alt+Tab 靠它把窗口归到
/// 同一个应用并找到图标和名字，所以 `.desktop` 文件必须叫 `app.zeeterm.ZeeTerm.desktop`，并写
/// `StartupWMClass=app.zeeterm.ZeeTerm`。
pub const APP_ID: &str = "app.zeeterm.ZeeTerm";

/// 一个窗口的外框由谁画。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chrome {
    /// 系统按 `WindowControlArea` 处理（Windows）。
    Native,
    /// Linux，客户端装饰：自己画按钮、处理拖动和缩放。
    Client,
    /// Linux，合成器 / 窗口管理器画框：什么都不画。
    Server,
}

/// 按平台和合成器给的装饰模式决定外框由谁画（纯函数，方便测）。
pub fn chrome_for(linux: bool, decorations: Decorations) -> Chrome {
    if !linux {
        Chrome::Native
    } else if matches!(decorations, Decorations::Client { .. }) {
        Chrome::Client
    } else {
        Chrome::Server
    }
}

/// 这个窗口现在的外框模式。
pub fn chrome(window: &Window) -> Chrome {
    chrome_for(cfg!(target_os = "linux"), window.window_decorations())
}

/// 要画哪几个窗口按钮。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Buttons {
    pub minimize: bool,
    pub maximize: bool,
    /// 关闭永远有：合成器不支持最小化 / 最大化（平铺式）时，关闭仍然由我们提供。
    pub close: bool,
}

/// 合成器声明的能力 + 窗口自己的属性 → 显示哪些按钮。`with_maximize` 为假的窗口（更新窗口）永远不放最大化。
pub fn buttons_for(
    supported: WindowControls,
    minimizable: bool,
    resizable: bool,
    with_maximize: bool,
) -> Buttons {
    Buttons {
        minimize: supported.minimize && minimizable,
        maximize: with_maximize && supported.maximize && resizable,
        close: true,
    }
}

/// 双击标题栏空白处是否可以最大化 / 还原（和最大化按钮同一条件，但更新窗口这类不放按钮的窗口也照系统习惯可以双击）。
pub fn can_zoom(supported: WindowControls, resizable: bool) -> bool {
    supported.maximize && resizable
}

/// Linux 上请求客户端装饰；其他平台不指定（保持 GPUI 默认，和以前一样）。
pub fn decorations_request(linux: bool) -> Option<WindowDecorations> {
    linux.then_some(WindowDecorations::Client)
}

/// 应用标识：只有 Linux 需要。
pub fn app_id_for(linux: bool) -> Option<String> {
    linux.then(|| APP_ID.to_owned())
}

/// 给没有自绘标题栏的窗口（演示 / 调试窗口，系统画标题栏）用：只带上应用标识。
pub fn app_options() -> WindowOptions {
    WindowOptions {
        app_id: app_id_for(cfg!(target_os = "linux")),
        ..Default::default()
    }
}

/// 给自绘标题栏的窗口用：应用标识 + 客户端装饰。
pub fn client_options() -> WindowOptions {
    WindowOptions {
        app_id: app_id_for(cfg!(target_os = "linux")),
        window_decorations: decorations_request(cfg!(target_os = "linux")),
        ..Default::default()
    }
}

/// 不走 `Root` 的窗口在最外层套一下：Linux 上加细边框、阴影和边缘缩放（客户端装饰时），服务端装饰时原样返回；
/// 其他平台原样返回。
#[cfg(target_os = "linux")]
pub fn frame(content: impl IntoElement) -> AnyElement {
    use ::gpui::ParentElement as _;
    gpui_kit::component::window_border()
        .child(content)
        .into_any_element()
}

#[cfg(not(target_os = "linux"))]
pub fn frame(content: impl IntoElement) -> AnyElement {
    content.into_any_element()
}

// ---------------------------------------------------------------------------------------------
// 标题栏拖动区

/// 拖动区手势的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragAction {
    None,
    /// 让合成器接管窗口移动。
    StartMove,
    /// 最大化 / 还原。
    Zoom,
}

/// 拖动区的手势状态机（纯逻辑，和 GPUI 事件分开，方便测）：
/// 按下只记一笔，真正的移动等到按住左键移动第一下才发起，这样单击、双击不会被一次移动抢走。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DragGesture {
    pending: bool,
}

impl DragGesture {
    /// 左键按下。`clicks` 是连续点击次数。
    pub fn press(&mut self, clicks: usize, can_zoom: bool) -> DragAction {
        if clicks >= 2 {
            self.pending = false;
            return if can_zoom {
                DragAction::Zoom
            } else {
                DragAction::None
            };
        }
        self.pending = true;
        DragAction::None
    }

    /// 指针移动。`left_down` 是移动时左键是否还按着。
    pub fn motion(&mut self, left_down: bool) -> DragAction {
        if !left_down {
            self.pending = false;
            return DragAction::None;
        }
        if std::mem::take(&mut self.pending) {
            DragAction::StartMove
        } else {
            DragAction::None
        }
    }

    /// 左键松开。
    pub fn release(&mut self) {
        self.pending = false;
    }
}

/// 全应用只有一个鼠标：按下发生在哪个窗口的拖动区，就只有那个窗口的后续移动能发起窗口移动。
#[derive(Default)]
struct PendingDrag {
    window: Option<WindowId>,
    gesture: DragGesture,
}

impl Global for PendingDrag {}

/// 把一个元素变成标题栏拖动区。Windows 上是系统的 `WindowControlArea::Drag`；
/// Linux 上挂鼠标事件（见模块说明）。
pub fn drag_region<E: InteractiveElement>(element: E) -> E {
    if cfg!(windows) {
        return element.window_control_area(WindowControlArea::Drag);
    }
    element
        .on_mouse_down(MouseButton::Left, |event, window, cx| {
            let me = window.window_handle().window_id();
            let can_zoom = can_zoom(window.window_controls(), window.is_resizable());
            let pending = cx.default_global::<PendingDrag>();
            pending.window = Some(me);
            if pending.gesture.press(event.click_count, can_zoom) == DragAction::Zoom {
                window.zoom_window();
            }
        })
        .on_mouse_down(MouseButton::Right, |event, window, _cx| {
            if window.window_controls().window_menu {
                window.show_window_menu(event.position);
            }
        })
        .on_mouse_move(|event, window, cx| {
            let me = window.window_handle().window_id();
            let pending = cx.default_global::<PendingDrag>();
            if pending.window != Some(me) {
                return;
            }
            if pending.gesture.motion(event.dragging()) == DragAction::StartMove {
                window.start_window_move();
            }
        })
        .on_mouse_up(MouseButton::Left, |_event, _window, cx| {
            cx.default_global::<PendingDrag>().gesture.release();
        })
}

// ---------------------------------------------------------------------------------------------
// 关闭

type CloseHandler = Rc<dyn Fn(&mut Window, &mut App) -> bool>;

#[derive(Default)]
struct CloseHandlers(HashMap<WindowId, CloseHandler>);

impl Global for CloseHandlers {}

/// 同 `window.on_window_should_close`，另外把回调登记下来供 [`request_close`] 使用。
/// 带自绘标题栏的窗口（Linux 上要靠它响应关闭按钮）都用这个注册；Windows 上行为和直接注册完全一样。
pub fn on_should_close(
    window: &mut Window,
    cx: &mut App,
    handler: impl Fn(&mut Window, &mut App) -> bool + 'static,
) {
    let handler: CloseHandler = Rc::new(handler);
    let live: Vec<WindowId> = cx.windows().iter().map(|w| w.window_id()).collect();
    let me = window.window_handle().window_id();
    let table = cx.default_global::<CloseHandlers>();
    // 关掉的窗口（回调里捕获着视图）不能一直留在表里。
    table.0.retain(|id, _| live.contains(id));
    table.0.insert(me, handler.clone());
    window.on_window_should_close(cx, move |window, cx| handler(window, cx));
}

/// 关闭按钮：走和系统关闭请求相同的路径——先问窗口注册的 `on_window_should_close`，返回 true 才关。
/// 没登记过的窗口直接关。
pub fn request_close(window: &mut Window, cx: &mut App) {
    let me = window.window_handle().window_id();
    let handler = cx
        .try_global::<CloseHandlers>()
        .and_then(|table| table.0.get(&me).cloned());
    let allowed = handler.is_none_or(|handler| handler(window, cx));
    if allowed {
        if cx.has_global::<CloseHandlers>() {
            cx.global_mut::<CloseHandlers>().0.remove(&me);
        }
        window.remove_window();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        APP_ID, Buttons, Chrome, DragAction, DragGesture, app_id_for, buttons_for, can_zoom,
        chrome_for, decorations_request,
    };
    use ::gpui::{Decorations, Tiling, WindowControls, WindowDecorations};

    /// 非测试部分的源码（测试模块里的字面量不算）。
    fn production(source: &str) -> &str {
        let marker = "#[cfg(test)]\nmod tests";
        source.split(marker).next().unwrap_or(source)
    }

    fn client() -> Decorations {
        Decorations::Client {
            tiling: Tiling::default(),
        }
    }

    fn controls(minimize: bool, maximize: bool) -> WindowControls {
        WindowControls {
            fullscreen: true,
            maximize,
            minimize,
            window_menu: true,
        }
    }

    #[test]
    fn windows_always_lets_the_system_hit_test_the_title_bar() {
        // Windows 的 GPUI 报 Server，但那里的标题栏按钮靠 WindowControlArea，必须是 Native。
        assert_eq!(chrome_for(false, Decorations::Server), Chrome::Native);
        assert_eq!(chrome_for(false, client()), Chrome::Native);
    }

    #[test]
    fn linux_draws_its_own_chrome_only_under_client_decorations() {
        assert_eq!(chrome_for(true, client()), Chrome::Client);
        // 合成器坚持服务端装饰：不画自己的按钮，不然就是两层标题栏。
        assert_eq!(chrome_for(true, Decorations::Server), Chrome::Server);
    }

    #[test]
    fn tiling_does_not_change_who_draws_the_chrome() {
        let tiled = Decorations::Client {
            tiling: Tiling::tiled(),
        };
        assert_eq!(chrome_for(true, tiled), Chrome::Client);
    }

    #[test]
    fn unsupported_minimize_or_maximize_hides_the_button_but_close_stays() {
        assert_eq!(
            buttons_for(controls(true, true), true, true, true),
            Buttons {
                minimize: true,
                maximize: true,
                close: true
            }
        );
        // 平铺式合成器两个都不支持。
        assert_eq!(
            buttons_for(controls(false, false), true, true, true),
            Buttons {
                minimize: false,
                maximize: false,
                close: true
            }
        );
        assert!(!buttons_for(controls(false, true), true, true, true).minimize);
        assert!(!buttons_for(controls(true, false), true, true, true).maximize);
    }

    #[test]
    fn a_window_that_cannot_resize_or_minimize_hides_those_buttons() {
        assert!(!buttons_for(controls(true, true), true, false, true).maximize);
        assert!(!buttons_for(controls(true, true), false, true, true).minimize);
    }

    #[test]
    fn the_update_window_never_gets_a_maximize_button() {
        let buttons = buttons_for(controls(true, true), true, true, false);
        assert!(buttons.minimize && !buttons.maximize && buttons.close);
    }

    #[test]
    fn double_click_zoom_needs_compositor_support_and_a_resizable_window() {
        assert!(can_zoom(controls(true, true), true));
        assert!(!can_zoom(controls(true, false), true));
        assert!(!can_zoom(controls(true, true), false));
    }

    #[test]
    fn only_linux_requests_client_decorations_and_an_app_id() {
        assert_eq!(decorations_request(true), Some(WindowDecorations::Client));
        assert_eq!(decorations_request(false), None);
        assert_eq!(app_id_for(true).as_deref(), Some(APP_ID));
        assert_eq!(app_id_for(false), None);
    }

    #[test]
    fn the_app_id_is_a_reverse_dns_name_the_desktop_file_can_match() {
        assert_eq!(APP_ID, "app.zeeterm.ZeeTerm");
        assert!(APP_ID.split('.').count() >= 3);
    }

    #[test]
    fn a_plain_click_never_starts_a_window_move() {
        let mut gesture = DragGesture::default();
        assert_eq!(gesture.press(1, true), DragAction::None);
        gesture.release();
        // 松开以后再怎么移动（没按键）都不动窗口。
        assert_eq!(gesture.motion(false), DragAction::None);
        assert_eq!(gesture.motion(true), DragAction::None);
    }

    #[test]
    fn moving_with_the_button_held_starts_the_move_once() {
        let mut gesture = DragGesture::default();
        assert_eq!(gesture.press(1, true), DragAction::None);
        assert_eq!(gesture.motion(true), DragAction::StartMove);
        assert_eq!(gesture.motion(true), DragAction::None, "只发起一次");
    }

    #[test]
    fn a_move_without_a_button_cancels_the_pending_drag() {
        let mut gesture = DragGesture::default();
        gesture.press(1, true);
        assert_eq!(gesture.motion(false), DragAction::None);
        assert_eq!(gesture.motion(true), DragAction::None);
    }

    #[test]
    fn a_selection_drag_that_started_elsewhere_does_not_move_the_window() {
        // 按下没发生在拖动区（没有 press），拖进标题栏也不能带着窗口走。
        let mut gesture = DragGesture::default();
        assert_eq!(gesture.motion(true), DragAction::None);
    }

    #[test]
    fn double_click_zooms_instead_of_moving() {
        let mut gesture = DragGesture::default();
        assert_eq!(gesture.press(2, true), DragAction::Zoom);
        assert_eq!(gesture.motion(true), DragAction::None);
        assert_eq!(DragGesture::default().press(2, false), DragAction::None);
    }

    #[test]
    fn title_bars_never_bind_window_control_area_outside_the_shared_helpers() {
        // 谁直接写 `.window_control_area(` 谁在 Linux 上就是死的：只有 `drag_region`（这里）和标题栏按钮的
        // Native 分支（`main_window/titlebar.rs`）可以，其余窗口一律走它们。
        let windows = [
            ("file_window.rs", include_str!("file_window.rs")),
            ("term_window.rs", include_str!("term_window.rs")),
            ("update/view.rs", include_str!("update/view.rs")),
            ("app_lock.rs", include_str!("app_lock.rs")),
        ];
        for (name, source) in windows {
            assert!(
                !production(source).contains(".window_control_area("),
                "{name} 直接用了 window_control_area：改用 window_frame::drag_region / titlebar::window_controls"
            );
        }
        let titlebar = production(include_str!("main_window/titlebar.rs"));
        assert_eq!(
            titlebar.matches(".window_control_area(").count(),
            1,
            "标题栏里只有按钮的 Native 分支可以用 window_control_area"
        );
        assert!(titlebar.contains("window_frame::Chrome::Native"));
    }

    #[test]
    fn windows_with_a_title_bar_register_close_through_the_shared_table() {
        // 关闭按钮要找到窗口注册的 should_close 回调，所以这几个窗口不能绕开 `on_should_close`。
        let windows = [
            ("main.rs", include_str!("main.rs")),
            ("file_window.rs", include_str!("file_window.rs")),
            ("term_window.rs", include_str!("term_window.rs")),
            ("update.rs", include_str!("update.rs")),
        ];
        for (name, source) in windows {
            let source = production(source);
            assert!(
                source.contains("window_frame::on_should_close("),
                "{name} 没有用 window_frame::on_should_close 注册关闭回调"
            );
            assert!(
                !source.contains(".on_window_should_close("),
                "{name} 直接注册了 on_window_should_close，Linux 的关闭按钮找不到它"
            );
        }
    }

    #[test]
    fn product_windows_ask_for_client_decorations_and_the_app_id() {
        for (name, source) in [
            ("main.rs", include_str!("main.rs")),
            ("file_window.rs", include_str!("file_window.rs")),
            ("term_window.rs", include_str!("term_window.rs")),
            ("update.rs", include_str!("update.rs")),
        ] {
            assert!(
                production(source).contains("window_frame::client_options()"),
                "{name} 的窗口选项没有走 window_frame::client_options()"
            );
        }
    }
}
