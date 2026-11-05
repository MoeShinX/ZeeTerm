//! 「所有网络连接」对话框（V2-U12）：概览面板「网卡」一节的按钮打开，居中的大对话框，列出这台服务器上
//! `ss` / `netstat` / `/proc/net` 看到的 TCP / UDP 连接。
//!
//! - 采集在 `zssh_core::ssh::connections`（一次 exec，5 秒超时）：点开和点「刷新」各执行一次，**不自动
//!   轮询**；可以打开「每 5 秒自动刷新」开关（默认关）。走当前标签已有的那条 SSH 连接，不新建连接、
//!   不往终端里打字；
//! - 筛选 / 搜索 / 排序都是 core 里的纯函数，这里只画：表格用虚拟列表，两万行也不卡；
//! - 拿不到进程信息时进程列是「—」，表格上方一行淡色提示；服务器不支持（不是 Linux）时对话框里写清原因。
//!
//! V2-U21 起每一行有右键菜单：复制、**断开这条连接**（`ss -K`）、**结束进程**、在进程管理中查看。这两个写操作和
//! 进程管理的结束进程是同一套交互：选中行高亮、置灰 + 原因、锁图标、确认框（版式见
//! [`super::processes_ui::confirm_overlay`]，Enter 不确认、Esc 先收确认框、执行中不能关）、结果提示、1 秒后自动刷新。
//! "结束进程"调用的就是进程管理的 `terminate_process`（同一个函数、同一套受保护规则、同一个"重新采一次核对再 kill"），
//! 这里不另写一份；ZeeTerm 自己正在用的那条 SSH 连接上锁，断开和结束进程都置灰。两个动作都没有接进 AI 工具桥。

use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use ::gpui::{
    App, ClipboardItem, MouseButton, SharedString, UniformListScrollHandle, Window, px,
    uniform_list,
};
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::ssh::{
    ConnFilter, ConnectionsError, ConnectionsReport, DisconnectError, Endpoint, KillError,
    KillSignal, NetConn, NetProto, ProcRisk, ProcRow, ProcessHint, ProcessReport, ProcessesError,
    SortKey, SortSpec, disconnect_check, disconnect_connection, kill_check, kill_verdict,
    menu_processes, same_endpoint, terminate_process, view_indices,
};

use super::MainWindow;
use super::bridge::on_ssh_runtime;
use super::modal::Modal;
use super::processes_ui::{
    AskText, ConfirmHandler, KillNotice, NoticeLevel, VERIFY_DELAY, Warn, ask_text,
    confirm_overlay, failure_notice, notice_line, sent_notice, verdict_notice,
};
use super::widgets::{ButtonLook, segmented, text_button};
use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::icons::Ico;
use crate::typography::{Role, Typo as _};

/// 本文件（「所有网络连接」对话框）的界面文案。TCP / UDP、ss、netstat、/proc/net、PID 这类
/// 技术名两个语言下写法相同；地址、端口、进程名、主机名来自远端，照原样显示。
#[derive(Clone, Copy, Debug)]
enum ConnMsg {
    // ---- 进程列提示 ----
    HintNotRoot,
    HintNoProcessSource,
    // ---- 右键菜单 ----
    CopyLocal,
    CopyRemote,
    CopyRow,
    DisconnectRow,
    KillProcess,
    KillProcessPrefix,
    KillProcessMiddle,
    KillProcessSuffix,
    ViewRestPrefix,
    ViewRestSuffix,
    ViewInManager,
    // ---- 确认框 ----
    SshWarning,
    NotRootWarning,
    DisconnectAskTitle,
    DotSep,
    WhoProcessPrefix,
    DisconnectEffect,
    ConfirmDisconnect,
    ForceEndExtra,
    ForceEnd,
    // ---- 结果提示 ----
    DisconnectedPrefix,
    DisconnectedSuffix,
    NoConnection,
    DisconnectRefreshFailedPrefix,
    KillRefreshFailedPrefix,
    KillRefreshFailedMiddle,
    KillRefreshProcsFailedMiddle,
    WrongServer,
    // ---- 工具栏 / 筛选 ----
    Title,
    AutoRefresh,
    Refreshing,
    Refresh,
    FilterAll,
    FilterListen,
    FilterEstablished,
    SummarySuffix,
    SourcePrefix,
    FetchedPrefix,
    TooManyPrefix,
    TooManySuffix,
    RefreshFailedPrefix,
    // ---- 表头 / 表体 ----
    SortTip,
    HeaderProto,
    HeaderLocal,
    HeaderRemote,
    HeaderState,
    HeaderProcess,
    Retry,
    Loading,
    Empty,
    // ---- 自己的连接 ----
    OwnConnectionPrefix,
    OwnConnectionTip,
}

impl Msg for ConnMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::HintNotRoot => {
                "以非 root 用户登录时看不到其他用户进程的信息，进程列显示为「—」。"
            }
            Self::HintNoProcessSource => {
                "服务器上没有 ss / netstat，只能读 /proc/net，这种来源不含进程信息。"
            }
            Self::CopyLocal => "复制本地地址",
            Self::CopyRemote => "复制远端地址",
            Self::CopyRow => "复制整行",
            Self::DisconnectRow => "断开这条连接",
            Self::KillProcess => "结束进程…",
            Self::KillProcessPrefix => "结束 ",
            Self::KillProcessMiddle => "（PID ",
            Self::KillProcessSuffix => "）",
            Self::ViewRestPrefix => "在进程管理里查看其余 ",
            Self::ViewRestSuffix => " 个…",
            Self::ViewInManager => "在进程管理中查看",
            Self::SshWarning => "这是一条 SSH 连接，断开后对方的终端会掉线。",
            Self::NotRootWarning => "你现在不是 root，可能没有权限断开连接（不会自动使用 sudo）。",
            Self::DisconnectAskTitle => "断开这条连接？",
            Self::DotSep => " · ",
            Self::WhoProcessPrefix => " · 进程 ",
            Self::DisconnectEffect => "服务器会立即关闭这条 TCP 连接，对方会收到连接被重置。",
            Self::ConfirmDisconnect => "断开连接",
            Self::ForceEndExtra => {
                "也可以直接点「强制结束」：发送 SIGKILL，立即终止，来不及保存数据或清理。"
            }
            Self::ForceEnd => "强制结束",
            Self::DisconnectedPrefix => "已断开：",
            Self::DisconnectedSuffix => "。",
            Self::NoConnection => "当前标签没有可用的 SSH 连接（还没连上或已断开）",
            Self::DisconnectRefreshFailedPrefix => "已发出断开请求，但刷新列表失败：",
            Self::KillRefreshFailedPrefix => "已向 PID ",
            Self::KillRefreshFailedMiddle => " 发出结束请求，但刷新列表失败：",
            Self::KillRefreshProcsFailedMiddle => " 发出结束请求，但刷新进程列表失败：",
            Self::WrongServer => {
                "当前连接已经不是打开这个窗口时的服务器，已取消，请重新打开所有网络连接。"
            }
            Self::Title => "所有网络连接",
            Self::AutoRefresh => "每 5 秒自动刷新",
            Self::Refreshing => "刷新中…",
            Self::Refresh => "刷新",
            Self::FilterAll => "全部",
            Self::FilterListen => "监听",
            Self::FilterEstablished => "已建立",
            Self::SummarySuffix => " 条",
            Self::SourcePrefix => " · 来源 ",
            Self::FetchedPrefix => " · 采集于 ",
            Self::TooManyPrefix => "连接太多，只显示前 ",
            Self::TooManySuffix => " 条。",
            Self::RefreshFailedPrefix => "刷新失败：",
            Self::SortTip => "点一下升序，再点降序，第三次恢复",
            Self::HeaderProto => "协议",
            Self::HeaderLocal => "本地地址",
            Self::HeaderRemote => "远端地址",
            Self::HeaderState => "状态",
            Self::HeaderProcess => "进程",
            Self::Retry => "重试",
            Self::Loading => "正在读取网络连接…",
            Self::Empty => "没有符合条件的连接",
            Self::OwnConnectionPrefix => "ZeeTerm 正在使用这条连接：",
            Self::OwnConnectionTip => "ZeeTerm 正在使用这条连接",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::HintNotRoot => {
                "When signed in as a non-root user, other users' process information is not visible; the process column shows “—”."
            }
            Self::HintNoProcessSource => {
                "The server has no ss / netstat, so only /proc/net can be read; that source carries no process information."
            }
            Self::CopyLocal => "Copy Local Address",
            Self::CopyRemote => "Copy Remote Address",
            Self::CopyRow => "Copy Row",
            Self::DisconnectRow => "Disconnect This Connection",
            Self::KillProcess => "End Process…",
            Self::KillProcessPrefix => "End ",
            Self::KillProcessMiddle => " (PID ",
            Self::KillProcessSuffix => ")",
            Self::ViewRestPrefix => "View the other ",
            Self::ViewRestSuffix => " in Process Manager…",
            Self::ViewInManager => "View in Process Manager",
            Self::SshWarning => {
                "This is an SSH connection; disconnecting drops the other side's terminal."
            }
            Self::NotRootWarning => {
                "You are not root and may not have permission to disconnect (sudo is never used automatically)."
            }
            Self::DisconnectAskTitle => "Disconnect this connection?",
            Self::DotSep => " · ",
            Self::WhoProcessPrefix => " · process ",
            Self::DisconnectEffect => {
                "The server closes this TCP connection immediately; the other side sees the connection reset."
            }
            Self::ConfirmDisconnect => "Disconnect",
            Self::ForceEndExtra => {
                "You can also click “Force End”: it sends SIGKILL, terminating immediately with no chance to save data or clean up."
            }
            Self::ForceEnd => "Force End",
            Self::DisconnectedPrefix => "Disconnected: ",
            Self::DisconnectedSuffix => ".",
            Self::NoConnection => {
                "The current tab has no usable SSH connection (not connected yet or already disconnected)"
            }
            Self::DisconnectRefreshFailedPrefix => {
                "Disconnect request sent, but refreshing the list failed: "
            }
            Self::KillRefreshFailedPrefix => "Sent an end request to PID ",
            Self::KillRefreshFailedMiddle => ", but refreshing the list failed: ",
            Self::KillRefreshProcsFailedMiddle => ", but refreshing the process list failed: ",
            Self::WrongServer => {
                "The current connection is no longer the server this window was opened for. Cancelled; reopen All Network Connections."
            }
            Self::Title => "All Network Connections",
            Self::AutoRefresh => "Auto-refresh every 5 seconds",
            Self::Refreshing => "Refreshing…",
            Self::Refresh => "Refresh",
            Self::FilterAll => "All",
            Self::FilterListen => "Listen",
            Self::FilterEstablished => "Established",
            Self::SummarySuffix => " connections",
            Self::SourcePrefix => " · source ",
            Self::FetchedPrefix => " · fetched at ",
            Self::TooManyPrefix => "Too many connections; showing only the first ",
            Self::TooManySuffix => ".",
            Self::RefreshFailedPrefix => "Refresh failed: ",
            Self::SortTip => {
                "Click to sort ascending, click again for descending, a third click restores the default"
            }
            Self::HeaderProto => "Protocol",
            Self::HeaderLocal => "Local Address",
            Self::HeaderRemote => "Remote Address",
            Self::HeaderState => "State",
            Self::HeaderProcess => "Process",
            Self::Retry => "Retry",
            Self::Loading => "Reading network connections…",
            Self::Empty => "No matching connections",
            Self::OwnConnectionPrefix => "ZeeTerm is using this connection: ",
            Self::OwnConnectionTip => "ZeeTerm is using this connection",
        }
    }
}

/// 对话框卡片的宽度。
pub const DIALOG_WIDTH: f32 = 940.0;
/// 自动刷新的间隔。
const AUTO_REFRESH: Duration = Duration::from_secs(5);
const ROW_HEIGHT: f32 = 26.0;
/// 表格各列的宽度（最后一列「进程」吃剩下的）。
const COL_PROTO: f32 = 56.0;
const COL_LOCAL: f32 = 250.0;
/// 远端地址列留够一个完整的 IPv6 地址加端口。
const COL_REMOTE: f32 = 310.0;
const COL_STATE: f32 = 100.0;

// ---- 状态 ----

/// 一行的身份：按协议 + 四元组记，刷新后行换了位置也跟着走（地址按 IP 比较，见 `same_endpoint`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowKey {
    pub proto: NetProto,
    pub local: Endpoint,
    pub remote: Endpoint,
}

impl RowKey {
    pub fn of(row: &NetConn) -> Self {
        Self {
            proto: row.proto,
            local: row.local.clone(),
            remote: row.remote.clone(),
        }
    }

    pub fn matches(&self, row: &NetConn) -> bool {
        row.proto == self.proto
            && same_endpoint(&row.local, &self.local)
            && same_endpoint(&row.remote, &self.remote)
    }
}

/// 确认框要问的事。
pub enum ConnAskKind {
    /// 断开这条连接（`ss -K`）。
    Disconnect(Box<NetConn>),
    /// 结束一个进程：`target` 是进程管理那份进程表里的一行（同一个类型），`signal` 是点确认时才定的。
    Kill {
        target: ProcRow,
        signal: KillSignal,
        risk: ProcRisk,
    },
}

/// 确认框的状态。
pub struct ConnAsk {
    pub kind: ConnAskKind,
    /// 已经点了确认、正在执行：不能再点、不能关。
    pub busy: bool,
}

/// 动作成功之后 1 秒要再刷新一次看结果。
pub enum ConnVerifyKind {
    /// 断开：刷新连接表就行。
    Disconnect,
    /// 结束进程：刷新后核对这个进程还在不在（同进程管理的 `kill_verdict`）。
    Kill { target: ProcRow, signal: KillSignal },
}

pub struct ConnVerify {
    pub kind: ConnVerifyKind,
    pub due: Instant,
    /// 已经发出了确认用的那次刷新。
    pub refreshing: bool,
}

/// 对话框的状态；搜索框是主窗口自己的 `conn_query`。
pub struct ConnectionsDialog {
    /// 打开对话框时那台主机：确认执行前核对连接还是它，免得操作到别的机器上。
    pub host_id: i64,
    pub host_name: String,
    pub report: Option<ConnectionsReport>,
    /// 最近一次采集失败的原因（已有数据时只在表格上方显示一行，数据还留着）。
    pub error: Option<String>,
    pub loading: bool,
    /// 采集完成的时刻（本地时间 HH:MM:SS）。
    pub fetched: Option<String>,
    pub filter: ConnFilter,
    pub sort: SortSpec,
    /// 「每 5 秒自动刷新」，默认关。
    pub auto: bool,
    /// 筛选 / 搜索 / 排序之后要显示的行（下标指向 `report.rows`）。
    pub shown: Vec<usize>,
    /// 现在在等的那次采集的编号：关掉对话框再打开后，迟到的旧结果不能写进新对话框。
    pub generation: u64,
    last_done: Option<Instant>,
    pub scroll: UniformListScrollHandle,
    /// 选中的行。
    pub selected: Option<RowKey>,
    /// 进程管理那份进程表：判断「结束进程」能不能点（受保护的置灰）、结束时拿 `ProcRow`。和连接表一起刷新。
    pub procs: Option<ProcessReport>,
    pub procs_loading: bool,
    pub ask: Option<ConnAsk>,
    pub notice: Option<KillNotice>,
    pub verify: Option<ConnVerify>,
    /// 下个节拍要刷新一次（列表过时了：连接没了、PID 变了）。
    pub want_refresh: bool,
}

impl ConnectionsDialog {
    pub fn new(host_id: i64, host_name: String, generation: u64) -> Self {
        Self {
            host_id,
            host_name,
            report: None,
            error: None,
            loading: false,
            fetched: None,
            filter: ConnFilter::All,
            sort: SortSpec::default(),
            auto: false,
            shown: Vec::new(),
            generation,
            last_done: None,
            scroll: UniformListScrollHandle::new(),
            selected: None,
            procs: None,
            procs_loading: false,
            ask: None,
            notice: None,
            verify: None,
            want_refresh: false,
        }
    }

    fn refilter(&mut self, query: &str) {
        self.shown = match &self.report {
            Some(report) => view_indices(&report.rows, self.filter, query, self.sort),
            None => Vec::new(),
        };
        self.scroll.scroll_to_item(0, ::gpui::ScrollStrategy::Top);
    }

    /// 自动刷新该不该采了：开着开关、没有在采、距离上次完成已满 5 秒。
    pub fn auto_due(&self, now: Instant) -> bool {
        self.auto
            && !self.loading
            && self
                .last_done
                .is_none_or(|done| now.duration_since(done) >= AUTO_REFRESH)
    }

    /// 该不该再采一次：没在采，并且（自动刷新到点、或列表过时了、或到了动作之后的确认时刻）。
    pub fn refresh_due(&self, now: Instant) -> bool {
        if self.loading || self.procs_loading {
            return false;
        }
        let verify = self
            .verify
            .as_ref()
            .is_some_and(|v| !v.refreshing && now >= v.due);
        self.auto_due(now) || self.want_refresh || verify
    }

    /// 确认框开着且已点了确认：对话框不能关。
    pub fn busy(&self) -> bool {
        self.ask.as_ref().is_some_and(|ask| ask.busy)
    }

    /// 选中的那一行（调试断言和单测用）。
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    pub fn selected_row(&self) -> Option<&NetConn> {
        let key = self.selected.as_ref()?;
        self.report
            .as_ref()?
            .rows
            .iter()
            .find(|row| key.matches(row))
    }

    /// 这次刷新要不要也采一次进程表：有行带进程信息（才有东西可结束），或者正在等结束之后的核对。
    pub fn needs_processes(&self) -> bool {
        let waiting_verdict = self
            .verify
            .as_ref()
            .is_some_and(|v| matches!(v.kind, ConnVerifyKind::Kill { .. }));
        waiting_verdict
            || self
                .report
                .as_ref()
                .is_some_and(|r| r.rows.iter().any(|row| !row.processes.is_empty()))
    }
}

/// 「HH:MM:SS」本地时间。
fn clock_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
        + crate::datetime::local_utc_offset();
    let in_day = secs.rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02}",
        in_day / 3600,
        in_day % 3600 / 60,
        in_day % 60
    )
}

/// 进程列为空时表格上方的淡色提示。
pub fn hint_text(hint: ProcessHint) -> &'static str {
    match hint {
        ProcessHint::NotRoot => t(ConnMsg::HintNotRoot),
        ProcessHint::NoProcessSource => t(ConnMsg::HintNoProcessSource),
    }
}

// ---- 右键菜单（纯数据，单测核对可用 / 置灰规则）----

/// 菜单项要做的事。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuAct {
    /// 复制一段文字。
    Copy(String),
    Disconnect,
    /// 结束这个 PID（走进程管理的 `terminate_process`）。
    Kill(u32),
    /// 进程列是空的：一行置灰的"结束进程…"，说明原因。
    KillNone,
    /// 打开进程管理并选中这个 PID。
    ShowProcess(u32),
}

/// 一行菜单。`off` = 置灰的完整原因（悬停说明），`short` = 右侧的简短原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuItem {
    pub act: MenuAct,
    pub label: String,
    pub off: Option<String>,
    pub short: Option<String>,
}

impl MenuItem {
    fn on(act: MenuAct, label: impl Into<String>) -> Self {
        Self {
            act,
            label: label.into(),
            off: None,
            short: None,
        }
    }

    fn off(act: MenuAct, label: impl Into<String>, full: String, short: impl Into<String>) -> Self {
        Self {
            act,
            label: label.into(),
            off: Some(full),
            short: Some(short.into()),
        }
    }
}

/// 一行的右键菜单：`None` = 分隔线。
pub fn row_menu(
    report: &ConnectionsReport,
    procs: Option<&ProcessReport>,
    row: &NetConn,
) -> Vec<Option<MenuItem>> {
    let mut entries = vec![
        Some(MenuItem::on(
            MenuAct::Copy(row.local.display()),
            t(ConnMsg::CopyLocal),
        )),
        Some(MenuItem::on(
            MenuAct::Copy(row.remote.display()),
            t(ConnMsg::CopyRemote),
        )),
        Some(MenuItem::on(
            MenuAct::Copy(row.tsv_line()),
            t(ConnMsg::CopyRow),
        )),
        None,
    ];
    // 断开这条连接。
    entries.push(Some(match disconnect_check(report, row) {
        Ok(()) => MenuItem::on(MenuAct::Disconnect, t(ConnMsg::DisconnectRow)),
        Err(block) => MenuItem::off(
            MenuAct::Disconnect,
            t(ConnMsg::DisconnectRow),
            block.reason().to_owned(),
            block.reason(),
        ),
    }));
    // 结束进程：每个 PID 一项（最多 6 个），一个都没有时一行置灰的说明。
    let (listed, hidden) = menu_processes(row);
    if listed.is_empty() {
        let block = if report.is_own_connection(row) {
            zssh_core::ssh::KillBlock::OwnConnection
        } else {
            zssh_core::ssh::KillBlock::NoProcessInfo
        };
        entries.push(Some(MenuItem::off(
            MenuAct::KillNone,
            t(ConnMsg::KillProcess),
            block.reason(),
            block.short(),
        )));
    }
    for proc in &listed {
        let label = format!(
            "{}{}{}{}{}",
            t(ConnMsg::KillProcessPrefix),
            proc.name,
            t(ConnMsg::KillProcessMiddle),
            proc.pid,
            t(ConnMsg::KillProcessSuffix)
        );
        entries.push(Some(match kill_check(report, row, procs, proc.pid) {
            Ok(()) => MenuItem::on(MenuAct::Kill(proc.pid), label),
            Err(block) => MenuItem::off(
                MenuAct::Kill(proc.pid),
                label,
                block.reason(),
                block.short(),
            ),
        }));
    }
    // 超过 6 个：其余的去进程管理里处理（从第 7 个开始定位）。
    if hidden > 0
        && let Some(first_hidden) = row.unique_processes().get(listed.len())
    {
        entries.push(Some(MenuItem::on(
            MenuAct::ShowProcess(first_hidden.pid),
            format!(
                "{}{hidden}{}",
                t(ConnMsg::ViewRestPrefix),
                t(ConnMsg::ViewRestSuffix)
            ),
        )));
    }
    entries.push(None);
    // 在进程管理中查看：多个 PID 时取第一个。
    entries.push(Some(match row.unique_processes().first() {
        Some(first) => MenuItem::on(MenuAct::ShowProcess(first.pid), t(ConnMsg::ViewInManager)),
        None => {
            let block = zssh_core::ssh::KillBlock::NoProcessInfo;
            MenuItem::off(
                MenuAct::ShowProcess(0),
                t(ConnMsg::ViewInManager),
                block.reason(),
                block.short(),
            )
        }
    }));
    entries
}

// ---- 确认框文案与结果提示 ----

/// 「断开这条连接？」确认框的文字。
pub fn disconnect_ask_text(row: &NetConn, uid: Option<u32>) -> AskText {
    let mut warnings = Vec::new();
    if row
        .processes
        .iter()
        .any(|p| p.name.starts_with("sshd") || p.name == "dropbear")
    {
        warnings.push((Warn::Caution, t(ConnMsg::SshWarning).to_owned()));
    }
    if uid.is_some_and(|uid| uid != 0) {
        warnings.push((Warn::Caution, t(ConnMsg::NotRootWarning).to_owned()));
    }
    AskText {
        title: t(ConnMsg::DisconnectAskTitle),
        who: format!(
            "{}{}{}{}{}",
            row.proto_label(),
            t(ConnMsg::DotSep),
            row.state.label(),
            t(ConnMsg::WhoProcessPrefix),
            row.process_label()
        ),
        command: format!("{}  ↔  {}", row.local.display(), row.remote.display()),
        effect: t(ConnMsg::DisconnectEffect).to_owned(),
        warnings,
        confirm_label: t(ConnMsg::ConfirmDisconnect),
    }
}

/// 「结束进程」确认框在 U18 的文字之外多一句：这里的确认框有「结束进程 / 强制结束」两个按钮。
pub fn kill_ask_text(
    target: &ProcRow,
    risk: ProcRisk,
    login_user: Option<&str>,
    uid: Option<u32>,
) -> AskText {
    let mut text = ask_text(target, KillSignal::Term, risk, login_user, uid);
    text.warnings
        .push((Warn::Note, t(ConnMsg::ForceEndExtra).to_owned()));
    text
}

/// 断开成功。
pub fn disconnected_notice(row: &NetConn) -> KillNotice {
    KillNotice {
        level: NoticeLevel::Success,
        text: format!(
            "{}{} ↔ {}{}",
            t(ConnMsg::DisconnectedPrefix),
            row.local.display(),
            row.remote.display(),
            t(ConnMsg::DisconnectedSuffix)
        ),
    }
}

/// 断开没成功：提示和要不要刷新列表（连接已经不在 / 没找到，列表过时了）。
pub fn disconnect_failure(err: &DisconnectError) -> (KillNotice, bool) {
    let (level, refresh) = match err {
        DisconnectError::Gone | DisconnectError::NoMatch => (NoticeLevel::Warn, true),
        _ => (NoticeLevel::Error, false),
    };
    (
        KillNotice {
            level,
            text: err.to_string(),
        },
        refresh,
    )
}

impl MainWindow {
    /// 概览面板「所有连接」：开对话框并立刻采一次。
    pub(crate) fn open_connections_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host_id) = self.status_host else {
            return;
        };
        let host_name = self
            .store
            .host(host_id)
            .map(|host| host.name.clone())
            .unwrap_or_default();
        self.conn_generation += 1;
        let generation = self.conn_generation;
        let query = self.conn_query.clone();
        query.update(cx, |input, cx| input.set_value("", window, cx));
        window.focus(&query.read(cx).focus_handle(cx), cx);
        self.modal = Some(Modal::Connections(Box::new(ConnectionsDialog::new(
            host_id, host_name, generation,
        ))));
        self.refresh_connections(window, cx);
        cx.notify();
    }

    /// 采一次（点开、点「刷新」、自动刷新到点、动作之后的确认）。上一次还没回来就不再发。
    pub(crate) fn refresh_connections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::Connections(dialog)) = &mut self.modal else {
            return;
        };
        if dialog.loading {
            return;
        }
        dialog.loading = true;
        dialog.want_refresh = false;
        if let Some(verify) = &mut dialog.verify
            && Instant::now() >= verify.due
        {
            verify.refreshing = true;
        }
        let generation = dialog.generation;
        cx.notify();

        // 调试钩子：喂一份假输出（走同一个解析器），没有真实服务器也能截图。
        if let Some(result) = self.dev_fake.connections_result() {
            self.connections_loaded(generation, result, window, cx);
            return;
        }

        let Some((connection, _, _)) = self.workspace.read(cx).active_shell_position() else {
            self.connections_loaded(
                generation,
                Err(ConnectionsError::Exec(t(ConnMsg::NoConnection).to_owned())),
                window,
                cx,
            );
            return;
        };
        let rx =
            on_ssh_runtime(async move { zssh_core::ssh::collect_connections(&connection).await });
        match rx {
            Ok(rx) => self.await_result(rx, window, cx, move |this, result, window, cx| {
                this.connections_loaded(generation, result, window, cx);
            }),
            Err(reason) => {
                self.connections_loaded(
                    generation,
                    Err(ConnectionsError::Exec(reason)),
                    window,
                    cx,
                );
            }
        }
    }

    fn connections_loaded(
        &mut self,
        generation: u64,
        result: Result<ConnectionsReport, ConnectionsError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let query = self.conn_query.read(cx).value().to_string();
        let Some(Modal::Connections(dialog)) = &mut self.modal else {
            return;
        };
        if dialog.generation != generation {
            return;
        }
        dialog.loading = false;
        dialog.last_done = Some(Instant::now());
        let verifying = dialog.verify.as_ref().is_some_and(|v| v.refreshing);
        let mut want_procs = false;
        match result {
            Ok(report) => {
                // 断开之后的确认刷新回来了：结果提示留着，不用再等什么。
                if verifying
                    && dialog
                        .verify
                        .as_ref()
                        .is_some_and(|v| matches!(v.kind, ConnVerifyKind::Disconnect))
                {
                    dialog.verify = None;
                }
                // 选中的连接已经不在了：取消选中。
                if dialog
                    .selected
                    .as_ref()
                    .is_some_and(|key| !report.rows.iter().any(|row| key.matches(row)))
                {
                    dialog.selected = None;
                }
                dialog.report = Some(report);
                dialog.error = None;
                dialog.fetched = Some(clock_now());
                dialog.refilter(&query);
                want_procs = dialog.needs_processes();
            }
            Err(err) => {
                if verifying && let Some(verify) = dialog.verify.take() {
                    dialog.notice = Some(KillNotice {
                        level: NoticeLevel::Warn,
                        text: match verify.kind {
                            ConnVerifyKind::Disconnect => {
                                format!("{}{err}", t(ConnMsg::DisconnectRefreshFailedPrefix))
                            }
                            ConnVerifyKind::Kill { target, .. } => format!(
                                "{}{}{}{err}",
                                t(ConnMsg::KillRefreshFailedPrefix),
                                target.pid,
                                t(ConnMsg::KillRefreshFailedMiddle)
                            ),
                        },
                    });
                }
                dialog.error = Some(err.to_string());
            }
        }
        cx.notify();
        if want_procs {
            self.refresh_conn_processes(window, cx);
        }
    }

    /// 连接表采完之后再采一次进程表（进程管理的同一个 `collect_processes`）：菜单里的「结束进程」按它判断受不受保护。
    fn refresh_conn_processes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::Connections(dialog)) = &mut self.modal else {
            return;
        };
        if dialog.procs_loading {
            return;
        }
        dialog.procs_loading = true;
        let generation = dialog.generation;
        cx.notify();

        if let Some(result) = self.dev_fake.processes_result() {
            self.conn_procs_loaded(generation, result, cx);
            return;
        }
        let Some((connection, _, _)) = self.workspace.read(cx).active_shell_position() else {
            self.conn_procs_loaded(
                generation,
                Err(ProcessesError::Exec(t(ConnMsg::NoConnection).to_owned())),
                cx,
            );
            return;
        };
        let rx =
            on_ssh_runtime(async move { zssh_core::ssh::collect_processes(&connection).await });
        match rx {
            Ok(rx) => self.await_result(rx, window, cx, move |this, result, _window, cx| {
                this.conn_procs_loaded(generation, result, cx);
            }),
            Err(reason) => {
                self.conn_procs_loaded(generation, Err(ProcessesError::Exec(reason)), cx)
            }
        }
    }

    fn conn_procs_loaded(
        &mut self,
        generation: u64,
        result: Result<ProcessReport, ProcessesError>,
        cx: &mut Context<Self>,
    ) {
        let Some(Modal::Connections(dialog)) = &mut self.modal else {
            return;
        };
        if dialog.generation != generation {
            return;
        }
        dialog.procs_loading = false;
        let verifying_kill = dialog
            .verify
            .as_ref()
            .is_some_and(|v| v.refreshing && matches!(v.kind, ConnVerifyKind::Kill { .. }));
        match result {
            Ok(report) => {
                if verifying_kill
                    && let Some(ConnVerify {
                        kind: ConnVerifyKind::Kill { target, signal },
                        ..
                    }) = dialog.verify.take()
                {
                    let verdict = kill_verdict(&report, &target);
                    dialog.notice = Some(verdict_notice(
                        verdict,
                        &target,
                        signal,
                        report.row(target.pid),
                    ));
                }
                dialog.procs = Some(report);
            }
            Err(err) => {
                if verifying_kill
                    && let Some(ConnVerify {
                        kind: ConnVerifyKind::Kill { target, .. },
                        ..
                    }) = dialog.verify.take()
                {
                    dialog.notice = Some(KillNotice {
                        level: NoticeLevel::Warn,
                        text: format!(
                            "{}{}{}{err}",
                            t(ConnMsg::KillRefreshFailedPrefix),
                            target.pid,
                            t(ConnMsg::KillRefreshProcsFailedMiddle)
                        ),
                    });
                }
                dialog.procs = None;
            }
        }
        cx.notify();
    }

    /// 每个节拍：自动刷新 / 列表过时 / 动作后的确认到点就再采一次。
    pub(super) fn connections_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let due = matches!(&self.modal, Some(Modal::Connections(dialog)) if dialog.refresh_due(Instant::now()));
        if due {
            self.refresh_connections(window, cx);
        }
    }

    /// 搜索框文字变了 / 换了筛选或排序：重算要显示的行。
    pub(super) fn connections_refilter(&mut self, cx: &mut Context<Self>) {
        let query = self.conn_query.read(cx).value().to_string();
        if let Some(Modal::Connections(dialog)) = &mut self.modal {
            dialog.refilter(&query);
            cx.notify();
        }
    }

    fn connections_set_filter(&mut self, filter: ConnFilter, cx: &mut Context<Self>) {
        if let Some(Modal::Connections(dialog)) = &mut self.modal {
            dialog.filter = filter;
        }
        self.connections_refilter(cx);
    }

    fn connections_sort_by(&mut self, key: SortKey, cx: &mut Context<Self>) {
        if let Some(Modal::Connections(dialog)) = &mut self.modal {
            dialog.sort = dialog.sort.toggled(key);
        }
        self.connections_refilter(cx);
    }

    fn connections_set_auto(&mut self, on: bool, cx: &mut Context<Self>) {
        if let Some(Modal::Connections(dialog)) = &mut self.modal {
            dialog.auto = on;
            // 刚打开开关：从现在起算 5 秒，不立刻再采一次。
            dialog.last_done = Some(Instant::now());
        }
        cx.notify();
    }

    pub(crate) fn connections_select(&mut self, key: RowKey, cx: &mut Context<Self>) {
        if let Some(Modal::Connections(dialog)) = &mut self.modal
            && !dialog.busy()
        {
            dialog.selected = Some(key);
            cx.notify();
        }
    }

    /// 弹「断开这条连接？」确认框（还不会断开任何东西）。不能断开的行（自己的连接、监听、UDP……）不弹框，只给一行提示。
    pub(crate) fn connections_ask_disconnect(&mut self, key: &RowKey, cx: &mut Context<Self>) {
        let Some(Modal::Connections(dialog)) = &mut self.modal else {
            return;
        };
        if dialog.busy() {
            return;
        }
        let Some(report) = &dialog.report else {
            return;
        };
        let Some(row) = report.rows.iter().find(|row| key.matches(row)) else {
            return;
        };
        dialog.selected = Some(key.clone());
        if let Err(block) = disconnect_check(report, row) {
            dialog.notice = Some(KillNotice {
                level: NoticeLevel::Error,
                text: DisconnectError::Blocked(block).to_string(),
            });
            cx.notify();
            return;
        }
        dialog.ask = Some(ConnAsk {
            kind: ConnAskKind::Disconnect(Box::new(row.clone())),
            busy: false,
        });
        cx.notify();
    }

    /// 弹「结束进程」确认框：判定用 core 的 `kill_check`（自己的连接、受保护进程都是它判的，和进程管理同一套规则），
    /// 过了才取进程管理那份 `ProcRow`。
    pub(crate) fn connections_ask_kill(&mut self, key: &RowKey, pid: u32, cx: &mut Context<Self>) {
        let Some(Modal::Connections(dialog)) = &mut self.modal else {
            return;
        };
        if dialog.busy() {
            return;
        }
        let Some(report) = &dialog.report else {
            return;
        };
        let Some(row) = report.rows.iter().find(|row| key.matches(row)) else {
            return;
        };
        let verdict = kill_check(report, row, dialog.procs.as_ref(), pid);
        dialog.selected = Some(key.clone());
        if let Err(block) = verdict {
            dialog.notice = Some(KillNotice {
                level: NoticeLevel::Error,
                text: block.reason(),
            });
            cx.notify();
            return;
        }
        let Some(procs) = &dialog.procs else {
            return;
        };
        let Some(target) = procs.row(pid) else {
            return;
        };
        let (target, risk) = (target.clone(), procs.risk_of(pid));
        dialog.ask = Some(ConnAsk {
            kind: ConnAskKind::Kill {
                target,
                signal: KillSignal::Term,
                risk,
            },
            busy: false,
        });
        cx.notify();
    }

    pub(crate) fn connections_cancel_ask(&mut self, cx: &mut Context<Self>) {
        if let Some(Modal::Connections(dialog)) = &mut self.modal
            && !dialog.busy()
        {
            dialog.ask = None;
            cx.notify();
        }
    }

    /// 用户在确认框里点了确认：执行。`force` 只对「结束进程」有意义（点的是「强制结束」）。
    pub(crate) fn connections_confirm(
        &mut self,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let status_host = self.status_host;
        let Some(Modal::Connections(dialog)) = &mut self.modal else {
            return;
        };
        let Some(ask) = &mut dialog.ask else {
            return;
        };
        if ask.busy {
            return;
        }
        let generation = dialog.generation;
        // 连接已经不是打开对话框时那台主机（切了标签、重连到别处）：不执行，免得操作到别的机器上。
        if status_host != Some(dialog.host_id) {
            dialog.ask = None;
            dialog.notice = Some(KillNotice {
                level: NoticeLevel::Error,
                text: t(ConnMsg::WrongServer).to_owned(),
            });
            cx.notify();
            return;
        }
        if let ConnAskKind::Kill { signal, .. } = &mut ask.kind {
            *signal = if force {
                KillSignal::Kill
            } else {
                KillSignal::Term
            };
        }
        ask.busy = true;
        let kind = match &ask.kind {
            ConnAskKind::Disconnect(row) => Ok((**row).clone()),
            ConnAskKind::Kill { target, signal, .. } => Err((target.clone(), *signal)),
        };
        cx.notify();
        let connection = self
            .workspace
            .read(cx)
            .active_shell_position()
            .map(|(connection, _, _)| connection);
        let no_connection = || t(ConnMsg::NoConnection).to_owned();

        match kind {
            Ok(row) => {
                // 调试钩子：假的断开结果（没有真实服务器也能截图）。
                if let Some(result) = self.dev_fake.disconnect_result(&row) {
                    self.connections_disconnect_done(generation, row, result, cx);
                    return;
                }
                let Some(connection) = connection else {
                    self.connections_disconnect_done(
                        generation,
                        row,
                        Err(DisconnectError::Failed(no_connection())),
                        cx,
                    );
                    return;
                };
                let for_task = row.clone();
                let rx =
                    on_ssh_runtime(
                        async move { disconnect_connection(&connection, &for_task).await },
                    );
                match rx {
                    Ok(rx) => {
                        self.await_result(rx, window, cx, move |this, result, _window, cx| {
                            this.connections_disconnect_done(generation, row, result, cx);
                        })
                    }
                    Err(reason) => self.connections_disconnect_done(
                        generation,
                        row,
                        Err(DisconnectError::Failed(reason)),
                        cx,
                    ),
                }
            }
            Err((target, signal)) => {
                if let Some(result) = self.dev_fake.kill_result(target.pid) {
                    self.connections_kill_done(generation, target, signal, result, cx);
                    return;
                }
                let Some(connection) = connection else {
                    self.connections_kill_done(
                        generation,
                        target,
                        signal,
                        Err(KillError::Failed(no_connection())),
                        cx,
                    );
                    return;
                };
                // 结束进程：进程管理的同一个函数（重新采一次核对同一个进程、受保护的再拦一次、不用 sudo）。
                let for_task = target.clone();
                let rx = on_ssh_runtime(async move {
                    terminate_process(&connection, &for_task, signal).await
                });
                match rx {
                    Ok(rx) => {
                        self.await_result(rx, window, cx, move |this, result, _window, cx| {
                            this.connections_kill_done(generation, target, signal, result, cx);
                        })
                    }
                    Err(reason) => self.connections_kill_done(
                        generation,
                        target,
                        signal,
                        Err(KillError::Failed(reason)),
                        cx,
                    ),
                }
            }
        }
    }

    fn connections_disconnect_done(
        &mut self,
        generation: u64,
        row: NetConn,
        result: Result<(), DisconnectError>,
        cx: &mut Context<Self>,
    ) {
        let Some(Modal::Connections(dialog)) = &mut self.modal else {
            return;
        };
        if dialog.generation != generation {
            return;
        }
        dialog.ask = None;
        match result {
            Ok(()) => {
                dialog.notice = Some(disconnected_notice(&row));
                dialog.verify = Some(ConnVerify {
                    kind: ConnVerifyKind::Disconnect,
                    due: Instant::now() + VERIFY_DELAY,
                    refreshing: false,
                });
            }
            Err(err) => {
                let (notice, refresh) = disconnect_failure(&err);
                dialog.notice = Some(notice);
                dialog.want_refresh |= refresh;
            }
        }
        cx.notify();
    }

    fn connections_kill_done(
        &mut self,
        generation: u64,
        target: ProcRow,
        signal: KillSignal,
        result: Result<(), KillError>,
        cx: &mut Context<Self>,
    ) {
        let Some(Modal::Connections(dialog)) = &mut self.modal else {
            return;
        };
        if dialog.generation != generation {
            return;
        }
        dialog.ask = None;
        let login_user = dialog.procs.as_ref().and_then(|r| r.user.clone());
        match result {
            Ok(()) => {
                dialog.notice = Some(sent_notice(&target, signal));
                dialog.verify = Some(ConnVerify {
                    kind: ConnVerifyKind::Kill { target, signal },
                    due: Instant::now() + VERIFY_DELAY,
                    refreshing: false,
                });
            }
            Err(err) => {
                dialog.notice = Some(failure_notice(&err, &target, login_user.as_deref()));
                // PID 变了 / 进程已经没了：列表过时了，刷新一次。
                if matches!(err, KillError::Changed | KillError::NoSuchProcess) {
                    dialog.want_refresh = true;
                }
            }
        }
        cx.notify();
    }

    // ---- 画 ----

    pub(super) fn connections_body(
        &self,
        dialog: &ConnectionsDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        // 对话框高度跟窗口走：卡片外有 24 的外边距、20 的内边距。
        let height = (self.window_height - 48.0 - 40.0).clamp(380.0, 640.0);

        let title = h_flex()
            .gap_2()
            .items_center()
            .child(
                Icon::new(IconName::Network)
                    .size_4()
                    .text_color(tokens.accent),
            )
            .child(
                div()
                    .typo(Role::GroupTitle, tokens)
                    .child(t(ConnMsg::Title)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .typo(Role::Caption, tokens)
                    .child(SharedString::from(format!("· {}", dialog.host_name))),
            )
            .child({
                let close = entity.clone();
                div()
                    .id("conn-close")
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(24.0))
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(tokens.text_muted)
                    .hover({
                        let hover = tokens.hover;
                        move |style| style.bg(hover)
                    })
                    .child(Icon::new(IconName::Close).size_4())
                    .on_click(move |_, window, cx| {
                        close.update(cx, |this, cx| this.close_modal(window, cx));
                    })
            });

        let filter_now = dialog.filter;
        let filters = div().w(px(330.0)).flex_none().child(segmented(
            "conn-filter",
            ConnFilter::ALL
                .iter()
                .map(|f| {
                    let label = match f {
                        ConnFilter::All => t(ConnMsg::FilterAll),
                        ConnFilter::Tcp => "TCP",
                        ConnFilter::Udp => "UDP",
                        ConnFilter::Listen => t(ConnMsg::FilterListen),
                        ConnFilter::Established => t(ConnMsg::FilterEstablished),
                    };
                    (label.to_owned(), *f == filter_now, None)
                })
                .collect(),
            {
                let entity = entity.clone();
                move |index, _window, cx| {
                    if let Some(filter) = ConnFilter::ALL.get(index).copied() {
                        entity.update(cx, |this, cx| this.connections_set_filter(filter, cx));
                    }
                }
            },
        ));
        let toolbar = h_flex()
            .gap_3()
            .items_center()
            .child(filters)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&self.conn_query).prefix(Icon::new(IconName::Search))),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .flex_none()
                    .child(Switch::new("conn-auto").checked(dialog.auto).on_click({
                        let entity = entity.clone();
                        let on = !dialog.auto;
                        move |_, _window, cx| {
                            entity.update(cx, |this, cx| this.connections_set_auto(on, cx));
                        }
                    }))
                    .child(
                        div()
                            .typo(Role::Caption, tokens)
                            .child(t(ConnMsg::AutoRefresh)),
                    ),
            )
            .child({
                let entity = entity.clone();
                text_button(
                    "conn-refresh",
                    if dialog.loading {
                        t(ConnMsg::Refreshing)
                    } else {
                        t(ConnMsg::Refresh)
                    },
                    ButtonLook::Outline,
                    move |_, window, cx| {
                        entity.update(cx, |this, cx| this.refresh_connections(window, cx));
                    },
                )
            });

        // 状态行：条数、来源、采集时间。
        let summary = {
            let total = dialog.report.as_ref().map_or(0, |r| r.rows.len());
            let mut text = format!(
                "{} / {}{}",
                dialog.shown.len(),
                total,
                t(ConnMsg::SummarySuffix)
            );
            if let Some(report) = &dialog.report {
                text.push_str(&format!(
                    "{}{}",
                    t(ConnMsg::SourcePrefix),
                    report.source.label()
                ));
            }
            if let Some(at) = &dialog.fetched {
                text.push_str(&format!("{}{at}", t(ConnMsg::FetchedPrefix)));
            }
            text
        };
        let hint = dialog
            .report
            .as_ref()
            .and_then(ConnectionsReport::process_hint)
            .map(hint_text);
        let info = v_flex()
            .gap_0p5()
            .child(
                h_flex()
                    .gap_3()
                    .justify_between()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .typo(Role::Caption, tokens)
                            .child(SharedString::from(hint.unwrap_or(""))),
                    )
                    .child(
                        div()
                            .id("conn-summary")
                            .flex_none()
                            .typo(Role::Caption, tokens)
                            .child(SharedString::from(summary)),
                    ),
            )
            .children(dialog.report.as_ref().filter(|r| r.truncated).map(|_| {
                div()
                    .typo(Role::Caption, tokens)
                    .text_color(tokens.caution)
                    .child(format!(
                        "{}{}{}",
                        t(ConnMsg::TooManyPrefix),
                        zssh_core::ssh::CONNECTIONS_MAX_ROWS,
                        t(ConnMsg::TooManySuffix)
                    ))
            }))
            .children(
                dialog
                    .error
                    .as_ref()
                    .filter(|_| dialog.report.is_some())
                    .map(|error| {
                        div()
                            .typo(Role::Caption, tokens)
                            .text_color(tokens.danger)
                            .child(SharedString::from(format!(
                                "{}{error}",
                                t(ConnMsg::RefreshFailedPrefix)
                            )))
                    }),
            )
            .children(
                dialog
                    .notice
                    .as_ref()
                    .map(|notice| notice_line(notice, tokens)),
            );

        // 表头：点列名排序。
        let header = {
            let sort = dialog.sort;
            let cell = |id: &'static str, label: &'static str, key: SortKey, width: Option<f32>| {
                let entity = entity.clone();
                let arrow = (sort.key == Some(key)).then_some(if sort.descending {
                    IconName::ArrowDown
                } else {
                    IconName::ArrowUp
                });
                let hover = tokens.hover;
                h_flex()
                    .id(id)
                    .gap_1()
                    .items_center()
                    .h_full()
                    .px_2()
                    .cursor_pointer()
                    .when_some(width, |d, w| d.w(px(w)).flex_none())
                    .when(width.is_none(), |d| d.flex_1().min_w_0())
                    .hover(move |style| style.bg(hover))
                    .tooltip(|window, cx| Tooltip::new(t(ConnMsg::SortTip)).build(window, cx))
                    .child(div().child(label))
                    .children(arrow.map(|icon| Icon::new(icon).size_3()))
                    .on_click(move |_, _window, cx| {
                        entity.update(cx, |this, cx| this.connections_sort_by(key, cx));
                    })
            };
            h_flex()
                .flex_none()
                .h(px(28.0))
                .items_center()
                .border_b_1()
                .border_color(tokens.border)
                .typo(Role::Caption, tokens)
                .font_weight(FontWeight::MEDIUM)
                .text_color(tokens.text_muted)
                .child(cell(
                    "conn-h-proto",
                    t(ConnMsg::HeaderProto),
                    SortKey::Proto,
                    Some(COL_PROTO),
                ))
                .child(cell(
                    "conn-h-local",
                    t(ConnMsg::HeaderLocal),
                    SortKey::Local,
                    Some(COL_LOCAL),
                ))
                .child(cell(
                    "conn-h-remote",
                    t(ConnMsg::HeaderRemote),
                    SortKey::Remote,
                    Some(COL_REMOTE),
                ))
                .child(cell(
                    "conn-h-state",
                    t(ConnMsg::HeaderState),
                    SortKey::State,
                    Some(COL_STATE),
                ))
                .child(cell(
                    "conn-h-proc",
                    t(ConnMsg::HeaderProcess),
                    SortKey::Process,
                    None,
                ))
        };

        // 表体：没数据 / 失败 / 没有符合条件的 / 虚拟列表。
        let placeholder = |text: String, color: ::gpui::Hsla| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p_6()
                .typo(Role::Body, tokens)
                .text_color(color)
                .child(SharedString::from(text))
                .into_any_element()
        };
        let body: AnyElement = match (&dialog.report, &dialog.error) {
            (None, Some(error)) => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_3()
                .p_6()
                .child(
                    div()
                        .typo(Role::Body, tokens)
                        .text_color(tokens.danger)
                        .child(SharedString::from(error.clone())),
                )
                .child({
                    let entity = entity.clone();
                    text_button(
                        "conn-retry",
                        t(ConnMsg::Retry),
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            entity.update(cx, |this, cx| this.refresh_connections(window, cx));
                        },
                    )
                })
                .into_any_element(),
            (None, None) => placeholder(t(ConnMsg::Loading).to_owned(), tokens.text_muted),
            (Some(_), _) if dialog.shown.is_empty() => {
                placeholder(t(ConnMsg::Empty).to_owned(), tokens.text_muted)
            }
            (Some(_), _) => {
                let row_tokens = *tokens;
                uniform_list(
                    "conn-rows",
                    dialog.shown.len(),
                    cx.processor(move |this, range: Range<usize>, _window, cx| {
                        this.connection_rows(range, &row_tokens, cx)
                    }),
                )
                .track_scroll(&dialog.scroll)
                .size_full()
                .into_any_element()
            }
        };
        let table = v_flex()
            .flex_1()
            .min_h_0()
            .rounded_md()
            .border_1()
            .border_color(tokens.border)
            .overflow_hidden()
            .child(header)
            .child(div().relative().flex_1().min_h_0().child(body));

        let confirm = dialog
            .ask
            .as_ref()
            .map(|ask| self.connections_confirm_box(dialog, ask, tokens, cx));

        v_flex()
            .relative()
            .gap_3()
            .h(px(height))
            .child(title)
            .child(toolbar)
            .child(info)
            .child(table)
            .children(confirm)
            .into_any_element()
    }

    /// 确认框：版式、按钮位置、遮罩全部是进程管理那个（[`confirm_overlay`]）。断开只有一个「断开连接」；
    /// 结束进程多一个「强制结束」，点哪个就发哪个信号。
    fn connections_confirm_box(
        &self,
        dialog: &ConnectionsDialog,
        ask: &ConnAsk,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let uid = dialog
            .report
            .as_ref()
            .and_then(|r| r.uid)
            .or_else(|| dialog.procs.as_ref().and_then(|p| p.uid));
        let cancel = entity.clone();
        let on_cancel = move |_window: &mut Window, cx: &mut App| {
            cancel.update(cx, |this, cx| this.connections_cancel_ask(cx));
        };
        let ok = entity.clone();
        match &ask.kind {
            ConnAskKind::Disconnect(row) => confirm_overlay(
                tokens,
                disconnect_ask_text(row, uid),
                true,
                ask.busy,
                on_cancel,
                move |window, cx| {
                    ok.update(cx, |this, cx| this.connections_confirm(false, window, cx));
                },
                None,
            ),
            ConnAskKind::Kill { target, risk, .. } => {
                let procs = dialog.procs.as_ref();
                let text = kill_ask_text(
                    target,
                    *risk,
                    procs.and_then(|r| r.user.as_deref()),
                    procs.and_then(|r| r.uid),
                );
                let force: ConfirmHandler = Rc::new(move |window, cx| {
                    entity.update(cx, |this, cx| this.connections_confirm(true, window, cx));
                });
                confirm_overlay(
                    tokens,
                    text,
                    true,
                    ask.busy,
                    on_cancel,
                    move |window, cx| {
                        ok.update(cx, |this, cx| this.connections_confirm(false, window, cx));
                    },
                    Some((t(ConnMsg::ForceEnd), force)),
                )
            }
        }
    }

    /// 虚拟列表要画的这一段行。
    fn connection_rows(
        &self,
        range: Range<usize>,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(Modal::Connections(dialog)) = &self.modal else {
            return Vec::new();
        };
        let Some(report) = &dialog.report else {
            return Vec::new();
        };
        let entity = cx.entity();
        let (hover, selected_bg) = (tokens.hover, tokens.accent_soft);
        range
            .filter_map(|at| {
                let row: &NetConn = report.rows.get(*dialog.shown.get(at)?)?;
                let key = RowKey::of(row);
                let is_selected = dialog.selected.as_ref() == Some(&key);
                let own = report.is_own_connection(row);
                let menu = row_menu(report, dialog.procs.as_ref(), row);
                let local = row.local.display();
                let remote = row.remote.display();
                let process = row.process_label();
                let state = row.state.label();
                let state_color = match row.state {
                    zssh_core::ssh::ConnState::Established => tokens.dot_connected,
                    zssh_core::ssh::ConnState::Listen => tokens.accent,
                    zssh_core::ssh::ConnState::TimeWait
                    | zssh_core::ssh::ConnState::CloseWait
                    | zssh_core::ssh::ConnState::LastAck
                    | zssh_core::ssh::ConnState::Closing
                    | zssh_core::ssh::ConnState::Close => tokens.text_muted,
                    _ => tokens.text,
                };
                let cell = |id: &'static str, text: String, width: Option<f32>| {
                    let tip = text.clone();
                    div()
                        .id((id, at))
                        .px_2()
                        .truncate()
                        .when_some(width, |d, w| d.w(px(w)).flex_none())
                        .when(width.is_none(), |d| d.flex_1().min_w_0())
                        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                        .child(SharedString::from(text))
                };
                let select = (entity.clone(), key.clone());
                let right_select = (entity.clone(), key.clone());
                let menu_entity = entity.clone();
                let menu_key = key.clone();
                let proc_tip = if own {
                    format!("{}{process}", t(ConnMsg::OwnConnectionPrefix))
                } else {
                    process.clone()
                };
                let no_process = row.processes.is_empty();
                Some(
                    h_flex()
                        .id(("conn-row", at))
                        .w_full()
                        .h(px(ROW_HEIGHT))
                        .items_center()
                        .typo(Role::Caption, tokens)
                        .text_color(tokens.text)
                        .cursor_pointer()
                        .when(is_selected, |d| d.bg(selected_bg))
                        .when(!is_selected, |d| d.hover(move |style| style.bg(hover)))
                        .child(cell(
                            "conn-c-proto",
                            row.proto_label().to_owned(),
                            Some(COL_PROTO),
                        ))
                        .child(cell("conn-c-local", local, Some(COL_LOCAL)))
                        .child(cell("conn-c-remote", remote, Some(COL_REMOTE)))
                        .child(cell("conn-c-state", state, Some(COL_STATE)).text_color(state_color))
                        .child(
                            // 进程列：ZeeTerm 自己的连接前面带小锁，悬停说原因（同进程管理里受保护进程的锁）。
                            h_flex()
                                .id(("conn-c-proc", at))
                                .px_2()
                                .gap_1()
                                .items_center()
                                .flex_1()
                                .min_w_0()
                                .when(no_process, |d| d.text_color(tokens.text_muted))
                                .tooltip(move |window, cx| {
                                    Tooltip::new(proc_tip.clone()).build(window, cx)
                                })
                                .children(own.then(|| {
                                    div()
                                        .id(("conn-lock", at))
                                        .flex_none()
                                        .text_color(tokens.caution)
                                        .tooltip(|window, cx| {
                                            Tooltip::new(t(ConnMsg::OwnConnectionTip))
                                                .build(window, cx)
                                        })
                                        .child(Icon::new(Ico::Lock).size_3())
                                }))
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .child(SharedString::from(process)),
                                ),
                        )
                        .on_click(move |_, _window, cx| {
                            let key = select.1.clone();
                            select
                                .0
                                .update(cx, |this, cx| this.connections_select(key, cx));
                        })
                        // 右键先选中这一行（菜单项按这一行动作，和选中状态无关）。
                        .on_mouse_down(MouseButton::Right, move |_, _window, cx| {
                            let key = right_select.1.clone();
                            right_select
                                .0
                                .update(cx, |this, cx| this.connections_select(key, cx));
                        })
                        .context_menu(move |popup, _window, cx| {
                            let tokens = Tokens::current(cx);
                            build_row_menu(popup, &menu, &menu_entity, &menu_key, &tokens)
                        })
                        .into_any_element(),
                )
            })
            .collect()
    }
}

/// 把 [`row_menu`] 的结果画成弹出菜单：全部走 `menu_kit`，每项有图标，置灰的行右侧写原因、悬停写完整原因。
fn build_row_menu(
    mut popup: PopupMenu,
    entries: &[Option<MenuItem>],
    entity: &Entity<MainWindow>,
    key: &RowKey,
    tokens: &Tokens,
) -> PopupMenu {
    for entry in entries {
        let Some(item) = entry else {
            popup = popup.separator();
            continue;
        };
        let icon = match item.act {
            MenuAct::Copy(_) => Ico::Copy,
            MenuAct::Disconnect => Ico::Unplug,
            MenuAct::Kill(_) | MenuAct::KillNone => Ico::X,
            MenuAct::ShowProcess(_) => Ico::ListChecks,
        };
        let mut line = crate::menu_kit::row(tokens, item.label.clone(), icon)
            .disabled(item.off.is_some())
            .right(item.short.clone())
            .tooltip(item.off.clone());
        if item.act == MenuAct::Disconnect {
            line = line.danger();
        }
        if item.off.is_some() {
            popup = popup.item(line.inert());
            continue;
        }
        let entity = entity.clone();
        let key = key.clone();
        let act = item.act.clone();
        popup = popup.item(line.on_click(move |_, window, cx| match &act {
            MenuAct::Copy(text) => cx.write_to_clipboard(ClipboardItem::new_string(text.clone())),
            MenuAct::Disconnect => {
                entity.update(cx, |this, cx| this.connections_ask_disconnect(&key, cx));
            }
            MenuAct::Kill(pid) => {
                let pid = *pid;
                entity.update(cx, |this, cx| this.connections_ask_kill(&key, pid, cx));
            }
            MenuAct::ShowProcess(pid) => {
                let pid = *pid;
                entity.update(cx, |this, cx| {
                    this.open_processes_dialog_at(Some(pid), window, cx);
                });
            }
            MenuAct::KillNone => {}
        }));
    }
    popup
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{
        ConnAsk, ConnAskKind, ConnVerify, ConnVerifyKind, ConnectionsDialog, MenuAct, MenuItem,
        RowKey, clock_now, disconnect_ask_text, disconnect_failure, disconnected_notice, hint_text,
        kill_ask_text, row_menu,
    };
    use crate::main_window::processes_ui::{NoticeLevel, Warn};
    use std::time::{Duration, Instant};
    use zssh_core::ssh::{
        ConnFilter, DisconnectError, KillSignal, ProcessHint, SortKey, SortSpec, parse_connections,
        parse_processes,
    };

    fn loaded() -> ConnectionsDialog {
        let mut dialog = ConnectionsDialog::new(1, "web-1".into(), 1);
        dialog.report = Some(
            parse_connections(
                "@@ZSSH uid 1000\n@@ZSSH source ss\n\
                 tcp LISTEN 0 1 0.0.0.0:80 0.0.0.0:* users:((\"nginx\",pid=1,fd=3))\n\
                 tcp ESTAB 0 0 192.0.2.1:22 198.51.100.2:5000\n\
                 udp UNCONN 0 0 *:5353 *:*\n",
            )
            .expect("解析"),
        );
        dialog.refilter("");
        dialog
    }

    #[test]
    fn a_fresh_dialog_is_empty_idle_and_not_auto_refreshing() {
        let dialog = ConnectionsDialog::new(7, "web-1".into(), 7);
        assert!(dialog.report.is_none() && dialog.shown.is_empty());
        assert!(!dialog.loading && !dialog.auto && !dialog.busy());
        assert!(dialog.selected.is_none() && dialog.ask.is_none() && dialog.notice.is_none());
        assert_eq!((dialog.host_id, dialog.generation), (7, 7));
        assert!(!dialog.auto_due(Instant::now()), "默认不自动刷新");
        assert!(!dialog.refresh_due(Instant::now()));
    }

    #[test]
    fn refiltering_follows_the_filter_the_search_and_the_sort() {
        let mut dialog = loaded();
        assert_eq!(dialog.shown.len(), 3);
        dialog.filter = ConnFilter::Listen;
        dialog.refilter("");
        assert_eq!(dialog.shown.len(), 2, "TCP LISTEN + 没连对端的 UDP");
        dialog.refilter("nginx");
        assert_eq!(dialog.shown.len(), 1);
        dialog.filter = ConnFilter::All;
        dialog.sort = SortSpec::default().toggled(SortKey::Local);
        dialog.refilter("");
        let ports: Vec<String> = dialog
            .shown
            .iter()
            .map(|i| {
                dialog
                    .report
                    .as_ref()
                    .map(|r| r.rows[*i].local.port.clone())
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(ports, ["22", "80", "5353"]);
    }

    #[test]
    fn auto_refresh_waits_five_seconds_after_the_last_result_and_never_overlaps() {
        let mut dialog = loaded();
        dialog.auto = true;
        let now = Instant::now();
        // 从没完成过：马上可以采。
        assert!(dialog.auto_due(now));
        dialog.last_done = Some(now);
        assert!(!dialog.auto_due(now + Duration::from_secs(4)));
        assert!(dialog.auto_due(now + Duration::from_secs(5)));
        // 上一次还没回来：不再发。
        dialog.loading = true;
        assert!(!dialog.auto_due(now + Duration::from_secs(60)));
    }

    #[test]
    fn a_stale_list_and_the_check_after_an_action_refresh_without_the_auto_switch() {
        let mut dialog = loaded();
        let now = Instant::now();
        assert!(
            !dialog.refresh_due(now + Duration::from_secs(60)),
            "没开自动刷新就不刷"
        );
        dialog.want_refresh = true;
        assert!(dialog.refresh_due(now), "列表过时了：马上刷新");
        dialog.loading = true;
        assert!(!dialog.refresh_due(now), "上一次没回来不再发");
        dialog.loading = false;
        dialog.want_refresh = false;
        dialog.verify = Some(ConnVerify {
            kind: ConnVerifyKind::Disconnect,
            due: now + Duration::from_secs(1),
            refreshing: false,
        });
        assert!(!dialog.refresh_due(now));
        assert!(
            dialog.refresh_due(now + Duration::from_secs(1)),
            "1 秒后确认一次"
        );
        dialog.verify.as_mut().expect("verify").refreshing = true;
        assert!(
            !dialog.refresh_due(now + Duration::from_secs(5)),
            "只确认一次"
        );
    }

    #[test]
    fn the_process_table_is_only_needed_when_something_can_be_ended_or_a_verdict_is_due() {
        let mut dialog = ConnectionsDialog::new(1, "web-1".into(), 1);
        assert!(!dialog.needs_processes(), "还没有数据");
        let mut none = loaded();
        none.report = Some(
            parse_connections(
                "@@ZSSH uid 1000\n@@ZSSH source ss\ntcp ESTAB 0 0 192.0.2.1:22 198.51.100.2:5000\n",
            )
            .expect("解析"),
        );
        assert!(!none.needs_processes(), "一行进程都看不到，没有东西可结束");
        assert!(loaded().needs_processes());
        let target = parse_processes(PS)
            .expect("进程")
            .row(812)
            .cloned()
            .expect("812");
        dialog.verify = Some(ConnVerify {
            kind: ConnVerifyKind::Kill {
                target,
                signal: KillSignal::Term,
            },
            due: Instant::now(),
            refreshing: false,
        });
        assert!(dialog.needs_processes(), "等着核对结束结果");
    }

    #[test]
    fn hints_say_what_is_missing() {
        assert!(hint_text(ProcessHint::NotRoot).contains("非 root"));
        assert!(hint_text(ProcessHint::NoProcessSource).contains("/proc/net"));
    }

    #[test]
    fn the_clock_is_hh_mm_ss() {
        let now = clock_now();
        assert_eq!(now.len(), 8);
        assert_eq!(&now[2..3], ":");
        assert_eq!(&now[5..6], ":");
    }

    // ---- V2-U21：右键菜单 ----

    const SS: &str = "\
@@ZSSH uid 0
@@ZSSH ssh_connection 198.51.100.7 51234 192.0.2.10 22
@@ZSSH source ss
udp   UNCONN 0 0 *:5353 *:* users:((\"mdns-agent\",pid=701,fd=12))
tcp   LISTEN 0 511 0.0.0.0:80 0.0.0.0:* users:((\"web-server\",pid=123,fd=6),(\"web-server\",pid=124,fd=6))
tcp   LISTEN 0 511 0.0.0.0:8000 0.0.0.0:* users:((\"w\",pid=11,fd=6),(\"w\",pid=12,fd=6),(\"w\",pid=13,fd=6),(\"w\",pid=14,fd=6),(\"w\",pid=15,fd=6),(\"w\",pid=16,fd=6),(\"w\",pid=17,fd=6),(\"w\",pid=18,fd=6))
tcp   ESTAB  0 0 192.0.2.10:22 198.51.100.7:51234 users:((\"sshd\",pid=3090,fd=4),(\"sshd\",pid=3100,fd=4))
tcp   ESTAB  0 0 192.0.2.10:22 198.51.100.8:40000 users:((\"sshd\",pid=1200,fd=4))
tcp   ESTAB  0 0 192.0.2.10:443 198.51.100.21:40112 users:((\"web-server\",pid=124,fd=11))
tcp   ESTAB  0 0 192.0.2.10:443 198.51.100.22:40113
tcp   ESTAB  0 0 [2001:db8::10]:443 [2001:db8::77]:40000 users:((\"web-server\",pid=124,fd=13))
tcp   TIME-WAIT 0 0 192.0.2.10:8080 198.51.100.9:33333
tcp   CLOSE-WAIT 1 0 192.0.2.10:8081 198.51.100.30:50000 users:((\"app-worker\",pid=812,fd=14))
tcp   ESTAB  0 0 192.0.2.10:514 198.51.100.40:50514 users:((\"rsyslogd\",pid=777,fd=5))
";

    const PS: &str = "\
@@ZSSH uid 0
@@ZSSH user root
@@ZSSH self 4242
@@ZSSH memtotal 16000000
@@ZSSH source procps
PID PPID USER %CPU %MEM RSS STAT ELAPSED COMMAND
1 0 root 0.0 0.3 48000 Ss 172800 /sbin/init
800 1 root 0.0 0.1 15000 Ss 172700 sshd: /usr/sbin/sshd -D [listener]
3090 800 root 0.0 0.1 11000 Ss 3600 sshd: root [priv]
3100 3090 root 0.0 0.1 9000 S 3600 sshd: root@notty
4242 3100 root 0.4 0.0 3000 Ss 0 sh -c exec
123 1 www-data 3.2 2.5 400000 S 86400 web-server: master process
124 123 www-data 3.2 2.5 400000 S 86400 web-server: worker process
701 1 avahi 0.1 0.1 6000 Ss 86400 mdns-agent: running
812 1 app 62.3 12.4 1984000 Ssl 93784 /usr/bin/app-worker --queue=high
1200 800 root 0.0 0.1 11000 Ss 3600 sshd: user [priv]
777 1 root 0.0 0.2 28000 Ssl 172800 /usr/sbin/rsyslogd -n
11 1 app 0.0 0.0 1000 S 100 w
12 1 app 0.0 0.0 1000 S 100 w
13 1 app 0.0 0.0 1000 S 100 w
14 1 app 0.0 0.0 1000 S 100 w
15 1 app 0.0 0.0 1000 S 100 w
16 1 app 0.0 0.0 1000 S 100 w
17 1 app 0.0 0.0 1000 S 100 w
18 1 app 0.0 0.0 1000 S 100 w
";

    fn menu_for(local: &str, remote: &str, with_procs: bool) -> Vec<Option<MenuItem>> {
        let report = parse_connections(SS).expect("解析");
        let procs = parse_processes(PS).expect("进程");
        let row = report
            .rows
            .iter()
            .find(|r| r.local.display() == local && r.remote.display() == remote)
            .unwrap_or_else(|| panic!("没有 {local} {remote}"));
        row_menu(&report, with_procs.then_some(&procs), row)
    }

    fn find<'a>(menu: &'a [Option<MenuItem>], label_start: &str) -> &'a MenuItem {
        menu.iter()
            .flatten()
            .find(|item| item.label.starts_with(label_start))
            .unwrap_or_else(|| panic!("菜单里没有 {label_start}：{menu:#?}"))
    }

    #[test]
    fn every_row_has_the_copy_items_a_separator_and_the_view_item() {
        let menu = menu_for("192.0.2.10:443", "198.51.100.21:40112", true);
        let labels: Vec<&str> = menu
            .iter()
            .map(|e| e.as_ref().map_or("—", |i| i.label.as_str()))
            .collect();
        assert_eq!(
            labels,
            [
                "复制本地地址",
                "复制远端地址",
                "复制整行",
                "—",
                "断开这条连接",
                "结束 web-server（PID 124）",
                "—",
                "在进程管理中查看",
            ]
        );
        assert_eq!(
            find(&menu, "复制整行").act,
            MenuAct::Copy(
                "TCP\t192.0.2.10:443\t198.51.100.21:40112\tESTABLISHED\tweb-server (124)".into()
            )
        );
        assert_eq!(
            find(&menu, "复制本地地址").act,
            MenuAct::Copy("192.0.2.10:443".into())
        );
        assert_eq!(
            find(&menu, "复制远端地址").act,
            MenuAct::Copy("198.51.100.21:40112".into())
        );
        // 复制永远可用。
        for item in menu.iter().flatten().take(3) {
            assert!(item.off.is_none());
        }
    }

    #[test]
    fn an_established_row_with_a_normal_process_can_do_everything() {
        let menu = menu_for("192.0.2.10:443", "198.51.100.21:40112", true);
        assert_eq!(find(&menu, "断开这条连接").off, None);
        assert_eq!(find(&menu, "断开这条连接").act, MenuAct::Disconnect);
        let kill = find(&menu, "结束 web-server");
        assert_eq!((&kill.act, &kill.off), (&MenuAct::Kill(124), &None));
        assert_eq!(
            find(&menu, "在进程管理中查看").act,
            MenuAct::ShowProcess(124)
        );
    }

    #[test]
    fn listen_udp_and_time_wait_rows_grey_out_disconnect_with_the_cards_reasons() {
        let reason = |local: &str, remote: &str| {
            find(&menu_for(local, remote, true), "断开这条连接")
                .off
                .clone()
        };
        assert_eq!(
            reason("0.0.0.0:80", "0.0.0.0:*").as_deref(),
            Some("监听端口没有对端，要停掉请结束进程")
        );
        assert_eq!(reason("*:5353", "*:*").as_deref(), Some("UDP 没有连接可断"));
        assert_eq!(
            reason("192.0.2.10:8080", "198.51.100.9:33333").as_deref(),
            Some("TIME-WAIT 已经在关闭")
        );
        // 置灰的项右侧也有简短原因。
        let menu = menu_for("0.0.0.0:80", "0.0.0.0:*", true);
        assert_eq!(
            find(&menu, "断开这条连接").short.as_deref(),
            Some("监听端口没有对端，要停掉请结束进程")
        );
        // CLOSE-WAIT 可以断。
        assert_eq!(
            reason("192.0.2.10:8081", "198.51.100.30:50000"),
            None,
            "CLOSE-WAIT 有对端"
        );
    }

    #[test]
    fn a_row_with_several_pids_lists_one_kill_item_per_pid() {
        let menu = menu_for("0.0.0.0:80", "0.0.0.0:*", true);
        let kills: Vec<(&str, &MenuAct)> = menu
            .iter()
            .flatten()
            .filter(|i| i.label.starts_with("结束"))
            .map(|i| (i.label.as_str(), &i.act))
            .collect();
        assert_eq!(
            kills,
            [
                ("结束 web-server（PID 123）", &MenuAct::Kill(123)),
                ("结束 web-server（PID 124）", &MenuAct::Kill(124)),
            ]
        );
        // 在进程管理里查看：取第一个 PID。
        assert_eq!(
            find(&menu, "在进程管理中查看").act,
            MenuAct::ShowProcess(123)
        );
    }

    #[test]
    fn more_than_six_pids_list_six_and_a_view_item_for_the_rest() {
        let menu = menu_for("0.0.0.0:8000", "0.0.0.0:*", true);
        let kills = menu
            .iter()
            .flatten()
            .filter(|i| matches!(i.act, MenuAct::Kill(_)))
            .count();
        assert_eq!(kills, 6);
        let rest = find(&menu, "在进程管理里查看其余");
        assert_eq!(rest.label, "在进程管理里查看其余 2 个…");
        assert_eq!(rest.act, MenuAct::ShowProcess(17), "从第 7 个开始定位");
        assert_eq!(rest.off, None);
    }

    #[test]
    fn an_empty_process_column_greys_out_ending_and_viewing_with_the_reason() {
        let menu = menu_for("192.0.2.10:443", "198.51.100.22:40113", true);
        let kill = find(&menu, "结束进程");
        assert_eq!(kill.act, MenuAct::KillNone);
        assert_eq!(
            kill.off.as_deref(),
            Some("看不到这条连接属于哪个进程（非 root 登录时看不到其他用户的进程）")
        );
        assert!(find(&menu, "在进程管理中查看").off.is_some());
        // 断开不受影响（它只看连接本身）。
        assert_eq!(find(&menu, "断开这条连接").off, None);
    }

    #[test]
    fn protected_processes_are_greyed_out_by_the_process_managers_rule() {
        // rsyslogd 是关键系统服务：受保护，置灰，右侧写原因，不弹框。
        let menu = menu_for("192.0.2.10:514", "198.51.100.40:50514", true);
        let kill = find(&menu, "结束 rsyslogd");
        assert_eq!(
            kill.off.as_deref(),
            Some("受保护的系统进程不能结束：关键系统服务")
        );
        assert_eq!(kill.short.as_deref(), Some("关键系统服务"));
        // 但这条连接本身可以断。
        assert_eq!(find(&menu, "断开这条连接").off, None);
    }

    #[test]
    fn without_the_process_list_ending_is_greyed_out_but_the_rest_still_works() {
        let menu = menu_for("192.0.2.10:443", "198.51.100.21:40112", false);
        let kill = find(&menu, "结束 web-server");
        assert!(kill.off.as_deref().is_some_and(|r| r.contains("进程列表")));
        assert_eq!(find(&menu, "断开这条连接").off, None);
        assert_eq!(find(&menu, "在进程管理中查看").off, None);
    }

    #[test]
    fn zeeterms_own_connection_greys_out_disconnect_and_every_kill_item_but_a_peer_ssh_is_fine() {
        let own = menu_for("192.0.2.10:22", "198.51.100.7:51234", true);
        assert_eq!(
            find(&own, "断开这条连接").off.as_deref(),
            Some("ZeeTerm 正在使用这条连接")
        );
        let kills: Vec<&MenuItem> = own
            .iter()
            .flatten()
            .filter(|i| i.label.starts_with("结束"))
            .collect();
        assert_eq!(kills.len(), 2, "sshd 两个进程都列出来，但都置灰");
        for item in kills {
            assert_eq!(item.off.as_deref(), Some("ZeeTerm 正在使用这条连接"));
        }
        // 复制和查看照常可用。
        assert!(find(&own, "复制整行").off.is_none());
        assert!(find(&own, "在进程管理中查看").off.is_none());
        // 同一主机上别人的 SSH 连接：断开和结束进程（别的用户的 sshd 会话进程）都能点。
        let other = menu_for("192.0.2.10:22", "198.51.100.8:40000", true);
        assert_eq!(find(&other, "断开这条连接").off, None);
        assert_eq!(find(&other, "结束 sshd（PID 1200）").off, None);
    }

    #[test]
    fn netstat_rows_cannot_be_disconnected_for_lack_of_ss() {
        let mut report = parse_connections(SS).expect("解析");
        report.source = zssh_core::ssh::ConnSource::Netstat;
        let row = report
            .rows
            .iter()
            .find(|r| r.local.display() == "192.0.2.10:443" && r.remote.port == "40112")
            .expect("行");
        let menu = row_menu(&report, None, row);
        assert_eq!(
            find(&menu, "断开这条连接").off.as_deref(),
            Some("服务器上没有 ss 命令，无法单独断开连接")
        );
    }

    // ---- 确认框文案和结果提示 ----

    #[test]
    fn the_disconnect_confirmation_names_the_four_tuple_the_process_and_the_effect() {
        let report = parse_connections(SS).expect("解析");
        let row = report
            .rows
            .iter()
            .find(|r| r.local.display() == "192.0.2.10:443" && r.remote.port == "40112")
            .expect("行");
        let text = disconnect_ask_text(row, Some(0));
        assert_eq!(text.title, "断开这条连接？");
        assert_eq!(text.who, "TCP · ESTABLISHED · 进程 web-server (124)");
        assert_eq!(text.command, "192.0.2.10:443  ↔  198.51.100.21:40112");
        assert_eq!(
            text.effect,
            "服务器会立即关闭这条 TCP 连接，对方会收到连接被重置。"
        );
        assert_eq!(text.confirm_label, "断开连接");
        assert!(text.warnings.is_empty(), "root 登录的普通连接没有警示");
        // 非 root：提示可能没权限，并写明不会自动 sudo。
        let nonroot = disconnect_ask_text(row, Some(1000));
        assert_eq!(nonroot.warnings.len(), 1);
        assert_eq!(nonroot.warnings[0].0, Warn::Caution);
        assert!(nonroot.warnings[0].1.contains("不会自动使用 sudo"));
        // 别人的 SSH 连接：提醒对方终端会掉线。
        let ssh = report
            .rows
            .iter()
            .find(|r| r.remote.port == "40000" && r.local.port == "22")
            .expect("ssh 行");
        assert!(
            disconnect_ask_text(ssh, Some(0)).warnings[0]
                .1
                .contains("SSH 连接")
        );
        // IPv6 完整显示。
        let v6 = report
            .rows
            .iter()
            .find(|r| r.local.host == "2001:db8::10")
            .expect("v6");
        assert_eq!(
            disconnect_ask_text(v6, Some(0)).command,
            "[2001:db8::10]:443  ↔  [2001:db8::77]:40000"
        );
    }

    #[test]
    fn the_kill_confirmation_is_the_process_managers_text_plus_one_line_about_force() {
        let procs = parse_processes(PS).expect("进程");
        let target = procs.row(812).cloned().expect("812");
        let text = kill_ask_text(
            &target,
            procs.risk_of(812),
            procs.user.as_deref(),
            procs.uid,
        );
        let base = crate::main_window::processes_ui::ask_text(
            &target,
            KillSignal::Term,
            procs.risk_of(812),
            procs.user.as_deref(),
            procs.uid,
        );
        assert_eq!(text.title, base.title);
        assert_eq!(text.who, base.who);
        assert_eq!(text.command, base.command);
        assert_eq!(text.effect, base.effect);
        assert_eq!(text.confirm_label, base.confirm_label);
        assert_eq!(text.warnings.len(), base.warnings.len() + 1);
        let (warn, line) = text.warnings.last().expect("最后一行");
        assert_eq!(*warn, Warn::Note);
        assert!(line.contains("强制结束") && line.contains("SIGKILL"));
    }

    #[test]
    fn disconnect_results_are_worded_per_the_card_and_stale_ones_refresh_the_list() {
        let report = parse_connections(SS).expect("解析");
        let row = &report.rows[5];
        let ok = disconnected_notice(row);
        assert_eq!(ok.level, NoticeLevel::Success);
        assert!(ok.text.starts_with("已断开："));
        for (err, level, refresh, needle) in [
            (
                DisconnectError::NotSupported,
                NoticeLevel::Error,
                false,
                "这台服务器的内核不支持单独断开连接，可以改为结束占用它的进程",
            ),
            (
                DisconnectError::NoPermission,
                NoticeLevel::Error,
                false,
                "需要 root 才能断开连接",
            ),
            (
                DisconnectError::NoMatch,
                NoticeLevel::Warn,
                true,
                "没有找到这条连接（可能已经关闭）",
            ),
            (
                DisconnectError::Gone,
                NoticeLevel::Warn,
                true,
                "这条连接已经不在了",
            ),
            (
                DisconnectError::NoSs,
                NoticeLevel::Error,
                false,
                "服务器上没有 ss 命令",
            ),
        ] {
            let (notice, want_refresh) = disconnect_failure(&err);
            assert_eq!(notice.level, level, "{err:?}");
            assert_eq!(want_refresh, refresh, "{err:?}");
            assert!(notice.text.contains(needle), "{}", notice.text);
            assert!(!notice.text.contains("sudo"));
        }
    }

    #[test]
    fn the_confirmation_blocks_closing_only_while_the_action_is_running() {
        let mut dialog = loaded();
        let row = dialog.report.as_ref().expect("report").rows[0].clone();
        dialog.ask = Some(ConnAsk {
            kind: ConnAskKind::Disconnect(Box::new(row)),
            busy: false,
        });
        assert!(!dialog.busy(), "只是弹着确认框：Esc 可以收掉");
        dialog.ask.as_mut().expect("ask").busy = true;
        assert!(dialog.busy(), "已经点了确认：不能关");
    }

    #[test]
    fn rows_are_selected_by_their_four_tuple_and_survive_a_reorder() {
        let mut dialog = loaded();
        let report = dialog.report.as_ref().expect("report");
        let key = RowKey::of(&report.rows[1]);
        dialog.selected = Some(key.clone());
        assert_eq!(
            dialog.selected_row().map(|r| r.local.display()),
            Some("192.0.2.1:22".to_owned())
        );
        // 同一条连接换了写法（IPv4-mapped）仍然是它；别的行不是。
        assert!(key.matches(&report.rows[1]));
        assert!(!key.matches(&report.rows[0]));
    }

    /// "结束进程"必须是进程管理的同一套函数：这个文件里调用 `terminate_process`，不自己构造 kill 命令、
    /// 不自己写受保护判定、不用 sudo。
    #[test]
    fn ending_a_process_goes_through_the_process_managers_functions_not_a_copy() {
        let source = include_str!("connections_ui.rs");
        let code = source.split("#[cfg(test)]").next().unwrap_or(source);
        assert!(
            code.contains("terminate_process("),
            "没有调用 terminate_process"
        );
        assert!(
            code.contains("kill_check("),
            "可用性要走 core 的 kill_check"
        );
        assert!(
            code.contains("confirm_overlay("),
            "确认框要复用进程管理的版式"
        );
        assert!(
            code.contains("ask_text("),
            "确认文案要复用进程管理的 ask_text"
        );
        for copy in [
            "kill_command(",
            "classify_kill_failure(",
            "same_process(",
            "validate_pid(",
            "\"kill -",
        ] {
            let code_without_docs: String = code
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                !code_without_docs.contains(copy),
                "不该在这里出现 {copy}（结束进程只能走 terminate_process）"
            );
        }
    }

    /// 写操作不接进 AI 工具桥 / zssh-mcp。
    #[test]
    fn the_disconnect_action_is_not_reachable_from_the_ai_tool_bridge() {
        fn scan(dir: &std::path::Path, hits: &mut Vec<String>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    scan(&path, hits);
                } else if path.extension().is_some_and(|e| e == "rs")
                    && std::fs::read_to_string(&path).is_ok_and(|text| {
                        text.contains("disconnect_connection")
                            || text.contains("disconnect_command")
                    })
                {
                    hits.push(path.display().to_string());
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates");
        let mut hits = Vec::new();
        scan(&root.join("zssh-core/src/ai"), &mut hits);
        scan(&root.join("zssh-mcp/src"), &mut hits);
        assert!(
            hits.is_empty(),
            "AI 工具桥 / zssh-mcp 引用了断开连接：{hits:?}"
        );
    }
}
