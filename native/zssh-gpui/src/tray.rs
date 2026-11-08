//! 托盘原型：隐藏到托盘、菜单唤回、退出。
//!
//! 托盘图标在独立线程创建并持续泵 Win32 消息；菜单/图标事件通过 tray-icon 的
//! 全局通道到达监听线程，再经一个发送端可跨线程的队列回到 GPUI 主线程。
//! 不跨线程直接碰 Entity，也不把 `AsyncApp` 移出主线程。

use crate::tray_menu::{CHECK_ID, LOCK_ID, QUIT_ID, SHOW_ID, TrayEntry, UPDATE_ID, menu_entries};
use futures::StreamExt as _;
use gpui::{AnyWindowHandle, App};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, DispatchMessageW, GetMessageW, GetSystemMetrics, HWND_NOTOPMOST,
    HWND_TOPMOST, MSG, SM_CXSMICON, SM_CYSMICON, SW_HIDE, SW_SHOW, SWP_NOMOVE, SWP_NOSIZE,
    SetForegroundWindow, SetTimer, SetWindowPos, ShowWindowAsync, TranslateMessage, WM_TIMER,
};

/// 托盘图标是否已经建好。没建好时"关闭"不能只把窗口藏起来：
/// 那样没有任何入口能把窗口叫回来，进程和 SSH 会话只能靠任务管理器结束。
static READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// build.rs 把 assets/brand/zeeterm.ico 编成资源 ID 1；GPUI 窗口类图标用的是同一个 ID。
const APP_ICON_RESOURCE_ID: u16 = 1;

/// 资源加载失败时的后备图标（32px，透明底）。
const FALLBACK_PNG: &[u8] = include_bytes!("../assets/brand/zeeterm-logo-32.png");

/// 主窗口点关闭按钮时该做什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseAction {
    HideToTray,
    Quit,
}

pub fn close_action() -> CloseAction {
    // 每次点 × 读一次设置：用户在设置页改了「关闭主窗口时」马上生效，不用重启。
    close_action_for(
        READY.load(std::sync::atomic::Ordering::Acquire),
        crate::app_prefs::AppPrefs::load_default_db().close_action,
    )
}

/// 托盘没建起来时没有入口能叫回窗口，不管设置怎么选都必须退出；托盘在，才按设置走。
fn close_action_for(tray_ready: bool, choice: crate::app_prefs::CloseChoice) -> CloseAction {
    if tray_ready && choice == crate::app_prefs::CloseChoice::Tray {
        CloseAction::HideToTray
    } else {
        CloseAction::Quit
    }
}

/// 锁屏启用了没有（V2-U26）：主窗口每次重读设置时写，托盘线程的定时器读，据此显示 / 隐藏菜单里的「锁定」。
static LOCK_AVAILABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 主窗口告诉托盘：锁屏现在启用了没有（没启用时菜单里没有「锁定」）。
pub fn set_lock_menu(enabled: bool) {
    LOCK_AVAILABLE.store(enabled, std::sync::atomic::Ordering::Release);
}

enum Command {
    Show,
    /// 托盘「锁定」：立即锁定（没启用锁屏时主窗口什么都不做）。
    Lock,
    CheckUpdate,
    /// 托盘「重启安装 vX」：把排队的更新计划补上"装完重启"，然后和「退出」走同一条退出路径。
    RestartInstall,
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

/// 把一个新开的窗口真正放到最前面。
///
/// 从托盘菜单打开的窗口光靠 `activate_window` 常常停在后面：菜单命令要先从托盘线程转到
/// GPUI 主线程，窗口建出来时前台已经不在本进程手里，Windows 的前台锁会把
/// `SetForegroundWindow` 降级成任务栏闪烁。先临时置顶再取消置顶，窗口至少会盖在其他
/// 窗口上面；再补一次 `SetForegroundWindow`，系统允许时顺便拿到键盘焦点。
pub fn bring_to_front(window: &mut gpui::Window) {
    window.activate_window();
    let Ok(handle) = window.window_handle() else {
        return;
    };
    if let RawWindowHandle::Win32(win32) = handle.as_raw() {
        let hwnd = HWND(win32.hwnd.get() as _);
        let flags = SWP_NOMOVE | SWP_NOSIZE;
        // SAFETY: hwnd 来自当前窗口的 raw handle，调用期间窗口仍然存活。
        unsafe {
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, flags);
            let _ = SetWindowPos(hwnd, Some(HWND_NOTOPMOST), 0, 0, 0, 0, flags);
            let _ = BringWindowToTop(hwnd);
            let _ = SetForegroundWindow(hwnd);
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
                Command::Lock => {
                    let main = main.clone();
                    let _ = window.update(cx, move |_, _window, cx| {
                        main.update(cx, |this, cx| this.lock_now(cx));
                    });
                }
                Command::CheckUpdate => {
                    // 和关于页同一个入口：弹出更新窗口。
                    let main = main.clone();
                    let _ = window.update(cx, move |_, window, cx| {
                        main.update(cx, |this, cx| this.check_for_updates(window, cx));
                    });
                }
                Command::RestartInstall | Command::Quit => {
                    if matches!(command, Command::RestartInstall) {
                        crate::update::arm_pending_restart();
                    }
                    // 有进行中的传输 / AI 回合：先把主窗口叫出来问一下，确认后由主窗口自己退出。
                    let asking = main.clone();
                    let can_quit = window
                        .update(cx, move |_, window, cx| {
                            let can = asking.update(cx, |this, cx| this.request_quit(cx));
                            if !can {
                                set_visible(window, true);
                            }
                            can
                        })
                        .unwrap_or(true);
                    if can_quit {
                        // 和菜单 / 确认框的退出同一条收尾（存状态、收会话、收全局智能体），
                        // 窗口句柄失效时才退而直接退出。
                        let main = main.clone();
                        let finished = window
                            .update(cx, move |_, window, cx| {
                                main.update(cx, |this, cx| this.do_quit(window, cx));
                            })
                            .is_ok();
                        if !finished {
                            cx.update(|app| app.quit());
                        }
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
            for entry in menu_entries(
                crate::product::AUTO_UPDATE_ENABLED,
                None,
                LOCK_AVAILABLE.load(std::sync::atomic::Ordering::Acquire),
            ) {
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

            let icon = match load_icon() {
                Ok(icon) => icon,
                Err(err) => {
                    tracing::warn!("tray icon could not be created: {err}");
                    return;
                }
            };

            let tray = match TrayIconBuilder::new()
                .with_menu(Box::new(menu.clone()))
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
                            LOCK_ID => {
                                let _ = menu_tx.unbounded_send(Command::Lock);
                            }
                            CHECK_ID => {
                                let _ = menu_tx.unbounded_send(Command::CheckUpdate);
                            }
                            UPDATE_ID => {
                                let _ = menu_tx.unbounded_send(Command::RestartInstall);
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

            // 线程定时器：每秒看一眼有没有排队等安装的更新，有就在菜单里加一项「重启安装 vX」。
            // 菜单对象只能在创建它的这个线程里改，所以放在消息循环里做。
            let mut restart_item = RestartItem {
                // 启动时菜单里已经按当时的状态放好了「锁定」（上面的 `menu_entries`）。
                lock_on: LOCK_AVAILABLE.load(std::sync::atomic::Ordering::Acquire),
                ..RestartItem::default()
            };
            restart_item.lock_item = menu.items().iter().find_map(|kind| match kind {
                tray_icon::menu::MenuItemKind::MenuItem(item) if item.id().0 == LOCK_ID => {
                    Some(item.clone())
                }
                _ => None,
            });
            // SAFETY: 线程定时器没有窗口句柄，WM_TIMER 直接进本线程的消息队列。
            unsafe {
                SetTimer(None, 0, RESTART_POLL_MS, None);
            }
            // SAFETY: 当前线程拥有托盘窗口，标准的 GetMessage/DispatchMessage 循环。
            unsafe {
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    if msg.message == WM_TIMER && msg.hwnd.is_invalid() {
                        let lock_now = LOCK_AVAILABLE.load(std::sync::atomic::Ordering::Acquire);
                        sync_dynamic_items(
                            &menu,
                            &mut restart_item,
                            crate::update::pending_install_version(),
                            lock_now,
                        );
                        continue;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            READY.store(false, std::sync::atomic::Ordering::Release);
            drop(tray);
        })
        .ok();
}

/// 查「有没有排队等安装的更新」的间隔（毫秒）。
const RESTART_POLL_MS: u32 = 1000;

/// 菜单里会随状态出现 / 消失的两项当前的样子：「重启安装」（对应哪个版本）和「锁定」（在不在）。
#[derive(Default)]
struct RestartItem {
    item: Option<MenuItem>,
    version: Option<String>,
    lock_item: Option<MenuItem>,
    lock_on: bool,
}

/// 让菜单里的「重启安装」和排队的更新一致：版本没变不动，变了就先删旧的再按 [`menu_entries`] 的顺序
///（显示、检查更新、重启安装、分隔线、退出）插在第 3 位。不管「锁定」项（保持它现在的样子）。
#[cfg(test)]
fn sync_restart_item(menu: &Menu, state: &mut RestartItem, ready: Option<String>) {
    let lock_on = state.lock_on;
    sync_dynamic_items(menu, state, ready, lock_on);
}

/// 同 [`sync_restart_item`]，同时让「锁定」项和锁屏是否启用一致。两项都要按 [`menu_entries`] 的次序插，
/// 所以任何一个变了就把两个动态项都删掉重插。
fn sync_dynamic_items(menu: &Menu, state: &mut RestartItem, ready: Option<String>, lock_on: bool) {
    if state.version == ready && state.lock_on == lock_on {
        return;
    }
    if let Some(old) = state.item.take() {
        let _ = menu.remove(&old);
    }
    if let Some(old) = state.lock_item.take() {
        let _ = menu.remove(&old);
    }
    state.version = ready.clone();
    state.lock_on = lock_on;
    let entries = menu_entries(
        crate::product::AUTO_UPDATE_ENABLED,
        ready.as_deref(),
        lock_on,
    );
    for (index, entry) in entries.iter().enumerate() {
        let slot = match entry {
            TrayEntry::RestartInstall { .. } => &mut state.item,
            TrayEntry::Lock => &mut state.lock_item,
            _ => continue,
        };
        let (Some(id), Some(label)) = (entry.id(), entry.label()) else {
            continue;
        };
        let item = MenuItem::with_id(id, label, true, None);
        if menu.insert(&item, index).is_ok() {
            *slot = Some(item);
        }
    }
}

/// 托盘图标：优先用 exe 里嵌入的图标资源（`build.rs` 编进去的 ID 1，系统会按托盘需要的尺寸
/// 从多尺寸 .ico 里挑最接近的，高 DPI 下不会把 16px 拉糊）；资源不在时（例如从没带资源的
/// 测试 / 自定义构建里启动）退回内置的 32px PNG，托盘总有个像样的图标。
fn load_icon() -> Result<Icon, String> {
    // SAFETY: GetSystemMetrics 只读系统参数，没有前置条件。
    let (width, height) = unsafe { (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)) };
    let size = (width > 0 && height > 0).then_some((width as u32, height as u32));
    match Icon::from_resource(APP_ICON_RESOURCE_ID, size) {
        Ok(icon) => Ok(icon),
        Err(err) => {
            tracing::warn!("tray icon resource not found, using the built-in PNG: {err}");
            fallback_icon()
        }
    }
}

/// 内置 PNG 解码成 RGBA。独立成函数是为了不起托盘也能测。
fn fallback_icon() -> Result<Icon, String> {
    let (rgba, width, height) = decode_fallback_png()?;
    Icon::from_rgba(rgba, width, height).map_err(|err| err.to_string())
}

fn decode_fallback_png() -> Result<(Vec<u8>, u32, u32), String> {
    let image = image::load_from_memory_with_format(FALLBACK_PNG, image::ImageFormat::Png)
        .map_err(|err| err.to_string())?
        .into_rgba8();
    let (width, height) = image.dimensions();
    Ok((image.into_raw(), width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fallback_icon_is_the_32px_logo_with_a_transparent_background() {
        let (rgba, width, height) = decode_fallback_png().expect("内置 PNG 应能解码");
        assert_eq!((width, height), (32, 32));
        assert_eq!(rgba.len(), 32 * 32 * 4);
        // 透明底（左上角）加不透明的 Logo 本体（中心），说明不是以前那块纯色方块。
        assert_eq!(rgba[3], 0, "左上角应该透明");
        let center = (16 * 32 + 16) * 4;
        assert_eq!(rgba[center + 3], 255, "中心应该不透明");
        assert!(fallback_icon().is_ok());
    }

    #[test]
    fn close_hides_only_when_the_tray_exists() {
        use crate::app_prefs::CloseChoice::{Quit, Tray};
        assert_eq!(close_action_for(true, Tray), CloseAction::HideToTray);
        assert_eq!(close_action_for(false, Tray), CloseAction::Quit);
        // 设置选了「退出程序」：托盘在也直接退出。
        assert_eq!(close_action_for(true, Quit), CloseAction::Quit);
        assert_eq!(close_action_for(false, Quit), CloseAction::Quit);
    }

    /// 菜单里现在每一项的文字（分隔线记为空串）。
    fn menu_texts(menu: &Menu) -> Vec<String> {
        use tray_icon::menu::MenuItemKind;
        menu.items()
            .iter()
            .map(|item| match item {
                MenuItemKind::MenuItem(item) => item.text(),
                _ => String::new(),
            })
            .collect()
    }

    #[test]
    fn the_live_menu_gains_and_loses_the_restart_item_with_the_queued_update() {
        let menu = Menu::new();
        for entry in menu_entries(crate::product::AUTO_UPDATE_ENABLED, None, false) {
            let appended = match (entry.id(), entry.label()) {
                (Some(id), Some(label)) => menu.append(&MenuItem::with_id(id, label, true, None)),
                _ => menu.append(&PredefinedMenuItem::separator()),
            };
            appended.expect("加菜单项");
        }
        let mut state = RestartItem::default();
        let idle = menu_texts(&menu);
        assert_eq!(idle, ["显示 ZeeTerm", "检查更新", "", "退出"]);

        // 有排队的更新：插在「检查更新」后面、分隔线前面。
        sync_restart_item(&menu, &mut state, Some("1.2.3".to_owned()));
        assert_eq!(
            menu_texts(&menu),
            ["显示 ZeeTerm", "检查更新", "重启安装 v1.2.3", "", "退出"]
        );
        // 同一个版本再同步：不重复加。
        sync_restart_item(&menu, &mut state, Some("1.2.3".to_owned()));
        assert_eq!(menu_texts(&menu).len(), 5);
        // 版本变了：换掉旧的那一项，不是再加一项。
        sync_restart_item(&menu, &mut state, Some("1.3.0".to_owned()));
        assert_eq!(
            menu_texts(&menu),
            ["显示 ZeeTerm", "检查更新", "重启安装 v1.3.0", "", "退出"]
        );
        // 排队的更新没了（比如已经启动了安装器）：菜单回到原样。
        sync_restart_item(&menu, &mut state, None);
        assert_eq!(menu_texts(&menu), idle);
    }

    #[test]
    fn the_live_menu_gains_and_loses_the_lock_item_with_the_setting() {
        let menu = Menu::new();
        for entry in menu_entries(crate::product::AUTO_UPDATE_ENABLED, None, false) {
            let appended = match (entry.id(), entry.label()) {
                (Some(id), Some(label)) => menu.append(&MenuItem::with_id(id, label, true, None)),
                _ => menu.append(&PredefinedMenuItem::separator()),
            };
            appended.expect("加菜单项");
        }
        let mut state = RestartItem::default();
        let idle = menu_texts(&menu);
        assert_eq!(idle, ["显示 ZeeTerm", "检查更新", "", "退出"]);

        sync_dynamic_items(&menu, &mut state, None, true);
        assert_eq!(
            menu_texts(&menu),
            ["显示 ZeeTerm", "锁定", "检查更新", "", "退出"]
        );
        // 同样的状态再同步：不重复加。
        sync_dynamic_items(&menu, &mut state, None, true);
        assert_eq!(menu_texts(&menu).len(), 5);
        // 又来了排队的更新：两个动态项都在，次序仍按 `menu_entries`。
        sync_dynamic_items(&menu, &mut state, Some("1.2.3".to_owned()), true);
        assert_eq!(
            menu_texts(&menu),
            [
                "显示 ZeeTerm",
                "锁定",
                "检查更新",
                "重启安装 v1.2.3",
                "",
                "退出"
            ]
        );
        // 关掉锁屏：「锁定」没了，「重启安装」留着。
        sync_dynamic_items(&menu, &mut state, Some("1.2.3".to_owned()), false);
        assert_eq!(
            menu_texts(&menu),
            ["显示 ZeeTerm", "检查更新", "重启安装 v1.2.3", "", "退出"]
        );
        sync_dynamic_items(&menu, &mut state, None, false);
        assert_eq!(menu_texts(&menu), idle);
    }

    #[test]
    fn the_main_window_tells_the_tray_whether_the_lock_is_on() {
        set_lock_menu(true);
        assert!(LOCK_AVAILABLE.load(std::sync::atomic::Ordering::Acquire));
        set_lock_menu(false);
        assert!(!LOCK_AVAILABLE.load(std::sync::atomic::Ordering::Acquire));
    }
}
