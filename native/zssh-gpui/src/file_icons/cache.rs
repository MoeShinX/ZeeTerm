//! 系统文件图标的后台查询 + 进程级缓存（和平台无关，查询函数可替换，所以能在任何平台上测）。
//!
//! 为什么要放到后台：第一次向外壳要某个扩展名的图标，可能要加载那个类型的外壳扩展（压缩软件、
//! 网盘、办公套件的图标处理器），慢的要几百毫秒，放在界面线程会卡住整个列表。所以：
//! - 界面线程只做 [`Resolver::lookup`]：命中就直接给结果，没命中登记一条「查询中」并把请求交给唯一的后台线程；
//! - 同一个键在「查询中」期间不会再次登记，所以同一扩展名同时只会查一次，查完（包括「查不到」）也只记一次；
//! - 每查完一个，[`Resolver::generation`] 加一，界面据此在下一拍重画列表。

use std::collections::HashMap;
use std::hash::Hash;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

/// 一次查询的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Lookup<T> {
    /// 还在后台查，先画内置图标。
    Pending,
    /// 查过了，系统没给可用的图标（没有关联 / 只有通用图标 / 出错）：用内置图标。
    Missing,
    Ready(T),
}

enum Slot<T> {
    Pending,
    Done(Option<T>),
}

struct Shared<K, T> {
    slots: Mutex<HashMap<K, Slot<T>>>,
    generation: AtomicU64,
}

/// 缓存 + 后台线程。键是 `(扩展名, 像素尺寸)` 这类小而有限的东西；值是查到的图标。
pub(super) struct Resolver<K, T> {
    shared: Arc<Shared<K, T>>,
    requests: Mutex<Sender<K>>,
    /// 最多缓存多少个键：远端目录里的文件名由对方说了算，扩展名五花八门的目录不能让缓存和后台线程无限增长。
    capacity: usize,
}

impl<K, T> Resolver<K, T>
where
    K: Clone + Eq + Hash + Send + 'static,
    T: Clone + Send + 'static,
{
    /// `on_start` 在后台线程里先跑一次（Windows 上用来初始化 COM），`query` 每个键调用一次。
    pub(super) fn new(
        capacity: usize,
        on_start: impl FnOnce() + Send + 'static,
        query: impl Fn(&K) -> Option<T> + Send + 'static,
    ) -> Self {
        let shared = Arc::new(Shared {
            slots: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(0),
        });
        let (tx, rx) = channel::<K>();
        let worker = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("file-icon-query".into())
            .spawn(move || {
                on_start();
                while let Ok(key) = rx.recv() {
                    // 外壳扩展是第三方代码，panic 了也要把「查询中」收尾成「查不到」，
                    // 否则这个键永远停在 Pending，后面的请求也没人处理。
                    let found = catch_unwind(AssertUnwindSafe(|| query(&key))).unwrap_or(None);
                    if let Ok(mut slots) = worker.slots.lock() {
                        slots.insert(key, Slot::Done(found));
                    }
                    worker.generation.fetch_add(1, Ordering::Release);
                }
            });
        if let Err(error) = spawned {
            // 起不来线程：之后每次请求发送都会失败，全部按「查不到」处理，界面照常用内置图标。
            tracing::warn!(%error, "file icon query thread did not start");
        }
        Self {
            shared,
            requests: Mutex::new(tx),
            capacity,
        }
    }

    /// 界面线程调用：不阻塞，只在第一次见到这个键时登记并发请求。
    pub(super) fn lookup(&self, key: &K) -> Lookup<T> {
        let Ok(mut slots) = self.shared.slots.lock() else {
            return Lookup::Missing;
        };
        match slots.get(key) {
            Some(Slot::Pending) => return Lookup::Pending,
            Some(Slot::Done(Some(value))) => return Lookup::Ready(value.clone()),
            Some(Slot::Done(None)) => return Lookup::Missing,
            None => {}
        }
        if slots.len() >= self.capacity {
            return Lookup::Missing;
        }
        let sent = self
            .requests
            .lock()
            .map(|tx| tx.send(key.clone()).is_ok())
            .unwrap_or(false);
        if sent {
            slots.insert(key.clone(), Slot::Pending);
            Lookup::Pending
        } else {
            slots.insert(key.clone(), Slot::Done(None));
            Lookup::Missing
        }
    }

    /// 每完成一次查询加一：界面记住上次看到的值，变了就重画。
    pub(super) fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    use super::*;

    /// 等到这个键不再是「查询中」。
    fn settle(resolver: &Resolver<String, u32>, key: &str) -> Lookup<u32> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match resolver.lookup(&key.to_owned()) {
                Lookup::Pending if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                other => return other,
            }
        }
    }

    fn counting(calls: &Arc<AtomicUsize>, capacity: usize) -> Resolver<String, u32> {
        let calls = Arc::clone(calls);
        Resolver::new(
            capacity,
            || {},
            move |key: &String| {
                calls.fetch_add(1, Ordering::SeqCst);
                // 名字带 none 的查不到，其余返回键长度当「图标」。
                (!key.contains("none")).then_some(key.len() as u32)
            },
        )
    }

    #[test]
    fn the_same_key_is_queried_only_once_however_often_it_is_looked_up() {
        let calls = Arc::new(AtomicUsize::new(0));
        let resolver = counting(&calls, 16);
        // 连着问很多次：第一次是「查询中」，之后要么「查询中」要么已经有结果，但绝不会再发第二次请求。
        for _ in 0..50 {
            let _ = resolver.lookup(&"zip".to_owned());
        }
        assert_eq!(settle(&resolver, "zip"), Lookup::Ready(3));
        for _ in 0..50 {
            assert_eq!(resolver.lookup(&"zip".to_owned()), Lookup::Ready(3));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_key_the_system_has_no_icon_for_is_cached_too() {
        let calls = Arc::new(AtomicUsize::new(0));
        let resolver = counting(&calls, 16);
        assert_eq!(settle(&resolver, "none1"), Lookup::Missing);
        for _ in 0..20 {
            assert_eq!(resolver.lookup(&"none1".to_owned()), Lookup::Missing);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "查不到的结果也要缓存");
    }

    #[test]
    fn the_first_lookup_returns_at_once_without_waiting_for_the_query() {
        // 查询故意很慢：lookup 必须马上返回「查询中」，不能卡调用方（界面线程）。
        let resolver: Resolver<String, u32> = Resolver::new(
            16,
            || {},
            |_| {
                std::thread::sleep(Duration::from_millis(400));
                Some(1)
            },
        );
        let started = Instant::now();
        assert_eq!(resolver.lookup(&"slow".to_owned()), Lookup::Pending);
        assert!(started.elapsed() < Duration::from_millis(200));
        assert_eq!(settle(&resolver, "slow"), Lookup::Ready(1));
    }

    #[test]
    fn every_finished_query_bumps_the_generation_so_the_list_repaints() {
        let calls = Arc::new(AtomicUsize::new(0));
        let resolver = counting(&calls, 16);
        let before = resolver.generation();
        let _ = settle(&resolver, "a");
        let after_one = resolver.generation();
        assert!(after_one > before);
        let _ = settle(&resolver, "b");
        assert!(resolver.generation() > after_one);
    }

    #[test]
    fn a_panicking_query_ends_as_missing_and_the_worker_keeps_serving() {
        let resolver: Resolver<String, u32> = Resolver::new(
            16,
            || {},
            |key: &String| {
                if key == "boom" {
                    panic!("shell extension blew up");
                }
                Some(7)
            },
        );
        assert_eq!(settle(&resolver, "boom"), Lookup::Missing);
        assert_eq!(settle(&resolver, "fine"), Lookup::Ready(7));
    }

    #[test]
    fn the_cache_stops_taking_new_keys_at_its_capacity() {
        let calls = Arc::new(AtomicUsize::new(0));
        let resolver = counting(&calls, 2);
        assert_eq!(settle(&resolver, "a"), Lookup::Ready(1));
        assert_eq!(settle(&resolver, "b"), Lookup::Ready(1));
        // 满了：新键不再发请求，直接按「查不到」画内置图标；已有的照常命中。
        assert_eq!(resolver.lookup(&"c".to_owned()), Lookup::Missing);
        assert_eq!(resolver.lookup(&"a".to_owned()), Lookup::Ready(1));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn on_start_runs_on_the_worker_thread_before_the_first_query() {
        let main = std::thread::current().id();
        let started_on = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&started_on);
        let resolver: Resolver<String, u32> = Resolver::new(
            4,
            move || {
                if let Ok(mut guard) = slot.lock() {
                    *guard = Some(std::thread::current().id());
                }
            },
            |_| Some(1),
        );
        assert_eq!(settle(&resolver, "x"), Lookup::Ready(1));
        let id = started_on.lock().map(|g| *g).unwrap_or(None);
        assert!(id.is_some_and(|id| id != main), "初始化要在后台线程里做");
    }
}
