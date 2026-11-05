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
use crate::i18n::{Msg, t};
use crate::icons::Ico;
use crate::settings_nav::SettingsSection;
use crate::settings_view::{card, row};
use crate::typography::{Role, Typo as _, UiFontStyled as _};

/// 锁按钮的悬停说明。快捷键部分（Ctrl+Alt+L）两个语言下都不译，只译「锁定 ZeeTerm」。
pub(crate) fn lock_tooltip() -> &'static str {
    t(LockUiMsg::LockTooltip)
}

/// 主窗口里和锁屏有关的界面文案（右键菜单、对话框、设置页「锁屏」一节）。
///
/// 只放应用自己的固定文案；产品名、分钟数、等待秒数由 `format!` 带进来。
#[derive(Clone, Copy, Debug)]
enum LockUiMsg {
    // ---- 锁按钮悬停说明 ----
    LockTooltip,
    // ---- 锁按钮右键菜单 ----
    MenuLockNow,
    MenuChangePassword,
    MenuSettings,
    // ---- 空闲自动锁定 ----
    IdleNever,
    IdleMinutesSuffix,
    // ---- 忘记密码 ----
    WindowsVerifyFailed,
    WindowsVerifyUnavailable,
    // ---- 对话框标题 ----
    DialogEnableTitle,
    DialogChangeTitle,
    DialogDisableTitle,
    DialogResetTitle,
    // ---- 对话框说明 ----
    DialogEnableDescription,
    DialogChangeDescription,
    DialogDisableDescription,
    DialogResetDescription,
    // ---- 字段 ----
    LabelNewPassword,
    LabelRepeat,
    LabelOldPassword,
    LabelRepeatNew,
    LabelCurrentPassword,
    // ---- 按钮 / 结果 ----
    OkEnable,
    OkChange,
    OkDisable,
    OkReset,
    SuccessEnabled,
    SuccessChanged,
    SuccessDisabled,
    SuccessReset,
    Cancel,
    Processing,
    WrongOldPassword,
    WrongPassword,
    SentenceEnd,
    // ---- 设置页「锁屏」一节 ----
    SwitchLabel,
    SwitchDescription,
    PasswordRowTitle,
    PasswordReadableDescription,
    PasswordUnreadableDescription,
    ChangePassword,
    DisableLock,
    IdleRowTitle,
    IdleRowDescription,
    TrayRowTitle,
    TrayRowDescription,
    StartRowTitle,
    StartDescriptionEnabled,
    StartDescriptionDisabled,
    ForgotRowTitle,
    ForgotRowDescription,
    ForgotBusy,
    ForgotRowButton,
    SectionTitle,
    SectionDescription,
    // ---- 输入校验（核心只给类型，文字在界面这一层出） ----
    PasswordTooShortPrefix,
    PasswordTooLongPrefix,
    PasswordLengthSuffix,
    PasswordMismatch,
    EnterOldPassword,
    EnterCurrentPassword,
}

impl Msg for LockUiMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::LockTooltip => "锁定 ZeeTerm（Ctrl+Alt+L）",
            Self::MenuLockNow => "立即锁定",
            Self::MenuChangePassword => "修改锁屏密码…",
            Self::MenuSettings => "锁屏设置…",
            Self::IdleNever => "不自动",
            Self::IdleMinutesSuffix => " 分钟",
            Self::WindowsVerifyFailed => "Windows 账户验证没有通过，没有重置锁屏密码",
            Self::WindowsVerifyUnavailable => "这台电脑上无法验证 Windows 账户，不能重置锁屏密码",
            Self::DialogEnableTitle => "设置锁屏密码",
            Self::DialogChangeTitle => "修改锁屏密码",
            Self::DialogDisableTitle => "关闭锁屏",
            Self::DialogResetTitle => "重置锁屏密码",
            Self::DialogEnableDescription => {
                "锁屏时窗口里的内容全部盖住，连接和传输照常在后台运行。密码只保存在这台电脑上，无法找回。"
            }
            Self::DialogChangeDescription => "修改密码需要先输入原来的密码。",
            Self::DialogDisableDescription => "关闭锁屏需要输入当前的锁屏密码。",
            Self::DialogResetDescription => {
                "已验证你是这台电脑的 Windows 用户。设一个新密码，旧密码作废。"
            }
            Self::LabelNewPassword => "新密码",
            Self::LabelRepeat => "再输入一次",
            Self::LabelOldPassword => "原密码",
            Self::LabelRepeatNew => "再输入一次新密码",
            Self::LabelCurrentPassword => "当前密码",
            Self::OkEnable => "启用",
            Self::OkChange => "修改",
            Self::OkDisable => "关闭锁屏",
            Self::OkReset => "重置",
            Self::SuccessEnabled => "锁屏已启用",
            Self::SuccessChanged => "锁屏密码已修改",
            Self::SuccessDisabled => "锁屏已关闭",
            Self::SuccessReset => "锁屏密码已重置",
            Self::Cancel => "取消",
            Self::Processing => "处理中…",
            Self::WrongOldPassword => "原密码不正确",
            Self::WrongPassword => "密码不正确",
            Self::SentenceEnd => "。",
            Self::SwitchLabel => "启用锁屏",
            Self::SwitchDescription => {
                "锁定后窗口里的内容全部盖住，要输入锁屏密码才能继续；连接、传输、AI 在后台照常运行。"
            }
            Self::PasswordRowTitle => "锁屏密码",
            Self::PasswordReadableDescription => "修改或关闭都要先输入原来的密码。",
            Self::PasswordUnreadableDescription => "密码记录读不出来，只能用下面的“忘记密码”重置。",
            Self::ChangePassword => "修改密码…",
            Self::DisableLock => "关闭锁屏",
            Self::IdleRowTitle => "空闲多久自动锁定",
            Self::IdleRowDescription => {
                "以键盘、鼠标在 ZeeTerm 窗口里的操作为准；窗口在后台时也计时。需要先启用锁屏。"
            }
            Self::TrayRowTitle => "关到托盘时自动锁定",
            Self::TrayRowDescription => {
                "点关闭按钮缩到托盘的同时锁定；从托盘重新打开时先看到锁屏页。需要先启用锁屏。"
            }
            Self::StartRowTitle => "启动时锁定",
            Self::StartDescriptionEnabled => {
                "打开后，每次启动 ZeeTerm 都先显示锁屏页，解锁前看不到任何主机和终端，也不会自动恢复或连接；从托盘打开、再次双击图标看到的也是锁屏页。"
            }
            Self::StartDescriptionDisabled => "需要先启用锁屏并设置密码，才能打开这一项。",
            Self::ForgotRowTitle => "忘记密码",
            Self::ForgotRowDescription => {
                "密码无法找回。可以用 Windows Hello / PIN（没有就用 Windows 登录密码）验证你是这台电脑的用户，然后设一个新密码。"
            }
            Self::ForgotBusy => "验证中…",
            Self::ForgotRowButton => "验证 Windows 账户并重置…",
            Self::SectionTitle => "锁屏",
            Self::SectionDescription => {
                "锁屏密码和这些选项只保存在这台电脑上，不同步，也不进备份。"
            }
            Self::PasswordTooShortPrefix => "密码至少 ",
            Self::PasswordTooLongPrefix => "密码最多 ",
            Self::PasswordLengthSuffix => " 个字符",
            Self::PasswordMismatch => "两次输入的密码不一致",
            Self::EnterOldPassword => "请输入原密码",
            Self::EnterCurrentPassword => "请输入当前密码",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::LockTooltip => "Lock ZeeTerm (Ctrl+Alt+L)",
            Self::MenuLockNow => "Lock Now",
            Self::MenuChangePassword => "Change Lock Password…",
            Self::MenuSettings => "App Lock Settings…",
            Self::IdleNever => "Never",
            Self::IdleMinutesSuffix => " min",
            Self::WindowsVerifyFailed => {
                "Windows account verification failed; the lock password was not reset"
            }
            Self::WindowsVerifyUnavailable => {
                "A Windows account cannot be verified on this computer; the lock password cannot be reset"
            }
            Self::DialogEnableTitle => "Set Lock Password",
            Self::DialogChangeTitle => "Change Lock Password",
            Self::DialogDisableTitle => "Turn Off App Lock",
            Self::DialogResetTitle => "Reset Lock Password",
            Self::DialogEnableDescription => {
                "While locked, everything in the window is covered and connections keep running in the background. The password is stored only on this computer and cannot be recovered."
            }
            Self::DialogChangeDescription => {
                "Changing the password requires the current password first."
            }
            Self::DialogDisableDescription => {
                "Turning off app lock requires the current lock password."
            }
            Self::DialogResetDescription => {
                "You have verified that you are a Windows user of this computer. Set a new password; the old one stops working."
            }
            Self::LabelNewPassword => "New password",
            Self::LabelRepeat => "Enter it again",
            Self::LabelOldPassword => "Current password",
            Self::LabelRepeatNew => "Enter the new password again",
            Self::LabelCurrentPassword => "Current password",
            Self::OkEnable => "Enable",
            Self::OkChange => "Change",
            Self::OkDisable => "Turn Off",
            Self::OkReset => "Reset",
            Self::SuccessEnabled => "App lock enabled",
            Self::SuccessChanged => "Lock password changed",
            Self::SuccessDisabled => "App lock turned off",
            Self::SuccessReset => "Lock password reset",
            Self::Cancel => "Cancel",
            Self::Processing => "Working…",
            Self::WrongOldPassword => "The current password is incorrect",
            Self::WrongPassword => "The password is incorrect",
            Self::SentenceEnd => ". ",
            Self::SwitchLabel => "Enable app lock",
            Self::SwitchDescription => {
                "When locked, everything in the window is covered and the lock password is required to continue; connections, transfers, and AI keep running in the background."
            }
            Self::PasswordRowTitle => "Lock password",
            Self::PasswordReadableDescription => {
                "Changing or turning it off requires the current password."
            }
            Self::PasswordUnreadableDescription => {
                "The password record cannot be read; use \"Forgot password\" below to reset it."
            }
            Self::ChangePassword => "Change Password…",
            Self::DisableLock => "Turn Off App Lock",
            Self::IdleRowTitle => "Auto-lock when idle",
            Self::IdleRowDescription => {
                "Keyboard and mouse activity in ZeeTerm windows counts, including windows in the background. Requires app lock to be enabled."
            }
            Self::TrayRowTitle => "Lock when minimized to tray",
            Self::TrayRowDescription => {
                "Locks when the window is minimized to the tray; reopening from the tray shows the lock screen. Requires app lock to be enabled."
            }
            Self::StartRowTitle => "Lock on startup",
            Self::StartDescriptionEnabled => {
                "When on, ZeeTerm shows the lock screen at every startup; no hosts or terminals are visible and nothing is restored or connected before unlocking. Opening from the tray or double-clicking the icon again also shows the lock screen."
            }
            Self::StartDescriptionDisabled => {
                "Enable app lock and set a password before turning this on."
            }
            Self::ForgotRowTitle => "Forgot password",
            Self::ForgotRowDescription => {
                "The password cannot be recovered. You can verify that you are a user of this computer with Windows Hello / PIN (or your Windows sign-in password), then set a new password."
            }
            Self::ForgotBusy => "Verifying…",
            Self::ForgotRowButton => "Verify Windows Account and Reset…",
            Self::SectionTitle => "App Lock",
            Self::SectionDescription => {
                "The lock password and these options are stored only on this computer; they are not synced or included in backups."
            }
            Self::PasswordTooShortPrefix => "Password must be at least ",
            Self::PasswordTooLongPrefix => "Password must be at most ",
            Self::PasswordLengthSuffix => " characters",
            Self::PasswordMismatch => "The two passwords do not match",
            Self::EnterOldPassword => "Enter the current password",
            Self::EnterCurrentPassword => "Enter the current password",
        }
    }
}

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
            label: t(LockUiMsg::MenuLockNow),
            enabled,
        },
        LockMenuEntry {
            item: LockMenuItem::ChangePassword,
            label: t(LockUiMsg::MenuChangePassword),
            enabled,
        },
        LockMenuEntry {
            item: LockMenuItem::Settings,
            label: t(LockUiMsg::MenuSettings),
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

// ---- 启动时锁定（V2-U35） ----

/// 主窗口建出来之前套用“启动时锁定”：开关打开并且锁屏启用着，就让第一帧直接是锁屏页。
/// 返回现在是不是锁着。其他窗口（文件窗口、拖出的终端窗口）读同一份全局状态，不用各自处理。
pub fn apply_start_lock(config: &LockConfig) -> bool {
    if config.start_locked() {
        lock_ui::set_locked(true);
    }
    lock_ui::is_locked()
}

/// 启动后要做的后台扫描（上次没处理完的修改、Z-SSH 迁移提示）能不能开始：锁着就等解锁以后。
/// 解锁前不恢复任何东西；解锁后照原来的逻辑做一次。
pub fn startup_scans_allowed() -> bool {
    !lock_ui::is_locked()
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
        t(LockUiMsg::IdleNever).to_owned()
    } else {
        format!("{minutes}{}", t(LockUiMsg::IdleMinutesSuffix))
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
        OsCheck::Failed => ForgotStep::Notice(t(LockUiMsg::WindowsVerifyFailed)),
        OsCheck::Cancelled => ForgotStep::Nothing,
        OsCheck::Unavailable => ForgotStep::Notice(t(LockUiMsg::WindowsVerifyUnavailable)),
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
            Self::Enable => t(LockUiMsg::DialogEnableTitle),
            Self::Change => t(LockUiMsg::DialogChangeTitle),
            Self::Disable => t(LockUiMsg::DialogDisableTitle),
            Self::Reset => t(LockUiMsg::DialogResetTitle),
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Enable => t(LockUiMsg::DialogEnableDescription),
            Self::Change => t(LockUiMsg::DialogChangeDescription),
            Self::Disable => t(LockUiMsg::DialogDisableDescription),
            Self::Reset => t(LockUiMsg::DialogResetDescription),
        }
    }

    /// 每个输入框的标签，从上到下。
    pub fn labels(self) -> Vec<&'static str> {
        match self {
            Self::Enable | Self::Reset => {
                vec![t(LockUiMsg::LabelNewPassword), t(LockUiMsg::LabelRepeat)]
            }
            Self::Change => vec![
                t(LockUiMsg::LabelOldPassword),
                t(LockUiMsg::LabelNewPassword),
                t(LockUiMsg::LabelRepeatNew),
            ],
            Self::Disable => vec![t(LockUiMsg::LabelCurrentPassword)],
        }
    }

    pub fn ok_label(self) -> &'static str {
        match self {
            Self::Enable => t(LockUiMsg::OkEnable),
            Self::Change => t(LockUiMsg::OkChange),
            Self::Disable => t(LockUiMsg::OkDisable),
            Self::Reset => t(LockUiMsg::OkReset),
        }
    }

    /// 要不要校验一个“现有密码”（要的话计入输错次数、受退避限制）。
    pub fn needs_old(self) -> bool {
        matches!(self, Self::Change | Self::Disable)
    }

    pub fn success_text(self) -> &'static str {
        match self {
            Self::Enable => t(LockUiMsg::SuccessEnabled),
            Self::Change => t(LockUiMsg::SuccessChanged),
            Self::Disable => t(LockUiMsg::SuccessDisabled),
            Self::Reset => t(LockUiMsg::SuccessReset),
        }
    }

    /// 把输入框里的值变成后台任务；不合格返回要给用户看的原因。
    /// `values` 和 [`Self::labels`] 一一对应。
    pub fn prepare(self, values: &[String]) -> Result<Job, String> {
        let value = |index: usize| values.get(index).cloned().unwrap_or_default();
        let check_new = |first: usize| {
            app_lock::check_new_password(&value(first), &value(first + 1))
                .map_err(password_problem_text)
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
                    return Err(t(LockUiMsg::EnterOldPassword).to_owned());
                }
                check_new(1)?;
                Ok(Job::Change {
                    old: value(0),
                    new: value(1),
                })
            }
            Self::Disable => {
                if value(0).is_empty() {
                    return Err(t(LockUiMsg::EnterCurrentPassword).to_owned());
                }
                Ok(Job::Disable { old: value(0) })
            }
        }
    }
}

/// 新密码不合格的原因：核心只给类型，文字在这里按当前语言生成。
fn password_problem_text(problem: app_lock::PasswordProblem) -> String {
    match problem {
        app_lock::PasswordProblem::TooShort => format!(
            "{}{}{}",
            t(LockUiMsg::PasswordTooShortPrefix),
            app_lock::MIN_PASSWORD_CHARS,
            t(LockUiMsg::PasswordLengthSuffix)
        ),
        app_lock::PasswordProblem::TooLong => format!(
            "{}{}{}",
            t(LockUiMsg::PasswordTooLongPrefix),
            app_lock::MAX_PASSWORD_CHARS,
            t(LockUiMsg::PasswordLengthSuffix)
        ),
        app_lock::PasswordProblem::Mismatch => t(LockUiMsg::PasswordMismatch).to_owned(),
    }
}

/// 锁着的时候还能画出来的对话框：“退出前的确认”（点了关闭按钮、有进行中的工作），以及锁屏页上
/// “忘记密码 → 验证 Windows 账户”通过后弹出的“重置锁屏密码”（V2-U35：开了“启动时锁定”，重新打开
/// 仍是锁屏页，到不了设置，所以重置必须能在锁屏页上完成）。别的对话框里可能有主机、文件名这类内容，
/// 等解锁再显示；“重置”只认**锁着的时候**才弹出来的那一个（`over_lock`），解锁状态下开着的重置框
/// 在锁上的那一刻就会被收掉，不能趁锁着替人把它点完。
pub fn modal_visible_while_locked(modal: &Modal) -> bool {
    match modal {
        Modal::Confirm(confirm) => confirm.action == ConfirmAction::QuitApp,
        Modal::Unsaved(ask) => ask.purpose == UnsavedPurpose::Quit,
        Modal::AppLock(dialog) => dialog.over_lock && dialog.kind == LockDialogKind::Reset,
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
    /// 是在锁着的时候（锁屏页上走“忘记密码”）弹出来的：只有这样的“重置”框才能叠在锁屏页上，
    /// 成功后顺带解锁。
    pub over_lock: bool,
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
        // 属性面板里显示着的明文密码，锁上的这一刻就丢掉；查看 / 复制密码的验证框同理。
        self.info_secret = None;
        self.drop_reveal_ask();
        // 解锁状态下开着的锁屏密码对话框（含验证过 Windows 账户的“重置”框）一并收掉。
        if matches!(self.modal, Some(Modal::AppLock(_))) {
            self.modal = None;
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
            // 叠在锁屏页上的“重置锁屏密码”框开着时，键盘归它的输入框，不能被锁屏页的密码框抢回去。
            let reset_dialog_open = matches!(self.modal, Some(Modal::AppLock(_)));
            if !reset_dialog_open && let Some(screen) = self.app_lock.screen.clone() {
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
        // 锁屏页的“忘记密码”里带“验证 Windows 账户并重置…”按钮（只有主窗口有；V2-U35）。
        let weak = cx.entity().downgrade();
        let forgot: lock_ui::ForgotHook = std::rc::Rc::new(move |window, cx| {
            let _ = weak.update(cx, |this, cx| this.start_forgot_reset(window, cx));
        });
        let cover =
            crate::app_lock::cover_with(&mut self.app_lock.screen, true, Some(forgot), window, cx)
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
            // 只有叠在锁屏页上的“重置锁屏密码”框要收 Esc / Enter（其余按键不管）。
            .capture_key_down(cx.listener(|this, event, window, cx| {
                this.locked_modal_key(event, window, cx);
            }))
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
            over_lock: lock_ui::is_locked(),
        })));
        cx.notify();
    }

    /// 锁着时的一次按键：只有叠在锁屏页上的“重置锁屏密码”框收 Esc / Enter，别的一概不管。
    pub(super) fn locked_modal_key(
        &mut self,
        event: &::gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.modal.as_ref().is_some_and(modal_visible_while_locked)
            || !matches!(self.modal, Some(Modal::AppLock(_)))
        {
            return;
        }
        let mods = event.keystroke.modifiers;
        let key = event.keystroke.key.to_ascii_lowercase();
        match super::modal::modal_key(&key, !mods.modified(), false, false) {
            super::modal::ModalKey::Cancel => {
                self.close_modal(window, cx);
                cx.stop_propagation();
            }
            super::modal::ModalKey::Confirm => {
                self.confirm_modal(window, cx);
                cx.stop_propagation();
            }
            super::modal::ModalKey::Pass => {}
        }
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
        let over_lock = match &mut self.modal {
            Some(Modal::AppLock(dialog)) => {
                dialog.busy = false;
                dialog.over_lock
            }
            _ => false,
        };
        match outcome {
            JobOutcome::Done => {
                if kind.needs_old() || kind == LockDialogKind::Reset {
                    lock_ui::record_attempt(true, Instant::now());
                }
                self.reload_lock_config();
                self.modal = None;
                // 在锁屏页上走“忘记密码”重置成功：Windows 账户和新密码都过了，直接解锁。
                if over_lock && kind == LockDialogKind::Reset && lock_ui::is_locked() {
                    lock_ui::unlock(cx);
                }
                self.focus_content(window, cx);
                window.push_notification(Notification::success(kind.success_text()), cx);
            }
            JobOutcome::WrongPassword => {
                let wait = lock_ui::record_attempt(false, Instant::now());
                let mut message = t(LockUiMsg::WrongOldPassword).to_owned();
                if kind == LockDialogKind::Disable {
                    message = t(LockUiMsg::WrongPassword).to_owned();
                }
                if !wait.is_zero() {
                    message = format!(
                        "{message}{}{}",
                        t(LockUiMsg::SentenceEnd),
                        lock_ui::wait_text(wait)
                    );
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
                        t(LockUiMsg::Cancel),
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            cancel.update(cx, |this, cx| this.close_modal(window, cx));
                        },
                    ))
                    .child(text_button(
                        "modal-ok",
                        if dialog.busy {
                            t(LockUiMsg::Processing)
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
            // 锁着时右上角的通知每个节拍都会清掉，提示写在锁屏页上才看得见。
            ForgotStep::Notice(text) if lock_ui::is_locked() => {
                if let Some(screen) = self.app_lock.screen.clone() {
                    screen.update(cx, |screen, cx| screen.show_notice(text, cx));
                }
            }
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
                .accessibility_label(t(LockUiMsg::SwitchLabel))
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
            t(LockUiMsg::SwitchLabel),
            Some(t(LockUiMsg::SwitchDescription)),
            switch,
            tokens,
        )];

        if enabled {
            let change = entity.clone();
            let disable = entity.clone();
            rows.push(row(
                t(LockUiMsg::PasswordRowTitle),
                Some(if unreadable {
                    t(LockUiMsg::PasswordUnreadableDescription)
                } else {
                    t(LockUiMsg::PasswordReadableDescription)
                }),
                h_flex()
                    .gap_2()
                    .child(
                        text_button(
                            "lock-change",
                            t(LockUiMsg::ChangePassword),
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
                            t(LockUiMsg::DisableLock),
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
            t(LockUiMsg::IdleRowTitle),
            Some(t(LockUiMsg::IdleRowDescription)),
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
            t(LockUiMsg::TrayRowTitle),
            Some(t(LockUiMsg::TrayRowDescription)),
            Switch::new("lock-on-tray")
                .checked(config.lock_on_tray)
                .accessibility_label(t(LockUiMsg::TrayRowTitle))
                .on_click(move |checked: &bool, window, cx| {
                    let on = *checked;
                    tray_entity.update(cx, |this, cx| this.set_lock_on_tray(on, window, cx));
                }),
            tokens,
        ));

        let start_entity = entity.clone();
        rows.push(row(
            t(LockUiMsg::StartRowTitle),
            Some(if enabled {
                t(LockUiMsg::StartDescriptionEnabled)
            } else {
                t(LockUiMsg::StartDescriptionDisabled)
            }),
            Switch::new("lock-on-start")
                .checked(enabled && config.lock_on_start)
                .disabled(!enabled)
                .accessibility_label(t(LockUiMsg::StartRowTitle))
                .on_click(move |checked: &bool, window, cx| {
                    let on = *checked;
                    start_entity.update(cx, |this, cx| this.set_lock_on_start(on, window, cx));
                }),
            tokens,
        ));

        if enabled {
            let forgot = entity.clone();
            let busy = self.app_lock.os_busy;
            rows.push(row(
                t(LockUiMsg::ForgotRowTitle),
                Some(t(LockUiMsg::ForgotRowDescription)),
                text_button(
                    "lock-forgot-reset",
                    if busy {
                        t(LockUiMsg::ForgotBusy)
                    } else {
                        t(LockUiMsg::ForgotRowButton)
                    },
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
            t(LockUiMsg::SectionTitle),
            Some(t(LockUiMsg::SectionDescription)),
            tokens,
            rows,
        )]
    }

    fn set_lock_idle(&mut self, minutes: u32, window: &mut Window, cx: &mut Context<Self>) {
        let result = self.store.db().and_then(|db| {
            app_lock::save_idle_minutes(db, minutes)
                .map_err(|err| crate::error_text::error_text(&err))
        });
        match result {
            Ok(()) => self.reload_lock_config(),
            Err(err) => self.notice_error(&err, window, cx),
        }
        cx.notify();
    }

    /// 设置页「启动时锁定」开关。打开要求已经设了锁屏密码（核心库里也会拒绝，这里把原因告诉用户）。
    pub(crate) fn set_lock_on_start(
        &mut self,
        on: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result = self.store.db().and_then(|db| {
            app_lock::save_lock_on_start(db, on).map_err(|err| crate::error_text::error_text(&err))
        });
        match result {
            Ok(()) => self.reload_lock_config(),
            Err(err) => self.notice_error(&err, window, cx),
        }
        cx.notify();
    }

    fn set_lock_on_tray(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        let result = self.store.db().and_then(|db| {
            app_lock::save_lock_on_tray(db, on).map_err(|err| crate::error_text::error_text(&err))
        });
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
        ForgotStep, LockClick, LockDialog, LockDialogKind, LockMenuItem, apply_start_lock,
        forgot_step, idle_expired, idle_label, is_lock_shortcut, left_click_action, lock_menu,
        modal_visible_while_locked, startup_scans_allowed,
    };
    use crate::app_lock::Job;
    use crate::main_window::modal::{Confirm, ConfirmAction, Modal};
    use std::time::Duration;
    use zssh_core::app_lock::OsCheck;
    use zssh_core::app_lock::{LockConfig, LockStatus};

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

    // ---- 启动时锁定 ----

    fn config(status: LockStatus, lock_on_start: bool) -> LockConfig {
        LockConfig {
            status,
            lock_on_start,
            ..LockConfig::default()
        }
    }

    #[test]
    fn start_lock_locks_the_first_frame_and_holds_back_the_startup_scans() {
        crate::app_lock::set_locked(false);
        // 记录读不懂（Unreadable）也算启用着：照样锁，用户走“忘记密码”重置。
        assert!(apply_start_lock(&config(LockStatus::Unreadable, true)));
        assert!(crate::app_lock::is_locked(), "第一帧就是锁屏页");
        assert!(!startup_scans_allowed(), "解锁前不扫描、不恢复");

        crate::app_lock::set_locked(false);
        assert!(startup_scans_allowed(), "解锁以后照原来的逻辑做");
    }

    #[test]
    fn start_lock_does_nothing_unless_switched_on_and_a_password_exists() {
        crate::app_lock::set_locked(false);
        // 开关默认关：启用了锁屏，启动也不锁（和以前一样）。
        assert!(!apply_start_lock(&config(LockStatus::Unreadable, false)));
        // 开关残留、密码已经没了：不能把人锁在没有密码的锁屏页上。
        assert!(!apply_start_lock(&config(LockStatus::Disabled, true)));
        assert!(!apply_start_lock(&LockConfig::default()));
        assert!(startup_scans_allowed());
    }

    #[test]
    fn start_lock_leaves_an_already_locked_state_alone() {
        crate::app_lock::set_locked(true);
        let epoch = crate::app_lock::lock_epoch();
        assert!(apply_start_lock(&config(LockStatus::Unreadable, true)));
        assert_eq!(
            crate::app_lock::lock_epoch(),
            epoch,
            "已经锁着，不算新的一次锁定"
        );
        crate::app_lock::set_locked(false);
    }

    fn lock_dialog(kind: LockDialogKind, over_lock: bool) -> Modal {
        Modal::AppLock(Box::new(LockDialog {
            kind,
            fields: Vec::new(),
            error: None,
            busy: false,
            over_lock,
        }))
    }

    #[test]
    fn only_a_reset_opened_on_the_lock_page_may_show_over_it() {
        assert!(modal_visible_while_locked(&lock_dialog(
            LockDialogKind::Reset,
            true
        )));
        // 解锁状态下开着的重置框（Windows 验证早就过了）：锁上以后不能趁锁着替人点完。
        assert!(!modal_visible_while_locked(&lock_dialog(
            LockDialogKind::Reset,
            false
        )));
        // 改密码、关锁屏要原密码，更不能在锁着时露出来。
        for kind in [
            LockDialogKind::Enable,
            LockDialogKind::Change,
            LockDialogKind::Disable,
        ] {
            assert!(!modal_visible_while_locked(&lock_dialog(kind, true)));
        }
    }
}
