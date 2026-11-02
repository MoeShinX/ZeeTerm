//! ZeeTerm 的端口转发运行时：每条开着的规则一个后台任务，任务持有一份池里的连接引用——转发
//! 开着，这条 SSH 连接就不会因为终端标签全关了而被池子回收。
//!
//! 生命周期语义和 Tauri 入口（`src-tauri/src/commands/forward.rs`，V2-F6 修过）是同一套：
//! - 每次启动的登记 key 带代次（`forward:{id}:{n}`）：停掉的旧任务还没从登记表里摘除时，同一个
//!   key 马上重新登记会被拒绝，编辑运行中的规则后转发被停掉就是这么来的；
//! - 停止 / 重启 / 删除 / 开启串行执行（`lifecycle` 锁）：保存规则时的「停旧再开新」是个有等待的
//!   多步操作，中间不能被另一次保存或点停止插进来；
//! - 重启在锁内：停旧 → `wait_released` 等旧任务真正结束（端口放掉）→ 按库里最新的规则开新，
//!   连续快速编辑排队执行，最后只剩一个实例；
//! - 退出时运行层统一取消，不用等超时。
//!
//! **为什么没有抽到 `zssh-app-runtime` 让两边共用**：Tauri 那份和 `AppState`（连接池、取消信号）、
//! `AppHandle`（状态事件）缠在一起，ZeeTerm 的连接池（`ssh_backend`）和提问（`ssh_interact`）
//! 是另一套；硬抽成泛型要同时改动 `src-tauri` 和根 workspace 的门禁，超出本卡范围。这里复用了
//! 真正承担并发语义的部分（`TaskRegistry` 的登记、`wait_released`、统一取消）和 core 的
//! `ssh::forward`（监听、转发、计数），只把「谁来给连接、怎么通知界面」换成原生的写法，并把
//! Tauri 那组并发测试原样带了过来。两份实现以后要合并时，从这里的测试开始对照。

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::task::AbortHandle;
use zssh_app_runtime::TaskRegistry;
use zssh_core::ssh::BoxFuture;
use zssh_core::ssh::connection::Connection;
use zssh_core::ssh::forward::{self, ForwardCounters, ForwardStats};
use zssh_core::store::forwards::{ForwardKind, PortForward};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// 正在连 SSH（可能在等用户输密码 / 确认指纹）。
    Starting,
    Running,
    Failed,
}

#[derive(Debug, Clone)]
pub struct ForwardStatus {
    pub id: i64,
    pub phase: Phase,
    pub error: Option<String>,
    /// 远程转发时服务器实际监听的端口（请求 0 之类时和规则里写的不一样）。
    pub listening_port: Option<u32>,
    pub counters: ForwardCounters,
}

impl ForwardStatus {
    /// 在跑（连接中或运行中）；失败的不算。
    pub fn active(&self) -> bool {
        self.phase != Phase::Failed
    }
}

/// 阶段、出错原因、远程转发实际监听的端口。
type PhaseState = (Phase, Option<String>, Option<u32>);

/// 远程转发停掉时要告诉服务器取消监听：任务被 abort 时跑不了异步清理，记在外面。
type RemoteCleanup = Option<(Arc<Connection>, String, u32)>;

struct Running {
    /// 这一次启动在运行层登记表里的 key（带代次）。
    key: String,
    phase: Arc<Mutex<PhaseState>>,
    stats: Arc<ForwardStats>,
    task: AbortHandle,
    remote: Arc<Mutex<RemoteCleanup>>,
}

/// 交给转发任务的共享状态：任务把阶段、计数、远程清理信息写在这里，界面通过
/// [`ForwardManager`] 读同一份。
struct LaunchCtx {
    phase: Arc<Mutex<PhaseState>>,
    stats: Arc<ForwardStats>,
    remote: Arc<Mutex<RemoteCleanup>>,
}

/// 等旧任务真正结束（放掉监听端口、连接引用）的上限；到点还没结束就照常往下走，
/// 新任务端口被占会如实显示失败，不会卡住界面。
const RELEASE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct ForwardManager {
    running: Mutex<HashMap<i64, Running>>,
    /// 每次启动加一，拼进登记 key。
    generation: AtomicU64,
    /// 刚被停掉、可能还没真正结束的任务：规则 id → 登记 key。重开前先等它们结束。
    retiring: Mutex<HashMap<i64, String>>,
    /// 状态变化计数：界面每个节拍看一眼，变了就重绘（没有事件总线，用版本号代替）。
    version: AtomicU64,
    /// 停止 / 重启 / 删除 / 手动开启串行执行。
    lifecycle: tokio::sync::Mutex<()>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 结束一条转发：任务取消（它持有的监听和每条连接跟着关），远程转发告诉服务器取消监听。
/// 返回它的登记 key（任务要稍后才真正结束）。
fn shut_down(running: Running) -> String {
    running.task.abort();
    let remote = lock(&running.remote).take();
    if let Some((connection, bind_host, port)) = remote {
        // 运行时已经没了（进程在退出）就不告诉服务器了，和 0.2.14 一样。
        if let Ok(runtime) = crate::ssh_backend::runtime() {
            runtime.spawn(async move { connection.stop_remote_forward(&bind_host, port).await });
        }
    }
    running.key
}

impl ForwardManager {
    fn bump(&self) {
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    /// 状态变化计数。
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    fn is_active(&self, id: i64) -> bool {
        lock(&self.running)
            .get(&id)
            .is_some_and(|running| lock(&running.phase).0 != Phase::Failed)
    }

    /// 停掉一条（没开也不算错）。任务要稍后才真正结束；要在它结束后再做事的调用方接着调 [`Self::settle`]。
    fn stop(&self, id: i64) {
        let removed = lock(&self.running).remove(&id);
        if let Some(running) = removed {
            let key = shut_down(running);
            lock(&self.retiring).insert(id, key);
        }
        self.bump();
    }

    /// 等这条规则刚被停掉的旧任务真正结束（监听端口放掉、连接引用释放）。
    async fn settle(&self, runtime: &Arc<TaskRegistry>, id: i64) {
        let key = lock(&self.retiring).remove(&id);
        if let Some(key) = key
            && !runtime.wait_released(&key, RELEASE_TIMEOUT).await
        {
            tracing::warn!(id, "旧的端口转发任务没在限期内结束，继续重开");
        }
    }

    /// 登记并启动一条转发任务；这条规则已经有在跑的（没失败的）就什么也不做，返回 false。
    /// 检查、清掉失败的旧记录、登记新任务都在同一把锁里：两次几乎同时的「开启」不会各起一个
    /// 任务、其中一个脱离管理。
    fn launch<F, Fut>(&self, runtime: &Arc<TaskRegistry>, id: i64, body: F) -> bool
    where
        F: FnOnce(LaunchCtx) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let mut running = lock(&self.running);
        if running
            .get(&id)
            .is_some_and(|existing| lock(&existing.phase).0 != Phase::Failed)
        {
            return false;
        }
        if let Some(failed) = running.remove(&id) {
            let key = shut_down(failed);
            lock(&self.retiring).insert(id, key);
        }

        let phase = Arc::new(Mutex::new((Phase::Starting, None, None)));
        let stats = Arc::new(ForwardStats::default());
        let remote = Arc::new(Mutex::new(None));
        let ctx = LaunchCtx {
            phase: Arc::clone(&phase),
            stats: Arc::clone(&stats),
            remote: Arc::clone(&remote),
        };
        // 先登记再起任务：任务很快失败时，它写的 Failed 状态要落在已登记的那一份上。
        let key = format!(
            "forward:{id}:{}",
            self.generation.fetch_add(1, Ordering::Relaxed) + 1
        );
        let spawned = runtime.spawn_with_abort(key.clone(), move |runtime_cancel| async move {
            tokio::select! {
                _ = body(ctx) => {}
                _ = runtime_cancel.cancelled() => {}
            }
        });
        let task = match spawned {
            Ok((_token, abort)) => abort,
            Err(err) => {
                tracing::warn!(%err, "端口转发任务已经登记");
                return false;
            }
        };
        running.insert(
            id,
            Running {
                key,
                phase,
                stats,
                task,
                remote,
            },
        );
        drop(running);
        self.bump();
        true
    }

    /// 规则改了：正开着的转发停掉旧的、等它真正结束，再按新设置开一个。没在跑（或已经失败）的
    /// 不动，返回 false。
    ///
    /// `prepare` 在拿到锁之后才调，返回新任务体（读库里最新的规则）；返回 `None` 表示规则已经
    /// 不在了，旧的不动（删除规则的路径自己会停它）。连续快速编辑排队执行，最后只剩一个实例。
    async fn restart<F, Fut>(
        &self,
        runtime: &Arc<TaskRegistry>,
        id: i64,
        prepare: impl FnOnce() -> Option<F>,
    ) -> bool
    where
        F: FnOnce(LaunchCtx) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let _serial = self.lifecycle.lock().await;
        if !self.is_active(id) {
            return false;
        }
        let Some(body) = prepare() else {
            return false;
        };
        self.stop(id);
        self.settle(runtime, id).await;
        self.launch(runtime, id, body)
    }

    /// 停掉所有「数据库里已经没有这条规则」的转发：删主机会级联删掉它的规则，同步、恢复备份、
    /// 替换导入也可能删掉主机，但跑着的监听和隧道不会跟着停。返回停掉了几条。
    fn stop_missing(&self, existing: &HashSet<i64>) -> Vec<i64> {
        let orphans: Vec<(i64, Running)> = {
            let mut running = lock(&self.running);
            let ids: Vec<i64> = running
                .keys()
                .copied()
                .filter(|id| !existing.contains(id))
                .collect();
            ids.into_iter()
                .filter_map(|id| running.remove(&id).map(|r| (id, r)))
                .collect()
        };
        let stopped: Vec<i64> = orphans
            .into_iter()
            .map(|(id, running)| {
                shut_down(running);
                id
            })
            .collect();
        if !stopped.is_empty() {
            self.bump();
        }
        stopped
    }

    pub fn statuses(&self) -> Vec<ForwardStatus> {
        lock(&self.running)
            .iter()
            .map(|(id, running)| {
                let (phase, error, listening_port) = lock(&running.phase).clone();
                ForwardStatus {
                    id: *id,
                    phase,
                    error,
                    listening_port,
                    counters: running.stats.snapshot(),
                }
            })
            .collect()
    }

    /// 退出时：停掉所有正在跑的转发（监听和隧道跟着关）。
    pub fn stop_all(&self) {
        let all: Vec<Running> = lock(&self.running).drain().map(|(_, r)| r).collect();
        for running in all {
            shut_down(running);
        }
        self.bump();
    }
}

// ---- 进程级单例 ----

static MANAGER: OnceLock<ForwardManager> = OnceLock::new();
static REGISTRY: OnceLock<Arc<TaskRegistry>> = OnceLock::new();

pub fn manager() -> &'static ForwardManager {
    MANAGER.get_or_init(ForwardManager::default)
}

/// 任务登记表：用 SSH 运行时的句柄建，这样从界面线程（没有 tokio 上下文）也能登记任务
/// （V2-F6 在 Tauri 入口踩过这个坑）。
fn registry() -> Result<&'static Arc<TaskRegistry>, String> {
    if let Some(registry) = REGISTRY.get() {
        return Ok(registry);
    }
    let handle = crate::ssh_backend::runtime()?.handle().clone();
    let _ = REGISTRY.set(TaskRegistry::with_handle(handle));
    REGISTRY
        .get()
        .ok_or_else(|| "转发任务登记表初始化失败".to_owned())
}

// ---- 一条规则的任务体 ----

/// 一条规则对应的任务体：跑到出错为止，把原因记成 Failed 状态。
fn forward_body(rule: PortForward) -> impl FnOnce(LaunchCtx) -> BoxFuture<'static, ()> {
    move |ctx| {
        Box::pin(async move {
            let reason = run(rule, &ctx.phase, ctx.stats, &ctx.remote).await;
            *lock(&ctx.phase) = (Phase::Failed, Some(reason), None);
            manager().bump();
        })
    }
}

/// 跑一条规则，返回停下来的原因（只有出错才会停；用户点停止是直接 abort）。
async fn run(
    rule: PortForward,
    phase: &Mutex<PhaseState>,
    stats: Arc<ForwardStats>,
    remote_slot: &Mutex<RemoteCleanup>,
) -> String {
    // 本地转发先占端口：端口被占用时不用白连一次 SSH。
    let listener = match rule.kind {
        ForwardKind::Local => match forward::bind_local(&rule.bind_host, rule.bind_port).await {
            Ok(listener) => Some(listener),
            Err(err) => return err.to_string(),
        },
        ForwardKind::Remote => None,
    };

    // 池里有这台主机的活连接就直接复用（不会再问一次密码）；没有就按拨号链连一条。
    let factory = match crate::ssh_backend::factory_for_host(rule.host_id).await {
        Ok(factory) => factory,
        Err(err) => return err,
    };
    let connection = factory.connection();

    let reason = match (rule.kind, listener) {
        (ForwardKind::Local, Some(listener)) => {
            *lock(phase) = (Phase::Running, None, None);
            manager().bump();
            forward::run_local(connection, listener, rule.dest_host, rule.dest_port, stats).await
        }
        _ => {
            let (sink, incoming) = tokio::sync::mpsc::unbounded_channel();
            let port = match connection
                .start_remote_forward(&rule.bind_host, rule.bind_port, sink)
                .await
            {
                Ok(port) => port,
                Err(err) => return err.to_string(),
            };
            *lock(remote_slot) = Some((Arc::clone(&connection), rule.bind_host.clone(), port));
            *lock(phase) = (Phase::Running, None, Some(port));
            manager().bump();
            forward::run_remote(connection, incoming, rule.dest_host, rule.dest_port, stats).await
        }
    };
    // 连接引用要活到转发结束：放掉之后池子才可能回收这条连接。
    drop(factory);
    reason.to_string()
}

/// 读库里一条规则（任务线程里单开一条库连接：连接对象不能跨线程）。
fn load_rule(id: i64) -> Result<PortForward, String> {
    let db = crate::hosts_demo::open_database().map_err(|err| err.to_string())?;
    db.forward_get(id).map_err(|err| err.to_string())
}

// ---- 给界面用的入口（都不阻塞界面线程） ----

/// 开启一条规则。马上返回；连没连上看 [`ForwardManager::statuses`]。
/// 刚点过停止再点开启：先等旧的真正结束，端口才空得出来。
pub fn start_rule(rule: PortForward) {
    let (Ok(registry), Ok(runtime)) = (registry(), crate::ssh_backend::runtime()) else {
        return;
    };
    let registry = Arc::clone(registry);
    runtime.spawn(async move {
        let manager = manager();
        let _serial = manager.lifecycle.lock().await;
        manager.settle(&registry, rule.id).await;
        let id = rule.id;
        manager.launch(&registry, id, forward_body(rule));
    });
}

/// 停止一条规则（没开也不算错）。
pub fn stop_rule(id: i64) {
    let Ok(runtime) = crate::ssh_backend::runtime() else {
        return;
    };
    runtime.spawn(async move {
        let manager = manager();
        let _serial = manager.lifecycle.lock().await;
        manager.stop(id);
    });
}

/// 规则存过了：正开着的转发按库里最新的规则重开（没开着的不动）。
pub fn restart_if_running(id: i64) {
    let (Ok(registry), Ok(runtime)) = (registry(), crate::ssh_backend::runtime()) else {
        return;
    };
    let registry = Arc::clone(registry);
    runtime.spawn(async move {
        manager()
            .restart(&registry, id, || match load_rule(id) {
                // 排队等锁的时候可能又被改过：按库里最新的一份重开。
                Ok(rule) => Some(forward_body(rule)),
                Err(err) => {
                    tracing::warn!(%err, id, "读刚保存的转发规则失败，不重开");
                    None
                }
            })
            .await;
    });
}

/// 删除一条规则：先停（在锁里，免得和正在排队的重启交错），再从库里删。结果从返回的通道里取。
pub fn delete_rule(id: i64) -> Result<tokio::sync::oneshot::Receiver<Result<(), String>>, String> {
    let runtime = crate::ssh_backend::runtime()?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    runtime.spawn(async move {
        let manager = manager();
        let _serial = manager.lifecycle.lock().await;
        manager.stop(id);
        let result = match crate::hosts_demo::open_database() {
            Ok(db) => db.forward_delete(id).map_err(|err| err.to_string()),
            Err(err) => Err(err.to_string()),
        };
        let _ = tx.send(result);
    });
    Ok(rx)
}

/// 数据库变了（删主机、同步合并、恢复备份）：规则已经不在了的转发停掉。
pub fn reconcile(existing: &HashSet<i64>) -> Vec<i64> {
    manager().stop_missing(existing)
}

/// 某台主机的终端连上了：把这台主机上勾了「自动开启」、还没开的规则都开起来。
pub fn auto_start_for_host(host_id: i64, rules: &[PortForward]) {
    for rule in rules
        .iter()
        .filter(|rule| rule.host_id == host_id && rule.auto_start)
    {
        if !manager().is_active(rule.id) {
            start_rule(rule.clone());
        }
    }
}

/// 退出：停掉所有转发。
pub fn shutdown() {
    manager().stop_all();
    if let Some(registry) = REGISTRY.get() {
        registry.cancel_all();
    }
}

// ---- 显示用的文字 ----

pub fn describe(rule: &PortForward) -> String {
    let bind = format!("{}:{}", rule.bind_host, rule.bind_port);
    let dest = format!("{}:{}", rule.dest_host, rule.dest_port);
    match rule.kind {
        ForwardKind::Local => format!("本机 {bind} → {dest}（经服务器）"),
        ForwardKind::Remote => format!("服务器 {bind} → 本机这边 {dest}"),
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let value = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if value < KB * KB {
        format!("{:.1} KB", value / KB)
    } else if value < KB * KB * KB {
        format!("{:.1} MB", value / KB / KB)
    } else {
        format!("{:.2} GB", value / KB / KB / KB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running_task() -> Running {
        Running {
            key: "forward:test".to_owned(),
            phase: Arc::new(Mutex::new((Phase::Running, None, None))),
            stats: Arc::new(ForwardStats::default()),
            task: tokio::spawn(std::future::pending::<()>()).abort_handle(),
            remote: Arc::new(Mutex::new(None)),
        }
    }

    /// 假的转发任务体：记下「同时活着几个」，被 drop（abort / 取消）时减回去。
    #[derive(Default)]
    struct Probe {
        live: std::sync::atomic::AtomicUsize,
        max_live: std::sync::atomic::AtomicUsize,
        started: std::sync::atomic::AtomicUsize,
    }

    struct Alive(Arc<Probe>);

    impl Alive {
        fn enter(probe: &Arc<Probe>) -> Self {
            use std::sync::atomic::Ordering::SeqCst;
            probe.started.fetch_add(1, SeqCst);
            let now = probe.live.fetch_add(1, SeqCst) + 1;
            probe.max_live.fetch_max(now, SeqCst);
            Self(Arc::clone(probe))
        }
    }

    impl Drop for Alive {
        fn drop(&mut self) {
            self.0
                .live
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn probe_body(probe: &Arc<Probe>) -> impl FnOnce(LaunchCtx) -> BoxFuture<'static, ()> + use<> {
        let probe = Arc::clone(probe);
        move |_ctx| {
            Box::pin(async move {
                let _alive = Alive::enter(&probe);
                std::future::pending::<()>().await;
            })
        }
    }

    async fn until(what: &str, mut ready: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while !ready() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("等不到：{what}"));
    }

    #[tokio::test]
    async fn a_forward_started_right_after_stopping_the_old_one_is_not_dropped() {
        let manager = ForwardManager::default();
        let runtime = TaskRegistry::new();
        let probe = Arc::new(Probe::default());
        assert!(manager.launch(&runtime, 1, probe_body(&probe)));
        until("第一个实例跑起来", || {
            probe.started.load(Ordering::SeqCst) == 1
        })
        .await;

        // 编辑规则：停掉旧的，紧接着（同一个同步段里）登记新的。
        manager.stop(1);
        let launched = manager.launch(&runtime, 1, probe_body(&probe));

        assert!(launched, "旧任务还没结束时，新任务也要登记上");
        until("第二个实例跑起来", || {
            probe.started.load(Ordering::SeqCst) == 2
        })
        .await;
        let _ = runtime.shutdown(Duration::from_secs(2)).await;
    }

    #[tokio::test]
    async fn restart_waits_for_the_old_instance_before_starting_the_new_one() {
        let manager = ForwardManager::default();
        let runtime = TaskRegistry::new();
        let probe = Arc::new(Probe::default());
        assert!(manager.launch(&runtime, 1, probe_body(&probe)));
        until("第一个实例跑起来", || {
            probe.started.load(Ordering::SeqCst) == 1
        })
        .await;

        let restarted = manager
            .restart(&runtime, 1, || Some(probe_body(&probe)))
            .await;

        assert!(restarted);
        until("新实例跑起来", || {
            probe.started.load(Ordering::SeqCst) == 2
        })
        .await;
        assert_eq!(probe.live.load(Ordering::SeqCst), 1, "旧实例必须已经结束");
        assert_eq!(
            probe.max_live.load(Ordering::SeqCst),
            1,
            "新旧实例不能同时活着（否则同一个监听端口会冲突）"
        );
        assert_eq!(manager.statuses().len(), 1);
        let _ = runtime.shutdown(Duration::from_secs(2)).await;
    }

    #[tokio::test]
    async fn restart_leaves_stopped_forwards_alone() {
        let manager = ForwardManager::default();
        let runtime = TaskRegistry::new();
        let probe = Arc::new(Probe::default());
        let restarted = manager
            .restart(&runtime, 1, || Some(probe_body(&probe)))
            .await;
        assert!(!restarted, "没开着的规则不会因为保存而被开起来");
        assert_eq!(probe.started.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rapid_consecutive_edits_leave_exactly_one_instance() {
        let manager = Arc::new(ForwardManager::default());
        let runtime = TaskRegistry::new();
        let probe = Arc::new(Probe::default());
        assert!(manager.launch(&runtime, 1, probe_body(&probe)));

        let mut edits = Vec::new();
        for _ in 0..20 {
            let manager = Arc::clone(&manager);
            let runtime = Arc::clone(&runtime);
            let probe = Arc::clone(&probe);
            edits.push(tokio::spawn(async move {
                manager
                    .restart(&runtime, 1, || Some(probe_body(&probe)))
                    .await
            }));
        }
        for edit in edits {
            assert!(edit.await.expect("编辑任务不 panic"));
        }

        until("只剩一个实例", || {
            probe.live.load(Ordering::SeqCst) == 1
        })
        .await;
        assert_eq!(probe.started.load(Ordering::SeqCst), 21);
        assert_eq!(
            probe.max_live.load(Ordering::SeqCst),
            1,
            "任何时刻都只有一个实例"
        );
        assert_eq!(manager.statuses().len(), 1);

        // 退出时仍能全部停掉。
        assert!(runtime.shutdown(Duration::from_secs(2)).await);
        assert_eq!(probe.live.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn runtime_shutdown_stops_running_forwards_without_waiting_for_the_timeout() {
        let manager = ForwardManager::default();
        let runtime = TaskRegistry::new();
        let probe = Arc::new(Probe::default());
        for id in 1..=3 {
            assert!(manager.launch(&runtime, id, probe_body(&probe)));
        }
        until("三个转发都跑起来", || {
            probe.live.load(Ordering::SeqCst) == 3
        })
        .await;

        let started = std::time::Instant::now();
        // 超时给得很长：任务响应取消的话会立刻返回 true，不会等满。
        let all_done = runtime.shutdown(Duration::from_secs(5)).await;

        assert!(all_done, "转发没有响应运行层的取消");
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "收尾用了 {:?}",
            started.elapsed()
        );
        assert_eq!(probe.live.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn forwards_whose_rules_are_gone_are_stopped() {
        let manager = ForwardManager::default();
        lock(&manager.running).insert(1, running_task());
        lock(&manager.running).insert(2, running_task());
        let doomed = lock(&manager.running)
            .get(&2)
            .map(|r| r.task.clone())
            .expect("有第二条");

        let stopped = manager.stop_missing(&HashSet::from([1]));

        assert_eq!(stopped, vec![2]);
        assert!(manager.is_active(1), "规则还在的不动");
        assert!(!manager.is_active(2));
        tokio::task::yield_now().await;
        assert!(
            doomed.is_finished(),
            "停掉的转发任务要真的结束（监听和连接跟着关）"
        );
    }

    #[tokio::test]
    async fn every_state_change_bumps_the_version_the_ui_watches() {
        let manager = ForwardManager::default();
        let runtime = TaskRegistry::new();
        let probe = Arc::new(Probe::default());
        let before = manager.version();
        assert!(manager.launch(&runtime, 1, probe_body(&probe)));
        let launched = manager.version();
        assert!(launched > before, "开启要让界面知道");
        manager.stop(1);
        assert!(manager.version() > launched, "停止也要");
        let _ = runtime.shutdown(Duration::from_secs(2)).await;
    }

    #[test]
    fn stop_all_empties_the_table() {
        // 没有运行时上下文：任务句柄用一个临时运行时造。
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("运行时");
        let _guard = runtime.enter();
        let manager = ForwardManager::default();
        lock(&manager.running).insert(7, running_task());
        manager.stop_all();
        assert!(manager.statuses().is_empty());
    }

    #[test]
    fn rules_are_described_the_way_the_old_panel_did() {
        let rule = |kind| PortForward {
            id: 1,
            host_id: 1,
            name: String::new(),
            kind,
            bind_host: "127.0.0.1".to_owned(),
            bind_port: 8080,
            dest_host: "localhost".to_owned(),
            dest_port: 80,
            auto_start: false,
        };
        assert_eq!(
            describe(&rule(ForwardKind::Local)),
            "本机 127.0.0.1:8080 → localhost:80（经服务器）"
        );
        assert_eq!(
            describe(&rule(ForwardKind::Remote)),
            "服务器 127.0.0.1:8080 → 本机这边 localhost:80"
        );
    }

    #[test]
    fn byte_counts_use_the_same_units_as_the_old_panel() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.00 GB");
    }
}
