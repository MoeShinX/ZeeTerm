//! 比传输行先到的进度报告的暂存。
//!
//! 粘贴的任务在后台排进引擎，行要等粘贴做完才在界面线程建；小文件可能在行建好之前就已经传完，
//! 它的进度 / 终态报告先到了。这里把它们暂存起来，行建好时补上。
//!
//! 暂存有上限（防止一直建不出行的任务把内存吃光），但上限不能丢终态：
//! - 已有任务号的报告总是替换成最新的（同一个任务只占一格）；
//! - 满了之后来了新任务号，优先淘汰最旧的**非终态**报告；全是终态时淘汰最旧的终态；
//!   新来的是非终态而暂存里全是终态，则丢掉新来的（它只是进度，行建好时还会跟引擎的快照对账）。

use std::collections::HashMap;

use zssh_core::sftp::transfer::{ProgressReport, TransferId};

/// 暂存上限：同时有这么多个还没建行的任务已经很不寻常。
pub(super) const ORPHAN_CAP: usize = 256;

pub(super) struct OrphanReports {
    cap: usize,
    /// 单调递增的到达序号：淘汰时按它找"最旧"。
    next_seq: u64,
    map: HashMap<TransferId, (u64, ProgressReport)>,
}

impl OrphanReports {
    pub(super) fn new() -> Self {
        Self::with_cap(ORPHAN_CAP)
    }

    pub(super) fn with_cap(cap: usize) -> Self {
        Self {
            cap,
            next_seq: 0,
            map: HashMap::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.map.len()
    }

    /// 暂存一份报告（规则见模块说明）。
    pub(super) fn insert(&mut self, report: ProgressReport) {
        self.next_seq += 1;
        let seq = self.next_seq;
        let id = report.id;
        if let Some(slot) = self.map.get_mut(&id) {
            *slot = (seq, report);
            return;
        }
        if self.map.len() >= self.cap {
            let terminal_new = report.state.is_terminal();
            let victim = self.oldest(|r| !r.state.is_terminal()).or_else(|| {
                // 暂存里全是终态：新来的也是终态才值得换掉最旧的终态。
                terminal_new.then(|| self.oldest(|_| true)).flatten()
            });
            match victim {
                Some(victim) => {
                    self.map.remove(&victim);
                }
                None => return,
            }
        }
        self.map.insert(id, (seq, report));
    }

    fn oldest(&self, pick: impl Fn(&ProgressReport) -> bool) -> Option<TransferId> {
        self.map
            .iter()
            .filter(|(_, (_, report))| pick(report))
            .min_by_key(|(_, (seq, _))| *seq)
            .map(|(id, _)| *id)
    }

    /// 取走（行建好了）。
    pub(super) fn take(&mut self, id: TransferId) -> Option<ProgressReport> {
        self.map.remove(&id).map(|(_, report)| report)
    }

    /// 引擎换了：旧引擎的任务不会再有后续报告，也不会再有行，全部作废。
    pub(super) fn clear(&mut self) {
        self.map.clear();
    }
}

/// 行建好时该套哪份报告：引擎里这个任务的当前快照永远不比暂存的旧，有它就用它（对账）；
/// 引擎里已经没有这个任务（例如已清理）才退回暂存的那份。
pub(super) fn reconcile(
    snapshot: Option<ProgressReport>,
    early: Option<ProgressReport>,
) -> Option<ProgressReport> {
    snapshot.or(early)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zssh_core::sftp::transfer::{Direction, TransferState};

    fn report(id: TransferId, state: TransferState, done_bytes: u64) -> ProgressReport {
        ProgressReport {
            id,
            direction: Direction::RemoteCopy,
            state,
            total_files: 1,
            done_files: 0,
            total_bytes: 100,
            done_bytes,
            speed_bps: 0,
            eta_secs: None,
            current: None,
            notes: Vec::new(),
        }
    }

    #[test]
    fn a_known_id_is_always_updated_even_when_full() {
        let mut cache = OrphanReports::with_cap(2);
        cache.insert(report(1, TransferState::Running, 10));
        cache.insert(report(2, TransferState::Running, 10));
        // 满了：已有 id 的终态更新必须进去（旧实现在这里整个丢掉）。
        cache.insert(report(1, TransferState::Done, 100));
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.take(1).map(|r| r.state), Some(TransferState::Done));
        assert_eq!(cache.take(2).map(|r| r.done_bytes), Some(10));
    }

    #[test]
    fn a_full_cache_evicts_the_oldest_unfinished_report_first() {
        let mut cache = OrphanReports::with_cap(3);
        cache.insert(report(1, TransferState::Running, 1));
        cache.insert(report(2, TransferState::Done, 100));
        cache.insert(report(3, TransferState::Running, 3));
        // 新任务来了：淘汰最旧的非终态（1），已完成的 2 保住。
        cache.insert(report(4, TransferState::Done, 100));
        assert!(cache.take(1).is_none());
        assert!(cache.take(2).is_some());
        assert!(cache.take(3).is_some());
        assert!(cache.take(4).is_some());
    }

    #[test]
    fn a_terminal_report_survives_many_newer_unfinished_ones() {
        let mut cache = OrphanReports::with_cap(4);
        cache.insert(report(1, TransferState::Failed("x".into()), 0));
        // 257+ 个快速任务：后面不断来非终态的新任务，最早那份终态不能被挤掉。
        for id in 2..300 {
            cache.insert(report(id, TransferState::Running, 1));
        }
        assert_eq!(cache.len(), 4);
        assert_eq!(
            cache.take(1).map(|r| r.state),
            Some(TransferState::Failed("x".into()))
        );
    }

    #[test]
    fn all_terminal_cache_replaces_the_oldest_terminal_with_a_new_terminal_only() {
        let mut cache = OrphanReports::with_cap(2);
        cache.insert(report(1, TransferState::Done, 100));
        cache.insert(report(2, TransferState::Done, 100));
        // 新来的只是进度：不值得挤掉终态。
        cache.insert(report(3, TransferState::Running, 5));
        assert!(cache.take(3).is_none());
        assert_eq!(cache.len(), 2);
        // 新来的是终态：换掉最旧的终态。
        cache.insert(report(4, TransferState::Cancelled, 0));
        assert!(cache.take(1).is_none());
        assert!(cache.take(2).is_some());
        assert!(cache.take(4).is_some());
    }

    #[test]
    fn clear_drops_everything_from_the_old_engine() {
        let mut cache = OrphanReports::with_cap(4);
        cache.insert(report(1, TransferState::Done, 100));
        cache.clear();
        assert_eq!(cache.len(), 0);
        assert!(cache.take(1).is_none());
    }

    #[test]
    fn the_engine_snapshot_wins_over_a_possibly_stale_early_report() {
        let early = report(1, TransferState::Running, 10);
        let snapshot = report(1, TransferState::Done, 100);
        let picked = reconcile(Some(snapshot.clone()), Some(early.clone()));
        assert_eq!(picked.map(|r| r.state), Some(TransferState::Done));
        // 引擎里没有这个任务了：退回暂存的。
        let picked = reconcile(None, Some(early));
        assert_eq!(picked.map(|r| r.done_bytes), Some(10));
        assert!(reconcile(None, None).is_none());
    }
}
