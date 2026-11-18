//! 行号 / 时间戳栏用的逐行记录（V2-U49）。
//!
//! Alacritty 的网格里没有「这一行是会话里第几行」「这一行是几点收到的」：回滚满了以后最老的行
//! 悄悄被覆盖，调用方既看不到丢了几行，也没处挂时间。这里在网格旁边记两样东西，都按
//! **绝对行号**（会话里从 0 起的物理行序号，网格里最老的一行是 [`RowLog::dropped`]）索引：
//!
//! - `dropped`：已经从回滚顶端挤出去的行数。绝对行号 = `dropped` + 在回滚 + 屏幕里的位置，
//!   所以回滚满了以后行号照常往上走、不会回头；
//! - `times`：每一行第一次收到输出的时间（Unix 秒），只保留还在网格里的那些行，长度不超过
//!   「回滚上限 + 屏幕行数 + 一点余量」。
//!
//! 数据只由 [`crate::TerminalModel`] 在喂完一小段输出后更新，网格本身一个字节都不改。

use std::collections::VecDeque;

/// 给回滚上限多留的余量（行）。
///
/// Alacritty 回滚满了就悄悄覆盖最老的行，丢了几行没有任何信号。所以让它自己的上限比用户设置的
/// 大这么多，喂完每一小段输出后由模型自己把回滚裁回用户的上限、并把裁掉的行数记进 `dropped`——
/// 只要一小段输出滚出的行数不超过余量（见 [`FEED_SLICE`]），丢弃数就是精确的。
pub(crate) const HISTORY_SLACK: usize = 1_200;

/// 一段输出最多喂这么多字节就停下来记一次账。
///
/// 普通输出一个字节最多滚一行，所以这个值要小于 [`HISTORY_SLACK`]；也顺带限制了余量那部分
/// 回滚短暂占用的内存（全是换行的极端输出，也不过一千多个空行）。
pub(crate) const FEED_SLICE: usize = 1_024;

/// 见模块说明。
#[derive(Debug, Default)]
pub(crate) struct RowLog {
    /// 网格里最老的一行的绝对行号（0 起）。
    pub(crate) dropped: u64,
    /// `times[0]` 对应的绝对行号。
    times_start: u64,
    times: VecDeque<Option<i64>>,
    /// 绝对行号小于它的回滚行都已经「定稿」（打过时间）。
    pub(crate) stamped_upto: u64,
    /// 备用屏幕期间改过列数：主屏幕的内容被 Alacritty 重新折行了，我们看不到，旧记录对不上，
    /// 回到主屏幕时作废（行显示空，不错位）。
    pub(crate) stale: bool,
}

impl RowLog {
    /// 丢掉不再在网格里的记录，并保证 `[dropped, end)` 都有位置（新行默认没有时间）。
    pub(crate) fn fit(&mut self, end: u64) {
        while self.times_start < self.dropped && self.times.pop_front().is_some() {
            self.times_start += 1;
        }
        if self.times.is_empty() {
            self.times_start = self.dropped;
        }
        let want = end.saturating_sub(self.times_start) as usize;
        // 上限是调用方保证的（回滚上限 + 屏幕行数 + 余量）；这里只是兜底，别因为异常的 `end` 吃光内存。
        let want = want.min(MAX_TRACKED_ROWS);
        while self.times.len() < want {
            self.times.push_back(None);
        }
        while self.times.len() > want {
            self.times.pop_back();
        }
    }

    pub(crate) fn time_of(&self, abs: u64) -> Option<i64> {
        let index = abs.checked_sub(self.times_start)? as usize;
        self.times.get(index).copied().flatten()
    }

    /// 这一行还没有时间就记上 `now`；`now` 读不到（系统时钟异常）时什么也不记。
    pub(crate) fn stamp(&mut self, abs: u64, now: Option<i64>) {
        let Some(now) = now else { return };
        let Some(index) = abs.checked_sub(self.times_start) else {
            return;
        };
        if let Some(slot) = self.times.get_mut(index as usize)
            && slot.is_none()
        {
            *slot = Some(now);
        }
    }

    /// 无条件改写一行的时间（折行重排后按逻辑行重新分配用）。
    pub(crate) fn set(&mut self, abs: u64, time: Option<i64>) {
        let Some(index) = abs.checked_sub(self.times_start) else {
            return;
        };
        if let Some(slot) = self.times.get_mut(index as usize) {
            *slot = time;
        }
    }

    /// 全部清空，从绝对行号 `dropped` 重新开始。
    pub(crate) fn clear_times(&mut self) {
        self.times.clear();
        self.times_start = self.dropped;
    }

    /// 只留绝对行号小于 `end` 的记录（清屏后屏幕下半部分都是空行，旧时间不能留着）。
    pub(crate) fn truncate_to(&mut self, end: u64) {
        let keep = end.saturating_sub(self.times_start) as usize;
        self.times.truncate(keep);
    }

    /// 现在记着多少行的时间（测试 / 诊断用）。
    pub(crate) fn tracked(&self) -> usize {
        self.times.len()
    }
}

/// 记录条数的绝对上限：回滚上限最大 10 万行，再留足屏幕和余量。
pub(crate) const MAX_TRACKED_ROWS: usize = 100_000 + HISTORY_SLACK + 4_096;
