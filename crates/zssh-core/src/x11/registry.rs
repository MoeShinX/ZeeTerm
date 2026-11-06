//! 连接级的 X11 分发表。
//!
//! 连接池让多个终端共用一条 SSH 连接，而 X11 通道是服务器在**连接**上开出来的，通道本身不带
//! "是哪个终端"。能区分的只有握手包里的 cookie：每个开了 X11 的 shell 向服务器登记一个自己的
//! 假 cookie，这里记着"假 cookie → 这个终端该连的本机 X 服务器和真 cookie"，通道进来时按握手包里
//! 的 cookie 查表。查不到就拒绝——不能"凑合用某一个"，否则 A 终端的 X 窗口会开到 B 终端的显示上。
//!
//! 上限都在这里：同时登记的终端数、每个终端同时开着的通道数、整条连接的通道总数。
//! 终端关闭（[`X11Registration`] 被丢弃）时登记项消失，并通知它名下还在转发的任务退出。

use crate::error::{reason_keys, Reason};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};

use super::cookie::{encode_hex, random_cookie};
use super::display::XEndpoint;
use super::failure::X11Failure;

/// 一条连接上最多同时有几个终端开着 X11。
pub const MAX_REGISTRATIONS: usize = 32;
/// 一个终端名下最多同时开着几条 X11 通道（一个 X 程序通常开一两条，大量窗口程序也不会上百）。
pub const MAX_CHANNELS_PER_REGISTRATION: usize = 64;
/// 一条连接上所有终端合计最多几条 X11 通道。
pub const MAX_CHANNELS_TOTAL: usize = 256;

/// 失败回调。回调里不能阻塞，也不能再回头调用分发表。
pub type FailureSink = Arc<dyn Fn(&X11Failure) + Send + Sync>;

/// 一个终端的转发目标：连哪个本机 X 服务器，用哪个真 cookie。
#[derive(Clone, PartialEq, Eq)]
pub struct X11Route {
    pub endpoint: XEndpoint,
    /// 本机 X 服务器的真 cookie；`None` = 拿不到（用户自己的 X 服务器），握手包原样透传。
    pub real_cookie: Option<Vec<u8>>,
}

impl std::fmt::Debug for X11Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 真 cookie 是本机 X 服务器的口令，不进日志。
        f.debug_struct("X11Route")
            .field("endpoint", &self.endpoint)
            .field(
                "real_cookie",
                &self.real_cookie.as_ref().map(|_| "<hidden>"),
            )
            .finish()
    }
}

/// 打开一个 shell 时对 X11 的要求。
#[derive(Clone)]
pub struct X11Request {
    pub route: X11Route,
    /// 界面想拿到结构化失败时设；终端里的提示不依赖它，总会写。
    pub on_failure: Option<FailureSink>,
}

/// 登记项：一个终端的路由和它名下通道的共同状态。
pub(crate) struct RouteEntry {
    pub(crate) route: X11Route,
    /// 这个终端名下同时开着的通道数。
    pub(crate) permits: Arc<Semaphore>,
    /// 终端关闭时置 true，名下的转发任务据此退出。
    cancel: watch::Sender<bool>,
    terminal_sink: Mutex<Option<FailureSink>>,
    observer: Mutex<Option<FailureSink>>,
    /// 已经报告过的失败代码：同一个原因在一个终端里只提示一次，不刷屏。
    reported: Mutex<HashSet<&'static str>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl RouteEntry {
    pub(crate) fn cancelled(&self) -> watch::Receiver<bool> {
        self.cancel.subscribe()
    }

    /// 报告一次失败：同一个代码只报一次；终端提示和界面回调各自取一份。
    pub(crate) fn report(&self, failure: &X11Failure) {
        if !lock(&self.reported).insert(failure.code()) {
            return;
        }
        // 先把回调克隆出来再调，回调执行期间不持有任何锁。
        let terminal = lock(&self.terminal_sink).clone();
        let observer = lock(&self.observer).clone();
        if let Some(sink) = terminal {
            sink(failure);
        }
        if let Some(sink) = observer {
            sink(failure);
        }
    }
}

struct HubInner {
    routes: Mutex<HashMap<Vec<u8>, Arc<RouteEntry>>>,
    total: Arc<Semaphore>,
}

/// 一条 SSH 连接的 X11 分发表。克隆出来的是同一张表。
#[derive(Clone)]
pub struct X11Hub {
    inner: Arc<HubInner>,
}

impl Default for X11Hub {
    fn default() -> Self {
        Self {
            inner: Arc::new(HubInner {
                routes: Mutex::new(HashMap::new()),
                total: Arc::new(Semaphore::new(MAX_CHANNELS_TOTAL)),
            }),
        }
    }
}

impl X11Hub {
    /// 为一个终端登记转发：生成它专属的假 cookie。
    pub fn register(&self, route: X11Route) -> Result<X11Registration, X11Failure> {
        let mut routes = lock(&self.inner.routes);
        if routes.len() >= MAX_REGISTRATIONS {
            return Err(X11Failure::NotRegistered {
                reason: Reason::new(
                    reason_keys::X11_MAX_REG,
                    format!("这条连接上已经有 {MAX_REGISTRATIONS} 个终端在用 X11 转发"),
                )
                .with("max", MAX_REGISTRATIONS),
            });
        }
        // 16 字节随机数撞车的概率可以忽略，仍然重试几次而不是覆盖别人的登记项。
        for _ in 0..4 {
            let cookie = random_cookie().ok_or_else(|| X11Failure::NotRegistered {
                reason: Reason::new(reason_keys::X11_RANDOM, "系统随机数不可用"),
            })?;
            if routes.contains_key(&cookie[..]) {
                continue;
            }
            let (cancel, _) = watch::channel(false);
            let entry = Arc::new(RouteEntry {
                route,
                permits: Arc::new(Semaphore::new(MAX_CHANNELS_PER_REGISTRATION)),
                cancel,
                terminal_sink: Mutex::new(None),
                observer: Mutex::new(None),
                reported: Mutex::new(HashSet::new()),
            });
            routes.insert(cookie.to_vec(), Arc::clone(&entry));
            return Ok(X11Registration {
                hub: self.clone(),
                cookie: cookie.to_vec(),
                entry,
            });
        }
        Err(X11Failure::NotRegistered {
            reason: Reason::new(
                reason_keys::X11_COOKIE_UNIQUE,
                "生成不重复的随机 cookie 失败",
            ),
        })
    }

    /// 现在有几个终端登记着 X11。
    pub fn registration_count(&self) -> usize {
        lock(&self.inner.routes).len()
    }

    /// 服务器想开一条 X11 通道时先过这一关：没有任何终端登记，或者通道总数到上限，就不收。
    pub(crate) fn admit(&self) -> Option<OwnedSemaphorePermit> {
        if lock(&self.inner.routes).is_empty() {
            return None;
        }
        Arc::clone(&self.inner.total).try_acquire_owned().ok()
    }

    /// 按握手包里的 cookie 查登记项。
    pub(crate) fn lookup(&self, cookie: &[u8]) -> Option<Arc<RouteEntry>> {
        lock(&self.inner.routes).get(cookie).cloned()
    }

    /// 没有登记项可归属的失败（比如 cookie 对不上）：通知所有终端，让用户看到有这回事。
    pub(crate) fn report_to_all(&self, failure: &X11Failure) {
        let entries: Vec<Arc<RouteEntry>> = lock(&self.inner.routes).values().cloned().collect();
        for entry in entries {
            entry.report(failure);
        }
    }

    fn remove(&self, cookie: &[u8]) {
        lock(&self.inner.routes).remove(cookie);
    }
}

/// 一个终端的 X11 登记。丢弃 = 撤销登记并让它名下的转发任务退出。
pub struct X11Registration {
    hub: X11Hub,
    cookie: Vec<u8>,
    entry: Arc<RouteEntry>,
}

impl X11Registration {
    /// 发给服务器的假 cookie（十六进制，`request_x11` 要的格式）。
    pub fn cookie_hex(&self) -> String {
        encode_hex(&self.cookie)
    }

    /// `request_x11` 的屏幕号。
    pub fn screen(&self) -> u32 {
        self.entry.route.endpoint.screen
    }

    /// 本机 X 服务器的地址（`主机:端口`）。
    pub fn display(&self) -> String {
        self.entry.route.endpoint.describe()
    }

    /// 终端里的提示通道：失败时往这个终端写一句中文。
    pub fn set_terminal_sink(&self, sink: FailureSink) {
        *lock(&self.entry.terminal_sink) = Some(sink);
    }

    /// 界面的结构化失败回调。
    pub fn set_observer(&self, sink: FailureSink) {
        *lock(&self.entry.observer) = Some(sink);
    }

    /// 自己主动报告一次失败（比如服务器拒绝时由打开流程报）。
    pub fn report(&self, failure: &X11Failure) {
        self.entry.report(failure);
    }
}

impl Drop for X11Registration {
    fn drop(&mut self) {
        self.hub.remove(&self.cookie);
        // lookup 已取走 entry、但 relay 尚未订阅时也必须保留取消状态。
        self.entry.cancel.send_replace(true);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn route(display: u16) -> X11Route {
        X11Route {
            endpoint: XEndpoint::loopback(display),
            real_cookie: Some(vec![display as u8; 16]),
        }
    }

    #[test]
    fn each_registration_gets_its_own_fake_cookie_and_is_found_by_it() {
        let hub = X11Hub::default();
        let a = hub.register(route(1)).expect("登记 a");
        let b = hub.register(route(2)).expect("登记 b");
        assert_ne!(a.cookie_hex(), b.cookie_hex());
        assert_eq!(a.cookie_hex().len(), 32);
        assert_eq!(hub.registration_count(), 2);

        let found_a = hub.lookup(&a.cookie).expect("按 a 的 cookie 查");
        assert_eq!(found_a.route.endpoint.port, 6001);
        let found_b = hub.lookup(&b.cookie).expect("按 b 的 cookie 查");
        assert_eq!(found_b.route.endpoint.port, 6002);
        assert!(hub.lookup(&[0u8; 16]).is_none(), "不认识的 cookie 查不到");
        assert!(hub.lookup(b"").is_none());
    }

    #[test]
    fn dropping_a_registration_removes_it_and_signals_cancel() {
        let hub = X11Hub::default();
        let reg = hub.register(route(1)).expect("登记");
        let cookie = reg.cookie.clone();
        let mut cancelled = reg.entry.cancelled();
        assert!(!*cancelled.borrow_and_update());
        drop(reg);
        assert!(hub.lookup(&cookie).is_none());
        assert_eq!(hub.registration_count(), 0);
        assert!(*cancelled.borrow_and_update(), "名下的转发任务要收到取消");
    }

    #[test]
    fn cancellation_survives_registration_drop_before_subscription() {
        let hub = X11Hub::default();
        let reg = hub.register(route(1)).expect("登记");
        let entry = hub.lookup(&reg.cookie).expect("先查表，尚未订阅");
        drop(reg);
        assert!(*entry.cancelled().borrow(), "迟到订阅仍须看到取消");
        assert_eq!(hub.registration_count(), 0);
    }

    #[test]
    fn registrations_are_capped() {
        let hub = X11Hub::default();
        let mut keep = Vec::new();
        for _ in 0..MAX_REGISTRATIONS {
            keep.push(hub.register(route(1)).expect("在上限内"));
        }
        let err = hub.register(route(1)).err().expect("超过上限");
        assert_eq!(err.code(), "not_registered");
        keep.pop();
        hub.register(route(1)).expect("腾出一个位置后又能登记");
    }

    #[test]
    fn channels_are_admitted_only_with_a_registration_and_within_the_cap() {
        let hub = X11Hub::default();
        assert!(hub.admit().is_none(), "没人登记时不收通道");
        let reg = hub.register(route(1)).expect("登记");
        let mut permits = Vec::new();
        for _ in 0..MAX_CHANNELS_TOTAL {
            permits.push(hub.admit().expect("在上限内"));
        }
        assert!(hub.admit().is_none(), "总数到上限");
        permits.pop();
        assert!(hub.admit().is_some(), "放掉一个又能收");
        drop(reg);
        assert!(hub.admit().is_none(), "登记没了又不收");
    }

    #[test]
    fn a_failure_is_reported_once_per_code_to_both_sinks() {
        let hub = X11Hub::default();
        let reg = hub.register(route(1)).expect("登记");
        let terminal = Arc::new(AtomicUsize::new(0));
        let observed = Arc::new(AtomicUsize::new(0));
        let t = Arc::clone(&terminal);
        reg.set_terminal_sink(Arc::new(move |_| {
            t.fetch_add(1, Ordering::SeqCst);
        }));
        let o = Arc::clone(&observed);
        reg.set_observer(Arc::new(move |_| {
            o.fetch_add(1, Ordering::SeqCst);
        }));
        reg.report(&X11Failure::UnknownCookie);
        reg.report(&X11Failure::UnknownCookie);
        reg.report(&X11Failure::BadSetup);
        assert_eq!(terminal.load(Ordering::SeqCst), 2, "两个不同代码各一次");
        assert_eq!(observed.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn report_to_all_reaches_every_terminal() {
        let hub = X11Hub::default();
        let a = hub.register(route(1)).expect("a");
        let b = hub.register(route(2)).expect("b");
        let hits = Arc::new(AtomicUsize::new(0));
        for reg in [&a, &b] {
            let hits = Arc::clone(&hits);
            reg.set_observer(Arc::new(move |_| {
                hits.fetch_add(1, Ordering::SeqCst);
            }));
        }
        hub.report_to_all(&X11Failure::UnknownCookie);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn route_debug_never_prints_the_real_cookie() {
        let text = format!("{:?}", route(7));
        assert!(text.contains("<hidden>"), "{text}");
        assert!(!text.contains("[7, 7"), "{text}");
    }
}
