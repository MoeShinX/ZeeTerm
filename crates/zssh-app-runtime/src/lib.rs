//! 应用运行层的基础设施，不依赖 Tauri、GPUI 或 WebView2。
//!
//! G04.1 先提供三样东西：
//! - 类型化标识：会话/代次、编辑版本、审批、传输、同步；
//! - 任务登记表：按 key 登记、取消、统一收尾；
//! - 类型化事件总线：入口层只转发，不持有核心状态。
//!
//! 连接池、认证、数据库仍由 `zssh-core` 提供；这里不复制第二套。

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use futures::FutureExt as _;
use tokio::sync::{broadcast, Notify};
use tokio::task::{AbortHandle, JoinHandle};

pub mod output;
pub mod session;

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_id!(SessionId);
string_id!(EditId);
string_id!(ApprovalId);
string_id!(TransferId);
string_id!(SyncId);

/// 会话重连后的代次；旧代次的迟到回调必须丢弃。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Generation(pub u64);

impl fmt::Display for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 编辑文件的版本；保存时必须带打开时的版本。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EditVersion(pub u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeEvent {
    SessionOpened {
        session: SessionId,
        generation: Generation,
    },
    SessionOutput {
        session: SessionId,
        generation: Generation,
        bytes: usize,
    },
    SessionClosed {
        session: SessionId,
        generation: Generation,
        reason: String,
    },
    EditSaved {
        edit: EditId,
        version: EditVersion,
    },
    ApprovalRequested {
        approval: ApprovalId,
        session: Option<SessionId>,
        summary: String,
    },
    TransferProgress {
        transfer: TransferId,
        done: u64,
        total: Option<u64>,
    },
    SyncFinished {
        sync: SyncId,
        ok: bool,
    },
}

/// 广播事件总线；入口层订阅后转成自己的事件。
#[derive(Clone)]
pub struct EventBus {
    sender: broadcast::Sender<RuntimeEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.max(1));
        Self { sender }
    }

    /// 没有订阅者时丢弃事件，不阻塞调用方。
    pub fn emit(&self, event: RuntimeEvent) {
        let _ = self.sender.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.sender.subscribe()
    }

    pub fn subscribers(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(256)
    }
}

/// 可克隆的取消信号；任务用 `cancelled().await` 等它。
#[derive(Clone)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
        }
    }

    pub fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::SeqCst) {
            self.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let notified = self.notify.notified();
        tokio::pin!(notified);
        // 先注册再检查，避免"检查完到开始等"之间丢掉唤醒。
        let _ = notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

struct TaskEntry {
    token: CancelToken,
    handle: JoinHandle<()>,
}

/// 任务结束时（正常、panic 或被 abort）在 drop 里摘除登记。
struct KeyGuard {
    registry: Weak<TaskRegistry>,
    key: String,
}

impl Drop for KeyGuard {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            registry.finish(&self.key);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SpawnError {
    AlreadyRunning(String),
    /// 调用线程不在 Tokio 运行时里，登记表也没有配置运行时句柄。
    NoRuntime,
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyRunning(key) => write!(f, "任务已经登记：{key}"),
            Self::NoRuntime => f.write_str("当前线程没有 Tokio 运行时，登记表也没有配置运行时句柄"),
        }
    }
}

impl std::error::Error for SpawnError {}

/// 按 key 登记的后台任务；同一 key 不允许重复登记。
#[derive(Default)]
pub struct TaskRegistry {
    tasks: Mutex<HashMap<String, TaskEntry>>,
    panics: AtomicU64,
    /// 任务跑在哪个运行时上。没配置就用调用线程所在的运行时。
    handle: Option<tokio::runtime::Handle>,
    /// 有任务从表里摘除时通知，`wait_released` 靠它等 key 空出来。
    released: Notify,
}

impl TaskRegistry {
    /// 任务跑在调用 `spawn` 的线程所在的 Tokio 运行时上；不在运行时里调用会返回
    /// `SpawnError::NoRuntime`（不会 panic）。
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 任务固定跑在 `handle` 的运行时上，任何线程（包括 Tauri `setup` 所在的、
    /// 没有运行时上下文的主线程）都能登记。入口层应该用这个。
    pub fn with_handle(handle: tokio::runtime::Handle) -> Arc<Self> {
        Arc::new(Self {
            handle: Some(handle),
            ..Self::default()
        })
    }

    /// 登记表的锁。容忍中毒：登记表只是 key → 句柄的映射，别处 panic 不会让它处于
    /// 不一致状态；这里再 panic 会让每个任务结束时的 `KeyGuard::drop` 跟着 panic。
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, TaskEntry>> {
        self.tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 登记并启动任务；任务拿到自己的取消信号，正常结束会自动从表里移除。
    pub fn spawn<F, Fut>(
        self: &Arc<Self>,
        key: impl Into<String>,
        task: F,
    ) -> Result<CancelToken, SpawnError>
    where
        F: FnOnce(CancelToken) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.register(key, task).map(|(token, _)| token)
    }

    /// 同 [`Self::spawn`]，但额外返回 `AbortHandle`，给仍以 abort 停止的任务用。
    /// 任务被 abort 时 `KeyGuard` 会在 drop 里摘除登记，不会留下死 key。
    pub fn spawn_with_abort<F, Fut>(
        self: &Arc<Self>,
        key: impl Into<String>,
        task: F,
    ) -> Result<(CancelToken, AbortHandle), SpawnError>
    where
        F: FnOnce(CancelToken) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.register(key, task)
    }

    fn register<F, Fut>(
        self: &Arc<Self>,
        key: impl Into<String>,
        task: F,
    ) -> Result<(CancelToken, AbortHandle), SpawnError>
    where
        F: FnOnce(CancelToken) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let key = key.into();
        let runtime = match &self.handle {
            Some(handle) => handle.clone(),
            None => tokio::runtime::Handle::try_current().map_err(|_| SpawnError::NoRuntime)?,
        };
        let mut tasks = self.lock();
        if tasks.contains_key(&key) {
            return Err(SpawnError::AlreadyRunning(key));
        }

        let token = CancelToken::new();
        let this = Arc::clone(self);
        let key_for_task = key.clone();
        let task_token = token.clone();
        let guard = KeyGuard {
            registry: Arc::downgrade(self),
            key: key_for_task.clone(),
        };
        let handle = runtime.spawn(async move {
            // 正常结束、panic、外部 abort 都会走到这里，保证登记被摘除。
            let _guard = guard;
            let result = std::panic::AssertUnwindSafe(task(task_token))
                .catch_unwind()
                .await;
            if let Err(panic) = result {
                this.panics.fetch_add(1, Ordering::Relaxed);
                tracing::error!(task = %key_for_task, "后台任务 panic：{panic:?}");
            }
        });
        let abort = handle.abort_handle();
        tasks.insert(
            key,
            TaskEntry {
                token: token.clone(),
                handle,
            },
        );
        Ok((token, abort))
    }

    fn finish(&self, key: &str) {
        // 取出并丢弃自己的 JoinHandle；任务已经结束，drop 只是解除登记。
        let _ = self.lock().remove(key);
        self.released.notify_waiters();
    }

    /// 等 `key` 从登记表里摘除（任务真正结束，`KeyGuard` 已 drop）。`abort()` 只是
    /// 请求取消，future 要等运行时下一次调度才会被 drop；调用方想在同一个 key 上
    /// 马上重新登记、或者想等旧任务放掉端口之类的资源时，先等这个。
    /// key 本来就不在表里返回 true；超时返回 false。
    pub async fn wait_released(&self, key: &str, timeout: Duration) -> bool {
        let wait = async {
            loop {
                let notified = self.released.notified();
                tokio::pin!(notified);
                // 先注册再检查，避免"检查完到开始等"之间丢掉唤醒。
                let _ = notified.as_mut().enable();
                if !self.lock().contains_key(key) {
                    return;
                }
                notified.await;
            }
        };
        tokio::time::timeout(timeout, wait).await.is_ok()
    }

    /// 只发取消信号，不等任务结束。
    pub fn cancel(&self, key: &str) -> bool {
        let tasks = self.lock();
        match tasks.get(key) {
            Some(entry) => {
                entry.token.cancel();
                true
            }
            None => false,
        }
    }

    pub fn cancel_all(&self) {
        let tasks = self.lock();
        for entry in tasks.values() {
            entry.token.cancel();
        }
    }

    pub fn active(&self) -> usize {
        self.lock().len()
    }

    /// 已经捕获并记录的 panic 数量。
    pub fn panic_count(&self) -> u64 {
        self.panics.load(Ordering::Relaxed)
    }

    /// 取消所有任务并在超时内等待它们结束；返回是否全部收尾。
    pub async fn shutdown(&self, timeout: Duration) -> bool {
        let entries: Vec<TaskEntry> = {
            let mut tasks = self.lock();
            tasks.drain().map(|(_, entry)| entry).collect()
        };
        for entry in &entries {
            entry.token.cancel();
        }
        let wait = async {
            for entry in entries {
                let _ = entry.handle.await;
            }
        };
        tokio::time::timeout(timeout, wait).await.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn spawn_cancel_and_shutdown() {
        let registry = TaskRegistry::new();
        let token = registry
            .spawn("watcher", |token| async move {
                token.cancelled().await;
            })
            .expect("登记任务");
        assert_eq!(registry.active(), 1);

        assert!(registry.cancel("watcher"));
        assert!(token.is_cancelled());
        assert!(registry.shutdown(Duration::from_secs(2)).await);
        assert_eq!(registry.active(), 0);
    }

    #[tokio::test]
    async fn finished_tasks_leave_the_registry() {
        let registry = TaskRegistry::new();
        registry.spawn("quick", |_| async {}).expect("登记任务");
        for _ in 0..20 {
            if registry.active() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(registry.active(), 0);
    }

    #[tokio::test]
    async fn duplicate_key_is_rejected() {
        let registry = TaskRegistry::new();
        registry
            .spawn("same", |_| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
            })
            .unwrap();
        let err = registry
            .spawn("same", |_| async {})
            .err()
            .expect("应拒绝重复 key");
        assert_eq!(err, SpawnError::AlreadyRunning("same".to_owned()));
        let _ = registry.shutdown(Duration::from_secs(1)).await;
    }

    #[tokio::test]
    async fn aborted_tasks_leave_the_registry() {
        let registry = TaskRegistry::new();
        let (_token, abort) = registry
            .spawn_with_abort("abortme", |_| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
            })
            .expect("登记任务");
        assert_eq!(registry.active(), 1);
        abort.abort();
        for _ in 0..100 {
            if registry.active() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(registry.active(), 0, "abort 的任务也要从表里摘除");
    }

    /// Tauri 的 `setup` 在主线程的事件循环回调里跑，那里没有 Tokio 运行时上下文；
    /// 启动时登记同步调度器就是在这种线程上调 `spawn`。
    #[test]
    fn spawning_from_a_thread_without_a_runtime_context_is_an_error_not_a_panic() {
        let registry = TaskRegistry::new();
        let outcome = std::thread::spawn(move || registry.spawn("no-context", |_| async {}).err())
            .join()
            .expect("没有运行时上下文时不能 panic");
        assert_eq!(outcome, Some(SpawnError::NoRuntime));
    }

    #[test]
    fn a_registry_with_a_handle_can_spawn_from_any_thread() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("建运行时");
        let registry = TaskRegistry::with_handle(runtime.handle().clone());

        // 当前线程不在运行时里（和 Tauri 的 setup 一样）。
        let (tx, rx) = std::sync::mpsc::channel();
        registry
            .spawn("from-main", move |_| async move {
                let _ = tx.send(());
            })
            .expect("带句柄的登记表在任何线程上都能起任务");
        rx.recv_timeout(Duration::from_secs(2))
            .expect("任务应该在运行时上执行");
        runtime.block_on(async {
            assert!(registry.shutdown(Duration::from_secs(1)).await);
        });
    }

    #[tokio::test]
    async fn wait_released_returns_once_an_aborted_task_is_really_gone() {
        let registry = TaskRegistry::new();
        let (_token, abort) = registry
            .spawn_with_abort("forward:1", |_| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
            })
            .expect("登记任务");

        abort.abort();
        // abort 只是请求取消：同一个 key 立刻重登记会撞上旧登记。
        // 先等旧任务真正结束，再登记就一定成功。
        assert!(
            registry
                .wait_released("forward:1", Duration::from_secs(2))
                .await
        );
        registry
            .spawn("forward:1", |token| async move { token.cancelled().await })
            .expect("旧任务放掉 key 之后可以重新登记");
        let _ = registry.shutdown(Duration::from_secs(1)).await;
    }

    #[tokio::test]
    async fn wait_released_is_true_for_unknown_keys_and_times_out_for_live_ones() {
        let registry = TaskRegistry::new();
        assert!(
            registry
                .wait_released("nothing", Duration::from_millis(10))
                .await
        );
        registry
            .spawn("alive", |token| async move { token.cancelled().await })
            .expect("登记任务");
        assert!(
            !registry
                .wait_released("alive", Duration::from_millis(30))
                .await
        );
        let _ = registry.shutdown(Duration::from_secs(1)).await;
    }

    #[tokio::test]
    async fn a_poisoned_registry_keeps_working() {
        let registry = TaskRegistry::new();
        // 在持锁时 panic，把登记表的锁弄成中毒状态。
        let poisoner = Arc::clone(&registry);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.tasks.lock().unwrap();
            panic!("poison the registry lock");
        })
        .join();
        assert!(registry.tasks.is_poisoned());

        // 之后登记、取消、统计、收尾都不能再 panic（任务结束时的 KeyGuard::drop 也走这把锁）。
        registry
            .spawn(
                "after-poison",
                |token| async move { token.cancelled().await },
            )
            .expect("中毒后仍可登记");
        assert_eq!(registry.active(), 1);
        assert!(registry.cancel("after-poison"));
        assert!(registry.shutdown(Duration::from_secs(1)).await);
        assert_eq!(registry.active(), 0);
    }

    #[tokio::test]
    async fn panicking_task_is_removed_and_counted() {
        let registry = TaskRegistry::new();
        registry
            .spawn("boom", |_| async { panic!("boom") })
            .expect("登记任务");
        for _ in 0..100 {
            if registry.active() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(registry.active(), 0, "panic 的任务也要从表里摘除");
        assert_eq!(registry.panic_count(), 1);
    }

    #[tokio::test]
    async fn shutdown_reports_timeout_for_stuck_tasks() {
        let registry = TaskRegistry::new();
        registry
            .spawn("stuck", |_| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
            })
            .unwrap();
        assert!(!registry.shutdown(Duration::from_millis(50)).await);
    }

    #[tokio::test]
    async fn events_reach_subscribers() {
        let bus = EventBus::new(8);
        let mut rx = bus.subscribe();
        bus.emit(RuntimeEvent::SessionOpened {
            session: SessionId::new("s1"),
            generation: Generation(2),
        });
        let event = rx.recv().await.expect("收到事件");
        assert_eq!(
            event,
            RuntimeEvent::SessionOpened {
                session: SessionId::new("s1"),
                generation: Generation(2),
            }
        );
    }

    #[test]
    fn identifiers_are_comparable_and_printable() {
        assert_eq!(SessionId::new("abc").to_string(), "abc");
        assert_ne!(EditVersion(1), EditVersion(2));
        assert_eq!(Generation(3).to_string(), "3");
    }
}
