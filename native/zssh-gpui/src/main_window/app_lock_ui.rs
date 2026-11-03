//! 锁屏（V2-U26）在主窗口里的部分：底部栏的锁按钮和右键菜单、Ctrl+Alt+L、设置页「锁屏」一节、
//! 设置 / 修改 / 关闭 / 重置密码的对话框、空闲和关到托盘时的自动锁定、锁着时主窗口画什么。
//!
//! 密码规则、哈希、设置读写在 `zssh_core::app_lock`；全局的“锁着没有”、输错退避、后台校验任务和锁屏页
//! 本身在 `crate::app_lock`。这里只做主窗口一侧的转接，能抽成纯数据的（菜单项、对话框字段、点击去向、
//! 空闲判断、忘记密码的结果）都抽出来，状态测试不用起窗口。

use std::time::{Duration, Instant};

use ::gpui::{AnyElement, ClickEvent, Entity, Window};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::PopupMenu;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::*;
use zssh_core::app_lock::{self, IDLE_CHOICES, LockConfig, LockStatus, OsCheck};

use super::MainWindow;
use super::editor_tabs::UnsavedPurpose;
use super::modal::{ConfirmAction, Modal};
use super::nav::MainView;
use super::widgets::{ButtonLook, Choice, error_note, field, note, select, text_button};
use crate::app_lock::{self as lock_ui, Job, JobOutcome, LockScreen};
use crate::app_theme::Tokens;
use crate::icons::Ico;
use crate::settings_nav::SettingsSection;
use crate::settings_view::{card, row};
use crate::typography::{Role, Typo as _, UiFontStyled as _};

/// 锁按钮的悬停说明。
pub const LOCK_TOOLTIP: &str = "锁定 ZeeTerm（Ctrl+Alt+L）";

// ---- 纯数据：点击去向、右键菜单 ----

/// 锁按钮左键点下去做什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockClick {
    /// 已启用：立即锁定。
    LockNow,
    /// 没启用：打开设置的「锁屏」一节并弹出“设置锁屏密码”。
    SetUp,
}

pub fn left_click_action(enabled: bool) -> LockClick {
    if enabled {
        LockClick::LockNow
    } else {
        LockClick::SetUp
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockMenuItem {
    LockNow,
    ChangePassword,
    Settings,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockMenuEntry {
    pub item: LockMenuItem,
    pub label: &'static str,
    pub enabled: bool,
}

/// 锁按钮的右键菜单：立即锁定 / 修改锁屏密码… / 锁屏设置…；没启用时前两项置灰。
pub fn lock_menu(enabled: bool) -> Vec<LockMenuEntry> {
    vec![
        LockMenuEntry {
            item: LockMenuItem::LockNow,
            label: "立即锁定",
            enabled,
        },
        LockMenuEntry {
            item: LockMenuItem::ChangePassword,
            label: "修改锁屏密码…",
            enabled,
        },
        LockMenuEntry {
            item: LockMenuItem::Settings,
            label: "锁屏设置…",
            enabled: true,
        },
    ]
}

fn menu_icon(item: LockMenuItem) -> Ico {
    match item {
        LockMenuItem::LockNow => Ico::Lock,
        LockMenuItem::ChangePassword => Ico::KeyRound,
        LockMenuItem::Settings => Ico::MonitorCog,
    }
}

/// 把 [`lock_menu`] 画成弹出菜单。
pub fn build_lock_menu(
    mut menu: PopupMenu,
    enabled: bool,
    entity: &Entity<MainWindow>,
    tokens: &Tokens,
) -> PopupMenu {
    for entry in lock_menu(enabled) {
        let line = crate::menu_kit::row(tokens, entry.label, menu_icon(entry.item))
            .disabled(!entry.enabled);
        let entity = entity.clone();
        let item = entry.item;
        let click = move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            entity.update(cx, |this, cx| match item {
                LockMenuItem::LockNow => this.lock_now(cx),
                LockMenuItem::ChangePassword => {
                    this.open_lock_dialog(LockDialogKind::Change, window, cx)
                }
                LockMenuItem::Settings => this.open_lock_settings(window, cx),
            });
        };
        menu = menu.item(if entry.enabled {
            line.on_click(click)
        } else {
            line.inert()
        });
    }
    menu
}

/// 这次按键是不是锁屏快捷键 Ctrl+Alt+L（不带 Shift / Win）。
pub fn is_lock_shortcut(key: &str, control: bool, alt: bool, shift: bool, platform: bool) -> bool {
    control && alt && !shift && !platform && key.eq_ignore_ascii_case("l")
}

// ---- 纯数据：空闲自动锁定 ----

/// 空闲了这么久要不要锁。设置是“不自动”（0 分钟）时永远不锁。`override_secs` 是调试钩子给的秒数
/// （release 里恒为 `None`），有就代替“分钟数 × 60”，让自动化不用真等几分钟。
pub fn idle_expired(idle: Duration, minutes: u32, override_secs: Option<u64>) -> bool {
    if minutes == 0 {
        return false;
    }
    let limit = match override_secs {
        Some(secs) => Duration::from_secs(secs),
        None => Duration::from_secs(u64::from(minutes) * 60),
    };
    idle >= limit
}

/// 设置页下拉里的名字。
pub fn idle_label(minutes: u32) -> String {
    if minutes == 0 {
        "不自动".to_owned()
    } else {
        format!("{minutes} 分钟")
    }
}

// ---- 纯数据：忘记密码的结果 ----

/// 验证了 Windows 账户之后界面该做什么。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForgotStep {
    /// 通过：弹出“设新密码”。
    OpenResetDialog,
    /// 没通过 / 走不了：提示这句话。
    Notice(&'static str),
    /// 用户自己取消：什么都不用说。
    Nothing,
}

pub fn forgot_step(check: OsCheck) -> ForgotStep {
    match check {
        OsCheck::Verified => ForgotStep::OpenResetDialog,
        OsCheck::Failed => ForgotStep::Notice("Windows 账户验证没有通过，没有重置锁屏密码"),
        OsCheck::Cancelled => ForgotStep::Nothing,
        OsCheck::Unavailable => {
            ForgotStep::Notice("这台电脑上无法验证 Windows 账户，不能重置锁屏密码")
        }
    }
}

// ---- 纯数据：对话框 ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockDialogKind {
    /// 启用：新密码 ×2。
    Enable,
    /// 修改：原密码 + 新密码 ×2。
    Change,
    /// 关闭：当前密码。
    Disable,
    /// 忘记密码、Windows 验证通过之后：新密码 ×2。
    Reset,
}

impl LockDialogKind {
    pub fn title(self) -> &'static str {
        match self {
            Self::Enable => "设置锁屏密码",
            Self::Change => "修改锁屏密码",
            Self::Disable => "关闭锁屏",
            Self::Reset => "重置锁屏密码",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Enable => {
                "锁屏时窗口里的内容全部盖住，连接和传输照常在后台运行。密码只保存在这台电脑上，无法找回。"
            }
            Self::Change => "修改密码需要先输入原来的密码。",
            Self::Disable => "关闭锁屏需要输入当前的锁屏密码。",
            Self::Reset => "已验证你是这台电脑的 Windows 用户。设一个新密码，旧密码作废。",
        }
    }

    /// 每个输入框的标签，从上到下。
    pub fn labels(self) -> &'static [&'static str] {
        match self {
            Self::Enable | Self::Reset => &["新密码", "再输入一次"],
            Self::Change => &["原密码", "新密码", "再输入一次新密码"],
            Self::Disable => &["当前密码"],
        }
    }

    pub fn ok_label(self) -> &'static str {
        match self {
            Self::Enable => "启用",
            Self::Change => "修改",
            Self::Disable => "关闭锁屏",
            Self::Reset => "重置",
        }
    }

    /// 要不要校验一个“现有密码”（要的话计入输错次数、受退避限制）。
    pub fn needs_old(self) -> bool {
        matches!(self, Self::Change | Self::Disable)
    }

    pub fn success_text(self) -> &'static str {
        match self {
            Self::Enable => "锁屏已启用",
            Self::Change => "锁屏密码已修改",
            Self::Disable => "锁屏已关闭",
            Self::Reset => "锁屏密码已重置",
        }
    }

    /// 把输入框里的值变成后台任务；不合格返回要给用户看的原因。
    /// `values` 和 [`Self::labels`] 一一对应。
    pub fn prepare(self, values: &[String]) -> Result<Job, String> {
        let value = |index: usize| values.get(index).cloned().unwrap_or_default();
        let check_new = |first: usize| {
            app_lock::check_new_password(&value(first), &value(first + 1))
                .map_err(|problem| problem.message())
        };
        match self {
            Self::Enable => {
                check_new(0)?;
                Ok(Job::Enable { password: value(0) })
            }
            Self::Reset => {
                check_new(0)?;
                Ok(Job::Reset { new: value(0) })
            }
            Self::Change => {
                if value(0).is_empty() {
                    return Err("请输入原密码".to_owned());
                }
                check_new(1)?;
                Ok(Job::Change {
                    old: value(0),
                    new: value(1),
                })
            }
            Self::Disable => {
                if value(0).is_empty() {
                    return Err("请输入当前密码".to_owned());
                }
                Ok(Job::Disable { old: value(0) })
            }
        }
    }
}

/// 锁着的时候还能画出来的对话框：只有“退出前的确认”（点了关闭按钮、有进行中的工作）。别的对话框
/// 里可能有主机、文件名这类内容，等解锁再显示。
pub fn modal_visible_while_locked(modal: &Modal) -> bool {
    match modal {
        Modal::Confirm(confirm) => confirm.action == ConfirmAction::QuitApp,
        Modal::Unsaved(ask) => ask.purpose == UnsavedPurpose::Quit,
        _ => false,
    }
}

// ---- 状态 ----

pub struct LockDialog {
    pub kind: LockDialogKind,
    pub fields: Vec<Entity<InputState>>,
    pub error: Option<String>,
    /// 后台正在算哈希：不能再点、不能关。
    pub busy: bool,
}

impl LockDialog {
    pub fn busy(&self) -> bool {
        self.busy
    }
}

/// 主窗口上和锁屏有关的状态。
pub struct AppLockUi {
    /// 库里的锁屏设置（每两秒重读一次，改完马上更新）。
    pub config: LockConfig,
    /// 主窗口自己的锁屏页。
    pub screen: Option<Entity<LockScreen>>,
    /// 画过锁屏页：解锁后第一帧要把键盘还给终端。
    pub refocus: bool,
    /// 正在等 Windows 验证（忘记密码）。
    pub os_busy: bool,
}

impl AppLockUi {
    pub fn load(db: Option<&zssh_core::store::Database>) -> Self {
        Self {
            config: db.map(LockConfig::load).unwrap_or_default(),
            screen: None,
            refocus: false,
            os_busy: false,
        }
    }
}

impl MainWindow {
    pub(super) fn lock_enabled(&self) -> bool {
        self.app_lock.config.status.is_enabled()
    }

    pub(super) fn reload_lock_config(&mut self) {
        if let Ok(db) = self.store.db() {
            self.app_lock.config = LockConfig::load(db);
        }
        crate::tray::set_lock_menu(self.lock_enabled());
    }

    // ---- 锁定 ----

    /// 立即锁定。没启用什么都不做。
    pub(crate) fn lock_now(&mut self, cx: &mut Context<Self>) {
        if !self.lock_enabled() || lock_ui::is_locked() {
            return;
        }
        lock_ui::lock(cx);
        cx.notify();
    }

    /// 关到托盘：设置里选了“关到托盘时锁定”就锁。
    pub(crate) fn lock_for_tray(&mut self, cx: &mut Context<Self>) {
        self.reload_lock_config();
        if self.app_lock.config.lock_on_tray {
            self.lock_now(cx);
        }
    }

    /// Ctrl+Alt+L（任何窗口里按都行，由全局按键观察者调用）。没启用时不拦：这个组合键在别的键盘布局
    /// 里可能是 AltGr+L，没开锁屏的人不该被抢走。返回是否处理了这次按键。
    pub(super) fn lock_shortcut(
        &mut self,
        event: &::gpui::KeystrokeEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let keystroke = &event.keystroke;
        let mods = keystroke.modifiers;
        if !is_lock_shortcut(
            &keystroke.key,
            mods.control,
            mods.alt,
            mods.shift,
            mods.platform,
        ) {
            return false;
        }
        if !self.lock_enabled() {
            return false;
        }
        self.lock_now(cx);
        true
    }

    /// 底部栏锁按钮的左键。
    pub(super) fn lock_button_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match left_click_action(self.lock_enabled()) {
            LockClick::LockNow => self.lock_now(cx),
            LockClick::SetUp => {
                self.open_lock_settings(window, cx);
                self.open_lock_dialog(LockDialogKind::Enable, window, cx);
            }
        }
    }

    /// 打开设置页的「锁屏」一节。
    pub(super) fn open_lock_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_settings_section(SettingsSection::Lock, cx);
        if self.nav.view != MainView::Settings {
            self.toggle_settings(window, cx);
        }
        cx.notify();
    }

    /// 每个节拍（半秒）：重读设置、同步托盘菜单；锁着时守住键盘焦点并清掉通知；没锁时判断空闲。
    pub(super) fn lock_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tick_count.is_multiple_of(4) {
            self.reload_lock_config();
        }
        let enabled = self.lock_enabled();
        if lock_ui::is_locked() {
            if !enabled {
                // 锁被别处关掉了（比如设置库被改）：别让人卡在锁屏页上。
                lock_ui::unlock(cx);
                cx.notify();
                return;
            }
            if let Some(screen) = self.app_lock.screen.clone() {
                screen.update(cx, |screen, cx| screen.keep_focus(window, cx));
            }
            clear_notifications(window, cx);
            return;
        }
        if !enabled {
            return;
        }
        let idle = zssh_platform_windows::activity::idle_for();
        let override_secs =
            crate::dev_hooks::var("ZSSH_LOCK_IDLE_SECS").and_then(|raw| raw.trim().parse().ok());
        if idle_expired(idle, self.app_lock.config.idle_minutes, override_secs) {
            self.lock_now(cx);
        }
    }

    /// 锁着时这个窗口画什么：整窗的锁屏页；退出前的确认对话框压在它上面。
    pub(super) fn render_lock_cover(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.app_lock.refocus = true;
        let tokens = Tokens::current(cx);
        let cover = crate::app_lock::cover(&mut self.app_lock.screen, true, window, cx)
            .unwrap_or_else(|| div().into_any_element());
        let modal = if self.modal.as_ref().is_some_and(modal_visible_while_locked) {
            self.render_modal(&tokens, window, cx)
        } else {
            None
        };
        div()
            .id("main-window-locked")
            .relative()
            .size_full()
            .bg(tokens.background)
            .ui_font_fallbacks(cx)
            .child(cover)
            .children(modal)
            .into_any_element()
    }

    /// 解锁后的第一帧：键盘还给终端 / 当前页。
    pub(super) fn after_unlock_refocus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.app_lock.refocus) {
            self.focus_content(window, cx);
        }
    }

    // ---- 对话框 ----

    pub(crate) fn open_lock_dialog(
        &mut self,
        kind: LockDialogKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let fields: Vec<Entity<InputState>> = kind
            .labels()
            .iter()
            .map(|_| cx.new(|cx| InputState::new(window, cx).masked(true)))
            .collect();
        if let Some(first) = fields.first() {
            let handle = first.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
        self.modal = Some(Modal::AppLock(Box::new(LockDialog {
            kind,
            fields,
            error: None,
            busy: false,
        })));
        cx.notify();
    }

    fn set_lock_dialog_error(&mut self, message: Option<String>, cx: &mut Context<Self>) {
        if let Some(Modal::AppLock(dialog)) = &mut self.modal {
            dialog.error = message;
        }
        cx.notify();
    }

    /// 对话框的“确定”（回车同）。
    pub(super) fn submit_lock_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::AppLock(dialog)) = &self.modal else {
            return;
        };
        if dialog.busy {
            return;
        }
        let kind = dialog.kind;
        let values: Vec<String> = dialog
            .fields
            .iter()
            .map(|field| field.read(cx).value().to_string())
            .collect();
        // 要校验现有密码的对话框和锁屏页共用一份输错计数：不能让它成为绕过退避的后门。
        if kind.needs_old()
            && let Some(left) = lock_ui::remaining_wait(Instant::now())
        {
            self.set_lock_dialog_error(Some(lock_ui::wait_text(left)), cx);
            return;
        }
        let job = match kind.prepare(&values) {
            Ok(job) => job,
            Err(reason) => {
                self.set_lock_dialog_error(Some(reason), cx);
                return;
            }
        };
        if let Some(Modal::AppLock(dialog)) = &mut self.modal {
            dialog.busy = true;
            dialog.error = None;
        }
        cx.notify();
        lock_ui::spawn_job(window, cx, job, move |this, outcome, window, cx| {
            this.lock_dialog_done(kind, outcome, window, cx)
        });
    }

    fn lock_dialog_done(
        &mut self,
        kind: LockDialogKind,
        outcome: JobOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(Modal::AppLock(dialog)) = &mut self.modal {
            dialog.busy = false;
        }
        match outcome {
            JobOutcome::Done => {
                if kind.needs_old() || kind == LockDialogKind::Reset {
                    lock_ui::record_attempt(true, Instant::now());
                }
                self.reload_lock_config();
                self.modal = None;
                self.focus_content(window, cx);
                window.push_notification(Notification::success(kind.success_text()), cx);
            }
            JobOutcome::WrongPassword => {
                let wait = lock_ui::record_attempt(false, Instant::now());
                let mut message = "原密码不正确".to_owned();
                if kind == LockDialogKind::Disable {
                    message = "密码不正确".to_owned();
                }
                if !wait.is_zero() {
                    message = format!("{message}。{}", lock_ui::wait_text(wait));
                }
                // 输错的那个框清空，光标回到它。
                if let Some(Modal::AppLock(dialog)) = &self.modal
                    && let Some(old) = dialog.fields.first().cloned()
                {
                    old.update(cx, |state, cx| state.set_value("", window, cx));
                    let handle = old.read(cx).focus_handle(cx);
                    window.focus(&handle, cx);
                }
                self.set_lock_dialog_error(Some(message), cx);
            }
            JobOutcome::Failed(reason) => self.set_lock_dialog_error(Some(reason), cx),
        }
        cx.notify();
    }

    pub(super) fn lock_dialog_body(
        &self,
        dialog: &LockDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cancel = cx.entity();
        let ok = cancel.clone();
        let kind = dialog.kind;
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_1p5()
                    .child(div().typo(Role::GroupTitle, tokens).child(kind.title()))
                    .child(note(kind.description(), tokens)),
            )
            .children(
                kind.labels()
                    .iter()
                    .zip(dialog.fields.iter())
                    .map(|(label, input)| field(label, None, Input::new(input), tokens)),
            )
            .children(
                dialog
                    .error
                    .clone()
                    .map(|error| error_note(error, tokens.danger)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(text_button(
                        "modal-cancel",
                        "取消",
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            cancel.update(cx, |this, cx| this.close_modal(window, cx));
                        },
                    ))
                    .child(text_button(
                        "modal-ok",
                        if dialog.busy {
                            "处理中…"
                        } else {
                            kind.ok_label()
                        },
                        if kind == LockDialogKind::Disable {
                            ButtonLook::Danger
                        } else {
                            ButtonLook::Primary
                        },
                        move |_, window, cx| {
                            ok.update(cx, |this, cx| this.confirm_modal(window, cx));
                        },
                    )),
            )
            .into_any_element()
    }

    // ---- 忘记密码 ----

    /// 设置里“忘记密码”：先验证当前 Windows 用户，过了才让设新密码。
    pub(super) fn start_forgot_reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.app_lock.os_busy || !self.lock_enabled() {
            return;
        }
        self.app_lock.os_busy = true;
        cx.notify();
        let hwnd = lock_ui::window_hwnd(window);
        cx.spawn_in(window, async move |this, cx| {
            // 系统验证窗口会一直等到用户操作完：放在后台线程，别卡住界面。
            let check = cx
                .background_executor()
                .spawn(async move { lock_ui::verify_windows_user(hwnd) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.finish_forgot_reset(check, window, cx)
            });
        })
        .detach();
    }

    pub(super) fn finish_forgot_reset(
        &mut self,
        check: OsCheck,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.app_lock.os_busy = false;
        match forgot_step(check) {
            ForgotStep::OpenResetDialog => self.open_lock_dialog(LockDialogKind::Reset, window, cx),
            ForgotStep::Notice(text) => self.notice_error(text, window, cx),
            ForgotStep::Nothing => {}
        }
        cx.notify();
    }

    // ---- 设置页「锁屏」一节 ----

    pub(super) fn lock_blocks(
        &mut self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let entity = cx.entity();
        let config = self.app_lock.config.clone();
        let enabled = config.status.is_enabled();
        let unreadable = config.status == LockStatus::Unreadable;

        let switch = {
            let entity = entity.clone();
            Switch::new("lock-enabled")
                .checked(enabled)
                .accessibility_label("启用锁屏")
                .on_click(move |checked: &bool, window, cx| {
                    let want = *checked;
                    entity.update(cx, |this, cx| {
                        let kind = if want {
                            LockDialogKind::Enable
                        } else {
                            LockDialogKind::Disable
                        };
                        this.open_lock_dialog(kind, window, cx);
                    });
                })
        };

        let mut rows = vec![row(
            "启用锁屏",
            Some(
                "锁定后窗口里的内容全部盖住，要输入锁屏密码才能继续；连接、传输、AI 在后台照常运行。",
            ),
            switch,
            tokens,
        )];

        if enabled {
            let change = entity.clone();
            let disable = entity.clone();
            rows.push(row(
                "锁屏密码",
                Some(if unreadable {
                    "密码记录读不出来，只能用下面的“忘记密码”重置。"
                } else {
                    "修改或关闭都要先输入原来的密码。"
                }),
                h_flex()
                    .gap_2()
                    .child(
                        text_button(
                            "lock-change",
                            "修改密码…",
                            ButtonLook::Outline,
                            move |_, window, cx| {
                                change.update(cx, |this, cx| {
                                    this.open_lock_dialog(LockDialogKind::Change, window, cx)
                                });
                            },
                        )
                        .disabled(unreadable),
                    )
                    .child(
                        text_button(
                            "lock-disable",
                            "关闭锁屏",
                            ButtonLook::Outline,
                            move |_, window, cx| {
                                disable.update(cx, |this, cx| {
                                    this.open_lock_dialog(LockDialogKind::Disable, window, cx)
                                });
                            },
                        )
                        .disabled(unreadable),
                    ),
                tokens,
            ));
        }

        let idle_entity = entity.clone();
        let idle_current = config.idle_minutes;
        rows.push(row(
            "空闲多久自动锁定",
            Some("以键盘、鼠标在 ZeeTerm 窗口里的操作为准；窗口在后台时也计时。需要先启用锁屏。"),
            div().w(::gpui::px(160.0)).child(select(
                "lock-idle",
                idle_label(idle_current),
                IDLE_CHOICES
                    .iter()
                    .map(|minutes| Choice::new(idle_label(*minutes), *minutes == idle_current))
                    .collect(),
                move |index, window, cx| {
                    let minutes = IDLE_CHOICES.get(index).copied().unwrap_or(0);
                    idle_entity.update(cx, |this, cx| this.set_lock_idle(minutes, window, cx));
                },
            )),
            tokens,
        ));

        let tray_entity = entity.clone();
        rows.push(row(
            "关到托盘时自动锁定",
            Some("点关闭按钮缩到托盘的同时锁定；从托盘重新打开时先看到锁屏页。需要先启用锁屏。"),
            Switch::new("lock-on-tray")
                .checked(config.lock_on_tray)
                .accessibility_label("关到托盘时自动锁定")
                .on_click(move |checked: &bool, window, cx| {
                    let on = *checked;
                    tray_entity.update(cx, |this, cx| this.set_lock_on_tray(on, window, cx));
                }),
            tokens,
        ));

        if enabled {
            let forgot = entity.clone();
            let busy = self.app_lock.os_busy;
            rows.push(row(
                "忘记密码",
                Some("密码无法找回。可以用 Windows Hello / PIN（没有就用 Windows 登录密码）验证你是这台电脑的用户，然后设一个新密码。"),
                text_button(
                    "lock-forgot-reset",
                    if busy { "验证中…" } else { "验证 Windows 账户并重置…" },
                    ButtonLook::Outline,
                    move |_, window, cx| {
                        forgot.update(cx, |this, cx| this.start_forgot_reset(window, cx));
                    },
                )
                .disabled(busy),
                tokens,
            ));
        }

        vec![card(
            "settings-lock",
            "锁屏",
            Some("锁屏密码和这些选项只保存在这台电脑上，不同步，也不进备份。"),
            tokens,
            rows,
        )]
    }

    fn set_lock_idle(&mut self, minutes: u32, window: &mut Window, cx: &mut Context<Self>) {
        let result = self
            .store
            .db()
            .and_then(|db| app_lock::save_idle_minutes(db, minutes).map_err(|err| err.to_string()));
        match result {
            Ok(()) => self.reload_lock_config(),
            Err(err) => self.notice_error(&err, window, cx),
        }
        cx.notify();
    }

    fn set_lock_on_tray(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        let result = self
            .store
            .db()
            .and_then(|db| app_lock::save_lock_on_tray(db, on).map_err(|err| err.to_string()));
        match result {
            Ok(()) => self.reload_lock_config(),
            Err(err) => self.notice_error(&err, window, cx),
        }
        cx.notify();
    }
}

/// 锁着时主窗口右上角弹出来的通知（小条）可能带主机、文件名：每个节拍清一遍。
/// 只有主窗口是用 `gpui_kit::open_window` 开的（带组件库的 Root，通知层在那里）；文件窗口、终端窗口、
/// 更新窗口是 `cx.open_window` 直接开的，没有通知层（对它们调 `notifications` 会 panic），也从不弹通知。
fn clear_notifications(window: &mut Window, cx: &mut Context<MainWindow>) {
    if !window.notifications(cx).is_empty() {
        window.clear_notifications(cx);
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{
        ForgotStep, LockClick, LockDialogKind, LockMenuItem, forgot_step, idle_expired, idle_label,
        is_lock_shortcut, left_click_action, lock_menu, modal_visible_while_locked,
    };
    use crate::app_lock::Job;
    use crate::main_window::modal::{Confirm, ConfirmAction, Modal};
    use std::time::Duration;
    use zssh_core::app_lock::OsCheck;

    // ---- 锁按钮 ----

    #[test]
    fn left_click_locks_when_enabled_and_starts_setup_otherwise() {
        assert_eq!(left_click_action(true), LockClick::LockNow);
        assert_eq!(left_click_action(false), LockClick::SetUp);
    }

    #[test]
    fn the_right_click_menu_has_three_items_and_greys_the_first_two_when_off() {
        let on = lock_menu(true);
        assert_eq!(
            on.iter().map(|e| e.label).collect::<Vec<_>>(),
            ["立即锁定", "修改锁屏密码…", "锁屏设置…"]
        );
        assert!(on.iter().all(|entry| entry.enabled));

        let off = lock_menu(false);
        assert_eq!(off[0].item, LockMenuItem::LockNow);
        assert!(!off[0].enabled, "没启用时不能立即锁定");
        assert_eq!(off[1].item, LockMenuItem::ChangePassword);
        assert!(!off[1].enabled, "没启用时没有密码可改");
        assert_eq!(off[2].item, LockMenuItem::Settings);
        assert!(off[2].enabled, "锁屏设置永远可点，启用也从这里来");
    }

    #[test]
    fn ctrl_alt_l_is_the_lock_shortcut_and_nothing_close_to_it() {
        assert!(is_lock_shortcut("l", true, true, false, false));
        assert!(is_lock_shortcut("L", true, true, false, false));
        assert!(
            !is_lock_shortcut("l", true, false, false, false),
            "Ctrl+L 留给终端"
        );
        assert!(!is_lock_shortcut("l", false, true, false, false));
        assert!(
            !is_lock_shortcut("l", true, true, true, false),
            "多按 Shift 不算"
        );
        assert!(!is_lock_shortcut("l", true, true, false, true));
        assert!(!is_lock_shortcut("k", true, true, false, false));
    }

    #[test]
    fn the_lock_shortcut_is_listed_once_and_no_other_listed_shortcut_uses_it() {
        let rows: Vec<_> = crate::settings_nav::SHORTCUTS
            .iter()
            .filter(|shortcut| shortcut.keys.contains("Ctrl+Alt"))
            .collect();
        assert_eq!(rows.len(), 1, "表里只有锁屏用 Ctrl+Alt 开头的组合键");
        assert_eq!(rows[0].keys, "Ctrl+Alt+L");
        assert!(rows[0].what.contains("锁定"));
    }

    // ---- 空闲 ----

    #[test]
    fn idle_lock_fires_only_after_the_chosen_minutes() {
        let mins = |m: u64| Duration::from_secs(m * 60);
        assert!(
            !idle_expired(mins(100), 0, None),
            "0 = 不自动，闲多久都不锁"
        );
        assert!(!idle_expired(mins(4), 5, None));
        assert!(idle_expired(mins(5), 5, None));
        assert!(idle_expired(mins(31), 30, None));
        assert!(!idle_expired(mins(59), 60, None));
        assert!(idle_expired(mins(60), 60, None));
    }

    #[test]
    fn the_debug_override_replaces_the_minutes() {
        assert!(idle_expired(Duration::from_secs(3), 5, Some(2)));
        assert!(!idle_expired(Duration::from_secs(1), 5, Some(2)));
        // 钩子只是把“分钟”换成“秒”，不能让“不自动”也锁。
        assert!(!idle_expired(Duration::from_secs(3), 0, Some(2)));
    }

    #[test]
    fn idle_choices_read_naturally() {
        assert_eq!(idle_label(0), "不自动");
        assert_eq!(idle_label(5), "5 分钟");
        assert_eq!(idle_label(60), "60 分钟");
        assert_eq!(
            zssh_core::app_lock::IDLE_CHOICES.map(idle_label),
            [
                "不自动",
                "5 分钟",
                "10 分钟",
                "15 分钟",
                "30 分钟",
                "60 分钟"
            ]
        );
    }

    // ---- 对话框的字段与规则 ----

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn each_dialog_asks_for_the_right_fields() {
        assert_eq!(LockDialogKind::Enable.labels(), ["新密码", "再输入一次"]);
        assert_eq!(
            LockDialogKind::Change.labels(),
            ["原密码", "新密码", "再输入一次新密码"]
        );
        assert_eq!(LockDialogKind::Disable.labels(), ["当前密码"]);
        assert_eq!(LockDialogKind::Reset.labels(), ["新密码", "再输入一次"]);
        assert!(LockDialogKind::Change.needs_old());
        assert!(LockDialogKind::Disable.needs_old());
        assert!(!LockDialogKind::Enable.needs_old());
        assert!(
            !LockDialogKind::Reset.needs_old(),
            "重置靠 Windows 验证，不问旧密码"
        );
    }

    #[test]
    fn enabling_needs_a_long_enough_password_typed_twice() {
        let kind = LockDialogKind::Enable;
        assert_eq!(
            kind.prepare(&v(&["abc", "abc"])),
            Err("密码至少 4 个字符".to_owned())
        );
        assert_eq!(
            kind.prepare(&v(&["abcd", "abce"])),
            Err("两次输入的密码不一致".to_owned())
        );
        assert!(matches!(
            kind.prepare(&v(&["abcd", "abcd"])),
            Ok(Job::Enable { .. })
        ));
    }

    #[test]
    fn changing_checks_the_new_password_and_insists_on_an_old_one() {
        let kind = LockDialogKind::Change;
        assert_eq!(
            kind.prepare(&v(&["", "newpass", "newpass"])),
            Err("请输入原密码".to_owned())
        );
        assert_eq!(
            kind.prepare(&v(&["oldpass", "new", "new"])),
            Err("密码至少 4 个字符".to_owned())
        );
        assert_eq!(
            kind.prepare(&v(&["oldpass", "newpass", "newpazz"])),
            Err("两次输入的密码不一致".to_owned())
        );
        match kind.prepare(&v(&["oldpass", "newpass", "newpass"])) {
            Ok(Job::Change { old, new }) => {
                assert_eq!((old.as_str(), new.as_str()), ("oldpass", "newpass"));
            }
            other => panic!("{:?}", other.map(|job| format!("{job:?}"))),
        }
    }

    #[test]
    fn disabling_and_resetting_have_their_own_rules() {
        assert_eq!(
            LockDialogKind::Disable.prepare(&v(&[""])),
            Err("请输入当前密码".to_owned())
        );
        assert!(matches!(
            LockDialogKind::Disable.prepare(&v(&["current"])),
            Ok(Job::Disable { .. })
        ));
        assert!(matches!(
            LockDialogKind::Reset.prepare(&v(&["fresh1", "fresh1"])),
            Ok(Job::Reset { .. })
        ));
        assert_eq!(
            LockDialogKind::Reset.prepare(&v(&["fresh1", "fresh2"])),
            Err("两次输入的密码不一致".to_owned())
        );
    }

    #[test]
    fn every_dialog_has_a_title_a_button_and_a_success_line() {
        for kind in [
            LockDialogKind::Enable,
            LockDialogKind::Change,
            LockDialogKind::Disable,
            LockDialogKind::Reset,
        ] {
            assert!(!kind.title().is_empty());
            assert!(!kind.description().is_empty());
            assert!(!kind.ok_label().is_empty());
            assert!(!kind.success_text().is_empty());
        }
        assert_eq!(LockDialogKind::Enable.title(), "设置锁屏密码");
    }

    // ---- 忘记密码 ----

    #[test]
    fn the_three_windows_check_outcomes_map_to_what_the_user_sees() {
        assert_eq!(forgot_step(OsCheck::Verified), ForgotStep::OpenResetDialog);
        assert!(
            matches!(forgot_step(OsCheck::Failed), ForgotStep::Notice(text) if text.contains("没有通过"))
        );
        assert!(
            matches!(forgot_step(OsCheck::Unavailable), ForgotStep::Notice(text) if text.contains("不能重置")),
            "两条路都走不了要明说：无法验证，不能重置"
        );
        assert_eq!(forgot_step(OsCheck::Cancelled), ForgotStep::Nothing);
    }

    // ---- 锁着时能画哪些对话框 ----

    #[test]
    fn only_the_quit_confirmation_may_show_over_the_lock_page() {
        let confirm = |action| {
            Modal::Confirm(Confirm {
                title: String::new(),
                message: String::new(),
                confirm_label: Some("退出".to_owned()),
                destructive: true,
                action,
            })
        };
        assert!(modal_visible_while_locked(&confirm(ConfirmAction::QuitApp)));
        assert!(!modal_visible_while_locked(&confirm(
            ConfirmAction::DeleteHost(1)
        )));
        assert!(!modal_visible_while_locked(&Modal::AiPresets));
    }
}
