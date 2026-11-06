//! X11 的界面：设置页「连接」分类里的「X11 转发」一节（X 显示来源、退出时是否关闭自己拉起的 X 服务器、
//! VcXsrv 状态与一键安装），以及连接开了 X11 的主机而本机没有可用 X 服务器时弹出的
//! 「需要 X 显示程序」提示。
//!
//! 业务都在 `zssh_core::x11::local`（检测、下载校验、UAC 静默安装、设置读写）和 `x11_glue`；
//! 这里只做状态、判定和界面。判定（本机有没有可用的 X 服务器、状态文字、进度文字）是纯函数，有单测。
//! 安装在后台线程里跑，进度写进共享状态，主窗口每个节拍读一次（同 SFTP 传输的做法）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ::gpui::{AppContext as _, Entity, SharedString, px};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::x11::local::install::InstallError;
use zssh_core::x11::local::{DisplayMode, LocalXStatus, X11Prefs, prefs};

use super::MainWindow;
use super::modal::Modal;
use super::nav::MainView;
use super::widgets::{ButtonLook, error_note, note, text_button};
use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::settings_nav::SettingsSection;
use crate::settings_view::{Segment, card, choice_group, row, row_with};
use crate::typography::{Role, Typo as _};

/// 本文件（X11 设置、VcXsrv 安装、X 显示程序提示）的界面文案。X11、VcXsrv、DISPLAY、
/// UAC、SHA-256 这类名字两个语言下写法相同；显示号、版本号、主机名来自系统或用户，照原样显示。
#[derive(Clone, Copy, Debug)]
pub(crate) enum X11Msg {
    // ---- 状态 ----
    RunningOwnedPrefix,
    RunningOwnedSuffix,
    RunningPrefix,
    RunningSuffix,
    InstalledPrefix,
    InstalledBare,
    NotInstalled,
    // ---- 安装进度 ----
    DownloadPrefix,
    DownloadParenOpen,
    DownloadPercentSuffix,
    DownloadNoSize,
    VerifyThenUac,
    CancelledInstall,
    CancelledInstallPeriod,
    // ---- 提示框 ----
    Placeholder,
    NeedsXServer,
    HostQuoteOpen,
    PromptSuffix,
    InstallNow,
    InstallNotePrompt,
    AlreadyInstalledSettings,
    SkipOnce,
    Cancel,
    // ---- 设置页 ----
    Auto,
    CustomDisplay,
    Save,
    CloseOwnedTitle,
    CloseOwnedDescription,
    Detecting,
    Recheck,
    StatusPrefix,
    InstallFailedPrefix,
    InstallNotePrefix,
    InstallNoteSuffix,
    X11Forwarding,
    X11CardDescription,
    SourceTitle,
    SourceDescription,
    InstalledNotice,
    // ---- Linux：本机 X 服务器就是桌面会话的 DISPLAY，没有 VcXsrv 可装 ----
    PlaceholderLinux,
    PromptSuffixLinux,
    SpecifyDisplaySettings,
    SourceDescriptionLinux,
    SessionServerTitle,
    SessionDisplayPrefix,
    NoSessionDisplay,
}

impl Msg for X11Msg {
    fn zh(&self) -> &'static str {
        match self {
            Self::RunningOwnedPrefix => "运行中（ZeeTerm 启动的，显示 :",
            Self::RunningOwnedSuffix => "）",
            Self::RunningPrefix => "运行中（显示 :",
            Self::RunningSuffix => "）",
            Self::InstalledPrefix => "已安装 ",
            Self::InstalledBare => "已安装",
            Self::NotInstalled => "未安装",
            Self::DownloadPrefix => "正在下载 VcXsrv：",
            Self::DownloadParenOpen => "（",
            Self::DownloadPercentSuffix => "%）",
            Self::DownloadNoSize => "正在下载 VcXsrv…",
            Self::VerifyThenUac => {
                "已下载并校验。请在 Windows 弹出的「用户账户控制」里点「是」，然后等安装完成…"
            }
            Self::CancelledInstall => "已取消安装",
            Self::CancelledInstallPeriod => "已取消安装。",
            Self::Placeholder => "例如 localhost:0",
            Self::NeedsXServer => "需要 X 显示程序",
            Self::HostQuoteOpen => "「",
            Self::PromptSuffix => {
                "」开了 X11 转发，但这台电脑上没有可用的 X 服务器（如 VcXsrv）。没有它，远程的图形程序显示不出来。"
            }
            Self::InstallNow => "一键安装 VcXsrv",
            Self::InstallNotePrompt => {
                "从官方发布页下载并校验后安装；安装时 Windows 会弹出权限确认（UAC），需要点「是」。"
            }
            Self::AlreadyInstalledSettings => "我已安装其他 X 服务器，去设置",
            Self::SkipOnce => "这次不用 X11 继续连接",
            Self::Cancel => "取消",
            Self::Auto => "自动",
            Self::CustomDisplay => "指定显示",
            Self::Save => "保存",
            Self::CloseOwnedTitle => "ZeeTerm 退出时关闭自己启动的 X 服务器",
            Self::CloseOwnedDescription => {
                "只关这一次 ZeeTerm 自己拉起的 VcXsrv；你自己开的 X 服务器永远不碰。"
            }
            Self::Detecting => "正在检测…",
            Self::Recheck => "重新检测",
            Self::StatusPrefix => "状态：",
            Self::InstallFailedPrefix => "安装失败：",
            Self::InstallNotePrefix => "一键安装会从 VcXsrv 官方发布页下载安装包（约 ",
            Self::InstallNoteSuffix => {
                "），校验固定的 SHA-256 之后才运行；安装时 Windows 会弹出「用户账户控制」（UAC）窗口，需要你点「是」。ZeeTerm 不随程序分发 VcXsrv。"
            }
            Self::X11Forwarding => "X11 转发",
            Self::X11CardDescription => {
                "连接开了 X11 转发的 SSH 主机时，把远程的图形程序显示在本机的 X 服务器上。主机上的开关在「主机属性 → 连接」里。"
            }
            Self::SourceTitle => "X 显示来源",
            Self::SourceDescription => {
                "自动：先用已经在运行的 X 服务器，没有就启动 VcXsrv。指定显示：连到你自己管理的 X 服务器（X410、MobaXterm 等）。"
            }
            Self::InstalledNotice => "VcXsrv 已安装好，现在可以连接开了 X11 的主机了",
            Self::PlaceholderLinux => "例如 :0 或 localhost:10",
            Self::PromptSuffixLinux => {
                "」开了 X11 转发，但这台电脑上没有可用的 X 服务器（环境里没有 DISPLAY）。Linux 下需要 Xwayland（Wayland 桌面）或 Xorg；没有它，远程的图形程序显示不出来。"
            }
            Self::SpecifyDisplaySettings => "去设置里指定显示",
            Self::SourceDescriptionLinux => {
                "自动：使用当前桌面会话的 X 服务器（环境变量 DISPLAY；Wayland 桌面需要 Xwayland，Xorg 桌面自带）。指定显示：连到你写的显示，例如 :1 或 localhost:10。"
            }
            Self::SessionServerTitle => "本机 X 服务器",
            Self::SessionDisplayPrefix => "使用当前会话的显示 ",
            Self::NoSessionDisplay => "没有（环境里没有 DISPLAY，需要 Xwayland 或 Xorg）",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::RunningOwnedPrefix => "Running (started by ZeeTerm, display :",
            Self::RunningOwnedSuffix => ")",
            Self::RunningPrefix => "Running (display :",
            Self::RunningSuffix => ")",
            Self::InstalledPrefix => "Installed ",
            Self::InstalledBare => "Installed",
            Self::NotInstalled => "Not installed",
            Self::DownloadPrefix => "Downloading VcXsrv: ",
            Self::DownloadParenOpen => " (",
            Self::DownloadPercentSuffix => "%)",
            Self::DownloadNoSize => "Downloading VcXsrv…",
            Self::VerifyThenUac => {
                "Downloaded and verified. Click “Yes” in the Windows User Account Control prompt, then wait for the installation to finish…"
            }
            Self::CancelledInstall => "Installation cancelled",
            Self::CancelledInstallPeriod => "Installation cancelled.",
            Self::Placeholder => "for example localhost:0",
            Self::NeedsXServer => "An X display program is required",
            Self::HostQuoteOpen => "“",
            Self::PromptSuffix => {
                "” has X11 forwarding enabled, but this computer has no usable X server (such as VcXsrv). Without one, remote graphical programs cannot be displayed."
            }
            Self::InstallNow => "Install VcXsrv",
            Self::InstallNotePrompt => {
                "Downloads from the official release page, verifies it, then installs; Windows shows a permission prompt (UAC), where you need to click “Yes”."
            }
            Self::AlreadyInstalledSettings => "I already have another X server; open the settings",
            Self::SkipOnce => "Continue without X11 this time",
            Self::Cancel => "Cancel",
            Self::Auto => "Automatic",
            Self::CustomDisplay => "Specific Display",
            Self::Save => "Save",
            Self::CloseOwnedTitle => "Close the X server ZeeTerm started when ZeeTerm exits",
            Self::CloseOwnedDescription => {
                "Closes only the VcXsrv that this ZeeTerm run started; X servers you started yourself are never touched."
            }
            Self::Detecting => "Detecting…",
            Self::Recheck => "Check Again",
            Self::StatusPrefix => "Status: ",
            Self::InstallFailedPrefix => "Installation failed: ",
            Self::InstallNotePrefix => {
                "The one-click install downloads the installer from the official VcXsrv release page (about "
            }
            Self::InstallNoteSuffix => {
                "), verifies its pinned SHA-256 and only then runs it; Windows shows a User Account Control (UAC) prompt, where you need to click “Yes”. ZeeTerm does not bundle VcXsrv."
            }
            Self::X11Forwarding => "X11 Forwarding",
            Self::X11CardDescription => {
                "When connecting to SSH hosts with X11 forwarding, show remote graphical programs on this machine's X server. The per-host switch is under Host Properties → Connection."
            }
            Self::SourceTitle => "X Display Source",
            Self::SourceDescription => {
                "Automatic: use an already running X server, or start VcXsrv if there is none. Specific display: connect to an X server you manage yourself (X410, MobaXterm, and so on)."
            }
            Self::InstalledNotice => {
                "VcXsrv is installed; you can now connect to hosts with X11 enabled"
            }
            Self::PlaceholderLinux => "for example :0 or localhost:10",
            Self::PromptSuffixLinux => {
                "” has X11 forwarding enabled, but this computer has no usable X server (the DISPLAY variable is not set). On Linux an X server comes from Xwayland (Wayland desktops) or Xorg; without one, remote graphical programs cannot be displayed."
            }
            Self::SpecifyDisplaySettings => "Specify a display in settings",
            Self::SourceDescriptionLinux => {
                "Automatic: use the X server of the current desktop session (the DISPLAY variable; Wayland desktops need Xwayland, Xorg desktops have one built in). Specific display: connect to the display you give, such as :1 or localhost:10."
            }
            Self::SessionServerTitle => "Local X Server",
            Self::SessionDisplayPrefix => "Using the current session's display ",
            Self::NoSessionDisplay => {
                "None (the DISPLAY variable is not set; Xwayland or Xorg is required)"
            }
        }
    }
}

// ---- 纯逻辑 ----

/// 一键安装的进度。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallPhase {
    Idle,
    Downloading {
        done: u64,
        total: u64,
    },
    /// 下载校验完，等系统的权限确认（UAC）和安装程序跑完。
    Installing,
    Done {
        version: Option<String>,
    },
    Failed(String),
    Cancelled,
}

impl InstallPhase {
    /// 下载或安装正在进行（按钮要禁用、不能再点一次）。
    pub fn busy(&self) -> bool {
        matches!(self, Self::Downloading { .. } | Self::Installing)
    }
}

/// 本机有没有可用的 X 服务器：指定了显示（用户自己管理）、有正在运行的、ZeeTerm 自己拉起过的、
/// 或者装了 VcXsrv（连接时由 ZeeTerm 拉起）。都没有才需要提示。
pub fn server_available(prefs: &X11Prefs, status: &LocalXStatus) -> bool {
    matches!(prefs.mode, DisplayMode::Custom(_))
        || !status.running_displays.is_empty()
        || status.owned_display.is_some()
        || status.vcxsrv.is_some()
        || status.session_display.is_some()
}

/// VcXsrv 状态一行的文字：`未安装` / `已安装 21.1.16` / `运行中（显示 :0）`。
pub fn status_headline(status: &LocalXStatus) -> String {
    if let Some(display) = status.owned_display {
        return format!(
            "{}{display}{}",
            t(X11Msg::RunningOwnedPrefix),
            t(X11Msg::RunningOwnedSuffix)
        );
    }
    if let Some(display) = status.running_displays.first() {
        return format!(
            "{}{display}{}",
            t(X11Msg::RunningPrefix),
            t(X11Msg::RunningSuffix)
        );
    }
    match &status.vcxsrv {
        Some(install) => match &install.version {
            Some(version) => format!("{}{version}", t(X11Msg::InstalledPrefix)),
            None => t(X11Msg::InstalledBare).to_owned(),
        },
        None => t(X11Msg::NotInstalled).to_owned(),
    }
}

/// Linux 上"本机 X 服务器"一行的状态文字：当前会话的 `DISPLAY`，或者说明没有。
pub fn session_headline(status: &LocalXStatus) -> String {
    match &status.session_display {
        Some(display) => format!("{}{display}", t(X11Msg::SessionDisplayPrefix)),
        None => t(X11Msg::NoSessionDisplay).to_owned(),
    }
}

/// 字节数写成 MB（一位小数）。
pub fn format_mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
}

/// 进度文字；没有进行中的安装返回 `None`。
pub fn progress_text(phase: &InstallPhase) -> Option<String> {
    match phase {
        InstallPhase::Downloading { done, total } if *total > 0 => Some(format!(
            "{}{} / {}{}{}{}",
            t(X11Msg::DownloadPrefix),
            format_mb(*done),
            format_mb(*total),
            t(X11Msg::DownloadParenOpen),
            (done.saturating_mul(100) / total).min(100),
            t(X11Msg::DownloadPercentSuffix)
        )),
        InstallPhase::Downloading { .. } => Some(t(X11Msg::DownloadNoSize).to_owned()),
        InstallPhase::Installing => Some(t(X11Msg::VerifyThenUac).to_owned()),
        _ => None,
    }
}

/// 安装失败 / 取消的原因（给用户看）。
pub fn failure_text(error: &InstallError) -> String {
    match error {
        InstallError::Cancelled => t(X11Msg::CancelledInstall).to_owned(),
        other => other.to_string(),
    }
}

// ---- 状态 ----

pub struct X11Ui {
    /// 「指定显示」的文字框（`DISPLAY` 写法，例如 `localhost:0`）。
    pub custom: Entity<InputState>,
    pub custom_error: Option<String>,
    pub status: Option<LocalXStatus>,
    pub prefs: X11Prefs,
    pub phase: Arc<Mutex<InstallPhase>>,
    pub cancel: Arc<AtomicBool>,
    /// 上一次画出来的进度（变了才重绘）。
    shown: InstallPhase,
    loaded: bool,
}

impl X11Ui {
    pub fn new(window: &mut Window, cx: &mut Context<MainWindow>) -> Self {
        Self {
            custom: cx.new(|cx| {
                let placeholder = if cfg!(unix) {
                    X11Msg::PlaceholderLinux
                } else {
                    X11Msg::Placeholder
                };
                InputState::new(window, cx).placeholder(t(placeholder))
            }),
            custom_error: None,
            status: None,
            prefs: X11Prefs::default(),
            phase: Arc::new(Mutex::new(InstallPhase::Idle)),
            cancel: Arc::new(AtomicBool::new(false)),
            shown: InstallPhase::Idle,
            loaded: false,
        }
    }

    fn phase_now(&self) -> InstallPhase {
        self.phase
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// 「需要 X 显示程序」提示要记住的东西。
pub struct X11Prompt {
    pub host_id: i64,
    pub host_name: String,
    /// 选了「继续连接」之后用哪种方式开标签。
    pub new_tab: bool,
}

/// 提示里的三个选择。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum X11Choice {
    Install,
    Settings,
    SkipOnce,
}

impl MainWindow {
    // ---- 读 / 写设置 ----

    fn x11_load_prefs(&self) -> X11Prefs {
        self.store.db().map(prefs::load_prefs).unwrap_or_default()
    }

    /// 第一次需要时（进设置页「连接」、弹提示前）读一遍设置和本机状态。
    pub(super) fn x11_ensure_loaded(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.x11_ui.loaded {
            return;
        }
        self.x11_ui.loaded = true;
        self.x11_reload(window, cx);
    }

    /// 重读设置，重新检测本机 X 服务器。
    pub(crate) fn x11_reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prefs = self.x11_load_prefs();
        if let DisplayMode::Custom(spec) = &prefs.mode {
            let spec = spec.clone();
            self.x11_ui
                .custom
                .update(cx, |input, cx| input.set_value(spec, window, cx));
        }
        self.x11_ui.prefs = prefs;
        self.x11_ui.status = Some(crate::x11_glue::local_status());
        cx.notify();
    }

    pub(crate) fn x11_set_auto(&mut self, cx: &mut Context<Self>) {
        let result = self.store.db().and_then(|db| {
            prefs::save_display_mode(db, &DisplayMode::Auto)
                .map_err(|e| crate::error_text::error_text(&e))
        });
        match result {
            Ok(()) => {
                self.x11_ui.prefs.mode = DisplayMode::Auto;
                self.x11_ui.custom_error = None;
            }
            Err(err) => self.x11_ui.custom_error = Some(err),
        }
        cx.notify();
    }

    /// 切到「指定显示」：先用框里的值（没填用默认的 `localhost:0`）存一次，写不合法会留在界面上提示。
    pub(crate) fn x11_set_custom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let spec = self.x11_ui.custom.read(cx).value().trim().to_owned();
        let spec = if spec.is_empty() {
            prefs::DEFAULT_CUSTOM_DISPLAY.to_owned()
        } else {
            spec
        };
        let mode = DisplayMode::Custom(spec.clone());
        let result = self.store.db().and_then(|db| {
            prefs::save_display_mode(db, &mode).map_err(|err| match err {
                // 输入框旁边只显示原因，不带"输入不合法："前缀。
                zssh_core::error::ZsshError::InvalidInput(reason) => {
                    crate::error_text::reason_text(&reason)
                }
                other => crate::error_text::error_text(&other),
            })
        });
        match result {
            Ok(()) => {
                self.x11_ui.prefs.mode = mode;
                self.x11_ui.custom_error = None;
                self.x11_ui
                    .custom
                    .update(cx, |input, cx| input.set_value(spec, window, cx));
            }
            Err(err) => {
                // 先把界面切到「指定显示」让用户看到错误和输入框。
                self.x11_ui.prefs.mode = mode;
                self.x11_ui.custom_error = Some(err);
            }
        }
        cx.notify();
    }

    pub(crate) fn x11_set_close_owned(&mut self, close: bool, cx: &mut Context<Self>) {
        let result = self.store.db().and_then(|db| {
            prefs::save_close_owned_on_exit(db, close)
                .map_err(|e| crate::error_text::error_text(&e))
        });
        match result {
            Ok(()) => self.x11_ui.prefs.close_owned_on_exit = close,
            Err(err) => {
                tracing::warn!(%err, "保存 X11 退出设置失败");
            }
        }
        cx.notify();
    }

    // ---- 一键安装 ----

    /// 开始下载并安装 VcXsrv（后台；进度在 `x11_ui.phase`）。已经在跑就什么都不做。
    pub(crate) fn x11_start_install(&mut self, cx: &mut Context<Self>) {
        let total = zssh_core::x11::local::install::pinned_source().size;
        {
            let mut phase = self
                .x11_ui
                .phase
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if phase.busy() {
                return;
            }
            *phase = InstallPhase::Downloading { done: 0, total };
        }
        self.x11_ui.cancel.store(false, Ordering::SeqCst);
        let phase = Arc::clone(&self.x11_ui.phase);
        let cancel = Arc::clone(&self.x11_ui.cancel);
        let set = move |next: InstallPhase| {
            *phase
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = next;
        };
        let runtime = match crate::ssh_backend::runtime() {
            Ok(runtime) => runtime,
            Err(err) => {
                set(InstallPhase::Failed(err));
                cx.notify();
                return;
            }
        };
        let progress = {
            let phase = Arc::clone(&self.x11_ui.phase);
            move |done: u64, total: u64| {
                let next = if total > 0 && done >= total {
                    InstallPhase::Installing
                } else {
                    InstallPhase::Downloading { done, total }
                };
                *phase
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = next;
            }
        };
        runtime.spawn(async move {
            match crate::x11_glue::install_vcxsrv(progress, &cancel).await {
                Ok(install) => set(InstallPhase::Done {
                    version: install.version,
                }),
                Err(InstallError::Cancelled) => set(InstallPhase::Cancelled),
                Err(err) => set(InstallPhase::Failed(failure_text(&err))),
            }
        });
        cx.notify();
    }

    /// 取消下载（已经在等 UAC / 安装程序时取消不了）。
    pub(crate) fn x11_cancel_install(&mut self, cx: &mut Context<Self>) {
        if matches!(self.x11_ui.phase_now(), InstallPhase::Downloading { .. }) {
            self.x11_ui.cancel.store(true, Ordering::SeqCst);
        }
        cx.notify();
    }

    /// 节拍：安装进度变了就重绘；装完重新检测本机状态。返回要不要重绘。
    pub(super) fn x11_poll(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let now = self.x11_ui.phase_now();
        if now == self.x11_ui.shown {
            return false;
        }
        let finished = matches!(now, InstallPhase::Done { .. });
        self.x11_ui.shown = now;
        if finished {
            self.x11_reload(window, cx);
            self.notice(t(X11Msg::InstalledNotice), window, cx);
        }
        true
    }

    // ---- 连接前的提示 ----

    /// 连接开了 X11 的主机之前：本机没有可用的 X 服务器就弹提示并拦下这次连接（返回 `true`）。
    pub(crate) fn x11_gate(
        &mut self,
        host_id: i64,
        new_tab: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(host) = self.store.host(host_id) else {
            return false;
        };
        if !host.protocol.x11_enabled() {
            return false;
        }
        let host_name = host.name.clone();
        let prefs = self.x11_load_prefs();
        let status = crate::x11_glue::local_status();
        if server_available(&prefs, &status) {
            return false;
        }
        self.x11_ensure_loaded(window, cx);
        self.modal = Some(Modal::X11Needed(X11Prompt {
            host_id,
            host_name,
            new_tab,
        }));
        cx.notify();
        true
    }

    /// 提示里的三个选择。
    pub(crate) fn x11_choose(
        &mut self,
        choice: X11Choice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Modal::X11Needed(prompt)) = self.modal.take() else {
            return;
        };
        self.focus_content(window, cx);
        match choice {
            X11Choice::Install => {
                self.open_x11_settings(window, cx);
                self.x11_start_install(cx);
            }
            X11Choice::Settings => self.open_x11_settings(window, cx),
            X11Choice::SkipOnce => {
                crate::x11_glue::skip_once(prompt.host_id);
                if prompt.new_tab {
                    self.open_host_in_new_tab_now(prompt.host_id, window, cx);
                } else {
                    self.open_host_now(prompt.host_id, window, cx);
                }
            }
        }
        cx.notify();
    }

    /// 打开设置页的「连接」分类（X11 那一节在里面）。
    pub(crate) fn open_x11_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_view(MainView::Settings, window, cx);
        self.set_settings_section(SettingsSection::Connection, cx);
        self.x11_reload(window, cx);
    }

    // ---- 画：提示 ----

    pub(super) fn x11_prompt_body(
        &self,
        prompt: &X11Prompt,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> ::gpui::AnyElement {
        let entity = cx.entity();
        let install_entity = entity.clone();
        let settings_entity = entity.clone();
        let skip_entity = entity.clone();
        let cancel_entity = entity;
        let can_install = self
            .x11_ui
            .status
            .as_ref()
            .is_none_or(|status| status.can_install);
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .typo(Role::GroupTitle, tokens)
                            .child(t(X11Msg::NeedsXServer)),
                    )
                    .child(div().text_sm().text_color(tokens.text_muted).child(
                        SharedString::from(format!(
                            "{}{}{}",
                            t(X11Msg::HostQuoteOpen),
                            prompt.host_name,
                            t(if cfg!(unix) {
                                X11Msg::PromptSuffixLinux
                            } else {
                                X11Msg::PromptSuffix
                            })
                        )),
                    )),
            )
            .child(
                v_flex()
                    .gap_2()
                    .when(can_install, |d| {
                        d.child(
                            Button::new("x11-install")
                                .primary()
                                .label(t(X11Msg::InstallNow))
                                .w_full()
                                .on_click(move |_, window, cx| {
                                    install_entity.update(cx, |this, cx| {
                                        this.x11_choose(X11Choice::Install, window, cx)
                                    });
                                }),
                        )
                        .child(note(t(X11Msg::InstallNotePrompt), tokens))
                    })
                    .child(
                        Button::new("x11-settings")
                            .outline()
                            .label(t(if cfg!(unix) {
                                X11Msg::SpecifyDisplaySettings
                            } else {
                                X11Msg::AlreadyInstalledSettings
                            }))
                            .w_full()
                            .on_click(move |_, window, cx| {
                                settings_entity.update(cx, |this, cx| {
                                    this.x11_choose(X11Choice::Settings, window, cx)
                                });
                            }),
                    )
                    .child(
                        Button::new("x11-skip")
                            .outline()
                            .label(t(X11Msg::SkipOnce))
                            .w_full()
                            .on_click(move |_, window, cx| {
                                skip_entity.update(cx, |this, cx| {
                                    this.x11_choose(X11Choice::SkipOnce, window, cx)
                                });
                            }),
                    ),
            )
            .child(h_flex().justify_end().child(text_button(
                "modal-cancel",
                t(X11Msg::Cancel),
                ButtonLook::Ghost,
                move |_, window, cx| {
                    cancel_entity.update(cx, |this, cx| this.close_modal(window, cx));
                },
            )))
            .into_any_element()
    }

    // ---- 画：设置页的 X11 一节 ----

    pub(super) fn x11_card(&self, tokens: &Tokens, cx: &mut Context<Self>) -> ::gpui::AnyElement {
        let entity = cx.entity();
        let prefs = &self.x11_ui.prefs;
        let custom = matches!(prefs.mode, DisplayMode::Custom(_));

        // ---- X 显示来源 ----
        let source_entity = entity.clone();
        let mut source = v_flex()
            .w(px(280.0))
            .gap_1p5()
            .items_end()
            .child(choice_group(
                vec![
                    Segment {
                        id: "x11-source-auto".into(),
                        label: t(X11Msg::Auto),
                        icon: None,
                        selected: !custom,
                    },
                    Segment {
                        id: "x11-source-custom".into(),
                        label: t(X11Msg::CustomDisplay),
                        icon: None,
                        selected: custom,
                    },
                ],
                tokens,
                move |index, window, cx| {
                    source_entity.update(cx, |this, cx| {
                        if index == 0 {
                            this.x11_set_auto(cx);
                        } else {
                            this.x11_set_custom(window, cx);
                        }
                    });
                },
            ));
        if custom {
            let save_entity = entity.clone();
            source = source
                .child(
                    h_flex()
                        .w_full()
                        .gap_1()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Input::new(&self.x11_ui.custom)),
                        )
                        .child(
                            Button::new("x11-custom-save")
                                .outline()
                                .compact()
                                .label(t(X11Msg::Save))
                                .on_click(move |_, window, cx| {
                                    save_entity
                                        .update(cx, |this, cx| this.x11_set_custom(window, cx));
                                }),
                        ),
                )
                .children(
                    self.x11_ui
                        .custom_error
                        .clone()
                        .map(|text| error_note(text, tokens.danger)),
                );
        }

        // Linux：本机 X 服务器就是桌面会话的 DISPLAY，没有 VcXsrv 可装、可拉起，只留"显示来源"和一行状态。
        if cfg!(unix) {
            return self.x11_card_session(source, tokens, cx);
        }

        // ---- 退出时关闭 ----
        let close_entity = entity.clone();
        let close_row = row(
            t(X11Msg::CloseOwnedTitle),
            Some(t(X11Msg::CloseOwnedDescription)),
            Switch::new("x11-close-owned")
                .checked(prefs.close_owned_on_exit)
                .accessibility_label(t(X11Msg::CloseOwnedTitle))
                .on_click(move |checked: &bool, _window, cx| {
                    let checked = *checked;
                    close_entity.update(cx, |this, cx| this.x11_set_close_owned(checked, cx));
                }),
            tokens,
        );

        // ---- VcXsrv 状态与安装 ----
        let phase = self.x11_ui.phase_now();
        let status = self.x11_ui.status.clone();
        let headline = status
            .as_ref()
            .map_or_else(|| t(X11Msg::Detecting).to_owned(), status_headline);
        let installed = status
            .as_ref()
            .is_some_and(|status| status.vcxsrv.is_some() || !status.running_displays.is_empty());
        let can_install = status.as_ref().is_none_or(|status| status.can_install);
        let install_entity = entity.clone();
        let cancel_entity = entity.clone();
        let recheck_entity = entity.clone();
        let mut controls = h_flex().gap_1().items_center();
        if phase.busy() {
            controls = controls.child(
                Button::new("x11-cancel-install")
                    .outline()
                    .compact()
                    .label(t(X11Msg::Cancel))
                    .disabled(matches!(phase, InstallPhase::Installing))
                    .on_click(move |_, _window, cx| {
                        cancel_entity.update(cx, |this, cx| this.x11_cancel_install(cx));
                    }),
            );
        } else {
            controls = controls
                .child(
                    Button::new("x11-recheck")
                        .ghost()
                        .compact()
                        .label(t(X11Msg::Recheck))
                        .on_click(move |_, window, cx| {
                            recheck_entity.update(cx, |this, cx| this.x11_reload(window, cx));
                        }),
                )
                .when(!installed && can_install, |d| {
                    d.child(
                        Button::new("x11-install-button")
                            .primary()
                            .compact()
                            .label(t(X11Msg::InstallNow))
                            .on_click(move |_, _window, cx| {
                                install_entity.update(cx, |this, cx| this.x11_start_install(cx));
                            }),
                    )
                });
        }
        let status_row = row_with(
            "VcXsrv",
            Some(SharedString::from(format!(
                "{}{headline}",
                t(X11Msg::StatusPrefix)
            ))),
            controls,
            tokens,
        );
        let mut notes: Vec<::gpui::AnyElement> = Vec::new();
        if let Some(text) = progress_text(&phase) {
            notes.push(
                div()
                    .id("x11-progress")
                    .text_xs()
                    .text_color(tokens.accent)
                    .child(SharedString::from(text))
                    .into_any_element(),
            );
        }
        match &phase {
            InstallPhase::Failed(reason) => notes.push(
                error_note(
                    format!("{}{reason}", t(X11Msg::InstallFailedPrefix)),
                    tokens.danger,
                )
                .into_any_element(),
            ),
            InstallPhase::Cancelled => {
                notes.push(note(t(X11Msg::CancelledInstallPeriod), tokens).into_any_element());
            }
            _ => {}
        }
        if !installed && !phase.busy() {
            let size = zssh_core::x11::local::install::pinned_source().size;
            notes.push(
                note(
                    format!(
                        "{}{}{}",
                        t(X11Msg::InstallNotePrefix),
                        format_mb(size),
                        t(X11Msg::InstallNoteSuffix)
                    ),
                    tokens,
                )
                .into_any_element(),
            );
        }

        card(
            "settings-x11",
            t(X11Msg::X11Forwarding),
            Some(t(X11Msg::X11CardDescription)),
            tokens,
            {
                let mut rows = vec![
                    row(
                        t(X11Msg::SourceTitle),
                        Some(t(X11Msg::SourceDescription)),
                        source,
                        tokens,
                    ),
                    close_row,
                    status_row,
                ];
                rows.extend(notes);
                rows
            },
        )
    }

    /// Linux 上的 X11 一节：X 显示来源 + 当前会话的显示状态（没有 VcXsrv 的检测 / 安装 / 退出时关闭）。
    fn x11_card_session(
        &self,
        source: impl IntoElement,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> ::gpui::AnyElement {
        let recheck_entity = cx.entity();
        let headline = self
            .x11_ui
            .status
            .as_ref()
            .map_or_else(|| t(X11Msg::Detecting).to_owned(), session_headline);
        let status_row = row_with(
            t(X11Msg::SessionServerTitle),
            Some(SharedString::from(format!(
                "{}{headline}",
                t(X11Msg::StatusPrefix)
            ))),
            Button::new("x11-recheck")
                .ghost()
                .compact()
                .label(t(X11Msg::Recheck))
                .on_click(move |_, window, cx| {
                    recheck_entity.update(cx, |this, cx| this.x11_reload(window, cx));
                }),
            tokens,
        );
        card(
            "settings-x11",
            t(X11Msg::X11Forwarding),
            Some(t(X11Msg::X11CardDescription)),
            tokens,
            vec![
                row(
                    t(X11Msg::SourceTitle),
                    Some(t(X11Msg::SourceDescriptionLinux)),
                    source,
                    tokens,
                ),
                status_row,
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use zssh_core::x11::local::VcXsrvInstall;

    use super::{
        DisplayMode, InstallError, InstallPhase, LocalXStatus, X11Prefs, failure_text,
        progress_text, server_available, session_headline, status_headline,
    };

    fn status(installed: Option<&str>, running: &[u16], owned: Option<u16>) -> LocalXStatus {
        LocalXStatus {
            vcxsrv: installed.map(|version| VcXsrvInstall {
                exe: PathBuf::from("vcxsrv.exe"),
                version: (!version.is_empty()).then(|| version.to_owned()),
            }),
            running_displays: running.to_vec(),
            owned_display: owned,
            can_install: true,
            session_display: None,
        }
    }

    #[test]
    fn the_prompt_is_needed_only_when_no_x_server_can_be_used() {
        let auto = X11Prefs::default();
        // 什么都没有：要提示。
        assert!(!server_available(&auto, &status(None, &[], None)));
        // 装了 VcXsrv（连接时由 ZeeTerm 拉起）、有别的 X 服务器在跑、ZeeTerm 自己拉起过：都不用提示。
        assert!(server_available(&auto, &status(Some("21"), &[], None)));
        assert!(server_available(&auto, &status(None, &[0], None)));
        assert!(server_available(&auto, &status(None, &[], Some(20))));
        // 设置里指定了显示：用户自己管理，不提示。
        let custom = X11Prefs {
            mode: DisplayMode::Custom("localhost:0".to_owned()),
            ..X11Prefs::default()
        };
        assert!(server_available(&custom, &status(None, &[], None)));
    }

    #[test]
    fn a_session_display_counts_as_a_usable_server_and_is_named_in_the_status_line() {
        let none = status(None, &[], None);
        let session = LocalXStatus {
            session_display: Some(":1".to_owned()),
            can_install: false,
            ..none.clone()
        };
        let auto = X11Prefs::default();
        assert!(!server_available(&auto, &none));
        assert!(server_available(&auto, &session));
        assert!(session_headline(&session).ends_with(":1"));
        assert!(session_headline(&session).contains("当前会话"));
        let missing = session_headline(&none);
        assert!(
            missing.contains("DISPLAY") && missing.contains("Xwayland"),
            "{missing}"
        );
    }

    #[test]
    fn the_status_line_says_not_installed_installed_or_running() {
        assert_eq!(status_headline(&status(None, &[], None)), "未安装");
        assert_eq!(
            status_headline(&status(Some("21.1.16"), &[], None)),
            "已安装 21.1.16"
        );
        assert_eq!(status_headline(&status(Some(""), &[], None)), "已安装");
        assert_eq!(
            status_headline(&status(Some("21"), &[0], None)),
            "运行中（显示 :0）"
        );
        assert!(status_headline(&status(Some("21"), &[0], Some(20))).contains("ZeeTerm 启动的"));
    }

    #[test]
    fn progress_text_covers_download_install_and_idle() {
        assert_eq!(progress_text(&InstallPhase::Idle), None);
        assert_eq!(progress_text(&InstallPhase::Cancelled), None);
        let downloading = InstallPhase::Downloading {
            done: 10 * 1_048_576,
            total: 40 * 1_048_576,
        };
        let text = progress_text(&downloading).expect("下载中有进度");
        assert!(text.contains("10.0 MB / 40.0 MB（25%）"), "{text}");
        assert!(
            progress_text(&InstallPhase::Installing)
                .expect("安装中有说明")
                .contains("用户账户控制")
        );
        assert!(downloading.busy() && InstallPhase::Installing.busy());
        assert!(!InstallPhase::Failed("x".to_owned()).busy());
        assert!(!InstallPhase::Done { version: None }.busy());
    }

    #[test]
    fn failures_and_cancellation_have_readable_reasons() {
        assert_eq!(failure_text(&InstallError::Cancelled), "已取消安装");
        assert!(failure_text(&InstallError::UacDenied).contains("没有安装"));
        assert!(failure_text(&InstallError::Verify("校验没过".to_owned())).contains("校验没过"));
    }
}
