//! 终端工作区：标签、窗格、分屏、广播、选区/粘贴/鼠标上报、高亮扫描，以及这些功能的渲染。
//!
//! 它是一个可复用的 entity（`TerminalWorkspace`）：主窗口 `MainWindow` 和调试用的
//! `TerminalDemo` 窗口各持有一个，两边用的是同一份代码。V2-F1 修过的流控、Ctrl+C、
//! 断线提示、异步连接等逻辑都在 [`Pane`] 和下面的输入处理里，抽取时原样搬过来，
//! 没有改动语义。
//!
//! 工作区自己不画标题栏、标签条、主机树，也不知道主机库：谁要用它，就在外面套自己的
//! 外壳，通过 `open_host_tab` / `new_empty_tab` / `duplicate_tab` 等方法开标签。
//! 网格在窗口里的位置由 `canvas` 量出来（上一帧的结果），不再写死窗口坐标。

use ::gpui::FontWeight;
use ::gpui::{
    Anchor, Bounds, ClipboardItem, DismissEvent, FocusHandle, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, ScrollDelta, ScrollEvent, Subscription,
    Task, Window, anchored, canvas, deferred,
};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::menu::PopupMenu;
use gpui_kit::component::slider::{SliderEvent, SliderState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use zssh_core::ssh::flow::FlowGate;
use zssh_core::ssh::latency::{self, LatencyMonitor, LatencySnapshot, LatencyViewer};
use zssh_session::{
    CoreFilter, FRAME_BYTE_BUDGET, FrameScheduler, KeyInput, LoopbackBackend, MouseAction,
    MouseButton as TerminalMouseButton, MouseInput, PollOutcome, SessionBackend, SessionCommand,
    SessionDriver, SessionPump, encode_key, encode_mouse,
};
use zssh_terminal::{MouseMode, NamedColor, Size, Snapshot, TerminalColor};

use crate::i18n::{Msg, t};

use crate::highlight::{
    EngineHandle, HighlightRule, HighlightScheduler, MAX_RULES as MAX_HIGHLIGHT_RULES, ScanKey,
    Span,
};
use crate::settings::{
    CursorShape, DEFAULT_FONT_SIZE, MAX_FONT_SIZE, MAX_LINE_HEIGHT, MAX_SCROLLBACK, MIN_FONT_SIZE,
    MIN_LINE_HEIGHT, MIN_SCROLLBACK, UiSettings,
};
use crate::settings_store::AppSettings;
use crate::sftp::{SftpDrag, terminal_insert_text};
use crate::split_tree::{self, Axis, Layout, Node, PaneId};
use crate::tab_set::TabSet;

/// 终端页状态栏、粘贴确认等文案。
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // 有些键只在测试 / 调试构建里用到
enum TermMsg {
    Connecting,
    Reconnecting,
    Disconnected,
    NoReconnectOffline,
    PasteNoPane,
    PasteCancelled,
    PasteTargetBroadcastPrefix,
    PasteTargetBroadcastSuffix,
    PasteTargetCurrent,
    PasteConfirmPrefix,
    PasteConfirmSuffix,
    PasteWarning,
    PasteMorePrefix,
    PasteMoreSuffix,
    Cancel,
    Paste,
    UnknownReason,
    DisconnectedBadge,
    ReconnectHint,
    TabTitlePrefix,
    NewTab,
    OfflineDemo,
    SettingsSaveFailedPrefix,
    HiddenPassword,
    Reconnected,
}

impl crate::i18n::Msg for TermMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::Connecting => "正在连接…",
            Self::Reconnecting => "正在重新连接…",
            Self::Disconnected => "已断开连接",
            Self::NoReconnectOffline => "离线标签没有可重连的会话",
            Self::PasteNoPane => "没有已连接的窗格，无法粘贴",
            Self::PasteCancelled => "会话已变化，已取消粘贴，请重新粘贴",
            Self::PasteTargetBroadcastPrefix => "会发送到 ",
            Self::PasteTargetBroadcastSuffix => " 个窗格（广播）",
            Self::PasteTargetCurrent => "会发送到当前窗格",
            Self::PasteConfirmPrefix => "粘贴 ",
            Self::PasteConfirmSuffix => " 行内容？",
            Self::PasteWarning => "内容带换行，对方 shell 没开括号粘贴模式时会直接执行。",
            Self::PasteMorePrefix => "共 ",
            Self::PasteMoreSuffix => " 行，这里只显示前面一部分",
            Self::Cancel => "取消",
            Self::Paste => "粘贴",
            Self::UnknownReason => "未知原因",
            Self::DisconnectedBadge => "已断开",
            Self::ReconnectHint => "按 R 重连，或者点这里",
            Self::TabTitlePrefix => "终端 #",
            Self::NewTab => "新标签",
            Self::OfflineDemo => "离线演示",
            Self::SettingsSaveFailedPrefix => "设置没能保存（会自动重试）：",
            Self::HiddenPassword => "<已隐藏>",
            Self::Reconnected => "已重新连接",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::Connecting => "Connecting…",
            Self::Reconnecting => "Reconnecting…",
            Self::Disconnected => "Disconnected",
            Self::NoReconnectOffline => "This offline tab has no session to reconnect",
            Self::PasteNoPane => "No connected pane — cannot paste",
            Self::PasteCancelled => "The session changed; the paste was cancelled — paste again",
            Self::PasteTargetBroadcastPrefix => "This will be sent to ",
            Self::PasteTargetBroadcastSuffix => " panes (broadcast)",
            Self::PasteTargetCurrent => "This will be sent to the active pane",
            Self::PasteConfirmPrefix => "Paste ",
            Self::PasteConfirmSuffix => " lines?",
            Self::PasteWarning => {
                "The content contains line breaks; if the remote shell does not enable bracketed paste, it will execute immediately."
            }
            Self::PasteMorePrefix => "Total ",
            Self::PasteMoreSuffix => " lines — only the first part is shown",
            Self::Cancel => "Cancel",
            Self::Paste => "Paste",
            Self::UnknownReason => "Unknown reason",
            Self::DisconnectedBadge => "Disconnected",
            Self::ReconnectHint => "Press R to reconnect, or click here",
            Self::TabTitlePrefix => "Terminal #",
            Self::NewTab => "New tab",
            Self::OfflineDemo => "Offline demo",
            Self::SettingsSaveFailedPrefix => {
                "Settings could not be saved (will retry automatically): "
            }
            Self::HiddenPassword => "<hidden>",
            Self::Reconnected => "Reconnected",
        }
    }
}

/// 每个窗格四周的内边距（0.2.14 的 `p-2`）：文字和光标不贴边，焦点线也不压在字上。
const PANE_PAD: f32 = 8.0;

/// 还没量到区域、也没有窗口信息时，新标签用的初始尺寸。
const DEFAULT_SIZE: Size = Size {
    rows: 30,
    cols: 120,
};
mod assist_ops;
#[cfg(test)]
mod assist_tests;
mod dev_tools;
mod highlight_ops;
mod ime_ops;
mod search_ops;
mod settings_ops;
#[cfg(test)]
mod suggest_flow_tests;
#[cfg(test)]
mod suggest_tests;
mod suggest_ui;

pub(crate) use assist_ops::{AssistView, IntegrationError, IntegrationState, StopOutcome};
use assist_ops::{PaneAssist, WriteOrigin, classify_key};
pub(crate) use suggest_ui::binding_text;
use zssh_core::terminal_assist::suggest::QuickSource;
use zssh_core::terminal_assist::{RejectReason, TrackerContext, WriteOutcome, WriteSource};

/// 窗格的会话状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaneState {
    /// 正常：离线演示窗格，或已连上的 SSH 会话。
    Live,
    /// 后台正在连接/开 shell，界面只显示crate::i18n::t(TermMsg::Connecting)。
    Connecting,
    /// 连接失败或已断开：输入不再发出，等用户重连。
    Disconnected,
    /// 空白标签（「+」新建、还没选主机）：没有会话，输入全部丢弃，也不显示任何提示。
    Empty,
}

/// 连接中/已断开的窗格用的写端：丢掉一切输入。
/// 不能退化成回环后端，否则键入的内容（包括密码）会在屏幕上明文回显。
struct NullBackend;

impl SessionBackend for NullBackend {
    fn send(&mut self, _command: SessionCommand) {}
}

/// 去掉控制字符：错误和断线原因里可能带远端可控的文字，不能让它在终端里夹带转义序列。
fn plain(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_control()).collect()
}

// 窗格里只留一行"发生了什么"当回滚记录；怎么重连由断线遮罩说（V2-U30），这里不再重复操作提示。

/// 断线、会话日志这些直接写进终端 / 状态栏的文案（两语言各一份）。
#[derive(Clone, Copy, Debug)]
enum ConnMsg {
    ManualNotice,
    ManualReason,
    NoticeHead,
    NoticeReasonPrefix,
    NoticeExitPrefix,
    NoticeExitSuffix,
    NoticeRemoteClosed,
    ReasonExitPrefix,
    ReasonRemoteClosed,
    LogStartedPrefix,
    LogStoppedPrefix,
    LogStopped,
    LogAutoPrefix,
    LogAutoSuffix,
    LogFailedPrefix,
}

impl Msg for ConnMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::ManualNotice => "已断开连接（手动）",
            Self::ManualReason => "已手动断开连接",
            Self::NoticeHead => "连接已断开",
            Self::NoticeReasonPrefix => "：",
            Self::NoticeExitPrefix => "（退出码 ",
            Self::NoticeExitSuffix => "）",
            Self::NoticeRemoteClosed => "（远端关闭了连接）",
            Self::ReasonExitPrefix => "退出码 ",
            Self::ReasonRemoteClosed => "远端关闭了连接",
            Self::LogStartedPrefix => "开始记录，日志保存在 ",
            Self::LogStoppedPrefix => "日志已停止：",
            Self::LogStopped => "日志已停止",
            Self::LogAutoPrefix => "正在记录（自动）：日志保存在 ",
            Self::LogAutoSuffix => "；要停止请在设置里关闭「记录会话日志」",
            Self::LogFailedPrefix => "无法开始记录：",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::ManualNotice => "Disconnected (manually)",
            Self::ManualReason => "Disconnected manually",
            Self::NoticeHead => "Disconnected",
            Self::NoticeReasonPrefix => ": ",
            Self::NoticeExitPrefix => " (exit code ",
            Self::NoticeExitSuffix => ")",
            Self::NoticeRemoteClosed => " (closed by the remote side)",
            Self::ReasonExitPrefix => "Exit code ",
            Self::ReasonRemoteClosed => "Remote closed the connection",
            Self::LogStartedPrefix => "Recording. Log file: ",
            Self::LogStoppedPrefix => "Logging stopped: ",
            Self::LogStopped => "Logging stopped",
            Self::LogAutoPrefix => "Recording (automatic): ",
            Self::LogAutoSuffix => "; turn off \"Session logging\" in Settings to stop",
            Self::LogFailedPrefix => "Could not start logging: ",
        }
    }
}

/// 本机时间 `HH:MM:SS`（重连分隔行用）。
fn local_clock() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
        + crate::datetime::local_utc_offset();
    let day = secs.rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", day / 3600, day % 3600 / 60, day % 60)
}

/// 重连后插在旧输出和新会话输出之间的暗色分隔行（本地写入，不发给远端）。光标不在行首时先换行。
fn reconnect_marker(at_line_start: bool, clock: &str) -> String {
    let lead = if at_line_start { "" } else { "\r\n" };
    format!(
        "{lead}\x1b[90m── {} {clock} ──\x1b[0m\r\n",
        t(TermMsg::Reconnected)
    )
}

/// 用户主动断开时窗格里显示的文字。
fn user_disconnect_notice() -> String {
    format!("\r\n\x1b[33m{}\x1b[0m\r\n", t(ConnMsg::ManualNotice))
}

/// 用户主动断开时遮罩上写的原因。
fn user_disconnect_reason() -> &'static str {
    t(ConnMsg::ManualReason)
}

/// 连接失败时窗格里显示的文字。
fn failure_notice(error: &str) -> String {
    format!("\r\n\x1b[31m{}\x1b[0m\r\n", plain(error))
}

/// 会话结束时窗格里显示的文字：有原因写原因，否则写退出码。
fn disconnect_notice(exit: &zssh_session::SessionExit) -> String {
    let detail = match (&exit.reason, exit.code) {
        (Some(reason), _) => format!("{}{}", t(ConnMsg::NoticeReasonPrefix), plain(reason)),
        (None, Some(code)) => format!(
            "{}{code}{}",
            t(ConnMsg::NoticeExitPrefix),
            t(ConnMsg::NoticeExitSuffix)
        ),
        (None, None) => t(ConnMsg::NoticeRemoteClosed).to_owned(),
    };
    format!("\r\n\x1b[33m{}{detail}\x1b[0m\r\n", t(ConnMsg::NoticeHead))
}

/// 断线遮罩上写的原因：和 [`disconnect_notice`] 同一套判断，只是不带"连接已断开"的前缀。
fn disconnect_reason(exit: &zssh_session::SessionExit) -> String {
    match (&exit.reason, exit.code) {
        (Some(reason), _) => plain(reason),
        (None, Some(code)) => format!("{}{code}", t(ConnMsg::ReasonExitPrefix)),
        (None, None) => t(ConnMsg::ReasonRemoteClosed).to_owned(),
    }
}

/// 按键是不是"在断线遮罩上按 R 重连"：只认不带任何修饰键的 R（Ctrl/Alt/Shift/Win+R 留给别的快捷键）。
fn is_reconnect_key(key: &str, modifiers: &::gpui::Modifiers) -> bool {
    key.eq_ignore_ascii_case("r") && !modifiers.modified()
}

/// 快速连接（`user@host:port` + 密码，不进主机库）的目标。密码只留在内存里，给重连和复制标签用
/// （0.2.14 的标签来源 `quick` 也带着密码）；手写 `Debug`，免得它进日志。
pub(crate) struct QuickSpec {
    pub target: zssh_core::model::QuickTarget,
    pub password: String,
    /// 标签上显示的名字（就是用户输入的那串）。
    pub label: String,
}

impl std::fmt::Debug for QuickSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuickSpec")
            .field("label", &self.label)
            .field("password", &crate::i18n::t(TermMsg::HiddenPassword))
            .finish()
    }
}

pub(crate) struct Pane {
    /// 分屏树的叶子只记它；重连换掉整个窗格时要沿用旧编号。
    pub(crate) id: PaneId,
    driver: SessionDriver<CoreFilter>,
    backend: Box<dyn SessionBackend>,
    pub(crate) snapshot: Snapshot,
    pub(crate) fed: usize,
    /// 从主机库打开的会话记住主机和已认证连接，复制标签时复用。
    pub(crate) host_id: Option<i64>,
    /// 快速连接的窗格（没有主机记录，靠这个记着连哪儿）。
    pub(crate) quick: Option<Arc<QuickSpec>>,
    pub(crate) factory: Option<Arc<crate::ssh_backend::SshSessionFactory>>,
    pub(crate) state: PaneState,
    /// 后台连接任务；窗格被关掉时随它一起取消。
    pending: Option<crate::ssh_backend::PendingOpen>,
    /// 最近一次交给后端的尺寸（"已排队"）；本地尺寸是 `snapshot.size`，两者不同只会发生在连接
    /// 期间（还没有后端可交），连上后要补发一次。后端把尺寸当"最新状态"保存、保证最新值最终送达，
    /// 输入堵塞不会让它丢（见 `SshBackend` / `StreamBackend`）。
    open_size: Size,
    history: usize,
    generation: u64,
    /// 这个窗格的 shell 是第几个在服务器上打开的（全进程递增）：同一条连接上有几个终端时，
    /// 跟随终端目录靠它和服务器上按打开顺序排的 shell 对号。
    shell_seq: u64,
    /// 这个窗格的 shell 在 AI 终端登记表里的编号（V2-G10b）；没连上 / 离线回环窗格没有。
    terminal_id: Option<String>,
    /// 右上角延迟徽标的数据来源；串口、经跳板机的 Telnet、没连上的窗格没有。
    latency: Option<PaneLatency>,
    /// 这个窗格的会话日志是怎么来的（日志本身挂在泵上）。
    log_mode: LogMode,
    /// 断开 / 连接失败的原因（只有 `Disconnected` 时有值），断线遮罩上显示；重连会换掉整个窗格，所以不用手动清。
    disconnect_reason: Option<String>,
    /// 这个窗格是重连出来的：连上时终端（屏幕 + 回滚）沿用重连前的，只换连接。连上（或放弃）后清掉。
    carry_terminal: bool,
    /// 输入跟踪：只旁路观察，不改变任何按键行为（见 `assist_ops`）。
    assist: PaneAssist,
}

/// 窗格会话日志的来源。写入位置只有一个——设置里的会话日志目录；区别只在谁开的：
/// 设置里的开关对所有会话自动开（`Auto`），还是用户对这个窗格手动开的（`Manual`）。
/// 自动开着时不会再另开第二份。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum LogMode {
    #[default]
    Off,
    Auto,
    Manual,
}

/// 状态提示的类别：界面据此决定怎么提示（错误用错误样式的通知，普通状态用普通通知）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatusLevel {
    /// 普通状态（日志续记到哪、不能执行的原因）。
    Info,
    /// 失败：用户的操作没做成，或后台写盘失败。
    Error,
}

/// 要弹给用户看的一条状态提示。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StatusNotice {
    pub(crate) text: String,
    pub(crate) level: StatusLevel,
}

/// 终端页的一行状态。release 里没有常驻的状态栏位置，所以需要用户看见的提示（`announce`）
/// 由外壳（主窗口）取走后用通知弹出来；界面上本来就有表现的（重新连接中、已断开）只记下文字（`set`），
/// 不再重复弹。后写的覆盖先写的：同一刻连着写两句，用户只需要看到最后那句。
#[derive(Debug)]
struct StatusLine {
    text: String,
    level: StatusLevel,
    /// 这句还没被外壳取走、且需要弹出来。
    pending: bool,
}

impl StatusLine {
    fn new() -> Self {
        Self {
            text: String::new(),
            level: StatusLevel::Info,
            pending: false,
        }
    }

    /// 只记下，不弹通知。
    fn set(&mut self, level: StatusLevel, text: impl Into<String>) {
        self.text = text.into();
        self.level = level;
        self.pending = false;
    }

    /// 记下并要求外壳弹出来。
    fn announce(&mut self, level: StatusLevel, text: impl Into<String>) {
        self.text = text.into();
        self.level = level;
        self.pending = true;
    }

    /// 外壳取走待弹的提示；取过一次就不会再有，直到下一次 `announce`。
    fn take_notice(&mut self) -> Option<StatusNotice> {
        if !std::mem::take(&mut self.pending) {
            return None;
        }
        Some(StatusNotice {
            text: self.text.clone(),
            level: self.level,
        })
    }
}

/// 点「记录」的结果。
#[derive(Debug)]
enum LogOutcome {
    Started {
        dir: std::path::PathBuf,
    },
    /// `path` 是 `None` 说明日志线程还没来得及建出文件（或建不出来，原因在设置页的会话日志一栏）。
    Stopped {
        path: Option<std::path::PathBuf>,
    },
    /// 设置里已经对所有会话自动记录：不另开一份，也不在这里停（要停就去设置里关）。
    Auto {
        dir: std::path::PathBuf,
    },
    Failed(String),
}

impl LogOutcome {
    /// 开不了日志是失败，其余只是告知。
    fn level(&self) -> StatusLevel {
        match self {
            Self::Failed(_) => StatusLevel::Error,
            _ => StatusLevel::Info,
        }
    }

    /// 状态栏上的一句话；路径过 `redact_paths`，用户目录显示成占位。
    fn status_text(&self) -> String {
        use crate::agent_text::redact_paths;
        let shown = |path: &std::path::Path| redact_paths(&path.display().to_string());
        match self {
            Self::Started { dir } => {
                format!("{}{}", t(ConnMsg::LogStartedPrefix), shown(dir))
            }
            Self::Stopped { path: Some(path) } => {
                format!("{}{}", t(ConnMsg::LogStoppedPrefix), shown(path))
            }
            Self::Stopped { path: None } => t(ConnMsg::LogStopped).to_owned(),
            Self::Auto { dir } => format!(
                "{}{}{}",
                t(ConnMsg::LogAutoPrefix),
                shown(dir),
                t(ConnMsg::LogAutoSuffix)
            ),
            Self::Failed(err) => {
                format!("{}{}", t(ConnMsg::LogFailedPrefix), redact_paths(err))
            }
        }
    }
}

/// 窗格对延迟监视器的登记：`viewer` 报告这个窗格现在看不看得见（都看不见时后台测量暂停），
/// `label` 是悬停提示里写的主机名。
pub(crate) struct PaneLatency {
    viewer: LatencyViewer,
    label: String,
}

/// 下一个窗格编号（全进程唯一：标签可以在窗口之间搬）。
fn next_pane_id() -> PaneId {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// 下一个连接代次（全进程唯一）。每个窗格的每次（重）连接都拿一个新的：迟到的事件、
/// 终端协议回复、待确认的粘贴靠它认出"这已经不是当初那条连接了"。
fn next_generation() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// 下一个 shell 的序号。
fn next_shell_seq() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl Pane {
    fn new(size: Size, history: usize, generation: u64) -> Self {
        let pump = SessionPump::new(
            size,
            history,
            Arc::new(FlowGate::new()),
            CoreFilter::new(),
            generation,
        );
        let (backend, events) = LoopbackBackend::new(generation);
        let mut pane = Self::with_parts(pump, Box::new(backend), events, None, None);
        pane.history = history;
        pane
    }

    fn with_parts(
        pump: SessionPump<CoreFilter>,
        backend: Box<dyn SessionBackend>,
        events: Receiver<zssh_session::SessionEvent>,
        host_id: Option<i64>,
        factory: Option<Arc<crate::ssh_backend::SshSessionFactory>>,
    ) -> Self {
        let snapshot = pump.snapshot();
        let generation = pump.generation();
        let id = next_pane_id();
        Self {
            id,
            assist: PaneAssist::new(TrackerContext {
                pane_id: id,
                connection_generation: generation,
                target: String::new(),
                broadcast_revision: 0,
            }),
            open_size: snapshot.size,
            driver: SessionDriver::new(pump, events),
            backend,
            snapshot,
            fed: 0,
            host_id,
            quick: None,
            factory,
            state: PaneState::Live,
            pending: None,
            history: crate::settings::DEFAULT_SCROLLBACK,
            generation,
            shell_seq: next_shell_seq(),
            terminal_id: None,
            latency: None,
            log_mode: LogMode::Off,
            disconnect_reason: None,
            carry_terminal: false,
        }
    }

    /// 没有活会话的占位窗格：输入全部丢弃，事件通道是空的（发送端已丢）。
    /// 连接中和失败/断开两种状态共用它。
    fn placeholder(
        size: Size,
        history: usize,
        generation: u64,
        host_id: Option<i64>,
        state: PaneState,
    ) -> Self {
        let pump = SessionPump::new(
            size,
            history,
            Arc::new(FlowGate::new()),
            CoreFilter::new(),
            generation,
        );
        let (_, events) = std::sync::mpsc::channel();
        let mut pane = Self::with_parts(pump, Box::new(NullBackend), events, host_id, None);
        pane.state = state;
        pane.history = history;
        pane.generation = generation;
        pane
    }

    /// 后台开始连接：窗格立刻可见，显示crate::i18n::t(TermMsg::Connecting)，结果在 `poll_events` 里接上。
    fn connecting(
        size: Size,
        history: usize,
        generation: u64,
        host_id: Option<i64>,
        pending: crate::ssh_backend::PendingOpen,
    ) -> Self {
        let mut pane = Self::placeholder(size, history, generation, host_id, PaneState::Connecting);
        pane.pending = Some(pending);
        pane.pump_mut()
            .feed_local(format!("{}\r\n", crate::i18n::t(TermMsg::Connecting)).as_bytes());
        pane.refresh();
        pane
    }

    /// 开新窗格：有已认证连接就在同一条连接上再开 shell，否则按主机库重新连接；
    /// 都没有就是离线回环窗格。连接都在后台做，这里不阻塞界面线程。
    fn start_ssh(
        factory: Option<Arc<crate::ssh_backend::SshSessionFactory>>,
        host_id: Option<i64>,
        size: Size,
        history: usize,
        generation: u64,
    ) -> Self {
        if let Some(factory) = factory {
            let pending = crate::ssh_backend::begin_open(factory, size, generation);
            return Self::connecting(size, history, generation, host_id, pending);
        }
        let Some(host_id) = host_id else {
            return Self::new(size, history, generation);
        };
        let pending = crate::ssh_backend::begin_connect(host_id, size, generation);
        Self::connecting(size, history, generation, Some(host_id), pending)
    }

    /// 快速连接的窗格：有已认证连接就在同一条连接上再开 shell，否则按地址 + 密码连；都在后台做。
    fn start_quick(
        spec: Arc<QuickSpec>,
        factory: Option<Arc<crate::ssh_backend::SshSessionFactory>>,
        size: Size,
        history: usize,
        generation: u64,
    ) -> Self {
        let pending = match factory {
            Some(factory) => crate::ssh_backend::begin_open(factory, size, generation),
            None => crate::ssh_backend::begin_connect_quick(
                &spec.target,
                spec.password.clone(),
                size,
                generation,
            ),
        };
        let mut pane = Self::connecting(size, history, generation, None, pending);
        pane.quick = Some(spec);
        pane
    }

    /// 会话日志文件名 / 延迟提示里用的名字：主机库里的主机用它的名字，快速连接用它的标签。
    ///
    /// 名字会随库变化，但附着、延迟标签这些路径会反复问（审查 M-11：同步开库会把
    /// 界面卡住），所以按主机 id 缓存一份，进程里每台主机只查一次。
    fn log_label(&self) -> String {
        use std::collections::HashMap;
        use std::sync::{Mutex, OnceLock};

        static HOST_NAMES: OnceLock<Mutex<HashMap<i64, String>>> = OnceLock::new();

        if let Some(spec) = &self.quick {
            return spec.label.clone();
        }
        let Some(id) = self.host_id else {
            return "session".to_owned();
        };
        let cache = HOST_NAMES.get_or_init(|| Mutex::new(HashMap::new()));
        if let Some(name) = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&id)
            .cloned()
        {
            return name;
        }
        let name = zssh_core::store::Database::open(&crate::settings::data_dir().join("zssh.db"))
            .ok()
            .and_then(|db| db.host_get(id).ok())
            .map(|host| host.name)
            .unwrap_or_else(|| "session".to_owned());
        cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, name.clone());
        name
    }

    /// 这个窗格是不是一个会话（主机库里的主机，或者快速连接）。
    fn is_session(&self) -> bool {
        self.host_id.is_some() || self.quick.is_some()
    }

    /// 连上了：换成真实会话的泵、写端和事件流，保留当前的尺寸和正在记录的日志。
    fn attach(
        &mut self,
        backend: Box<dyn SessionBackend>,
        events: Receiver<zssh_session::SessionEvent>,
        gate: Arc<FlowGate>,
        factory: Option<Arc<crate::ssh_backend::SshSessionFactory>>,
    ) {
        let size = self.snapshot.size;
        let log = self.pump_mut().take_plain_log();
        let mut pump =
            SessionPump::new(size, self.history, gate, CoreFilter::new(), self.generation);
        if std::mem::take(&mut self.carry_terminal) {
            // 重连：新泵接过旧终端，分隔行写在旧输出和新会话的输出之间（连上了才写，失败的尝试不留"已重新连接"）。
            pump.adopt_terminal(self.pump_mut());
            let at_line_start = pump.snapshot().cursor_col == 0;
            pump.feed_local(reconnect_marker(at_line_start, &local_clock()).as_bytes());
        }
        if let Some(log) = log {
            // 连接期间手动开的日志接着用，不再另开。
            pump.set_plain_log(log);
        } else if self.is_session() {
            // 设置从进程缓存拿：附着路径在 UI 线程上，不能每次开库（审查 M-11）。
            let prefs = crate::app_prefs::cached();
            if let Some(plain) = crate::app_prefs::open_session_log_with(&prefs, &self.log_label())
            {
                // 设置里开了会话日志：这个会话一个文件（每次连上 / 重连各一个）。
                pump.set_plain_log(plain);
                self.log_mode = LogMode::Auto;
            }
        }
        self.driver = SessionDriver::new(pump, events);
        // 告诉 AI 终端登记表：这个 shell 属于哪台主机（快速连接没有主机记录，登记表里就没有主机）。
        self.terminal_id = backend.terminal_id();
        if let Some(id) = &self.terminal_id {
            crate::ai_terminals::terminals().describe(id, self.host_id);
        }
        self.backend = backend;
        self.factory = factory;
        self.state = PaneState::Live;
        self.disconnect_reason = None;
        // 新连接：旧泵里的标记、旧会话的输入状态都不再有意义。
        self.assist.on_connection_reset(Instant::now());
        self.shell_seq = next_shell_seq();
        if size != self.open_size {
            self.backend.send(SessionCommand::Resize(size));
            self.open_size = size;
        }
        self.snapshot = self.driver.pump().snapshot();
    }

    /// 连接失败或会话断开：停止接收输入，在窗格里写一行记录，原因另存一份给断线遮罩用。
    fn disconnect(&mut self, reason: String, notice: &str) {
        self.terminal_id = None;
        // 断开后徽标没有意义；丢掉登记，共用这条连接的其他窗格不受影响。
        self.latency = None;
        self.state = PaneState::Disconnected;
        self.disconnect_reason = Some(reason);
        self.pending = None;
        self.assist.on_connection_reset(Instant::now());
        self.pump_mut().feed_local(notice.as_bytes());
    }

    /// 断线遮罩要不要盖在这个窗格上，盖的话返回原因（可能为空串）。只有会话窗格会盖：
    /// 离线演示窗格、空白标签没有"重连"可言；连接中的窗格另有crate::i18n::t(TermMsg::Connecting)，也不盖。
    /// 用户主动断开、远端正常退出（`exit`）、连接失败都算断开，和标签上的断开状态点一致。
    pub(crate) fn overlay_reason(&self) -> Option<&str> {
        (self.state == PaneState::Disconnected && self.is_session())
            .then(|| self.disconnect_reason.as_deref().unwrap_or(""))
    }

    /// 这个窗格有没有可以主动断开的会话：连着或正在连的主机 / 快速连接。
    fn can_disconnect(&self) -> bool {
        self.is_session() && matches!(self.state, PaneState::Live | PaneState::Connecting)
    }

    /// 用户主动断开（标签右键「断开连接」）：只结束这个窗格自己的 shell 通道，窗格留着显示已断开，
    /// 之后能用「重新连接」恢复。同一条 SSH 连接上别的标签 / 窗格 / SFTP 另外持有连接的引用，
    /// 这里只放掉自己那一份（`factory` 置空、关通道），连接是不是跟着关交给引用计数：最后一个
    /// 使用者走了才会真的断，不会误伤别人。
    fn disconnect_by_user(&mut self) {
        if !self.can_disconnect() {
            return;
        }
        // 先收尾：日志写完、通道发 Close、还在连的任务取消（`pending` 一并丢）。
        self.close();
        // 旧写端丢掉：之后的输入不会再写进（可能已经半关的）通道。
        self.backend = Box::new(NullBackend);
        self.factory = None;
        self.disconnect(
            user_disconnect_reason().to_owned(),
            &user_disconnect_notice(),
        );
        self.refresh();
    }

    /// 把这个窗格原地换成一条新连接（窗格编号、位置、尺寸不变；旧通道和日志先收尾，免得新旧两条同时跑）。
    /// `factory` 是已认证的连接（`None` 就按库里的主机 / 快速连接信息重新拨号；只有测试会给）。
    /// 不是会话窗格什么也不做，返回 `None`；否则返回重连前是不是在手动记日志（调用方负责接着记）。
    fn reconnect_in_place(
        &mut self,
        history: usize,
        factory: Option<Arc<crate::ssh_backend::SshSessionFactory>>,
    ) -> Option<bool> {
        if !self.is_session() {
            return None;
        }
        let (host_id, quick) = (self.host_id, self.quick.clone());
        let size = self.snapshot.size;
        // 手动开的日志重连后接着记；设置里自动开的，新连接上时会按设置重新开。
        let logging = self.log_mode == LogMode::Manual;
        // 分屏树的叶子认窗格编号：换掉整个窗格时沿用旧编号，位置和比例都不变。
        self.reconnect_with(|| match quick {
            Some(spec) => Pane::start_quick(spec, factory, size, history, next_generation()),
            None => Pane::start_ssh(factory, host_id, size, history, next_generation()),
        });
        Some(logging)
    }

    /// 先收旧通道（串口之类独占的资源要先放掉，新连接才拿得到），再用 `open` 开出的新窗格顶替。
    fn reconnect_with(&mut self, open: impl FnOnce() -> Pane) {
        self.close();
        let fresh = open();
        self.replace_connection(fresh);
    }

    /// 用刚开始连接的 `fresh` 顶掉这个窗格的连接：窗格编号沿用（分屏树认它），终端内容（屏幕 + 回滚缓冲）
    /// 也沿用，只换后端、泵、工厂、代次这些跟连接有关的东西；旧的随这次赋值一起丢掉（通道在调用前已收尾）。
    /// 旧终端里远端程序遗留的状态（备用屏幕、鼠标上报、隐藏光标……）先复位，新会话从干净的模式开始。
    fn replace_connection(&mut self, mut fresh: Pane) {
        fresh.id = self.id;
        fresh.pump_mut().adopt_terminal(self.pump_mut());
        fresh.pump_mut().reset_terminal_for_new_session();
        // 新窗格自己的"正在连接…"写在了被换走的空终端里，这里在旧输出后面重写一遍。
        if fresh.state == PaneState::Connecting {
            let lead = if fresh.pump().snapshot().cursor_col == 0 {
                ""
            } else {
                "\r\n"
            };
            let text = format!("{lead}{}\r\n", crate::i18n::t(TermMsg::Connecting));
            fresh.pump_mut().feed_local(text.as_bytes());
        }
        fresh.carry_terminal = true;
        *self = fresh;
        self.refresh();
    }

    /// 接上延迟监视器（`None` 就是这个窗格不显示延迟）。
    fn set_latency(&mut self, monitor: Option<Arc<LatencyMonitor>>) {
        self.latency = monitor.map(|monitor| PaneLatency {
            viewer: monitor.viewer(),
            label: self.latency_label(),
        });
    }

    /// 悬停提示里写的主机名：快速连接用输入的那串，主机库里的主机用它的名字。
    fn latency_label(&self) -> String {
        if self.is_session() {
            self.log_label()
        } else {
            String::new()
        }
    }

    /// 这个窗格现在看得见吗（外壳每个节拍告诉它）；变了才动监视器。
    fn set_latency_visible(&mut self, visible: bool) {
        if let Some(latency) = &mut self.latency {
            latency.viewer.set_visible(visible);
        }
    }

    /// 徽标要画的东西：监视器的快照和悬停提示用的主机名。已经断开 / 还没有第一次结果就不画。
    fn latency_snapshot(&self) -> Option<(LatencySnapshot, &str)> {
        let latency = self.latency.as_ref()?;
        let snapshot = latency.viewer.monitor().snapshot();
        (!snapshot.stopped && !snapshot.stats.is_empty())
            .then_some((snapshot, latency.label.as_str()))
    }

    /// 调试钩子：摆一个固定的延迟值（给截图用）。窗格没有监视器就装一个不测量的。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_set_latency(&mut self, sample: Option<latency::Sample>) {
        let Some(sample) = sample else {
            self.latency = None;
            return;
        };
        if self.latency.is_none() {
            self.set_latency(Some(LatencyMonitor::new(latency::ProbeKind::Ssh)));
            if let Some(latency) = &mut self.latency
                && latency.label.is_empty()
            {
                latency.label = "web-1".to_owned();
            }
        }
        if let Some(latency) = &self.latency {
            latency.viewer.monitor().record(sample);
        }
    }

    /// 回滚上限改了：新建会话（重连后的泵）用新值，正在用的终端立刻生效（调小会丢掉最旧的行）。
    fn set_history(&mut self, history: usize) {
        self.history = history;
        self.pump_mut().set_history(history);
        self.refresh();
    }

    fn pump(&self) -> &SessionPump<CoreFilter> {
        self.driver.pump()
    }

    fn pump_mut(&mut self) -> &mut SessionPump<CoreFilter> {
        self.driver.pump_mut()
    }

    /// 后台连接任务有结果了就接上；返回是否有变化。
    fn poll_pending(&mut self) -> bool {
        let Some(result) = self.pending.as_ref().and_then(|pending| pending.try_take()) else {
            return false;
        };
        self.pending = None;
        match result {
            Ok(opened) => {
                // SSH 窗格没有自带的监视器：用已认证连接上那一份（同一条连接上的窗格共用）。
                let monitor = opened.latency.or_else(|| {
                    opened
                        .factory
                        .as_ref()
                        .map(|factory| Arc::clone(factory.connection().latency()))
                });
                self.attach(opened.backend, opened.events, opened.gate, opened.factory);
                self.set_latency(monitor);
            }
            Err(err) => self.disconnect(plain(&err), &failure_notice(&err)),
        }
        true
    }

    /// 事件到了就刷新快照；`allowance` 是这一帧分给本窗格的数据字节额度（见
    /// `zssh_session::FrameScheduler`）。返回是否有变化和实际处理的字节数。
    fn poll_events_within(&mut self, allowance: usize) -> PollOutcome {
        let mut changed = self.poll_pending();
        let polled = self.driver.poll_counted(allowance);
        if polled.changed {
            changed = true;
        }
        // 远端退出、连接断了、写失败：提示一次。离线回环窗格关闭时的 Exit 不提示。
        if self.state == PaneState::Live
            && self.is_session()
            && let Some(exit) = self.driver.exit().cloned()
        {
            self.disconnect(disconnect_reason(&exit), &disconnect_notice(&exit));
            changed = true;
        }
        // 不管有没有重绘、窗格在不在前台：远端的查询（DSR / DA 等）等着回复。
        self.flush_replies();
        if self.observe_input() {
            changed = true;
        }
        if changed {
            self.snapshot = self.driver.pump().snapshot();
        }
        PollOutcome {
            changed,
            used: polled.used,
        }
    }

    /// 写入后立刻取一下回环回显：这是单个窗格自己的轮询，不经帧调度，额度给一整帧。
    fn poll_events(&mut self) -> bool {
        self.poll_events_within(FRAME_BYTE_BUDGET).changed
    }

    fn refresh(&mut self) {
        self.snapshot = self.driver.pump().snapshot();
    }

    /// 输入跟踪能认出的目标：窗格连的是谁。快速连接带着实际的用户 / 地址 / 端口；主机库里的主机用库编号
    /// （连接参数改了要重连才生效，重连换连接代次，所以编号加代次足够区分）。
    fn target_identity(&self) -> String {
        match (&self.quick, self.host_id) {
            (Some(spec), _) => format!("ssh://{}", spec.target.display()),
            (None, Some(id)) => format!("host:{id}"),
            (None, None) => String::new(),
        }
    }

    /// 每次处理完输出（以及定时）：让输入跟踪看一眼——先确认它还属于这条连接，再处理新到的 shell 标记，
    /// 最后对一遍屏幕。只读，不写远端。
    ///
    /// 返回候选浮层要画的东西有没有变（出现、消失、选中项变了）。
    fn observe_input(&mut self) -> bool {
        self.observe_input_at(Instant::now())
    }

    fn observe_input_at(&mut self, now: Instant) -> bool {
        let target = self.target_identity();
        self.assist
            .sync_context(self.id, self.generation, &target, now);
        self.assist.after_output(self.driver.pump_mut(), now);
        let live = self.state == PaneState::Live && self.is_session();
        self.assist.suggest_tick(live, now)
    }

    /// 限制条件（锁屏、广播、模态框）由工作区每帧告诉窗格；鼠标上报和备用屏幕 / 滚回历史由窗格自己从终端读。
    fn set_assist_restrictions(
        &mut self,
        locked: bool,
        broadcast: bool,
        modal: bool,
        enabled: bool,
    ) {
        let now = Instant::now();
        let mouse = self.pump().mouse_mode().reports_buttons();
        self.assist.set_enabled(enabled, now);
        self.assist
            .set_restrictions(locked, broadcast, modal, mouse, now);
    }

    /// 把终端对远端查询的协议回复写回这个窗格自己的会话。走写端专门的回复接口，不经 [`Self::write`]：
    /// 它不是用户输入，不受广播、锁屏、快捷命令影响；回复只回给产生查询的会话，代次不符就丢。
    /// 没连上（连接中 / 已断开 / 空白）的窗格没有可回的远端，回复作废。
    fn flush_replies(&mut self) {
        let Some(replies) = self.pump_mut().take_replies() else {
            return;
        };
        if self.state != PaneState::Live || replies.generation != self.generation {
            return;
        }
        self.backend.send_reply(replies.generation, replies.bytes);
    }

    /// 粘贴确认要记住的"目标身份"：窗格编号 + 连接代次 + 状态。重连换代次，断开 / 连上换状态，
    /// 任何一项变了都说明这已经不是确认条上展示过的那个目标。
    fn paste_target(&self) -> PasteTarget {
        PasteTarget {
            pane_id: self.id,
            generation: self.generation,
            state: self.state,
        }
    }

    /// 写命令走会话后端；同进程回环后端会立刻回送事件。
    /// 还在连接或已断开时丢弃输入，不能回显也不能攒着。
    ///
    /// 给不关心结果、也不属于任何输入来源的调用方（调试钩子、测试）用；产品里的输入路径一律走
    /// [`Self::write_as`]，把来源和结果告诉输入跟踪。
    #[cfg(any(test, debug_assertions))]
    fn write(&mut self, bytes: &[u8]) {
        let _ = self.write_as(bytes, WriteOrigin::Untracked);
    }

    /// 带来源、带结果的本地写入。
    ///
    /// 顺序是：先把这次写入登记给输入跟踪，再交给后端发送，最后报告结果——同步回环回显会在发送的
    /// 同一个调用里到达，不先登记就认不出它。锁屏、非 Live、队列满、通道已关都会得到明确的
    /// [`WriteOutcome`]，不会静默当成已经写入；`Queued` 只表示本地排上了队，不是送达也不是回显。
    pub(crate) fn write_as(&mut self, bytes: &[u8], origin: WriteOrigin) -> WriteOutcome {
        if bytes.is_empty() {
            return WriteOutcome::Rejected(RejectReason::Empty);
        }
        if self.state != PaneState::Live {
            return WriteOutcome::Rejected(RejectReason::NotLive);
        }
        // 锁屏（V2-U26）：键盘、粘贴、快捷命令、撰写栏、广播都从这里出去，锁着时一律丢掉。
        // AI 助手往终端写命令不走这里（走 `ai_terminals`），它在工具桥那一层拦：锁着时 AI 的
        // 任何执行请求都要先经用户确认（`agent_host::set_ai_locked`），所以锁定期间不会有新命令进来。
        if crate::app_lock::is_locked() {
            self.assist.note_locked(Instant::now());
            return WriteOutcome::Rejected(RejectReason::Locked);
        }
        let now = Instant::now();
        let seq = self.assist.before_write(origin, bytes, now);
        let outcome = self.backend.send_write(bytes.to_vec());
        self.assist.after_write(origin, seq, outcome, now);
        self.poll_events();
        outcome
    }

    /// 发送 Break 信号（串口会话的按钮 / 快捷键用，后续界面卡接上；其他协议的会话忽略）。
    #[allow(dead_code)]
    pub(crate) fn send_break(&mut self) {
        if self.state == PaneState::Live {
            self.backend.send_break();
        }
    }

    /// 扫描高亮时用的"这是哪一屏"：窗格、连接代次、屏幕内容修订号、规则版本、视口。
    fn scan_key(&self, rules_revision: u64) -> ScanKey {
        ScanKey {
            pane: self.id,
            generation: self.generation,
            content: self.snapshot.revision,
            rules: rules_revision,
            rows: self.snapshot.size.rows,
            cols: self.snapshot.size.cols,
            offset: self.snapshot.display_offset,
        }
    }

    /// 本地重排后把新尺寸也交给后端（回环后端忽略）。后端不会因为输入队列满而丢它，所以这里
    /// 不需要靠"下一次还会再发"来补。
    fn resize(&mut self, size: Size) {
        self.pump_mut().resize(size);
        // 重排之后输入起点的行列号不再对应原来的格子。
        self.assist.on_geometry_changed(Instant::now());
        if self.state == PaneState::Live {
            self.backend.send(SessionCommand::Resize(size));
            self.open_size = size;
        }
        self.refresh();
    }

    /// 点「记录」：没在记就手动开始，手动开着就停，设置里自动开着的不另开也不在这里停。
    /// 日志的写与收尾都在日志自己的线程里，不阻塞界面线程；`prefs` 由调用方给（测试用）。
    fn toggle_log_with(&mut self, prefs: &crate::app_prefs::AppPrefs) -> LogOutcome {
        match self.log_mode {
            LogMode::Auto => LogOutcome::Auto {
                dir: prefs.session_log_dir_path(),
            },
            LogMode::Manual => {
                self.log_mode = LogMode::Off;
                // 丢掉日志只是通知写线程收尾（不等磁盘）；路径要在丢之前取。
                let path = self
                    .pump_mut()
                    .take_plain_log()
                    .and_then(|log| log.path().map(std::path::Path::to_path_buf));
                LogOutcome::Stopped { path }
            }
            LogMode::Off => match crate::app_prefs::start_session_log(prefs, &self.log_label()) {
                Ok(log) => {
                    self.pump_mut().set_plain_log(log);
                    self.log_mode = LogMode::Manual;
                    LogOutcome::Started {
                        dir: prefs.session_log_dir_path(),
                    }
                }
                Err(err) => LogOutcome::Failed(err.to_string()),
            },
        }
    }

    /// 关闭窗格：通知后端；还在连接的任务随 `pending` 一起取消。日志随泵一起丢掉，
    /// 写线程自己把排着的写完（程序退出时另有有限等待）。
    fn close(&mut self) {
        self.pending = None;
        self.backend.send(SessionCommand::Close);
    }
}

fn terminal_button(button: MouseButton) -> Option<TerminalMouseButton> {
    match button {
        MouseButton::Left => Some(TerminalMouseButton::Left),
        MouseButton::Middle => Some(TerminalMouseButton::Middle),
        MouseButton::Right => Some(TerminalMouseButton::Right),
        _ => None,
    }
}

/// 标签上状态点的含义（0.2.14 的 `tabDot`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TabStatus {
    /// 空白标签。
    Empty,
    Connecting,
    Connected,
    Disconnected,
}

/// 一个标签里各窗格的状态合成一个点：正在连 > 已连上 > 全断开 > 空标签。
pub(crate) fn tab_status(states: &[PaneState]) -> TabStatus {
    if states.contains(&PaneState::Connecting) {
        TabStatus::Connecting
    } else if states.contains(&PaneState::Live) {
        TabStatus::Connected
    } else if states.contains(&PaneState::Disconnected) {
        TabStatus::Disconnected
    } else {
        TabStatus::Empty
    }
}

/// 外壳（标题栏标签条）画一个标签需要的全部信息。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TabInfo {
    pub id: u64,
    pub title: String,
    pub status: TabStatus,
    /// 聚焦窗格连的主机；空白标签和离线演示窗格没有。
    pub host_id: Option<i64>,
    /// 聚焦窗格是快速连接时的 `user@host:port`（右键「复制地址」用）。
    pub quick_target: Option<String>,
    /// 聚焦窗格是快速连接时归并用的键 `用户@地址:端口`（地址不分大小写）：输入写法不同（带不带端口）
    /// 但连的是同一处，就算同一台主机（标签序号按它编号）。
    pub quick_key: Option<String>,
    /// 至少有一个窗格已经断开（右键「重新连接」才可用）。
    pub can_reconnect: bool,
    /// 至少有一个窗格还连着或正在连（右键「断开连接」才可用）。
    pub can_disconnect: bool,
    /// AI 此刻正在这个标签的某个终端里执行命令（标签上画一个活动点，0.2.14 的 `aiBadges`）。
    pub ai_active: bool,
}

/// 标签 id 全进程唯一：标签可以在窗口之间搬，id 不能撞。
fn next_tab_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub(crate) struct Tab {
    id: u64,
    title: String,
    /// 窗格，永远按视觉顺序排（和 `tree` 的叶子顺序一致：左到右、上到下），所以下标就是
    /// 屏幕上的次序；`active_pane` 是聚焦窗格的下标。
    panes: Vec<Pane>,
    active_pane: usize,
    /// 分屏树（0.2.14 的 `splitTree`）：叶子是窗格编号，分割点记方向和比例。
    tree: Node,
    /// 输入广播：键盘和粘贴同时发给这个标签的所有分屏窗格（0.2.14 也只作用于当前标签）。
    broadcast: bool,
    /// 广播开关翻转过几次：输入跟踪的候选请求带着它，翻转就让在途请求过期。
    broadcast_revision: u64,
}

impl Tab {
    fn new(title: String, pane: Pane) -> Self {
        Self {
            id: next_tab_id(),
            title,
            tree: Node::Pane(pane.id),
            panes: vec![pane],
            active_pane: 0,
            broadcast: false,
            broadcast_revision: 0,
        }
    }

    /// 整个标签换成一个新窗格（空白标签里选了主机）：分屏树一并重置。
    fn reset_to(&mut self, pane: Pane) {
        self.tree = Node::Pane(pane.id);
        self.panes = vec![pane];
        self.active_pane = 0;
    }

    /// 把聚焦的窗格按 `axis` 一分为二：新窗格紧跟在它后面（右 / 下），并拿到焦点（0.2.14 的
    /// `splitAt`）。返回新窗格的下标。
    fn split_active(&mut self, axis: Axis, new_pane: Pane) -> usize {
        let at = self.active_pane.min(self.panes.len() - 1);
        let target = self.panes[at].id;
        let new_id = new_pane.id;
        if !self
            .tree
            .split_pane(target, new_id, split_tree::next_split_id(), axis)
        {
            // 树和窗格表不一致（不该发生）：宁可不拆，也不让两边对不上。
            return at;
        }
        self.panes.insert(at + 1, new_pane);
        self.active_pane = at + 1;
        self.debug_check_order();
        at + 1
    }

    /// 窗格表必须和分屏树的叶子同序（下标 = 屏幕上的次序，鼠标命中、广播、验证钩子都靠它）。
    fn debug_check_order(&self) {
        debug_assert_eq!(
            self.tree.pane_ids(),
            self.panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
            "窗格表和分屏树不同序" // i18n-allow: 内部不变式（调试）
        );
    }

    /// 关掉第 `index` 个窗格，兄弟节点顶掉它的位置；焦点按 0.2.14 的规则挪（关的不是焦点
    /// 窗格就不动，是的话落到相邻窗格）。只剩一个窗格时不动，返回 `None`（调用方该关整个标签）。
    fn close_pane_at(&mut self, index: usize) -> Option<Pane> {
        if self.panes.len() <= 1 || index >= self.panes.len() {
            return None;
        }
        let victim = self.panes[index].id;
        let focused = self.panes[self.active_pane.min(self.panes.len() - 1)].id;
        let tree = std::mem::replace(&mut self.tree, Node::Pane(victim));
        let (tree, focus) = split_tree::close_pane_with_focus(tree, victim, focused);
        let Some(tree) = tree else {
            // 多于一个窗格时关一个不会清空树；防御性地还原。
            self.tree = Node::Pane(self.panes[0].id);
            return None;
        };
        self.tree = tree;
        let pane = self.panes.remove(index);
        self.active_pane = focus
            .and_then(|id| self.panes.iter().position(|pane| pane.id == id))
            .unwrap_or(0);
        self.debug_check_order();
        Some(pane)
    }

    fn active(&self) -> &Pane {
        &self.panes[self.active_pane.min(self.panes.len() - 1)]
    }

    fn active_mut(&mut self) -> &mut Pane {
        let index = self.active_pane.min(self.panes.len() - 1);
        &mut self.panes[index]
    }

    fn is_split(&self) -> bool {
        self.panes.len() > 1
    }

    /// 键盘输入和粘贴要发给哪些窗格：开了广播是这个标签的所有窗格，否则只有聚焦的那个。
    /// 广播只作用于当前标签，不会越过标签去碰别的主机。
    fn input_targets(&self) -> Vec<usize> {
        if self.broadcast {
            (0..self.panes.len()).collect()
        } else {
            vec![self.active_pane.min(self.panes.len() - 1)]
        }
    }

    /// 为一次待确认的粘贴记下目标：标签、广播开关、每个接收窗格的身份。确认条上写的接收者就是
    /// 这里记的"已连接"窗格；没有任何已连接窗格时返回 `None`（粘贴不出去，不必弹确认）。
    fn capture_paste(&self, text: String) -> Option<PendingPaste> {
        let targets: Vec<PasteTarget> = self
            .input_targets()
            .into_iter()
            .map(|index| self.panes[index].paste_target())
            .collect();
        if !targets.iter().any(|target| target.state == PaneState::Live) {
            return None;
        }
        Some(PendingPaste {
            text,
            tab_id: self.id,
            broadcast: self.broadcast,
            pane_ids: self.panes.iter().map(|pane| pane.id).collect(),
            targets,
        })
    }

    /// 确认时核对：这个标签还是当初那个、广播开关没变、（广播时）窗格集合没变、每个目标窗格还在且
    /// 代次 / 状态都没变。全对才返回要发的窗格下标（只含已连接的）；任何一项变了返回 `None`。
    /// 焦点换到别的窗格不算变化：非广播时目标是当初那个窗格，不是现在聚焦的。
    fn resolve_paste(&self, pending: &PendingPaste) -> Option<Vec<usize>> {
        if self.id != pending.tab_id || self.broadcast != pending.broadcast {
            return None;
        }
        if pending.broadcast {
            let now: Vec<PaneId> = self.panes.iter().map(|pane| pane.id).collect();
            if now != pending.pane_ids {
                return None;
            }
        }
        let mut indices = Vec::new();
        for target in &pending.targets {
            let index = self
                .panes
                .iter()
                .position(|pane| pane.id == target.pane_id)?;
            if self.panes[index].paste_target() != *target {
                return None;
            }
            if target.state == PaneState::Live {
                indices.push(index);
            }
        }
        Some(indices)
    }

    fn status(&self) -> TabStatus {
        let states: Vec<PaneState> = self.panes.iter().map(|pane| pane.state).collect();
        tab_status(&states)
    }

    /// 断开标签里所有连着的会话窗格；返回断开了几个。
    fn disconnect_sessions(&mut self) -> usize {
        let mut count = 0;
        for pane in &mut self.panes {
            if pane.can_disconnect() {
                pane.disconnect_by_user();
                count += 1;
            }
        }
        count
    }

    /// 还没连任何主机的空白标签。
    fn is_blank(&self) -> bool {
        self.panes.iter().all(|pane| pane.state == PaneState::Empty)
    }

    fn info(&self) -> TabInfo {
        TabInfo {
            id: self.id,
            title: self.title.clone(),
            status: self.status(),
            host_id: self.active().host_id,
            quick_target: self.active().quick.as_ref().map(|spec| spec.label.clone()),
            quick_key: self.active().quick.as_ref().map(|spec| {
                format!(
                    "{}@{}:{}",
                    spec.target.username,
                    spec.target.host.to_lowercase(),
                    spec.target.port
                )
            }),
            can_reconnect: self
                .panes
                .iter()
                .any(|pane| pane.state == PaneState::Disconnected && pane.is_session()),
            can_disconnect: self.panes.iter().any(Pane::can_disconnect),
            ai_active: self.panes.iter().any(|pane| {
                pane.terminal_id
                    .as_deref()
                    .is_some_and(|id| crate::ai_terminals::terminals().is_ai_active(id))
            }),
        }
    }
}

/// 待确认粘贴的一个目标窗格的身份（见 [`Pane::paste_target`]）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PasteTarget {
    pane_id: PaneId,
    generation: u64,
    state: PaneState,
}

/// 多行粘贴在确认框里等用户点头。它绑定的是**发起粘贴时**的目标，不是确认时的：用户在确认框
/// 开着的时候切了分屏焦点、开关了广播、关了窗格或重连，都不能让这段文字悄悄发到别处。
struct PendingPaste {
    text: String,
    tab_id: u64,
    broadcast: bool,
    /// 发起时这个标签的全部窗格（广播时用来发现窗格增减）。
    pane_ids: Vec<PaneId>,
    /// 发起时的接收窗格（含身份）。
    targets: Vec<PasteTarget>,
}

impl PendingPaste {
    /// 实际会收到的窗格个数（已连接的）：确认框上写的就是它。
    fn recipients(&self) -> usize {
        self.targets
            .iter()
            .filter(|target| target.state == PaneState::Live)
            .count()
    }
}

/// 粘贴目标变了而取消时的提示。
fn paste_cancelled() -> &'static str {
    crate::i18n::t(TermMsg::PasteCancelled)
}

/// 高亮状态：规则、修订号、最近扫描结果都在这里（面板的输入框等界面状态归外壳）。
struct HighlightUi {
    rules: Vec<HighlightRule>,
    /// 规则集修订号：规则一变就加一，进扫描请求的 `ScanKey`，旧规则的结果因此对不上。
    revision: u64,
    next_id: u64,
    /// 提交、收取后台扫描，并认定哪份结果能显示。
    scheduler: HighlightScheduler,
    /// 聚焦窗格这一帧的视图（窗格、连接代次、规则、视口）：画命中区间时按它取，换了视图就取不到旧结果。
    current_view: Option<ScanKey>,
    status: Option<String>,
}

impl HighlightUi {
    fn bump(&mut self) {
        self.revision += 1;
    }
}

/// 一个窗格在窗口里的位置（窗口坐标，逻辑像素）：鼠标命中和换算格子都靠它。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct PaneGeom {
    /// 窗格整块（含四周内边距）：点在这里面就是聚焦这个窗格。
    rect: split_tree::Rect,
    /// 文字区的左上角。
    left: f32,
    top: f32,
    cols: usize,
    rows: usize,
}

pub struct TerminalWorkspace {
    tabs: TabSet<Tab>,
    /// 每帧输出预算的分配器：窗格再多，一帧解析的终端数据总量也有上限。
    frame_budget: FrameScheduler,
    focus: FocusHandle,
    /// (anchor, head)，可见区坐标。
    selection: Option<((usize, usize), (usize, usize))>,
    selecting: bool,
    /// 左键按下的格子；指针离开这一格才产生选区（单击不留空选区）。
    drag_anchor: Option<(usize, usize)>,
    cell_width: f32,
    cell_height: f32,
    /// 触控板 / 高精度滚轮累积下来、还不够一整行的滚动量（行）；见 [`take_whole_lines`]。
    wheel_remainder: f32,
    pending_paste: Option<PendingPaste>,
    hover_link: Option<Arc<str>>,
    /// 应用级设置实体（全进程一份，所有窗口共用）；改设置走它，不在窗口里存盘。
    settings_store: Entity<AppSettings>,
    /// 界面设置的读缓存（字号、行高、光标、主题……）和它对应的修订号：别处改了会通过订阅更新。
    settings: UiSettings,
    settings_rev: u64,
    /// 已经提示过的写盘失败次数：同一次失败只提示一回。
    settings_error_seen: u64,
    /// 一行状态文字（日志路径、「正在重新连接…」、不能执行的原因）；需要用户看见的由外壳取走弹通知。
    status: StatusLine,
    /// 高亮规则与最近一次扫描结果。
    highlight: HighlightUi,
    /// 定时取后台结果的轻量任务。
    _highlight_tick: Task<()>,
    /// 正在拖的分隔条（分割点编号）。
    dragging_split: Option<u64>,
    /// 活动标签上一帧的窗格位置（和 `Tab::panes` 同序）。
    pane_geom: Vec<PaneGeom>,
    /// 活动标签上一帧的布局（相对终端区域）：拖分隔条时按它换算比例。
    layout: Layout,
    /// 终端区域在窗口里的左上角（上一帧量到的）。
    area_origin: (f32, f32),
    /// 设置里的字体栈解析出来的实际字体名：栈没变就不重算。
    font_cache: Option<(String, SharedString)>,
    /// 设置页「自定义字体」的输入框（工作区拥有，设置页只是画它）。
    font_input: Entity<InputState>,
    /// 设置页选了「自定义…」：即使栈的第一项是常见字体也显示输入框。
    font_custom: bool,
    /// 设置页回滚行数的滑块；松手才应用（拖的过程中只是预览，免得途经小值时丢掉历史）。
    scrollback_slider: Entity<SliderState>,
    _settings_subs: Vec<Subscription>,
    /// 弹着的终端右键菜单（V2-U25）；关掉（点外面、Esc、点了某项）时清空。
    context_menu: Option<ContextMenu>,
    /// 开着的搜索条（右键菜单「搜索」打开，V2-TERM-UX1）；同一时间只有一个窗格有。
    search: Option<search_ops::SearchState>,
    /// 搜索条的输入框（工作区拥有，开关搜索条时复用同一个）。
    search_input: Entity<InputState>,
    /// 远端已接管本次左/中/右键按下，松开时要补一条上报。
    mouse_pressed: bool,
    /// 上一次上报的格子；移动事件只在跨格时发。
    last_mouse_cell: Option<(usize, usize)>,
    /// 窗格区域在窗口里的位置：canvas 在绘制时写入，下一帧排版读取。
    area: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// 上一次排版用的区域；和 `area` 不一样说明窗口或外壳变了，要重排。
    layout_area: Option<Bounds<Pixels>>,
    /// 新开的标签用的初始尺寸（最近一次排版算出来的整宽窗格）。
    default_size: Size,
    /// 输入法状态机里本窗口的名字（关掉「自动切英文」时要把输入法还回去）。
    ime_key: String,
    /// 光标闪烁的相位时钟。
    cursor_blink: BlinkClock,
    /// 终端有没有注册文字输入处理器（Linux 和 macOS 注册，见 `ime_ops`）：决定带字符的按键处理完要不要拦下事件。
    ime_input: bool,
    /// 输入法还没提交的文字（拼音等），只画在光标处，不发给远端。
    ime_preedit: ime_ops::Preedit,
    /// 上一帧告诉输入法的光标格子位置：变了才通知平台挪候选框。
    ime_cell: Option<(f32, f32)>,
    /// 终端内容现在有没有摆在窗口里（主窗口在主机页 / 设置页 / 文件标签时没有）。外壳每个节拍告诉
    /// 它；独立终端窗口恒为真。延迟测量只对看得见的窗格做。
    surface_shown: bool,
    /// 上一次看到的各窗格延迟版本之和：变了就要重画徽标。
    latency_stamp: u64,
    /// 终端内命令建议用的快捷命令快照（由外壳每个节拍同步，见 `sync_quick_sources`）。
    assist_quick: Arc<Vec<QuickSource>>,
    _ime_focus: Subscription,
    _ime_blur: Subscription,
}

/// 弹着的右键菜单：菜单实体、弹出的位置（窗口坐标）、弹出时的条目（调试断言读它）。
struct ContextMenu {
    menu: Entity<PopupMenu>,
    /// 右键点中的窗格：「关闭窗格」「搜索」「清屏」都作用在它身上（弹出后焦点可能又换了）。
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    pane: PaneId,
    position: ::gpui::Point<Pixels>,
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    entries: Vec<crate::terminal_menu::Entry>,
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    link: Option<Arc<str>>,
    /// 订阅随菜单一起放着：丢掉菜单，订阅也就没了。
    _dismiss: Subscription,
}

/// 窗口现在是不是真的摆在用户面前：可见且没有最小化（收进托盘的窗口不可见）。
/// 非 Windows 没有这个概念（ZeeTerm 只面向 Windows），一律当作可见。
fn window_shown(window: &Window) -> bool {
    #[cfg(windows)]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindowVisible};
        if let Ok(handle) = HasWindowHandle::window_handle(window)
            && let RawWindowHandle::Win32(win32) = handle.as_raw()
        {
            let hwnd = HWND(win32.hwnd.get() as _);
            // SAFETY: 只读查询，句柄失效时返回假。
            return unsafe { IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool() };
        }
    }
    let _ = window;
    true
}

/// 哪些窗格该被登记成"看得见"：只有活动标签的窗格，而且设置开着、窗口和页面都摆在用户面前。
pub(crate) fn latency_visible_flags(
    pane_count: usize,
    tab_is_active: bool,
    setting_on: bool,
    surface_shown: bool,
    window_shown: bool,
) -> Vec<bool> {
    vec![tab_is_active && setting_on && surface_shown && window_shown; pane_count]
}

impl TerminalWorkspace {
    /// 空的工作区：一个标签也没有。外壳自己决定什么时候开第一个。
    pub fn new(window: &mut Window, cx: &mut Context<Self>, ime_key: String) -> Self {
        let focus = cx.focus_handle();
        let settings_store = AppSettings::global(cx);
        let (settings, settings_rev) = {
            let model = settings_store.read(cx);
            (model.settings().clone(), model.revision())
        };
        let rules = settings
            .highlight_rules
            .clone()
            .unwrap_or_else(crate::highlight::default_rules);
        let next_id = rules.iter().map(|rule| rule.id).max().unwrap_or(0) + 1;
        let highlight = HighlightUi {
            rules,
            revision: 1,
            next_id,
            scheduler: HighlightScheduler::new(Box::new(EngineHandle::start)),
            current_view: None,
            status: None,
        };
        // 「终端里自动切到英文输入」关着时不碰输入法（离开时 `apply(false)` 对没进过英文区的窗口是空操作）。
        let _ime_focus = cx.on_focus(&focus, window, {
            let key = ime_key.clone();
            move |this, _window, _cx| {
                if this.settings.ime_english {
                    crate::ime::apply(&key, true);
                }
            }
        });
        let ime_blur_key = ime_key.clone();
        let _ime_blur = cx.on_blur(&focus, window, move |this, _window, _cx| {
            // 焦点走了，输入法没提交的预编辑也随之作废（输入法那边会自己取消）。
            this.ime_preedit.clear();
            crate::ime::apply(&ime_blur_key, false);
            // 接受键按住时失焦：松开事件收不到了，latch 在这里清掉。
            this.suggest_focus_lost();
        });
        // 设置页的两个控件：自定义字体的输入框、回滚行数的滑块。状态归工作区，这样设置页每次
        // 重绘拿到的是同一个实体。
        let font_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::settings::DEFAULT_FONT_FAMILY)
        });
        font_input.update(cx, |state, cx| {
            state.set_value(settings.font_family.clone(), window, cx);
        });
        let scrollback_slider = cx.new(|_| {
            // 先设上限再设下限：组件每设一个就按当前的另一端夹一次值，反过来会先撞上默认的上限 100。
            SliderState::new()
                .max(MAX_SCROLLBACK as f32)
                .min(MIN_SCROLLBACK as f32)
                .step(crate::settings::SCROLLBACK_STEP as f32)
                .default_value(settings.scrollback as f32)
        });
        let search_input = cx.new(|cx| InputState::new(window, cx));
        let mut settings_subs = Vec::new();
        // 搜索框：文字变了就重搜，Enter 下一个，Shift+Enter 上一个。输入框是工作区自己画出来的元素，
        // 它的状态（光标、选区）一变工作区也要重画。
        settings_subs.push(cx.subscribe_in(
            &search_input,
            window,
            |this, _input, event: &InputEvent, _window, cx| match event {
                InputEvent::Change => this.search_text_changed(cx),
                InputEvent::PressEnter { shift, .. } => this.search_step(!*shift, cx),
                _ => {}
            },
        ));
        settings_subs.push(cx.observe(&search_input, |_, _, cx| cx.notify()));
        settings_subs.push(cx.subscribe_in(
            &font_input,
            window,
            |this, input, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Change) {
                    let text = input.read(cx).value().to_string();
                    this.set_font_family(&text, cx);
                }
            },
        ));
        settings_subs.push(cx.subscribe_in(
            &scrollback_slider,
            window,
            |this, _slider, event: &SliderEvent, _window, cx| match event {
                // 拖的过程中只让数字跟着变；松手才真的应用（调小会丢掉最旧的行）。
                SliderEvent::Change(_) => cx.notify(),
                SliderEvent::Release(value) => {
                    this.set_scrollback(value.end().round() as usize, cx)
                }
            },
        ));
        // 别的窗口（主窗口、独立终端窗口）改了设置：更新读缓存，处理这个窗口里受影响的东西。
        settings_subs.push(
            cx.observe_in(&settings_store, window, |this, _store, window, cx| {
                this.sync_settings(Some(window), cx)
            }),
        );
        let mut this = Self {
            tabs: TabSet::new(),
            frame_budget: FrameScheduler::new(),
            focus,
            selection: None,
            selecting: false,
            drag_anchor: None,
            cell_width: 8.0,
            cell_height: 18.0,
            wheel_remainder: 0.0,
            pending_paste: None,
            hover_link: None,
            settings_store,
            settings,
            settings_rev,
            settings_error_seen: 0,
            status: StatusLine::new(),
            dragging_split: None,
            pane_geom: Vec::new(),
            layout: Layout::default(),
            area_origin: (0.0, 0.0),
            font_cache: None,
            font_input,
            font_custom: false,
            scrollback_slider,
            _settings_subs: settings_subs,
            context_menu: None,
            search: None,
            search_input,
            mouse_pressed: false,
            last_mouse_cell: None,
            highlight,
            _highlight_tick: Task::ready(()),
            area: Rc::new(Cell::new(None)),
            layout_area: None,
            default_size: DEFAULT_SIZE,
            ime_key,
            cursor_blink: BlinkClock::new(),
            ime_input: ime_ops::INPUT_HANDLER_ENABLED,
            ime_preedit: ime_ops::Preedit::default(),
            ime_cell: None,
            surface_shown: true,
            latency_stamp: 0,
            assist_quick: Arc::new(Vec::new()),
            _ime_focus,
            _ime_blur,
        };
        // 后台扫描完成后主动重绘；没有新结果时只醒来一次。区域变了（窗口缩放、外壳的
        // 侧栏/状态栏出现消失）也在这里发现并重排。
        this._highlight_tick = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(60))
                    .await;
                let alive = this.update_in(cx, |this, window, cx| {
                    let sessions = this.poll_sessions();
                    let relayout = this.area.get() != this.layout_area;
                    // 光标闪烁只在终端有焦点时重画（没焦点光标画成静止，不必每半秒重画）。
                    let blink = this.settings.cursor_blink
                        && this.focus.is_focused(window)
                        && this.cursor_blink.changed();
                    let latency = this.sync_latency(window);
                    let save_failed = this.note_settings_save_failure(cx);
                    let search = this.tick_search();
                    if this.poll_highlight()
                        || search
                        || sessions
                        || relayout
                        || blink
                        || latency
                        || save_failed
                    {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        this
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// 设置写盘失败要让用户看得见（状态栏一行字）；同一次失败只提示一回，后台会自动重试。
    fn note_settings_save_failure(&mut self, cx: &App) -> bool {
        let (error, seq) = self.settings_store.read(cx).save_error();
        if seq == self.settings_error_seen {
            return false;
        }
        self.settings_error_seen = seq;
        let Some(error) = error else {
            return false;
        };
        self.status.announce(
            StatusLevel::Error,
            format!(
                "{}{error}",
                crate::i18n::t(TermMsg::SettingsSaveFailedPrefix)
            ),
        );
        true
    }

    /// 外壳告诉工作区：终端内容现在有没有摆在窗口里（影响延迟测量的暂停 / 恢复）。
    pub(crate) fn set_surface_shown(&mut self, shown: bool) {
        self.surface_shown = shown;
    }

    /// 每个节拍：把各窗格"看不看得见"登记给延迟监视器（变了才有动作），并看延迟有没有新数据。
    /// 返回是否要重画。
    fn sync_latency(&mut self, window: &Window) -> bool {
        let shown = window_shown(window);
        let active = self.tabs.active_index();
        let setting = self.settings.show_latency;
        let surface = self.surface_shown;
        let mut stamp = 0u64;
        for (index, tab) in self.tabs.items_mut().iter_mut().enumerate() {
            let flags = latency_visible_flags(
                tab.panes.len(),
                Some(index) == active,
                setting,
                surface,
                shown,
            );
            for (pane, visible) in tab.panes.iter_mut().zip(flags) {
                pane.set_latency_visible(visible);
                if visible && let Some(latency) = &pane.latency {
                    stamp = stamp.wrapping_add(latency.viewer.monitor().version() + 1);
                }
            }
        }
        let changed = stamp != self.latency_stamp;
        self.latency_stamp = stamp;
        changed
    }

    // ---- 只读查询（外壳用） ----

    pub(crate) fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub(crate) fn active_index(&self) -> Option<usize> {
        self.tabs.active_index()
    }

    pub(crate) fn tab_infos(&self) -> Vec<TabInfo> {
        self.tabs.items().iter().map(Tab::info).collect()
    }

    /// 活动标签是不是还没选主机的空白标签。
    pub(crate) fn active_tab_is_blank(&self) -> bool {
        self.tabs.active().is_some_and(Tab::is_blank)
    }

    /// 活动标签聚焦窗格连的主机（状态栏、右键「复制地址」用）。
    pub(crate) fn active_host_id(&self) -> Option<i64> {
        self.tabs.active().and_then(|tab| tab.active().host_id)
    }

    pub(crate) fn tab_host_id(&self, index: usize) -> Option<i64> {
        self.tabs.get(index).and_then(|tab| tab.active().host_id)
    }

    /// 这个标签聚焦窗格的主机和已认证连接（右键「打开 SFTP」复用它，不再握手）。
    pub(crate) fn tab_origin(
        &self,
        index: usize,
    ) -> Option<(i64, Option<Arc<crate::ssh_backend::SshSessionFactory>>)> {
        let pane = self.tabs.get(index)?.active();
        Some((pane.host_id?, pane.factory.clone()))
    }

    /// 每个标签聚焦窗格的主机 id（AI 面板按标签各自一份，目标主机跟着各自的标签走）。
    pub(crate) fn tab_hosts(&self) -> Vec<(u64, Option<i64>)> {
        self.tabs
            .items()
            .iter()
            .map(|tab| (tab.id, tab.active().host_id))
            .collect()
    }

    /// 这个窗口里所有标签、所有分屏窗格的主机 id（含没聚焦的、已经断开的：标签还在就还能重连）。
    /// AI 对话锚定的主机「还有没有标签」按它算；只看各标签的聚焦窗格会把分屏里的主机漏掉。
    pub(crate) fn pane_host_ids(&self) -> std::collections::HashSet<i64> {
        self.tabs
            .items()
            .iter()
            .flat_map(|tab| tab.panes.iter().filter_map(|pane| pane.host_id))
            .collect()
    }

    /// 活动标签聚焦窗格是快速连接时：（它的终端编号，连上了才有；显示名）。AI 对话锚定快速连接时认终端本身。
    pub(crate) fn active_quick(&self) -> Option<(Option<String>, String)> {
        let pane = self.pane()?;
        let spec = pane.quick.as_ref()?;
        Some((self.active_terminal_id(), spec.label.clone()))
    }

    /// 这个标签聚焦窗格是快速连接时的 `user@host:port`。
    pub(crate) fn tab_quick_target(&self, index: usize) -> Option<String> {
        self.tabs
            .get(index)
            .and_then(|tab| tab.active().quick.as_ref().map(|spec| spec.label.clone()))
    }

    /// 活动窗格的主机和已认证连接（开 SFTP / 新窗口时复用）。
    pub(crate) fn active_origin(
        &self,
    ) -> Option<(i64, Option<Arc<crate::ssh_backend::SshSessionFactory>>)> {
        let pane = self.pane()?;
        Some((pane.host_id?, pane.factory.clone()))
    }

    pub(crate) fn settings(&self) -> &UiSettings {
        &self.settings
    }

    /// 外壳取走待弹的状态提示（主窗口在工作区通知重画时调用，转成通知）。
    pub(crate) fn take_status_notice(&mut self) -> Option<StatusNotice> {
        self.status.take_notice()
    }

    #[cfg(debug_assertions)]
    pub(crate) fn status_text(&self) -> &str {
        &self.status.text
    }

    #[cfg(test)]
    pub(crate) fn status_level(&self) -> StatusLevel {
        self.status.level
    }

    #[cfg(debug_assertions)]
    pub(crate) fn set_status_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.status.set(StatusLevel::Info, text);
        cx.notify();
    }

    pub(crate) fn broadcast_on(&self) -> bool {
        self.tabs.active().is_some_and(|tab| tab.broadcast)
    }

    #[cfg(debug_assertions)]
    pub(crate) fn is_split(&self) -> bool {
        self.tabs.active().is_some_and(Tab::is_split)
    }

    pub(crate) fn highlight_rules(&self) -> &[HighlightRule] {
        &self.highlight.rules
    }

    #[cfg(debug_assertions)]
    pub(crate) fn highlight_status(&self) -> Option<&str> {
        self.highlight.status.as_deref()
    }

    #[cfg(debug_assertions)]
    pub(crate) fn highlight_set_status(&mut self, text: Option<String>) {
        self.highlight.status = text;
    }

    /// 活动标签的屏幕文字：截图前的隐私检查用。
    #[cfg(debug_assertions)]
    pub(crate) fn visible_text(&self) -> Vec<String> {
        let Some(tab) = self.tabs.active() else {
            return Vec::new();
        };
        tab.panes
            .iter()
            .flat_map(|pane| pane.snapshot.rows.iter().map(|row| row.text.clone()))
            .collect()
    }

    /// 活动窗格是否已经连上（调试钩子等它连上再往里打字）。
    pub(crate) fn active_pane_is_live(&self) -> bool {
        self.pane()
            .is_some_and(|pane| pane.state == PaneState::Live && pane.is_session())
    }

    // ---- 内部访问 ----

    fn pane(&self) -> Option<&Pane> {
        self.tabs.active().map(Tab::active)
    }

    fn pane_mut(&mut self) -> Option<&mut Pane> {
        self.tabs.active_mut().map(Tab::active_mut)
    }

    /// 换标签 / 改标签结构以后，旧的选区、悬停链接、待确认粘贴都不能留。
    fn reset_interaction(&mut self) {
        self.selection = None;
        self.selecting = false;
        self.drag_anchor = None;
        self.hover_link = None;
        self.ime_preedit.clear();
        self.note_ime_preedit();
        // 标签 / 窗格结构变了，待确认的粘贴作废：不静默，告诉用户要重来。
        if self.pending_paste.take().is_some() {
            self.status.announce(StatusLevel::Info, paste_cancelled());
        }
        self.mouse_pressed = false;
        self.last_mouse_cell = None;
        self.dragging_split = None;
        self.context_menu = None;
    }

    fn refresh(&mut self) {
        if let Some(pane) = self.pane_mut() {
            pane.refresh();
        }
    }

    /// 每帧取一下所有窗格的会话事件；后台标签也继续消费。一帧的数据总量由 `frame_budget`
    /// 在所有窗格之间分配（前台优先、后台轮转），不是每个窗格各吃一份。
    fn poll_sessions(&mut self) -> bool {
        let active = self.tabs.active_index();
        self.sync_assist_restrictions(active);
        let mut panes: Vec<(bool, &mut Pane)> = Vec::new();
        for (index, tab) in self.tabs.items_mut().iter_mut().enumerate() {
            let foreground = Some(index) == active;
            panes.extend(tab.panes.iter_mut().map(|pane| (foreground, pane)));
        }
        let foreground: Vec<bool> = panes.iter().map(|(front, _)| *front).collect();
        self.frame_budget.run(
            panes.len(),
            |i| foreground[i],
            |i, allowance| panes[i].1.poll_events_within(allowance),
        )
    }

    /// 终端内命令建议用的快捷命令快照（命令正文 + 是否勾了"排除自动建议"）。内容没变就不换，
    /// 换了各窗格下一帧收到新的，旧候选作废。
    pub(crate) fn sync_quick_sources(&mut self, wanted: &[(&str, bool)]) {
        let same = self.assist_quick.len() == wanted.len()
            && self
                .assist_quick
                .iter()
                .zip(wanted)
                .all(|(have, (text, excluded))| have.text == *text && have.excluded == *excluded);
        if !same {
            self.assist_quick = Arc::new(
                wanted
                    .iter()
                    .map(|(text, excluded)| QuickSource {
                        text: (*text).to_owned(),
                        excluded: *excluded,
                    })
                    .collect(),
            );
        }
    }

    /// 把锁屏、广播、模态框这些"此刻不该给建议"的条件告诉每个窗格的输入跟踪。
    /// 广播只作用于它所在的标签；模态框（粘贴确认、右键菜单）只挡住活动标签的聚焦窗格。
    fn sync_assist_restrictions(&mut self, active: Option<usize>) {
        let locked = crate::app_lock::is_locked();
        let prefs = crate::app_prefs::cached();
        let (enabled, history_enabled) =
            (prefs.terminal_assist_enabled, prefs.terminal_assist_history);
        let modal = self.pending_paste.is_some() || self.context_menu.is_some();
        let now = Instant::now();
        let quick = Arc::clone(&self.assist_quick);
        for (index, tab) in self.tabs.items_mut().iter_mut().enumerate() {
            let (broadcast, revision) = (tab.broadcast, tab.broadcast_revision);
            let focused = tab.active_pane;
            let foreground = Some(index) == active;
            for (pane_index, pane) in tab.panes.iter_mut().enumerate() {
                pane.assist.set_broadcast_revision(revision, now);
                pane.assist.set_history_enabled(history_enabled);
                pane.assist.set_quick_sources(&quick);
                // 候选只给活动标签的聚焦窗格；别的窗格不生成、不展示。
                pane.assist
                    .set_suggest_active(foreground && pane_index == focused);
                pane.set_assist_restrictions(
                    locked,
                    broadcast,
                    modal && foreground && pane_index == focused,
                    enabled,
                );
            }
        }
    }

    /// 输入法的预编辑开始 / 结束：告诉聚焦窗格的输入跟踪（预编辑期间不记录、不检索、不填入）。
    pub(super) fn note_ime_preedit(&mut self) {
        let composing = !self.ime_preedit.is_empty();
        let now = Instant::now();
        let active = self.tabs.active_index();
        for (index, tab) in self.tabs.items_mut().iter_mut().enumerate() {
            let focused = tab.active_pane;
            for (pane_index, pane) in tab.panes.iter_mut().enumerate() {
                // 预编辑只属于聚焦窗格；别的窗格（含别的标签）不能残留"正在预编辑"。
                let mine = composing && Some(index) == active && pane_index == focused;
                pane.assist.on_ime_preedit(mine, now);
            }
        }
    }

    /// 关闭所有会话（窗口关闭前调用），避免 SSH 通道挂着。
    pub fn close_all(&mut self) {
        for tab in self.tabs.items_mut() {
            for pane in &mut tab.panes {
                pane.close();
            }
        }
    }

    fn close_tab_panes(mut tab: Tab) {
        for pane in &mut tab.panes {
            pane.close();
        }
    }

    // ---- 标签操作 ----

    pub(crate) fn select_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.tabs.select(index) {
            self.reset_interaction();
            self.refresh();
            cx.notify();
        }
    }

    /// Ctrl+Tab / Ctrl+Shift+Tab。
    pub(crate) fn cycle_tab(&mut self, step: isize, cx: &mut Context<Self>) -> bool {
        if !self.tabs.cycle(step) {
            return false;
        }
        self.reset_interaction();
        self.refresh();
        cx.notify();
        true
    }

    /// 在新标签里连一台主机（后台连接，立刻可见）。返回新标签的下标。
    pub(crate) fn open_host_tab(
        &mut self,
        host_id: i64,
        title: String,
        cx: &mut Context<Self>,
    ) -> usize {
        self.open_host_tab_with_factory(host_id, title, None, cx)
    }

    /// 同上；给了已认证连接就在这条连接上再开一个 shell，不再握手。
    pub(crate) fn open_host_tab_with_factory(
        &mut self,
        host_id: i64,
        title: String,
        factory: Option<Arc<crate::ssh_backend::SshSessionFactory>>,
        cx: &mut Context<Self>,
    ) -> usize {
        let history = self.settings.scrollback;
        let pane = Pane::start_ssh(
            factory,
            Some(host_id),
            self.default_size,
            history,
            next_generation(),
        );
        let index = self.tabs.push(Tab::new(title, pane));
        self.reset_interaction();
        cx.notify();
        index
    }

    /// 快速连接：在新标签里连 `user@host:port`（不保存主机）。返回新标签的下标。
    pub(crate) fn open_quick_tab(
        &mut self,
        target: zssh_core::model::QuickTarget,
        password: String,
        label: String,
        cx: &mut Context<Self>,
    ) -> usize {
        let spec = Arc::new(QuickSpec {
            target,
            password,
            label: label.clone(),
        });
        let history = self.settings.scrollback;
        let pane = Pane::start_quick(spec, None, self.default_size, history, next_generation());
        let index = self.tabs.push(Tab::new(label, pane));
        self.reset_interaction();
        cx.notify();
        index
    }

    /// 调试窗口用：丢掉现有的所有标签，只留一个正在连这台主机的标签。
    #[cfg(debug_assertions)]
    pub(crate) fn replace_with_host_tab(
        &mut self,
        host_id: i64,
        factory: Option<Arc<crate::ssh_backend::SshSessionFactory>>,
        size: Size,
        cx: &mut Context<Self>,
    ) {
        let history = self.settings.scrollback;
        let pane = Pane::start_ssh(factory, Some(host_id), size, history, next_generation());
        self.replace_with_tab(Tab::new(
            format!("{}{host_id}", crate::i18n::t(TermMsg::TabTitlePrefix)),
            pane,
        ));
        cx.notify();
    }

    /// 调试窗口顶部那行：窗格尺寸、历史、滚动偏移、流控账本。
    #[cfg(debug_assertions)]
    pub(crate) fn stats_line(&self) -> String {
        let Some(pane) = self.pane() else {
            return String::new();
        };
        let snapshot = &pane.snapshot;
        let ledger = pane.pump().ledger();
        format!(
            "{} 行 x {} 列  历史 {}  滚动 {}  已喂 {}  raw {}  acked {}", // i18n-allow: 调试钩子
            snapshot.size.rows,
            snapshot.size.cols,
            snapshot.history_size,
            pane.pump().display_offset(),
            pane.fed,
            ledger.raw,
            ledger.acked
        )
    }

    /// 「+」：一个空白标签，等用户在里面选主机。
    pub(crate) fn new_empty_tab(&mut self, cx: &mut Context<Self>) -> usize {
        let history = self.settings.scrollback;
        let pane = Pane::placeholder(
            self.default_size,
            history,
            next_generation(),
            None,
            PaneState::Empty,
        );
        let index = self
            .tabs
            .push(Tab::new(crate::i18n::t(TermMsg::NewTab).to_owned(), pane));
        self.reset_interaction();
        cx.notify();
        index
    }

    /// 在活动的空白标签里连一台主机（0.2.14：空白标签选主机就在原地连，不再另开）。
    pub(crate) fn connect_active_empty(
        &mut self,
        host_id: i64,
        title: String,
        cx: &mut Context<Self>,
    ) -> bool {
        let size = self.default_size;
        let history = self.settings.scrollback;
        let Some(tab) = self.tabs.active_mut() else {
            return false;
        };
        if !tab.is_blank() {
            return false;
        }
        tab.title = title;
        // 空白标签里即使被分过屏，选了主机也是整个标签换成一个窗格。
        tab.reset_to(Pane::start_ssh(
            None,
            Some(host_id),
            size,
            history,
            next_generation(),
        ));
        self.reset_interaction();
        cx.notify();
        true
    }

    /// 复制标签：紧挨在原标签右边再开一个连同一台主机的标签。活的会话复用它的已认证
    /// 连接（只多开一个 shell）；断线的会话连接已经死了，要重新连。空白标签没有可复制的。
    pub(crate) fn duplicate_tab(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        let Some(tab) = self.tabs.get(index) else {
            return false;
        };
        let source = tab.active();
        if source.host_id.is_none() && source.quick.is_none() {
            return false;
        }
        let factory = if source.state == PaneState::Live {
            source.factory.clone()
        } else {
            None
        };
        let size = source.snapshot.size;
        let title = tab.title.clone();
        let history = self.settings.scrollback;
        let pane = match (&source.quick, source.host_id) {
            (Some(spec), _) => {
                Pane::start_quick(Arc::clone(spec), factory, size, history, next_generation())
            }
            (None, host_id) => Pane::start_ssh(factory, host_id, size, history, next_generation()),
        };
        self.tabs.insert_after(index, Tab::new(title, pane));
        self.reset_interaction();
        cx.notify();
        true
    }

    /// 关掉一个标签（可以关到一个不剩）。
    pub(crate) fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        let Some(tab) = self.tabs.remove(index) else {
            return false;
        };
        Self::close_tab_panes(tab);
        self.reset_interaction();
        self.refresh();
        cx.notify();
        true
    }

    pub(crate) fn close_other_tabs(&mut self, keep: usize, cx: &mut Context<Self>) -> usize {
        let removed = self.tabs.remove_others(keep);
        self.finish_bulk_close(removed, cx)
    }

    pub(crate) fn close_tabs_left(&mut self, index: usize, cx: &mut Context<Self>) -> usize {
        let removed = self.tabs.remove_left(index);
        self.finish_bulk_close(removed, cx)
    }

    pub(crate) fn close_tabs_right(&mut self, index: usize, cx: &mut Context<Self>) -> usize {
        let removed = self.tabs.remove_right(index);
        self.finish_bulk_close(removed, cx)
    }

    pub(crate) fn close_all_tabs(&mut self, cx: &mut Context<Self>) -> usize {
        let removed = self.tabs.remove_all();
        self.finish_bulk_close(removed, cx)
    }

    fn finish_bulk_close(&mut self, removed: Vec<Tab>, cx: &mut Context<Self>) -> usize {
        let count = removed.len();
        for tab in removed {
            Self::close_tab_panes(tab);
        }
        if count > 0 {
            self.reset_interaction();
            self.refresh();
            cx.notify();
        }
        count
    }

    /// Ctrl+Shift+W：分屏时关聚焦的窗格，否则关整个标签。返回有没有动。
    pub(crate) fn close_focused_pane_or_tab(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(index) = self.tabs.active_index() else {
            return false;
        };
        let Some(tab) = self.tabs.get_mut(index) else {
            return false;
        };
        if !tab.is_split() {
            return self.close_tab(index, cx);
        }
        let at = tab.active_pane.min(tab.panes.len() - 1);
        self.close_pane_index(at, cx)
    }

    /// 关掉活动标签里的第 `index` 个窗格：兄弟节点顶掉它的位置，焦点按 0.2.14 的规则挪。
    /// 只剩一个窗格时不动（要关整个标签用 `close_tab`）。
    pub(crate) fn close_pane_index(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        let Some(tab) = self.tabs.active_mut() else {
            return false;
        };
        let Some(mut pane) = tab.close_pane_at(index) else {
            return false;
        };
        pane.close();
        self.reset_interaction();
        self.refresh();
        cx.notify();
        true
    }

    /// 关掉活动标签里编号为 `pane_id` 的窗格（右键菜单「关闭窗格」）：和 Ctrl+Shift+W 关聚焦窗格是同一条
    /// 路径（`close_pane_index`，兄弟顶上、焦点规则不变），只是目标是右键点中的窗格。只剩一个窗格时不动。
    pub(crate) fn close_pane_by_id(&mut self, pane_id: PaneId, cx: &mut Context<Self>) -> bool {
        let Some(index) = self
            .tabs
            .active()
            .and_then(|tab| tab.panes.iter().position(|pane| pane.id == pane_id))
        else {
            return false;
        };
        self.close_pane_index(index, cx)
    }

    /// 按标签编号取出一个标签（拖出窗口 / 合并到别的窗口用）：同一个 `Tab` 交出去，会话不断、不重连。
    /// 和 `take_active_tab` 不同，最后一个标签也能取（主窗口没有终端标签时显示主机页）。
    pub(crate) fn take_tab_by_id(
        &mut self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) -> Option<(usize, Tab)> {
        let index = self.tabs.items().iter().position(|tab| tab.id == tab_id)?;
        let tab = self.tabs.remove(index)?;
        self.reset_interaction();
        self.refresh();
        cx.notify();
        Some((index, tab))
    }

    /// 聚焦活动标签的第 `index` 个窗格（验证钩子用；鼠标点窗格走 `focus_pane`）。
    #[cfg(debug_assertions)]
    pub(crate) fn focus_pane_index(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        let Some(tab) = self.tabs.active() else {
            return false;
        };
        if index >= tab.panes.len() {
            return false;
        }
        self.focus_pane(index);
        cx.notify();
        true
    }

    /// 取出活动标签（搬到别的窗口用）；最后一个标签不取，返回 `None`。
    #[cfg(debug_assertions)]
    pub(crate) fn take_active_tab(&mut self, cx: &mut Context<Self>) -> Option<(usize, Tab)> {
        if self.tabs.len() <= 1 {
            return None;
        }
        let index = self.tabs.active_index()?;
        let tab = self.tabs.remove(index)?;
        self.reset_interaction();
        self.refresh();
        cx.notify();
        Some((index, tab))
    }

    /// 放回被取走的标签（新窗口没建起来时），回到原来的位置并设为活动。
    pub(crate) fn restore_tab(&mut self, index: usize, tab: Tab, cx: &mut Context<Self>) {
        let mut rest = self.tabs.remove_all();
        let at = index.min(rest.len());
        rest.insert(at, tab);
        for item in rest {
            self.tabs.push(item);
        }
        self.tabs.select(at);
        self.reset_interaction();
        self.refresh();
        cx.notify();
    }

    /// 接管另一个窗口合并过来的标签：同一个 `Tab` 直接接过来，会话不断、不重连。
    pub(crate) fn adopt_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tabs.push(tab);
        self.reset_interaction();
        self.refresh();
        cx.notify();
    }

    /// 新窗口接管一个已经存在的标签：先结束原有窗格，日志和会话不能留在新窗口里。
    pub(crate) fn replace_with_tab(&mut self, tab: Tab) {
        for old in self.tabs.remove_all() {
            Self::close_tab_panes(old);
        }
        self.tabs.push(tab);
        self.reset_interaction();
        self.refresh();
    }

    /// 离线演示标签（不连任何主机，键入内容本地回显）：只有调试窗口用。
    #[cfg(debug_assertions)]
    pub(crate) fn push_offline_tab(&mut self, title: &str, size: Size) -> usize {
        let pane = Pane::new(size, self.settings.scrollback, next_generation());
        let index = self.tabs.push(Tab::new(title.to_owned(), pane));
        self.reset_interaction();
        index
    }

    /// 调试驱动用：开一个离线演示标签（本地回显，不连任何主机），用来截光标样式这类外观。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_offline_tab(&mut self, cx: &mut Context<Self>) {
        let size = self.default_size;
        self.push_offline_tab(crate::i18n::t(TermMsg::OfflineDemo), size);
        cx.notify();
    }

    /// 调试驱动用：直接设选区（可见区坐标）。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_select(
        &mut self,
        anchor: (usize, usize),
        head: (usize, usize),
        cx: &mut Context<Self>,
    ) {
        self.selection = Some((anchor, head));
        cx.notify();
    }

    /// 调试驱动用：现在有没有非空的选区文字。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_selected_text(&self) -> String {
        self.selected_text()
    }

    /// 调试驱动用：活动标签第 `pane` 个窗格的断线遮罩（`None` = 没有这个窗格；`Some(None)` = 没盖遮罩）。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_overlay(&self, pane: usize) -> Option<Option<String>> {
        let pane = self.tabs.active()?.panes.get(pane)?;
        Some(pane.overlay_reason().map(str::to_owned))
    }

    /// 调试驱动用：活动标签第 `pane` 个窗格是不是翻在回滚里（离开了最底下）。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_scrolled_back(&self, pane: usize) -> Option<bool> {
        let pane = self.tabs.active()?.panes.get(pane)?;
        Some(pane.snapshot.display_offset != 0)
    }

    /// 调试驱动用：这段文字在聚焦窗格里第一次出现的位置，返回它中间那一格的中心（窗口逻辑像素）。
    /// 按字符算列，所以只对前面没有宽字符的行准。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_text_center(&self, text: &str) -> Option<(f32, f32)> {
        let tab = self.tabs.active()?;
        let geom = self.pane_geom.get(tab.active_pane)?;
        for (row, line) in tab.active().snapshot.rows.iter().enumerate() {
            if let Some(byte) = line.text.find(text) {
                let col = line.text[..byte].chars().count() + text.chars().count() / 2;
                return Some((
                    geom.left + (col as f32 + 0.5) * self.cell_width,
                    geom.top + (row as f32 + 0.5) * self.cell_height,
                ));
            }
        }
        None
    }

    /// 调试驱动用：搜索条现在的查询和计数（没开是 `None`）。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_search_state(&self) -> Option<(String, String)> {
        self.search
            .as_ref()
            .map(|state| (state.query.clone(), state.count_label()))
    }

    /// 调试驱动用：弹着的右键菜单的条目（没弹是 `None`）。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_menu_entries(&self) -> Option<Vec<crate::terminal_menu::Entry>> {
        self.context_menu.as_ref().map(|open| open.entries.clone())
    }

    /// 调试驱动用：选菜单里的一项（和点它走同一个 `run_menu_item`）；置灰的项选不了。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_menu_pick(
        &mut self,
        label: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(open) = self.context_menu.as_ref() else {
            return Err("菜单没有弹出".to_owned()); // i18n-allow: 调试钩子
        };
        let Some(entry) = open.entries.iter().find(|entry| entry.label == label) else {
            return Err("菜单里没有这一项".to_owned()); // i18n-allow: 调试钩子
        };
        if !entry.enabled {
            return Err("这一项是置灰的".to_owned()); // i18n-allow: 调试钩子
        }
        let (item, link, pane) = (entry.item, open.link.clone(), open.pane);
        self.context_menu = None;
        self.run_menu_item(item, pane, link.as_deref(), window, cx);
        Ok(())
    }

    /// 调试驱动用：关掉右键菜单（不选任何项），焦点回终端。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_menu_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.context_menu = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// 调试驱动用：「右键粘贴」设成指定值（走设置页开关同一个方法）。
    #[cfg(debug_assertions)]
    /// 调试钩子：这个窗口的设置读缓存里某一项的文字（跨窗口一致性断言用，不读盘）。
    pub(crate) fn dev_settings_text(&self, field: &str, cx: &App) -> Result<String, String> {
        Ok(match field {
            "font_size" => format!("{}", self.settings.font_size),
            "dark" => (if crate::app_theme::is_dark(cx) {
                "yes"
            } else {
                "no"
            })
            .to_owned(),
            "follow_system" => (if self.settings.follow_system {
                "on"
            } else {
                "off"
            })
            .to_owned(),
            "highlight" => (if self.settings.highlight_enabled {
                "on"
            } else {
                "off"
            })
            .to_owned(),
            "rules" => self.highlight.rules.len().to_string(),
            "revision" => self.settings_rev.to_string(),
            _ => {
                return Err(
                    "字段只能是 font_size / dark / follow_system / highlight / rules / revision" // i18n-allow: 调试钩子
                        .to_owned(),
                );
            }
        })
    }

    #[cfg(debug_assertions)]
    pub(crate) fn dev_right_click_paste(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.settings.right_click_paste != on {
            self.toggle_right_click_paste(cx);
        }
    }

    /// 调试驱动用：键盘焦点现在在终端上吗。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_terminal_focused(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
    }

    /// 调试驱动用：给活动标签的窗格摆延迟值（`pane` 为空就是所有窗格；`sample` 为空是清掉）。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_latency_set(
        &mut self,
        sample: Option<latency::Sample>,
        pane: Option<usize>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let tab = self.tabs.active_mut().ok_or("没有标签")?; // i18n-allow: 调试钩子
        if pane.is_some_and(|at| at >= tab.panes.len()) {
            return Err("没有这个窗格".to_owned()); // i18n-allow: 调试钩子
        }
        for (at, target) in tab.panes.iter_mut().enumerate() {
            if pane.is_none_or(|wanted| wanted == at) {
                target.dev_set_latency(sample);
            }
        }
        cx.notify();
        Ok(())
    }

    /// 调试驱动用：第 `tab` 个标签第 `pane` 个窗格的（延迟监视器登记的"看得见"数, 已有样本数）；
    /// 没有监视器是 `None`。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_latency_state(&self, tab: usize, pane: usize) -> Option<(usize, usize)> {
        let latency = self.tabs.get(tab)?.panes.get(pane)?.latency.as_ref()?;
        let monitor = latency.viewer.monitor();
        Some((monitor.visible_viewers(), monitor.snapshot().stats.len()))
    }

    /// 活动窗格的 shell 在它那条连接上排第几个、这条连接上现在有几个 shell（跟随终端目录用，见
    /// `zssh_core::ssh::cwd`）。没连上、不是 SSH 会话都是 `None`。
    pub(crate) fn active_shell_position(
        &self,
    ) -> Option<(Arc<zssh_core::ssh::Connection>, usize, usize)> {
        let tab = self.tabs.active()?;
        let pane = tab
            .panes
            .get(tab.active_pane.min(tab.panes.len().checked_sub(1)?))?;
        if pane.state != PaneState::Live {
            return None;
        }
        let connection = pane.factory.as_ref()?.connection();
        let mut seqs: Vec<u64> = self
            .tabs
            .items()
            .iter()
            .flat_map(|tab| tab.panes.iter())
            .filter(|other| other.state == PaneState::Live)
            .filter(|other| {
                other
                    .factory
                    .as_ref()
                    .is_some_and(|factory| Arc::ptr_eq(&factory.connection(), &connection))
            })
            .map(|other| other.shell_seq)
            .collect();
        seqs.sort_unstable();
        let ordinal = seqs.iter().position(|seq| *seq == pane.shell_seq)?;
        Some((connection, ordinal, seqs.len()))
    }

    /// 往活动窗格写一段内容（离线演示用；已连接的窗格会把它当键入发给远端）。
    #[cfg(debug_assertions)]
    pub(crate) fn write_active(&mut self, bytes: &[u8]) {
        if let Some(pane) = self.pane_mut() {
            pane.write(bytes);
            pane.refresh();
        }
    }

    /// 快捷命令栏和撰写栏往终端发内容：`all` 是这个标签的所有分屏窗格，否则只发聚焦的窗格。
    /// 和键盘输入同一条写路径（`Pane::write`），没连上的窗格丢弃输入；返回真正写出去几个。
    /// 标签开着输入广播时 `all` 由调用方算好（0.2.14 的 `sendCommandToTab`）。
    pub(crate) fn send_to_tab(&mut self, data: &str, all: bool, cx: &mut Context<Self>) -> usize {
        self.selection = None;
        let Some(tab) = self.tabs.active_mut() else {
            return 0;
        };
        let targets: Vec<usize> = if all {
            (0..tab.panes.len()).collect()
        } else {
            vec![tab.active_pane.min(tab.panes.len() - 1)]
        };
        let mut sent = 0;
        for index in targets {
            let pane = &mut tab.panes[index];
            if pane.state != PaneState::Live {
                continue;
            }
            if pane.pump().display_offset() != 0 {
                pane.pump_mut().scroll_to_bottom();
            }
            // 快捷命令栏和撰写栏：不是逐键输入，输入跟踪要求之后重新取得可靠的输入边界。
            let _ = pane.write_as(
                data.as_bytes(),
                WriteOrigin::Foreign(WriteSource::QuickCommand),
            );
            sent += 1;
        }
        cx.notify();
        sent
    }

    /// 活动标签有几个窗格（分屏数）。
    pub(crate) fn pane_count(&self) -> usize {
        self.tabs.active().map_or(0, |tab| tab.panes.len())
    }

    /// 活动标签的编号（AI 对话按标签各自一份）。
    pub(crate) fn active_tab_id(&self) -> Option<u64> {
        self.tabs.active().map(|tab| tab.id)
    }

    /// 聚焦窗格的 AI 终端编号（V2-G10b）：AI 把「这台主机的焦点终端」认成它。没连上 / 离线窗格没有。
    pub(crate) fn active_terminal_id(&self) -> Option<String> {
        self.pane()
            .filter(|pane| pane.state == PaneState::Live)
            .and_then(|pane| pane.terminal_id.clone())
    }

    /// 切到这个终端所在的标签（并聚焦那个窗格）；找不到返回 `false`。
    pub(crate) fn select_terminal(&mut self, terminal_id: &str, cx: &mut Context<Self>) -> bool {
        let found = self
            .tabs
            .items()
            .iter()
            .enumerate()
            .find_map(|(index, tab)| {
                tab.panes
                    .iter()
                    .position(|pane| pane.terminal_id.as_deref() == Some(terminal_id))
                    .map(|pane| (index, pane))
            });
        let Some((index, pane)) = found else {
            return false;
        };
        self.select_tab(index, cx);
        self.focus_pane(pane);
        true
    }

    /// 切到这台主机的一个标签：优先活动标签，其次第一个。「查看终端」在目标还没连上时用它。
    pub(crate) fn select_host_tab(&mut self, host_id: i64, cx: &mut Context<Self>) -> bool {
        let index = self
            .tabs
            .active_index()
            .filter(|index| {
                self.tabs.items()[*index]
                    .panes
                    .iter()
                    .any(|p| p.host_id == Some(host_id))
            })
            .or_else(|| {
                self.tabs
                    .items()
                    .iter()
                    .position(|tab| tab.panes.iter().any(|p| p.host_id == Some(host_id)))
            });
        match index {
            Some(index) => {
                self.select_tab(index, cx);
                true
            }
            None => false,
        }
    }

    /// 活动标签每个窗格的屏幕文字（核对「发到了哪几个窗格」用）。
    #[cfg(debug_assertions)]
    pub(crate) fn pane_texts(&self) -> Vec<String> {
        self.tabs
            .active()
            .map(|tab| {
                tab.panes
                    .iter()
                    .map(|pane| {
                        pane.snapshot
                            .rows
                            .iter()
                            .map(|row| row.text.as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 聚焦窗格的下标（活动标签）。
    #[cfg(debug_assertions)]
    pub(crate) fn focused_pane_index(&self) -> Option<usize> {
        self.tabs
            .active()
            .map(|tab| tab.active_pane.min(tab.panes.len() - 1))
    }

    /// 所有标签所有窗格现在存着多少行回滚。
    #[cfg(debug_assertions)]
    pub(crate) fn history_sizes(&self) -> Vec<usize> {
        self.tabs
            .items()
            .iter()
            .flat_map(|tab| tab.panes.iter().map(|pane| pane.pump().history_size()))
            .collect()
    }

    /// 终端实际用的字体名（验证钩子核对「栈里没装的字体会回退」）。
    #[cfg(debug_assertions)]
    pub(crate) fn resolved_font(&mut self, cx: &App) -> String {
        self.mono_family(cx).to_string()
    }

    /// 所有标签所有窗格现在的回滚上限（验证钩子核对「已开的窗格也用了新设置」）。
    #[cfg(debug_assertions)]
    pub(crate) fn history_limits(&self) -> Vec<usize> {
        self.tabs
            .items()
            .iter()
            .flat_map(|tab| tab.panes.iter().map(|pane| pane.pump().history_limit()))
            .collect()
    }

    /// 活动标签所有分割点的 (编号, 方向, 比例)。
    #[cfg(debug_assertions)]
    pub(crate) fn split_ratios(&self) -> Vec<(u64, Axis, f32)> {
        self.tabs
            .active()
            .map(|tab| tab.tree.splits())
            .unwrap_or_default()
    }

    /// 活动标签上一帧的窗格布局，一个窗格一行：下标、是否聚焦、矩形、行列数；再加分隔条。
    /// 只写数字，不写屏幕内容（验证钩子写进日志用）。
    #[cfg(debug_assertions)]
    pub(crate) fn pane_report(&self) -> Vec<String> {
        let Some(tab) = self.tabs.active() else {
            return Vec::new();
        };
        let mut lines = Vec::new();
        for (index, pane) in tab.panes.iter().enumerate() {
            let rect = self.layout.rect_of(pane.id).unwrap_or_default();
            lines.push(format!(
                "pane {index}{} rect=({:.0},{:.0},{:.0},{:.0}) grid={}x{}",
                if index == tab.active_pane {
                    " focus"
                } else {
                    ""
                },
                rect.x,
                rect.y,
                rect.w,
                rect.h,
                pane.snapshot.size.cols,
                pane.snapshot.size.rows,
            ));
        }
        for divider in &self.layout.dividers {
            let rect = divider.rect;
            lines.push(format!(
                "divider {:?} rect=({:.0},{:.0},{:.0},{:.0}) origin=({:.0},{:.0})",
                divider.axis,
                rect.x,
                rect.y,
                rect.w,
                rect.h,
                self.area_origin.0,
                self.area_origin.1
            ));
        }
        lines
    }

    /// 终端区域（窗口坐标）的左上角和大小：验证钩子按它算要点的坐标。
    #[cfg(debug_assertions)]
    pub(crate) fn area_rect(&self) -> Option<(f32, f32, f32, f32)> {
        self.area.get().map(|bounds| {
            (
                f32::from(bounds.origin.x),
                f32::from(bounds.origin.y),
                f32::from(bounds.size.width),
                f32::from(bounds.size.height),
            )
        })
    }

    /// 核对活动标签的布局铺满终端区域、没有重叠，且每个窗格的终端尺寸和它的矩形一致
    /// （窗口里真的重排过，不是只改了数字）。
    #[cfg(debug_assertions)]
    pub(crate) fn check_layout(&self) -> Result<(), String> {
        let Some(bounds) = self.area.get() else {
            return Err("还没有量到终端区域".to_owned()); // i18n-allow: 调试钩子
        };
        let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let tab = self.tabs.active().ok_or("没有标签")?; // i18n-allow: 调试钩子
        // 上一帧画的布局要和树现在的比例对得上：拖完分隔条、关完窗格之后，下一帧才会重排。
        if split_tree::compute_layout(&tab.tree, w, h) != self.layout {
            return Err("窗口还没有按最新的分屏树重排".to_owned()); // i18n-allow: 调试钩子
        }
        split_tree::check_tiling(&self.layout, w, h)?;
        for (index, (pane, geom)) in tab.panes.iter().zip(&self.pane_geom).enumerate() {
            let want = Size {
                rows: geom.rows,
                cols: geom.cols,
            };
            if pane.snapshot.size != want {
                return Err(format!(
                    "窗格 {index} 的终端是 {}x{}，矩形算出来是 {}x{}", // i18n-allow: 调试钩子
                    pane.snapshot.size.cols, pane.snapshot.size.rows, want.cols, want.rows
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn toggle_broadcast(&mut self, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.active_mut() {
            tab.broadcast = !tab.broadcast;
            tab.broadcast_revision += 1;
            cx.notify();
        }
    }

    /// Ctrl+Shift+D（左右，`Axis::X`）/ Ctrl+Shift+E（上下，`Axis::Y`）：把聚焦的窗格一分为二，
    /// 新窗格在右 / 下并拿到焦点。可以接着拆，没有个数上限（同 0.2.14 的 `splitAt`）。
    /// 新窗格复用聚焦窗格的已认证连接再开一个 shell；断线的连接已经死了，重新连。
    pub(crate) fn split_active(&mut self, axis: Axis, cx: &mut Context<Self>) -> bool {
        let history = self.settings.scrollback;
        let Some(tab) = self.tabs.active_mut() else {
            return false;
        };
        let source = tab.active();
        let size = source.snapshot.size;
        let host_id = source.host_id;
        let quick = source.quick.clone();
        let blank = source.state == PaneState::Empty;
        let factory = if source.state == PaneState::Live {
            source.factory.clone()
        } else {
            None
        };
        // 新窗格先按一半的尺寸申请 PTY，下一帧排版会按实际矩形再对一次。
        let half = match axis {
            Axis::X => Size {
                rows: size.rows,
                cols: (size.cols / 2).max(20),
            },
            Axis::Y => Size {
                rows: (size.rows / 2).max(2),
                cols: size.cols,
            },
        };
        let mut pane = if blank {
            Pane::placeholder(half, history, next_generation(), None, PaneState::Empty)
        } else if let Some(spec) = quick.clone() {
            Pane::start_quick(spec, factory, half, history, next_generation())
        } else {
            Pane::start_ssh(factory, host_id, half, history, next_generation())
        };
        if host_id.is_none() && quick.is_none() && !blank {
            for i in 0..5 {
                let _ = pane.write_as(
                    format!("split pane line {i}\r\n").as_bytes(),
                    WriteOrigin::Untracked,
                );
            }
            pane.fed = 5;
        }
        tab.split_active(axis, pane);
        self.reset_interaction();
        cx.notify();
        true
    }

    /// 调试窗口的分屏按钮：没分屏就左右拆，已经分屏就收掉最后一个窗格。
    #[cfg(debug_assertions)]
    pub(crate) fn toggle_split(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.active() else {
            return;
        };
        if tab.is_split() {
            let last = tab.panes.len() - 1;
            self.close_pane_index(last, cx);
        } else {
            self.split_active(Axis::X, cx);
        }
    }

    /// 重连一个标签里所有已断开的窗格（标签右键「重新连接」）；返回重连了几个。
    pub(crate) fn reconnect_tab(&mut self, index: usize, cx: &mut Context<Self>) -> usize {
        let Some(tab) = self.tabs.get(index) else {
            return 0;
        };
        let targets: Vec<usize> = tab
            .panes
            .iter()
            .enumerate()
            .filter(|(_, pane)| pane.state == PaneState::Disconnected && pane.is_session())
            .map(|(at, _)| at)
            .collect();
        self.status
            .set(StatusLevel::Info, crate::i18n::t(TermMsg::Reconnecting));
        for &at in &targets {
            self.reconnect_pane(index, at);
        }
        if !targets.is_empty() {
            self.reset_interaction();
            self.refresh();
            cx.notify();
        }
        targets.len()
    }

    /// 断开一个标签里所有还连着的窗格（标签右键「断开连接」，分屏的每个窗格都断）；标签本身留着，
    /// 状态变成已断开，可以「重新连接」。返回断开了几个窗格。
    /// 给这个标签聚焦的窗格发 Break（串口）。窗格已经断开返回 `false`。
    pub(crate) fn send_break_tab(&mut self, index: usize) -> bool {
        let Some(tab) = self.tabs.get_mut(index) else {
            return false;
        };
        let pane = tab.active_mut();
        let live = pane.state == PaneState::Live;
        pane.send_break();
        live
    }

    /// 活动标签聚焦的窗格发 Break。
    pub(crate) fn send_break_active(&mut self) -> bool {
        let Some(tab) = self.tabs.active_mut() else {
            return false;
        };
        let pane = tab.active_mut();
        let live = pane.state == PaneState::Live;
        pane.send_break();
        live
    }

    pub(crate) fn disconnect_tab(&mut self, index: usize, cx: &mut Context<Self>) -> usize {
        let Some(tab) = self.tabs.get_mut(index) else {
            return 0;
        };
        let count = tab.disconnect_sessions();
        if count > 0 {
            // 标签上的状态点已经变了，不再弹通知。
            self.status
                .set(StatusLevel::Info, crate::i18n::t(TermMsg::Disconnected));
            self.reset_interaction();
            self.refresh();
            cx.notify();
        }
        count
    }

    /// 重建一个窗格的连接：先收掉旧窗格（日志和通道都要结束，否则新旧两条会同时跑），
    /// 之前在记录就继续记到新文件。只在用户操作时调用，不做自动重连。
    fn reconnect_pane(&mut self, tab_index: usize, pane_index: usize) {
        let history = self.settings.scrollback;
        let Some(pane) = self
            .tabs
            .get_mut(tab_index)
            .and_then(|tab| tab.panes.get_mut(pane_index))
        else {
            return;
        };
        if let Some(logging) = pane.reconnect_in_place(history, None)
            && logging
        {
            let prefs = crate::app_prefs::cached();
            // 断线重连后续记日志：续不上要告诉用户（日志在断档），续上了告知写到哪。
            let outcome = pane.toggle_log_with(&prefs);
            self.status.announce(outcome.level(), outcome.status_text());
        }
    }

    /// 点断线遮罩 / 在遮罩上按 R：重连活动标签里的第 `pane_index` 个窗格，并让它成为聚焦窗格。
    /// 和 Ctrl+Shift+R、标签右键「重新连接」走同一个 [`Self::reconnect_pane`]；只对盖着遮罩的窗格生效
    /// （广播开着也只重连这一个）。返回是否真的开始了重连。
    fn reconnect_overlay_pane(&mut self, pane_index: usize, cx: &mut Context<Self>) -> bool {
        let Some(index) = self.tabs.active_index() else {
            return false;
        };
        let covered = self
            .tabs
            .get(index)
            .and_then(|tab| tab.panes.get(pane_index))
            .is_some_and(|pane| pane.overlay_reason().is_some());
        if !covered {
            return false;
        }
        self.focus_pane(pane_index);
        // 先写提示：之前在记录日志时，新窗格续记会把状态改成日志路径，那个更有用。
        self.status
            .set(StatusLevel::Info, crate::i18n::t(TermMsg::Reconnecting));
        self.reconnect_pane(index, pane_index);
        self.reset_interaction();
        self.refresh();
        cx.notify();
        true
    }

    /// Ctrl+Shift+R：重连当前窗格；离线窗格只提示。
    pub(crate) fn reconnect(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.active_index() else {
            return;
        };
        let (pane_index, is_session) = match self.tabs.get(index) {
            Some(tab) => (tab.active_pane, tab.active().is_session()),
            None => return,
        };
        if !is_session {
            // 什么都没发生，得告诉用户为什么。
            self.status.announce(
                StatusLevel::Info,
                crate::i18n::t(TermMsg::NoReconnectOffline),
            );
            cx.notify();
            return;
        }
        // 先写提示：之前在记录日志时，新窗格续记会把状态改成日志路径，那个更有用。
        self.status
            .set(StatusLevel::Info, crate::i18n::t(TermMsg::Reconnecting));
        self.reconnect_pane(index, pane_index);
        self.reset_interaction();
        self.refresh();
        cx.notify();
    }

    // ---- 键盘 ----

    /// 按键入口：处理交给 [`Self::handle_key_down`]，之后在注册了文字输入处理器的平台（Linux、macOS，见 `ime_ops`）
    /// 上拦下带字符的按键。窗口层在 `KeyDown` 没被拦下时，还会把 `key_char` 当文字再交给输入处理器插一遍，
    /// 不拦就会让每个字符发两次。终端没有焦点（别的输入框有焦点）时不拦，让那个输入框自己收。
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let focused = self.focus.is_focused(window);
        self.handle_key_down(event, window, cx);
        if self.ime_input && focused && event.keystroke.key_char.is_some() {
            cx.stop_propagation();
        }
    }

    /// 按键编码后直接发给活动窗格的会话（离线演示窗格会本地回显）。
    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 按键从子元素冒泡上来：别的输入框有焦点时终端不能抢键。
        if !self.focus.is_focused(window) {
            return;
        }
        if self.tabs.is_empty() {
            return;
        }
        let keystroke = &event.keystroke;
        let ctrl = keystroke.modifiers.control;
        let shift = keystroke.modifiers.shift;
        let key = keystroke.key.as_str();
        // 粘贴确认框开着是模态的：Esc 取消、回车粘贴（沿用 0.2.14），其余按键一律不进终端。
        if self.pending_paste.is_some() {
            match key {
                "escape" => self.cancel_paste(window, cx),
                "enter" => self.confirm_paste(window, cx),
                _ => {}
            }
            return;
        }
        // 打字时光标先亮着，停下来才闪（和常见终端一致）。
        self.cursor_blink.reset();

        // 命令建议：只有显式进入"选择建议"之后，Tab / Enter / 上下键才归它；其余时候（含 Alt、Ctrl+Alt
        // 方向键）原样往下走，编码后发给 shell。Esc 只在候选列表正显示时才被它消费。
        if self.suggest_key_down(event) {
            cx.notify();
            return;
        }

        // Ctrl+Shift+R 重连当前 SSH 窗格；离线标签只提示。
        if ctrl && shift && key.eq_ignore_ascii_case("r") {
            self.reconnect(cx);
            return;
        }

        // 断线遮罩盖着聚焦窗格时，单按 R（不带修饰键）重连这一个窗格（V2-U30）。遮罩只盖已断开的窗格，
        // 所以平时 R 照常进终端；遮罩盖着时别的键都发不出去（`Pane::write` 丢弃），不会排队也不报错。
        // 锁屏时主窗口根本不画终端，这里再兜一次底。
        if is_reconnect_key(key, &keystroke.modifiers)
            && !crate::app_lock::is_locked()
            && let Some(active) = self.tabs.active().map(|tab| tab.active_pane)
            && self.reconnect_overlay_pane(active, cx)
        {
            return;
        }

        // Ctrl+= / Ctrl+- / Ctrl+0 缩放终端字号；macOS 上是 Cmd（Ctrl 组合要照常发给远端）。
        let zoom_mod = if cfg!(target_os = "macos") {
            keystroke.modifiers.platform
        } else {
            ctrl
        };
        if zoom_mod {
            match key {
                "=" | "+" => {
                    self.update_settings(cx, |s| {
                        s.font_size = (s.font_size + 1.0).min(MAX_FONT_SIZE)
                    });
                    cx.notify();
                    return;
                }
                "-" => {
                    self.update_settings(cx, |s| {
                        s.font_size = (s.font_size - 1.0).max(MIN_FONT_SIZE)
                    });
                    cx.notify();
                    return;
                }
                "0" => {
                    self.update_settings(cx, |s| s.font_size = DEFAULT_FONT_SIZE);
                    cx.notify();
                    return;
                }
                _ => {}
            }
        }

        // Shift+PageUp/PageDown 翻页，Shift+Home/End 到顶/到底。
        if shift {
            let rows = self
                .pane()
                .map(|pane| pane.snapshot.size.rows as i32)
                .unwrap_or(0);
            if let Some(pane) = self.pane_mut() {
                match key {
                    "pageup" => {
                        pane.pump_mut().scroll(rows);
                        pane.refresh();
                        cx.notify();
                        return;
                    }
                    "pagedown" => {
                        pane.pump_mut().scroll(-rows);
                        pane.refresh();
                        cx.notify();
                        return;
                    }
                    "home" => {
                        pane.pump_mut().scroll_to_top();
                        pane.refresh();
                        cx.notify();
                        return;
                    }
                    "end" => {
                        pane.pump_mut().scroll_to_bottom();
                        pane.refresh();
                        cx.notify();
                        return;
                    }
                    _ => {}
                }
            }
        }

        // 普通输入先回到当前行，避免在历史里输入。
        if let Some(pane) = self.pane_mut()
            && pane.pump().display_offset() != 0
        {
            pane.pump_mut().scroll_to_bottom();
            pane.refresh();
        }

        // 剪贴板快捷键与现有前端保持一致：
        // - Ctrl+V / Ctrl+Shift+V 粘贴；
        // - Ctrl+C 有选区才复制，没有选区继续发 ^C；
        // - Ctrl+Shift+C 没有选区时吞掉，不能把 ^C 发给远端。
        // macOS 上复制粘贴是 Cmd+C / Cmd+V；Ctrl+C 必须继续发给远端（中断），不能被抢。
        let copy_mod = if cfg!(target_os = "macos") {
            keystroke.modifiers.platform
        } else {
            ctrl
        };
        if copy_mod && key.eq_ignore_ascii_case("v") {
            self.request_paste(window, cx);
            return;
        }
        if copy_mod && key.eq_ignore_ascii_case("c") {
            let selected = self.selected_text();
            match crate::selection::copy_key_action(&selected, shift) {
                crate::selection::CopyKey::Copy => {
                    self.copy_selection(cx);
                    // 和 0.2.14 一致：复制后清掉选区，下一次 Ctrl+C 才能中断命令。
                    self.selection = None;
                    cx.notify();
                    return;
                }
                crate::selection::CopyKey::Swallow => return,
                crate::selection::CopyKey::Interrupt => {}
            }
        }
        if shift && key.eq_ignore_ascii_case("insert") {
            self.request_paste(window, cx);
            return;
        }

        // macOS：Cmd 组合是应用快捷键，上面没接住的也不发给远端（否则 Cmd+K 这类会把字母打进去）。
        if cfg!(target_os = "macos") && keystroke.modifiers.platform {
            return;
        }

        let Some(app_cursor) = self.pane().map(|pane| pane.pump().app_cursor()) else {
            return;
        };
        let alt = alt_as_meta(
            keystroke.modifiers.alt,
            keystroke.key_char.is_some(),
            cfg!(target_os = "macos"),
        );
        let input = KeyInput {
            key: &keystroke.key,
            key_char: keystroke.key_char.as_deref(),
            ctrl: keystroke.modifiers.control,
            alt,
            shift: keystroke.modifiers.shift,
            app_cursor,
        };
        if let Some(bytes) = encode_key(input) {
            // 按键该发什么字节不变；这里只多告诉输入跟踪"这是字符还是交给 shell 的键"。
            let origin = classify_key(
                &keystroke.key,
                keystroke.key_char.as_deref(),
                keystroke.modifiers.control,
                alt,
                &bytes,
            );
            self.send_bytes(&bytes, origin);
            cx.notify();
        }
    }

    // ---- 鼠标 ----

    /// 点在哪个窗格里（含四周的内边距）：点一下就聚焦它。
    fn pane_at(&self, position: ::gpui::Point<::gpui::Pixels>) -> Option<usize> {
        let (x, y) = (f32::from(position.x), f32::from(position.y));
        self.pane_geom
            .iter()
            .position(|geom| geom.rect.contains(x, y))
    }

    /// 命中哪个窗格的哪一格；每个窗格按自己文字区的左上角换算。
    fn cell_at(&self, position: ::gpui::Point<::gpui::Pixels>) -> Option<(usize, usize, usize)> {
        let (x, y) = (f32::from(position.x), f32::from(position.y));
        let tab = self.tabs.active()?;
        for (index, geom) in self.pane_geom.iter().enumerate() {
            let (local_x, local_y) = (x - geom.left, y - geom.top);
            if local_x < 0.0 || local_y < 0.0 {
                continue;
            }
            let col = (local_x / self.cell_width.max(1.0)).floor() as usize;
            let row = (local_y / self.cell_height.max(1.0)).floor() as usize;
            let Some(pane) = tab.panes.get(index) else {
                continue;
            };
            if col >= geom.cols || row >= pane.snapshot.size.rows {
                continue;
            }
            return Some((index, row, col));
        }
        None
    }

    fn link_at(&self, position: ::gpui::Point<::gpui::Pixels>) -> Option<Arc<str>> {
        let (pane_ix, row, col) = self.cell_at(position)?;
        let row_snapshot = self
            .tabs
            .active()?
            .panes
            .get(pane_ix)?
            .snapshot
            .rows
            .get(row)?;
        if let Some(link) = &row_snapshot.cells.get(col)?.hyperlink {
            return Some(link.clone());
        }
        crate::links::find_plain_links(row_snapshot)
            .into_iter()
            .find(|link| col >= link.start_col && col <= link.end_col)
            .map(|link| Arc::from(link.url))
    }

    /// 按下的位置落在分隔条上（命中区 8px，比可见线宽）：单击开始拖，双击恢复平分
    /// （0.2.14 的 `onDoubleClick`）。返回有没有被分隔条接走。
    fn start_split_drag(
        &mut self,
        position: ::gpui::Point<::gpui::Pixels>,
        click_count: usize,
    ) -> bool {
        let Some(tab) = self.tabs.active() else {
            return false;
        };
        if !tab.is_split() {
            return false;
        }
        let rel_x = f32::from(position.x) - self.area_origin.0;
        let rel_y = f32::from(position.y) - self.area_origin.1;
        let Some(divider) = split_tree::divider_at(&self.layout, rel_x, rel_y).copied() else {
            return false;
        };
        if click_count >= 2 {
            if let Some(tab) = self.tabs.active_mut() {
                tab.tree.set_ratio(divider.split_id, 0.5);
            }
            return true;
        }
        self.dragging_split = Some(divider.split_id);
        true
    }

    /// 拖动中：指针位置换算成这条分隔条的比例（两边留出最小窗格，再夹到 0.1～0.9）。
    fn update_split(&mut self, position: ::gpui::Point<::gpui::Pixels>) {
        let Some(split_id) = self.dragging_split else {
            return;
        };
        let Some(divider) = self
            .layout
            .dividers
            .iter()
            .find(|divider| divider.split_id == split_id)
            .copied()
        else {
            return;
        };
        let pointer = match divider.axis {
            Axis::X => f32::from(position.x) - self.area_origin.0,
            Axis::Y => f32::from(position.y) - self.area_origin.1,
        };
        let ratio = split_tree::ratio_from_pointer(&divider, pointer);
        if let Some(tab) = self.tabs.active_mut() {
            tab.tree.set_ratio(split_id, ratio);
        }
    }

    /// 远端接管鼠标时返回它的上报模式；按住 Shift 强制走本地选择（xterm 惯例）。
    fn mouse_report_mode(&self, shift: bool) -> MouseMode {
        let remote = self
            .pane()
            .map(|pane| pane.pump().mouse_mode())
            .unwrap_or_default();
        crate::terminal_menu::effective_mouse_mode(remote, shift)
    }

    /// 上报坐标按聚焦窗格的网格换算；超出范围钳到边缘，避免窗口边距丢掉事件。
    fn mouse_cell_at(&self, position: ::gpui::Point<::gpui::Pixels>) -> (usize, usize) {
        let Some(tab) = self.tabs.active() else {
            return (0, 0);
        };
        let geom = self
            .pane_geom
            .get(tab.active_pane)
            .copied()
            .unwrap_or_default();
        let cols = geom.cols.max(1);
        let rows = tab.active().snapshot.size.rows.max(1);
        let col = ((f32::from(position.x) - geom.left) / self.cell_width.max(1.0)).floor();
        let row = ((f32::from(position.y) - geom.top) / self.cell_height.max(1.0)).floor();
        (
            (row.max(0.0) as usize).min(rows - 1),
            (col.max(0.0) as usize).min(cols - 1),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn report_mouse(
        &mut self,
        action: MouseAction,
        button: TerminalMouseButton,
        position: ::gpui::Point<::gpui::Pixels>,
        shift: bool,
        alt: bool,
        ctrl: bool,
        sgr: bool,
    ) {
        let (row, col) = self.mouse_cell_at(position);
        let bytes = encode_mouse(MouseInput {
            action,
            button,
            col,
            row,
            shift,
            alt,
            ctrl,
            sgr,
        });
        if let Some(pane) = self.pane_mut() {
            // 鼠标上报只在远端接管鼠标（全屏程序）时才有，输入跟踪此时早已暂停，不经它。
            let _ = pane.write_as(&bytes, WriteOrigin::Untracked);
        }
    }

    fn focus_pane(&mut self, pane: usize) {
        if let Some(tab) = self.tabs.active_mut()
            && pane != tab.active_pane
        {
            tab.active_pane = pane;
            self.selection = None;
            self.hover_link = None;
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        // 点终端：先关掉命令建议浮层 / 选择模式，原事件照常处理一次（浮层自己的点击在它那里就拦下了）。
        self.suggest_dismiss();
        // 拖动分屏分隔条：只改比例，不做选择。
        if self.start_split_drag(event.position, event.click_count) {
            cx.notify();
            return;
        }
        // 点在哪个窗格就聚焦哪个（含四周的内边距）；先聚焦，下面的模式判断、选择、上报都按它走。
        if let Some(index) = self.pane_at(event.position) {
            self.focus_pane(index);
            // 左键点在断线遮罩上 = 重连这个窗格（V2-U30）；右键仍可弹菜单（复制回滚里的内容）。
            if event.button == MouseButton::Left
                && !crate::app_lock::is_locked()
                && self.reconnect_overlay_pane(index, cx)
            {
                return;
            }
        }

        let mode = self.mouse_report_mode(event.modifiers.shift);
        if mode.reports_buttons() {
            if let Some(button) = terminal_button(event.button) {
                self.report_mouse(
                    MouseAction::Press,
                    button,
                    event.position,
                    event.modifiers.shift,
                    event.modifiers.alt,
                    event.modifiers.control,
                    mode.sgr,
                );
                self.mouse_pressed = true;
                self.last_mouse_cell = Some(self.mouse_cell_at(event.position));
                cx.notify();
            }
            return;
        }

        if event.button == MouseButton::Right {
            // 上面的上报分支已经把远端接管的情形（没按 Shift）处理掉了，这里只剩粘贴和弹菜单。
            let link = self.link_at(event.position);
            match crate::terminal_menu::right_click_route(
                mode,
                self.settings.right_click_paste,
                link.is_some(),
            ) {
                crate::terminal_menu::RightClick::Paste => self.request_paste(window, cx),
                crate::terminal_menu::RightClick::Menu => {
                    self.open_context_menu(event.position, link, window, cx)
                }
                crate::terminal_menu::RightClick::Remote => {}
            }
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }

        // 双击链接：交给系统默认浏览器；只放行 http/https。
        if event.click_count >= 2
            && let Some(link) = self.link_at(event.position)
        {
            self.open_link(&link);
            return;
        }
        // 三击选行、双击选词（链接优先）；单击只记锚点，拖动离开这一格才成为选区。
        if let Some((_, row, col)) = self.cell_at(event.position) {
            let Some(line) = self.pane().and_then(|pane| pane.snapshot.rows.get(row)) else {
                return;
            };
            match crate::selection::press_action(line, col, event.click_count) {
                crate::selection::PressAction::Select((start, end)) => {
                    self.selection = Some(((row, start), (row, end)));
                    self.selecting = false;
                    self.drag_anchor = None;
                    self.copy_on_select(cx);
                }
                crate::selection::PressAction::Anchor => {
                    self.selection = None;
                    self.drag_anchor = Some((row, col));
                    self.selecting = true;
                }
            }
            cx.notify();
        }
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dragging_split.is_some() {
            if event.pressed_button != Some(MouseButton::Left) {
                self.dragging_split = None;
                cx.notify();
                return;
            }
            self.update_split(event.position);
            cx.notify();
            return;
        }
        if self.selecting {
            // 丢失 mouse up 时不要继续扩选。
            if event.pressed_button != Some(MouseButton::Left) {
                self.selecting = false;
                return;
            }
            // 选区只在聚焦窗格里：指针拖到别的窗格或窗格外时，按聚焦窗格的边缘钳住。
            if let Some(anchor) = self.drag_anchor {
                let (row, col) = self.mouse_cell_at(event.position);
                self.selection = crate::selection::drag_selection(anchor, (row, col));
                self.copy_on_select(cx);
                cx.notify();
            }
            return;
        }

        // 1002 拖动、1003 悬停：只在跨格时发，避免每个像素一条事件。
        let mode = self.mouse_report_mode(event.modifiers.shift);
        if mode.reports_motion() {
            let cell = self.mouse_cell_at(event.position);
            let button = match event.pressed_button {
                Some(button) => terminal_button(button),
                None if mode.motion => Some(TerminalMouseButton::None),
                None => None,
            };
            if let Some(button) = button {
                if self.last_mouse_cell != Some(cell) {
                    self.last_mouse_cell = Some(cell);
                    self.report_mouse(
                        MouseAction::Motion,
                        button,
                        event.position,
                        event.modifiers.shift,
                        event.modifiers.alt,
                        event.modifiers.control,
                        mode.sgr,
                    );
                    cx.notify();
                }
                return;
            }
        }

        let hover = self.link_at(event.position);
        if self.hover_link != hover {
            self.hover_link = hover;
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.mouse_pressed {
            if let Some(button) = terminal_button(event.button) {
                let mode = self.mouse_report_mode(false);
                self.report_mouse(
                    MouseAction::Release,
                    button,
                    event.position,
                    event.modifiers.shift,
                    event.modifiers.alt,
                    event.modifiers.control,
                    mode.sgr,
                );
                cx.notify();
            }
            self.mouse_pressed = false;
            self.last_mouse_cell = None;
            return;
        }
        self.selecting = false;
        if self.dragging_split.take().is_some() {
            // 松手：下一帧窗格按最终尺寸重排并上报（拖的过程中没有重排）。
            cx.notify();
        }
    }

    fn on_wheel(&mut self, event: &ScrollEvent, _window: &mut Window, cx: &mut Context<Self>) {
        // 滚动：候选列表不占滚轮，关掉浮层，滚动照常处理一次。
        self.suggest_dismiss();
        let row_height = self.cell_height;
        let lines = match event.delta {
            ScrollDelta::Lines(point) => point.y,
            ScrollDelta::Pixels(point) => f32::from(point.y) / row_height.max(1.0),
        };
        let delta = take_whole_lines(&mut self.wheel_remainder, lines);

        let mode = self.mouse_report_mode(event.modifiers.shift);
        if mode.reports_buttons() && delta != 0 {
            let action = if delta > 0 {
                MouseAction::WheelUp
            } else {
                MouseAction::WheelDown
            };
            // 一次滚轮事件可能跨多行；最多补 5 条，防止触控板刷屏。
            for _ in 0..delta.unsigned_abs().min(5) {
                self.report_mouse(
                    action,
                    TerminalMouseButton::None,
                    event.position,
                    event.modifiers.shift,
                    event.modifiers.alt,
                    event.modifiers.control,
                    mode.sgr,
                );
            }
            cx.notify();
            return;
        }

        // 滚的是指针下面的窗格（分屏时不一定是聚焦的那个）；不在任何窗格里就滚聚焦的。
        if delta != 0 {
            let under = self.pane_at(event.position);
            if let Some(tab) = self.tabs.active_mut() {
                let index = under.unwrap_or(tab.active_pane).min(tab.panes.len() - 1);
                let pane = &mut tab.panes[index];
                pane.pump_mut().scroll(delta);
                pane.refresh();
                cx.notify();
            }
        }
    }

    // ---- 复制、粘贴、日志 ----

    /// 当前选区的文字；没有选区返回空串。
    fn selected_text(&self) -> String {
        let Some((anchor, head)) = self.selection else {
            return String::new();
        };
        self.pane()
            .map(|pane| pane.pump().selection_text(anchor, head))
            .unwrap_or_default()
    }

    pub(crate) fn copy_selection(&mut self, cx: &mut Context<Self>) {
        let text = self.selected_text();
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// 用系统默认浏览器打开链接（先过 http/https 校验）。失败只记日志，和双击链接一贯的做法一样。
    fn open_link(&self, link: &str) {
        let opened = crate::terminal_menu::open_link(link, |url| {
            zssh_core::external_url::open_external_url(url)
        });
        if let Err(err) = opened {
            tracing::warn!(%err, "打开链接失败");
        }
    }

    /// 在 `position`（窗口坐标）弹出终端右键菜单。`link` 是鼠标下的链接。点在已有选区之外也不清选区，
    /// 这样「先选中再右键复制」能用。
    fn open_context_menu(
        &mut self,
        position: ::gpui::Point<Pixels>,
        link: Option<Arc<str>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = crate::terminal_menu::MenuState {
            link: link.as_deref().map(str::to_owned),
            has_selection: !self.selected_text().is_empty(),
            clipboard_has_text: cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .is_some_and(|text| !text.is_empty()),
            split: self.tabs.active().is_some_and(Tab::is_split),
        };
        // 菜单上的动作都作用在右键点中的这个窗格（按下时已经聚焦了它）。
        let pane = self.tabs.active().map_or(0, |tab| tab.active().id);
        let entries = crate::terminal_menu::menu_entries(&state);
        let tokens = crate::app_theme::Tokens::current(cx);
        let entity = cx.entity();
        let dispatch: crate::terminal_menu::Dispatch = {
            let link = link.clone();
            Rc::new(move |item, window, cx| {
                entity.update(cx, |this, cx| {
                    this.run_menu_item(item, pane, link.as_deref(), window, cx)
                });
            })
        };
        let shown = entries.clone();
        // `action_context` 让菜单关闭后把焦点还给终端（点了某项、点外面、Esc 都一样）。
        let focus = self.focus.clone();
        let menu = PopupMenu::build(window, cx, move |menu, _window, _cx| {
            crate::terminal_menu::build_menu(menu.action_context(focus), &shown, &tokens, dispatch)
        });
        let dismiss = cx.subscribe_in(
            &menu,
            window,
            |this, menu, _event: &DismissEvent, _window, cx| {
                // 在别处再右键一次时，旧菜单的「点外面关闭」可能晚于新菜单的创建才送到：只清自己那一个。
                if this
                    .context_menu
                    .as_ref()
                    .is_some_and(|open| open.menu.entity_id() == menu.entity_id())
                {
                    this.context_menu = None;
                }
                cx.notify();
            },
        );
        // 焦点要等这次按下分发完再给菜单：终端根元素跟踪了焦点，按下冒泡到它时会把焦点抢回去，
        // 这里同步抢的话菜单拿不到 Esc / 方向键（组件库自己的右键菜单也是下一拍才聚焦）。
        let menu_focus = menu.clone();
        window.defer(cx, move |window, cx| {
            menu_focus.focus_handle(cx).focus(window, cx);
        });
        self.context_menu = Some(ContextMenu {
            menu,
            pane,
            position,
            entries,
            link,
            _dismiss: dismiss,
        });
        cx.notify();
    }

    /// 菜单里选了一项（和对应的快捷键走同一批方法）。
    fn run_menu_item(
        &mut self,
        item: crate::terminal_menu::MenuItem,
        pane: PaneId,
        link: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::terminal_menu::MenuItem;
        match item {
            MenuItem::OpenLink => {
                if let Some(link) = link {
                    self.open_link(link);
                }
            }
            MenuItem::CopyLink => {
                if let Some(link) = link {
                    cx.write_to_clipboard(ClipboardItem::new_string(link.to_owned()));
                }
            }
            MenuItem::Copy => {
                // 和 Ctrl+Shift+C 一样：复制后清掉选区。
                self.copy_selection(cx);
                self.selection = None;
            }
            MenuItem::Paste => self.request_paste(window, cx),
            MenuItem::SelectAll => self.select_all(cx),
            MenuItem::SplitRight => {
                self.split_active(Axis::X, cx);
            }
            MenuItem::SplitDown => {
                self.split_active(Axis::Y, cx);
            }
            MenuItem::Search => {
                // 焦点要留给搜索框：不走下面还给终端的那一步。
                self.open_search(pane, window, cx);
                return;
            }
            MenuItem::Clear => self.clear_pane_screen(pane, cx),
            MenuItem::ClosePane => {
                self.close_pane_by_id(pane, cx);
            }
            MenuItem::Separator => {}
        }
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// 全选：聚焦窗格当前屏幕上的所有格子（不含回滚缓冲）。「选中即复制」开着时同样会写剪贴板。
    fn select_all(&mut self, cx: &mut Context<Self>) {
        let Some(size) = self.pane().map(|pane| pane.snapshot.size) else {
            return;
        };
        self.selection = crate::terminal_menu::select_all_range(size.rows, size.cols);
        self.selecting = false;
        self.drag_anchor = None;
        self.copy_on_select(cx);
    }

    /// 选中即复制：开关打开时，选区一变化就写剪贴板。
    fn copy_on_select(&mut self, cx: &mut Context<Self>) {
        if !self.settings.copy_on_select {
            return;
        }
        self.copy_selection(cx);
    }

    /// 「记录」：控制当前窗格要不要写会话日志（写到设置里的会话日志目录）。设置里已经对所有
    /// 会话自动记录时，显示「正在记录（自动）」并打开日志文件夹，不再另开一份。
    #[cfg(debug_assertions)]
    pub(crate) fn toggle_log(&mut self, cx: &mut Context<Self>) {
        let prefs = crate::app_prefs::cached();
        let Some(pane) = self.pane_mut() else {
            return;
        };
        let outcome = pane.toggle_log_with(&prefs);
        if let LogOutcome::Auto { dir } = &outcome {
            // 还没写过任何会话时目录可能不存在：先建出来，资源管理器才打得开。
            let _ = std::fs::create_dir_all(dir);
            cx.reveal_path(dir);
        }
        self.status.announce(outcome.level(), outcome.status_text());
        cx.notify();
    }

    fn request_paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let Some(text) = item.text() else {
            return;
        };
        self.begin_paste(text, window, cx);
    }

    /// 粘贴一段文字（剪贴板读出来之后的那一步，调试钩子也走这里）：多行先弹确认框，目标在这一刻
    /// 就定下来；单行直接发。
    pub(crate) fn begin_paste(
        &mut self,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if text.is_empty() {
            return;
        }
        // 多行粘贴先确认：远端没开 bracketed paste 时，多行会被 shell 直接执行。
        if text.contains(['\r', '\n']) && self.settings.confirm_multiline {
            self.pending_paste = self.tabs.active().and_then(|tab| tab.capture_paste(text));
            if self.pending_paste.is_some() {
                // 键盘留在终端上：Enter / Esc 由终端的按键处理接走，不会漏到远端。
                window.focus(&self.focus, cx);
            } else {
                // 粘贴的内容没有去处，用户得知道没粘上。
                self.status
                    .announce(StatusLevel::Error, crate::i18n::t(TermMsg::PasteNoPane));
            }
            cx.notify();
        } else {
            // 直接粘贴时清掉之前的确认框，避免旧内容还挂在那里。
            self.pending_paste = None;
            self.send_paste(&text, cx);
        }
    }

    /// 输入的去向：开了广播就是当前标签的所有分屏窗格，否则只有聚焦的窗格。
    /// 各窗格各自回底并刷新快照。
    fn send_bytes(&mut self, bytes: &[u8], origin: WriteOrigin) {
        // 键盘输入发出去之后选区就过时了，留着会让下一次 Ctrl+C 走复制。
        self.selection = None;
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };
        for index in tab.input_targets() {
            let pane = &mut tab.panes[index];
            if pane.pump().display_offset() != 0 {
                pane.pump_mut().scroll_to_bottom();
            }
            let _ = pane.write_as(bytes, origin);
        }
    }

    /// 从文件面板的远端栏拖来的文件落在第 `index` 个窗格上：把远端路径插到输入行（逐个单引号引用，不执行、
    /// 不加回车，走和粘贴同一条编码，所以开了 bracketed paste 的 shell 里也是crate::i18n::t(TermMsg::Paste)而不是逐键输入）。
    /// 窗格没连上、路径里有终端控制字符都不插。
    fn drop_sftp_paths(
        &mut self,
        index: usize,
        drag: &SftpDrag,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(paths) = drag.remote_paths(cx) else {
            return;
        };
        let text = match terminal_insert_text(&paths) {
            Ok(text) => text,
            Err(reason) => {
                // 0.2.14 弹的是错误提示：这里是窗口右上角的通知。
                window.push_notification(reason.to_owned(), cx);
                return;
            }
        };
        let mode = self.settings.bracketed_paste;
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };
        let Some(pane) = tab.panes.get_mut(index) else {
            return;
        };
        if pane.state != PaneState::Live {
            return;
        }
        if pane.pump().display_offset() != 0 {
            pane.pump_mut().scroll_to_bottom();
        }
        let wrapped = mode.wraps(pane.pump().bracketed_paste());
        let _ = pane.write_as(
            &zssh_session::encode_paste(&text, wrapped),
            WriteOrigin::Foreign(WriteSource::Paste),
        );
        tab.active_pane = index;
        self.selection = None;
        cx.notify();
    }

    /// 不需要确认的粘贴：发给此刻的输入目标（聚焦窗格，或广播时的全部窗格）。
    fn send_paste(&mut self, text: &str, cx: &mut Context<Self>) {
        let (Some(index), Some(tab)) = (self.tabs.active_index(), self.tabs.active()) else {
            return;
        };
        let targets = tab.input_targets();
        self.send_paste_to(index, &targets, text, cx);
    }

    /// 把粘贴发给第 `tab_index` 个标签的指定窗格（各自按自己的括号粘贴状态编码）。
    fn send_paste_to(
        &mut self,
        tab_index: usize,
        targets: &[usize],
        text: &str,
        cx: &mut Context<Self>,
    ) {
        self.selection = None;
        let mode = self.settings.bracketed_paste;
        if let Some(tab) = self.tabs.get_mut(tab_index) {
            for &index in targets {
                let Some(pane) = tab.panes.get_mut(index) else {
                    continue;
                };
                let wrapped = mode.wraps(pane.pump().bracketed_paste());
                let _ = pane.write_as(
                    &zssh_session::encode_paste(text, wrapped),
                    WriteOrigin::Foreign(WriteSource::Paste),
                );
            }
        }
        cx.notify();
    }

    /// 确认框点了「粘贴」（或回车）：核对目标还是发起时的那些，是才发，否则取消并提示，绝不改发别处。
    fn confirm_paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_paste.take() else {
            return;
        };
        window.focus(&self.focus, cx);
        let resolved = self
            .tabs
            .items()
            .iter()
            .position(|tab| tab.id == pending.tab_id)
            .and_then(|index| {
                self.tabs
                    .get(index)
                    .and_then(|tab| tab.resolve_paste(&pending))
                    .map(|targets| (index, targets))
            });
        match resolved {
            Some((index, targets)) => self.send_paste_to(index, &targets, &pending.text, cx),
            None => {
                // 这里已经就地弹了通知，状态只记文字，不再让外壳重复弹。
                self.status.set(StatusLevel::Info, paste_cancelled());
                window.push_notification(paste_cancelled().to_owned(), cx);
            }
        }
        cx.notify();
    }

    fn cancel_paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_paste = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// 调试钩子：确认框现在的样子（行数、接收窗格数、是不是广播）；没有确认框返回 `None`。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_paste_state(&self) -> Option<(usize, usize, bool)> {
        self.pending_paste.as_ref().map(|pending| {
            (
                pending.text.lines().count().max(1),
                pending.recipients(),
                pending.broadcast,
            )
        })
    }

    #[cfg(debug_assertions)]
    pub(crate) fn dev_paste_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_paste(window, cx);
    }

    #[cfg(debug_assertions)]
    pub(crate) fn dev_paste_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_paste(window, cx);
    }

    /// 调试钩子用：往活动窗格的会话里"打字"（和键盘输入同一条路径）。
    #[cfg(debug_assertions)]
    pub(crate) fn type_text(&mut self, text: &str, cx: &mut Context<Self>) {
        // 和键盘输入同一条路径，字节原样：连续的普通文字按字符记，回车按 Enter 记，其余控制字符按交给 shell 的键记。
        use zssh_core::terminal_assist::{HandoffKey, KeyClass};
        let mut run = String::new();
        for ch in text.chars() {
            if !ch.is_control() {
                run.push(ch);
                continue;
            }
            if !run.is_empty() {
                self.send_bytes(run.as_bytes(), WriteOrigin::TypedText);
                run.clear();
            }
            let class = match ch {
                '\r' | '\n' => KeyClass::Enter,
                _ => KeyClass::Handoff(HandoffKey::Control),
            };
            let mut buf = [0u8; 4];
            self.send_bytes(ch.encode_utf8(&mut buf).as_bytes(), WriteOrigin::Key(class));
        }
        if !run.is_empty() {
            self.send_bytes(run.as_bytes(), WriteOrigin::TypedText);
        }
        cx.notify();
    }

    // ---- 离线演示用的小工具（调试窗口） ----
}

/// 光标闪烁的相位：每次按键 / 改设置后从「亮」重新计时，之后每半个周期翻一次
/// （530 ms，xterm 的节奏）。相位由时间算出来，不需要定时器翻转状态；窗口的 60 ms 节拍
/// 只负责在相位变了的时候通知重画。
pub(crate) struct BlinkClock {
    since: std::time::Instant,
    /// 上一次通知重画时的相位。
    drawn: bool,
}

const BLINK_HALF_PERIOD: Duration = Duration::from_millis(530);

impl BlinkClock {
    pub(crate) fn new() -> Self {
        Self {
            since: std::time::Instant::now(),
            drawn: true,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.since = std::time::Instant::now();
    }

    fn phase_at(elapsed: Duration) -> bool {
        (elapsed.as_millis() / BLINK_HALF_PERIOD.as_millis()).is_multiple_of(2)
    }

    /// 现在光标是亮的吗。
    pub(crate) fn visible(&self) -> bool {
        Self::phase_at(self.since.elapsed())
    }

    /// 相位和上次画的不一样了（该重画）；顺手记下。
    fn changed(&mut self) -> bool {
        let now = self.visible();
        let changed = now != self.drawn;
        self.drawn = now;
        changed
    }
}

/// 终端一行的高度（逻辑像素），取整到整数设备像素。
///
/// gpui 在布局之前就把显式的 `h(..)` 按设备像素取整（`round_half_toward_zero`）：12px 字体 × 行高 1.3
/// = 15.6px 实际每行占 16px。如果行数仍按 15.6 算（区域高 ÷ 15.6），50 行就多出 20px，最后一行
/// 伸出终端区域被底部栏盖住，而且已经在最底部、滚不下去。绘制、行数、鼠标命中、输入法位置都用这个
/// 取整后的值，三处就永远一致。`scale` 是窗口缩放比例（100% = 1.0）。
pub(crate) fn snap_row_height(row_height: f32, scale: f32) -> f32 {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let device = (row_height * scale).round().max(1.0);
    device / scale
}

/// 把这次滚动量（行，可以是小数）加进累积余数，取出其中的整行数，余数留着给下一次。
///
/// 触控板 / 高精度滚轮每个事件只有零点几行：逐个 `round()` 全是 0，怎么滚都不动。累积后总量不丢；
/// 方向反过来时先清掉旧余数（不然反向的第一下要先抵消前面攒的，手感发滞）。余数放在窗口级而不是
/// 每个窗格一份：它最多不到一行，滚动目标窗格是每次事件按指针位置现找的，串到相邻窗格也看不出来。
fn take_whole_lines(remainder: &mut f32, lines: f32) -> i32 {
    if !lines.is_finite() {
        return 0;
    }
    if remainder.signum() * lines.signum() < 0.0 {
        *remainder = 0.0;
    }
    let total = *remainder + lines;
    let whole = total.trunc();
    *remainder = total - whole;
    whole as i32
}

impl Render for TerminalWorkspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = self.mono_family(cx);
        let mono_size = ::gpui::px(self.settings.font_size);
        // 终端自己的色板：配色可以固定成和界面皮肤不同的一套（默认界面 Dracula、终端 Tokyo Night），
        // 所以底色、前景、选区、光标都取终端色板，不取界面主题。
        let term = crate::app_theme::terminal_palette(cx);
        let foreground: Hsla = ::gpui::rgb(term.foreground).into();
        let background: Hsla = ::gpui::rgb(term.background).into();
        let selection_color: Hsla = ::gpui::rgb(term.selection).into();
        let cursor_color: Hsla = ::gpui::rgb(term.cursor).into();
        let ansi = term.ansi;
        let entity = cx.entity();

        // 按区域大小和等宽字符宽度算行列；尺寸变化时重排终端。
        let font_id = window
            .text_system()
            .resolve_font(&::gpui::font(mono.clone()));
        let cell_width = window
            .text_system()
            .advance(font_id, mono_size, 'M')
            .map(|size| f32::from(size.width))
            .unwrap_or(f32::from(mono_size) * 0.6);
        // 行高取整到整数设备像素：gpui 在布局前就把 `h(..)` 取整，行高是小数时每行实际占的高度和这里按
        // 小数算出来的行数对不上，行数越多累计越多，最后一行会伸到区域外被底部栏盖住。
        let cell_height = snap_row_height(
            f32::from(mono_size) * self.settings.line_height,
            window.scale_factor(),
        );
        self.cell_width = cell_width;
        self.cell_height = cell_height;

        // 区域用上一帧量到的；第一帧还没有，先按整个窗口估，下一帧就会纠正。
        let measured = self.area.get();
        let (area_x, area_y, area_w, area_h) = match measured {
            Some(bounds) => (
                f32::from(bounds.origin.x),
                f32::from(bounds.origin.y),
                f32::from(bounds.size.width),
                f32::from(bounds.size.height),
            ),
            None => {
                let viewport = window.viewport_size();
                (
                    0.0,
                    0.0,
                    f32::from(viewport.width),
                    f32::from(viewport.height),
                )
            }
        };
        self.layout_area = measured;
        self.area_origin = (area_x, area_y);

        let tokens = crate::app_theme::Tokens::current(cx);
        let Some(active) = self.tabs.active() else {
            return v_flex()
                .size_full()
                .track_focus(&self.focus)
                .bg(background)
                .into_any_element();
        };

        // 新开的标签用整块区域的尺寸（扣掉四周内边距）。
        let usable_w = (area_w - PANE_PAD * 2.0).max(0.0);
        let usable_h = (area_h - PANE_PAD * 2.0).max(0.0);
        self.default_size = Size {
            rows: ((usable_h / cell_height).floor() as usize).max(2),
            cols: ((usable_w / cell_width).floor() as usize).max(20),
        };

        // 分屏树 -> 每个窗格的矩形和分隔条；窗格表和树同序，下标一一对应。
        let layout = split_tree::compute_layout(&active.tree, area_w, area_h);
        let mut geoms: Vec<PaneGeom> = Vec::with_capacity(active.panes.len());
        for pane in &active.panes {
            let rect = layout.rect_of(pane.id).unwrap_or(split_tree::Rect {
                x: 0.0,
                y: 0.0,
                w: area_w,
                h: area_h,
            });
            let inner_w = (rect.w - PANE_PAD * 2.0).max(0.0);
            let inner_h = (rect.h - PANE_PAD * 2.0).max(0.0);
            geoms.push(PaneGeom {
                rect: split_tree::Rect {
                    x: area_x + rect.x,
                    y: area_y + rect.y,
                    ..rect
                },
                left: area_x + rect.x + PANE_PAD,
                top: area_y + rect.y + PANE_PAD,
                cols: ((inner_w / cell_width).floor() as usize).max(20),
                rows: ((inner_h / cell_height).floor() as usize).max(2),
            });
        }

        // 活动标签的每个窗格按自己的矩形重排；隐藏标签保持原尺寸，切回时再算。
        // 拖分隔条的过程中不重排（0.2.14 UI-1：只在松手后重排并上报，免得每帧给远端发窗口变化）。
        if self.dragging_split.is_none()
            && let Some(tab) = self.tabs.active_mut()
        {
            for (pane, geom) in tab.panes.iter_mut().zip(geoms.iter()) {
                let size = Size {
                    rows: geom.rows,
                    cols: geom.cols,
                };
                if pane.snapshot.size != size {
                    pane.resize(size);
                }
            }
        }
        self.pane_geom = geoms;
        self.layout = layout.clone();
        self.sync_suggest_room(&layout);

        if self.tabs.active().is_none() {
            return v_flex()
                .size_full()
                .track_focus(&self.focus)
                .bg(background)
                .into_any_element();
        }
        self.request_highlight_scan();
        // 光标挪了格子：让平台把输入法候选框挪到新位置（预编辑过程中的位置由输入法事件自己更新）。
        if self.ime_input && self.focus.is_focused(window) {
            let cell = self.ime_cursor_origin();
            if cell != self.ime_cell {
                self.ime_cell = cell;
                window.invalidate_character_coordinates();
            }
        }

        let Some(active) = self.tabs.active() else {
            return v_flex()
                .size_full()
                .track_focus(&self.focus)
                .bg(background)
                .into_any_element();
        };
        let active_pane = active.active_pane;
        let show_latency = self.settings.show_latency;
        let ui_skin = crate::app_theme::active_skin(cx);
        let split = active.is_split();
        let selection = self.selection;
        let broadcast = active.broadcast;
        let dragging = self.dragging_split;

        // 多行粘贴的确认框：窗口正中的模态框（0.2.14 的样子），盖满整个窗口，不只是终端区域。
        let paste_dialog = self.pending_paste.as_ref().map(|pending| {
            let viewport = window.viewport_size();
            let dialog = paste_dialog(pending, &tokens, &mono, &entity);
            deferred(
                anchored()
                    .position(::gpui::point(::gpui::px(0.0), ::gpui::px(0.0)))
                    .anchor(Anchor::TopLeft)
                    .child(
                        div()
                            .relative()
                            .w(viewport.width)
                            .h(viewport.height)
                            .child(dialog),
                    ),
            )
            .with_priority(gpui_kit::base::POPUP_PRIORITY)
        });

        let area_cell = Rc::clone(&self.area);
        let ime_input = self.ime_input;
        let ime_focus = self.focus.clone();
        let ime_entity = entity.clone();
        // 搜索条画在被搜的窗格里（不在当前标签就不画，`tick_search` 随后会把它收起来）。
        let search_pane = self
            .search
            .as_ref()
            .map(|state| state.pane_id)
            .filter(|id| active.panes.iter().any(|pane| pane.id == *id));
        let mut search_bar = search_pane.and_then(|_| self.search_bar(&tokens, cx));
        let mut children: Vec<AnyElement> = Vec::new();
        for (index, pane) in active.panes.iter().enumerate() {
            let rect = layout.rect_of(pane.id).unwrap_or(split_tree::Rect {
                x: 0.0,
                y: 0.0,
                w: area_w,
                h: area_h,
            });
            let focused = index == active_pane;
            let pane_selection = if focused { selection } else { None };
            let rule_spans: &[Vec<Span>] = match (&self.highlight.current_view, focused) {
                (Some(view), true) => self.highlight.scheduler.spans(view),
                _ => &[],
            };
            // 搜索命中的底色叠在高亮规则之上（后面的盖前面的）。
            let merged_spans: Vec<Vec<Span>>;
            let pane_highlights: &[Vec<Span>] = match self
                .search
                .as_ref()
                .filter(|state| state.pane_id == pane.id)
            {
                Some(state) => {
                    let found = state.visible_spans(&pane.snapshot);
                    merged_spans = found
                        .into_iter()
                        .enumerate()
                        .map(|(row, mut spans)| {
                            let mut all = rule_spans.get(row).cloned().unwrap_or_default();
                            all.append(&mut spans);
                            all
                        })
                        .collect();
                    &merged_spans
                }
                None => rule_spans,
            };
            let bar = if search_pane == Some(pane.id) {
                search_bar.take()
            } else {
                None
            };
            let accepts_drop = pane.state == PaneState::Live;
            let drop_border = tokens.accent;
            children.push(
                div()
                    .id(("pane", index))
                    .absolute()
                    .left(::gpui::px(rect.x))
                    .top(::gpui::px(rect.y))
                    .w(::gpui::px(rect.w))
                    .h(::gpui::px(rect.h))
                    .overflow_hidden()
                    .bg(background)
                    // 远端栏拖来的文件：只有连着的窗格接，本地栏的条目不接（同 0.2.14）。
                    .drag_over::<SftpDrag>(move |style, drag, _, _| {
                        if accepts_drop && drag.is_remote() {
                            style.border_1().border_color(drop_border)
                        } else {
                            style
                        }
                    })
                    .on_drop::<SftpDrag>(cx.listener(move |this, drag: &SftpDrag, window, cx| {
                        this.drop_sftp_paths(index, drag, window, cx);
                    }))
                    .child(
                        v_flex()
                            .size_full()
                            .p(::gpui::px(PANE_PAD))
                            .child(pane_grid(
                                pane,
                                pane_selection,
                                pane_highlights,
                                focused,
                                &mono,
                                self.settings.font_size,
                                self.cell_width,
                                self.cell_height,
                                self.settings.cursor,
                                !self.settings.cursor_blink || self.cursor_blink.visible(),
                                foreground,
                                background,
                                selection_color,
                                cursor_color,
                                &ansi,
                            )),
                    )
                    // 输入法的预编辑文字画在光标处（只有聚焦窗格、停在最新一屏时）。
                    .children(
                        (focused
                            && self.ime_input
                            && !self.ime_preedit.is_empty()
                            && pane.snapshot.display_offset == 0)
                            .then(|| {
                                ime_ops::preedit_overlay(
                                    self.ime_preedit.text(),
                                    (
                                        PANE_PAD
                                            + pane.snapshot.cursor_col as f32 * self.cell_width,
                                        PANE_PAD
                                            + pane.snapshot.cursor_row as f32 * self.cell_height,
                                    ),
                                    self.cell_height,
                                    &mono,
                                    self.settings.font_size,
                                    (foreground, background),
                                )
                            }),
                    )
                    // 断线遮罩（V2-U30）：盖暗整个窗格，正中写明已断开 + 原因 + 怎么重连。点击 / 按 R 的处理
                    // 在窗口级的鼠标 / 键盘入口（`reconnect_overlay_pane`），所以这里只画不接事件：
                    // 滚轮能照常滚动下面的回滚。
                    .children(pane.overlay_reason().map(|reason| {
                        disconnect_overlay(index, reason, &tokens, foreground, background)
                    }))
                    // 分屏时聚焦窗格顶上一道细线，告诉用户键盘输入去哪（单个窗格不画；
                    // 广播时每个窗格都收输入，线就没有意义了）。同 0.2.14 的 `pane-focus-line`。
                    .when(split && focused && !broadcast, |d| {
                        d.child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .right_0()
                                .h(::gpui::px(2.0))
                                .bg(tokens.accent.alpha(0.7)),
                        )
                    })
                    // 右上角延迟徽标：设置开着、窗格连着、已经测到第一个值才画。
                    .when(show_latency && pane.state == PaneState::Live, |d| {
                        d.children(latency_badge(
                            index,
                            pane,
                            &mono,
                            ui_skin,
                            term.background,
                            self.settings.font_size,
                        ))
                    })
                    // 搜索条最后画，盖在延迟徽标上面。
                    .children(bar)
                    // 命令建议浮层：锚定光标，只画在聚焦窗格里。
                    .children(
                        focused
                            .then(|| {
                                self.suggest_overlay(
                                    pane,
                                    index,
                                    (rect.w, rect.h),
                                    &mono,
                                    &tokens,
                                    cx,
                                )
                            })
                            .flatten(),
                    )
                    .into_any_element(),
            );
        }
        // 分隔条：可见线 4px，命中区每边多 2px（透明，叠在窗格边缘上）；拖的时候线变成强调色。
        let grow = (split_tree::DIVIDER_HIT - split_tree::DIVIDER_SIZE) / 2.0;
        for (index, divider) in layout.dividers.iter().enumerate() {
            let line = divider.rect;
            let horizontal = divider.axis == Axis::X;
            let (x, y, w, h) = if horizontal {
                (line.x - grow, line.y, line.w + grow * 2.0, line.h)
            } else {
                (line.x, line.y - grow, line.w, line.h + grow * 2.0)
            };
            let color = if dragging == Some(divider.split_id) {
                tokens.accent
            } else {
                cx.theme().border
            };
            children.push(
                div()
                    .id(("split-divider", index))
                    .absolute()
                    .left(::gpui::px(x))
                    .top(::gpui::px(y))
                    .w(::gpui::px(w))
                    .h(::gpui::px(h))
                    .when(horizontal, |d| d.cursor_col_resize())
                    .when(!horizontal, |d| d.cursor_row_resize())
                    .child(
                        div()
                            .absolute()
                            .left(::gpui::px(if horizontal { grow } else { 0.0 }))
                            .top(::gpui::px(if horizontal { 0.0 } else { grow }))
                            .w(::gpui::px(line.w))
                            .h(::gpui::px(line.h))
                            .bg(color),
                    )
                    .into_any_element(),
            );
        }

        // 右键菜单：画在最上层（deferred），位置是弹出时的窗口坐标；菜单上滚轮不要滚到后面的终端。
        let context_menu = self.context_menu.as_ref().map(|open| {
            deferred(
                anchored()
                    .position(open.position)
                    .anchor(Anchor::TopLeft)
                    .snap_to_window_with_margin(::gpui::px(8.0))
                    .child(
                        div()
                            .on_scroll(|_, _, cx| cx.stop_propagation())
                            .child(open.menu.clone()),
                    ),
            )
            .with_priority(gpui_kit::base::POPUP_PRIORITY)
        });

        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_key_up(cx.listener(Self::on_key_up))
            .on_scroll(cx.listener(Self::on_wheel))
            .bg(background)
            .when(self.hover_link.is_some(), |d| d.cursor_pointer())
            .children(context_menu)
            .children(paste_dialog)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(
                        canvas(
                            move |bounds, _, _| area_cell.set(Some(bounds)),
                            // 终端聚焦时把它登记成文字输入目标：输入法的提交 / 预编辑才到得了这里。
                            move |bounds, _, window, cx| {
                                if ime_input {
                                    ime_ops::register(
                                        &ime_focus,
                                        bounds,
                                        ime_entity.clone(),
                                        window,
                                        cx,
                                    );
                                }
                            },
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(
                        div()
                            .id("pane-area")
                            .relative()
                            .size_full()
                            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
                            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
                            .on_mouse_move(cx.listener(Self::on_mouse_move))
                            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
                            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
                            .children(children),
                    )
                    // 广播开着时在窗格右上角常驻提示：同一段输入会进到这个标签的每个窗格。
                    .when(broadcast, |d| {
                        d.child(
                            div()
                                .absolute()
                                .top_1()
                                .right_3()
                                .px_2()
                                .py_0p5()
                                .rounded_full()
                                .bg(tokens.accent)
                                .text_color(tokens.on_accent)
                                .text_xs()
                                .child("\u{5e7f}\u{64ad}\u{4e2d}"),
                        )
                    }),
            )
            .into_any_element()
    }
}

/// Alt / Option 要不要当 Meta（在字符前补 ESC）。
///
/// Windows / Linux：Alt 一律是 Meta，和以前一样。macOS：Option 默认不当 Meta（和系统自带的终端一致）——
/// 德语、北欧、西班牙语等键盘的 `@ [ ] { } | \ ~` 都靠 Option 打出来，系统已经把组合好的字符放进 `key_char`，
/// 要原样发出去；没有字符的键（方向键、退格这类）仍然带上修饰键参数。
fn alt_as_meta(alt: bool, has_char: bool, macos: bool) -> bool {
    if macos { alt && !has_char } else { alt }
}

/// 预览框最多显示的行数 / 字符数：几万行的剪贴板不能整段塞进界面。
const PASTE_PREVIEW_LINES: usize = 30;
const PASTE_PREVIEW_CHARS: usize = 4000;

/// 确认框里的预览文字：前若干行（太长截断），以及是不是被截断了。
fn paste_preview(text: &str) -> (String, bool) {
    let mut out = String::new();
    let mut truncated = false;
    for (index, line) in text.lines().enumerate() {
        if index >= PASTE_PREVIEW_LINES || out.chars().count() >= PASTE_PREVIEW_CHARS {
            truncated = true;
            break;
        }
        if index > 0 {
            out.push('\n');
        }
        // 终端控制字符不放进界面预览（可能夹带转义序列，也会让排版乱）。
        out.extend(
            line.chars()
                .map(|ch| if ch.is_control() { '\u{fffd}' } else { ch }),
        );
    }
    if out.chars().count() > PASTE_PREVIEW_CHARS {
        out = out.chars().take(PASTE_PREVIEW_CHARS).collect();
        truncated = true;
    }
    (out, truncated)
}

/// 确认框上写的"发给谁"：和实际接收者一致（见 [`PendingPaste::recipients`]）。
fn paste_target_line(pending: &PendingPaste) -> String {
    if pending.broadcast {
        format!(
            "{}{}{}",
            crate::i18n::t(TermMsg::PasteTargetBroadcastPrefix),
            pending.recipients(),
            crate::i18n::t(TermMsg::PasteTargetBroadcastSuffix)
        )
    } else {
        crate::i18n::t(TermMsg::PasteTargetCurrent).to_owned()
    }
}

/// 多行粘贴的确认框：标题、说明、目标、等宽预览、取消 / 粘贴。外壳和主窗口的确认框同一套。
fn paste_dialog(
    pending: &PendingPaste,
    tokens: &crate::app_theme::Tokens,
    mono: &::gpui::SharedString,
    entity: &Entity<TerminalWorkspace>,
) -> AnyElement {
    use crate::main_window::widgets::{ButtonLook, modal_card, modal_scrim, text_button};
    use crate::typography::{Role, Typo as _};

    let lines = pending.text.lines().count().max(1);
    let (preview, truncated) = paste_preview(&pending.text);
    let scrim_cancel = entity.clone();
    let cancel = entity.clone();
    let confirm = entity.clone();
    modal_scrim("paste-scrim", false, move |window, cx| {
        scrim_cancel.update(cx, |this, cx| this.cancel_paste(window, cx));
    })
    .child(
        modal_card("paste-card", 420.0, false, tokens)
            .child(
                v_flex()
                    .gap_2()
                    .child(div().typo(Role::GroupTitle, tokens).child(format!(
                        "{}{lines}{}",
                        crate::i18n::t(TermMsg::PasteConfirmPrefix),
                        crate::i18n::t(TermMsg::PasteConfirmSuffix)
                    )))
                    .child(
                        div()
                            .typo(Role::Body, tokens)
                            .child(crate::i18n::t(TermMsg::PasteWarning)),
                    )
                    .child(
                        div()
                            .typo(Role::Caption, tokens)
                            .child(paste_target_line(pending)),
                    )
                    .child(
                        div()
                            .id("paste-preview")
                            .max_h(::gpui::px(160.0))
                            .overflow_y_scroll()
                            .rounded(::gpui::px(crate::app_theme::CARD_RADIUS))
                            .p_2()
                            .bg(tokens.subtle)
                            .font_family(mono.clone())
                            .text_size(::gpui::px(12.0))
                            .text_color(tokens.text)
                            .child(preview),
                    )
                    .when(truncated, |d| {
                        d.child(div().typo(Role::Caption, tokens).child(format!(
                            "{}{lines}{}",
                            crate::i18n::t(TermMsg::PasteMorePrefix),
                            crate::i18n::t(TermMsg::PasteMoreSuffix)
                        )))
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(text_button(
                        "paste-cancel",
                        crate::i18n::t(TermMsg::Cancel),
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            cancel.update(cx, |this, cx| this.cancel_paste(window, cx));
                        },
                    ))
                    .child(text_button(
                        "paste-ok",
                        crate::i18n::t(TermMsg::Paste),
                        ButtonLook::Primary,
                        move |_, window, cx| {
                            confirm.update(cx, |this, cx| this.confirm_paste(window, cx));
                        },
                    )),
            ),
    )
    .into_any_element()
}

/// 断线遮罩（0.2.14 的样子）：底色是终端自己的背景色 88% 不透明（界面和终端一明一暗时仍和终端是一块），
/// 文字用终端前景色，下面的回滚内容隐约可见。正中竖排：「已断开」（危险色、加粗）、原因（淡一档、过长截断）、
/// 重连提示。
fn disconnect_overlay(
    index: usize,
    reason: &str,
    tokens: &crate::app_theme::Tokens,
    foreground: Hsla,
    background: Hsla,
) -> AnyElement {
    use crate::icons::Ico;
    let reason: ::gpui::SharedString = if reason.is_empty() {
        crate::i18n::t(TermMsg::UnknownReason).into()
    } else {
        reason.to_owned().into()
    };
    v_flex()
        .id(("disconnect-overlay", index))
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .items_center()
        .justify_center()
        .gap(::gpui::px(6.0))
        .bg(background.alpha(0.88))
        .text_color(foreground)
        .text_size(::gpui::px(13.0))
        .cursor_pointer()
        .child(
            h_flex()
                .items_center()
                .gap_2()
                .text_color(tokens.danger)
                .font_weight(FontWeight::SEMIBOLD)
                .child(Icon::new(Ico::Unplug).size_4())
                .child(crate::i18n::t(TermMsg::DisconnectedBadge)),
        )
        .child(
            div()
                .max_w(::gpui::relative(0.8))
                .truncate()
                .text_size(::gpui::px(12.0))
                .text_color(foreground.alpha(0.7))
                .child(reason),
        )
        .child(
            h_flex()
                .items_center()
                .gap_1()
                .mt_1()
                .text_size(::gpui::px(12.0))
                .text_color(foreground.alpha(0.8))
                .child(Icon::new(Ico::RotateCcw).size_3())
                .child(crate::i18n::t(TermMsg::ReconnectHint)),
        )
        .into_any_element()
}

/// 窗格右上角的延迟徽标：小圆角、半透明底（悬停变实）、数值按色档着色；悬停提示写明测的是什么。
/// 窗格没有监视器、还没有第一个值、或者已断开时是 `None`。
fn latency_badge(
    index: usize,
    pane: &Pane,
    mono: &::gpui::SharedString,
    skin: &crate::app_theme::Skin,
    terminal_bg: u32,
    font_size: f32,
) -> Option<AnyElement> {
    let (snapshot, host) = pane.latency_snapshot()?;
    let last = snapshot.stats.last()?;
    let badge = crate::app_theme::latency_badge(latency::level(last), skin, terminal_bg);
    let fill: Hsla = ::gpui::rgb(badge.fill).into();
    let fg: Hsla = ::gpui::rgb(badge.fg).into();
    let text = latency::format_sample(last);
    let tip: ::gpui::SharedString = latency::tooltip(host, snapshot.kind, &snapshot.stats).into();
    Some(
        div()
            .id(("latency-badge", index))
            .absolute()
            .top(::gpui::px(6.0))
            .right(::gpui::px(12.0))
            .px(::gpui::px(6.0))
            .py(::gpui::px(1.0))
            .rounded_full()
            .bg(fill.alpha(0.8))
            .hover(move |style| style.bg(fill))
            .text_color(fg)
            .font_family(mono.clone())
            .text_size(::gpui::px((font_size - 2.0).max(10.0)))
            .child(text)
            // 点在徽标上不要开始选区 / 抢走终端焦点以外的东西。
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .tooltip(move |window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
            })
            .into_any_element(),
    )
}

/// 一个窗格的可见行；不同窗格各自独立选区，只有聚焦窗格画光标。
#[allow(clippy::too_many_arguments)]
fn pane_grid(
    pane: &Pane,
    selection: Option<((usize, usize), (usize, usize))>,
    highlights: &[Vec<Span>],
    focused: bool,
    mono: &::gpui::SharedString,
    font_size: f32,
    // 一个栅格列的宽度：选区底纹按「列区间 × 列宽」整块画，不留缝。
    cell_width: f32,
    // 一行的高度（逻辑像素），和布局算行数用的是同一个值（见 `snap_row_height`）。
    row_height: f32,
    cursor_shape: CursorShape,
    cursor_lit: bool,
    foreground: Hsla,
    background: Hsla,
    selection_color: Hsla,
    cursor_color: Hsla,
    ansi: &[u32; 16],
) -> impl IntoElement {
    let mono_size = ::gpui::px(font_size);
    let cursor = if focused && cursor_lit {
        pane.pump().cursor_visible()
    } else {
        None
    };
    v_flex()
        .flex_1()
        .overflow_hidden()
        .children(pane.snapshot.rows.iter().enumerate().map(|(row_ix, row)| {
            let plain_links = crate::links::find_plain_links(row);
            let row_spans = highlights.get(row_ix).map(Vec::as_slice).unwrap_or(&[]);
            let row_h = ::gpui::px(row_height);
            // 选区底纹按「连续的列区间」整块画在文字下面：逐字 div 的背景宽度取字形 advance，
            // 非整数像素时相邻背景之间会露缝；按 列宽 × 列数 画整块矩形就没有缝。
            div()
                .relative()
                .h(row_h)
                .w_full()
                .children(
                    selection_spans(selection, row_ix, row.cells.len())
                        .into_iter()
                        .map(|(start, end)| {
                            div()
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .left(::gpui::px(start as f32 * cell_width))
                                .w(::gpui::px((end - start + 1) as f32 * cell_width))
                                .bg(selection_color)
                        }),
                )
                .child(
                    h_flex().h(row_h).children(
                        runs(
                            row,
                            foreground,
                            background,
                            selection,
                            row_ix,
                            cursor,
                            &plain_links,
                            row_spans,
                            ansi,
                        )
                        .into_iter()
                        .map(|run| {
                            let block = run.cursor && cursor_shape == CursorShape::Block;
                            let underline = run.cursor && cursor_shape == CursorShape::Underline;
                            let bar = run.cursor && cursor_shape == CursorShape::Bar;
                            let bg = run_background(
                                block,
                                run.selected,
                                run.bg,
                                background,
                                cursor_color,
                            );
                            div()
                                .font_family(mono.clone())
                                .text_size(mono_size)
                                .text_color(if block { background } else { run.fg })
                                .when_some(bg, |d, bg| d.bg(bg))
                                .when(run.underline || underline, |d| d.underline())
                                .when(run.strikethrough, |d| d.line_through())
                                .when(run.bold, |d| d.font_weight(FontWeight::BOLD))
                                .when(run.italic, |d| d.italic())
                                .child(run.text)
                                // 竖线光标：压在格子左边的 2 px 细条，不占宽度，后面的字不会被挤开。
                                .when(bar, |d| {
                                    d.relative().child(
                                        div()
                                            .absolute()
                                            .top_0()
                                            .bottom_0()
                                            .left_0()
                                            .w(::gpui::px(2.0))
                                            .bg(cursor_color),
                                    )
                                })
                        }),
                    ),
                )
        }))
}

fn is_selected(
    selection: Option<((usize, usize), (usize, usize))>,
    row: usize,
    col: usize,
) -> bool {
    let Some((anchor, head)) = selection else {
        return false;
    };
    let (start, end) = if anchor <= head {
        (anchor, head)
    } else {
        (head, anchor)
    };
    (row > start.0 || (row == start.0 && col >= start.1))
        && (row < end.0 || (row == end.0 && col <= end.1))
}

/// 把一行按相同属性（含选中状态）切成连续的 run；宽字符占位格跳过。
struct Run {
    text: String,
    fg: Hsla,
    bg: Hsla,
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
    selected: bool,
    cursor: bool,
}

/// 一段文字自己要画的底色。
///
/// 选区底纹画在文字下面一层；被选中的格子如果再画自己的底色（htop / vim / tmux 状态栏、
/// 带背景色的 `ls`），就会把选区盖掉、看不出选中了。所以选中时不画自己的底色，块光标照旧最上。
fn run_background<C: PartialEq + Copy>(
    block_cursor: bool,
    selected: bool,
    run_bg: C,
    background: C,
    cursor: C,
) -> Option<C> {
    if block_cursor {
        Some(cursor)
    } else if selected || run_bg == background {
        None
    } else {
        Some(run_bg)
    }
}

/// 这一行被选中的连续列区间（闭区间）：选区底纹按列区间整块画，避免逐字背景之间露缝。
fn selection_spans(
    selection: Option<((usize, usize), (usize, usize))>,
    row_ix: usize,
    cols: usize,
) -> Vec<(usize, usize)> {
    if selection.is_none() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for col in 0..cols {
        if is_selected(selection, row_ix, col) {
            start.get_or_insert(col);
        } else if let Some(from) = start.take() {
            out.push((from, col - 1));
        }
    }
    if let Some(from) = start {
        out.push((from, cols.saturating_sub(1)));
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn runs(
    row: &zssh_terminal::RowSnapshot,
    foreground: Hsla,
    background: Hsla,
    selection: Option<((usize, usize), (usize, usize))>,
    row_ix: usize,
    cursor: Option<(usize, usize)>,
    plain_links: &[crate::links::LinkSpan],
    spans: &[Span],
    ansi: &[u32; 16],
) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    for (col, cell) in row.cells.iter().enumerate() {
        if cell.ch == '\0' || cell.spacer {
            continue;
        }
        let (mut fg, mut bg) = (
            color_to_hsla(cell.fg, foreground, background, ansi),
            color_to_hsla(cell.bg, foreground, background, ansi),
        );
        // 反显：前景/背景互换。
        if cell.inverse {
            std::mem::swap(&mut fg, &mut bg);
        }
        // 高亮规则叠加在 ANSI 颜色之上；后加的规则优先。
        let mut span_fg = None;
        let mut span_bg = None;
        for span in spans {
            if col >= span.start && col <= span.end {
                span_fg = Some(span.fg);
                span_bg = span.bg;
            }
        }
        if let Some(color) = span_fg {
            fg = ::gpui::rgb(color).into();
        }
        if let Some(color) = span_bg {
            bg = ::gpui::rgb(color).into();
        }
        let selected = is_selected(selection, row_ix, col);
        let is_cursor = cursor == Some((row_ix, col));
        let underline = cell.underline
            || cell.hyperlink.is_some()
            || plain_links
                .iter()
                .any(|link| col >= link.start_col && col <= link.end_col);
        let mut text = if cell.hidden {
            " ".to_owned()
        } else {
            let mut text = String::from(cell.ch);
            if let Some(zerowidth) = &cell.zerowidth {
                text.extend(zerowidth.iter());
            }
            text
        };
        match out.last_mut() {
            Some(run)
                if run.fg == fg
                    && run.bg == bg
                    && run.bold == cell.bold
                    && run.italic == cell.italic
                    && run.underline == underline
                    && run.strikethrough == cell.strikethrough
                    && run.selected == selected
                    && run.cursor == is_cursor =>
            {
                run.text.push_str(&text);
            }
            _ => out.push(Run {
                text: std::mem::take(&mut text),
                fg,
                bg,
                bold: cell.bold,
                italic: cell.italic,
                underline,
                strikethrough: cell.strikethrough,
                selected,
                cursor: is_cursor,
            }),
        }
    }
    // 行尾空白只在没有背景色、没被选中/光标时裁掉。
    while let Some(run) = out.last() {
        if run.text.trim().is_empty() && !run.cursor && !run.selected && run.bg == background {
            out.pop();
        } else {
            break;
        }
    }
    out
}

fn color_to_hsla(
    cell_color: TerminalColor,
    foreground: Hsla,
    background: Hsla,
    ansi: &[u32; 16],
) -> Hsla {
    match cell_color {
        TerminalColor::Named(named) => named_color(named, foreground, background, ansi),
        TerminalColor::Indexed(index) => indexed_color(index, ansi),
        TerminalColor::Spec(rgb) => {
            ::gpui::rgb(((rgb.r as u32) << 16) | ((rgb.g as u32) << 8) | rgb.b as u32).into()
        }
    }
}

/// 16 色按当前主题的终端色板取（`app_theme` 里的 `ansi`）；「暗」色没有单独的色值，和
/// 普通色一样；前景/背景用主题的前景/背景。
fn named_color(color: NamedColor, foreground: Hsla, background: Hsla, ansi: &[u32; 16]) -> Hsla {
    let index = match color {
        NamedColor::Black | NamedColor::DimBlack => 0,
        NamedColor::Red | NamedColor::DimRed => 1,
        NamedColor::Green | NamedColor::DimGreen => 2,
        NamedColor::Yellow | NamedColor::DimYellow => 3,
        NamedColor::Blue | NamedColor::DimBlue => 4,
        NamedColor::Magenta | NamedColor::DimMagenta => 5,
        NamedColor::Cyan | NamedColor::DimCyan => 6,
        NamedColor::White | NamedColor::DimWhite => 7,
        NamedColor::BrightBlack => 8,
        NamedColor::BrightRed => 9,
        NamedColor::BrightGreen => 10,
        NamedColor::BrightYellow => 11,
        NamedColor::BrightBlue => 12,
        NamedColor::BrightMagenta => 13,
        NamedColor::BrightCyan => 14,
        NamedColor::BrightWhite => 15,
        NamedColor::Background => return background,
        NamedColor::Foreground | NamedColor::BrightForeground => return foreground,
        _ => return foreground,
    };
    ::gpui::rgb(ansi[index]).into()
}

fn indexed_color(index: u8, ansi: &[u32; 16]) -> Hsla {
    let rgb = match index {
        0..=15 => ansi[index as usize],
        16..=231 => {
            let i = index - 16;
            let r = (i / 36) % 6;
            let g = (i / 6) % 6;
            let b = i % 6;
            let scale = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            ((scale(r) as u32) << 16) | ((scale(g) as u32) << 8) | scale(b) as u32
        }
        _ => {
            let level = 8 + (index - 232) * 10;
            ((level as u32) << 16) | ((level as u32) << 8) | level as u32
        }
    };
    ::gpui::rgb(rgb).into()
}

#[cfg(test)]
mod alt_meta_tests {
    use super::alt_as_meta;

    #[test]
    fn alt_is_meta_except_for_option_characters_on_macos() {
        // Windows / Linux：和以前一样，Alt 一律补 ESC。
        assert!(alt_as_meta(true, true, false));
        assert!(alt_as_meta(true, false, false));
        // macOS：Option 打出来的字符原样发；没有字符的键（方向键）仍带修饰键。
        assert!(!alt_as_meta(true, true, true));
        assert!(alt_as_meta(true, false, true));
        // 没按 Alt 永远不补。
        assert!(!alt_as_meta(false, true, false));
        assert!(!alt_as_meta(false, true, true));
    }
}

#[cfg(test)]
mod selection_paint_tests {
    use super::{run_background, selection_spans};

    #[test]
    fn selected_cells_do_not_paint_their_own_background_over_the_selection() {
        // 字符 0 是默认底色，1 是 htop 那种彩色底，9 是块光标。
        assert_eq!(run_background(false, false, 0, 0, 9), None);
        assert_eq!(run_background(false, false, 1, 0, 9), Some(1));
        assert_eq!(
            run_background(false, true, 1, 0, 9),
            None,
            "选中时让选区透出来"
        );
        assert_eq!(
            run_background(true, true, 1, 0, 9),
            Some(9),
            "块光标照旧在最上"
        );
    }

    #[test]
    fn selection_spans_cover_contiguous_columns() {
        assert!(selection_spans(None, 0, 10).is_empty());
        // 同一行选第 2..=5 列。
        assert_eq!(selection_spans(Some(((0, 2), (0, 5))), 0, 10), vec![(2, 5)]);
        // 跨行选区的中间行整行都选中。
        assert_eq!(selection_spans(Some(((0, 3), (2, 1))), 1, 8), vec![(0, 7)]);
        // 不在选区里的行没有区间。
        assert!(selection_spans(Some(((0, 3), (0, 4))), 1, 8).is_empty());
    }
}

#[cfg(test)]
mod pane_tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{
        CoreFilter, FlowGate, LogMode, LogOutcome, Pane, PaneState, SessionBackend, SessionCommand,
        SessionPump, Size, StatusLevel, StatusLine, StatusNotice, disconnect_notice,
        failure_notice,
    };
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use zssh_session::{SessionEvent, SessionExit};

    const SIZE: Size = Size { rows: 12, cols: 60 };

    fn screen(pane: &Pane) -> String {
        pane.snapshot
            .rows
            .iter()
            .map(|row| row.text.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 记录收到的命令，并把写入当成"远端没有回显"丢掉。
    struct Recorder(Arc<Mutex<Vec<SessionCommand>>>);

    impl SessionBackend for Recorder {
        fn send(&mut self, command: SessionCommand) {
            self.0.lock().unwrap().push(command);
        }
    }

    fn remote_pane() -> (
        Pane,
        std::sync::mpsc::Sender<SessionEvent>,
        Arc<Mutex<Vec<SessionCommand>>>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let pump = SessionPump::new(SIZE, 100, Arc::new(FlowGate::new()), CoreFilter::new(), 1);
        let pane = Pane::with_parts(
            pump,
            Box::new(Recorder(Arc::clone(&sent))),
            rx,
            Some(7),
            None,
        );
        (pane, tx, sent)
    }

    /// R04：高亮请求靠"这是哪一屏"认结果。窗格、连接代次、屏幕内容（含本地提示文字）、规则版本、
    /// 视口里的任何一项变了，键就必须变；只有内容变时仍算同一个视图（滚动输出时上一屏的结果慢一拍可画）。
    #[test]
    fn the_scan_key_tells_apart_pane_connection_content_rules_and_viewport() {
        let (mut pane, tx, _sent) = remote_pane();
        let first = pane.scan_key(1);

        // 内容：远端输出和界面自己写的提示文字都算。
        tx.send(SessionEvent::Data {
            generation: 1,
            bytes: b"error
"
            .to_vec(),
        })
        .unwrap();
        assert!(pane.poll_events());
        let after_output = pane.scan_key(1);
        assert_ne!(after_output, first);
        assert!(after_output.same_view(&first), "只有内容变，仍是同一个视图");
        pane.pump_mut().feed_local(b"local notice");
        pane.refresh();
        let after_local = pane.scan_key(1);
        assert_ne!(after_local.content, after_output.content);
        assert!(after_local.same_view(&first));

        // 规则版本、视口（大小、滚动）：换视图。
        assert!(!pane.scan_key(2).same_view(&after_local));
        pane.resize(Size { rows: 12, cols: 70 });
        let resized = pane.scan_key(1);
        assert_eq!((resized.rows, resized.cols), (12, 70));
        assert!(!resized.same_view(&after_local));
        for i in 0..40 {
            pane.pump_mut().feed_local(
                format!(
                    "line {i}
"
                )
                .as_bytes(),
            );
        }
        pane.pump_mut().scroll(3);
        pane.refresh();
        let scrolled = pane.scan_key(1);
        assert_eq!(scrolled.offset, 3);
        assert!(!scrolled.same_view(&resized));

        // 另一个窗格（哪怕内容一样）和同一窗格的新连接（重连沿用窗格编号、换代次）：换视图。
        let (other, _tx, _sent) = remote_pane();
        assert!(!other.scan_key(1).same_view(&first));
        let mut reconnected = Pane::new(SIZE, 100, pane.generation + 1000);
        reconnected.id = pane.id;
        assert_eq!(reconnected.id, pane.id);
        assert!(
            !reconnected
                .scan_key(1)
                .same_view(&Pane::new(SIZE, 100, pane.generation).scan_key(1))
        );
    }

    #[test]
    fn connection_failure_shows_the_reason_and_never_echoes_input() {
        let pending = crate::ssh_backend::ready(Err("连接失败：timed out".to_owned()));
        let mut pane = Pane::connecting(SIZE, 100, 1, Some(7), pending);
        assert_eq!(pane.state, PaneState::Connecting);
        assert!(screen(&pane).contains("正在连接"));

        assert!(pane.poll_events());
        assert_eq!(pane.state, PaneState::Disconnected);
        let text = screen(&pane);
        assert!(text.contains("连接失败：timed out"), "{text}");
        assert!(!text.contains("Ctrl+Shift+R"), "{text}");
        assert_eq!(pane.overlay_reason(), Some("连接失败：timed out"));
        // 还能重连：主机 id 没丢。
        assert_eq!(pane.host_id, Some(7));

        // 失败后键入的内容（比如密码）不能回显到屏幕上。
        pane.write(b"hunter2\r");
        assert!(!screen(&pane).contains("hunter2"));
    }

    // ---- 重连保留终端历史（V2-TERM-UX1） ----

    fn send(tx: &std::sync::mpsc::Sender<SessionEvent>, generation: u64, text: &str) {
        tx.send(SessionEvent::Data {
            generation,
            bytes: text.as_bytes().to_vec(),
        })
        .unwrap();
    }

    /// 一条"新连接"：返回开新窗格的闭包要用的东西（事件发送端、写端收到的命令）。
    fn new_connection(
        generation: u64,
    ) -> (
        Pane,
        std::sync::mpsc::Sender<SessionEvent>,
        Arc<Mutex<Vec<SessionCommand>>>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let pending = crate::ssh_backend::ready(Ok(crate::ssh_backend::Opened {
            factory: None,
            backend: Box::new(Recorder(Arc::clone(&sent))),
            events: rx,
            gate: Arc::new(FlowGate::new()),
            latency: None,
        }));
        (
            Pane::connecting(SIZE, 100, generation, Some(7), pending),
            tx,
            sent,
        )
    }

    /// 在整个终端（回滚 + 屏幕）里数一段文字出现了几处。
    fn count_in_terminal(pane: &Pane, text: &str) -> usize {
        pane.pump().search(text, false).len()
    }

    fn position_in_terminal(pane: &Pane, text: &str) -> (i32, usize) {
        pane.pump().search(text, false)[0].start
    }

    #[test]
    fn reconnecting_keeps_the_scrollback_and_marks_where_the_new_session_starts() {
        let (mut pane, tx, old_sent) = remote_pane();
        for i in 0..30 {
            send(&tx, 1, &format!("old line {i}\r\n"));
        }
        tx.send(SessionEvent::Exit {
            generation: 1,
            code: Some(255),
        })
        .unwrap();
        pane.poll_events();
        assert_eq!(pane.state, PaneState::Disconnected);
        assert!(pane.pump().history_size() > 0, "旧输出已经滚进回滚缓冲");
        let id = pane.id;

        let (fresh, new_tx, new_sent) = new_connection(2);
        pane.reconnect_with(|| fresh);

        // 旧通道收到 Close；旧事件通道的接收端已经丢掉，旧会话再发什么都进不来。
        assert!(old_sent.lock().unwrap().contains(&SessionCommand::Close));
        assert!(
            tx.send(SessionEvent::Exit {
                generation: 1,
                code: None
            })
            .is_err()
        );
        // 窗格编号不变，遮罩消失，回滚里的旧输出还在。
        assert_eq!(pane.id, id);
        assert_eq!(pane.state, PaneState::Connecting);
        assert_eq!(pane.overlay_reason(), None);
        assert_eq!(count_in_terminal(&pane, "old line 0"), 1);
        assert_eq!(count_in_terminal(&pane, "old line 29"), 1);
        assert!(screen(&pane).contains("正在连接"), "{}", screen(&pane));
        // 连上之前还没有"已重新连接"：失败的尝试不留这一行。
        assert_eq!(count_in_terminal(&pane, "已重新连接"), 0);

        pane.poll_events();
        assert_eq!(pane.state, PaneState::Live);
        send(&new_tx, 2, "new session output\r\n");
        pane.poll_events();

        assert_eq!(count_in_terminal(&pane, "已重新连接"), 1);
        assert_eq!(count_in_terminal(&pane, "old line 0"), 1, "最旧的一行没丢");
        let old_last = position_in_terminal(&pane, "old line 29");
        let marker = position_in_terminal(&pane, "已重新连接");
        let fresh_output = position_in_terminal(&pane, "new session output");
        assert!(
            old_last < marker && marker < fresh_output,
            "顺序：旧输出 < 分隔行 < 新输出"
        );
        assert_eq!(marker.1, 3, "分隔行从行首的 \"── \" 开始，\"已\" 在第 4 格");
        // 时间是 HH:MM:SS。
        let line = pane
            .pump()
            .search("已重新连接 ", false)
            .first()
            .map(|found| found.start);
        assert!(line.is_some());

        // 之后的输入只进新通道，旧通道除了那条 Close 什么也没有。
        pane.write(b"ls\r");
        assert!(
            new_sent
                .lock()
                .unwrap()
                .contains(&SessionCommand::Write(b"ls\r".to_vec()))
        );
        assert_eq!(*old_sent.lock().unwrap(), [SessionCommand::Close]);
    }

    #[test]
    fn reconnecting_resets_what_a_dead_remote_program_left_behind() {
        let (mut pane, tx, _) = remote_pane();
        send(&tx, 1, "before\r\n");
        // vim 之类：备用屏幕、隐藏光标、鼠标上报、括号粘贴、应用光标键，然后连接断了。
        send(
            &tx,
            1,
            "\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b[?1hvim",
        );
        tx.send(SessionEvent::Exit {
            generation: 1,
            code: None,
        })
        .unwrap();
        pane.poll_events();
        assert_eq!(pane.state, PaneState::Disconnected);
        assert!(pane.pump().alt_screen());
        assert!(pane.pump().mouse_mode().reports_buttons());

        let (fresh, new_tx, _) = new_connection(2);
        pane.reconnect_with(|| fresh);
        pane.poll_events();
        send(&new_tx, 2, "$ ");
        pane.poll_events();

        assert!(!pane.pump().alt_screen(), "退回主屏幕");
        assert!(!pane.pump().mouse_mode().reports_buttons());
        assert!(!pane.pump().mouse_mode().sgr);
        assert!(!pane.pump().bracketed_paste());
        assert!(!pane.pump().app_cursor());
        assert!(pane.pump().cursor_visible().is_some(), "光标重新可见");
        // 主屏幕上重连之前的内容没动。
        assert_eq!(count_in_terminal(&pane, "before"), 1);
        assert_eq!(
            count_in_terminal(&pane, "vim"),
            0,
            "备用屏幕的内容不属于主屏幕"
        );
        assert_eq!(count_in_terminal(&pane, "已重新连接"), 1);
    }

    #[test]
    fn a_failed_reconnect_keeps_the_history_and_leaves_no_reconnected_line() {
        let (mut pane, tx, _) = remote_pane();
        send(&tx, 1, "kept output\r\n");
        tx.send(SessionEvent::Exit {
            generation: 1,
            code: None,
        })
        .unwrap();
        pane.poll_events();

        pane.reconnect_with(|| {
            let pending = crate::ssh_backend::ready(Err("连接失败：refused".to_owned()));
            Pane::connecting(SIZE, 100, 2, Some(7), pending)
        });
        pane.poll_events();

        assert_eq!(pane.state, PaneState::Disconnected);
        assert_eq!(pane.overlay_reason(), Some("连接失败：refused"));
        assert_eq!(count_in_terminal(&pane, "kept output"), 1);
        assert_eq!(count_in_terminal(&pane, "连接失败：refused"), 1);
        assert_eq!(count_in_terminal(&pane, "已重新连接"), 0);

        // 失败之后再重连一次：历史仍然只有一份，连上时才写分隔行。
        let (fresh, _new_tx, _) = new_connection(3);
        pane.reconnect_with(|| fresh);
        pane.poll_events();
        assert_eq!(pane.state, PaneState::Live);
        assert_eq!(count_in_terminal(&pane, "kept output"), 1);
        assert_eq!(count_in_terminal(&pane, "已重新连接"), 1);
    }

    #[test]
    fn the_first_connection_of_a_pane_does_not_inherit_or_mark_anything() {
        let (mut pane, new_tx, _) = new_connection(1);
        assert!(!pane.carry_terminal);
        pane.poll_events();
        send(&new_tx, 1, "hello\r\n");
        pane.poll_events();
        assert_eq!(count_in_terminal(&pane, "已重新连接"), 0);
        assert_eq!(
            count_in_terminal(&pane, "正在连接"),
            0,
            "首次连上后连接中的提示被新终端替换"
        );
    }

    #[test]
    fn the_reconnect_marker_is_a_dim_line_that_starts_on_its_own_row() {
        let at_start = super::reconnect_marker(true, "14:05:31");
        assert_eq!(at_start, "\x1b[90m── 已重新连接 14:05:31 ──\x1b[0m\r\n");
        let mid_line = super::reconnect_marker(false, "14:05:31");
        assert!(mid_line.starts_with("\r\n\x1b[90m── "), "{mid_line:?}");
        // 本地时间 HH:MM:SS。
        let clock = super::local_clock();
        let parts: Vec<_> = clock.split(':').collect();
        assert_eq!(parts.len(), 3, "{clock}");
        assert!(
            parts
                .iter()
                .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_digit()))
        );
    }

    #[test]
    fn the_reconnect_marker_starts_on_a_fresh_row_after_a_half_written_line() {
        let (mut pane, tx, _) = remote_pane();
        send(&tx, 1, "user@web-1:~$ partial");
        tx.send(SessionEvent::Exit {
            generation: 1,
            code: None,
        })
        .unwrap();
        pane.poll_events();
        let (fresh, _new_tx, _) = new_connection(2);
        pane.reconnect_with(|| fresh);
        pane.poll_events();
        let marker = position_in_terminal(&pane, "已重新连接");
        assert_eq!(marker.1, 3);
        assert_eq!(count_in_terminal(&pane, "partial"), 1);
        // 分隔行不和别的文字挤在一行。
        let text = screen(&pane);
        let line = text.lines().find(|l| l.contains("已重新连接")).unwrap();
        assert!(line.starts_with("── 已重新连接"), "{line}");
    }

    #[test]
    fn typing_while_connecting_is_dropped() {
        let pending = crate::ssh_backend::ready(Err("x".to_owned()));
        let mut pane = Pane::connecting(SIZE, 100, 1, Some(7), pending);
        pane.write(b"early keys");
        assert!(!screen(&pane).contains("early keys"));
    }

    #[test]
    fn remote_exit_marks_the_pane_disconnected_once() {
        let (mut pane, tx, sent) = remote_pane();
        tx.send(SessionEvent::Data {
            generation: 1,
            bytes: b"bye\r\n".to_vec(),
        })
        .unwrap();
        tx.send(SessionEvent::Exit {
            generation: 1,
            code: Some(3),
        })
        .unwrap();
        assert!(pane.poll_events());
        assert_eq!(pane.state, PaneState::Disconnected);
        let text = screen(&pane);
        assert!(text.contains("bye"), "{text}");
        assert!(text.contains("连接已断开（退出码 3）"), "{text}");
        assert!(!text.contains("Ctrl+Shift+R"), "{text}");
        assert_eq!(pane.overlay_reason(), Some("退出码 3"));

        // 之后的输入不再发给后端，也不再重复提示。
        pane.write(b"ls\r");
        assert!(sent.lock().unwrap().is_empty());
        assert!(!pane.poll_events());
        assert_eq!(screen(&pane).matches("连接已断开").count(), 1);
    }

    #[test]
    fn session_error_reason_is_shown() {
        let (mut pane, tx, _) = remote_pane();
        tx.send(SessionEvent::Error {
            generation: 1,
            message: "写入远端失败：broken pipe".to_owned(),
        })
        .unwrap();
        assert!(pane.poll_events());
        let text = screen(&pane);
        assert!(
            text.contains("连接已断开：写入远端失败：broken pipe"),
            "{text}"
        );
    }

    #[test]
    fn offline_pane_close_does_not_claim_a_disconnect() {
        let mut pane = Pane::new(SIZE, 100, 1);
        pane.write(b"echo\r\n");
        assert!(screen(&pane).contains("echo"));
        pane.close();
        pane.poll_events();
        assert_eq!(pane.state, PaneState::Live);
        assert!(!screen(&pane).contains("连接已断开"));
    }

    #[test]
    fn notices_cannot_smuggle_escape_sequences() {
        let text = disconnect_notice(&SessionExit {
            code: None,
            reason: Some("bad\x1b]0;title\x07\r\nreason".to_owned()),
        });
        assert!(!text.contains("\x1b]0;"));
        assert!(!text.contains('\x07'));
        let text = failure_notice("e\x1b[2Jrror");
        assert!(!text.contains("\x1b[2J"));
    }

    /// 临时的会话日志目录 + 指向它的设置；每个用例一个目录。
    fn log_prefs(
        name: &str,
    ) -> (
        tempfile::TempDir,
        std::path::PathBuf,
        crate::app_prefs::AppPrefs,
    ) {
        let guard = tempfile::Builder::new()
            .prefix(&format!("zssh-pane-log-{name}-"))
            .tempdir()
            .expect("临时目录");
        let dir = guard.path().to_path_buf();
        let prefs = crate::app_prefs::AppPrefs {
            session_log_dir: dir.display().to_string(),
            ..Default::default()
        };
        (guard, dir, prefs)
    }

    /// 日志在自己的线程里写：等文件出现并带上期望的内容（最多 10 秒），返回目录里所有 `.log`。
    fn wait_for_log(dir: &std::path::Path, expected: &str) -> Vec<std::path::PathBuf> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let files: Vec<_> = std::fs::read_dir(dir)
                .map(|entries| {
                    entries
                        .filter_map(Result::ok)
                        .map(|entry| entry.path())
                        .filter(|path| path.extension().is_some_and(|ext| ext == "log"))
                        .collect()
                })
                .unwrap_or_default();
            let done = files
                .iter()
                .any(|path| std::fs::read_to_string(path).is_ok_and(|text| text == expected));
            if done || std::time::Instant::now() > deadline {
                return files;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn attach_keeps_the_log_and_syncs_a_resized_window() {
        // 目录守卫先声明：局部变量逆序 drop，pane（连着日志文件）先关，目录后删。
        let (_dir_guard, dir, prefs) = log_prefs("attach");
        let pending = crate::ssh_backend::ready(Err("never read".to_owned()));
        let mut pane = Pane::connecting(SIZE, 100, 1, Some(7), pending);
        // 连接期间窗口变小了，也在记录日志。
        pane.resize(Size { rows: 8, cols: 40 });
        assert!(matches!(
            pane.toggle_log_with(&prefs),
            LogOutcome::Started { .. }
        ));

        let (tx, rx) = std::sync::mpsc::channel();
        let sent = Arc::new(Mutex::new(Vec::new()));
        pane.attach(
            Box::new(Recorder(Arc::clone(&sent))),
            rx,
            Arc::new(FlowGate::new()),
            None,
        );
        assert_eq!(pane.state, PaneState::Live);
        assert!(pane.pump().has_plain_log(), "重连/连上后日志要继续记");
        assert_eq!(
            pane.log_mode,
            LogMode::Manual,
            "手动开的日志不会被当成自动的"
        );
        assert_eq!(
            *sent.lock().unwrap(),
            vec![SessionCommand::Resize(Size { rows: 8, cols: 40 })]
        );

        tx.send(SessionEvent::Data {
            generation: 1,
            bytes: b"hello
"
            .to_vec(),
        })
        .unwrap();
        assert!(pane.poll_events());
        assert!(screen(&pane).contains("hello"));
        assert_eq!(pane.snapshot.size, Size { rows: 8, cols: 40 });
        pane.close();
        // 关窗格不等磁盘，日志在自己的线程里收尾；只有设置里的会话日志目录里这一份。
        let files = wait_for_log(
            &dir, "hello
",
        );
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(
            std::fs::read_to_string(&files[0]).unwrap(),
            "hello
"
        );
    }

    #[test]
    fn manual_logging_writes_one_file_into_the_settings_directory_and_stop_reports_its_path() {
        let (_dir_guard, dir, prefs) = log_prefs("manual");
        let (mut pane, _tx, _sent) = remote_pane();

        let started = pane.toggle_log_with(&prefs);
        let LogOutcome::Started { dir: shown } = &started else {
            panic!("应该开始记录：{started:?}");
        };
        assert_eq!(shown, &dir, "写到设置里的会话日志目录");
        assert!(pane.pump().has_plain_log());
        pane.pump_mut().on_data(
            1, b"one
",
        );

        let stopped = pane.toggle_log_with(&prefs);
        assert!(!pane.pump().has_plain_log(), "停止后窗格不再记录");
        assert_eq!(pane.log_mode, LogMode::Off);
        let LogOutcome::Stopped { path } = &stopped else {
            panic!("应该是停止：{stopped:?}");
        };
        let files = wait_for_log(
            &dir, "one
",
        );
        assert_eq!(files.len(), 1, "{files:?}");
        if let Some(path) = path {
            assert_eq!(path, &files[0], "停止时报的是实际的日志文件");
        }

        // 停止以后收到的输出不再进日志。
        pane.pump_mut().on_data(
            1, b"two
",
        );
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            std::fs::read_to_string(&files[0]).unwrap(),
            "one
"
        );
    }

    #[test]
    fn when_the_setting_already_records_everything_the_pane_does_not_open_a_second_log() {
        let (_dir_guard, dir, prefs) = log_prefs("auto");
        let (mut pane, _tx, _sent) = remote_pane();
        // 设置里自动开着：连上时挂上的那份。
        let auto = crate::app_prefs::start_session_log(&prefs, "web-1").expect("起日志线程");
        pane.pump_mut().set_plain_log(auto);
        pane.log_mode = LogMode::Auto;

        for _ in 0..2 {
            let outcome = pane.toggle_log_with(&prefs);
            assert!(matches!(outcome, LogOutcome::Auto { .. }), "{outcome:?}");
            assert!(
                outcome.status_text().contains("正在记录（自动）"),
                "{}",
                outcome.status_text()
            );
        }
        assert_eq!(pane.log_mode, LogMode::Auto, "这里不能停自动记录");
        assert!(pane.pump().has_plain_log());
        pane.pump_mut().on_data(
            1, b"once
",
        );
        pane.close();
        let files = wait_for_log(
            &dir, "once
",
        );
        assert_eq!(files.len(), 1, "同一份字节只写一处：{files:?}");
    }

    #[test]
    fn the_status_line_never_shows_the_user_directory() {
        let Some(home) = std::env::home_dir().and_then(|home| home.to_str().map(str::to_owned))
        else {
            eprintln!("跳过：找不到家目录");
            return;
        };
        // `redact_paths`（agent_text.rs）目前只认 Windows 的 USERPROFILE，Linux 上还不会脱敏家目录：
        // 那一处归 AI 卡改，改完把这个跳过去掉。
        if !cfg!(windows) {
            eprintln!("跳过：非 Windows 上 redact_paths 还不认家目录");
            return;
        }
        let path = std::path::Path::new(&home).join("logs").join("web-1.log");
        let text = LogOutcome::Stopped { path: Some(path) }.status_text();
        assert!(!text.contains(&home), "{text}");
        assert!(text.contains("日志已停止："), "{text}");
        let text = LogOutcome::Started {
            dir: std::path::Path::new(&home).join("logs"),
        }
        .status_text();
        assert!(!text.contains(&home), "{text}");
    }

    #[test]
    fn an_announced_status_reaches_the_shell_once_with_its_level() {
        let mut line = StatusLine::new();
        assert_eq!(line.take_notice(), None, "什么都没写过就没有提示");
        line.announce(StatusLevel::Error, "设置没能保存");
        assert_eq!(
            line.take_notice(),
            Some(StatusNotice {
                text: "设置没能保存".to_owned(),
                level: StatusLevel::Error,
            })
        );
        assert_eq!(line.take_notice(), None, "取走以后不重复弹");
        line.announce(StatusLevel::Info, "粘贴已取消");
        assert_eq!(line.take_notice().map(|n| n.level), Some(StatusLevel::Info));
    }

    #[test]
    fn a_quiet_status_is_remembered_but_never_popped_up() {
        let mut line = StatusLine::new();
        line.set(StatusLevel::Info, "正在重新连接…");
        assert_eq!(line.text, "正在重新连接…");
        assert_eq!(line.take_notice(), None);
        // 先弹后又被界面上已有表现的状态顶掉：后写的赢，不会弹出过期的那句。
        line.announce(StatusLevel::Error, "旧的失败");
        line.set(StatusLevel::Info, "已断开连接");
        assert_eq!(line.take_notice(), None);
        // 反过来：后写的是要弹的，弹的是后写那句。
        line.set(StatusLevel::Info, "正在重新连接…");
        line.announce(StatusLevel::Info, "日志已续记");
        assert_eq!(
            line.take_notice().map(|n| n.text),
            Some("日志已续记".to_owned())
        );
    }

    #[test]
    fn only_a_log_that_cannot_start_counts_as_an_error() {
        let dir = std::path::PathBuf::from("logs");
        assert_eq!(
            LogOutcome::Failed("x".to_owned()).level(),
            StatusLevel::Error
        );
        assert_eq!(
            LogOutcome::Started { dir: dir.clone() }.level(),
            StatusLevel::Info
        );
        assert_eq!(LogOutcome::Auto { dir }.level(), StatusLevel::Info);
        assert_eq!(
            LogOutcome::Stopped { path: None }.level(),
            StatusLevel::Info
        );
    }
}

/// 标签模型：状态点怎么合成、标签信息、空白标签的判断（不起窗口，只用离线 / 占位窗格）。
#[cfg(test)]
mod tab_tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{
        Axis, LatencyMonitor, Pane, PaneState, QuickSpec, Size, Tab, TabStatus, latency,
        latency_visible_flags, split_tree, tab_status,
    };
    use std::sync::Arc;
    use std::time::Duration;

    const SIZE: Size = Size { rows: 10, cols: 40 };

    /// 按下标顺序列出窗格编号（= 屏幕上的次序）。
    fn order(tab: &Tab) -> Vec<u64> {
        tab.panes.iter().map(|pane| pane.id).collect()
    }

    fn pane_in(state: PaneState, host: Option<i64>) -> Pane {
        Pane::placeholder(SIZE, 100, 1, host, state)
    }

    fn monitor_with(rtt_ms: u64) -> Arc<LatencyMonitor> {
        let monitor = LatencyMonitor::new(latency::ProbeKind::Ssh);
        monitor.record(latency::Sample::Rtt(Duration::from_millis(rtt_ms)));
        monitor
    }

    #[test]
    fn a_pane_registers_whether_it_is_visible_and_forgets_it_on_disconnect() {
        let mut pane = pane_in(PaneState::Live, None);
        let monitor = LatencyMonitor::new(latency::ProbeKind::Ssh);
        pane.set_latency(Some(Arc::clone(&monitor)));
        assert_eq!(
            monitor.visible_viewers(),
            0,
            "接上时默认看不见，等外壳的节拍"
        );
        assert!(pane.latency_snapshot().is_none(), "还没有第一个值不画徽标");

        monitor.record(latency::Sample::Rtt(Duration::from_millis(42)));
        pane.set_latency_visible(true);
        pane.set_latency_visible(true);
        assert_eq!(monitor.visible_viewers(), 1);
        let (snapshot, _) = pane.latency_snapshot().expect("有值就画");
        assert_eq!(
            snapshot.stats.last(),
            Some(latency::Sample::Rtt(Duration::from_millis(42)))
        );

        pane.set_latency_visible(false);
        assert_eq!(monitor.visible_viewers(), 0, "切到后台标签 / 最小化：暂停");
        pane.set_latency_visible(true);
        pane.disconnect("断开".to_owned(), "断开");
        assert!(pane.latency.is_none() && pane.latency_snapshot().is_none());
        assert_eq!(monitor.visible_viewers(), 0, "断开后不再占着监视器");
    }

    #[test]
    fn panes_on_one_connection_share_a_value_and_count_separately() {
        let monitor = monitor_with(8);
        let mut a = pane_in(PaneState::Live, None);
        let mut b = pane_in(PaneState::Live, None);
        a.set_latency(Some(Arc::clone(&monitor)));
        b.set_latency(Some(Arc::clone(&monitor)));
        a.set_latency_visible(true);
        b.set_latency_visible(true);
        assert_eq!(monitor.visible_viewers(), 2);
        assert_eq!(
            a.latency_snapshot().map(|(s, _)| s),
            b.latency_snapshot().map(|(s, _)| s),
            "同一条连接上的窗格显示同一个值"
        );
        a.set_latency_visible(false);
        assert_eq!(monitor.visible_viewers(), 1, "还有一个窗格看得见，测量继续");
    }

    #[test]
    fn a_pane_without_a_monitor_shows_nothing_and_ignores_visibility() {
        // 串口、经跳板机的 Telnet、离线窗格：没有监视器。
        let mut pane = pane_in(PaneState::Live, None);
        pane.set_latency(None);
        pane.set_latency_visible(true);
        assert!(pane.latency_snapshot().is_none());
    }

    #[test]
    fn only_the_active_tab_of_a_shown_window_with_the_setting_on_counts_as_visible() {
        assert_eq!(
            latency_visible_flags(2, true, true, true, true),
            [true, true]
        );
        assert_eq!(
            latency_visible_flags(2, false, true, true, true),
            [false, false]
        );
        assert_eq!(latency_visible_flags(1, true, false, true, true), [false]);
        assert_eq!(latency_visible_flags(1, true, true, false, true), [false]);
        assert_eq!(latency_visible_flags(1, true, true, true, false), [false]);
    }

    #[test]
    fn the_tab_dot_prefers_connecting_then_connected_then_disconnected() {
        use PaneState::{Connecting, Disconnected, Empty, Live};
        assert_eq!(tab_status(&[Live, Connecting]), TabStatus::Connecting);
        assert_eq!(tab_status(&[Disconnected, Live]), TabStatus::Connected);
        assert_eq!(
            tab_status(&[Disconnected, Disconnected]),
            TabStatus::Disconnected
        );
        assert_eq!(tab_status(&[Empty]), TabStatus::Empty);
        assert_eq!(tab_status(&[]), TabStatus::Empty);
        // 分屏里一个断了、另一个还连着：整个标签看着还是「已连接」，重连要靠右键菜单。
        assert_eq!(tab_status(&[Live, Disconnected]), TabStatus::Connected);
    }

    #[test]
    fn a_blank_tab_has_nothing_to_duplicate_or_reconnect() {
        let tab = Tab::new(
            crate::i18n::t(super::TermMsg::NewTab).to_owned(),
            pane_in(PaneState::Empty, None),
        );
        assert!(tab.is_blank());
        let info = tab.info();
        assert_eq!(info.status, TabStatus::Empty);
        assert_eq!(info.host_id, None);
        assert!(!info.can_reconnect);
    }

    fn quick_pane(state: PaneState) -> Pane {
        let mut pane = pane_in(state, None);
        pane.quick = Some(Arc::new(QuickSpec {
            target: zssh_core::model::QuickTarget {
                username: "ops".to_owned(),
                host: "192.0.2.5".to_owned(),
                port: 22,
            },
            password: "hunter2".to_owned(),
            label: "ops@192.0.2.5".to_owned(),
        }));
        pane
    }

    #[test]
    fn a_quick_connect_pane_is_a_session_even_without_a_saved_host() {
        let down = Tab::new(
            "ops@192.0.2.5".to_owned(),
            quick_pane(PaneState::Disconnected),
        );
        let info = down.info();
        assert_eq!(info.host_id, None, "快速连接没有主机记录");
        assert_eq!(info.quick_target.as_deref(), Some("ops@192.0.2.5"));
        // 归并用的键带上端口（输入时没写端口也一样），地址不分大小写。
        assert_eq!(info.quick_key.as_deref(), Some("ops@192.0.2.5:22"));
        assert!(info.can_reconnect, "断线后能重连（地址和密码都记着）");
        assert!(
            !Tab::new("x".to_owned(), pane_in(PaneState::Disconnected, None))
                .info()
                .can_reconnect,
            "离线演示窗格没有会话，不能重连"
        );
    }

    #[test]
    fn the_password_of_a_quick_connect_never_reaches_a_debug_print() {
        let pane = quick_pane(PaneState::Live);
        let text = format!("{:?}", pane.quick.as_ref().expect("有快速连接信息"));
        assert!(text.contains("ops@192.0.2.5"));
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn only_a_disconnected_host_pane_can_be_reconnected() {
        let down = Tab::new("a".to_owned(), pane_in(PaneState::Disconnected, Some(7)));
        let info = down.info();
        assert_eq!(info.status, TabStatus::Disconnected);
        assert_eq!(info.host_id, Some(7));
        assert!(info.can_reconnect);

        // 离线演示窗格（没有主机）断不断都谈不上重连。
        let offline = Tab::new("b".to_owned(), pane_in(PaneState::Disconnected, None));
        assert!(!offline.info().can_reconnect);

        let live = Tab::new("c".to_owned(), pane_in(PaneState::Live, Some(7)));
        assert!(!live.info().can_reconnect);
        assert!(!live.is_blank());
    }

    #[test]
    fn disconnect_is_offered_only_while_a_host_session_is_up_or_coming_up() {
        for (state, host, want) in [
            (PaneState::Live, Some(7), true),
            (PaneState::Connecting, Some(7), true),
            (PaneState::Disconnected, Some(7), false),
            (PaneState::Empty, None, false),
            // 离线演示窗格不是会话，没有东西可断。
            (PaneState::Live, None, false),
        ] {
            let tab = Tab::new("t".to_owned(), pane_in(state, host));
            assert_eq!(tab.info().can_disconnect, want, "{state:?} {host:?}");
        }
        let mut quick = Tab::new("q".to_owned(), quick_pane(PaneState::Live));
        assert!(quick.info().can_disconnect, "快速连接也能断");
        assert_eq!(quick.disconnect_sessions(), 1);
        assert!(
            quick.info().can_reconnect,
            "断开之后地址和密码还记着，能重连"
        );
    }

    #[test]
    fn disconnecting_a_split_tab_cuts_every_session_pane_and_keeps_the_tab() {
        let mut tab = Tab::new("a".to_owned(), pane_in(PaneState::Live, Some(1)));
        tab.split_active(Axis::X, pane_in(PaneState::Live, Some(1)));
        tab.split_active(Axis::Y, pane_in(PaneState::Connecting, Some(1)));
        assert_eq!(tab.info().status, TabStatus::Connecting);
        assert_eq!(tab.disconnect_sessions(), 3, "分屏的每个窗格都断");
        assert_eq!(tab.panes.len(), 3, "标签和窗格都还在，只是断了");
        assert!(
            tab.panes
                .iter()
                .all(|pane| pane.state == PaneState::Disconnected && pane.factory.is_none())
        );
        let info = tab.info();
        assert_eq!(info.status, TabStatus::Disconnected, "标签显示已断开");
        assert!(info.can_reconnect, "能用「重新连接」恢复");
        assert!(!info.can_disconnect);
        assert_eq!(tab.disconnect_sessions(), 0, "再断一次什么也不做");
    }

    #[test]
    fn input_goes_to_the_focused_pane_unless_the_tab_broadcasts() {
        let mut tab = Tab::new("a".to_owned(), pane_in(PaneState::Live, Some(1)));
        tab.split_active(Axis::X, pane_in(PaneState::Live, Some(1)));
        tab.split_active(Axis::Y, pane_in(PaneState::Live, Some(1)));
        tab.active_pane = 1;
        assert_eq!(tab.input_targets(), vec![1]);
        tab.broadcast = true;
        assert_eq!(
            tab.input_targets(),
            vec![0, 1, 2],
            "广播：当前标签的所有分屏窗格，而且只有它们"
        );
        // 聚焦下标越界（窗格刚被关掉）时钳到最后一个，不会 panic。
        tab.broadcast = false;
        tab.active_pane = 9;
        assert_eq!(tab.input_targets(), vec![2]);
    }

    #[test]
    fn a_tab_starts_as_a_single_pane_tree() {
        let tab = Tab::new("a".to_owned(), pane_in(PaneState::Live, Some(1)));
        assert!(!tab.is_split());
        assert_eq!(tab.tree.pane_ids(), order(&tab));
    }

    #[test]
    fn splitting_puts_the_new_pane_after_the_focused_one_and_focuses_it() {
        let mut tab = Tab::new("a".to_owned(), pane_in(PaneState::Live, Some(1)));
        let first = tab.panes[0].id;
        assert_eq!(
            tab.split_active(Axis::X, pane_in(PaneState::Live, Some(1))),
            1
        );
        assert_eq!(tab.active_pane, 1, "新窗格拿到焦点");
        // 聚焦回第一个，再上下拆：新窗格紧跟在它后面，不是排到最后。
        tab.active_pane = 0;
        assert_eq!(
            tab.split_active(Axis::Y, pane_in(PaneState::Live, Some(1))),
            1
        );
        assert_eq!(tab.panes.len(), 3);
        assert_eq!(tab.panes[0].id, first);
        assert_eq!(tab.tree.pane_ids(), order(&tab), "窗格表和树同序");
        let axes: Vec<Axis> = tab.tree.splits().iter().map(|split| split.1).collect();
        assert_eq!(axes, vec![Axis::X, Axis::Y], "先左右拆，再把左半上下拆");
    }

    #[test]
    fn a_four_pane_grid_can_be_built_and_each_pane_has_its_own_rect() {
        let mut tab = Tab::new("a".to_owned(), pane_in(PaneState::Live, Some(1)));
        tab.split_active(Axis::X, pane_in(PaneState::Live, Some(1))); // A | B
        tab.split_active(Axis::Y, pane_in(PaneState::Live, Some(1))); // B 上下：B / C
        tab.active_pane = 0;
        tab.split_active(Axis::Y, pane_in(PaneState::Live, Some(1))); // A 上下：A / D
        assert_eq!(tab.panes.len(), 4);
        assert_eq!(tab.tree.pane_ids(), order(&tab));
        let layout = split_tree::compute_layout(&tab.tree, 1004.0, 604.0);
        assert_eq!(split_tree::check_tiling(&layout, 1004.0, 604.0), Ok(()));
        let rects: Vec<_> = tab
            .panes
            .iter()
            .map(|pane| layout.rect_of(pane.id).expect("每个窗格都有矩形"))
            .collect();
        // 下标 0、1 在左半（上、下），2、3 在右半（上、下）。
        assert!(rects[0].x == rects[1].x && rects[1].y > rects[0].y);
        assert!(rects[2].x == rects[3].x && rects[3].y > rects[2].y);
        assert!(rects[2].x > rects[0].x);
    }

    #[test]
    fn closing_a_pane_merges_its_sibling_and_moves_focus_like_0_2_14() {
        let mut tab = Tab::new("a".to_owned(), pane_in(PaneState::Live, Some(1)));
        tab.split_active(Axis::X, pane_in(PaneState::Live, Some(1)));
        tab.split_active(Axis::Y, pane_in(PaneState::Live, Some(1)));
        // A | (B / C)，焦点在 C（下标 2）。
        let before = order(&tab);
        // 关的不是焦点（B）：焦点还在 C，只是下标变了。
        let closed = tab.close_pane_at(1).expect("关得掉");
        assert_eq!(closed.id, before[1]);
        assert_eq!(order(&tab), vec![before[0], before[2]]);
        assert_eq!(tab.panes[tab.active_pane].id, before[2]);
        assert_eq!(tab.tree.pane_ids(), order(&tab));
        // 剩下 A | C：关焦点窗格 C，焦点落到相邻的 A。
        let closed = tab.close_pane_at(1).expect("关得掉");
        assert_eq!(closed.id, before[2]);
        assert_eq!(tab.panes.len(), 1);
        assert_eq!(tab.active_pane, 0);
        assert!(!tab.is_split());
        // 只剩一个窗格：关不掉（调用方该关整个标签）。
        assert!(tab.close_pane_at(0).is_none());
    }

    #[test]
    fn closing_the_middle_pane_of_four_keeps_the_other_three_in_place() {
        let mut tab = Tab::new("a".to_owned(), pane_in(PaneState::Live, Some(1)));
        tab.split_active(Axis::X, pane_in(PaneState::Live, Some(1))); // A | B
        tab.active_pane = 0;
        tab.split_active(Axis::Y, pane_in(PaneState::Live, Some(1))); // A / C | B
        tab.active_pane = 2;
        tab.split_active(Axis::Y, pane_in(PaneState::Live, Some(1))); // A / C | B / D
        let ids = order(&tab); // [A, C, B, D]
        assert_eq!(ids.len(), 4);
        tab.active_pane = 1; // 焦点在 C
        let closed = tab.close_pane_at(1).expect("关得掉"); // 关掉 C
        assert_eq!(closed.id, ids[1]);
        assert_eq!(order(&tab), vec![ids[0], ids[2], ids[3]]);
        assert_eq!(tab.panes[tab.active_pane].id, ids[0], "焦点落到兄弟 A");
        // A 现在占满左半，B / D 仍是右半的上下分。
        let layout = split_tree::compute_layout(&tab.tree, 1004.0, 604.0);
        assert_eq!(split_tree::check_tiling(&layout, 1004.0, 604.0), Ok(()));
        let a = layout.rect_of(ids[0]).expect("A");
        let b = layout.rect_of(ids[2]).expect("B");
        let d = layout.rect_of(ids[3]).expect("D");
        assert_eq!(a.h, 604.0);
        assert!(b.x > a.x + a.w && d.x == b.x && d.y > b.y);
    }

    #[test]
    fn a_blank_tab_that_was_split_becomes_one_pane_when_a_host_is_chosen() {
        let mut tab = Tab::new(
            crate::i18n::t(super::TermMsg::NewTab).to_owned(),
            pane_in(PaneState::Empty, None),
        );
        tab.split_active(Axis::X, pane_in(PaneState::Empty, None));
        assert!(tab.is_blank() && tab.is_split());
        let replacement = pane_in(PaneState::Connecting, Some(3));
        let id = replacement.id;
        tab.reset_to(replacement);
        assert_eq!(order(&tab), vec![id]);
        assert_eq!(tab.tree.pane_ids(), vec![id]);
        assert!(!tab.is_split());
        assert_eq!(tab.active_pane, 0);
    }

    #[test]
    fn broadcast_reaches_every_pane_of_a_split_tab() {
        let mut tab = Tab::new("a".to_owned(), pane_in(PaneState::Live, Some(1)));
        for axis in [Axis::X, Axis::Y, Axis::X] {
            tab.split_active(axis, pane_in(PaneState::Live, Some(1)));
        }
        tab.broadcast = true;
        assert_eq!(tab.input_targets(), vec![0, 1, 2, 3]);
        // 关掉中间一个之后广播的对象跟着变，不会指向已经不存在的窗格。
        tab.close_pane_at(1);
        assert_eq!(tab.input_targets(), vec![0, 1, 2]);
    }

    #[test]
    fn a_pane_uses_the_scrollback_it_was_opened_with_and_follows_later_changes() {
        let mut pane = Pane::new(SIZE, 50, 1);
        assert_eq!(pane.pump().history_limit(), 50);
        assert_eq!(pane.history, 50);
        for i in 0..120 {
            pane.write(format!("line {i}\r\n").as_bytes());
        }
        assert!(pane.pump().history_size() <= 50);
        assert!(pane.pump().history_size() > 30, "历史应该攒起来了");
        // 调小：已有的历史立刻被裁。
        pane.set_history(10);
        assert_eq!(pane.pump().history_limit(), 10);
        assert!(pane.pump().history_size() <= 10);
        // 调大：之后能攒更多，重连新建的泵也用新值。
        pane.set_history(80);
        for i in 0..200 {
            pane.write(format!("more {i}\r\n").as_bytes());
        }
        let size = pane.pump().history_size();
        assert!(size > 10 && size <= 80, "history={size}");
        assert_eq!(pane.history, 80);
    }

    #[test]
    fn tab_ids_are_unique_across_tabs() {
        let a = Tab::new("a".to_owned(), pane_in(PaneState::Empty, None));
        let b = Tab::new("b".to_owned(), pane_in(PaneState::Empty, None));
        assert_ne!(a.id, b.id, "标签能在窗口之间搬，id 不能撞");
    }
}

/// 联网验证：真实会话里的大输出、Ctrl+C、less、断线和重连，走和界面相同的
/// `Pane` 路径（后台连接 → 60 ms 轮询 → 账本/流控）。需要 `ZSSH_LIVE_FULL=1` 和
/// `ZSSH_TEST_HOST/PORT/USER` + `ZSSH_TEST_KEY_PATH`；缺了就打印原因跳过。
#[cfg(test)]
mod live_tests {
    use super::{Pane, PaneState, Size};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use zssh_core::model::ProxyKind;
    use zssh_core::ssh::{
        AuthConfig, BoxFuture, ChainHop, Connection, HostKeyDecision, HostKeyInfo, Interactor,
        KeySource, KnownHosts, PromptAnswers, PromptRequest, ProxyConfig,
    };

    const LIVE: Size = Size {
        rows: 30,
        cols: 120,
    };

    /// 测试里没有人点对话框：主机指纹仅本次信任，其余提问一律取消。
    struct TrustOnce;

    impl Interactor for TrustOnce {
        fn confirm_host_key(&self, _info: HostKeyInfo) -> BoxFuture<'_, HostKeyDecision> {
            Box::pin(async { HostKeyDecision::TrustOnce })
        }

        fn prompt(&self, _request: PromptRequest) -> BoxFuture<'_, Option<PromptAnswers>> {
            Box::pin(async { None })
        }
    }

    fn env(name: &str) -> Option<String> {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    }

    fn proxy_from_env() -> Option<ProxyConfig> {
        let raw = env("ZSSH_TEST_SOCKS5")?;
        let (host, port) = raw.rsplit_once(':')?;
        Some(ProxyConfig {
            kind: ProxyKind::Socks5,
            host: host.to_owned(),
            port: port.parse().ok()?,
            username: None,
            password: None,
        })
    }

    fn screen(pane: &Pane) -> String {
        pane.snapshot
            .rows
            .iter()
            .map(|row| row.text.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn has_line(pane: &Pane, wanted: &str) -> bool {
        pane.snapshot
            .rows
            .iter()
            .any(|row| row.text.trim() == wanted)
    }

    /// 像界面一样每 60 ms 轮询一次，直到条件成立或超时。
    fn wait_until(pane: &mut Pane, timeout: Duration, mut done: impl FnMut(&Pane) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            pane.poll_events();
            if done(pane) {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
    }

    fn type_line(pane: &mut Pane, line: &str) {
        pane.write(format!("{line}\r").as_bytes());
    }

    #[test]
    fn real_session_flood_interrupt_less_disconnect_and_reopen() {
        if env("ZSSH_LIVE_FULL").as_deref() != Some("1") {
            println!("跳过：没有设置 ZSSH_LIVE_FULL=1");
            return;
        }
        let (Some(host), Some(user), Some(key)) = (
            env("ZSSH_TEST_HOST"),
            env("ZSSH_TEST_USER"),
            env("ZSSH_TEST_KEY_PATH"),
        ) else {
            println!("跳过：缺少 ZSSH_TEST_HOST / ZSSH_TEST_USER / ZSSH_TEST_KEY_PATH");
            return;
        };
        let port = env("ZSSH_TEST_PORT")
            .and_then(|raw| raw.parse::<u16>().ok())
            .unwrap_or(22);
        let _known_hosts_guard = tempfile::Builder::new()
            .prefix("zssh-f1-kh-")
            .tempdir()
            .expect("临时目录");
        let known_hosts = _known_hosts_guard.path().join("known_hosts");
        let hop = ChainHop {
            label: "测试服务器".to_owned(),
            host,
            port,
            username: user,
            proxy: proxy_from_env(),
            auth: AuthConfig::key(KeySource::File {
                path: key.into(),
                passphrase: env("ZSSH_TEST_KEY_PASSPHRASE"),
            }),
            known_hosts: KnownHosts::new(&known_hosts),
            legacy_algorithms: false,
        };
        let runtime = crate::ssh_backend::runtime().expect("runtime");
        let connection = runtime
            .block_on(Connection::connect_chain(vec![hop], Arc::new(TrustOnce)))
            .expect("连接测试服务器");
        let factory = crate::ssh_backend::SshSessionFactory::from_connection(connection);

        // 1. 打开真实会话：先是"正在连接"，轮询后接上。
        let mut pane = Pane::start_ssh(Some(Arc::clone(&factory)), Some(1), LIVE, 10_000, 1);
        assert_eq!(pane.state, PaneState::Connecting);
        assert!(
            wait_until(&mut pane, Duration::from_secs(20), |p| p.state
                == PaneState::Live),
            "会话应在 20 秒内接上：{}",
            screen(&pane)
        );
        // 全程只在自己的 /tmp/zssh-f1 下操作。
        type_line(
            &mut pane,
            "mkdir -p /tmp/zssh-f1 && cd /tmp/zssh-f1 && echo READY_$((40+2))",
        );
        assert!(
            wait_until(&mut pane, Duration::from_secs(20), |p| has_line(
                p, "READY_42"
            )),
            "shell 应能交互：{}",
            screen(&pane)
        );

        // 2. 大输出（B-1）：约 14 MB，累计远超 1 MiB 高水位，之后仍能交互。
        let started = Instant::now();
        type_line(&mut pane, "seq 1 2000000");
        assert!(
            wait_until(&mut pane, Duration::from_secs(180), |p| has_line(
                p, "2000000"
            )),
            "大输出应全部显示完；账本 {:?}",
            pane.pump().ledger()
        );
        let flood = started.elapsed();
        wait_until(&mut pane, Duration::from_secs(5), |p| {
            p.pump().gate().unacked() == 0
        });
        let ledger = pane.pump().ledger();
        println!(
            "大输出：{} 字节，用时 {:?}，未确认 {}",
            ledger.raw,
            flood,
            pane.pump().gate().unacked()
        );
        assert!(ledger.raw >= 14_000_000, "输出量不对：{ledger:?}");
        assert_eq!(pane.pump().gate().unacked(), 0, "流控账必须回到 0");
        type_line(&mut pane, "echo AFTER_$((1+2))");
        assert!(
            wait_until(&mut pane, Duration::from_secs(10), |p| has_line(
                p, "AFTER_3"
            )),
            "大输出之后还要能交互：{}",
            screen(&pane)
        );

        // 3. Ctrl+C 中断 sleep 100：^C 字节真的送到远端。
        let started = Instant::now();
        type_line(&mut pane, "sleep 100");
        std::thread::sleep(Duration::from_millis(800));
        pane.write(b"\x03");
        type_line(&mut pane, "echo INT_$((20+2))");
        assert!(
            wait_until(&mut pane, Duration::from_secs(10), |p| has_line(
                p, "INT_22"
            )),
            "Ctrl+C 应当中断 sleep：{}",
            screen(&pane)
        );
        println!("Ctrl+C 中断 sleep 100：{:?}", started.elapsed());
        assert!(started.elapsed() < Duration::from_secs(20));

        // 4. less 进入再退出。
        type_line(&mut pane, "seq 1 200 > f.txt; less f.txt");
        assert!(
            // less 只画第一屏，底部提示行带文件名；看不到 199。
            wait_until(&mut pane, Duration::from_secs(10), |p| has_line(p, "10")
                && screen(p).contains("f.txt")),
            "less 应当显示文件内容：{}",
            screen(&pane)
        );
        std::thread::sleep(Duration::from_millis(500));
        pane.write(b"q");
        type_line(&mut pane, "echo LESS_$((5+5))");
        assert!(
            wait_until(&mut pane, Duration::from_secs(10), |p| has_line(
                p, "LESS_10"
            )),
            "q 之后应回到 shell：{}",
            screen(&pane)
        );

        // 5. 清理自己的目录，然后杀掉自己的 shell：界面应提示已断开。
        type_line(
            &mut pane,
            "cd /tmp && rm -rf /tmp/zssh-f1 && echo CLEAN_$((7+1))",
        );
        assert!(wait_until(
            &mut pane,
            Duration::from_secs(10),
            |p| has_line(p, "CLEAN_8")
        ));
        type_line(&mut pane, "kill -9 $$");
        assert!(
            wait_until(&mut pane, Duration::from_secs(20), |p| p.state
                == PaneState::Disconnected),
            "shell 被杀后窗格应显示已断开：{}",
            screen(&pane)
        );
        let text = screen(&pane);
        assert!(text.contains("连接已断开"), "{text}");
        assert!(pane.overlay_reason().is_some(), "{text}");
        // 断开后的输入不回显、不发送。
        pane.write(b"must-not-appear\r");
        assert!(!screen(&pane).contains("must-not-appear"));
        println!(
            "断线提示：{}",
            text.lines()
                .filter(|line| line.contains("连接已断开"))
                .collect::<Vec<_>>()
                .join(" | ")
        );

        // 6. 重连：同一条连接上重新开 shell，窗格恢复可用。
        let mut again = Pane::start_ssh(Some(factory), Some(1), LIVE, 10_000, 1);
        assert!(wait_until(&mut again, Duration::from_secs(20), |p| p.state
            == PaneState::Live));
        type_line(&mut again, "echo RECONNECT_$((3+4))");
        assert!(
            wait_until(&mut again, Duration::from_secs(10), |p| has_line(
                p,
                "RECONNECT_7"
            )),
            "重连后应可交互：{}",
            screen(&again)
        );
        again.close();
    }

    /// 连上进程内的迷你 SSH 服务器（本机回环，不碰任何外部主机）。
    fn local_factory() -> Arc<crate::ssh_backend::SshSessionFactory> {
        let runtime = crate::ssh_backend::runtime().expect("runtime");
        let addr = runtime.block_on(crate::test_ssh_server::spawn());
        let known_hosts_guard = tempfile::Builder::new()
            .prefix("zssh-f1-local-kh-")
            .tempdir()
            .expect("临时目录");
        let known_hosts = known_hosts_guard.path().join("known_hosts");
        let hop = ChainHop {
            label: "本机测试服务器".to_owned(),
            host: addr.ip().to_string(),
            port: addr.port(),
            username: "tester".to_owned(),
            proxy: None,
            auth: AuthConfig::password(Some("pw".to_owned())),
            known_hosts: KnownHosts::new(&known_hosts),
            legacy_algorithms: false,
        };
        let connection = runtime
            .block_on(Connection::connect_chain(vec![hop], Arc::new(TrustOnce)))
            .expect("连接本机测试服务器");
        crate::ssh_backend::SshSessionFactory::from_connection(connection)
    }

    fn open_live(factory: &Arc<crate::ssh_backend::SshSessionFactory>) -> Pane {
        let mut pane = Pane::start_ssh(Some(Arc::clone(factory)), Some(1), LIVE, 10_000, 1);
        assert_eq!(pane.state, PaneState::Connecting);
        assert!(
            wait_until(&mut pane, Duration::from_secs(20), |p| has_line(
                p,
                "MINI_READY"
            )),
            "会话应接上：{}",
            screen(&pane)
        );
        assert_eq!(pane.state, PaneState::Live);
        pane
    }

    /// V2-C1 端到端：服务端发 `ESC[6n` / `ESC[c`，窗格自己的终端回答，经真实 SSH 通道回到服务端
    /// （服务端把收到的回复原样回显成 `GOT_REPLY:...`）。没有任何用户输入参与；窗格不重绘也一样。
    #[test]
    fn a_real_ssh_channel_gets_the_terminal_protocol_replies() {
        let factory = local_factory();
        let mut pane = open_live(&factory);
        type_line(&mut pane, "cpr");
        assert!(
            wait_until(&mut pane, Duration::from_secs(10), |p| screen(p)
                .contains("GOT_REPLY:^[[5;12R")),
            "光标位置回复应到达服务端：{}",
            screen(&pane)
        );
        type_line(&mut pane, "da");
        assert!(
            wait_until(&mut pane, Duration::from_secs(10), |p| screen(p)
                .contains("GOT_REPLY:^[[?6c")),
            "设备属性回复应到达服务端：{}",
            screen(&pane)
        );
        // 回复是协议应答不是按键：服务端的行缓冲里没有它们（再按回车只会回显空行）。
        type_line(&mut pane, "");
        assert!(
            wait_until(&mut pane, Duration::from_secs(10), |p| has_line(p, "ECHO:")),
            "{}",
            screen(&pane)
        );
        pane.close();
    }

    /// 真连进程内迷你 SSH 服务器：开 shell 就起这条连接的延迟测量，窗格从连接上取到同一份监视器；
    /// 看不见时不测，看得见时测到真实往返；同一条连接上的第二个窗格拿到的是同一个监视器。
    #[test]
    fn a_local_ssh_pane_measures_a_real_round_trip_only_while_visible() {
        let factory = local_factory();
        let mut first = open_live(&factory);
        let mut second = open_live(&factory);
        let monitor = Arc::clone(factory.connection().latency());
        assert!(monitor.is_started(), "开 shell 时起了测量任务");
        for pane in [&first, &second] {
            let own = pane.latency.as_ref().expect("SSH 窗格有延迟监视器");
            assert!(
                Arc::ptr_eq(own.viewer.monitor(), &monitor),
                "共用同一个监视器"
            );
        }

        // 都没登记成看得见：等一阵也不测。
        std::thread::sleep(Duration::from_millis(800));
        assert!(monitor.snapshot().stats.is_empty());

        first.set_latency_visible(true);
        assert!(
            wait_until(&mut first, Duration::from_secs(10), |p| p
                .latency_snapshot()
                .is_some()),
            "看得见之后应当很快测到第一个值"
        );
        let (snapshot, _) = first.latency_snapshot().expect("有值");
        assert!(matches!(
            snapshot.stats.last(),
            Some(zssh_core::ssh::latency::Sample::Rtt(rtt)) if rtt > Duration::ZERO
        ));
        assert_eq!(
            second.latency_snapshot().map(|(s, _)| s),
            Some(snapshot),
            "同一条连接上的另一个窗格读到同一个值"
        );

        first.close();
        second.close();
    }

    /// B-1 的端到端验证：真实 russh 传输 + 核心流控，输出 6 MiB（高水位的 6 倍）
    /// 仍然全部显示，账本回到 0，之后还能交互；Ctrl+C 字节到达远端；
    /// 远端退出和整条连接被断开都会让窗格显示crate::i18n::t(super::TermMsg::DisconnectedBadge)。
    #[test]
    fn local_server_flood_ctrl_c_exit_and_dropped_connection() {
        let factory = local_factory();

        let mut pane = open_live(&factory);
        type_line(&mut pane, "hello");
        assert!(wait_until(
            &mut pane,
            Duration::from_secs(10),
            |p| has_line(p, "ECHO:hello")
        ));

        let started = Instant::now();
        type_line(&mut pane, "flood");
        assert!(
            wait_until(&mut pane, Duration::from_secs(60), |p| has_line(
                p,
                "FLOOD_DONE"
            )),
            "大输出应全部显示完；账本 {:?}，未确认 {}",
            pane.pump().ledger(),
            pane.pump().gate().unacked()
        );
        println!(
            "本机服务器大输出：{} 字节，用时 {:?}",
            pane.pump().ledger().raw,
            started.elapsed()
        );
        assert!(pane.pump().ledger().raw >= crate::test_ssh_server::FLOOD_BYTES);
        wait_until(&mut pane, Duration::from_secs(5), |p| {
            p.pump().gate().unacked() == 0
        });
        assert_eq!(pane.pump().gate().unacked(), 0, "流控账必须回到 0");
        type_line(&mut pane, "after");
        assert!(wait_until(
            &mut pane,
            Duration::from_secs(10),
            |p| has_line(p, "ECHO:after")
        ));

        pane.write(b"\x03");
        assert!(wait_until(
            &mut pane,
            Duration::from_secs(10),
            |p| has_line(p, "GOT_CTRL_C")
        ));

        // 远端退出：显示退出码，之后的输入不再发出。
        type_line(&mut pane, "bye");
        assert!(wait_until(&mut pane, Duration::from_secs(10), |p| p.state
            == PaneState::Disconnected));
        assert!(
            screen(&pane).contains("连接已断开（退出码 3）"),
            "{}",
            screen(&pane)
        );
        pane.write(b"ignored\r");
        assert!(!screen(&pane).contains("ECHO:ignored"));

        // 同一条连接上重新开一个会话（重连）。
        let mut again = open_live(&factory);
        type_line(&mut again, "again");
        assert!(wait_until(&mut again, Duration::from_secs(10), |p| {
            has_line(p, "ECHO:again")
        }));

        // 整条连接被断开：窗格提示断开；再往这条死连接上开 shell 要明确失败，不能卡住。
        type_line(&mut again, "drop");
        assert!(
            wait_until(&mut again, Duration::from_secs(20), |p| p.state
                == PaneState::Disconnected),
            "{}",
            screen(&again)
        );
        assert!(screen(&again).contains("连接已断开"), "{}", screen(&again));

        let mut dead = Pane::start_ssh(Some(factory), Some(1), LIVE, 10_000, 1);
        assert!(
            wait_until(&mut dead, Duration::from_secs(30), |p| p.state
                == PaneState::Disconnected),
            "死连接上开 shell 应当明确失败：{}",
            screen(&dead)
        );
        assert!(dead.overlay_reason().is_some(), "{}", screen(&dead));
        assert_eq!(dead.host_id, Some(1), "失败后仍可重连");
    }
    /// V2-U1 第 3 条：标签右键「断开连接」只放掉这个窗格自己的那份连接引用。同一条 SSH 连接上
    /// 另一个窗格（别的标签 / 分屏 / SFTP）照常能用；最后一个使用者走了，连接才真的释放。
    #[test]
    fn disconnecting_one_pane_leaves_the_shared_connection_to_the_others() {
        let factory = local_factory();
        let weak_connection = Arc::downgrade(&factory.connection());
        let mut a = open_live(&factory);
        let mut b = open_live(&factory);
        let before = Arc::strong_count(&factory);
        assert!(before >= 3, "测试自己 + 两个窗格各持一份：{before}");

        a.disconnect_by_user();
        assert_eq!(a.state, PaneState::Disconnected);
        assert!(a.factory.is_none(), "放掉自己那份引用");
        assert_eq!(
            Arc::strong_count(&factory),
            before - 1,
            "只少了这个窗格的一份"
        );
        assert!(screen(&a).contains("已断开连接（手动）"), "{}", screen(&a));
        assert!(a.overlay_reason().is_some(), "{}", screen(&a));
        // 断开之后输入不再发出，也不回显。
        a.write(b"must-not-appear\r");
        assert!(!screen(&a).contains("ECHO:must-not-appear"));

        // 另一个窗格不受影响：同一条连接照常收发。
        assert_eq!(b.state, PaneState::Live);
        assert!(!factory.connection().is_closed(), "连接还有别人在用");
        type_line(&mut b, "still-here");
        assert!(
            wait_until(&mut b, Duration::from_secs(10), |p| has_line(
                p,
                "ECHO:still-here"
            )),
            "{}",
            screen(&b)
        );
        // 断开的这个可以在同一条连接上重新连。
        let mut again = open_live(&factory);
        type_line(&mut again, "back");
        assert!(wait_until(&mut again, Duration::from_secs(10), |p| {
            has_line(p, "ECHO:back")
        }));

        // 全部断开、测试自己也放手：没人用了，连接才真的释放。
        b.disconnect_by_user();
        again.disconnect_by_user();
        drop(factory);
        let released = (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(100));
            weak_connection.upgrade().is_none()
        });
        assert!(released, "最后一个使用者走了之后连接应当释放");
    }

    /// 还没连上（正在连接）的窗格也能断开：后台的连接任务随之取消，窗格显示已断开。
    #[test]
    fn a_connecting_pane_can_be_disconnected_too() {
        let factory = local_factory();
        let mut pane = Pane::start_ssh(Some(Arc::clone(&factory)), Some(1), LIVE, 10_000, 1);
        assert_eq!(pane.state, PaneState::Connecting);
        assert!(pane.can_disconnect());
        pane.disconnect_by_user();
        assert_eq!(pane.state, PaneState::Disconnected);
        assert!(pane.pending.is_none(), "后台任务一并取消");
        // 之后不会有迟到的结果把它又变回已连接。
        std::thread::sleep(Duration::from_millis(500));
        pane.poll_events();
        assert_eq!(pane.state, PaneState::Disconnected);
    }
}

#[cfg(test)]
mod blink_tests {
    use super::{BLINK_HALF_PERIOD, BlinkClock};
    use std::time::Duration;

    #[test]
    fn the_cursor_is_lit_for_half_a_period_then_dark_and_repeats() {
        let half = BLINK_HALF_PERIOD;
        assert!(BlinkClock::phase_at(Duration::ZERO), "刚打字：亮着");
        assert!(BlinkClock::phase_at(half - Duration::from_millis(1)));
        assert!(!BlinkClock::phase_at(half), "半个周期后灭");
        assert!(!BlinkClock::phase_at(half * 2 - Duration::from_millis(1)));
        assert!(BlinkClock::phase_at(half * 2), "一个周期后又亮");
    }

    #[test]
    fn a_fresh_clock_starts_lit_and_reports_no_flip() {
        let mut clock = BlinkClock::new();
        assert!(clock.visible());
        assert!(!clock.changed(), "相位没变就不用重画");
    }
}

#[cfg(test)]
mod protocol_session_tests {
    use super::{Pane, PaneState, Size};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use zssh_core::ssh::flow::FlowGate;
    use zssh_session::{CoreFilter, SessionBackend, SessionCommand, SessionPump};

    /// 记录 Break 次数的写端。
    struct BreakCounter(Arc<AtomicUsize>);

    impl SessionBackend for BreakCounter {
        fn send(&mut self, _command: SessionCommand) {}

        fn send_break(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// 记录送到写端的输入次数。
    struct WriteCounter(Arc<AtomicUsize>);

    impl SessionBackend for WriteCounter {
        fn send(&mut self, command: SessionCommand) {
            if matches!(command, SessionCommand::Write(_)) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    fn pane_counting_writes(counter: Arc<AtomicUsize>) -> Pane {
        let pump = SessionPump::new(
            Size { rows: 5, cols: 20 },
            100,
            Arc::new(FlowGate::new()),
            CoreFilter::new(),
            1,
        );
        let (_, events) = std::sync::mpsc::channel();
        Pane::with_parts(pump, Box::new(WriteCounter(counter)), events, Some(1), None)
    }

    /// 锁屏（V2-U26）：窗格是用户输入进会话的唯一出口，锁着时键入、粘贴、快捷命令、广播全被丢掉，
    /// 解锁后恢复；锁定本身不动会话（状态仍是 Live）。
    #[test]
    fn user_input_is_dropped_while_the_app_is_locked_and_flows_again_after() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut pane = pane_counting_writes(Arc::clone(&counter));
        pane.write(b"ls\r");
        assert_eq!(counter.load(Ordering::SeqCst), 1, "没锁时正常送出");

        crate::app_lock::set_locked(true);
        pane.write(b"rm -rf x\r");
        pane.write(&zssh_session::encode_paste("pasted text", true));
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "锁着时键入和粘贴都不送出"
        );
        assert_eq!(pane.state, PaneState::Live, "锁定不会断开会话");

        crate::app_lock::set_locked(false);
        pane.write(b"pwd\r");
        assert_eq!(counter.load(Ordering::SeqCst), 2, "解锁后恢复");
    }

    /// 广播输入（和快捷命令 / 撰写栏发给“整个标签”）走的是同一个出口：锁着时每个窗格都收不到。
    #[test]
    fn a_broadcast_reaches_no_pane_while_the_app_is_locked() {
        let counters: Vec<Arc<AtomicUsize>> =
            (0..3).map(|_| Arc::new(AtomicUsize::new(0))).collect();
        let mut panes: Vec<Pane> = counters
            .iter()
            .map(|counter| pane_counting_writes(Arc::clone(counter)))
            .collect();
        let total = |counters: &[Arc<AtomicUsize>]| -> usize {
            counters.iter().map(|c| c.load(Ordering::SeqCst)).sum()
        };
        for pane in &mut panes {
            pane.write(b"echo hi\r");
        }
        assert_eq!(total(&counters), 3);

        crate::app_lock::set_locked(true);
        for pane in &mut panes {
            pane.write(b"echo secret\r");
        }
        assert_eq!(total(&counters), 3, "锁着时三个窗格一个都没收到");
        crate::app_lock::set_locked(false);
    }

    fn pane_with(counter: Arc<AtomicUsize>) -> Pane {
        let pump = SessionPump::new(
            Size { rows: 5, cols: 20 },
            100,
            Arc::new(FlowGate::new()),
            CoreFilter::new(),
            1,
        );
        let (_, events) = std::sync::mpsc::channel();
        Pane::with_parts(pump, Box::new(BreakCounter(counter)), events, Some(1), None)
    }

    /// 窗格的 Break 命令只在会话活着时送到写端；断开之后不再发。
    #[test]
    fn send_break_reaches_the_backend_only_while_the_session_is_live() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut pane = pane_with(Arc::clone(&counter));
        pane.send_break();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        pane.state = PaneState::Disconnected;
        pane.send_break();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    /// 终端窗格接上一个没有 SSH 工厂的会话（Telnet）：横幅显示在屏幕上，键入送到服务器，
    /// 对端关闭之后窗格显示已断开。走的就是 SSH 会话同一条 `attach` 路径。
    #[test]
    fn a_telnet_session_attaches_to_a_pane_without_an_ssh_factory() {
        use std::time::{Duration, Instant};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let runtime = crate::ssh_backend::runtime().expect("runtime");
        let (opened, mut server_rx) = runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            tokio::spawn(async move {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let _ = socket.write_all(b"PANE_OK\r\n").await;
                let mut buffer = [0u8; 256];
                loop {
                    match socket.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            let chunk = buffer[..read].to_vec();
                            if chunk.windows(4).any(|w| w == b"quit") {
                                let _ = tx.send(chunk);
                                let _ = socket.shutdown().await;
                                break;
                            }
                            let _ = tx.send(chunk);
                        }
                    }
                }
            });
            let socket = tokio::net::TcpStream::connect(addr).await.unwrap();
            let opened = crate::protocol_backend::start_telnet(
                Box::new(socket),
                Box::new(()),
                zssh_core::model::TelnetOptions::default(),
                String::new(),
                None,
                Size { rows: 5, cols: 40 },
                1,
            );
            (opened, rx)
        });
        let pending = crate::ssh_backend::ready(Ok(opened));
        let mut pane = Pane::connecting(Size { rows: 5, cols: 40 }, 100, 1, Some(1), pending);
        assert_eq!(pane.state, PaneState::Connecting);

        let screen = |pane: &Pane| -> String {
            pane.snapshot
                .rows
                .iter()
                .map(|row| row.text.clone())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !screen(&pane).contains("PANE_OK") {
            pane.poll_events();
            assert!(Instant::now() < deadline, "横幅没有出现：{}", screen(&pane));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(pane.state, PaneState::Live);
        assert!(pane.factory.is_none());
        assert!(screen(&pane).contains("Telnet 不加密"));

        pane.write(b"quit\r");
        let seen = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut all = Vec::new();
                while let Some(chunk) = server_rx.recv().await {
                    all.extend_from_slice(&chunk);
                    if all.windows(4).any(|w| w == b"quit") {
                        break;
                    }
                }
                all
            })
            .await
        });
        assert!(seen.is_ok(), "服务器没有收到键入");

        let deadline = Instant::now() + Duration::from_secs(10);
        while pane.state != PaneState::Disconnected {
            pane.poll_events();
            assert!(Instant::now() < deadline, "对端关闭后窗格应当显示已断开");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// V2-C1：多行粘贴的确认绑定发起时的目标（R01）。纯状态测试：不起窗口，用带记录的假后端。
#[cfg(test)]
mod paste_tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{
        Axis, CoreFilter, FlowGate, PASTE_PREVIEW_CHARS, PASTE_PREVIEW_LINES, Pane, PaneState,
        SessionBackend, SessionCommand, SessionPump, Size, Tab, paste_preview, paste_target_line,
    };
    use std::sync::{Arc, Mutex};

    const SIZE: Size = Size { rows: 8, cols: 40 };

    type Sent = Arc<Mutex<Vec<SessionCommand>>>;

    /// 记录收到的写入（用户输入 / 粘贴）。
    struct Recorder(Sent);

    impl SessionBackend for Recorder {
        fn send(&mut self, command: SessionCommand) {
            self.0.lock().unwrap().push(command);
        }
    }

    fn live_pane() -> (Pane, Sent) {
        let (_tx, rx) = std::sync::mpsc::channel();
        let sent: Sent = Arc::new(Mutex::new(Vec::new()));
        let generation = super::next_generation();
        let pump = SessionPump::new(
            SIZE,
            50,
            Arc::new(FlowGate::new()),
            CoreFilter::new(),
            generation,
        );
        let pane = Pane::with_parts(
            pump,
            Box::new(Recorder(Arc::clone(&sent))),
            rx,
            Some(1),
            None,
        );
        (pane, sent)
    }

    /// n 个已连接的窗格组成一个标签，聚焦第一个。
    fn tab_of(n: usize) -> (Tab, Vec<Sent>) {
        let (first, sent) = live_pane();
        let mut logs = vec![sent];
        let mut tab = Tab::new("t".to_owned(), first);
        for _ in 1..n {
            let (pane, sent) = live_pane();
            tab.split_active(Axis::X, pane);
            logs.push(sent);
        }
        // `split_active` 把焦点给了新窗格；回到第一个。
        tab.active_pane = 0;
        (tab, logs)
    }

    fn writes(log: &Sent) -> Vec<Vec<u8>> {
        log.lock()
            .unwrap()
            .iter()
            .filter_map(|command| match command {
                SessionCommand::Write(bytes) => Some(bytes.clone()),
                _ => None,
            })
            .collect()
    }

    /// 像界面的 `confirm_paste` 那样：核对，通过才逐个窗格写出。
    fn confirm(tab: &mut Tab, pending: &super::PendingPaste) -> Option<Vec<usize>> {
        let targets = tab.resolve_paste(pending)?;
        for &index in &targets {
            let pane = &mut tab.panes[index];
            pane.write(&zssh_session::encode_paste(&pending.text, false));
        }
        Some(targets)
    }

    #[test]
    fn confirming_sends_to_the_pane_the_paste_started_in_even_after_focus_moves() {
        let (mut tab, logs) = tab_of(2);
        let pending = tab.capture_paste("a\nb".to_owned()).unwrap();
        assert_eq!(pending.recipients(), 1);
        assert_eq!(
            paste_target_line(&pending),
            crate::i18n::t(super::TermMsg::PasteTargetCurrent)
        );

        // 确认条出来之后点到了另一个分屏。
        tab.active_pane = 1;
        assert_eq!(confirm(&mut tab, &pending), Some(vec![0]));
        let expected = zssh_session::encode_paste("a\nb", false);
        assert_eq!(writes(&logs[0]), vec![expected]);
        assert!(writes(&logs[1]).is_empty(), "不能改发到现在聚焦的窗格");
    }

    #[test]
    fn toggling_broadcast_either_way_cancels() {
        // 单窗格发起，之后开了广播：接收者会变多。
        let (mut tab, logs) = tab_of(3);
        let pending = tab.capture_paste("a\nb".to_owned()).unwrap();
        tab.broadcast = true;
        assert_eq!(confirm(&mut tab, &pending), None);
        assert!(logs.iter().all(|log| writes(log).is_empty()));

        // 广播时发起，之后关了广播：接收者会变少。
        let pending = tab.capture_paste("a\nb".to_owned()).unwrap();
        assert!(pending.broadcast);
        tab.broadcast = false;
        assert_eq!(confirm(&mut tab, &pending), None);
        assert!(logs.iter().all(|log| writes(log).is_empty()));
    }

    #[test]
    fn a_broadcast_paste_goes_to_exactly_the_panes_it_listed() {
        let (mut tab, logs) = tab_of(3);
        tab.broadcast = true;
        let pending = tab.capture_paste("x\ny".to_owned()).unwrap();
        assert_eq!(pending.recipients(), 3);
        assert_eq!(paste_target_line(&pending), "会发送到 3 个窗格（广播）");
        assert_eq!(confirm(&mut tab, &pending), Some(vec![0, 1, 2]));
        let expected = zssh_session::encode_paste("x\ny", false);
        for log in &logs {
            assert_eq!(writes(log), vec![expected.clone()]);
        }
    }

    #[test]
    fn closing_or_adding_a_pane_cancels_a_broadcast_paste() {
        let (mut tab, logs) = tab_of(3);
        tab.broadcast = true;
        let pending = tab.capture_paste("x\ny".to_owned()).unwrap();
        tab.close_pane_at(2);
        assert_eq!(confirm(&mut tab, &pending), None);

        let (mut tab, other_logs) = tab_of(2);
        tab.broadcast = true;
        let pending = tab.capture_paste("x\ny".to_owned()).unwrap();
        let (extra, _sent) = live_pane();
        tab.split_active(Axis::Y, extra);
        assert_eq!(confirm(&mut tab, &pending), None, "多了一个窗格也算变了");
        assert!(logs.iter().all(|log| writes(log).is_empty()));
        assert!(other_logs.iter().all(|log| writes(log).is_empty()));
    }

    #[test]
    fn closing_the_target_pane_cancels_but_closing_another_does_not() {
        let (mut tab, logs) = tab_of(3);
        tab.active_pane = 1;
        let pending = tab.capture_paste("a\nb".to_owned()).unwrap();
        // 关掉的不是目标窗格：目标没变，照常发。
        tab.close_pane_at(2);
        assert_eq!(confirm(&mut tab, &pending), Some(vec![1]));
        assert_eq!(writes(&logs[1]).len(), 1);

        // 关掉目标窗格本身：撤销。
        let (mut tab, logs) = tab_of(2);
        let pending = tab.capture_paste("a\nb".to_owned()).unwrap();
        tab.close_pane_at(0);
        assert_eq!(confirm(&mut tab, &pending), None);
        assert!(writes(&logs[1]).is_empty());
    }

    #[test]
    fn reconnecting_or_disconnecting_the_target_cancels() {
        // 重连：整个窗格被换掉（沿用编号，代次是新的）。
        let (mut tab, logs) = tab_of(2);
        let pending = tab.capture_paste("a\nb".to_owned()).unwrap();
        let (mut fresh, _sent) = live_pane();
        fresh.id = tab.panes[0].id;
        tab.panes[0] = fresh;
        assert_eq!(confirm(&mut tab, &pending), None);

        // 断开：状态变了。
        let (mut tab, _logs) = tab_of(2);
        let pending = tab.capture_paste("a\nb".to_owned()).unwrap();
        tab.panes[0].disconnect("断开".to_owned(), "断开");
        assert_eq!(confirm(&mut tab, &pending), None);
        assert!(writes(&logs[0]).is_empty());
    }

    #[test]
    fn a_different_tab_never_receives_the_paste() {
        let (mut tab, _logs) = tab_of(1);
        let (mut other, other_logs) = tab_of(1);
        let pending = tab.capture_paste("a\nb".to_owned()).unwrap();
        assert_eq!(confirm(&mut other, &pending), None);
        assert!(writes(&other_logs[0]).is_empty());
        assert!(confirm(&mut tab, &pending).is_some());
    }

    #[test]
    fn nothing_to_paste_into_means_no_confirmation() {
        let mut tab = Tab::new(
            "t".to_owned(),
            Pane::placeholder(SIZE, 10, 1, Some(1), PaneState::Disconnected),
        );
        assert!(tab.capture_paste("a\nb".to_owned()).is_none());
        // 广播里只有已连接的窗格算接收者，确认框写的数字和实际一致。
        let (live, sent) = live_pane();
        tab.split_active(Axis::X, live);
        tab.broadcast = true;
        let pending = tab.capture_paste("a\nb".to_owned()).unwrap();
        assert_eq!(pending.recipients(), 1);
        assert_eq!(confirm(&mut tab, &pending), Some(vec![1]));
        assert_eq!(writes(&sent).len(), 1);
    }

    #[test]
    fn the_preview_is_bounded_and_strips_control_characters() {
        let (small, cut) = paste_preview("ls\npwd");
        assert_eq!((small.as_str(), cut), ("ls\npwd", false));

        let many: String = (0..500).map(|i| format!("line {i}\n")).collect();
        let (shown, cut) = paste_preview(&many);
        assert!(cut);
        assert_eq!(shown.lines().count(), PASTE_PREVIEW_LINES);

        let wide = "x".repeat(PASTE_PREVIEW_CHARS * 3);
        let (shown, cut) = paste_preview(&wide);
        assert!(cut);
        assert_eq!(shown.chars().count(), PASTE_PREVIEW_CHARS);

        let (shown, _) = paste_preview("a\x1b[2Jb");
        assert!(!shown.contains('\x1b'));
    }
}

/// V2-C1：终端协议回复（DSR / DA …）接回产生查询的那个窗格（R02）。
#[cfg(test)]
mod reply_tests {
    use super::{
        Axis, CoreFilter, FlowGate, Pane, PaneState, SessionBackend, SessionCommand, SessionPump,
        Size, Tab,
    };
    use std::sync::mpsc::Sender;
    use std::sync::{Arc, Mutex};
    use zssh_session::SessionEvent;

    const SIZE: Size = Size { rows: 8, cols: 40 };

    /// 记录写入和协议回复，并像真实写端一样丢掉代次不符的回复。
    #[derive(Default)]
    struct Log {
        writes: Vec<Vec<u8>>,
        replies: Vec<Vec<u8>>,
    }

    struct Recorder {
        log: Arc<Mutex<Log>>,
        generation: u64,
    }

    impl SessionBackend for Recorder {
        fn send(&mut self, command: SessionCommand) {
            if let SessionCommand::Write(bytes) = command {
                self.log.lock().unwrap().writes.push(bytes);
            }
        }

        fn send_reply(&mut self, generation: u64, bytes: Vec<u8>) {
            if generation == self.generation {
                self.log.lock().unwrap().replies.push(bytes);
            }
        }
    }

    struct Fixture {
        pane: Pane,
        events: Sender<SessionEvent>,
        log: Arc<Mutex<Log>>,
        generation: u64,
    }

    fn fixture() -> Fixture {
        let (events, rx) = std::sync::mpsc::channel();
        let log = Arc::new(Mutex::new(Log::default()));
        let generation = super::next_generation();
        let pump = SessionPump::new(
            SIZE,
            50,
            Arc::new(FlowGate::new()),
            CoreFilter::new(),
            generation,
        );
        let backend = Recorder {
            log: Arc::clone(&log),
            generation,
        };
        let pane = Pane::with_parts(pump, Box::new(backend), rx, Some(1), None);
        Fixture {
            pane,
            events,
            log,
            generation,
        }
    }

    impl Fixture {
        fn remote_says(&self, bytes: &[u8]) {
            self.events
                .send(SessionEvent::Data {
                    generation: self.generation,
                    bytes: bytes.to_vec(),
                })
                .unwrap();
        }

        fn replies(&self) -> Vec<Vec<u8>> {
            self.log.lock().unwrap().replies.clone()
        }
    }

    fn dummy() -> Pane {
        Pane::placeholder(SIZE, 10, 1, None, PaneState::Empty)
    }

    #[test]
    fn a_status_and_cursor_query_are_answered_through_the_reply_interface_not_as_input() {
        let mut f = fixture();
        f.remote_says(b"ab\x1b[5n\x1b[6n");
        f.pane.poll_events();
        assert_eq!(f.replies(), vec![b"\x1b[0n\x1b[1;3R".to_vec()]);
        assert!(f.log.lock().unwrap().writes.is_empty(), "不是用户输入");

        f.remote_says(b"\x1b[c");
        f.pane.poll_events();
        assert_eq!(f.replies().last(), Some(&b"\x1b[?6c".to_vec()));
    }

    #[test]
    fn replies_are_sent_by_the_tick_alone_without_any_redraw() {
        // 后台标签的窗格没有人看、不重绘，只有每个节拍的 `poll_events`：照样要回。
        let mut f = fixture();
        f.remote_says(b"\x1b[5n");
        f.pane.poll_events();
        assert_eq!(f.replies().len(), 1);
        // 没有新数据的节拍不会重复回复。
        f.pane.poll_events();
        assert_eq!(f.replies().len(), 1);
    }

    #[test]
    fn broadcast_and_focus_do_not_redirect_a_reply() {
        let mut a = fixture();
        let mut b = fixture();
        let mut tab = Tab::new("t".to_owned(), std::mem::replace(&mut a.pane, dummy()));
        tab.split_active(Axis::X, std::mem::replace(&mut b.pane, dummy()));
        tab.broadcast = true;
        tab.active_pane = 1;
        // 只有第一个窗格的远端发了查询。
        a.remote_says(b"\x1b[5n");
        for pane in &mut tab.panes {
            pane.poll_events();
        }
        assert_eq!(a.replies(), vec![b"\x1b[0n".to_vec()]);
        assert!(
            b.replies().is_empty(),
            "广播 / 焦点都不会把回复送给别的窗格"
        );
    }

    #[test]
    fn a_pane_that_is_not_connected_has_nobody_to_answer() {
        let mut f = fixture();
        f.remote_says(b"\x1b[5n");
        f.pane.state = PaneState::Disconnected;
        f.pane.poll_events();
        assert!(f.replies().is_empty());
        // 断开期间攒的回复作废：重新连上后不会补发。
        f.pane.state = PaneState::Live;
        f.pane.poll_events();
        assert!(f.replies().is_empty());
    }

    #[test]
    fn an_old_connection_cannot_answer_into_a_new_one() {
        let mut f = fixture();
        let old_log = Arc::clone(&f.log);
        f.remote_says(b"\x1b[5n");
        f.pane.poll_events();
        assert_eq!(old_log.lock().unwrap().replies.len(), 1);

        // 窗格换了写端和事件流（新的泵）。旧写端收不到之后的回复。
        let (events, rx) = std::sync::mpsc::channel();
        let new_log = Arc::new(Mutex::new(Log::default()));
        f.pane.attach(
            Box::new(Recorder {
                log: Arc::clone(&new_log),
                generation: f.generation,
            }),
            rx,
            Arc::new(FlowGate::new()),
            None,
        );
        events
            .send(SessionEvent::Data {
                generation: f.generation,
                bytes: b"\x1b[6n".to_vec(),
            })
            .unwrap();
        f.pane.poll_events();
        assert_eq!(new_log.lock().unwrap().replies, vec![b"\x1b[1;1R".to_vec()]);
        assert_eq!(old_log.lock().unwrap().replies.len(), 1, "旧连接没有多收");
    }

    #[test]
    fn a_reply_tagged_with_another_generation_is_dropped_before_the_backend() {
        let mut f = fixture();
        // 泵换了代次但窗格没有（不该发生）：回复带着泵的代次，窗格核对不上就丢。
        f.pane.pump_mut().set_generation(f.generation + 100);
        f.events
            .send(SessionEvent::Data {
                generation: f.generation + 100,
                bytes: b"\x1b[5n".to_vec(),
            })
            .unwrap();
        f.pane.poll_events();
        assert!(f.replies().is_empty());
    }

    #[test]
    fn an_offline_loopback_pane_does_not_echo_replies_onto_the_screen() {
        let mut pane = Pane::new(SIZE, 50, super::next_generation());
        pane.write(b"\x1b[5n\x1b[6n");
        let text: String = pane
            .snapshot
            .rows
            .iter()
            .map(|row| row.text.clone())
            .collect();
        assert!(!text.contains("0n") && !text.contains('R'), "{text:?}");
    }

    #[test]
    fn osc52_and_color_queries_get_no_reply_through_the_pane() {
        let mut f = fixture();
        f.remote_says(b"\x1b]52;c;?\x07\x1b]52;c;aGk=\x07\x1b]11;?\x07\x1b[14t");
        f.pane.poll_events();
        assert!(f.replies().is_empty());
        assert!(f.log.lock().unwrap().writes.is_empty());
    }
}

/// 断线遮罩（V2-U30）：什么时候盖、盖的原因、重连的触发键，以及一遍进程内 SSH 服务器上的
/// "服务端断开 -> 遮罩 -> 重连 -> 重新连上"。点击和滚轮属于窗口级输入，由 `dev_acts_u30` 的脚本验证。
#[cfg(test)]
mod overlay_tests {
    use super::{
        CoreFilter, FlowGate, Pane, PaneState, QuickSpec, SessionBackend, SessionCommand,
        SessionPump, Size, Tab, disconnect_reason, is_reconnect_key,
    };
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use zssh_session::{SessionEvent, SessionExit};

    const SIZE: Size = Size { rows: 12, cols: 60 };

    fn screen(pane: &Pane) -> String {
        pane.snapshot
            .rows
            .iter()
            .map(|row| row.text.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    struct Recorder(Arc<Mutex<Vec<SessionCommand>>>);

    impl SessionBackend for Recorder {
        fn send(&mut self, command: SessionCommand) {
            self.0.lock().unwrap().push(command);
        }
    }

    fn remote_pane() -> (
        Pane,
        std::sync::mpsc::Sender<SessionEvent>,
        Arc<Mutex<Vec<SessionCommand>>>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let pump = SessionPump::new(SIZE, 100, Arc::new(FlowGate::new()), CoreFilter::new(), 1);
        let pane = Pane::with_parts(
            pump,
            Box::new(Recorder(Arc::clone(&sent))),
            rx,
            Some(7),
            None,
        );
        (pane, tx, sent)
    }

    fn mods(control: bool, alt: bool, shift: bool, platform: bool) -> ::gpui::Modifiers {
        ::gpui::Modifiers {
            control,
            alt,
            shift,
            platform,
            function: false,
        }
    }

    #[test]
    fn a_live_connecting_or_offline_pane_has_no_overlay() {
        let (live, _tx, _sent) = remote_pane();
        assert_eq!(live.overlay_reason(), None);
        // 离线演示窗格没有会话，永远不会"断开"。
        let mut offline = Pane::new(SIZE, 100, 1);
        offline.close();
        offline.poll_events();
        assert_eq!(offline.overlay_reason(), None);
        // 连接中：有crate::i18n::t(super::TermMsg::Connecting)，不盖遮罩。
        let pending = crate::ssh_backend::ready(Err("never read".to_owned()));
        let connecting = Pane::connecting(SIZE, 100, 1, Some(7), pending);
        assert_eq!(connecting.state, PaneState::Connecting);
        assert_eq!(connecting.overlay_reason(), None);
    }

    #[test]
    fn a_remote_exit_shows_the_overlay_with_the_reason_and_keeps_one_line_in_the_scrollback() {
        let (mut pane, tx, sent) = remote_pane();
        tx.send(SessionEvent::Data {
            generation: 1,
            bytes: b"before\r\n".to_vec(),
        })
        .unwrap();
        tx.send(SessionEvent::Exit {
            generation: 1,
            code: Some(3),
        })
        .unwrap();
        assert!(pane.poll_events());
        assert_eq!(pane.overlay_reason(), Some("退出码 3"));
        let text = screen(&pane);
        assert!(text.contains("before"), "回滚里断线前的输出还在：{text}");
        assert_eq!(text.matches("连接已断开").count(), 1, "{text}");
        assert!(
            !text.contains("Ctrl+Shift+R") && !text.contains("重连"),
            "操作提示只在遮罩上说：{text}"
        );

        // 遮罩盖着时按键、粘贴都发不出去，也不排队：之后重连也不会补发。
        pane.write(b"ls\r");
        pane.write(b"r");
        assert!(
            !sent
                .lock()
                .unwrap()
                .iter()
                .any(|cmd| matches!(cmd, SessionCommand::Write(_))),
            "已断开的会话不收输入"
        );
    }

    #[test]
    fn the_overlay_reason_follows_the_same_cases_as_the_scrollback_line() {
        let exit = |code, reason: Option<&str>| SessionExit {
            code,
            reason: reason.map(str::to_owned),
        };
        assert_eq!(disconnect_reason(&exit(None, None)), "远端关闭了连接");
        assert_eq!(disconnect_reason(&exit(Some(0), None)), "退出码 0");
        assert_eq!(
            disconnect_reason(&exit(Some(1), Some("写入远端失败：broken pipe"))),
            "写入远端失败：broken pipe"
        );
        // 原因里可能带远端可控的文字：控制字符一律去掉。
        let reason = disconnect_reason(&exit(None, Some("bad\x1b]0;title\x07\r\nreason")));
        assert!(!reason.chars().any(char::is_control), "{reason:?}");
    }

    #[test]
    fn a_failed_connect_shows_the_overlay_and_a_user_disconnect_shows_its_own_reason() {
        let pending = crate::ssh_backend::ready(Err("连接失败：timed out".to_owned()));
        let mut pane = Pane::connecting(SIZE, 100, 1, Some(7), pending);
        assert!(pane.poll_events());
        assert_eq!(pane.overlay_reason(), Some("连接失败：timed out"));
        let text = screen(&pane);
        assert!(text.contains("连接失败：timed out"), "{text}");
        assert!(!text.contains("Ctrl+Shift+R"), "{text}");

        let (mut live, _tx, _sent) = remote_pane();
        live.disconnect_by_user();
        assert_eq!(live.overlay_reason(), Some("已手动断开连接"));
        let text = screen(&live);
        assert!(text.contains("已断开连接（手动）"), "{text}");
        assert!(!text.contains("Ctrl+Shift+R"), "{text}");
    }

    #[test]
    fn only_a_bare_r_triggers_the_reconnect() {
        let none = mods(false, false, false, false);
        assert!(is_reconnect_key("r", &none));
        assert!(is_reconnect_key("R", &none), "大写锁定下键名可能是大写");
        assert!(!is_reconnect_key("r", &mods(true, false, false, false)));
        assert!(!is_reconnect_key("r", &mods(false, true, false, false)));
        assert!(!is_reconnect_key("r", &mods(false, false, true, false)));
        assert!(!is_reconnect_key("r", &mods(false, false, false, true)));
        assert!(!is_reconnect_key("r", &mods(true, false, true, false)));
        assert!(!is_reconnect_key("enter", &none));
        assert!(!is_reconnect_key("rr", &none));
    }

    /// 每个窗格各盖各的：分屏里一边断开，另一边还连着时只有断开的那个有遮罩。
    #[test]
    fn each_split_pane_has_its_own_overlay() {
        let (mut left, tx, _sent) = remote_pane();
        let (right, _tx2, _sent2) = remote_pane();
        tx.send(SessionEvent::Exit {
            generation: 1,
            code: None,
        })
        .unwrap();
        left.poll_events();
        let mut tab = Tab::new("t".to_owned(), left);
        tab.panes.push(right);
        assert!(tab.panes[0].overlay_reason().is_some());
        assert!(tab.panes[1].overlay_reason().is_none());
    }

    fn wait_for(pane: &mut Pane, what: &str, done: impl Fn(&Pane) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !done(pane) {
            assert!(
                Instant::now() < deadline,
                "等不到：{what}；屏幕：\n{}",
                screen(pane)
            );
            pane.poll_events();
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn quick_spec() -> Arc<QuickSpec> {
        Arc::new(QuickSpec {
            target: zssh_core::model::QuickTarget {
                username: "tester".to_owned(),
                host: "127.0.0.1".to_owned(),
                port: 22,
            },
            password: "pw".to_owned(),
            label: "tester@lab".to_owned(),
        })
    }

    /// 进程内 SSH 服务器上走一遍：连上 -> 服务端主动断开 -> 遮罩（原因对）-> 重连开始遮罩立即消失 ->
    /// 重新连上能用；重连换了连接但窗格编号、尺寸不变。再让第二条连接退出，遮罩再次出现；重连失败带新原因。
    #[test]
    fn a_server_side_disconnect_then_a_reconnect_over_the_test_server() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("运行时");
        let _dir_guard = tempfile::Builder::new()
            .prefix("zssh-u30-")
            .tempdir()
            .expect("临时目录");
        let dir = _dir_guard.path().to_path_buf();
        let connect = || {
            let addr = runtime.block_on(crate::test_ssh_server::spawn());
            let connection = runtime.block_on(crate::test_ssh_server::connect_trusting(addr, &dir));
            crate::ssh_backend::SshSessionFactory::from_connection(connection)
        };

        let mut pane = Pane::start_quick(quick_spec(), Some(connect()), SIZE, 100, 1);
        let id = pane.id;
        assert_eq!(pane.state, PaneState::Connecting);
        assert_eq!(pane.overlay_reason(), None, "连接中不盖");
        wait_for(&mut pane, "连上", |p| p.state == PaneState::Live);
        pane.write(b"hello\r");
        wait_for(&mut pane, "回显", |p| screen(p).contains("ECHO:hello"));
        assert_eq!(pane.overlay_reason(), None);

        // 服务端不发退出码直接断开整条 TCP 连接。
        pane.write(b"drop\r");
        wait_for(&mut pane, "断开", |p| p.state == PaneState::Disconnected);
        let reason = pane.overlay_reason().expect("断开后盖遮罩").to_owned();
        assert!(!reason.is_empty(), "要有原因");
        let text = screen(&pane);
        assert!(
            text.contains("ECHO:hello"),
            "断线前的输出留在回滚里：{text}"
        );
        assert_eq!(text.matches("连接已断开").count(), 1, "{text}");
        assert!(!text.contains("Ctrl+Shift+R"), "{text}");
        // 之后的按键既不报错也不排队。
        pane.write(b"echo more\r");

        // 重连（和 R / 点遮罩 / Ctrl+Shift+R 同一个函数）：窗格原地换成新连接，遮罩立刻没了。
        let logging = pane.reconnect_in_place(100, Some(connect()));
        assert_eq!(logging, Some(false));
        assert_eq!(pane.id, id, "分屏树按窗格编号认位置");
        assert_eq!(pane.state, PaneState::Connecting);
        assert_eq!(pane.overlay_reason(), None, "重连一开始遮罩就消失");
        assert!(screen(&pane).contains("正在连接"));
        wait_for(&mut pane, "重新连上", |p| p.state == PaneState::Live);
        assert_eq!(pane.overlay_reason(), None);
        pane.write(b"again\r");
        wait_for(&mut pane, "新连接回显", |p| {
            screen(p).contains("ECHO:again")
        });
        assert!(
            !screen(&pane).contains("more"),
            "遮罩期间的按键不会补发到新连接"
        );

        // 再断一次（这回服务端发退出码）：遮罩再出现。
        pane.write(b"bye\r");
        wait_for(&mut pane, "再次断开", |p| {
            p.state == PaneState::Disconnected
        });
        assert_eq!(pane.overlay_reason(), Some("退出码 3"));

        // 重连失败（拨号失败）：遮罩再显示，原因是新的。
        let pending = crate::ssh_backend::ready(Err("连接失败：refused".to_owned()));
        let mut failing = Pane::connecting(SIZE, 100, 2, None, pending);
        failing.quick = Some(quick_spec());
        failing.poll_events();
        assert_eq!(failing.overlay_reason(), Some("连接失败：refused"));
    }
}

#[cfg(test)]
mod row_height_tests {
    use super::{snap_row_height, take_whole_lines};

    /// gpui 布局前对显式长度的取整（`round_half_toward_zero`，设备像素）：每行真正占的高度。
    fn gpui_row_height(row: f32, scale: f32) -> f32 {
        let device = row * scale;
        (device - 0.5).ceil() / scale
    }

    /// 布局按 `rows` 行排：区域里放得下这么多行时 `rows * 每行真正占的高度` 不能超过文字区高度。
    fn rows_that_fit(inner_h: f32, cell_h: f32) -> usize {
        (inner_h / cell_h).floor() as usize
    }

    #[test]
    fn fractional_row_height_snaps_to_whole_device_pixels() {
        // 12px × 1.3 = 15.6：100% 缩放下实际一行 16px（现场量到的就是 16.0）。
        assert_eq!(snap_row_height(15.6, 1.0), 16.0);
        // 150% 缩放：15.6 × 1.5 = 23.4 → 23 设备像素。
        assert!((snap_row_height(15.6, 1.5) * 1.5 - 23.0).abs() < 1e-4);
        // 非法缩放按 1.0 处理，行高不会变成 0 / NaN。
        assert_eq!(snap_row_height(15.6, 0.0), 16.0);
        assert_eq!(snap_row_height(15.6, f32::NAN), 16.0);
        assert!(snap_row_height(0.0, 1.0) >= 1.0);
    }

    #[test]
    fn unsnapped_row_height_overflowed_the_area() {
        // 复现现场：810px 的终端区、8px 内边距，行高 15.6 → 按 15.6 排出 50 行，
        // 但每行真占 16px，50 行 = 800px > 794px，最后一行伸出区域。
        let inner = 810.0 - 16.0;
        let rows = rows_that_fit(inner, 15.6);
        assert_eq!(rows, 50);
        assert!(rows as f32 * gpui_row_height(15.6, 1.0) > inner);
        // 取整之后排出 49 行，放得下。
        let snapped = snap_row_height(15.6, 1.0);
        let rows = rows_that_fit(inner, snapped);
        assert_eq!(rows, 49);
        assert!(rows as f32 * gpui_row_height(snapped, 1.0) <= inner);
    }

    #[test]
    fn snapped_rows_always_fit_for_common_scales_and_sizes() {
        for scale in [1.0_f32, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0] {
            for size in (16..=64).map(|n| n as f32 * 0.5) {
                for lh in (0..=12).map(|n| 1.0 + n as f32 * 0.05) {
                    let snapped = snap_row_height(size * lh, scale);
                    // 取整后就是整数设备像素：gpui 再取整也不会变。
                    let device = snapped * scale;
                    assert!(
                        (device - device.round()).abs() < 1e-3,
                        "{size} {lh} {scale}"
                    );
                    assert!(
                        (gpui_row_height(snapped, scale) - snapped).abs() < 1e-3,
                        "{size} {lh} {scale}"
                    );
                    for inner in [300.0_f32, 794.0, 797.5, 1210.0] {
                        let rows = rows_that_fit(inner, snapped);
                        assert!(
                            rows as f32 * gpui_row_height(snapped, scale) <= inner + 1e-3,
                            "{size} {lh} {scale} {inner}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn small_wheel_deltas_accumulate_into_whole_lines() {
        let mut rem = 0.0;
        // 触控板：每个事件 0.3 行。以前每个都 round 成 0，一行也滚不动。
        let total: i32 = (0..10).map(|_| take_whole_lines(&mut rem, 0.3)).sum();
        assert_eq!(total, 3, "10 × 0.3 行 = 3 行，一点不丢");
        assert!(rem.abs() < 1e-3);
        // 整行的滚轮：余数一直是 0。
        let mut rem = 0.0;
        assert_eq!(take_whole_lines(&mut rem, 3.0), 3);
        assert_eq!(take_whole_lines(&mut rem, -2.0), -2);
        assert_eq!(rem, 0.0);
        // 一次跨多行带小数：整行先出，小数留着。
        assert_eq!(take_whole_lines(&mut rem, 2.56), 2);
        assert!((rem - 0.56).abs() < 1e-5);
        assert_eq!(take_whole_lines(&mut rem, 0.5), 1);
    }

    #[test]
    fn wheel_remainder_resets_when_direction_reverses() {
        let mut rem = 0.0;
        assert_eq!(take_whole_lines(&mut rem, 0.8), 0);
        // 反向：不用先抵消前面攒的 0.8，直接从 0 开始攒。
        assert_eq!(take_whole_lines(&mut rem, -0.4), 0);
        assert!((rem + 0.4).abs() < 1e-5);
        assert_eq!(take_whole_lines(&mut rem, -0.7), -1);
        // 非法值不动余数。
        let before = rem;
        assert_eq!(take_whole_lines(&mut rem, f32::NAN), 0);
        assert_eq!(rem, before);
    }
}
