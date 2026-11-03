//! 终端右上角的延迟徽标：怎么测、怎么统计、怎么取色档、怎么写成文字。界面只负责显示。
//!
//! 测什么：
//! - **SSH**：在这个标签已有的 SSH 连接上发一个全局请求（`keepalive@openssh.com`，要回复），服务器回
//!   成功或失败都算一次往返，所以测到的是经过代理 / 跳板之后、到服务器 sshd 的真实应用层往返；
//!   不新建 TCP 连接，也不碰终端数据。russh 自带的 keepalive 定时器不暴露往返时间，所以这里自己发；
//!   我们的请求回复也算"收到过数据"，会让 russh 的 30 秒定时器顺延，两者不会互相打架。
//! - **Telnet**：没有协议级往返，只能对同一地址端口单独做 TCP connect 计时（经代理时只测得到代理）。
//!   经 SSH 跳板机的 Telnet 测不到"到目标的真实延迟"，诚实起见不显示。
//! - **串口**：不显示。
//!
//! 纯逻辑（统计、色档、文字、暂停恢复规则、各协议测不测）都在这里并有单元测试；后台任务
//! （[`LatencyMonitor::spawn`]）只是把这些规则按时间跑起来。

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tokio::time::timeout;

use crate::ssh::connection::Connection;
use crate::ssh::interact::BoxFuture;

/// SSH 往返的测量间隔（标签可见时）。
pub const SSH_INTERVAL: Duration = Duration::from_secs(5);
/// Telnet 的 TCP connect 间隔：每次都是一条新连接，比 SSH 稀一点。
pub const TCP_INTERVAL: Duration = Duration::from_secs(10);
/// 单次超时：超过就记一次"超时"。
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// 统计窗口：最近多少次。
pub const WINDOW: usize = 12;
/// 低于它算好（绿）。
pub const GOOD_BELOW: Duration = Duration::from_millis(100);
/// 低于它算一般（琥珀）；再慢和超时都是红。
pub const WARN_BELOW: Duration = Duration::from_millis(300);

// ---------------------------------------------------------------- 样本与统计

/// 一次测量的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sample {
    Rtt(Duration),
    Timeout,
}

/// 最近 [`WINDOW`] 次的结果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LatencyStats {
    samples: VecDeque<Sample>,
}

impl LatencyStats {
    pub fn record(&mut self, sample: Sample) {
        if self.samples.len() == WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// 最近一次（可能是超时）。
    pub fn last(&self) -> Option<Sample> {
        self.samples.back().copied()
    }

    fn rtts(&self) -> impl Iterator<Item = Duration> + '_ {
        self.samples.iter().filter_map(|sample| match sample {
            Sample::Rtt(rtt) => Some(*rtt),
            Sample::Timeout => None,
        })
    }

    /// 窗口内成功次数的平均往返；一次都没成功是 `None`。
    pub fn average(&self) -> Option<Duration> {
        let (sum, count) = self
            .rtts()
            .fold((Duration::ZERO, 0u32), |(sum, count), rtt| {
                (sum + rtt, count + 1)
            });
        (count > 0).then(|| sum / count)
    }

    /// 抖动：相邻两次成功往返之差的绝对值的平均（超时的那些次跳过）；成功不到两次是 `None`。
    pub fn jitter(&self) -> Option<Duration> {
        let rtts: Vec<Duration> = self.rtts().collect();
        if rtts.len() < 2 {
            return None;
        }
        let total: Duration = rtts.windows(2).map(|pair| pair[0].abs_diff(pair[1])).sum();
        Some(total / (rtts.len() as u32 - 1))
    }

    /// （超时次数, 窗口内总次数）。
    pub fn lost(&self) -> (usize, usize) {
        let lost = self
            .samples
            .iter()
            .filter(|sample| matches!(sample, Sample::Timeout))
            .count();
        (lost, self.samples.len())
    }
}

// ---------------------------------------------------------------- 色档与文字

/// 徽标颜色档：界面按它取皮肤里的绿 / 琥珀 / 红。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Good,
    Warn,
    Bad,
}

pub fn level(sample: Sample) -> Level {
    match sample {
        Sample::Rtt(rtt) if rtt < GOOD_BELOW => Level::Good,
        Sample::Rtt(rtt) if rtt < WARN_BELOW => Level::Warn,
        Sample::Rtt(_) | Sample::Timeout => Level::Bad,
    }
}

/// 徽标上的文字：`<1ms`、`NNms`、`1.2s`（1 秒及以上）、`超时`。
pub fn format_sample(sample: Sample) -> String {
    let Sample::Rtt(rtt) = sample else {
        return "超时".to_owned();
    };
    let micros = rtt.as_micros();
    if micros < 1_000 {
        return "<1ms".to_owned();
    }
    // 先四舍五入到毫秒再判断是不是进了秒：999.6ms 不能写成 1000ms。
    let millis = (micros + 500) / 1_000;
    if millis < 1_000 {
        return format!("{millis}ms");
    }
    let seconds = millis as f64 / 1_000.0;
    if seconds < 10.0 {
        format!("{seconds:.1}s")
    } else {
        format!("{seconds:.0}s")
    }
}

/// 悬停提示里的"N ms"。
pub fn format_ms(duration: Duration) -> String {
    let micros = duration.as_micros();
    if micros < 1_000 {
        "<1 ms".to_owned()
    } else {
        format!("{} ms", (micros + 500) / 1_000)
    }
}

fn format_last(sample: Option<Sample>) -> String {
    match sample {
        Some(Sample::Rtt(rtt)) => format_ms(rtt),
        Some(Sample::Timeout) => "超时".to_owned(),
        None => "—".to_owned(),
    }
}

/// 悬停提示：`到 <主机> 的往返延迟（SSH）· 最近 N ms · 平均 N ms · 抖动 N ms · 丢失 N/N`。
/// Telnet 写成 TCP 连接延迟；经代理时写明只测到代理。还没有任何样本时是"正在测量…"。
pub fn tooltip(host: &str, kind: ProbeKind, stats: &LatencyStats) -> String {
    let head = match kind {
        ProbeKind::Ssh => format!("到 {host} 的往返延迟（SSH）"),
        ProbeKind::Tcp { via_proxy: false } => format!("到 {host} 的 TCP 连接延迟（Telnet）"),
        ProbeKind::Tcp { via_proxy: true } => {
            "到代理的 TCP 连接延迟（Telnet，只测到代理，不含代理到目标）".to_owned()
        }
    };
    if stats.is_empty() {
        return format!("{head} · 正在测量…");
    }
    let (lost, total) = stats.lost();
    let dash = |value: Option<Duration>| value.map_or_else(|| "—".to_owned(), format_ms);
    format!(
        "{head} · 最近 {} · 平均 {} · 抖动 {} · 丢失 {lost}/{total}",
        format_last(stats.last()),
        dash(stats.average()),
        dash(stats.jitter()),
    )
}

// ---------------------------------------------------------------- 测什么、什么时候测

/// 用什么办法测。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeKind {
    /// 已有 SSH 连接上的 keepalive 往返。
    Ssh,
    /// 对目标（或代理）单独做 TCP connect 计时。
    Tcp { via_proxy: bool },
}

impl ProbeKind {
    pub fn interval(self) -> Duration {
        match self {
            Self::Ssh => SSH_INTERVAL,
            Self::Tcp { .. } => TCP_INTERVAL,
        }
    }
}

/// 一个终端窗格是怎么连上的（只为决定测不测、怎么测）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Ssh,
    TelnetDirect,
    TelnetProxy,
    /// Telnet 经 SSH 跳板机的 `direct-tcpip`：到目标那一段没有办法单独测。
    TelnetJump,
    Serial,
}

/// 这种连接要不要显示延迟、用什么办法。
pub fn probe_kind_for(transport: Transport) -> Option<ProbeKind> {
    match transport {
        Transport::Ssh => Some(ProbeKind::Ssh),
        Transport::TelnetDirect => Some(ProbeKind::Tcp { via_proxy: false }),
        Transport::TelnetProxy => Some(ProbeKind::Tcp { via_proxy: true }),
        Transport::TelnetJump | Transport::Serial => None,
    }
}

/// 后台任务下一步该做什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// 连接已断：停止，不再测。
    Stop,
    /// 没有可见的窗格（标签在后台 / 窗口最小化 / 不在终端页）：暂停，等恢复。
    Paused,
    /// 过这么久再测；0 表示现在就测。
    After(Duration),
}

/// 暂停 / 恢复 / 间隔的全部规则：
/// 断开优先于一切；不可见就暂停；可见时距上一次测量满一个间隔才测（刚恢复可见、上一次已经很久以前
/// 就立刻测；从来没测过也立刻测）。
pub fn next_wait(
    gone: bool,
    visible: bool,
    since_last: Option<Duration>,
    interval: Duration,
) -> Wait {
    if gone {
        Wait::Stop
    } else if !visible {
        Wait::Paused
    } else {
        Wait::After(since_last.map_or(Duration::ZERO, |since| interval.saturating_sub(since)))
    }
}

// ---------------------------------------------------------------- 探测

/// 一次探测的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    Done(Sample),
    /// 连接已经没了（对端断开 / 连接被关）：任务该结束。
    Gone,
}

/// 能做一次测量的东西。
pub trait Probe: Send + Sync + 'static {
    /// 目标已经没了吗（连接关了）。
    fn is_gone(&self) -> bool;
    /// 测一次。必须自己带超时。
    fn run(&self) -> BoxFuture<'_, ProbeOutcome>;
}

/// SSH 用的探测：在已有连接上发 keepalive 往返。只拿连接的弱引用。
pub struct SshProbe {
    connection: Weak<Connection>,
}

impl SshProbe {
    pub fn new(connection: Weak<Connection>) -> Self {
        Self { connection }
    }
}

impl Probe for SshProbe {
    fn is_gone(&self) -> bool {
        self.connection
            .upgrade()
            .is_none_or(|connection| connection.is_closed())
    }

    fn run(&self) -> BoxFuture<'_, ProbeOutcome> {
        Box::pin(async move {
            let Some(connection) = self.connection.upgrade() else {
                return ProbeOutcome::Gone;
            };
            match connection.measure_rtt(PROBE_TIMEOUT).await {
                Ok(Some(rtt)) => ProbeOutcome::Done(Sample::Rtt(rtt)),
                Ok(None) => ProbeOutcome::Done(Sample::Timeout),
                Err(_) => ProbeOutcome::Gone,
            }
        })
    }
}

/// 对一个地址做 TCP connect 并计时（连上立刻关）。超时返回 `None`，连不上返回错误。
pub async fn tcp_connect_rtt(
    addr: SocketAddr,
    limit: Duration,
) -> Result<Option<Duration>, std::io::Error> {
    let started = Instant::now();
    match timeout(limit, tokio::net::TcpStream::connect(addr)).await {
        Ok(Ok(stream)) => {
            let rtt = started.elapsed();
            drop(stream);
            Ok(Some(rtt))
        }
        Ok(Err(err)) => Err(err),
        Err(_) => Ok(None),
    }
}

/// Telnet 用的探测：对同一地址端口（经代理时是代理）做 TCP connect 计时。
///
/// 域名只解析一次并记住：测的是连接往返，不该把每次 DNS 的抖动算进去。
pub struct TcpProbe {
    host: String,
    port: u16,
    resolved: Mutex<Option<SocketAddr>>,
}

impl TcpProbe {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            resolved: Mutex::new(None),
        }
    }

    async fn address(&self) -> Option<SocketAddr> {
        if let Some(addr) = *self.resolved.lock().unwrap_or_else(|p| p.into_inner()) {
            return Some(addr);
        }
        let target = format!("{}:{}", self.host, self.port);
        let found = timeout(PROBE_TIMEOUT, tokio::net::lookup_host(target))
            .await
            .ok()?
            .ok()?
            .next()?;
        *self.resolved.lock().unwrap_or_else(|p| p.into_inner()) = Some(found);
        Some(found)
    }
}

impl Probe for TcpProbe {
    fn is_gone(&self) -> bool {
        false
    }

    fn run(&self) -> BoxFuture<'_, ProbeOutcome> {
        Box::pin(async move {
            // 解析不出来、连不上、超时都是"这次没通"，记超时；会话断没断由窗格自己的状态管。
            let Some(addr) = self.address().await else {
                return ProbeOutcome::Done(Sample::Timeout);
            };
            match tcp_connect_rtt(addr, PROBE_TIMEOUT).await {
                Ok(Some(rtt)) => ProbeOutcome::Done(Sample::Rtt(rtt)),
                Ok(None) | Err(_) => ProbeOutcome::Done(Sample::Timeout),
            }
        })
    }
}

// ---------------------------------------------------------------- 监视器

struct State {
    stats: LatencyStats,
    last_at: Option<tokio::time::Instant>,
    stopped: bool,
}

/// 某条连接（或某个 Telnet 会话）的延迟统计 + 后台测量任务的开关。
///
/// 一条 SSH 连接上有几个终端窗格，它们共用同一个监视器（同一个数值）；每个窗格拿一个
/// [`LatencyViewer`] 报告自己现在看不看得见，一个看得见的都没有时后台任务暂停。
pub struct LatencyMonitor {
    kind: ProbeKind,
    state: Mutex<State>,
    /// 现在看得见的窗格数。
    viewers: AtomicUsize,
    /// 每次状态变化加一：界面比较它决定要不要重画。
    version: AtomicU64,
    notify: Arc<Notify>,
    started: std::sync::atomic::AtomicBool,
}

/// 界面读到的一份快照。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LatencySnapshot {
    pub kind: ProbeKind,
    pub stats: LatencyStats,
    /// 后台任务已经因为连接断开而结束。
    pub stopped: bool,
}

impl LatencyMonitor {
    pub fn new(kind: ProbeKind) -> Arc<Self> {
        Arc::new(Self {
            kind,
            state: Mutex::new(State {
                stats: LatencyStats::default(),
                last_at: None,
                stopped: false,
            }),
            viewers: AtomicUsize::new(0),
            version: AtomicU64::new(0),
            notify: Arc::new(Notify::new()),
            started: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn kind(&self) -> ProbeKind {
        self.kind
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 记一次结果（后台任务调用；调试钩子和测试也用它摆值）。
    pub fn record(&self, sample: Sample) {
        {
            let mut state = self.lock();
            state.stats.record(sample);
            state.last_at = Some(tokio::time::Instant::now());
        }
        self.version.fetch_add(1, Ordering::Release);
    }

    pub fn snapshot(&self) -> LatencySnapshot {
        let state = self.lock();
        LatencySnapshot {
            kind: self.kind,
            stats: state.stats.clone(),
            stopped: state.stopped,
        }
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// 现在有几个窗格看得见。
    pub fn visible_viewers(&self) -> usize {
        self.viewers.load(Ordering::SeqCst)
    }

    fn mark_stopped(&self) {
        self.lock().stopped = true;
        self.version.fetch_add(1, Ordering::Release);
    }

    fn since_last(&self) -> Option<Duration> {
        self.lock().last_at.map(|at| at.elapsed())
    }

    /// 登记一个窗格；一开始是"看不见"。
    pub fn viewer(self: &Arc<Self>) -> LatencyViewer {
        LatencyViewer {
            monitor: Arc::clone(self),
            visible: false,
        }
    }

    /// 起后台任务（要在 tokio 运行时里调用；没有运行时返回 `false`）。每个监视器只起一次。
    pub fn spawn(self: &Arc<Self>, probe: impl Probe) -> bool {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return false;
        };
        if self.started.swap(true, Ordering::SeqCst) {
            return true;
        }
        runtime.spawn(run_loop(
            Arc::downgrade(self),
            Arc::clone(&self.notify),
            probe,
        ));
        true
    }

    /// 任务有没有起过（测试用）。
    pub fn is_started(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }
}

/// 一个窗格对监视器的"我看得见 / 看不见"登记。丢掉它就等于看不见了。
pub struct LatencyViewer {
    monitor: Arc<LatencyMonitor>,
    visible: bool,
}

impl LatencyViewer {
    pub fn monitor(&self) -> &Arc<LatencyMonitor> {
        &self.monitor
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// 看得见 / 看不见变了才动监视器；刚变成看得见时叫醒后台任务（上一次已经很久就立刻测）。
    pub fn set_visible(&mut self, visible: bool) {
        if visible == self.visible {
            return;
        }
        self.visible = visible;
        if visible {
            self.monitor.viewers.fetch_add(1, Ordering::SeqCst);
            // `notify_one` 会留一个许可，任务就算还没走到等待那一步也不会错过。
            self.monitor.notify.notify_one();
        } else {
            self.monitor.viewers.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl Drop for LatencyViewer {
    fn drop(&mut self) {
        self.set_visible(false);
    }
}

/// 后台任务：只拿监视器的弱引用，窗格都没了（监视器被丢掉）就自己结束。
async fn run_loop(monitor: Weak<LatencyMonitor>, notify: Arc<Notify>, probe: impl Probe) {
    loop {
        let Some(strong) = monitor.upgrade() else {
            return;
        };
        let interval = strong.kind.interval();
        let wait = next_wait(
            probe.is_gone() || strong.lock().stopped,
            strong.visible_viewers() > 0,
            strong.since_last(),
            interval,
        );
        // 等待期间不拿强引用：窗格关了监视器能立刻被回收。
        drop(strong);
        match wait {
            Wait::Stop => {
                if let Some(strong) = monitor.upgrade() {
                    strong.mark_stopped();
                }
                return;
            }
            // 暂停时每个间隔也醒一次：看看监视器还在不在、连接还在不在。
            Wait::Paused => {
                let _ = timeout(interval, notify.notified()).await;
            }
            Wait::After(delay) if !delay.is_zero() => {
                let _ = timeout(delay, notify.notified()).await;
            }
            Wait::After(_) => match probe.run().await {
                ProbeOutcome::Done(sample) => {
                    let Some(strong) = monitor.upgrade() else {
                        return;
                    };
                    strong.record(sample);
                }
                ProbeOutcome::Gone => {
                    if let Some(strong) = monitor.upgrade() {
                        strong.mark_stopped();
                    }
                    return;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn stats_of(samples: &[Sample]) -> LatencyStats {
        let mut stats = LatencyStats::default();
        for sample in samples {
            stats.record(*sample);
        }
        stats
    }

    #[test]
    fn statistics_cover_last_average_jitter_and_loss() {
        let stats = stats_of(&[
            Sample::Rtt(ms(10)),
            Sample::Rtt(ms(30)),
            Sample::Timeout,
            Sample::Rtt(ms(20)),
        ]);
        assert_eq!(stats.last(), Some(Sample::Rtt(ms(20))));
        assert_eq!(stats.average(), Some(ms(20)));
        // 超时的那次跳过：|30-10| 和 |20-30| 的平均。
        assert_eq!(stats.jitter(), Some(ms(15)));
        assert_eq!(stats.lost(), (1, 4));
    }

    #[test]
    fn empty_and_all_lost_windows_have_no_average_or_jitter() {
        let empty = LatencyStats::default();
        assert_eq!(
            (empty.last(), empty.average(), empty.jitter(), empty.lost()),
            (None, None, None, (0, 0))
        );
        let lost = stats_of(&[Sample::Timeout, Sample::Timeout]);
        assert_eq!(lost.average(), None);
        assert_eq!(lost.jitter(), None);
        assert_eq!(lost.lost(), (2, 2));
        // 只有一次成功：没有"相邻两次"可比。
        assert_eq!(stats_of(&[Sample::Rtt(ms(5))]).jitter(), None);
    }

    #[test]
    fn the_window_keeps_only_the_last_twelve() {
        let mut stats = LatencyStats::default();
        stats.record(Sample::Timeout);
        for _ in 0..WINDOW {
            stats.record(Sample::Rtt(ms(7)));
        }
        assert_eq!(stats.len(), WINDOW);
        assert_eq!(stats.lost(), (0, WINDOW), "最早那次超时已经滑出窗口");
    }

    #[test]
    fn colour_tiers_switch_at_100_and_300_ms() {
        assert_eq!(level(Sample::Rtt(ms(0))), Level::Good);
        assert_eq!(level(Sample::Rtt(ms(99))), Level::Good);
        assert_eq!(level(Sample::Rtt(ms(100))), Level::Warn);
        assert_eq!(level(Sample::Rtt(ms(299))), Level::Warn);
        assert_eq!(level(Sample::Rtt(ms(300))), Level::Bad);
        assert_eq!(level(Sample::Timeout), Level::Bad);
    }

    #[test]
    fn badge_text_is_ms_then_seconds_then_timeout() {
        let text = |d: Duration| format_sample(Sample::Rtt(d));
        assert_eq!(text(Duration::from_micros(400)), "<1ms");
        assert_eq!(text(ms(1)), "1ms");
        assert_eq!(text(ms(99)), "99ms");
        assert_eq!(text(Duration::from_micros(99_600)), "100ms");
        assert_eq!(text(ms(999)), "999ms");
        assert_eq!(
            text(Duration::from_micros(999_600)),
            "1.0s",
            "四舍五入进了 1000ms 就写成秒"
        );
        assert_eq!(text(ms(1000)), "1.0s");
        assert_eq!(text(ms(1234)), "1.2s");
        assert_eq!(text(ms(12_400)), "12s");
        assert_eq!(format_sample(Sample::Timeout), "超时");
    }

    #[test]
    fn the_tooltip_lists_last_average_jitter_and_loss() {
        let stats = stats_of(&[Sample::Rtt(ms(10)), Sample::Timeout, Sample::Rtt(ms(30))]);
        assert_eq!(
            tooltip("web-1", ProbeKind::Ssh, &stats),
            "到 web-1 的往返延迟（SSH） · 最近 30 ms · 平均 20 ms · 抖动 20 ms · 丢失 1/3"
        );
        assert_eq!(
            tooltip("sw-1", ProbeKind::Tcp { via_proxy: false }, &stats),
            "到 sw-1 的 TCP 连接延迟（Telnet） · 最近 30 ms · 平均 20 ms · 抖动 20 ms · 丢失 1/3"
        );
        assert!(tooltip("sw-1", ProbeKind::Tcp { via_proxy: true }, &stats).starts_with("到代理的"));
        assert!(tooltip("a", ProbeKind::Ssh, &LatencyStats::default()).ends_with("正在测量…"));
        // 最近一次超时、还没有成功过：不写出假数字。
        let lost = stats_of(&[Sample::Timeout]);
        let text = tooltip("a", ProbeKind::Ssh, &lost);
        assert!(text.contains("最近 超时"), "{text}");
        assert!(
            text.contains("平均 —") && text.contains("丢失 1/1"),
            "{text}"
        );
    }

    #[test]
    fn only_ssh_and_direct_or_proxied_telnet_show_a_latency() {
        assert_eq!(probe_kind_for(Transport::Ssh), Some(ProbeKind::Ssh));
        assert_eq!(
            probe_kind_for(Transport::TelnetDirect),
            Some(ProbeKind::Tcp { via_proxy: false })
        );
        assert_eq!(
            probe_kind_for(Transport::TelnetProxy),
            Some(ProbeKind::Tcp { via_proxy: true })
        );
        assert_eq!(probe_kind_for(Transport::TelnetJump), None);
        assert_eq!(probe_kind_for(Transport::Serial), None, "串口不显示");
        assert_eq!(ProbeKind::Ssh.interval(), Duration::from_secs(5));
        assert_eq!(
            ProbeKind::Tcp { via_proxy: false }.interval(),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn pause_resume_and_stop_rules() {
        let five = SSH_INTERVAL;
        // 断开优先于一切。
        assert_eq!(next_wait(true, true, None, five), Wait::Stop);
        assert_eq!(next_wait(true, false, Some(ms(1)), five), Wait::Stop);
        // 看不见（后台标签 / 最小化）：暂停，不管多久没测。
        assert_eq!(next_wait(false, false, None, five), Wait::Paused);
        assert_eq!(
            next_wait(false, false, Some(Duration::from_secs(60)), five),
            Wait::Paused
        );
        // 看得见：从没测过就立刻测；到点才测；刚恢复可见而上一次早就过期也立刻测。
        assert_eq!(
            next_wait(false, true, None, five),
            Wait::After(Duration::ZERO)
        );
        assert_eq!(
            next_wait(false, true, Some(ms(1500)), five),
            Wait::After(ms(3500))
        );
        assert_eq!(
            next_wait(false, true, Some(Duration::from_secs(60)), five),
            Wait::After(Duration::ZERO)
        );
    }

    #[test]
    fn a_viewer_counts_only_while_visible_and_releases_on_drop() {
        let monitor = LatencyMonitor::new(ProbeKind::Ssh);
        let mut a = monitor.viewer();
        let mut b = monitor.viewer();
        assert_eq!(monitor.visible_viewers(), 0, "登记之后默认看不见");
        a.set_visible(true);
        a.set_visible(true);
        assert_eq!(monitor.visible_viewers(), 1, "重复设置不叠加");
        b.set_visible(true);
        assert_eq!(monitor.visible_viewers(), 2, "同一连接上两个窗格各算一个");
        a.set_visible(false);
        assert_eq!(monitor.visible_viewers(), 1);
        drop(b);
        assert_eq!(monitor.visible_viewers(), 0, "窗格被关掉等于看不见");
    }

    #[test]
    fn recording_bumps_the_version_the_ui_polls() {
        let monitor = LatencyMonitor::new(ProbeKind::Ssh);
        let before = monitor.version();
        monitor.record(Sample::Rtt(ms(12)));
        assert_ne!(monitor.version(), before);
        let snap = monitor.snapshot();
        assert_eq!(snap.stats.last(), Some(Sample::Rtt(ms(12))));
        assert!(!snap.stopped);
    }

    /// 按脚本回结果的假探测，数调用次数。
    struct Scripted {
        calls: Arc<AtomicUsize>,
        gone_after: usize,
    }

    impl Probe for Scripted {
        fn is_gone(&self) -> bool {
            self.calls.load(Ordering::SeqCst) >= self.gone_after
        }

        fn run(&self) -> BoxFuture<'_, ProbeOutcome> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                ProbeOutcome::Done(Sample::Rtt(Duration::from_millis(5)))
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_task_measures_only_while_someone_is_watching_and_stops_when_gone() {
        let monitor = LatencyMonitor::new(ProbeKind::Ssh);
        let calls = Arc::new(AtomicUsize::new(0));
        assert!(monitor.spawn(Scripted {
            calls: Arc::clone(&calls),
            gone_after: 4,
        }));
        assert!(monitor.spawn(Scripted {
            calls: Arc::clone(&calls),
            gone_after: 4,
        }));
        // 没人看：过多久都不测。
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0, "暂停时不测");

        // 看得见：立刻测一次，之后每 5 秒一次。
        let mut viewer = monitor.viewer();
        viewer.set_visible(true);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::sleep(SSH_INTERVAL).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // 又看不见：停住；恢复可见（上一次已经过期）立刻补测一次。
        viewer.set_visible(false);
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        viewer.set_visible(true);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        // 连接断了（第 4 次之后）：任务标记停止并结束，不再测。
        tokio::time::sleep(SSH_INTERVAL).await;
        tokio::time::sleep(SSH_INTERVAL).await;
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert!(monitor.snapshot().stopped);
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_monitor_ends_the_task() {
        let calls = Arc::new(AtomicUsize::new(0));
        let monitor = LatencyMonitor::new(ProbeKind::Ssh);
        monitor.spawn(Scripted {
            calls: Arc::clone(&calls),
            gone_after: usize::MAX,
        });
        let mut viewer = monitor.viewer();
        viewer.set_visible(true);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        drop(viewer);
        drop(monitor);
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "监视器没了任务不再测");
    }

    #[tokio::test]
    async fn the_tcp_probe_times_a_connect_to_a_local_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { while listener.accept().await.is_ok() {} });
        let probe = TcpProbe::new("127.0.0.1", port);
        for _ in 0..3 {
            match probe.run().await {
                ProbeOutcome::Done(Sample::Rtt(rtt)) => {
                    assert!(rtt < Duration::from_secs(2), "{rtt:?}");
                }
                other => panic!("本机监听应当连得上：{other:?}"),
            }
        }
        // 端口没人监听：这次没通，记超时（不是崩、不是 Gone）。
        let closed = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        assert_eq!(
            TcpProbe::new("127.0.0.1", closed).run().await,
            ProbeOutcome::Done(Sample::Timeout)
        );
    }
}
