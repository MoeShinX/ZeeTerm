//! macOS：还没有菜单栏图标（NSStatusItem）。和 Windows 的 `tray.rs`、Linux 的 `tray_linux.rs` 同一套对外接口，
//! 界面层不用分叉；行为就是 Linux "托盘不可用"时的那一套：
//! - [`available`] 永远是假；
//! - [`close_action`]：关闭主窗口一律退出（没有托盘就没有入口把藏起来的窗口叫回来）；
//! - [`hide`] 什么都不做（窗口留在屏幕上）；
//! - [`bring_to_front`]：再次启动时把主窗口叫到前面（单实例的激活通道用它）。

use gpui::{AnyWindowHandle, App, Entity};

/// 主窗口点关闭按钮时该做什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseAction {
    HideToTray,
    Quit,
}

/// macOS 版没有菜单栏图标。
pub fn available() -> bool {
    false
}

pub fn close_action() -> CloseAction {
    close_action_for(
        available(),
        crate::app_prefs::AppPrefs::load_default_db().close_action,
    )
}

/// 托盘不可用时不管设置怎么选都必须退出（同 Linux）。
fn close_action_for(tray_available: bool, choice: crate::app_prefs::CloseChoice) -> CloseAction {
    if tray_available && choice == crate::app_prefs::CloseChoice::Tray {
        CloseAction::HideToTray
    } else {
        CloseAction::Quit
    }
}

/// 没有托盘菜单，也就没有「锁定」项要同步。
pub fn set_lock_menu(_enabled: bool) {}

/// 没有托盘可收：窗口留在屏幕上。
pub fn hide(_window: &mut gpui::Window) {}

/// 把窗口放到最前面。
pub fn bring_to_front(window: &mut gpui::Window) {
    window.activate_window();
}

/// 没有托盘要建。
pub fn spawn(
    _cx: &mut App,
    _window: AnyWindowHandle,
    _main: Entity<crate::main_window::MainWindow>,
) {
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_prefs::CloseChoice;

    #[test]
    fn without_a_menu_bar_icon_closing_always_quits() {
        assert!(!available());
        assert_eq!(
            close_action_for(false, CloseChoice::Tray),
            CloseAction::Quit
        );
        assert_eq!(
            close_action_for(false, CloseChoice::Quit),
            CloseAction::Quit
        );
    }
}
