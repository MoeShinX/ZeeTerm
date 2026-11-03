//! 每帧的输出预算：把一帧能处理的终端数据总量在所有窗格之间分配。
//!
//! 以前预算是"每个窗格 1 MiB"：20 个窗格同时被刷，界面线程一帧要解析 20 MiB，按键回显跟着卡；
//! 遍历顺序固定，前面的窗格吃满预算，后面的没有公平性可言。
//!
//! 现在的规则：
//! - 一帧总量是 [`FRAME_BYTE_BUDGET`]，所有窗格共用；
//! - 前台窗格（当前标签里看得见的）先拿，但最多拿走总量的四分之三，剩下的四分之一留给后台窗格，
//!   所以前台被刷的时候后台也会推进，不会被饿死；前台没用完的额度顺延给后台；
//! - 后台窗格每个一帧最多拿 [`BACKGROUND_QUANTUM`]，让一帧里能照顾到好几个；
//! - 前台、后台各自从"上一帧额度用完处的下一个"开始轮转，连续几帧下来每个窗格都轮得到。
//!
//! 没分到额度的窗格仍会被调用（额度为 0）：连接结果、退出提示这些不吃数据预算的事不能被推迟。
//! 单个事件批次不会被拆开，所以一帧的实际用量最多超出总量一个批次。

/// 一帧处理的终端数据字节总量（所有窗格合计）。
pub const FRAME_BYTE_BUDGET: usize = 1024 * 1024;

/// 后台窗格一帧最多拿的字节数。
pub const BACKGROUND_QUANTUM: usize = 64 * 1024;

/// 一个会话这一帧的轮询结果。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PollOutcome {
    /// 有没有新内容要重绘。
    pub changed: bool,
    /// 实际处理的数据字节数。
    pub used: usize,
}

/// 帧预算的分配器；需要在帧与帧之间保留轮转位置，所以由调用方长期持有。
#[derive(Debug)]
pub struct FrameScheduler {
    budget: usize,
    foreground_cursor: usize,
    background_cursor: usize,
}

impl Default for FrameScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameScheduler {
    pub fn new() -> Self {
        Self::with_budget(FRAME_BYTE_BUDGET)
    }

    pub fn with_budget(budget: usize) -> Self {
        Self {
            budget,
            foreground_cursor: 0,
            background_cursor: 0,
        }
    }

    /// 跑一帧：`count` 个窗格，`foreground(i)` 说第 i 个是不是前台，`poll(i, 额度)` 去轮询它。
    /// 每个窗格恰好被调用一次；返回有没有窗格报告了变化。
    pub fn run(
        &mut self,
        count: usize,
        foreground: impl Fn(usize) -> bool,
        mut poll: impl FnMut(usize, usize) -> PollOutcome,
    ) -> bool {
        let front: Vec<usize> = (0..count).filter(|&i| foreground(i)).collect();
        let back: Vec<usize> = (0..count).filter(|&i| !foreground(i)).collect();
        // 没有后台窗格就不必留保留额度，单个窗格仍能拿到整帧。
        let reserve = if back.is_empty() { 0 } else { self.budget / 4 };

        let (mut changed, front_used) = run_phase(
            &front,
            &mut self.foreground_cursor,
            self.budget - reserve,
            usize::MAX,
            &mut poll,
        );
        let (back_changed, _) = run_phase(
            &back,
            &mut self.background_cursor,
            self.budget.saturating_sub(front_used),
            BACKGROUND_QUANTUM,
            &mut poll,
        );
        changed |= back_changed;
        changed
    }
}

/// 在 `list`（窗格下标，升序）里从 `cursor` 之后轮转着分配 `limit` 字节；
/// 额度恰好在某个窗格之后用完时，下一帧从它的下一个开始。
fn run_phase(
    list: &[usize],
    cursor: &mut usize,
    limit: usize,
    quantum: usize,
    poll: &mut impl FnMut(usize, usize) -> PollOutcome,
) -> (bool, usize) {
    let mut changed = false;
    let mut spent = 0usize;
    let mut next_cursor = None;
    let first = list.partition_point(|&i| i < *cursor);
    for step in 0..list.len() {
        let index = list[(first + step) % list.len()];
        let allowance = quantum.min(limit.saturating_sub(spent));
        let outcome = poll(index, allowance);
        changed |= outcome.changed;
        spent = spent.saturating_add(outcome.used);
        if allowance > 0 && spent >= limit && next_cursor.is_none() {
            next_cursor = Some(index + 1);
        }
    }
    if let Some(next) = next_cursor {
        *cursor = next;
    }
    (changed, spent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CoreFilter, SessionDriver, SessionEvent, SessionPump};
    use std::sync::Arc;
    use std::sync::mpsc::Sender;
    use zssh_core::ssh::flow::FlowGate;
    use zssh_terminal::Size;

    const BATCH: usize = 4096;

    struct Fake {
        driver: SessionDriver<CoreFilter>,
        tx: Sender<SessionEvent>,
    }

    fn fake() -> Fake {
        let (tx, rx) = std::sync::mpsc::channel();
        let pump = SessionPump::new(
            Size { rows: 5, cols: 40 },
            100,
            Arc::new(FlowGate::new()),
            CoreFilter::new(),
            1,
        );
        Fake {
            driver: SessionDriver::new(pump, rx),
            tx,
        }
    }

    fn flood(fake: &Fake, bytes: usize) {
        for _ in 0..bytes / BATCH {
            fake.tx
                .send(SessionEvent::Data {
                    generation: 1,
                    bytes: vec![b'a'; BATCH],
                })
                .expect("发洪水数据");
        }
    }

    /// 跑一帧并返回这一帧每个假驱动实际处理的字节数。
    fn frame(
        scheduler: &mut FrameScheduler,
        fakes: &mut [Fake],
        front: &[usize],
    ) -> (Vec<usize>, bool) {
        let before: Vec<usize> = fakes.iter().map(|f| f.driver.pump().ledger().raw).collect();
        let changed = scheduler.run(
            fakes.len(),
            |i| front.contains(&i),
            |i, allowance| fakes[i].driver.poll_counted(allowance),
        );
        let used = fakes
            .iter()
            .zip(before)
            .map(|(f, b)| f.driver.pump().ledger().raw - b)
            .collect();
        (used, changed)
    }

    #[test]
    fn twenty_flooding_panes_share_one_frame_budget() {
        let mut fakes: Vec<Fake> = (0..20).map(|_| fake()).collect();
        for f in &fakes {
            flood(f, 2 * 1024 * 1024);
        }
        let mut scheduler = FrameScheduler::new();
        let (used, changed) = frame(&mut scheduler, &mut fakes, &[0]);
        let total: usize = used.iter().sum();
        assert!(changed);
        // 以前是每窗格各 1 MiB（共 20 MiB）；现在合计不超过一帧预算加一个批次。
        assert!(
            total <= FRAME_BYTE_BUDGET + BATCH,
            "一帧合计处理了 {total} 字节"
        );
        assert!(total >= FRAME_BYTE_BUDGET, "预算没有被浪费：{total}");
    }

    #[test]
    fn every_pane_progresses_over_consecutive_frames() {
        let mut fakes: Vec<Fake> = (0..20).map(|_| fake()).collect();
        for f in &fakes {
            flood(f, 2 * 1024 * 1024);
        }
        let mut scheduler = FrameScheduler::new();
        let mut progressed = vec![false; fakes.len()];
        for _ in 0..8 {
            let (used, _) = frame(&mut scheduler, &mut fakes, &[0]);
            for (flag, n) in progressed.iter_mut().zip(used) {
                *flag |= n > 0;
            }
        }
        // 窗格 0 是前台，一直在被刷；后台 19 个窗格也都要在几帧内轮到。
        assert!(progressed.iter().all(|&p| p), "{progressed:?}");
    }

    #[test]
    fn a_flooded_foreground_pane_cannot_starve_the_background() {
        let mut fakes: Vec<Fake> = (0..3).map(|_| fake()).collect();
        for f in &fakes {
            flood(f, 8 * 1024 * 1024);
        }
        let mut scheduler = FrameScheduler::new();
        for round in 0..4 {
            let (used, _) = frame(&mut scheduler, &mut fakes, &[0]);
            // 保留额度够两个后台窗格各拿满一份配额。
            assert!(
                used[1] >= BACKGROUND_QUANTUM && used[2] >= BACKGROUND_QUANTUM,
                "第 {round} 帧后台只拿到 {used:?}"
            );
            assert!(used[0] <= FRAME_BYTE_BUDGET - FRAME_BYTE_BUDGET / 4 + BATCH);
        }
    }

    #[test]
    fn a_single_pane_still_gets_the_whole_frame() {
        let mut fakes = vec![fake()];
        flood(&fakes[0], 4 * 1024 * 1024);
        let mut scheduler = FrameScheduler::new();
        let (used, _) = frame(&mut scheduler, &mut fakes, &[0]);
        assert!(used[0] >= FRAME_BYTE_BUDGET, "{used:?}");
    }

    #[test]
    fn idle_frames_report_no_change_and_call_every_pane_once() {
        let mut calls = vec![0usize; 5];
        let mut scheduler = FrameScheduler::new();
        let changed = scheduler.run(
            5,
            |i| i == 2,
            |i, _| {
                calls[i] += 1;
                PollOutcome::default()
            },
        );
        assert!(!changed);
        assert_eq!(calls, vec![1; 5]);
        assert!(!scheduler.run(0, |_| true, |_, _| PollOutcome::default()));
    }

    #[test]
    fn starved_panes_are_still_called_with_zero_allowance() {
        let mut seen = Vec::new();
        let mut scheduler = FrameScheduler::with_budget(100);
        scheduler.run(
            3,
            |_| true,
            |i, allowance| {
                seen.push((i, allowance));
                PollOutcome {
                    changed: false,
                    used: allowance,
                }
            },
        );
        assert_eq!(seen, vec![(0, 100), (1, 0), (2, 0)]);
    }
}
