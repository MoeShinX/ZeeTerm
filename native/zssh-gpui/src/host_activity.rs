//! 进程级的「AI 正在使用哪台主机」记录。
//!
//! AI 对话窗口在真实回合开始 / 结束时登记主机 id；主机库窗口只看这个集合，
//! 在自己的行上画「AI 使用中」标记。离线演示的回合不登记，也不代表有进程在跑。
//!
//! 一个进程里可能开着多个对话窗口：集合按主机 id 去重，同一台主机只留一份。
//! 增删都是幂等的，迟到的「结束」不会把别人还在用的主机清掉；
//! 极端情况下（同一个窗口重复登记）也只算一次，因为这里是集合不是计数。

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

/// 空闲主机集合；第一次用到时才建。
fn active_hosts() -> &'static Mutex<HashSet<i64>> {
    static ACTIVE: OnceLock<Mutex<HashSet<i64>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 拿到集合再做一次操作。锁被毒掉时继续用里面的值：这里只有集合增删，
/// 没有留下半截状态的操作，没必要让整个窗口跟着崩。
fn with_active<T>(f: impl FnOnce(&mut HashSet<i64>) -> T) -> T {
    match active_hosts().lock() {
        Ok(mut guard) => f(&mut guard),
        Err(poisoned) => f(&mut poisoned.into_inner()),
    }
}

/// AI 开始在一个真实回合里使用这台主机。
pub fn turn_started(host_id: i64) {
    with_active(|active| {
        active.insert(host_id);
    });
}

/// AI 用完这台主机；重复调用没有副作用。
pub fn turn_finished(host_id: i64) {
    with_active(|active| {
        active.remove(&host_id);
    });
}

/// 这台主机现在有没有 AI 在使用。
pub fn is_active(host_id: i64) -> bool {
    with_active(|active| active.contains(&host_id))
}

/// 当前在用的主机 id，升序；主机库窗口拿它判断要不要重绘。
pub fn snapshot() -> Vec<i64> {
    with_active(|active| {
        let mut ids: Vec<i64> = active.iter().copied().collect();
        ids.sort_unstable();
        ids
    })
}

/// 主机行上的标记文字；没有 AI 在用时返回 `None`。
pub fn activity_label(active: bool) -> Option<&'static str> {
    if active { Some("AI 使用中") } else { None }
}

/// AI 改了哪台主机的长期记忆（0.2.14 的 `host-memory-changed` 事件）：主机详情里开着的记忆框要刷新。
/// 进程级的小账本，只留最近若干条；界面每个节拍拿着自己上次看到的序号来问「之后有哪些主机变了」。
type MemoryLog = (u64, Vec<(u64, i64)>);

fn memory_log() -> &'static Mutex<MemoryLog> {
    static LOG: OnceLock<Mutex<MemoryLog>> = OnceLock::new();
    LOG.get_or_init(|| Mutex::new((0, Vec::new())))
}

/// 记一笔：AI 改了这台主机的记忆。
pub fn memory_changed(host_id: i64) {
    let mut log = memory_log().lock().unwrap_or_else(|p| p.into_inner());
    log.0 += 1;
    let version = log.0;
    log.1.push((version, host_id));
    if log.1.len() > 64 {
        let drop = log.1.len() - 64;
        log.1.drain(..drop);
    }
}

/// 当前的序号（界面第一次看时用它当起点，只关心之后发生的变化）。
pub fn memory_version() -> u64 {
    memory_log().lock().unwrap_or_else(|p| p.into_inner()).0
}

/// `since` 之后记忆变了的主机（去重，升序）和新的序号。
pub fn memory_changes_since(since: u64) -> (u64, Vec<i64>) {
    let log = memory_log().lock().unwrap_or_else(|p| p.into_inner());
    let mut hosts: Vec<i64> = log
        .1
        .iter()
        .filter(|(version, _)| *version > since)
        .map(|(_, host)| *host)
        .collect();
    hosts.sort_unstable();
    hosts.dedup();
    (log.0, hosts)
}

#[cfg(test)]
mod tests {
    use super::{activity_label, is_active, snapshot, turn_finished, turn_started, with_active};
    use std::sync::{Mutex, MutexGuard};

    /// 集合是进程级的，几个测试并行跑会互相踩；拿锁串起来，进出都清空。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    struct TestGuard {
        _lock: MutexGuard<'static, ()>,
    }

    impl TestGuard {
        fn new() -> Self {
            let lock = TEST_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            with_active(|active| active.clear());
            Self { _lock: lock }
        }
    }

    impl Drop for TestGuard {
        fn drop(&mut self) {
            with_active(|active| active.clear());
        }
    }

    #[test]
    fn starting_and_finishing_marks_one_host() {
        let _guard = TestGuard::new();
        assert!(!is_active(1));
        assert_eq!(snapshot(), Vec::<i64>::new());

        turn_started(1);
        assert!(is_active(1));
        assert_eq!(snapshot(), vec![1]);

        turn_finished(1);
        assert!(!is_active(1));
        assert_eq!(snapshot(), Vec::<i64>::new());
    }

    #[test]
    fn finishing_twice_keeps_other_hosts() {
        let _guard = TestGuard::new();
        turn_started(2);
        turn_started(3);

        turn_finished(2);
        turn_finished(2);
        assert!(!is_active(2));
        assert!(is_active(3));
        assert_eq!(snapshot(), vec![3]);
    }

    #[test]
    fn starting_twice_still_counts_as_one_host() {
        let _guard = TestGuard::new();
        turn_started(4);
        turn_started(4);
        assert_eq!(snapshot(), vec![4]);

        turn_finished(4);
        assert!(!is_active(4));
    }

    #[test]
    fn multiple_hosts_are_reported_sorted() {
        let _guard = TestGuard::new();
        for id in [9, 3, 7] {
            turn_started(id);
        }
        assert_eq!(snapshot(), vec![3, 7, 9]);
        assert!(is_active(3) && is_active(7) && is_active(9));

        turn_finished(7);
        assert_eq!(snapshot(), vec![3, 9]);
    }

    #[test]
    fn unknown_hosts_are_never_active() {
        let _guard = TestGuard::new();
        assert!(!is_active(42));
        turn_finished(42);
        assert_eq!(snapshot(), Vec::<i64>::new());
    }

    #[test]
    fn the_marker_text_follows_the_active_flag() {
        let _guard = TestGuard::new();
        assert_eq!(activity_label(true), Some("AI 使用中"));
        assert_eq!(activity_label(false), None);
    }
}
