//! 安装前的进程级安全检查。
//!
//! 更新器启动安装器会退出当前程序；SFTP 传输、真实 AI 回合、同步和备份恢复都挂在
//! 当前进程里，直接退出会丢工作。所以每一个"来源"（一个 SFTP 窗口、一个 AI 回合、
//! 一次同步、一次恢复）各自登记一条 [`Registration`]：来源自己维护自己的数量，
//! 总数 = 所有登记之和；来源结束（`Drop`）时登记自动撤销。更新窗口在生成安装计划 /
//! 启动安装器之前问一次 [`blocking_reason`]。
//!
//! 以前是一个全局计数、"最后报告的说了算"：两个 SFTP 窗口里 A 有传输、B 报 0，
//! 总数就变成 0，安装检查放行，A 的传输被中断。按来源登记后互不覆盖。
//!
//! 编辑器里没保存的修改也算一种（[`WorkKind::UnsavedEdits`]）：安装器会退出程序，草稿虽然会落盘，
//! 但用户还没决定保存还是丢掉，不该替他退出（同 0.2.14 更新前检查里的 `unsavedEdits`）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

/// 登记的是哪一类工作；同一类的数量相加后给界面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkKind {
    /// SFTP 未结束的传输。
    Transfers,
    /// 真实 AI 回合。
    AiTurn,
    /// 同步进行中（手动或自动）。
    Sync,
    /// 备份恢复 / 离线包导入进行中。
    Restore,
    /// 编辑器标签里没保存的修改（一个文件最多算一件）。
    UnsavedEdits,
}

/// 当前还没结束的工作。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PendingWork {
    pub transfers: usize,
    pub ai_turns: usize,
    pub syncs: usize,
    pub restores: usize,
    /// 有未保存修改的编辑器标签数。
    pub unsaved_edits: usize,
}

impl PendingWork {
    fn is_idle(&self) -> bool {
        *self == Self::default()
    }
}

type SourceMap = HashMap<u64, (WorkKind, usize)>;

/// 来源编号 → （类别，这个来源现在有几件未结束的事）。
static SOURCES: Mutex<Option<SourceMap>> = Mutex::new(None);
static NEXT_SOURCE: AtomicU64 = AtomicU64::new(1);

/// 锁中毒时拿回内部值继续用：计数丢了会让安装检查失效，比多拦一次更糟。
fn sources() -> MutexGuard<'static, Option<SourceMap>> {
    SOURCES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 一个来源的登记。持有期间它报告的数量算进总数；`Drop` 时撤销，所以窗口 / 任务
/// 不管怎么结束（正常、出错、被丢掉）都不会一直拦着安装。
#[derive(Debug)]
pub struct Registration {
    id: u64,
}

impl Registration {
    /// 登记一个新来源，初始数量 0（0 不算进总数）。
    pub fn new(kind: WorkKind) -> Self {
        let id = NEXT_SOURCE.fetch_add(1, Ordering::SeqCst);
        sources()
            .get_or_insert_with(HashMap::new)
            .insert(id, (kind, 0));
        Self { id }
    }

    /// 登记并立刻算 1 件（同步 / 恢复 / 一个 AI 回合这类"有或没有"的工作）。
    pub fn started(kind: WorkKind) -> Self {
        let registration = Self::new(kind);
        registration.set(1);
        registration
    }

    /// 这个来源现在有几件未结束的事；只覆盖自己的数量，不影响别的来源。
    pub fn set(&self, count: usize) {
        if let Some(entry) = sources().get_or_insert_with(HashMap::new).get_mut(&self.id) {
            entry.1 = count;
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(map) = sources().as_mut() {
            map.remove(&self.id);
        }
    }
}

/// 读一次当前状态，给界面和测试共用。
pub fn snapshot() -> PendingWork {
    let mut work = PendingWork::default();
    if let Some(map) = sources().as_ref() {
        for (kind, count) in map.values() {
            let slot = match kind {
                WorkKind::Transfers => &mut work.transfers,
                WorkKind::AiTurn => &mut work.ai_turns,
                WorkKind::Sync => &mut work.syncs,
                WorkKind::Restore => &mut work.restores,
                WorkKind::UnsavedEdits => &mut work.unsaved_edits,
            };
            *slot = slot.saturating_add(*count);
        }
    }
    work
}

/// 拦下安装时那句话的文案。
#[derive(Clone, Copy, Debug)]
enum InstallMsg {
    Prefix,
    Suffix,
    TransfersSuffix,
    AiTurnSuffix,
    Sync,
    Restore,
    UnsavedSuffix,
    ListSeparator,
}

impl crate::i18n::Msg for InstallMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::Prefix => "还有 ",
            Self::Suffix => "在进行，先等它们结束再安装",
            Self::TransfersSuffix => " 个传输",
            Self::AiTurnSuffix => " 个 AI 回合",
            Self::Sync => "同步",
            Self::Restore => "备份恢复",
            Self::UnsavedSuffix => " 个文件没保存的修改",
            Self::ListSeparator => "、",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::Prefix => "Still busy with ",
            Self::Suffix => "; wait for it to finish before installing",
            Self::TransfersSuffix => " transfers",
            Self::AiTurnSuffix => " AI turns",
            Self::Sync => "a sync",
            Self::Restore => "a backup restore",
            Self::UnsavedSuffix => " files with unsaved edits",
            Self::ListSeparator => ", ",
        }
    }
}

/// 现在为什么不能安装；没有未结束的工作时返回 `None`。
pub fn blocking_reason() -> Option<String> {
    let work = snapshot();
    if work.is_idle() {
        return None;
    }
    let mut parts = Vec::with_capacity(4);
    if work.transfers > 0 {
        parts.push(crate::i18n::count(
            work.transfers as u64,
            crate::i18n::t(InstallMsg::TransfersSuffix),
            "transfer",
            "transfers",
        ));
    }
    if work.ai_turns > 0 {
        parts.push(crate::i18n::count(
            work.ai_turns as u64,
            crate::i18n::t(InstallMsg::AiTurnSuffix),
            "AI turn",
            "AI turns",
        ));
    }
    if work.syncs > 0 {
        parts.push(crate::i18n::t(InstallMsg::Sync).to_owned());
    }
    if work.restores > 0 {
        parts.push(crate::i18n::t(InstallMsg::Restore).to_owned());
    }
    if work.unsaved_edits > 0 {
        parts.push(crate::i18n::count(
            work.unsaved_edits as u64,
            crate::i18n::t(InstallMsg::UnsavedSuffix),
            "file with unsaved edits",
            "files with unsaved edits",
        ));
    }
    Some(format!(
        "{}{}{}",
        crate::i18n::t(InstallMsg::Prefix),
        parts.join(crate::i18n::t(InstallMsg::ListSeparator)),
        crate::i18n::t(InstallMsg::Suffix)
    ))
}

#[cfg(test)]
mod tests {
    use super::{PendingWork, Registration, WorkKind, blocking_reason, snapshot};
    use std::sync::{Mutex, MutexGuard};

    /// 传输和 AI 回合的计数只有本模块的测试动，但它们彼此并行，拿锁串起来。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock() -> MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // 登记表是进程级的，同步 / 恢复模块的测试也会往里登记，所以这里不假设"一开始是
    // 0"：每条断言都比较"本测试的登记前后差了多少"。传输和 AI 回合只有本模块的测试
    // 动，可以精确比较；同步和恢复只断言"至少"和原因文本。

    fn transfers_delta(before: PendingWork) -> i64 {
        snapshot().transfers as i64 - before.transfers as i64
    }

    fn ai_delta(before: PendingWork) -> i64 {
        snapshot().ai_turns as i64 - before.ai_turns as i64
    }

    #[test]
    fn registrations_do_not_count_until_they_report() {
        let _lock = lock();
        let before = snapshot();
        let source = Registration::new(WorkKind::Transfers);
        assert_eq!(transfers_delta(before), 0);
        source.set(2);
        assert_eq!(transfers_delta(before), 2);
    }

    #[test]
    fn two_windows_add_up_instead_of_overwriting() {
        let _lock = lock();
        // 审查 C-H2：窗口 A 有 3 个传输，窗口 B 报 0 或被关掉，总数不能变成 0。
        let before = snapshot();
        let window_a = Registration::new(WorkKind::Transfers);
        let window_b = Registration::new(WorkKind::Transfers);
        window_a.set(3);
        window_b.set(1);
        assert_eq!(transfers_delta(before), 4);

        window_b.set(0);
        assert_eq!(transfers_delta(before), 3);
        drop(window_b);
        assert_eq!(transfers_delta(before), 3);
        assert!(
            blocking_reason().is_some_and(|reason| reason.contains("个传输")),
            "窗口 A 的传输还在，安装必须被拦"
        );
        drop(window_a);
        assert_eq!(transfers_delta(before), 0);
    }

    #[test]
    fn many_windows_in_parallel_threads_sum_correctly() {
        let _lock = lock();
        let before = snapshot();
        let handles: Vec<_> = (0..8usize)
            .map(|index| {
                std::thread::spawn(move || {
                    let source = Registration::new(WorkKind::Transfers);
                    for round in 0..200usize {
                        source.set((index + round) % 3);
                    }
                    source.set(2);
                    source
                })
            })
            .collect();
        let sources: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("线程不应 panic"))
            .collect();
        assert_eq!(transfers_delta(before), 16);
        drop(sources);
        assert_eq!(transfers_delta(before), 0);
    }

    #[test]
    fn dropping_a_source_withdraws_its_work() {
        let _lock = lock();
        let before = snapshot();
        {
            let _turn = Registration::started(WorkKind::AiTurn);
            assert_eq!(ai_delta(before), 1);
        }
        assert_eq!(ai_delta(before), 0);
    }

    #[test]
    fn sync_and_restore_are_reported_in_the_blocking_reason() {
        let sync = Registration::started(WorkKind::Sync);
        let restore = Registration::started(WorkKind::Restore);
        let after = snapshot();
        assert!(after.syncs >= 1 && after.restores >= 1, "{after:?}");
        let reason = blocking_reason().expect("同步和恢复进行中应当拦下安装");
        assert!(reason.contains("同步"), "{reason}");
        assert!(reason.contains("备份恢复"), "{reason}");
        drop(sync);
        drop(restore);
    }

    /// 编辑器里没保存的修改也拦安装（和 0.2.14 更新前检查的 `unsavedEdits` 一致）。
    #[test]
    fn unsaved_editor_changes_block_an_install() {
        let edits = Registration::new(WorkKind::UnsavedEdits);
        let before = snapshot().unsaved_edits;
        edits.set(0);
        assert_eq!(snapshot().unsaved_edits, before, "没改动时不算");
        edits.set(1);
        assert_eq!(snapshot().unsaved_edits, before + 1);
        let reason = blocking_reason().expect("有未保存的编辑要拦下安装");
        assert!(reason.contains("没保存的修改"), "{reason}");
        drop(edits);
        assert_eq!(snapshot().unsaved_edits, before);
    }

    #[test]
    fn transfers_and_ai_turns_are_both_reported() {
        let _lock = lock();
        let transfers = Registration::new(WorkKind::Transfers);
        transfers.set(1);
        let _turn = Registration::started(WorkKind::AiTurn);
        let reason = blocking_reason().expect("有传输和 AI 回合时应当拦下安装");
        assert!(reason.contains("个传输"), "{reason}");
        assert!(reason.contains("个 AI 回合"), "{reason}");
        assert!(reason.starts_with("还有 "), "{reason}");
        assert!(reason.ends_with("先等它们结束再安装"), "{reason}");
    }
}
