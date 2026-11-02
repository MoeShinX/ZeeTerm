//! 托盘原型：隐藏到托盘、菜单唤回、退出。
//!
//! 托盘图标在独立线程创建并持续泵 Win32 消息；菜单/图标事件通过 tray-icon 的
//! 全局通道到达监听线程，再经一个发送端可跨线程的队列回到 GPUI 主线程。
//! 不跨线程直接碰 Entity，也不把 `AsyncApp` 移出主线程。

use futures::StreamExt as _;
use gpui::{AnyWindowHandle, App};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, SW_HIDE, SW_SHOW, ShowWindowAsync, TranslateMessage,
};

/// 托盘图标是否已经建好。没建好时"关闭"不能只把窗口藏起来：
/// 那样没有任何入口能把窗口叫回来，进程和 SSH 会话只能靠任务管理器结束。
static READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 主窗口点关闭按钮时该做什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseAction {
    HideToTray,
    Quit,
}

pub fn close_action() -> CloseAction {
    close_action_for(READY.load(std::sync::atomic::Ordering::Acquire))
}

fn close_action_for(tray_ready: bool) -> CloseAction {
    if tray_ready {
        CloseAction::HideToTray
    } else {
        CloseAction::Quit
    }
}

const SHOW_ID: &str = "tray-show";
const UPDATE_ID: &str = "tray-update";
const QUIT_ID: &str = "tray-quit";

/// 托盘菜单里的一项。抽成纯数据，菜单有哪些项不用起窗口就能测（同 0.2.14 的 `TrayEntry`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayEntry {
    Show,
    /// 后台已经下好了更新：`version` 不带前缀 `v`。
    RestartInstall {
        version: String,
    },
    Separator,
    Quit,
}

impl TrayEntry {
    pub fn label(&self) -> Option<String> {
        match self {
            Self::Show => Some(format!("显示 {}", crate::product::NAME)),
            Self::RestartInstall { version } => Some(format!("重启安装 v{version}")),
            Self::Separator => None,
            Self::Quit => Some("退出".to_owned()),
        }
    }

    fn id(&self) -> Option<&'static str> {
        match self {
            Self::Show => Some(SHOW_ID),
            Self::RestartInstall { .. } => Some(UPDATE_ID),
            Self::Separator => None,
            Self::Quit => Some(QUIT_ID),
        }
    }
}

/// 托盘菜单该有哪些项。只有"自动更新开着、并且已经下载好一个更新"才多一项「重启安装」；
/// 试用版（`AUTO_UPDATE_ENABLED` 为 false）永远只有「显示」和「退出」。
pub fn menu_entries(auto_update: bool, ready_version: Option<&str>) -> Vec<TrayEntry> {
    let mut entries = vec![TrayEntry::Show];
    if let (true, Some(version)) = (auto_update, ready_version) {
        entries.push(TrayEntry::RestartInstall {
            version: version.trim_start_matches('v').to_owned(),
        });
    }
    entries.push(TrayEntry::Separator);
    entries.push(TrayEntry::Quit);
    entries
}

enum Command {
    Show,
    Quit,
}

/// 隐藏主窗口，但保留进程与后台任务。
pub fn hide(window: &mut gpui::Window) {
    crate::ime::apply("main", false);
    set_visible(window, false);
}

fn set_visible(window: &mut gpui::Window, visible: bool) {
    let Ok(handle) = window.window_handle() else {
        return;
    };
    if let RawWindowHandle::Win32(win32) = handle.as_raw() {
        let hwnd = HWND(win32.hwnd.get() as _);
        // SAFETY: hwnd 来自当前窗口的 raw handle，调用期间窗口仍然存活。
        unsafe {
            let _ = ShowWindowAsync(hwnd, if visible { SW_SHOW } else { SW_HIDE });
        }
        if visible {
            window.activate_window();
        }
    }
}

/// 启动托盘。必须在 GPUI 主线程调用；托盘本身在独立线程建并泵消息。
pub fn spawn(
    cx: &mut App,
    window: AnyWindowHandle,
    main: gpui::Entity<crate::main_window::MainWindow>,
) {
    let (tx, mut rx) = futures::channel::mpsc::unbounded::<Command>();

    // 主线程任务：接收托盘线程的命令，再在 GPUI 上下文里改窗口/退出。
    cx.spawn(async move |cx| {
        while let Some(command) = rx.next().await {
            match command {
                Command::Show => {
                    let _ = window.update(cx, |_, window, _| set_visible(window, true));
                }
                Command::Quit => {
                    // 有进行中的传输 / AI 回合：先把主窗口叫出来问一下，确认后由主窗口自己退出。
                    let main = main.clone();
                    let can_quit = window
                        .update(cx, move |_, window, cx| {
                            let can = main.update(cx, |this, cx| this.request_quit(cx));
                            if !can {
                                set_visible(window, true);
                            }
                            can
                        })
                        .unwrap_or(true);
                    if can_quit {
                        cx.update(|app| app.quit());
                    }
                }
            }
        }
    })
    .detach();

    let menu_tx = tx.clone();
    let click_tx = tx;

    std::thread::Builder::new()
        .name("zssh-tray".to_owned())
        .spawn(move || {
            let menu = Menu::new();
            for entry in menu_entries(crate::product::AUTO_UPDATE_ENABLED, None) {
                let appended = match (entry.id(), entry.label()) {
                    (Some(id), Some(label)) => {
                        menu.append(&MenuItem::with_id(id, label, true, None))
                    }
                    _ => menu.append(&PredefinedMenuItem::separator()),
                };
                if appended.is_err() {
                    tracing::warn!("tray menu could not be built");
                    return;
                }
            }

            let icon = match Icon::from_rgba(solid_icon(), 32, 32) {
                Ok(icon) => icon,
                Err(err) => {
                    tracing::warn!("tray icon could not be created: {err}");
                    return;
                }
            };

            let tray = match TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_tooltip(crate::product::NAME)
                .with_icon(icon)
                .build()
            {
                Ok(tray) => tray,
                Err(err) => {
                    tracing::warn!("tray icon could not be created: {err}");
                    return;
                }
            };
            READY.store(true, std::sync::atomic::Ordering::Release);

            // 菜单和左键事件走独立线程，避免阻塞托盘线程的 Win32 消息泵。
            std::thread::Builder::new()
                .name("zssh-tray-menu".to_owned())
                .spawn(move || {
                    while let Ok(event) = MenuEvent::receiver().recv() {
                        match event.id.0.as_str() {
                            SHOW_ID => {
                                let _ = menu_tx.unbounded_send(Command::Show);
                            }
                            QUIT_ID => {
                                let _ = menu_tx.unbounded_send(Command::Quit);
                                return;
                            }
                            _ => {}
                        }
                    }
                })
                .ok();

            std::thread::Builder::new()
                .name("zssh-tray-click".to_owned())
                .spawn(move || {
                    while let Ok(event) = TrayIconEvent::receiver().recv() {
                        if let TrayIconEvent::Click {
                            button: tray_icon::MouseButton::Left,
                            button_state: tray_icon::MouseButtonState::Up,
                            ..
                        } = event
                        {
                            let _ = click_tx.unbounded_send(Command::Show);
                        }
                    }
                })
                .ok();

            // SAFETY: 当前线程拥有托盘窗口，标准的 GetMessage/DispatchMessage 循环。
            unsafe {
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            READY.store(false, std::sync::atomic::Ordering::Release);
            drop(tray);
        })
        .ok();
}

/// 32×32 纯色图标，先证明托盘通路；正式图标在 P3/G07 换。
fn solid_icon() -> Vec<u8> {
    let mut rgba = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32u8 {
        for x in 0..32u8 {
            let edge = x < 2 || y < 2 || x > 29 || y > 29;
            if edge {
                rgba.extend_from_slice(&[0x0f, 0x17, 0x2a, 0xff]);
            } else {
                rgba.extend_from_slice(&[0x3b, 0x82, 0xf6, 0xff]);
            }
        }
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_hides_only_when_the_tray_exists() {
        assert_eq!(close_action_for(true), CloseAction::HideToTray);
        assert_eq!(close_action_for(false), CloseAction::Quit);
    }

    fn labels(entries: &[TrayEntry]) -> Vec<Option<String>> {
        entries.iter().map(TrayEntry::label).collect()
    }

    #[test]
    fn the_menu_is_show_and_quit_in_the_trial_build() {
        // 试用版没有自动更新：就算（假设）有已下载的更新，菜单也不多出「重启安装」。
        let entries = menu_entries(crate::product::AUTO_UPDATE_ENABLED, Some("9.9.9"));
        assert_eq!(
            labels(&entries),
            vec![
                Some("显示 ZeeTerm".to_owned()),
                None,
                Some("退出".to_owned())
            ]
        );
    }

    #[test]
    fn restart_install_appears_only_with_auto_update_and_a_ready_package() {
        let ready = menu_entries(true, Some("v1.2.3"));
        assert_eq!(
            labels(&ready),
            vec![
                Some("显示 ZeeTerm".to_owned()),
                Some("重启安装 v1.2.3".to_owned()),
                None,
                Some("退出".to_owned())
            ]
        );
        assert_eq!(menu_entries(true, None).len(), 3);
        assert_eq!(menu_entries(false, Some("1.2.3")).len(), 3);
    }
}
