//! Linux 托盘占位：首版不带托盘，关闭主窗口就退出。
//!
//! 真正的托盘要接 StatusNotifierItem（KSNI / libappindicator），属于 L4 打包阶段；
//! 这里保持和 Windows 模块相同的函数形状，界面层不用分叉。

use gpui::{AnyWindowHandle, App, Entity};

/// 主窗口点关闭按钮时该做什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseAction {
    /// 和 Windows 模块保持同一套分支；Linux 首版没有托盘，不会返回它。
    #[allow(dead_code)]
    HideToTray,
    Quit,
}

/// 没有托盘，就没有能把窗口叫回来的入口：一律退出。
pub fn close_action() -> CloseAction {
    CloseAction::Quit
}

/// 没有托盘菜单，保留接口给界面层调用。
pub fn set_lock_menu(_enabled: bool) {}

/// 没有托盘可收，隐藏窗口等于让用户失去入口；这里什么都不做。
pub fn hide(_window: &mut gpui::Window) {}

/// 把窗口放到最前面（GNOME/Wayland 下由合成器决定，激活请求可能被忽略）。
pub fn bring_to_front(window: &mut gpui::Window) {
    window.activate_window();
}

/// 不启动托盘线程。
pub fn spawn(
    _cx: &mut App,
    _window: AnyWindowHandle,
    _main: Entity<crate::main_window::MainWindow>,
) {
}
