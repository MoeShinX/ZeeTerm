//! 已认证 SSH 连接的复用池：同一份配置的新窗口 / SFTP / 转发直接用现成的连接，不重新握手。
//!
//! 这个模块只管两件事，且两件事共用同一张表：
//!
//! - **同键建连合并**（审计 R08）：同一个键同时来多个请求，只发起一次握手，其余的等同一份结果。
//!   握手跑在池自己的任务里：任何一个等待者退出（包括最先来的那个）都不会取消其他人还需要的连接；
//!   只有等待者全走光了才中止握手（窗格都关了就别再弹认证框）。失败的结果不留在表里，下一次请求重新拨号。
//! - **空闲回收**（审计 R07）：池自己握着一份强引用，使用者都放手之后才开始计时，到点关闭。
//!   「使用者」= 池之外还拿着这条连接的任何一方（终端 shell、SFTP 通道、端口转发、AI 工具通道、
//!   状态采集、复制出来的标签……）——不是登记表，而是看实际还有没有人持有，所以没有哪类使用者会被漏记；
//!   具体怎么数见 [`PoolItem::outside_holders`]。
//!
//! 空闲检测靠一个后台任务按固定间隔巡查（持有计数没有「放手」通知可挂），所以实际关闭时刻比
//! 「最后一个使用者放手 + 超时」最多晚一个巡查间隔（生产里是 [`PRODUCTION_POLL`]）。
//!
//! 超时的语义：
//! - 计时从巡查第一次看到「没有使用者」的那一刻起；中途有人用，计时作废，下次放手重新算。
//! - 运行中改超时：已经在空闲的连接**从改动那一刻起按新值重新计时**（新值是 0 就在下一次巡查时关）。
//! - 0 = 没有使用者就关（设置页的「0 = 立刻断开」）。设置页没有「永不」这个取值。

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::AbortHandle;
use tokio::time::Instant;
use zssh_core::ssh::BoxFuture;

/// 生产里的巡查间隔：对「到点关闭」的精度够用，一条连接每秒只加锁四次。
pub const PRODUCTION_POLL: Duration = Duration::from_millis(250);

/// 连接池自己的错误文案。
#[derive(Clone, Copy, Debug)]
enum ConnPoolMsg {
    DialTaskEnded,
}

impl crate::i18n::Msg for ConnPoolMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::DialTaskEnded => "连接任务意外结束",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::DialTaskEnded => "The connection task ended unexpectedly",
        }
    }
}

/// 关闭一条空闲连接最多等多久：对端不回应也不能拖住后面的巡查。
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// 池里放的东西（生产里是 `SshSessionFactory`）。做成 trait 是为了用不联网的假连接测池本身。
pub trait PoolItem: Send + Sync + 'static {
    /// 连接是不是已经断了（对端断开或保活超时）。断了的不再复用。
    fn is_closed(&self) -> bool;

    /// 池之外还有多少处拿着它。`this` 是池表里存的那份 `Arc`，所以池自己的一份已经在
    /// `Arc::strong_count(this)` 里，实现要把它扣掉；条目内部还握着别的 `Arc`（比如工厂里的
    /// `Arc<Connection>`）时，别人单独拿走那一份也算使用者，实现要一并数进来。
    fn outside_holders(this: &Arc<Self>) -> usize;

    /// 关闭连接（尽力而为：已经断了也不能报错）。
    fn close(self: Arc<Self>) -> BoxFuture<'static, ()>;
}

/// 池的配置。
#[derive(Clone, Debug)]
pub struct PoolConfig {
    /// 没有使用者之后，连接再空转多久关闭。
    pub idle_timeout: Duration,
    /// 巡查间隔（测试里缩短）。
    pub poll_interval: Duration,
}

/// 一次 [`Pool::acquire`] 的结果。
pub struct Acquired<T> {
    pub item: Arc<T>,
    /// `true` = 池里本来就有这条连接（没有人为它握手）。拿它开 shell 失败时可以丢掉它重连一次；
    /// `false` = 这一次刚握手出来的（自己发起的，或者等别人发起的那一次）。
    pub reused: bool,
}

type DialResult<T> = Result<Arc<T>, String>;

/// 一次正在进行的握手。
struct Dial<T> {
    id: u64,
    /// 结果出来之前是 `None`。发送端握在握手任务里：任务被中止或 panic 时发送端被丢掉，等待者据此得知。
    result: watch::Receiver<Option<DialResult<T>>>,
    /// 还在等这次握手的请求数（改动都在池的锁里）。
    waiters: AtomicUsize,
    abort: Mutex<Option<AbortHandle>>,
}

struct Ready<T> {
    item: Arc<T>,
    /// 巡查第一次看到没有使用者的时刻；有人用就清掉。
    idle_since: Option<Instant>,
}

enum Slot<T> {
    Connecting(Arc<Dial<T>>),
    Ready(Ready<T>),
}

struct State<T> {
    slots: HashMap<String, Slot<T>>,
    idle_timeout: Duration,
    /// 超时最近一次被改成不同值的时刻：已经在空闲的连接从这一刻起重新计时。
    timeout_changed: Instant,
    reaper_running: bool,
}

struct Inner<T> {
    state: Mutex<State<T>>,
    poll_interval: Duration,
    next_dial: AtomicU64,
}

impl<T> Inner<T> {
    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// 连接池。克隆出来的句柄共享同一张表。
pub struct Pool<T: PoolItem> {
    inner: Arc<Inner<T>>,
}

impl<T: PoolItem> Clone for Pool<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T: PoolItem> Pool<T> {
    pub fn new(config: PoolConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    slots: HashMap::new(),
                    idle_timeout: config.idle_timeout,
                    timeout_changed: Instant::now(),
                    reaper_running: false,
                }),
                poll_interval: config.poll_interval,
                next_dial: AtomicU64::new(1),
            }),
        }
    }

    /// 改空闲超时（设置页调）。值没变什么都不做；变了，已经空闲的连接从现在起按新值重新计时。
    pub fn set_idle_timeout(&self, timeout: Duration) {
        let mut state = self.inner.lock();
        if state.idle_timeout != timeout {
            state.idle_timeout = timeout;
            state.timeout_changed = Instant::now();
        }
    }

    /// 现在的空闲超时（测试读它确认设置真的传进来了）。
    #[cfg(test)]
    pub fn idle_timeout(&self) -> Duration {
        self.inner.lock().idle_timeout
    }

    /// 池里这个键现成的、没断的连接；没有（或在建、已断）就是 `None`。
    #[cfg(test)]
    pub fn get(&self, key: &str) -> Option<Arc<T>> {
        let mut state = self.inner.lock();
        match state.slots.get_mut(key) {
            Some(Slot::Ready(ready)) if !ready.item.is_closed() => {
                ready.idle_since = None;
                Some(Arc::clone(&ready.item))
            }
            _ => None,
        }
    }

    /// 池里现在的条目数（含正在建的）。
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.lock().slots.len()
    }

    /// 把这条连接从池里摘掉（拿它开 shell 发现它其实死了）。只摘同一条，别误伤已经换上的新连接。
    pub fn forget(&self, key: &str, item: &Arc<T>) {
        let mut state = self.inner.lock();
        if matches!(state.slots.get(key), Some(Slot::Ready(ready)) if Arc::ptr_eq(&ready.item, item))
        {
            state.slots.remove(key);
        }
    }

    /// 拿这个键的连接：池里有就直接给；有人正在握手就等那一次；否则用 `dial` 握手。
    ///
    /// `dial` 只有真的要拨号时才会被调用，且跑在池自己的任务里（所以需要运行时）。
    pub async fn acquire<F, Fut>(&self, key: &str, dial: F) -> Result<Acquired<T>, String>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = DialResult<T>> + Send + 'static,
    {
        let joined = {
            let mut state = self.inner.lock();
            let existing = match state.slots.get_mut(key) {
                Some(Slot::Ready(ready)) if !ready.item.is_closed() => {
                    ready.idle_since = None;
                    return Ok(Acquired {
                        item: Arc::clone(&ready.item),
                        reused: true,
                    });
                }
                Some(Slot::Ready(_)) => {
                    // 死连接：摘掉，下面重新拨。
                    state.slots.remove(key);
                    None
                }
                Some(Slot::Connecting(dial)) => Some(Arc::clone(dial)),
                None => None,
            };
            let dial_state = match existing {
                Some(dial_state) => dial_state,
                None => self.start_dial(&mut state, key, dial()),
            };
            dial_state.waiters.fetch_add(1, Ordering::SeqCst);
            Joined {
                inner: Arc::clone(&self.inner),
                key: key.to_owned(),
                dial: dial_state,
            }
        };
        let item = joined.wait().await?;
        Ok(Acquired {
            item,
            reused: false,
        })
    }

    /// 在池的锁里登记一次新握手并起任务。
    fn start_dial<Fut>(&self, state: &mut State<T>, key: &str, future: Fut) -> Arc<Dial<T>>
    where
        Fut: Future<Output = DialResult<T>> + Send + 'static,
    {
        let id = self.inner.next_dial.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = watch::channel(None);
        let dial = Arc::new(Dial {
            id,
            result: receiver,
            waiters: AtomicUsize::new(0),
            abort: Mutex::new(None),
        });
        state
            .slots
            .insert(key.to_owned(), Slot::Connecting(Arc::clone(&dial)));
        let inner = Arc::clone(&self.inner);
        let owned_key = key.to_owned();
        let handle = tokio::spawn(async move {
            // 任务被中止、panic、或者没走到登记就退出：把还挂着的「正在建」摘掉，等待者不会永远等。
            let mut cleanup = DialCleanup {
                inner: Arc::clone(&inner),
                key: owned_key.clone(),
                id,
                armed: true,
            };
            let result = future.await;
            {
                let mut state = inner.lock();
                let mine = matches!(
                    state.slots.get(&owned_key),
                    Some(Slot::Connecting(current)) if current.id == id
                );
                if mine {
                    match &result {
                        Ok(item) => {
                            state.slots.insert(
                                owned_key.clone(),
                                Slot::Ready(Ready {
                                    item: Arc::clone(item),
                                    idle_since: None,
                                }),
                            );
                            ensure_reaper(&inner, &mut state);
                        }
                        Err(_) => {
                            state.slots.remove(&owned_key);
                        }
                    }
                }
                cleanup.armed = false;
            }
            // 结果放进 watch 通道：它自己握着一份 `Arc`，等待者领走之前连接不会被当成「没人用」。
            let _ = sender.send(Some(result));
            // 发送端要活到所有等待者都读完：它随这个任务结束而丢，而 watch 里的最后一个值会留在通道里。
        });
        if let Ok(mut slot) = dial.abort.lock() {
            *slot = Some(handle.abort_handle());
        }
        dial
    }
}

/// 握手任务没正常走完时摘掉「正在建」的占位。
struct DialCleanup<T: PoolItem> {
    inner: Arc<Inner<T>>,
    key: String,
    id: u64,
    armed: bool,
}

impl<T: PoolItem> Drop for DialCleanup<T> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self.inner.lock();
        if matches!(
            state.slots.get(&self.key),
            Some(Slot::Connecting(current)) if current.id == self.id
        ) {
            state.slots.remove(&self.key);
        }
    }
}

/// 一个等着握手结果的请求。丢掉它 = 这个请求不再等了；最后一个也走了就中止握手。
struct Joined<T: PoolItem> {
    inner: Arc<Inner<T>>,
    key: String,
    dial: Arc<Dial<T>>,
}

impl<T: PoolItem> Joined<T> {
    async fn wait(&self) -> DialResult<T> {
        let mut receiver = self.dial.result.clone();
        loop {
            if let Some(result) = receiver.borrow_and_update().clone() {
                return result;
            }
            if receiver.changed().await.is_err() {
                // 发送端没了而没有结果：握手任务被中止了或 panic 了。
                return match receiver.borrow().clone() {
                    Some(result) => result,
                    None => Err(crate::i18n::t(ConnPoolMsg::DialTaskEnded).to_owned()),
                };
            }
        }
    }
}

impl<T: PoolItem> Drop for Joined<T> {
    fn drop(&mut self) {
        let mut state = self.inner.lock();
        let left = self.dial.waiters.fetch_sub(1, Ordering::SeqCst) - 1;
        if left > 0 {
            return;
        }
        // 最后一个等待者走了。握手还没完成（占位还挂着）就中止，别在没人要的时候继续弹认证框。
        let pending = matches!(
            state.slots.get(&self.key),
            Some(Slot::Connecting(current)) if current.id == self.dial.id
        );
        if pending {
            state.slots.remove(&self.key);
            drop(state);
            if let Ok(mut abort) = self.dial.abort.lock()
                && let Some(handle) = abort.take()
            {
                handle.abort();
            }
        }
    }
}

/// 起巡查任务（已经在跑就什么都不做）。调用时持有池的锁，所以不会重复起，也不会和「表空了就退出」错过。
fn ensure_reaper<T: PoolItem>(inner: &Arc<Inner<T>>, state: &mut State<T>) {
    if state.reaper_running {
        return;
    }
    state.reaper_running = true;
    tokio::spawn(patrol(Arc::downgrade(inner)));
}

/// 巡查：按间隔看每条连接有没有使用者，空闲够久就关。表空了就退出（下次有条目再起）。
async fn patrol<T: PoolItem>(inner: Weak<Inner<T>>) {
    loop {
        let poll = match inner.upgrade() {
            Some(inner) => inner.poll_interval,
            None => return,
        };
        tokio::time::sleep(poll).await;
        let Some(inner) = inner.upgrade() else {
            return;
        };
        let to_close = {
            let mut state = inner.lock();
            let now = Instant::now();
            let timeout = state.idle_timeout;
            let changed = state.timeout_changed;
            let mut to_close = Vec::new();
            state.slots.retain(|_, slot| match slot {
                Slot::Connecting(_) => true,
                Slot::Ready(ready) => {
                    if ready.item.is_closed() {
                        return false;
                    }
                    if T::outside_holders(&ready.item) > 0 {
                        ready.idle_since = None;
                        return true;
                    }
                    let since = *ready.idle_since.get_or_insert(now);
                    // 改过超时的话，空闲从改动那一刻起重新算。
                    let start = since.max(changed);
                    if now.saturating_duration_since(start) >= timeout {
                        to_close.push(Arc::clone(&ready.item));
                        false
                    } else {
                        true
                    }
                }
            });
            if state.slots.is_empty() {
                state.reaper_running = false;
                drop(state);
                spawn_closes(to_close);
                return;
            }
            to_close
        };
        spawn_closes(to_close);
    }
}

fn spawn_closes<T: PoolItem>(items: Vec<Arc<T>>) {
    for item in items {
        tokio::spawn(async move {
            let _ = tokio::time::timeout(CLOSE_TIMEOUT, item.close()).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// 不联网的假连接。
    struct Fake {
        id: usize,
        closed: AtomicBool,
        /// 被 `close` 调用的次数。放在外面共享，是因为观察者不能拿 `Arc<Fake>`——那会被池当成使用者。
        close_calls: Arc<AtomicUsize>,
        /// 模拟条目内部握着的另一份 `Arc`（对应工厂里的 `Arc<Connection>`）。
        link: Arc<()>,
    }

    impl PoolItem for Fake {
        fn is_closed(&self) -> bool {
            self.closed.load(Ordering::SeqCst)
        }

        fn outside_holders(this: &Arc<Self>) -> usize {
            (Arc::strong_count(this) - 1) + (Arc::strong_count(&this.link) - 1)
        }

        fn close(self: Arc<Self>) -> BoxFuture<'static, ()> {
            Box::pin(async move {
                self.close_calls.fetch_add(1, Ordering::SeqCst);
                self.closed.store(true, Ordering::SeqCst);
            })
        }
    }

    /// 假「网络」：数握手次数，给每条连接记一个关闭计数器。
    #[derive(Default)]
    struct Net {
        dials: AtomicUsize,
        close_counters: Mutex<Vec<Arc<AtomicUsize>>>,
    }

    impl Net {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn dials(&self) -> usize {
            self.dials.load(Ordering::SeqCst)
        }

        /// 第 `index` 条（从 0 起）握出来的连接被关了几次。
        fn closes_of(&self, index: usize) -> usize {
            self.close_counters.lock().unwrap()[index].load(Ordering::SeqCst)
        }

        /// 一次耗时 `handshake` 的拨号。
        fn dial(
            self: &Arc<Self>,
            handshake: Duration,
        ) -> impl FnOnce() -> BoxFuture<'static, DialResult<Fake>> + use<> {
            let net = Arc::clone(self);
            move || {
                Box::pin(async move {
                    let id = net.dials.fetch_add(1, Ordering::SeqCst) + 1;
                    tokio::time::sleep(handshake).await;
                    let close_calls = Arc::new(AtomicUsize::new(0));
                    net.close_counters
                        .lock()
                        .unwrap()
                        .push(Arc::clone(&close_calls));
                    Ok(Arc::new(Fake {
                        id,
                        closed: AtomicBool::new(false),
                        close_calls,
                        link: Arc::new(()),
                    }))
                })
            }
        }

        /// 一次耗时 `handshake` 后失败的拨号。
        fn failing_dial(
            self: &Arc<Self>,
            handshake: Duration,
        ) -> impl FnOnce() -> BoxFuture<'static, DialResult<Fake>> + use<> {
            let net = Arc::clone(self);
            move || {
                Box::pin(async move {
                    net.dials.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(handshake).await;
                    Err("认证失败".to_owned())
                })
            }
        }
    }

    fn pool(idle: Duration) -> Pool<Fake> {
        Pool::new(PoolConfig {
            idle_timeout: idle,
            poll_interval: Duration::from_millis(100),
        })
    }

    async fn wait(duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    // ---------------------------------------------------------------- R08

    /// 同一个键同时来十个请求：只握一次手，十个人拿到的是同一条连接。
    #[tokio::test(start_paused = true)]
    async fn ten_concurrent_requests_share_one_handshake() {
        let pool = pool(Duration::from_secs(60));
        let net = Net::new();
        let mut tasks = Vec::new();
        for _ in 0..10 {
            let pool = pool.clone();
            let net = Arc::clone(&net);
            tasks.push(tokio::spawn(async move {
                pool.acquire("host-a", net.dial(Duration::from_millis(500)))
                    .await
            }));
        }
        let mut items = Vec::new();
        for task in tasks {
            items.push(task.await.expect("任务").expect("拿到连接").item);
        }
        assert_eq!(net.dials(), 1, "握手只能发生一次");
        assert!(
            items.iter().all(|item| Arc::ptr_eq(item, &items[0])),
            "十个请求拿到的必须是同一条连接"
        );
        // 之后再来的直接命中池里的，仍然不握手。
        let again = pool
            .acquire("host-a", net.dial(Duration::ZERO))
            .await
            .expect("命中");
        assert!(again.reused && Arc::ptr_eq(&again.item, &items[0]));
        assert_eq!(net.dials(), 1);
    }

    /// 握手失败：同时在等的人都拿到这次的错误，之后再来的重新拨号，不会永远等在失败的占位上。
    #[tokio::test(start_paused = true)]
    async fn a_failed_handshake_is_not_kept_and_the_next_request_dials_again() {
        let pool = pool(Duration::from_secs(60));
        let net = Net::new();
        let (a, b, c) = tokio::join!(
            pool.acquire("k", net.failing_dial(Duration::from_millis(200))),
            pool.acquire("k", net.failing_dial(Duration::from_millis(200))),
            pool.acquire("k", net.failing_dial(Duration::from_millis(200))),
        );
        for result in [a, b, c] {
            assert_eq!(result.err().as_deref(), Some("认证失败"));
        }
        assert_eq!(net.dials(), 1, "三个等待者共用一次失败");
        assert_eq!(pool.len(), 0, "失败的占位不能留在表里");

        // 重试：这次成功。必须在有限时间内返回，而不是等一个再也不会有结果的占位。
        let retried = tokio::time::timeout(
            Duration::from_secs(5),
            pool.acquire("k", net.dial(Duration::from_millis(50))),
        )
        .await
        .expect("重试不能永久等待")
        .expect("重试成功");
        assert!(!retried.reused);
        assert_eq!(net.dials(), 2);
    }

    /// 配置不同（键不同）：各握各的，不会共用。
    #[tokio::test(start_paused = true)]
    async fn different_keys_never_share_a_connection() {
        let pool = pool(Duration::from_secs(60));
        let net = Net::new();
        let (a, b) = tokio::join!(
            pool.acquire("host|cfg-1", net.dial(Duration::from_millis(100))),
            pool.acquire("host|cfg-2", net.dial(Duration::from_millis(100))),
        );
        let (a, b) = (a.expect("a").item, b.expect("b").item);
        assert_eq!(net.dials(), 2);
        assert!(!Arc::ptr_eq(&a, &b));
        assert_ne!(a.id, b.id);
    }

    /// 一个等待者退出（哪怕就是最先来的那个）不能取消其他人还需要的连接。
    #[tokio::test(start_paused = true)]
    async fn a_waiter_leaving_does_not_cancel_the_shared_handshake() {
        let pool = pool(Duration::from_secs(60));
        let net = Net::new();
        let first = {
            let pool = pool.clone();
            let net = Arc::clone(&net);
            tokio::spawn(async move { pool.acquire("k", net.dial(Duration::from_secs(2))).await })
        };
        wait(Duration::from_millis(100)).await;
        let second = {
            let pool = pool.clone();
            let net = Arc::clone(&net);
            tokio::spawn(async move { pool.acquire("k", net.dial(Duration::from_secs(2))).await })
        };
        wait(Duration::from_millis(100)).await;
        // 发起握手的第一个请求被取消（比如它的窗格关了）。
        first.abort();
        let _ = first.await;
        let got = second.await.expect("任务").expect("第二个请求仍然拿到连接");
        assert_eq!(net.dials(), 1, "没有为了第二个请求重新握手");
        assert!(!got.item.is_closed());
    }

    /// 等待者全走光了：握手被中止（不再占着认证框），占位清掉，之后的请求能重新拨号。
    #[tokio::test(start_paused = true)]
    async fn when_every_waiter_leaves_the_handshake_is_cancelled() {
        let pool = pool(Duration::from_secs(60));
        let net = Net::new();
        let waiter = {
            let pool = pool.clone();
            let net = Arc::clone(&net);
            tokio::spawn(async move { pool.acquire("k", net.dial(Duration::from_secs(30))).await })
        };
        wait(Duration::from_secs(1)).await;
        assert_eq!(net.dials(), 1, "握手已经开始");
        waiter.abort();
        let _ = waiter.await;
        wait(Duration::from_secs(60)).await;
        assert!(
            net.close_counters.lock().unwrap().is_empty(),
            "没人等了，握手不该继续跑完"
        );
        assert_eq!(pool.len(), 0);

        let next = pool
            .acquire("k", net.dial(Duration::from_millis(10)))
            .await
            .expect("取消之后可以重新拨号");
        assert!(!next.reused);
        assert_eq!(net.dials(), 2);
    }

    /// 池里的连接已经断了：不复用，重新拨号。
    #[tokio::test(start_paused = true)]
    async fn a_dead_connection_is_replaced() {
        let pool = pool(Duration::from_secs(60));
        let net = Net::new();
        let first = pool
            .acquire("k", net.dial(Duration::ZERO))
            .await
            .expect("first")
            .item;
        first.closed.store(true, Ordering::SeqCst);
        let second = pool
            .acquire("k", net.dial(Duration::ZERO))
            .await
            .expect("second");
        assert!(!second.reused);
        assert!(!Arc::ptr_eq(&first, &second.item));
        assert_eq!(net.dials(), 2);
    }

    // ---------------------------------------------------------------- R07

    /// 两个不同的超时值分别验证：放手之后，没到点还开着，过了点就关。
    #[tokio::test(start_paused = true)]
    async fn the_idle_timeout_decides_when_a_released_connection_closes() {
        for secs in [5u64, 90] {
            let pool = pool(Duration::from_secs(secs));
            let net = Net::new();
            let item = pool
                .acquire("k", net.dial(Duration::ZERO))
                .await
                .expect("拿到")
                .item;
            drop(item);
            // 还差半秒：开着。
            wait(Duration::from_secs(secs) - Duration::from_millis(500)).await;
            assert_eq!(net.closes_of(0), 0, "超时 {secs}s：没到点不该关");
            assert_eq!(pool.len(), 1);
            // 过了点（再宽限一个巡查间隔）：关了，也从池里摘掉了。
            wait(Duration::from_millis(1500)).await;
            assert_eq!(net.closes_of(0), 1, "超时 {secs}s：到点要关");
            assert_eq!(pool.len(), 0);
        }
    }

    /// 有使用者的时候一直不计时；放手之后才开始。
    #[tokio::test(start_paused = true)]
    async fn the_countdown_starts_only_after_every_user_has_let_go() {
        let pool = pool(Duration::from_secs(30));
        let net = Net::new();
        let first = pool
            .acquire("k", net.dial(Duration::ZERO))
            .await
            .expect("拿到")
            .item;
        let second = pool.get("k").expect("第二个使用者");

        // 两个使用者握着，远超超时也不关。
        wait(Duration::from_secs(600)).await;
        assert_eq!(net.closes_of(0), 0);

        // 一个放手：还有一个在用，仍不关。
        drop(first);
        wait(Duration::from_secs(120)).await;
        assert_eq!(net.closes_of(0), 0, "还有使用者，不能关");

        // 最后一个放手：从这一刻起才计时。25 秒还开着，过了 30 秒才关。
        drop(second);
        wait(Duration::from_secs(25)).await;
        assert_eq!(net.closes_of(0), 0, "放手后没到点");
        wait(Duration::from_secs(6)).await;
        assert_eq!(net.closes_of(0), 1, "放手满 30 秒该关");
    }

    /// 只拿着内部那一份（对应只拿 `Arc<Connection>` 的 SFTP 通道 / 转发 / AI 工具通道）也是使用者。
    #[tokio::test(start_paused = true)]
    async fn holding_only_the_inner_handle_still_counts_as_using_it() {
        let pool = pool(Duration::from_secs(10));
        let net = Net::new();
        let item = pool
            .acquire("k", net.dial(Duration::ZERO))
            .await
            .expect("拿到")
            .item;
        let inner_handle = Arc::clone(&item.link);
        drop(item);

        wait(Duration::from_secs(300)).await;
        assert_eq!(
            net.closes_of(0),
            0,
            "有人还拿着内部那一份（比如 SFTP 通道），不能关"
        );

        drop(inner_handle);
        wait(Duration::from_secs(9)).await;
        assert_eq!(net.closes_of(0), 0);
        wait(Duration::from_secs(3)).await;
        assert_eq!(net.closes_of(0), 1);
    }

    /// 空闲计时中途又有人来用：计时作废，下次放手重新算。
    #[tokio::test(start_paused = true)]
    async fn reusing_a_connection_during_the_countdown_cancels_it() {
        let pool = pool(Duration::from_secs(20));
        let net = Net::new();
        let item = pool
            .acquire("k", net.dial(Duration::ZERO))
            .await
            .expect("拿到")
            .item;
        drop(item);
        wait(Duration::from_secs(15)).await;

        let again = pool
            .acquire("k", net.dial(Duration::ZERO))
            .await
            .expect("复用");
        assert!(again.reused);
        assert_eq!(net.dials(), 1);
        wait(Duration::from_secs(60)).await;
        assert_eq!(net.closes_of(0), 0, "有人在用，不能被上一轮计时关掉");

        drop(again);
        wait(Duration::from_secs(15)).await;
        assert_eq!(net.closes_of(0), 0, "新一轮计时刚走了 15 秒");
        wait(Duration::from_secs(8)).await;
        assert_eq!(net.closes_of(0), 1);
    }

    /// 运行中改超时：已经空闲的连接从改动那一刻起按新值重新计时。
    #[tokio::test(start_paused = true)]
    async fn changing_the_timeout_restarts_the_countdown_with_the_new_value() {
        let pool = pool(Duration::from_secs(30));
        let net = Net::new();
        let item = pool
            .acquire("k", net.dial(Duration::ZERO))
            .await
            .expect("拿到")
            .item;
        drop(item);
        wait(Duration::from_secs(20)).await;

        // 放大：已经空闲 20 秒，改成 100 秒后从现在重新数。
        pool.set_idle_timeout(Duration::from_secs(100));
        wait(Duration::from_secs(95)).await;
        assert_eq!(net.closes_of(0), 0, "改动后要满 100 秒才关");
        wait(Duration::from_secs(8)).await;
        assert_eq!(net.closes_of(0), 1);

        // 缩小：另一条连接空闲着，改成 0 就在下一次巡查时关。
        let second = pool
            .acquire("k2", net.dial(Duration::ZERO))
            .await
            .expect("拿到")
            .item;
        drop(second);
        wait(Duration::from_secs(10)).await;
        assert_eq!(net.closes_of(1), 0);
        pool.set_idle_timeout(Duration::ZERO);
        wait(Duration::from_millis(500)).await;
        assert_eq!(net.closes_of(1), 1, "改成 0 = 没人用就关");
    }

    /// 超时为 0：最后一个使用者放手后（下一次巡查）就关；有人用时不关。
    #[tokio::test(start_paused = true)]
    async fn zero_timeout_closes_as_soon_as_nobody_is_using_it() {
        let pool = pool(Duration::ZERO);
        let net = Net::new();
        let item = pool
            .acquire("k", net.dial(Duration::ZERO))
            .await
            .expect("拿到")
            .item;
        wait(Duration::from_secs(30)).await;
        assert_eq!(net.closes_of(0), 0, "拿着的时候不能关");
        drop(item);
        wait(Duration::from_millis(500)).await;
        assert_eq!(net.closes_of(0), 1, "只关一次");
    }

    /// 对端自己断了的连接：巡查把它从表里清掉，不会一直占位。
    #[tokio::test(start_paused = true)]
    async fn a_connection_closed_by_the_peer_is_dropped_from_the_table() {
        let pool = pool(Duration::from_secs(600));
        let net = Net::new();
        let item = pool
            .acquire("k", net.dial(Duration::ZERO))
            .await
            .expect("拿到")
            .item;
        item.closed.store(true, Ordering::SeqCst);
        wait(Duration::from_secs(1)).await;
        assert_eq!(pool.len(), 0);
        assert!(pool.get("k").is_none());
    }

    /// `forget` 只摘同一条：已经换上的新连接不受影响。
    #[tokio::test(start_paused = true)]
    async fn forget_only_removes_the_same_connection() {
        let pool = pool(Duration::from_secs(60));
        let net = Net::new();
        let old = pool
            .acquire("k", net.dial(Duration::ZERO))
            .await
            .expect("old")
            .item;
        old.closed.store(true, Ordering::SeqCst);
        let fresh = pool
            .acquire("k", net.dial(Duration::ZERO))
            .await
            .expect("fresh")
            .item;
        pool.forget("k", &old);
        assert!(
            pool.get("k").is_some_and(|got| Arc::ptr_eq(&got, &fresh)),
            "不能把新连接一起摘掉"
        );
        pool.forget("k", &fresh);
        assert!(pool.get("k").is_none());
    }
}
