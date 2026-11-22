//! Linux 托盘：StatusNotifierItem（`ksni`，纯 Rust 的 D-Bus 实现，不引 GTK）。
//!
//! 和 Windows 的 `tray.rs` 同一套对外接口，界面层不用分叉；菜单项（显示、锁定、检查更新、退出）
//! 来自两边共用的 `tray_menu`，次序和文字永远一致。Linux 没有"退出后安装"的更新队列，所以没有「重启安装」项。
//!
//! **托盘不一定存在**：原版 GNOME 没有 StatusNotifierWatcher（要装 AppIndicator 扩展），
//! 这时关闭按钮不能把窗口藏起来——藏起来就没有任何入口把它找回来。所以"可用"的定义是：
//! 托盘服务已建好 **并且** StatusNotifierWatcher 在线（[`available`]）。
//! - [`close_action`]：不可用时一律退出，不管设置里选的是什么；
//! - [`hide`]：不可用时什么都不做（窗口留在屏幕上）；
//! - 宿主是否在线**一开始是未知的**，按"没有托盘"处理：ksni 只在宿主**下线 / 重新上线**时回调
//!   `watcher_offline` / `watcher_online`，启动时宿主本来就在是不回调的；所以以 `spawn()` 返回为界——
//!   它返回前如果没有收到 `watcher_offline`（初次登记失败会在返回前同步回调），才确认在线。
//!   在这之前"启动时隐藏"和"关窗口"都不会把窗口藏起来；
//! - 宿主后来出现 / 消失：ksni 在 `org.kde.StatusNotifierWatcher` 上线 / 下线时回调
//!   `watcher_online` / `watcher_offline`，这里据此更新可用状态并通知界面（设置页的提示跟着变）；
//!   窗口正被收在托盘里时宿主消失了，立刻把窗口叫出来，免得用户找不回来；
//! - 左键点图标（[`toggle_action`]）：窗口被收在托盘里、不可见（最小化）或不在前台，一律叫到前台，
//!   只有"可见且在前台"才收起——否则用户自己最小化的窗口，第一次点托盘会"再收一次"（还可能顺带锁屏）。
//!
//! 收起主窗口 = 最小化（GPUI 没有"隐藏窗口"，Wayland 本身也没有）。从托盘点回来用 `activate_window`；
//! Wayland 下激活请求可能被合成器当作抢焦点而拒绝（GNOME 会弹"ZeeTerm 已就绪"通知），
//! 这是协议层面的限制，窗口仍在概览 / 任务切换里。

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures::StreamExt as _;
use gpui::{AnyWindowHandle, App, Entity};
use ksni::blocking::TrayMethods as _;

use crate::tray_menu::{CHECK_ID, LOCK_ID, QUIT_ID, SHOW_ID, TrayEntry, menu_entries};

/// 主窗口点关闭按钮时该做什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseAction {
    HideToTray,
    Quit,
}

/// 托盘现在能不能用：服务已建好，并且 StatusNotifierWatcher 在线。
pub fn available() -> bool {
    state().available()
}

pub fn close_action() -> CloseAction {
    // 每次点 × 读一次设置：用户在设置页改了「关闭主窗口时」马上生效，不用重启。
    close_action_for(
        available(),
        crate::app_prefs::AppPrefs::load_default_db().close_action,
    )
}

/// 托盘不可用时没有入口能叫回窗口，不管设置怎么选都必须退出；托盘在，才按设置走。
fn close_action_for(tray_available: bool, choice: crate::app_prefs::CloseChoice) -> CloseAction {
    if tray_available && choice == crate::app_prefs::CloseChoice::Tray {
        CloseAction::HideToTray
    } else {
        CloseAction::Quit
    }
}

/// 主窗口告诉托盘：锁屏现在启用了没有（没启用时菜单里没有「锁定」）。
pub fn set_lock_menu(enabled: bool) {
    state().lock_menu.store(enabled, Ordering::Release);
}

/// 收起主窗口，保留进程与后台任务。托盘不可用时什么都不做：窗口留在屏幕上。
pub fn hide(window: &mut gpui::Window) {
    if !available() {
        return;
    }
    crate::ime::apply("main", false);
    state().hidden.store(true, Ordering::Release);
    window.minimize_window();
}

/// 左键点托盘图标之后该做什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToggleAction {
    BringToFront,
    Hide,
}

/// 窗口收在托盘里（`hidden`）、不可见（GPUI 的 `is_visible()` 在最小化时为假）或不在前台（`active` 为假）时，
/// 都叫到前台；只有"可见且在前台"才收起。GPUI 没有单独的"是否最小化"接口，这两项合起来够用：
/// 用户自己点标题栏最小化的窗口，`hidden` 仍是假，但它不可见也不在前台。
/// 代价：窗口可见但被别的窗口盖住时点托盘，也是"叫到前面"而不是收起，和多数 SNI 应用一致。
fn toggle_action(hidden: bool, visible: bool, active: bool) -> ToggleAction {
    if hidden || !visible || !active {
        ToggleAction::BringToFront
    } else {
        ToggleAction::Hide
    }
}

/// 把窗口放到最前面（GNOME/Wayland 下由合成器决定，激活请求可能被忽略）。
pub fn bring_to_front(window: &mut gpui::Window) {
    state().hidden.store(false, Ordering::Release);
    window.activate_window();
}

// ---------------------------------------------------------------------------------------------
// 状态

/// `org.kde.StatusNotifierWatcher` 的在线状态。三态：启动时还没确认的"未知"不能当成在线。
struct Watcher(AtomicU8);

impl Watcher {
    const UNKNOWN: u8 = 0;
    const UP: u8 = 1;
    const DOWN: u8 = 2;

    const fn new() -> Self {
        Self(AtomicU8::new(Self::UNKNOWN))
    }

    fn is_up(&self) -> bool {
        self.0.load(Ordering::Acquire) == Self::UP
    }

    /// 已经明确收到过"下线"（只给测试等它用；"未知"不算）。
    #[cfg(test)]
    fn is_down(&self) -> bool {
        self.0.load(Ordering::Acquire) == Self::DOWN
    }

    /// ksni 的回调：宿主上线了。
    fn set_up(&self) {
        self.0.store(Self::UP, Ordering::Release);
    }

    /// ksni 的回调：宿主下线了。
    fn set_down(&self) {
        self.0.store(Self::DOWN, Ordering::Release);
    }

    /// 重新建服务之前：之前的结论作废。
    fn reset(&self) {
        self.0.store(Self::UNKNOWN, Ordering::Release);
    }

    /// `spawn()` 返回后：期间没有收到过任何回调（还是"未知"）就确认在线；已经收到下线 / 上线回调的不动。
    /// 用比较交换而不是先读后写，避免把紧跟着到的 `watcher_offline` 覆盖掉。
    fn confirm_up_if_unknown(&self) {
        let _ =
            self.0
                .compare_exchange(Self::UNKNOWN, Self::UP, Ordering::AcqRel, Ordering::Acquire);
    }
}

/// 托盘相关的进程级状态。全是原子量：ksni 的回调在它自己的线程里，界面线程随时要读。
struct State {
    /// 托盘服务已建好（`ksni` 的 `spawn` 成功）。
    spawned: AtomicBool,
    /// `org.kde.StatusNotifierWatcher` 在线（没有它，图标没地方显示）。
    watcher: Watcher,
    /// 主窗口是被我们收进托盘的（最小化了）。
    hidden: AtomicBool,
    /// 锁屏启用了没有，决定菜单里有没有「锁定」。
    lock_menu: AtomicBool,
}

impl State {
    const fn new() -> Self {
        Self {
            spawned: AtomicBool::new(false),
            watcher: Watcher::new(),
            hidden: AtomicBool::new(false),
            lock_menu: AtomicBool::new(false),
        }
    }

    fn available(&self) -> bool {
        self.spawned.load(Ordering::Acquire) && self.watcher.is_up()
    }
}

fn state() -> &'static Arc<State> {
    static STATE: OnceLock<Arc<State>> = OnceLock::new();
    STATE.get_or_init(|| Arc::new(State::new()))
}

/// 托盘线程发给界面线程的命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    /// 菜单「显示」。
    Show,
    /// 左键点图标：收起 / 显示切换。
    Toggle,
    Lock,
    CheckUpdate,
    Quit,
    /// 托盘可用状态变了（宿主出现 / 消失）。
    AvailabilityChanged,
}

type Sender = futures::channel::mpsc::UnboundedSender<Command>;

// ---------------------------------------------------------------------------------------------
// StatusNotifierItem

struct ZeeTray {
    state: Arc<State>,
    tx: Sender,
    lock_on: bool,
}

impl ZeeTray {
    fn send(&self, command: Command) {
        let _ = self.tx.unbounded_send(command);
    }
}

impl ksni::Tray for ZeeTray {
    fn id(&self) -> String {
        crate::window_frame::APP_ID.to_owned()
    }

    fn title(&self) -> String {
        crate::product::NAME.to_owned()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        icons()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: crate::product::NAME.to_owned(),
            ..Default::default()
        }
    }

    /// 左键点图标。
    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Command::Toggle);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        build_menu(&menu_entries(
            crate::product::AUTO_UPDATE_ENABLED,
            None,
            self.lock_on,
        ))
    }

    fn watcher_online(&self) {
        self.state.watcher.set_up();
        self.send(Command::AvailabilityChanged);
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        tracing::info!(
            ?reason,
            "StatusNotifierWatcher is offline, the tray icon is not shown"
        );
        self.state.watcher.set_down();
        self.send(Command::AvailabilityChanged);
        // 服务继续留着：宿主回来时 ksni 会自动重新登记
        true
    }
}

/// 菜单项 → 命令。
fn command_for(id: &str) -> Option<Command> {
    match id {
        SHOW_ID => Some(Command::Show),
        LOCK_ID => Some(Command::Lock),
        CHECK_ID => Some(Command::CheckUpdate),
        QUIT_ID => Some(Command::Quit),
        _ => None,
    }
}

/// 把共用的菜单项翻成 ksni 的菜单。
fn build_menu(entries: &[TrayEntry]) -> Vec<ksni::MenuItem<ZeeTray>> {
    entries
        .iter()
        .map(|entry| match (entry.id(), entry.label()) {
            (Some(id), Some(label)) => {
                let command = command_for(id);
                ksni::MenuItem::Standard(ksni::menu::StandardItem {
                    // D-Bus 菜单里单个下划线是快捷键标记，字面的下划线要写两个
                    label: label.replace('_', "__"),
                    activate: Box::new(move |tray: &mut ZeeTray| {
                        if let Some(command) = command {
                            tray.send(command);
                        }
                    }),
                    ..Default::default()
                })
            }
            _ => ksni::MenuItem::Separator,
        })
        .collect()
}

const ICON_64: &[u8] = include_bytes!("../assets/brand/zeeterm-logo-64.png");
const ICON_32: &[u8] = include_bytes!("../assets/brand/zeeterm-logo-32.png");

/// 托盘图标：应用 Logo 的两个尺寸，宿主按自己的需要挑。
fn icons() -> Vec<ksni::Icon> {
    [ICON_64, ICON_32]
        .into_iter()
        .filter_map(|png| match decode_icon(png) {
            Ok(icon) => Some(icon),
            Err(err) => {
                tracing::warn!("tray icon could not be decoded: {err}");
                None
            }
        })
        .collect()
}

fn decode_icon(png: &[u8]) -> Result<ksni::Icon, String> {
    let image = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .map_err(|err| err.to_string())?
        .into_rgba8();
    let (width, height) = image.dimensions();
    Ok(ksni::Icon {
        width: width as i32,
        height: height as i32,
        data: rgba_to_argb(image.into_raw()),
    })
}

/// SNI 的像素是网络字节序的 ARGB（字节顺序 A, R, G, B），解码出来是 R, G, B, A。
fn rgba_to_argb(mut data: Vec<u8>) -> Vec<u8> {
    for pixel in data.as_chunks_mut::<4>().0 {
        pixel.rotate_right(1);
    }
    data
}

// ---------------------------------------------------------------------------------------------
// 服务线程

/// 建托盘服务。`assume_sni_available(true)`：没有 StatusNotifierWatcher（原版 GNOME）也不报错返回，
/// 而是把它当作"宿主离线"交给 `watcher_offline`，等宿主出现时 ksni 自己重新登记。
fn try_spawn(
    state: &Arc<State>,
    tx: &Sender,
    lock_on: bool,
) -> Result<ksni::blocking::Handle<ZeeTray>, ksni::Error> {
    // 宿主在不在线要等 `spawn()` 的结果才知道：先标成"未知"（按没有托盘处理），
    // 初次登记失败时 ksni 会在 `spawn()` 返回前同步回调 `watcher_offline`。
    state.watcher.reset();
    let handle = ZeeTray {
        state: Arc::clone(state),
        tx: tx.clone(),
        lock_on,
    }
    .assume_sni_available(true)
    .spawn()?;
    state.spawned.store(true, Ordering::Release);
    // `spawn()` 成功返回、期间没有 `watcher_offline`：宿主在线（启动时就在的宿主不会触发 `watcher_online`）。
    state.watcher.confirm_up_if_unknown();
    let _ = tx.unbounded_send(Command::AvailabilityChanged);
    Ok(handle)
}

/// D-Bus 连不上之类的错误之后多久再试一次。
const RETRY_AFTER: Duration = Duration::from_secs(20);
/// 查一次"菜单要不要变"（锁屏开关）的间隔。
const MENU_POLL: Duration = Duration::from_secs(1);

fn supervise(state: Arc<State>, tx: Sender) {
    loop {
        let lock_on = state.lock_menu.load(Ordering::Acquire);
        let handle = match try_spawn(&state, &tx, lock_on) {
            Ok(handle) => handle,
            Err(err) => {
                tracing::warn!("tray service could not be started: {err}");
                state.spawned.store(false, Ordering::Release);
                std::thread::sleep(RETRY_AFTER);
                continue;
            }
        };
        let mut shown_lock = lock_on;
        while !handle.is_closed() {
            std::thread::sleep(MENU_POLL);
            let lock_on = state.lock_menu.load(Ordering::Acquire);
            if lock_on != shown_lock {
                handle.update(move |tray| tray.lock_on = lock_on);
                shown_lock = lock_on;
            }
        }
        state.spawned.store(false, Ordering::Release);
        let _ = tx.unbounded_send(Command::AvailabilityChanged);
    }
}

/// 启动托盘。必须在 GPUI 主线程调用；托盘服务在自己的线程里，命令经一个可跨线程发送的队列回到主线程，
/// 不跨线程碰 Entity，也不把 `AsyncApp` 移出主线程。
pub fn spawn(cx: &mut App, window: AnyWindowHandle, main: Entity<crate::main_window::MainWindow>) {
    let (tx, mut rx) = futures::channel::mpsc::unbounded::<Command>();
    let state = Arc::clone(state());

    // 窗口自己回到前台了（Alt+Tab、点任务栏……）就不再算"收在托盘里"。
    let _ = window.update(cx, {
        let main = main.clone();
        move |_, window, cx| {
            main.update(cx, |_, cx| {
                cx.observe_window_activation(window, |_, window, _| {
                    if window.is_window_active() {
                        state_hidden().store(false, Ordering::Release);
                    }
                })
                .detach();
            });
        }
    });

    cx.spawn(async move |cx| {
        while let Some(command) = rx.next().await {
            match command {
                Command::Show => {
                    let _ = window.update(cx, |_, window, _| bring_to_front(window));
                }
                Command::Toggle => {
                    // 窗口的真实状态要在界面线程上读：收在托盘里 / 不可见（最小化）/ 不在前台 -> 叫出来。
                    let main = main.clone();
                    let _ = window.update(cx, move |_, window, cx| {
                        let action = toggle_action(
                            state_hidden().load(Ordering::Acquire),
                            window.is_visible(),
                            window.is_window_active(),
                        );
                        match action {
                            ToggleAction::BringToFront => bring_to_front(window),
                            ToggleAction::Hide => {
                                // 和点关闭按钮收进托盘走同一套：记住位置布局，需要的话先锁定
                                main.update(cx, |this, cx| {
                                    this.save_state(window);
                                    this.lock_for_tray(cx);
                                });
                                hide(window);
                            }
                        }
                    });
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
                Command::Quit => {
                    // 有进行中的传输 / AI 回合：先把主窗口叫出来问一下，确认后由主窗口自己退出。
                    let main = main.clone();
                    let can_quit = window
                        .update(cx, move |_, window, cx| {
                            let can = main.update(cx, |this, cx| this.request_quit(cx));
                            if !can {
                                bring_to_front(window);
                            }
                            can
                        })
                        .unwrap_or(true);
                    if can_quit {
                        cx.update(|app| app.quit());
                    }
                }
                Command::AvailabilityChanged => {
                    // 宿主消失时窗口正收在托盘里：立刻叫出来，免得找不回
                    if !available() && state_hidden().load(Ordering::Acquire) {
                        let _ = window.update(cx, |_, window, _| bring_to_front(window));
                    }
                    // 设置页里"关闭时最小化到托盘"的可用提示要跟着变
                    cx.update(|app| app.refresh_windows());
                }
            }
        }
    })
    .detach();

    if let Err(err) = std::thread::Builder::new()
        .name("zssh-tray".to_owned())
        .spawn(move || supervise(state, tx))
    {
        tracing::warn!("tray thread could not be started: {err}");
    }
}

fn state_hidden() -> &'static AtomicBool {
    &state().hidden
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_hides_only_when_the_tray_is_available() {
        use crate::app_prefs::CloseChoice::{Quit, Tray};
        assert_eq!(close_action_for(true, Tray), CloseAction::HideToTray);
        // 没有托盘宿主：不管设置选了什么都退出，不能把窗口藏起来
        assert_eq!(close_action_for(false, Tray), CloseAction::Quit);
        assert_eq!(close_action_for(true, Quit), CloseAction::Quit);
        assert_eq!(close_action_for(false, Quit), CloseAction::Quit);
    }

    #[test]
    fn the_tray_is_available_only_with_both_the_service_and_the_watcher() {
        let state = State::new();
        assert!(!state.available(), "服务还没建好");
        state.spawned.store(true, Ordering::Release);
        assert!(!state.available(), "宿主在不在线还没确认：按没有托盘处理");
        state.watcher.set_up();
        assert!(state.available());
        state.watcher.set_down();
        assert!(!state.available(), "宿主(StatusNotifierWatcher)离线");
        state.watcher.set_up();
        assert!(state.available(), "宿主回来了");
    }

    #[test]
    fn the_watcher_is_unknown_at_first_and_only_confirmed_when_no_callback_came() {
        // 初始未知，不是在线：确认之前"启动时隐藏"和"关窗口"都当没有托盘。
        let state = State::new();
        state.spawned.store(true, Ordering::Release);
        assert!(!state.available());
        // spawn 返回、期间没有任何回调：确认在线。
        state.watcher.confirm_up_if_unknown();
        assert!(state.available());

        // spawn 期间收到了 watcher_offline（原版 GNOME，没有宿主）：不能被"确认在线"覆盖。
        let state = State::new();
        state.spawned.store(true, Ordering::Release);
        state.watcher.set_down();
        state.watcher.confirm_up_if_unknown();
        assert!(!state.available());

        // 重新建服务之前，旧结论作废。
        state.watcher.reset();
        assert!(!state.available());
    }

    #[test]
    fn clicking_the_icon_only_hides_a_visible_foreground_window() {
        use ToggleAction::{BringToFront, Hide};
        // 收在托盘里 -> 叫出来
        assert_eq!(toggle_action(true, false, false), BringToFront);
        assert_eq!(toggle_action(true, true, true), BringToFront);
        // 用户自己最小化的（hidden 还是假，但不可见）-> 叫出来，不能再收一次
        assert_eq!(toggle_action(false, false, false), BringToFront);
        assert_eq!(toggle_action(false, false, true), BringToFront);
        // 可见但不在前台 -> 叫到前台
        assert_eq!(toggle_action(false, true, false), BringToFront);
        // 可见且在前台 -> 收起
        assert_eq!(toggle_action(false, true, true), Hide);
    }

    #[test]
    fn the_menu_is_the_shared_menu_with_the_right_commands() {
        let entries = menu_entries(true, None, true);
        let menu = build_menu(&entries);
        assert_eq!(menu.len(), entries.len());
        let labels: Vec<Option<String>> = menu
            .iter()
            .map(|item| match item {
                ksni::MenuItem::Standard(item) => Some(item.label.clone()),
                ksni::MenuItem::Separator => None,
                other => panic!("unexpected menu item {other:?}"),
            })
            .collect();
        assert_eq!(
            labels,
            vec![
                Some("显示 ZeeTerm".to_owned()),
                Some("锁定".to_owned()),
                Some("检查更新".to_owned()),
                None,
                Some("退出".to_owned()),
            ]
        );
        assert_eq!(command_for(SHOW_ID), Some(Command::Show));
        assert_eq!(command_for(LOCK_ID), Some(Command::Lock));
        assert_eq!(command_for(CHECK_ID), Some(Command::CheckUpdate));
        assert_eq!(command_for(QUIT_ID), Some(Command::Quit));
        assert_eq!(command_for("nope"), None);
    }

    #[test]
    fn clicking_a_menu_item_sends_its_command() {
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<Command>();
        let mut tray = ZeeTray {
            state: Arc::new(State::new()),
            tx,
            lock_on: true,
        };
        let menu = ksni::Tray::menu(&tray);
        for (item, expected) in menu.iter().zip([
            Some(Command::Show),
            Some(Command::Lock),
            Some(Command::CheckUpdate),
            None,
            Some(Command::Quit),
        ]) {
            if let ksni::MenuItem::Standard(item) = item {
                (item.activate)(&mut tray);
                assert_eq!(rx.try_recv().ok(), expected);
            }
        }
        // 左键点图标 = 切换显示
        ksni::Tray::activate(&mut tray, 0, 0);
        assert_eq!(rx.try_recv().ok(), Some(Command::Toggle));
    }

    #[test]
    fn labels_escape_underscores_for_the_dbus_menu() {
        // 版本号 / 产品名里将来出现下划线时不能被当成快捷键标记吃掉
        let entry = TrayEntry::RestartInstall {
            version: "1.0_rc".to_owned(),
        };
        let menu = build_menu(&[entry]);
        match &menu[0] {
            ksni::MenuItem::Standard(item) => assert!(item.label.ends_with("1.0__rc")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn icons_are_argb_with_a_transparent_corner_and_an_opaque_centre() {
        let icons = icons();
        assert_eq!(icons.len(), 2);
        for icon in &icons {
            assert_eq!(icon.data.len() as i32, icon.width * icon.height * 4);
            assert_eq!(icon.data[0], 0, "左上角的 alpha 应该是 0（透明）");
            let centre = ((icon.height / 2 * icon.width + icon.width / 2) * 4) as usize;
            assert_eq!(icon.data[centre], 255, "中心的 alpha 应该是 255（不透明）");
        }
        assert_eq!(rgba_to_argb(vec![1, 2, 3, 4]), vec![4, 1, 2, 3]);
    }
}

/// 真实 D-Bus 上的生命周期：宿主（StatusNotifierWatcher）晚于托盘出现、中途消失、再回来，
/// 可用状态都要跟着变。需要会话总线（门禁在一次性的 dbus-run-session 里跑）；没有就跳过。
#[cfg(test)]
mod dbus_tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Instant;

    struct FakeWatcher {
        registered: Arc<Mutex<Vec<String>>>,
    }

    #[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
    impl FakeWatcher {
        fn register_status_notifier_item(&self, service: &str) {
            if let Ok(mut list) = self.registered.lock() {
                list.push(service.to_owned());
            }
        }

        #[zbus(property)]
        fn is_status_notifier_host_registered(&self) -> bool {
            true
        }
    }

    fn start_watcher(
        registered: &Arc<Mutex<Vec<String>>>,
    ) -> zbus::Result<zbus::blocking::Connection> {
        zbus::blocking::connection::Builder::session()?
            .name("org.kde.StatusNotifierWatcher")?
            .serve_at(
                "/StatusNotifierWatcher",
                FakeWatcher {
                    registered: Arc::clone(registered),
                },
            )?
            .build()
    }

    fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if condition() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for: {what}");
    }

    #[test]
    fn availability_follows_the_watcher_appearing_vanishing_and_returning() {
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            eprintln!("skipped: no D-Bus session bus in this environment");
            return;
        }
        let registered = Arc::new(Mutex::new(Vec::new()));
        // 开发机的桌面上可能已经有真的 watcher：不去抢它的名字
        match start_watcher(&registered) {
            Ok(conn) => drop(conn),
            Err(err) => {
                eprintln!("skipped: cannot own the watcher name here: {err}");
                return;
            }
        }
        wait_until("the probe watcher to go away", || {
            zbus::blocking::Connection::session()
                .ok()
                .and_then(|c| zbus::blocking::fdo::DBusProxy::new(&c).ok())
                .and_then(|p| {
                    p.name_has_owner("org.kde.StatusNotifierWatcher".try_into().ok()?)
                        .ok()
                })
                == Some(false)
        });
        registered.lock().expect("lock").clear();

        let state = Arc::new(State::new());
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<Command>();
        // 1. 先起托盘，此时还没有 watcher（原版 GNOME 的情形）：服务建好了，但不可用
        let handle = try_spawn(&state, &tx, false).expect("tray service starts without a watcher");
        assert!(state.spawned.load(Ordering::Acquire));
        wait_until("watcher_offline", || state.watcher.is_down());
        assert!(!state.available(), "没有宿主时不可用");

        // 2. 宿主出现（用户装上 / 启用了 AppIndicator 扩展）：自动登记，变为可用，并通知界面
        let watcher = start_watcher(&registered).expect("start fake watcher");
        wait_until("availability after the watcher appears", || {
            state.available()
        });
        wait_until("registration at the watcher", || {
            !registered.lock().expect("lock").is_empty()
        });
        let mut notified = false;
        while let Ok(command) = rx.try_recv() {
            notified |= command == Command::AvailabilityChanged;
        }
        assert!(notified, "界面应收到可用状态变化的通知");

        // 3. 宿主消失（扩展被关掉）：不可用
        drop(watcher);
        wait_until("availability to drop", || !state.available());

        // 4. 宿主又回来：重新登记，恢复可用
        registered.lock().expect("lock").clear();
        let watcher = start_watcher(&registered).expect("restart fake watcher");
        wait_until("availability to return", || state.available());
        wait_until("re-registration", || {
            !registered.lock().expect("lock").is_empty()
        });

        drop(watcher);
        handle.shutdown().wait();
    }
}
