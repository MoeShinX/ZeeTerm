//! 服务器状态采集：进程级 hub + 轮询器（主窗口的底部状态栏和服务器概览面板读它），以及一个调试用的状态窗口
//! （`ZSSH_STATUS=1`，只在 debug 构建里读）：选一台已保存的主机，每 2 秒采一次，用四张卡片显示。
//!
//! 采集走 core 的 [`StatsCollector`]（0.2.14 同一份脚本和解析）：一次 exec 拿回 CPU（含
//! 每个核心）、内存、交换、全部分区、网卡、占内存最多的进程、系统和内核信息，占的是独立
//! 的 exec 通道，不碰终端会话，也不进会话流控。采集器按连接各留一份，上一次的计数在里面，
//! CPU 使用率和网速都是两次采样的差值，所以第一次采样没有这两项（界面显示「—」）。
//! 采集在 [`crate::ssh_backend::runtime`] 上跑，界面每 100ms 只读一份快照，不阻塞。
//!
//! 轮询器放在进程级 [`StatusHub`] 里，按主机 id 去重：同一台主机开两个窗口（或者主窗口里
//! 多个窗格看同一台主机）只跑一个轮询器，最后一个订阅者离开才停。hub 只依赖 [`Poller`]
//! 接口，测试用假实现，不起 runtime、不碰网络。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use ::gpui::{App, FocusHandle, Task, Window, WindowBounds, WindowOptions};
use gpui_kit::component::button::Button;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::model::Host;
use zssh_core::ssh::BoxFuture;
use zssh_core::ssh::stats::{ServerStats, StatsCollector};
use zssh_core::store::Database;

use crate::settings::data_dir;
use crate::ssh_backend::{SshSessionFactory, factory_for_host, runtime};

/// 服务器状态窗口的文案（时间格式走 *_in，中文分支用 i18n-allow 标注）。
#[derive(Clone, Copy, Debug)]
enum StatusMsg {
    WindowTitle,
    Stopped,
    ConnectTimeoutPrefix,
    ConnectTimeoutSuffix,
    NoConnection,
    SampleFailed,
    RunStatusFailedPrefix,
    NoSavedHosts,
    RefreshRetrySuffix,
    DbOpenFailedPrefix,
    HostListFailedPrefix,
    DbNotCreated,
    Avg1m,
    Load5mPrefix,
    Load15mMid,
    CoresSuffix,
    UsedPrefix,
    UsedFreeMid,
    RootUsedPrefix,
    UptimeSecondsPrefix,
    SecondsSuffix,
    NoHostsParen,
    Disconnected,
    Refreshing,
    Polling,
    NotPolling,
    Load,
    Memory,
    DiskRoot,
    Uptime,
    CollectFailed,
    NoData,
    SwitchHost,
    Refresh,
    StopPolling,
    StartPolling,
    LastUpdatedPrefix,
    LastUpdatedNever,
}

impl crate::i18n::Msg for StatusMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::WindowTitle => "服务器状态",
            Self::Stopped => "已停止",
            Self::ConnectTimeoutPrefix => "连接超过 ",
            Self::ConnectTimeoutSuffix => " 秒没有完成",
            Self::NoConnection => "没有可用的连接",
            Self::SampleFailed => {
                "这次没有采到：超过 5 秒没有结果，或者服务器返回的内容不像 Linux 系统信息"
            }
            Self::RunStatusFailedPrefix => "执行状态命令失败：",
            Self::NoSavedHosts => "还没有保存的主机；先在「主机」窗口里添加",
            Self::RefreshRetrySuffix => "；刷新列表后再试",
            Self::DbOpenFailedPrefix => "打开主机库失败：",
            Self::HostListFailedPrefix => "读取主机列表失败：",
            Self::DbNotCreated => "主机库还没有创建",
            Self::Avg1m => "1 分钟平均",
            Self::Load5mPrefix => "5 分钟 ",
            Self::Load15mMid => " · 15 分钟 ",
            Self::CoresSuffix => " 核",
            Self::UsedPrefix => "已用 ",
            Self::UsedFreeMid => "% · 空闲 ",
            Self::RootUsedPrefix => "根分区 / · 已用 ",
            Self::UptimeSecondsPrefix => "开机 ",
            Self::SecondsSuffix => " 秒",
            Self::NoHostsParen => "（还没有主机）",
            Self::Disconnected => "已断开",
            Self::Refreshing => "刷新中…",
            Self::Polling => "轮询中（每 2 秒）",
            Self::NotPolling => "未轮询",
            Self::Load => "负载",
            Self::Memory => "内存",
            Self::DiskRoot => "磁盘（/）",
            Self::Uptime => "运行时间",
            Self::CollectFailed => "采集失败：原因见下面",
            Self::NoData => "还没有数据：点「刷新」采集一次，或点「开始轮询」每 2 秒采集",
            Self::SwitchHost => "切换主机",
            Self::Refresh => "刷新",
            Self::StopPolling => "停止轮询",
            Self::StartPolling => "开始轮询",
            Self::LastUpdatedPrefix => "最后更新：",
            Self::LastUpdatedNever => "最后更新：—",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::WindowTitle => "Server Status",
            Self::Stopped => "Stopped",
            Self::ConnectTimeoutPrefix => "The connection did not finish within ",
            Self::ConnectTimeoutSuffix => " seconds",
            Self::NoConnection => "No connection available",
            Self::SampleFailed => {
                "Nothing was collected this time: no result within 5 seconds, or the server output did not look like Linux system information"
            }
            Self::RunStatusFailedPrefix => "Running the status command failed: ",
            Self::NoSavedHosts => "No saved hosts yet — add one in the Hosts window first",
            Self::RefreshRetrySuffix => "; refresh the list and try again",
            Self::DbOpenFailedPrefix => "Failed to open the host database: ",
            Self::HostListFailedPrefix => "Failed to read the host list: ",
            Self::DbNotCreated => "The host database has not been created yet",
            Self::Avg1m => "1-minute average",
            Self::Load5mPrefix => "5 minutes ",
            Self::Load15mMid => " · 15 minutes ",
            Self::CoresSuffix => "cores",
            Self::UsedPrefix => "Used ",
            Self::UsedFreeMid => "% · free ",
            Self::RootUsedPrefix => "Root partition / · used ",
            Self::UptimeSecondsPrefix => "Up for ",
            Self::SecondsSuffix => "seconds",
            Self::NoHostsParen => "(no hosts yet)",
            Self::Disconnected => "Disconnected",
            Self::Refreshing => "Refreshing…",
            Self::Polling => "Polling (every 2 seconds)",
            Self::NotPolling => "Not polling",
            Self::Load => "Load",
            Self::Memory => "Memory",
            Self::DiskRoot => "Disk (/)",
            Self::Uptime => "Uptime",
            Self::CollectFailed => "Collection failed — see below",
            Self::NoData => {
                "No data yet: click Refresh to collect once, or Start Polling to collect every 2 seconds"
            }
            Self::SwitchHost => "Switch host",
            Self::Refresh => "Refresh",
            Self::StopPolling => "Stop Polling",
            Self::StartPolling => "Start Polling",
            Self::LastUpdatedPrefix => "Last updated: ",
            Self::LastUpdatedNever => "Last updated: —",
        }
    }
}

/// 轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// 设置页「状态栏刷新间隔」（秒）；0 = 没设置，用 [`POLL_INTERVAL`]。
static REFRESH_SECS: AtomicU64 = AtomicU64::new(0);

/// 改采集间隔：正在跑的轮询器下一轮就按新值走。只影响生产节奏（测试用的缩短节奏不受它影响）。
pub(crate) fn set_refresh_secs(secs: u64) {
    REFRESH_SECS.store(secs, Ordering::SeqCst);
}

/// 生产节奏下套上设置里的间隔；别的节奏（测试缩短过的）原样返回。
fn with_refresh_override(timing: Timing) -> Timing {
    let secs = REFRESH_SECS.load(Ordering::SeqCst);
    if secs > 0 && timing.interval == POLL_INTERVAL {
        Timing {
            interval: Duration::from_secs(secs),
            ..timing
        }
    } else {
        timing
    }
}

/// 失败后的最长等待：连续失败时间隔翻倍，到这里封顶。
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// 建连接（含认证）的时限。采集命令有自己的时限（core 的 `COLLECT_TIMEOUT`），连接卡住不能把
/// 轮询器一直占着，窗口关闭的停止信号也要能打断它。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// 等待期间多久看一次停止 / 刷新信号。
const STOP_CHECK: Duration = Duration::from_millis(100);

/// 界面取后台结果的间隔。
const UI_TICK: Duration = Duration::from_millis(100);

/// 一次采集的结果。
///
/// `has_previous` 说明采集器里有没有上一次的计数：没有就不能把 `net_*_speed` 当网速
/// （core 在第一次采样时给的是 0，不是「不知道」），界面要显示「—」。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ServerStatus {
    pub stats: ServerStats,
    pub has_previous: bool,
}

impl ServerStatus {
    /// 内存使用率（已用 / 总量）。
    fn mem_percent(&self) -> Option<u8> {
        mem_percent_of(&self.stats)
    }
}

fn mem_percent_of(stats: &ServerStats) -> Option<u8> {
    match (stats.mem_used_mib, stats.mem_total_mib) {
        (Some(used), Some(total)) if total > 0 => Some(
            (used as f64 / total as f64 * 100.0)
                .round()
                .clamp(0.0, 100.0) as u8,
        ),
        _ => None,
    }
}

/// 拨号次数、采集次数（进程内累计）。验收「多个窗格看同一台主机只有一个采集」时对前后
/// 两次读数做差；不用于任何逻辑。
static DIALS: AtomicU64 = AtomicU64::new(0);
static COLLECTS: AtomicU64 = AtomicU64::new(0);

/// （拨号次数, 采集次数）。
#[cfg_attr(not(debug_assertions), allow(dead_code))]
pub(crate) fn poll_counters() -> (u64, u64) {
    (
        DIALS.load(Ordering::SeqCst),
        COLLECTS.load(Ordering::SeqCst),
    )
}

// ---------------------------------------------------------------------------
// 显示用的纯函数
// ---------------------------------------------------------------------------

/// 进度条宽度（0..=1）：百分比再脏也不会画到框外。
fn bar_fraction(percent: u8) -> f32 {
    f32::from(percent.min(100)) / 100.0
}

/// 运行时间：天 / 小时 / 分钟；不足一天不显示"0 天"。英文用紧凑写法（`3d 2h 5m`）。
fn format_uptime(seconds: u64) -> String {
    format_uptime_in(crate::i18n::current(), seconds)
}

fn format_uptime_in(lang: crate::i18n::Lang, seconds: u64) -> String {
    let minutes = seconds / 60;
    let days = minutes / (24 * 60);
    let hours = (minutes / 60) % 24;
    let minutes = minutes % 60;
    match lang {
        crate::i18n::Lang::Zh => {
            if days > 0 {
                format!("{days} 天 {hours} 小时 {minutes} 分钟") // i18n-allow: 中文分支
            } else if hours > 0 {
                format!("{hours} 小时 {minutes} 分钟") // i18n-allow: 中文分支
            } else {
                format!("{minutes} 分钟") // i18n-allow: 中文分支
            }
        }
        crate::i18n::Lang::En => {
            if days > 0 {
                format!("{days}d {hours}h {minutes}m")
            } else if hours > 0 {
                format!("{hours}h {minutes}m")
            } else {
                format!("{minutes}m")
            }
        }
    }
}

/// "最后更新"用相对时间：窗口里一直重绘，不需要再起一个格式化时钟。
fn format_ago(elapsed: Duration) -> String {
    format_ago_in(crate::i18n::current(), elapsed)
}

fn format_ago_in(lang: crate::i18n::Lang, elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    match lang {
        crate::i18n::Lang::Zh => match seconds {
            0 => "刚刚".to_owned(),              // i18n-allow: 中文分支
            1..=59 => format!("{seconds} 秒前"), // i18n-allow: 中文分支
            60..=3599 => format!("{} 分 {} 秒前", seconds / 60, seconds % 60), // i18n-allow: 中文分支
            _ => format!("{} 小时 {} 分前", seconds / 3600, (seconds % 3600) / 60), // i18n-allow: 中文分支
        },
        crate::i18n::Lang::En => match seconds {
            0 => "just now".to_owned(),
            1..=59 => format!("{seconds}s ago"),
            60..=3599 => {
                let (m, s) = (seconds / 60, seconds % 60);
                if s == 0 {
                    format!("{m}m ago")
                } else {
                    format!("{m}m {s}s ago")
                }
            }
            _ => {
                let (h, m) = (seconds / 3600, (seconds % 3600) / 60);
                if m == 0 {
                    format!("{h}h ago")
                } else {
                    format!("{h}h {m}m ago")
                }
            }
        },
    }
}

// ---------------------------------------------------------------------------
// 进程级 hub：一台主机一个轮询器，多个窗口共用
// ---------------------------------------------------------------------------

/// 轮询器把一次结果交回 hub 的回调。
type ReportFn = Arc<dyn Fn(i64, Result<ServerStatus, String>) + Send + Sync>;

/// 轮询器接口。真实实现是 [`LinkPoller`] + [`SshLink`]；测试换成假实现，不起 runtime、不碰网络。
///
/// `start` 必须很快返回，并且不能在这期间回调 `report`（hub 调用它时还拿着锁）。
trait Poller: Send + Sync + 'static {
    /// `in_flight` 用来串行同一台主机上的命令：旧轮询器还没退出时，新轮询器先等它，
    /// 避免"停止后立刻开始"让两条命令同时在服务器上跑。
    fn start(
        &self,
        host_id: i64,
        in_flight: Arc<tokio::sync::Mutex<()>>,
        report: ReportFn,
    ) -> Result<PollHandle, String>;
}

/// 一个在跑的轮询器。`Drop` 也会置停止标志，漏掉哪条退出路径都不会留下后台任务。
struct PollHandle {
    stop: Arc<AtomicBool>,
    /// 「刷新」：叫醒轮询器马上再采一次，不另起一条采集。
    wake: Option<std::sync::mpsc::Sender<()>>,
}

impl PollHandle {
    fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    fn refresh(&self) {
        if let Some(wake) = &self.wake {
            let _ = wake.send(());
        }
    }
}

impl Drop for PollHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 一台主机的最新快照。界面只读它，轮询器只写它。
#[derive(Clone, Debug, Default)]
struct StatusSnapshot {
    /// 最近一次成功的数据；失败时保留，界面标「已断开」。
    status: Option<ServerStatus>,
    /// 最近一次失败的原因（原文）；成功一次就清掉。
    error: Option<String>,
    /// 每次回调 +1，界面靠它判断有没有新数据。
    revision: u64,
    /// 最近一次成功的时间。
    updated_at: Option<Instant>,
    /// 最近一次的网速（收，发），字节/秒；这条连接上还没有两个样本时是 `None`。
    net_rate: Option<(f64, f64)>,
}

struct HostEntry {
    subscribers: usize,
    /// 轮询器代号：每次启动 +1，旧轮询器迟到的结果按代号丢掉。
    generation: u64,
    handle: Option<PollHandle>,
    in_flight: Arc<tokio::sync::Mutex<()>>,
    snapshot: StatusSnapshot,
}

impl Default for HostEntry {
    fn default() -> Self {
        Self {
            subscribers: 0,
            generation: 0,
            handle: None,
            in_flight: Arc::new(tokio::sync::Mutex::new(())),
            snapshot: StatusSnapshot::default(),
        }
    }
}

#[derive(Default)]
struct HubState {
    hosts: HashMap<i64, HostEntry>,
}

/// 进程级采集中心：订阅计数 + 共享快照。
struct StatusHub {
    poller: Arc<dyn Poller>,
    state: Arc<Mutex<HubState>>,
}

impl StatusHub {
    fn new(poller: Arc<dyn Poller>) -> Self {
        Self {
            poller,
            state: Arc::new(Mutex::new(HubState::default())),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HubState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 订阅一台主机，返回订阅数。已经有人在采就直接复用，不重新连、不重复轮询。
    fn subscribe(&self, host_id: i64) -> usize {
        let mut state = self.lock();
        let entry = state.hosts.entry(host_id).or_default();
        entry.subscribers += 1;
        if entry.handle.is_none() {
            entry.generation = entry.generation.wrapping_add(1);
            // 停了一阵再订阅：网速是新连接上两次采样的差值，旧的不能冒充现在的。
            entry.snapshot.net_rate = None;
            let generation = entry.generation;
            let shared = Arc::clone(&self.state);
            let report: ReportFn = Arc::new(move |host_id, result| {
                record_result(&shared, host_id, generation, result);
            });
            match self
                .poller
                .start(host_id, Arc::clone(&entry.in_flight), report)
            {
                Ok(handle) => entry.handle = Some(handle),
                // 起不来（比如没有 runtime）：把原因放进快照，界面照常显示。
                Err(err) => {
                    entry.snapshot.error = Some(err);
                    entry.snapshot.revision = entry.snapshot.revision.wrapping_add(1);
                }
            }
        }
        entry.subscribers
    }

    /// 退订。最后一个订阅者走了就停轮询器；快照留着，下次打开先显示旧数据。
    fn unsubscribe(&self, host_id: i64) {
        let mut state = self.lock();
        let Some(entry) = state.hosts.get_mut(&host_id) else {
            return;
        };
        entry.subscribers = entry.subscribers.saturating_sub(1);
        if entry.subscribers == 0 {
            // Drop 置停止标志；代号 +1 让还在路上的结果作废。
            entry.handle = None;
            entry.generation = entry.generation.wrapping_add(1);
        }
    }

    fn snapshot(&self, host_id: i64) -> Option<StatusSnapshot> {
        self.lock()
            .hosts
            .get(&host_id)
            .map(|entry| entry.snapshot.clone())
    }

    /// 让后台立刻采一次（只对已经在跑的主机有效）。
    fn refresh(&self, host_id: i64) {
        let state = self.lock();
        if let Some(handle) = state
            .hosts
            .get(&host_id)
            .and_then(|entry| entry.handle.as_ref())
        {
            handle.refresh();
        }
    }

    #[cfg(test)]
    fn subscribers(&self, host_id: i64) -> usize {
        self.lock()
            .hosts
            .get(&host_id)
            .map(|entry| entry.subscribers)
            .unwrap_or(0)
    }
}

/// 轮询器回调：只接受当前这一代的结果，旧轮询器迟到的直接丢掉。
fn record_result(
    state: &Mutex<HubState>,
    host_id: i64,
    generation: u64,
    result: Result<ServerStatus, String>,
) {
    let mut state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(entry) = state.hosts.get_mut(&host_id) else {
        return;
    };
    if entry.generation != generation {
        return;
    }
    match result {
        Ok(status) => {
            let now = Instant::now();
            entry.snapshot.net_rate = status
                .has_previous
                .then_some((status.stats.net_rx_speed, status.stats.net_tx_speed));
            entry.snapshot.status = Some(status);
            entry.snapshot.error = None;
            entry.snapshot.updated_at = Some(now);
        }
        // 失败时保留上一次的数据：界面按「已断开」显示旧数据 + 原文错误。
        Err(err) => entry.snapshot.error = Some(err),
    }
    entry.snapshot.revision = entry.snapshot.revision.wrapping_add(1);
}

static HUB: OnceLock<StatusHub> = OnceLock::new();

fn hub() -> &'static StatusHub {
    HUB.get_or_init(|| {
        StatusHub::new(Arc::new(LinkPoller {
            link: Arc::new(SshLink),
            timing: Timing::PRODUCTION,
        }))
    })
}

// ---------------------------------------------------------------------------
// 给主窗口状态栏用的入口（和状态窗口共用同一个 hub，同一台主机只跑一个轮询器）
// ---------------------------------------------------------------------------

/// 状态栏 / 概览面板要显示的一次采集结果：core 的 [`ServerStats`] 原样带着（每个核心、分区、
/// 网卡、进程、系统信息都在里面），再加上这条连接上算得出来的网速。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StatusSummary {
    pub stats: ServerStats,
    /// 网速（收，发），字节/秒；还没有算出来是 `None`（界面显示「—」）。
    pub net_rate: Option<(f64, f64)>,
}

impl StatusSummary {
    fn from_status(status: &ServerStatus, net_rate: Option<(f64, f64)>) -> Self {
        Self {
            stats: status.stats.clone(),
            net_rate,
        }
    }

    pub fn mem_percent(&self) -> Option<u8> {
        mem_percent_of(&self.stats)
    }

    /// 负载折成百分比：1 核跑满 = 100%（同 0.2.14 概览的「负载」圆环）。
    pub fn load_percent(&self) -> Option<u8> {
        let load = self.stats.load_1m?;
        let cores = self.stats.cpu_cores.filter(|cores| *cores > 0)?;
        Some(percent_f(load / f64::from(cores) * 100.0))
    }
}

fn percent_f(value: f64) -> u8 {
    if value.is_finite() {
        value.round().clamp(0.0, 100.0) as u8
    } else {
        0
    }
}

/// 主机最新的状态：数据（最近一次成功的）和最近一次失败的原因。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HostStatus {
    pub summary: Option<StatusSummary>,
    pub error: Option<String>,
    /// 每次后台有新结果就变，界面靠它判断要不要重绘。
    pub revision: u64,
}

/// 订阅一台主机的状态采集；返回时轮询器已经在后台跑（或原因已写进快照）。
pub(crate) fn subscribe_host(host_id: i64) {
    hub().subscribe(host_id);
}

/// 退订；最后一个订阅者走了 hub 才停轮询器。
pub(crate) fn unsubscribe_host(host_id: i64) {
    hub().unsubscribe(host_id);
}

/// 读 hub 里这台主机的快照；从没订阅过就是 `None`。
pub(crate) fn host_status(host_id: i64) -> Option<HostStatus> {
    let snapshot = hub().snapshot(host_id)?;
    Some(HostStatus {
        summary: snapshot
            .status
            .as_ref()
            .map(|status| StatusSummary::from_status(status, snapshot.net_rate)),
        error: snapshot.error,
        revision: snapshot.revision,
    })
}

/// 状态栏里一项的颜色档位：80% 以上黄、95% 以上红（同 0.2.14 的 `WARN` / `CRITICAL`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    Normal,
    Warn,
    Critical,
}

pub(crate) fn level_of(percent: u8) -> Level {
    match percent {
        95..=u8::MAX => Level::Critical,
        80..=94 => Level::Warn,
        _ => Level::Normal,
    }
}

/// 运行时间的短写法（同 0.2.14 的 `formatUptime`）：N 天 N 小时 / N 小时 N 分 / N 分钟。
pub(crate) fn uptime_label(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if days > 0 {
        format!("{days} 天 {hours} 小时") // i18n-allow: 中文分支
    } else if hours > 0 {
        format!("{hours} 小时 {minutes} 分") // i18n-allow: 中文分支
    } else {
        format!("{minutes} 分钟") // i18n-allow: 中文分支
    }
}

/// MiB 换成「x.xGB」（同 0.2.14 的 `formatMibAsG`）；没有读数是 `--`。
pub(crate) fn mib_gb_label(mib: Option<f64>, digits: usize) -> String {
    match mib {
        Some(value) if value.is_finite() => format!("{:.digits$}GB", value / 1024.0),
        _ => "--".to_owned(),
    }
}

/// 轮询的节奏。生产用 [`Timing::PRODUCTION`]，测试缩短它，不用真等几秒。
#[derive(Clone, Copy, Debug)]
struct Timing {
    interval: Duration,
    max_backoff: Duration,
    connect_timeout: Duration,
    /// 等待期间看停止 / 刷新信号的间隔。
    check: Duration,
}

impl Timing {
    const PRODUCTION: Self = Self {
        interval: POLL_INTERVAL,
        max_backoff: MAX_BACKOFF,
        connect_timeout: CONNECT_TIMEOUT,
        check: STOP_CHECK,
    };
}

/// 连续失败次数 → 下一次等多久：没失败是正常间隔，每连续失败一次翻一倍，封顶
/// `max_backoff`；成功一次就回到正常间隔。
#[derive(Clone, Copy, Debug, Default)]
struct RetryState {
    failures: u32,
}

impl RetryState {
    fn record(&mut self, ok: bool) {
        self.failures = if ok {
            0
        } else {
            self.failures.saturating_add(1)
        };
    }

    fn delay(self, timing: &Timing) -> Duration {
        if self.failures == 0 {
            return timing.interval;
        }
        // 位移量封顶，避免极端失败次数溢出；封顶之后反正会被 max_backoff 截断。
        let factor = 1u32 << self.failures.min(16);
        timing
            .interval
            .saturating_mul(factor)
            .min(timing.max_backoff)
    }
}

/// 采集用的连接：拨号一次，之后在同一条连接上反复采。真实实现是 [`SshLink`]，
/// 测试换成假的，用来数"拨了几次号"。
trait StatusLink: Send + Sync + 'static {
    type Conn: Send + Sync + 'static;

    fn dial(&self, host_id: i64) -> BoxFuture<'_, Result<Self::Conn, String>>;

    /// 连接还能用吗；不能用就丢掉，下一轮重新拨号。
    fn is_alive(&self, conn: &Self::Conn) -> bool;

    fn collect<'a>(&'a self, conn: &'a Self::Conn) -> BoxFuture<'a, Result<ServerStatus, String>>;
}

/// 真实连接：和终端 / SFTP 同一个连接池（`factory_for_host`），池里已经有同配置的活
/// 连接就直接复用，没有才拨号。轮询器自己再握一份，连接不会因为这一轮的 `Arc` 被丢掉
/// 而关掉。采集器跟着连接走：换了连接就是新的，计数从头算。
struct SshLink;

/// 一条采集用的连接和它的采集器。
struct SshConn {
    factory: Arc<SshSessionFactory>,
    collector: tokio::sync::Mutex<StatsCollector>,
    /// 这条连接上已经成功采过几次；0 说明下一次是第一次（没有上一次的计数可以做差）。
    samples: AtomicU32,
}

impl StatusLink for SshLink {
    type Conn = SshConn;

    fn dial(&self, host_id: i64) -> BoxFuture<'_, Result<Self::Conn, String>> {
        Box::pin(async move {
            DIALS.fetch_add(1, Ordering::SeqCst);
            let factory = factory_for_host(host_id).await?;
            Ok(SshConn {
                factory,
                collector: tokio::sync::Mutex::new(StatsCollector::new()),
                samples: AtomicU32::new(0),
            })
        })
    }

    fn is_alive(&self, conn: &Self::Conn) -> bool {
        !conn.factory.connection().is_closed()
    }

    fn collect<'a>(&'a self, conn: &'a Self::Conn) -> BoxFuture<'a, Result<ServerStatus, String>> {
        Box::pin(collect_on(conn))
    }
}

/// 真实轮询器：在 SSH runtime 上跑一条后台循环。
struct LinkPoller<L: StatusLink> {
    link: Arc<L>,
    timing: Timing,
}

impl<L: StatusLink> Poller for LinkPoller<L> {
    fn start(
        &self,
        host_id: i64,
        in_flight: Arc<tokio::sync::Mutex<()>>,
        report: ReportFn,
    ) -> Result<PollHandle, String> {
        let runtime = runtime()?;
        let stop = Arc::new(AtomicBool::new(false));
        let (wake, wakes) = std::sync::mpsc::channel();
        runtime.spawn(poll_loop(
            Arc::clone(&self.link),
            self.timing,
            host_id,
            in_flight,
            Arc::clone(&stop),
            wakes,
            report,
        ));
        Ok(PollHandle {
            stop,
            wake: Some(wake),
        })
    }
}

/// 一台主机的轮询循环：连接复用、失败退避、可随时停止。
async fn poll_loop<L: StatusLink>(
    link: Arc<L>,
    timing: Timing,
    host_id: i64,
    in_flight: Arc<tokio::sync::Mutex<()>>,
    stop: Arc<AtomicBool>,
    mut wakes: std::sync::mpsc::Receiver<()>,
    report: ReportFn,
) {
    let mut conn: Option<L::Conn> = None;
    let mut retry = RetryState::default();
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        // 同一台主机同一时刻只跑一条命令：上一代轮询器还没退出时先等它。
        let busy = in_flight.lock().await;
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let result = poll_with(&*link, &timing, host_id, &mut conn, &stop).await;
        if stop.load(Ordering::SeqCst) {
            break;
        }
        retry.record(result.is_ok());
        report(host_id, result);
        drop(busy);
        let live = with_refresh_override(timing);
        if wait_next(&stop, &mut wakes, retry.delay(&live), live.check).await {
            break;
        }
    }
}

/// 一轮采集：没有可用连接就拨号（有时限，停止信号能打断），然后在连接上采一次。
/// 连接死了（采集失败且连接已关）就丢掉，下一轮重新拨。
async fn poll_with<L: StatusLink>(
    link: &L,
    timing: &Timing,
    host_id: i64,
    conn: &mut Option<L::Conn>,
    stop: &AtomicBool,
) -> Result<ServerStatus, String> {
    if conn.as_ref().is_some_and(|conn| !link.is_alive(conn)) {
        *conn = None;
    }
    if conn.is_none() {
        let dial = tokio::time::timeout(timing.connect_timeout, link.dial(host_id));
        let dialed = tokio::select! {
            dialed = dial => dialed,
            () = wait_stopped(stop, timing.check) => return Err(crate::i18n::t(StatusMsg::Stopped).to_owned()),
        };
        let fresh = dialed.map_err(|_| {
            format!(
                "{}{}{}",
                crate::i18n::t(StatusMsg::ConnectTimeoutPrefix),
                timing.connect_timeout.as_secs().max(1),
                crate::i18n::t(StatusMsg::ConnectTimeoutSuffix)
            )
        })??;
        *conn = Some(fresh);
    }
    let Some(current) = conn.as_ref() else {
        return Err(crate::i18n::t(StatusMsg::NoConnection).to_owned());
    };
    let result = link.collect(current).await;
    if result.is_err() && !link.is_alive(current) {
        *conn = None;
    }
    result
}

/// 一直等到停止标志被置上。
async fn wait_stopped(stop: &AtomicBool, check: Duration) {
    while !stop.load(Ordering::SeqCst) {
        tokio::time::sleep(check).await;
    }
}

/// 等下一次采集：`delay` 到点，或者收到「刷新」。返回 `true` 表示该退出了。
async fn wait_next(
    stop: &AtomicBool,
    wakes: &mut std::sync::mpsc::Receiver<()>,
    delay: Duration,
    check: Duration,
) -> bool {
    // 刷新请求如果是在这次采集期间来的，这次结果已经是新的了，不用再补一次。
    while wakes.try_recv().is_ok() {}
    let deadline = Instant::now() + delay;
    loop {
        if stop.load(Ordering::SeqCst) {
            return true;
        }
        if wakes.try_recv().is_ok() {
            // 连着点了几次刷新也只补一次。
            while wakes.try_recv().is_ok() {}
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        // 分段睡：停止 / 刷新都能及时看到，也不会空转。
        tokio::time::sleep((deadline - now).min(check)).await;
    }
}

/// 在已认证连接上采一次：脚本和解析都是 core 的（0.2.14 同一份）。
async fn collect_on(conn: &SshConn) -> Result<ServerStatus, String> {
    COLLECTS.fetch_add(1, Ordering::SeqCst);
    let connection = conn.factory.connection();
    let mut collector = conn.collector.lock().await;
    match collector.collect(&connection).await {
        Ok(Some(stats)) => {
            let has_previous = conn.samples.fetch_add(1, Ordering::SeqCst) > 0;
            Ok(ServerStatus {
                stats,
                has_previous,
            })
        }
        // core 把「超时」和「输出认不出」都当成「这次没采到」。
        Ok(None) => Err(crate::i18n::t(StatusMsg::SampleFailed).to_owned()),
        Err(err) => Err(format!(
            "{}{err}",
            crate::i18n::t(StatusMsg::RunStatusFailedPrefix)
        )),
    }
}

// ---------------------------------------------------------------------------
// 窗口
// ---------------------------------------------------------------------------

pub struct StatusDemo {
    hosts: Vec<Host>,
    selected: usize,
    /// 窗口自己的开关：打开时持有一份 hub 订阅。
    polling: bool,
    /// 一次性刷新进行中：结果回来后自动退订，后台不留轮询器。
    one_shot: bool,
    /// 当前订阅的主机；`None` = 没订阅。
    subscribed: Option<i64>,
    /// 上次看到的快照修订号，用来判断有没有新数据。
    seen_revision: u64,
    data: Option<ServerStatus>,
    /// 最近一次采集失败的原因（原文）。
    error: Option<String>,
    updated_at: Option<Instant>,
    /// 主机库 / 主机本身的问题，和采集错误分开显示。
    load_error: Option<String>,
    focus: FocusHandle,
    _tick: Task<()>,
}

impl StatusDemo {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (hosts, load_error) = match load_hosts() {
            Ok(hosts) => (hosts, None),
            Err(err) => (Vec::new(), Some(err)),
        };
        let focus = cx.focus_handle();
        let mut this = Self {
            hosts,
            selected: 0,
            polling: false,
            one_shot: false,
            subscribed: None,
            seen_revision: 0,
            data: None,
            error: None,
            updated_at: None,
            load_error,
            focus,
            _tick: Task::ready(()),
        };
        // 别的窗口可能正在采同一台主机：先把它缓存的旧数据显示出来。
        this.show_cached();
        // 100ms 取一次 hub 快照；窗口没了 `update_in` 会报错，任务自己退出。
        this._tick = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(UI_TICK).await;
                if this
                    .update_in(cx, |this, _window, cx| this.poll_hub(cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        this
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    fn current_host(&self) -> Option<&Host> {
        self.hosts.get(self.selected)
    }

    fn host_id(&self) -> Option<i64> {
        self.current_host().map(|host| host.id)
    }

    /// 把 hub 里已有的快照显示出来（不订阅、不采集）。
    fn show_cached(&mut self) {
        let Some(host_id) = self.host_id() else {
            return;
        };
        let Some(snapshot) = hub().snapshot(host_id) else {
            return;
        };
        self.seen_revision = snapshot.revision;
        self.data = snapshot.status;
        self.error = snapshot.error;
        self.updated_at = snapshot.updated_at;
    }

    fn clear_data(&mut self) {
        self.data = None;
        self.error = None;
        self.updated_at = None;
        self.seen_revision = 0;
    }

    /// 每 100ms 看一眼订阅的主机；有新快照就搬到界面，不碰网络。
    fn poll_hub(&mut self, cx: &mut Context<Self>) {
        let Some(host_id) = self.subscribed else {
            return;
        };
        let Some(snapshot) = hub().snapshot(host_id) else {
            return;
        };
        if snapshot.revision == self.seen_revision {
            return;
        }
        self.seen_revision = snapshot.revision;
        self.data = snapshot.status;
        self.error = snapshot.error;
        self.updated_at = snapshot.updated_at;
        if self.one_shot {
            // 一次性刷新拿到结果就退订，后台不留下轮询器。
            self.unsubscribe();
        }
        cx.notify();
    }

    fn unsubscribe(&mut self) {
        self.one_shot = false;
        if let Some(host_id) = self.subscribed.take() {
            hub().unsubscribe(host_id);
        }
    }

    fn start_polling(&mut self, cx: &mut Context<Self>) {
        if self.polling {
            return;
        }
        let Some(host) = self.current_host().cloned() else {
            self.load_error = Some(crate::i18n::t(StatusMsg::NoSavedHosts).to_owned());
            cx.notify();
            return;
        };
        // 主机可能在窗口开着的时候被改名 / 删掉了：连之前重新读一次库。
        match load_host(host.id) {
            Ok(fresh) => {
                if let Some(slot) = self.hosts.get_mut(self.selected) {
                    *slot = fresh;
                }
            }
            Err(err) => {
                self.load_error = Some(format!(
                    "{err}{}",
                    crate::i18n::t(StatusMsg::RefreshRetrySuffix)
                ));
                cx.notify();
                return;
            }
        }
        self.load_error = None;
        self.polling = true;
        // 清掉可能残留的一次性订阅，保证一个窗口只有一个订阅。
        self.unsubscribe();
        self.subscribed = Some(host.id);
        self.seen_revision = 0;
        hub().subscribe(host.id);
        self.show_cached();
        cx.notify();
    }

    fn stop_polling(&mut self, cx: &mut Context<Self>) {
        self.polling = false;
        self.unsubscribe();
        cx.notify();
    }

    /// 换到下一台主机；原来在轮询就继续轮询新主机，但绝不同时跑两台。
    fn cycle_host(&mut self, cx: &mut Context<Self>) {
        if self.hosts.len() < 2 {
            return;
        }
        let was_polling = self.polling;
        self.polling = false;
        self.unsubscribe();
        self.selected = (self.selected + 1) % self.hosts.len();
        self.clear_data();
        self.show_cached();
        if was_polling {
            self.start_polling(cx);
        } else {
            cx.notify();
        }
    }

    /// 重读主机列表（本地库，很快）。选中的主机按 id 找回来，不在列表里就回到第一台。
    ///
    /// 返回 `true` 表示列表没读成，或原来选中的主机已经不在了（调用方不要再自动采集）。
    fn refresh_hosts(&mut self) -> bool {
        let anchored = self.subscribed.or_else(|| self.host_id());
        match load_hosts() {
            Ok(hosts) => {
                self.hosts = hosts;
                self.load_error = None;
                match anchored.and_then(|id| self.hosts.iter().position(|host| host.id == id)) {
                    Some(index) => {
                        self.selected = index;
                        false
                    }
                    None => {
                        // 原来那台被删了：停掉轮询、清掉它的数据。
                        if self.subscribed.is_some() {
                            self.polling = false;
                            self.unsubscribe();
                            self.clear_data();
                        }
                        self.selected = 0;
                        anchored.is_some()
                    }
                }
            }
            Err(err) => {
                self.load_error = Some(err);
                true
            }
        }
    }

    /// 「刷新」：轮询中就立刻补采一次；没轮询就做一次性的采集。
    fn refresh_clicked(&mut self, cx: &mut Context<Self>) {
        if self.refresh_hosts() {
            cx.notify();
            return;
        }
        let Some(host_id) = self.host_id() else {
            cx.notify();
            return;
        };
        match (self.polling, self.subscribed) {
            // 已经在采这台主机（轮询中，或上一次一次性刷新还没回来）：只补采一次，
            // 不再加一份订阅——否则这份订阅没人退，轮询器会一直跑下去。
            (_, Some(subscribed)) if subscribed == host_id => {
                hub().refresh(host_id);
            }
            _ => {
                self.polling = false;
                self.subscribed = Some(host_id);
                self.one_shot = true;
                self.seen_revision = 0;
                hub().subscribe(host_id);
                self.show_cached();
                hub().refresh(host_id);
            }
        }
        cx.notify();
    }

    /// 窗口关闭：退订；最后一个订阅者走了 hub 会停轮询器。
    fn close(&mut self) {
        self.polling = false;
        self.unsubscribe();
    }
}

impl Drop for StatusDemo {
    fn drop(&mut self) {
        // 兜底：窗口没走 `on_window_should_close` 也不会留下轮询器。
        self.close();
    }
}

/// 读主机库；库文件还不存在就当作"没有主机"，不为了看一眼状态去建一个空库。
fn open_host_database() -> Result<Option<Database>, String> {
    let path = data_dir().join("zssh.db");
    if !path.exists() {
        return Ok(None);
    }
    let db = Database::open(&path)
        .map_err(|err| format!("{}{err}", crate::i18n::t(StatusMsg::DbOpenFailedPrefix)))?;
    Ok(Some(db))
}

fn load_hosts() -> Result<Vec<Host>, String> {
    let Some(db) = open_host_database()? else {
        return Ok(Vec::new());
    };
    db.host_list()
        .map_err(|err| format!("{}{err}", crate::i18n::t(StatusMsg::HostListFailedPrefix)))
}

fn load_host(host_id: i64) -> Result<Host, String> {
    let Some(db) = open_host_database()? else {
        return Err(crate::i18n::t(StatusMsg::DbNotCreated).to_owned());
    };
    db.host_get(host_id)
        .map_err(|err| crate::error_text::error_text(&err))
}

/// 打开服务器状态窗口；同一个进程可以开多个，采集由进程级 hub 去重。
pub fn open_status_window(cx: &mut App) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(
            size(::gpui::px(860.), ::gpui::px(600.)),
            cx,
        )),
        focus: true,
        ..crate::window_frame::app_options()
    };
    let _ = cx.open_window(options, |window, cx| {
        let view = cx.new(|cx| StatusDemo::new(window, cx));
        window.set_window_title(crate::i18n::t(StatusMsg::WindowTitle));
        let weak = view.downgrade();
        window.on_window_should_close(cx, move |_window, cx| {
            if let Some(view) = weak.upgrade() {
                view.update(cx, |this, _cx| this.close());
            }
            true
        });
        window.focus(&view.read(cx).focus_handle(), cx);
        view
    });
}

// ---------------------------------------------------------------------------
// 绘制
// ---------------------------------------------------------------------------

/// 卡片用到的主题色一次取齐，免得每个 helper 都去借 `cx`。
#[derive(Clone, Copy)]
struct CardColors {
    muted: Hsla,
    track: Hsla,
    success: Hsla,
    warning: Hsla,
    danger: Hsla,
    primary: Hsla,
    border: Hsla,
    card: Hsla,
}

fn status_card(title: &str, body: AnyElement, colors: &CardColors) -> AnyElement {
    v_flex()
        .flex_1()
        .h(::gpui::px(150.))
        .gap_2()
        .p_3()
        .rounded_md()
        .border_1()
        .border_color(colors.border)
        .bg(colors.card)
        .child(
            div()
                .text_xs()
                .text_color(colors.muted)
                .child(title.to_owned()),
        )
        .child(body)
        .into_any_element()
}

fn percent_bar(percent: u8, fill: Hsla, colors: &CardColors) -> AnyElement {
    div()
        .h(::gpui::px(8.))
        .w_full()
        .rounded_full()
        .overflow_hidden()
        .bg(colors.track)
        .child(
            div()
                .h_full()
                .w(::gpui::relative(bar_fraction(percent)))
                .rounded_full()
                .bg(fill),
        )
        .into_any_element()
}

/// 用量越高越显眼；阈值只影响颜色，不影响数值。
fn level_color(percent: u8, colors: &CardColors) -> Hsla {
    match percent {
        0..=69 => colors.success,
        70..=89 => colors.warning,
        _ => colors.danger,
    }
}

fn load_card(data: &ServerStatus, colors: &CardColors) -> AnyElement {
    let stats = &data.stats;
    let load = |value: Option<f64>| value.map_or_else(|| "--".to_owned(), |v| format!("{v:.2}"));
    v_flex()
        .gap_1()
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(div().text_2xl().child(load(stats.load_1m)))
                .child(
                    div()
                        .text_xs()
                        .text_color(colors.muted)
                        .child(crate::i18n::t(StatusMsg::Avg1m)),
                ),
        )
        .child(div().text_sm().child(format!(
            "{}{}{}{}",
            crate::i18n::t(StatusMsg::Load5mPrefix),
            load(stats.load_5m),
            crate::i18n::t(StatusMsg::Load15mMid),
            load(stats.load_15m)
        )))
        .child(div().text_xs().text_color(colors.muted).child(format!(
            "CPU {}",
            stats.cpu_cores.map_or_else(
                || "--".to_owned(),
                |c| crate::i18n::count(
                    c as u64,
                    crate::i18n::t(StatusMsg::CoresSuffix),
                    "core",
                    "cores"
                )
            )
        )))
        .into_any_element()
}

fn memory_card(data: &ServerStatus, colors: &CardColors) -> AnyElement {
    let stats = &data.stats;
    let used_percent = data.mem_percent().unwrap_or(0);
    v_flex()
        .gap_2()
        .child(div().text_sm().child(format!(
            "{} / {}",
            mib_gb_label(stats.mem_used_mib.map(|v| v as f64), 1),
            mib_gb_label(stats.mem_total_mib.map(|v| v as f64), 1)
        )))
        .child(percent_bar(
            used_percent,
            level_color(used_percent, colors),
            colors,
        ))
        .child(div().text_xs().text_color(colors.muted).child(format!(
            "{}{used_percent}%{}{}",
            crate::i18n::t(StatusMsg::UsedPrefix),
            crate::i18n::t(StatusMsg::UsedFreeMid),
            mib_gb_label(stats.mem_free_mib.map(|v| v as f64), 1)
        )))
        .into_any_element()
}

fn disk_card(data: &ServerStatus, colors: &CardColors) -> AnyElement {
    let stats = &data.stats;
    let used_percent = stats.disk_percent.unwrap_or(0);
    v_flex()
        .gap_2()
        .child(div().text_sm().child(format!(
            "{} / {}",
            mib_gb_label(stats.disk_used_mib, 1),
            mib_gb_label(stats.disk_total_mib, 1)
        )))
        .child(percent_bar(
            used_percent,
            level_color(used_percent, colors),
            colors,
        ))
        .child(div().text_xs().text_color(colors.muted).child(format!(
            "{}{used_percent}%",
            crate::i18n::t(StatusMsg::RootUsedPrefix)
        )))
        .into_any_element()
}

fn uptime_card(data: &ServerStatus, colors: &CardColors) -> AnyElement {
    let seconds = data.stats.uptime_secs.unwrap_or(0);
    v_flex()
        .gap_1()
        .child(div().text_lg().child(format_uptime(seconds)))
        .child(div().text_xs().text_color(colors.muted).child(format!(
            "{}{}",
            crate::i18n::t(StatusMsg::UptimeSecondsPrefix),
            crate::i18n::count(
                seconds,
                crate::i18n::t(StatusMsg::SecondsSuffix),
                "second",
                "seconds"
            )
        )))
        .into_any_element()
}

impl Render for StatusDemo {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let theme = cx.theme();
        let colors = CardColors {
            muted: theme.muted_foreground,
            track: theme.muted,
            success: theme.success,
            warning: theme.warning,
            danger: theme.danger,
            primary: theme.primary,
            border: theme.border,
            card: theme.group_box,
        };
        let host_label = match self.current_host() {
            Some(host) => format!(
                "{} · {}@{}:{}",
                host.name, host.username, host.host, host.port
            ),
            None => crate::i18n::t(StatusMsg::NoHostsParen).to_owned(),
        };
        let total_hosts = self.hosts.len();
        let position = if total_hosts == 0 {
            "0 / 0".to_owned()
        } else {
            format!("{} / {}", self.selected + 1, total_hosts)
        };
        let has_host = total_hosts > 0;
        let polling = self.polling;
        let one_shot = self.one_shot;
        let data = self.data.clone();
        let error = self.error.clone();
        let load_error = self.load_error.clone();
        let updated = self.updated_at.map(|at| format_ago(at.elapsed()));

        let (badge_text, badge_color) = if error.is_some() {
            (crate::i18n::t(StatusMsg::Disconnected), colors.danger)
        } else if one_shot {
            (crate::i18n::t(StatusMsg::Refreshing), colors.primary)
        } else if polling {
            (crate::i18n::t(StatusMsg::Polling), colors.success)
        } else {
            (crate::i18n::t(StatusMsg::NotPolling), colors.muted)
        };

        let cycle_entity = entity.clone();
        let refresh_entity = entity.clone();
        let toggle_entity = entity.clone();

        let cards = match data {
            Some(data) => v_flex()
                .gap_3()
                .child(
                    h_flex()
                        .gap_3()
                        .child(status_card(
                            crate::i18n::t(StatusMsg::Load),
                            load_card(&data, &colors),
                            &colors,
                        ))
                        .child(status_card(
                            crate::i18n::t(StatusMsg::Memory),
                            memory_card(&data, &colors),
                            &colors,
                        )),
                )
                .child(
                    h_flex()
                        .gap_3()
                        .child(status_card(
                            crate::i18n::t(StatusMsg::DiskRoot),
                            disk_card(&data, &colors),
                            &colors,
                        ))
                        .child(status_card(
                            crate::i18n::t(StatusMsg::Uptime),
                            uptime_card(&data, &colors),
                            &colors,
                        )),
                )
                .into_any_element(),
            None => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_sm()
                        .text_color(colors.muted)
                        .child(if error.is_some() {
                            crate::i18n::t(StatusMsg::CollectFailed)
                        } else {
                            crate::i18n::t(StatusMsg::NoData)
                        }),
                )
                .into_any_element(),
        };

        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .track_focus(&self.focus)
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(crate::i18n::t(StatusMsg::WindowTitle)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .text_color(colors.muted)
                            .child(host_label),
                    )
                    .child(div().text_xs().text_color(colors.muted).child(position))
                    .child(
                        Button::new("status-cycle")
                            .label(crate::i18n::t(StatusMsg::SwitchHost))
                            .disabled(total_hosts < 2)
                            .on_click(move |_, _, cx| {
                                cycle_entity.update(cx, |this, cx| this.cycle_host(cx));
                            }),
                    )
                    .child(
                        Button::new("status-refresh")
                            .label(crate::i18n::t(StatusMsg::Refresh))
                            .on_click(move |_, _, cx| {
                                refresh_entity.update(cx, |this, cx| this.refresh_clicked(cx));
                            }),
                    )
                    .child(
                        Button::new("status-toggle")
                            .label(if polling {
                                crate::i18n::t(StatusMsg::StopPolling)
                            } else {
                                crate::i18n::t(StatusMsg::StartPolling)
                            })
                            .disabled(!has_host)
                            .on_click(move |_, _, cx| {
                                toggle_entity.update(cx, |this, cx| {
                                    if this.polling {
                                        this.stop_polling(cx);
                                    } else {
                                        this.start_polling(cx);
                                    }
                                });
                            }),
                    ),
            )
            .when_some(load_error, |parent, message| {
                parent.child(div().text_sm().text_color(colors.danger).child(message))
            })
            .when(!has_host && self.load_error.is_none(), |parent| {
                parent.child(
                    div()
                        .text_sm()
                        .text_color(colors.muted)
                        .child(crate::i18n::t(StatusMsg::NoSavedHosts)),
                )
            })
            .child(cards)
            .child(div().flex_1())
            .child(
                v_flex()
                    .gap_1()
                    .border_t_1()
                    .border_color(colors.border)
                    .pt_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().text_xs().text_color(badge_color).child(badge_text))
                            .child(div().text_xs().text_color(colors.muted).child(
                                match &updated {
                                    Some(label) => format!(
                                        "{}{label}",
                                        crate::i18n::t(StatusMsg::LastUpdatedPrefix)
                                    ),
                                    None => crate::i18n::t(StatusMsg::LastUpdatedNever).to_owned(),
                                },
                            )),
                    )
                    .when_some(error, |parent, message| {
                        parent.child(div().text_xs().text_color(colors.danger).child(message))
                    }),
            )
    }
}

// ---------------------------------------------------------------------------
// 测试：全部是纯函数 / 假轮询器，不连 SSH、不碰网络
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use zssh_core::ssh::stats::ServerStats;

    // 不 `use super::*`：那会把 `gpui_kit` 的 `test` 宏带进来，盖掉内建的 `#[test]`。
    use super::{
        PollHandle, Poller, ReportFn, ServerStatus, StatusHub, bar_fraction, format_ago,
        format_ago_in, format_uptime, format_uptime_in,
    };

    /// 假轮询器：只记录启动 / 停止和回调，不连网。
    #[derive(Default)]
    struct FakePoller {
        started: Mutex<Vec<(i64, ReportFn)>>,
        alive: Mutex<Vec<Arc<AtomicBool>>>,
    }

    impl FakePoller {
        fn shared() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn start_count(&self) -> usize {
            self.started
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len()
        }

        fn is_alive(&self, index: usize) -> bool {
            self.alive
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(index)
                .is_some_and(|flag| !flag.load(Ordering::SeqCst))
        }

        fn report(&self, index: usize, host_id: i64, result: Result<ServerStatus, String>) {
            let report = {
                let started = self
                    .started
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                started.get(index).map(|(_, report)| Arc::clone(report))
            };
            if let Some(report) = report {
                report(host_id, result);
            }
        }
    }

    impl Poller for FakePoller {
        fn start(
            &self,
            host_id: i64,
            _in_flight: Arc<tokio::sync::Mutex<()>>,
            report: ReportFn,
        ) -> Result<PollHandle, String> {
            let stop = Arc::new(AtomicBool::new(false));
            self.alive
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(Arc::clone(&stop));
            self.started
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((host_id, report));
            Ok(PollHandle { stop, wake: None })
        }
    }

    fn sample_status() -> ServerStatus {
        ServerStatus {
            stats: ServerStats {
                cpu_cores: Some(4),
                load_1m: Some(0.5),
                load_5m: Some(0.4),
                load_15m: Some(0.3),
                mem_total_mib: Some(8192),
                mem_used_mib: Some(4096),
                disk_total_mib: Some(102_400.0),
                disk_used_mib: Some(51_200.0),
                disk_percent: Some(50),
                uptime_secs: Some(3600),
                ..ServerStats::default()
            },
            has_previous: false,
        }
    }

    #[test]
    fn the_bar_fraction_is_clamped() {
        assert!((bar_fraction(0) - 0.0).abs() < f32::EPSILON);
        assert!((bar_fraction(50) - 0.5).abs() < f32::EPSILON);
        assert!((bar_fraction(150) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn formats_uptime_and_ago() {
        use crate::i18n::Lang;

        // 中文（默认语言）：保持原来的写法。
        assert_eq!(format_uptime_in(Lang::Zh, 0), "0 分钟");
        assert_eq!(format_uptime_in(Lang::Zh, 59), "0 分钟");
        assert_eq!(format_uptime_in(Lang::Zh, 60), "1 分钟");
        assert_eq!(format_uptime_in(Lang::Zh, 3600), "1 小时 0 分钟");
        assert_eq!(format_uptime_in(Lang::Zh, 90_061), "1 天 1 小时 1 分钟");
        assert_eq!(format_ago_in(Lang::Zh, Duration::from_millis(300)), "刚刚");
        assert_eq!(format_ago_in(Lang::Zh, Duration::from_secs(5)), "5 秒前");
        assert_eq!(
            format_ago_in(Lang::Zh, Duration::from_secs(65)),
            "1 分 5 秒前"
        );
        assert_eq!(
            format_ago_in(Lang::Zh, Duration::from_secs(3700)),
            "1 小时 1 分前"
        );

        // 英文：紧凑单位，不出现 "+1 items" 那种复数错。
        assert_eq!(format_uptime_in(Lang::En, 0), "0m");
        assert_eq!(format_uptime_in(Lang::En, 60), "1m");
        assert_eq!(format_uptime_in(Lang::En, 3600), "1h 0m");
        assert_eq!(format_uptime_in(Lang::En, 90_061), "1d 1h 1m");
        assert_eq!(
            format_ago_in(Lang::En, Duration::from_millis(300)),
            "just now"
        );
        assert_eq!(format_ago_in(Lang::En, Duration::from_secs(5)), "5s ago");
        assert_eq!(
            format_ago_in(Lang::En, Duration::from_secs(65)),
            "1m 5s ago"
        );
        assert_eq!(format_ago_in(Lang::En, Duration::from_secs(3600)), "1h ago");
        assert_eq!(
            format_ago_in(Lang::En, Duration::from_secs(3700)),
            "1h 1m ago"
        );

        // 包一层：默认语言（测试里是中文）下公开函数和 _in 版本一致。
        assert_eq!(
            format_uptime(60),
            format_uptime_in(crate::i18n::current(), 60)
        );
        assert_eq!(
            format_ago(Duration::from_secs(5)),
            format_ago_in(crate::i18n::current(), Duration::from_secs(5))
        );
    }

    #[test]
    fn two_subscribers_share_one_poller() {
        let poller = FakePoller::shared();
        let hub = StatusHub::new(poller.clone());

        assert_eq!(hub.subscribe(3), 1);
        assert_eq!(hub.subscribe(3), 2);
        assert_eq!(poller.start_count(), 1, "第二个订阅者复用同一个轮询器");

        poller.report(0, 3, Ok(sample_status()));
        let snapshot = hub.snapshot(3).unwrap();
        assert_eq!(snapshot.revision, 1);
        assert_eq!(snapshot.status, Some(sample_status()));
        assert!(snapshot.error.is_none());

        hub.unsubscribe(3);
        assert!(poller.is_alive(0), "还有订阅者，轮询器不能停");
        hub.unsubscribe(3);
        assert!(!poller.is_alive(0), "最后一个订阅者走了就停");
        assert_eq!(hub.subscribers(3), 0);
    }

    #[test]
    fn failure_keeps_last_data_and_shows_the_error() {
        let poller = FakePoller::shared();
        let hub = StatusHub::new(poller.clone());
        hub.subscribe(3);

        poller.report(0, 3, Ok(sample_status()));
        poller.report(0, 3, Err("连接失败：超时".to_owned()));

        let snapshot = hub.snapshot(3).unwrap();
        assert_eq!(snapshot.status, Some(sample_status()), "失败时保留旧数据");
        assert_eq!(snapshot.error.as_deref(), Some("连接失败：超时"));
        assert_eq!(snapshot.revision, 2, "失败也算一次新结果");
    }

    #[test]
    fn the_hub_shows_a_speed_only_once_the_connection_has_two_samples() {
        let poller = FakePoller::shared();
        let hub = StatusHub::new(poller.clone());
        hub.subscribe(5);
        let sample = |has_previous: bool, rx: f64, tx: f64| {
            let mut status = sample_status();
            status.stats.net_rx_speed = rx;
            status.stats.net_tx_speed = tx;
            status.has_previous = has_previous;
            status
        };

        // 第一个样本：core 给的 0 不是「没有流量」，是「还算不出来」，不能当网速。
        poller.report(0, 5, Ok(sample(false, 0.0, 0.0)));
        assert_eq!(hub.snapshot(5).unwrap().net_rate, None);

        poller.report(0, 5, Ok(sample(true, 2_000.0, 500.0)));
        assert_eq!(hub.snapshot(5).unwrap().net_rate, Some((2_000.0, 500.0)));

        // 重新拨号（新采集器，又是第一个样本）：不拿上一条连接的数字冒充现在的。
        poller.report(0, 5, Ok(sample(false, 0.0, 0.0)));
        assert_eq!(hub.snapshot(5).unwrap().net_rate, None);
    }

    #[test]
    fn late_results_from_a_stopped_poller_are_dropped() {
        let poller = FakePoller::shared();
        let hub = StatusHub::new(poller.clone());
        hub.subscribe(9);
        hub.unsubscribe(9);
        assert!(!poller.is_alive(0));

        // 停止时还在路上的那条命令回来了：不能写进快照。
        poller.report(0, 9, Ok(sample_status()));
        let snapshot = hub.snapshot(9).unwrap();
        assert!(snapshot.status.is_none());
        assert_eq!(snapshot.revision, 0);
    }

    #[test]
    fn resubscribing_starts_a_fresh_poller() {
        let poller = FakePoller::shared();
        let hub = StatusHub::new(poller.clone());
        hub.subscribe(9);
        hub.unsubscribe(9);
        hub.subscribe(9);

        assert_eq!(poller.start_count(), 2, "停掉之后重新订阅要重新起一个");
        assert!(poller.is_alive(1));
        poller.report(1, 9, Ok(sample_status()));
        assert!(hub.snapshot(9).unwrap().status.is_some());
    }
}

/// V2-F5（D2-13）：轮询循环复用连接、失败退避、可随时停止。用假连接数"拨了几次号"，
/// 间隔缩到几十毫秒，不连网、不起真实 SSH。
#[cfg(test)]
mod poll_tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use zssh_core::ssh::BoxFuture;
    use zssh_core::ssh::stats::ServerStats;

    use super::{
        LinkPoller, PollHandle, Poller, ReportFn, RetryState, ServerStatus, StatusHub, StatusLink,
        Timing, poll_loop,
    };

    fn fast() -> Timing {
        Timing {
            interval: Duration::from_millis(20),
            max_backoff: Duration::from_millis(160),
            connect_timeout: Duration::from_millis(80),
            check: Duration::from_millis(5),
        }
    }

    fn status() -> ServerStatus {
        ServerStatus {
            stats: ServerStats {
                cpu_cores: Some(2),
                load_1m: Some(0.1),
                ..ServerStats::default()
            },
            has_previous: false,
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Dial {
        Ok,
        Fail,
        Hang,
    }

    /// 假连接：记录拨号次数 / 采集次数，连接是否还活着、拨号结果都可以改。
    struct FakeLink {
        dials: AtomicUsize,
        collects: AtomicUsize,
        alive: AtomicBool,
        dial: Mutex<Dial>,
        collect_fails: AtomicBool,
    }

    impl FakeLink {
        fn new(dial: Dial) -> Arc<Self> {
            Arc::new(Self {
                dials: AtomicUsize::new(0),
                collects: AtomicUsize::new(0),
                alive: AtomicBool::new(true),
                dial: Mutex::new(dial),
                collect_fails: AtomicBool::new(false),
            })
        }

        fn dials(&self) -> usize {
            self.dials.load(Ordering::SeqCst)
        }

        fn collects(&self) -> usize {
            self.collects.load(Ordering::SeqCst)
        }
    }

    impl StatusLink for FakeLink {
        type Conn = ();

        fn dial(&self, _host_id: i64) -> BoxFuture<'_, Result<(), String>> {
            self.dials.fetch_add(1, Ordering::SeqCst);
            let mode = *self.dial.lock().unwrap_or_else(|p| p.into_inner());
            Box::pin(async move {
                match mode {
                    Dial::Ok => {
                        self.alive.store(true, Ordering::SeqCst);
                        Ok(())
                    }
                    Dial::Fail => Err("连接失败：拒绝连接".to_owned()),
                    Dial::Hang => std::future::pending().await,
                }
            })
        }

        fn is_alive(&self, _conn: &()) -> bool {
            self.alive.load(Ordering::SeqCst)
        }

        fn collect<'a>(&'a self, _conn: &'a ()) -> BoxFuture<'a, Result<ServerStatus, String>> {
            self.collects.fetch_add(1, Ordering::SeqCst);
            let fails = self.collect_fails.load(Ordering::SeqCst);
            Box::pin(async move {
                if fails {
                    Err("执行状态命令失败：连接断了".to_owned())
                } else {
                    Ok(status())
                }
            })
        }
    }

    type Reports = Arc<Mutex<Vec<Result<ServerStatus, String>>>>;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime")
    }

    /// 起一个轮询循环；返回停止标志、结果记录和任务句柄。
    fn spawn_loop(
        runtime: &tokio::runtime::Runtime,
        link: &Arc<FakeLink>,
        timing: Timing,
    ) -> (Arc<AtomicBool>, Reports, tokio::task::JoinHandle<()>) {
        let stop = Arc::new(AtomicBool::new(false));
        let reports: Reports = Arc::default();
        let sink = Arc::clone(&reports);
        let report: ReportFn = Arc::new(move |_host, result| {
            sink.lock().unwrap_or_else(|p| p.into_inner()).push(result);
        });
        let (_wake, wakes) = std::sync::mpsc::channel();
        let handle = runtime.spawn(poll_loop(
            Arc::clone(link),
            timing,
            7,
            Arc::new(tokio::sync::Mutex::new(())),
            Arc::clone(&stop),
            wakes,
            report,
        ));
        // `_wake` 在这里被丢掉也没关系：发送端没了只是收不到「刷新」，不影响循环。
        (stop, reports, handle)
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "等不到：{what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn retry_delay_doubles_per_failure_caps_and_resets_on_success() {
        let timing = Timing::PRODUCTION;
        let mut retry = RetryState::default();
        assert_eq!(
            retry.delay(&timing),
            Duration::from_secs(2),
            "没失败是正常间隔"
        );
        let mut seen = Vec::new();
        for _ in 0..8 {
            retry.record(false);
            seen.push(retry.delay(&timing).as_secs());
        }
        assert_eq!(seen, vec![4, 8, 16, 32, 60, 60, 60, 60], "翻倍，封顶 60 秒");
        retry.record(true);
        assert_eq!(
            retry.delay(&timing),
            Duration::from_secs(2),
            "成功一次回到正常间隔"
        );
        // 极端失败次数不溢出。
        let mut many = RetryState::default();
        for _ in 0..100_000 {
            many.record(false);
        }
        assert_eq!(many.delay(&timing), Duration::from_secs(60));
    }

    #[test]
    fn one_connection_is_reused_for_every_poll() {
        let runtime = runtime();
        let link = FakeLink::new(Dial::Ok);
        let (stop, reports, handle) = spawn_loop(&runtime, &link, fast());
        // 等报告本身而不是采集计数：采集计数先加、报告后推，按计数停下时最后一份报告可能还没推进去。
        wait_until("至少报告 6 次", || {
            reports.lock().unwrap_or_else(|p| p.into_inner()).len() >= 6
        });
        stop.store(true, Ordering::SeqCst);
        runtime.block_on(handle).expect("循环正常结束");

        assert_eq!(link.dials(), 1, "连接要一直复用，不能每轮重新拨号");
        let reports = reports.lock().unwrap_or_else(|p| p.into_inner());
        assert!(reports.len() >= 6 && reports.iter().all(Result::is_ok));
    }

    #[test]
    fn a_dead_connection_is_redialled_once_not_every_round() {
        let runtime = runtime();
        let link = FakeLink::new(Dial::Ok);
        let (stop, _reports, handle) = spawn_loop(&runtime, &link, fast());
        wait_until("先采到数据", || link.collects() >= 2);
        // 连接断了：采集失败，下一轮重新拨一次，之后又复用新连接。
        link.collect_fails.store(true, Ordering::SeqCst);
        link.alive.store(false, Ordering::SeqCst);
        wait_until("重新拨号", || link.dials() >= 2);
        link.collect_fails.store(false, Ordering::SeqCst);
        let after = link.collects();
        wait_until("恢复后继续采", || link.collects() >= after + 4);
        stop.store(true, Ordering::SeqCst);
        runtime.block_on(handle).expect("循环正常结束");
        assert_eq!(link.dials(), 2, "断一次只重拨一次");
    }

    #[test]
    fn failing_connections_back_off_instead_of_redialling_every_interval() {
        let runtime = runtime();
        let link = FakeLink::new(Dial::Fail);
        let (stop, reports, handle) = spawn_loop(&runtime, &link, fast());
        std::thread::sleep(Duration::from_millis(700));
        stop.store(true, Ordering::SeqCst);
        runtime.block_on(handle).expect("循环正常结束");

        // 不退避的话 700ms / 20ms ≈ 35 次；退避 40/80/160/160… 只有七八次。
        let attempts = link.dials();
        assert!(
            (3..=10).contains(&attempts),
            "失败应当指数退避，实际拨了 {attempts} 次"
        );
        let reports = reports.lock().unwrap_or_else(|p| p.into_inner());
        assert!(
            reports
                .iter()
                .all(|result| matches!(result, Err(message) if message.contains("拒绝连接")))
        );
        assert_eq!(link.collects(), 0, "没连上就不采集");
    }

    #[test]
    fn a_hanging_connect_times_out_and_the_stop_flag_interrupts_it() {
        let runtime = runtime();
        let link = FakeLink::new(Dial::Hang);
        let (stop, reports, handle) = spawn_loop(&runtime, &link, fast());
        // 连接超时：报出原因，循环还活着继续试。
        wait_until("连接超时被报告", || {
            reports
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .any(|result| matches!(result, Err(message) if message.contains("连接超过")))
        });

        // 停止信号：正卡在拨号时也要马上退出，不用等超时。
        let mut slow = fast();
        slow.connect_timeout = Duration::from_secs(60);
        let hang = FakeLink::new(Dial::Hang);
        let (stop2, reports2, handle2) = spawn_loop(&runtime, &hang, slow);
        wait_until("开始拨号", || hang.dials() >= 1);
        let started = Instant::now();
        stop2.store(true, Ordering::SeqCst);
        runtime.block_on(handle2).expect("循环正常结束");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "停止信号要能打断拨号，实际等了 {:?}",
            started.elapsed()
        );
        assert!(
            reports2
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_empty(),
            "停止后不能再回报结果"
        );

        stop.store(true, Ordering::SeqCst);
        runtime.block_on(handle).expect("循环正常结束");
    }

    /// 窗口关闭 = 最后一个订阅者退订：后台循环要停，不再采集、不再拨号。
    #[test]
    fn the_last_unsubscribe_stops_the_background_loop() {
        let link = FakeLink::new(Dial::Ok);
        let poller: Arc<dyn Poller> = Arc::new(LinkPoller {
            link: Arc::clone(&link),
            timing: fast(),
        });
        let hub = StatusHub::new(poller);
        assert_eq!(hub.subscribe(3), 1);
        assert_eq!(hub.subscribe(3), 2, "第二个窗口共用同一个循环");
        wait_until("开始采集", || link.collects() >= 3);
        assert_eq!(link.dials(), 1, "两个订阅者只拨一次号");

        hub.unsubscribe(3);
        std::thread::sleep(Duration::from_millis(100));
        let still_running = link.collects();
        std::thread::sleep(Duration::from_millis(100));
        assert!(link.collects() > still_running, "还有一个订阅者，不能停");

        hub.unsubscribe(3);
        std::thread::sleep(Duration::from_millis(150));
        let stopped_at = link.collects();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(link.collects(), stopped_at, "最后一个订阅者走了循环必须停");
        assert_eq!(link.dials(), 1);
    }

    #[test]
    fn poll_handle_drop_sets_the_stop_flag() {
        let stop = Arc::new(AtomicBool::new(false));
        drop(PollHandle {
            stop: Arc::clone(&stop),
            wake: None,
        });
        assert!(stop.load(Ordering::SeqCst));
    }
}

/// 联网验证（D2-13）：在一条真实的已认证连接上反复采集，只拨一次号。
/// 需要 `ZSSH_LIVE_FULL=1` 和 `ZSSH_TEST_HOST/USER/KEY_PATH`（可选 `ZSSH_TEST_PORT`、
/// `ZSSH_TEST_SOCKS5`）；缺了就打印原因跳过。服务器只读：采集命令只读 `/proc` 和 `df`。
#[cfg(test)]
mod live_tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use zssh_core::model::ProxyKind;
    use zssh_core::ssh::{
        AuthConfig, BoxFuture, ChainHop, Connection, HostKeyDecision, HostKeyInfo, Interactor,
        KeySource, KnownHosts, PromptAnswers, PromptRequest, ProxyConfig,
    };

    use super::{ReportFn, ServerStatus, SshConn, StatusLink, Timing, collect_on, poll_loop};
    use crate::ssh_backend::SshSessionFactory;
    use zssh_core::ssh::stats::StatsCollector;

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

    /// 拨号只是把已经建好的真实连接递出去（并计数）；采集走真实的 `collect_on`。
    struct LiveLink {
        factory: Arc<SshSessionFactory>,
        dials: AtomicUsize,
    }

    impl StatusLink for LiveLink {
        type Conn = SshConn;

        fn dial(&self, _host_id: i64) -> BoxFuture<'_, Result<Self::Conn, String>> {
            self.dials.fetch_add(1, Ordering::SeqCst);
            let factory = Arc::clone(&self.factory);
            Box::pin(async move {
                Ok(SshConn {
                    factory,
                    collector: tokio::sync::Mutex::new(StatsCollector::new()),
                    samples: std::sync::atomic::AtomicU32::new(0),
                })
            })
        }

        fn is_alive(&self, conn: &Self::Conn) -> bool {
            !conn.factory.connection().is_closed()
        }

        fn collect<'a>(
            &'a self,
            conn: &'a Self::Conn,
        ) -> BoxFuture<'a, Result<ServerStatus, String>> {
            Box::pin(collect_on(conn))
        }
    }

    #[test]
    fn polling_a_real_server_reuses_one_authenticated_connection() {
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
        let proxy = env("ZSSH_TEST_SOCKS5").and_then(|raw| {
            let (host, port) = raw.rsplit_once(':')?;
            Some(ProxyConfig {
                kind: ProxyKind::Socks5,
                host: host.to_owned(),
                port: port.parse().ok()?,
                username: None,
                password: None,
            })
        });
        let known_hosts = std::env::temp_dir().join(format!("zssh-f5-kh-{}", std::process::id()));
        let hop = ChainHop {
            label: "测试服务器".to_owned(),
            host,
            port,
            username: user,
            proxy,
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
        let link = Arc::new(LiveLink {
            factory: SshSessionFactory::from_connection(connection),
            dials: AtomicUsize::new(0),
        });

        let timing = Timing {
            interval: Duration::from_millis(100),
            ..Timing::PRODUCTION
        };
        let stop = Arc::new(AtomicBool::new(false));
        let reports: Arc<Mutex<Vec<Result<ServerStatus, String>>>> = Arc::default();
        let sink = Arc::clone(&reports);
        let report: ReportFn = Arc::new(move |_host, result| {
            sink.lock().unwrap_or_else(|p| p.into_inner()).push(result);
        });
        let (_wake, wakes) = std::sync::mpsc::channel();
        let handle = runtime.spawn(poll_loop(
            Arc::clone(&link),
            timing,
            1,
            Arc::new(tokio::sync::Mutex::new(())),
            Arc::clone(&stop),
            wakes,
            report,
        ));
        let deadline = Instant::now() + Duration::from_secs(60);
        while reports.lock().unwrap_or_else(|p| p.into_inner()).len() < 5 {
            assert!(Instant::now() < deadline, "60 秒内应当采到 5 次");
            std::thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::SeqCst);
        runtime.block_on(handle).expect("循环正常结束");

        let reports = reports.lock().unwrap_or_else(|p| p.into_inner());
        for (index, result) in reports.iter().enumerate() {
            let status = result.as_ref().expect("每次采集都应成功");
            let stats = &status.stats;
            assert!(stats.cpu_cores.unwrap_or(0) > 0 && stats.mem_total_mib.unwrap_or(0) > 0);
            // 第一次采样没有上一次的计数，之后每次都有（CPU 使用率、每个核心、网速都是差值）。
            assert_eq!(status.has_previous, index > 0, "第 {index} 次");
            if index > 0 {
                assert!(
                    stats.cpu_percent.is_some(),
                    "第 {index} 次应当有 CPU 使用率"
                );
                assert_eq!(
                    stats.per_core_percent.len(),
                    stats.cpu_cores.unwrap_or(0) as usize
                );
            }
            // 0.2.14 概览里的这几节，只要服务器有就不能是空的。
            assert!(!stats.mounts.is_empty(), "全部分区");
            assert!(!stats.net_interfaces.is_empty(), "网卡");
            assert!(!stats.top_processes.is_empty(), "占内存最多的进程");
            assert!(
                !stats.os_name.is_empty() && !stats.kernel.is_empty(),
                "系统 / 内核"
            );
        }
        assert_eq!(
            link.dials.load(Ordering::SeqCst),
            1,
            "{} 次采集只能用同一条已认证连接",
            reports.len()
        );
        let _ = std::fs::remove_file(&known_hosts);
    }
}

#[cfg(test)]
mod bar_tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{
        Level, ServerStatus, StatusSummary, level_of, mib_gb_label, percent_f, uptime_label,
    };
    use zssh_core::ssh::stats::ServerStats;

    fn status(load1: f64, cores: u32) -> ServerStatus {
        ServerStatus {
            stats: ServerStats {
                cpu_cores: Some(cores),
                cpu_percent: Some(37),
                load_1m: Some(load1),
                mem_total_mib: Some(2000),
                mem_used_mib: Some(1500),
                ..ServerStats::default()
            },
            has_previous: true,
        }
    }

    #[test]
    fn the_bar_colours_switch_at_80_and_95() {
        assert_eq!(level_of(0), Level::Normal);
        assert_eq!(level_of(79), Level::Normal);
        assert_eq!(level_of(80), Level::Warn);
        assert_eq!(level_of(94), Level::Warn);
        assert_eq!(level_of(95), Level::Critical);
        assert_eq!(level_of(100), Level::Critical);
    }

    #[test]
    fn the_summary_uses_the_measured_cpu_and_folds_load_per_core() {
        let summary = StatusSummary::from_status(&status(1.0, 4), Some((1500.0, 20.0)));
        // CPU 是真实采样（/proc/stat 差值），不再拿负载估。
        assert_eq!(summary.stats.cpu_percent, Some(37));
        assert_eq!(summary.mem_percent(), Some(75));
        assert_eq!(summary.load_percent(), Some(25));
        // 负载超过核数也不能画到框外，核数为 0 的脏数据不能除零。
        assert_eq!(
            StatusSummary::from_status(&status(9.0, 2), None).load_percent(),
            Some(100)
        );
        assert_eq!(
            StatusSummary::from_status(&status(1.0, 0), None).load_percent(),
            None
        );
        assert_eq!(percent_f(f64::NAN), 0);
        assert_eq!(percent_f(-3.0), 0);
        // 没有总量就没有百分比，不是 0%。
        let mut empty = status(1.0, 2);
        empty.stats.mem_total_mib = None;
        assert_eq!(StatusSummary::from_status(&empty, None).mem_percent(), None);
    }

    #[test]
    fn sizes_and_uptime_read_like_the_old_front_end() {
        assert_eq!(mib_gb_label(Some(1536.0), 1), "1.5GB");
        assert_eq!(mib_gb_label(Some(10_240.0), 0), "10GB");
        assert_eq!(mib_gb_label(None, 1), "--");
        assert_eq!(mib_gb_label(Some(f64::NAN), 1), "--");
        assert_eq!(uptime_label(59), "0 分钟");
        assert_eq!(uptime_label(3_700), "1 小时 1 分");
        assert_eq!(uptime_label(90_061), "1 天 1 小时");
    }
}
