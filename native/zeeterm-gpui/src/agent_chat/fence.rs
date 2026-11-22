//! 后台启动 / 发送结果的"代次门"。
//!
//! 起会话、发消息都在后台任务里做，结果经通道在下一个节拍才回到界面。这期间用户可能已经
//! 点了「停止」「新对话」甚至又发了下一条：迟到的结果如果只看"现在是不是在跑"，就会把
//! 新回合当成旧回合失败了（`Sent(Err)`），或者让旧的启动结果吃掉新启动的"丢弃"标记。
//! 所以每个回合有一个递增的编号，后台任务带着它回来，界面只认当前编号。

/// 启动结果该怎么处理。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StartVerdict {
    /// 不是当前这次启动的结果（启动被「新对话」取消、或者之后又开了新回合）：丢掉，
    /// 不碰任何界面状态；带回来的会话由调用方关掉。
    Stale,
    /// 当前这次启动的结果，但用户在启动期间点了「停止」：会话起来就关掉。
    Stopped,
    /// 当前这次启动的结果，照常使用。
    Current,
}

/// 回合编号和"有一次启动在后台进行"的标记。
#[derive(Debug, Default)]
pub(super) struct TurnFence {
    turn: u64,
    starting: bool,
}

impl TurnFence {
    /// 开始一个新的真实回合，返回它的编号（带给这个回合的后台任务）。
    pub(super) fn begin_turn(&mut self) -> u64 {
        self.turn = self.turn.wrapping_add(1);
        self.turn
    }

    /// 为当前回合开始一次后台启动。之前还没回来的启动（用户停止后又发了新消息）从此作废。
    pub(super) fn begin_start(&mut self) -> u64 {
        self.starting = true;
        self.turn
    }

    /// 取消在途的启动（「新对话」）：它的结果之后到了一律算过期。
    pub(super) fn cancel_start(&mut self) {
        self.starting = false;
    }

    pub(super) fn starting(&self) -> bool {
        self.starting
    }

    /// 一次启动的结果回来了。只有"正在启动、并且就是当前回合发起的"那一次才算数。
    pub(super) fn finish_start(&mut self, turn: u64, stop_requested: bool) -> StartVerdict {
        if !self.starting || turn != self.turn {
            return StartVerdict::Stale;
        }
        self.starting = false;
        if stop_requested {
            StartVerdict::Stopped
        } else {
            StartVerdict::Current
        }
    }

    /// 已有会话上的发送结果该不该处理：必须是当前回合发的，并且这个回合还在跑。
    pub(super) fn send_is_current(&self, turn: u64, running: bool) -> bool {
        running && turn == self.turn
    }
}

#[cfg(test)]
mod tests {
    use super::{StartVerdict, TurnFence};

    #[test]
    fn a_late_send_error_from_the_previous_turn_does_not_hit_the_new_turn() {
        let mut fence = TurnFence::default();
        let old = fence.begin_turn();
        // 旧回合被停止，用户马上发了下一条。
        let new = fence.begin_turn();
        assert!(
            !fence.send_is_current(old, true),
            "新回合在跑，旧结果也不算数"
        );
        assert!(fence.send_is_current(new, true));
    }

    #[test]
    fn a_send_result_after_the_turn_ended_is_ignored() {
        let mut fence = TurnFence::default();
        let turn = fence.begin_turn();
        assert!(!fence.send_is_current(turn, false));
    }

    #[test]
    fn a_normal_start_is_current_once() {
        let mut fence = TurnFence::default();
        let turn = fence.begin_turn();
        assert_eq!(fence.begin_start(), turn);
        assert!(fence.starting());
        assert_eq!(fence.finish_start(turn, false), StartVerdict::Current);
        assert!(!fence.starting());
        assert_eq!(
            fence.finish_start(turn, false),
            StartVerdict::Stale,
            "同一个结果不会被处理两次"
        );
    }

    #[test]
    fn a_stop_during_start_discards_only_that_start() {
        let mut fence = TurnFence::default();
        let turn = fence.begin_turn();
        fence.begin_start();
        assert_eq!(fence.finish_start(turn, true), StartVerdict::Stopped);
    }

    #[test]
    fn a_cancelled_start_is_stale_even_when_a_new_conversation_starts_at_once() {
        let mut fence = TurnFence::default();
        let first = fence.begin_turn();
        fence.begin_start();
        // 启动中点「新对话」，立刻在新对话里发消息、又起一次启动。
        fence.cancel_start();
        let second = fence.begin_turn();
        fence.begin_start();
        assert!(fence.starting(), "新的启动不能被旧结果抹掉");

        // 旧启动的结果先到：过期，新启动照常等着。
        assert_eq!(fence.finish_start(first, false), StartVerdict::Stale);
        assert!(fence.starting());
        assert_eq!(fence.finish_start(second, false), StartVerdict::Current);
    }

    #[test]
    fn the_new_start_result_arriving_first_is_not_eaten_by_the_old_one() {
        // 旧实现用一个布尔"丢弃标记"：新启动的结果先到时会被误丢，旧的反而被留下。
        let mut fence = TurnFence::default();
        let first = fence.begin_turn();
        fence.begin_start();
        fence.cancel_start();
        let second = fence.begin_turn();
        fence.begin_start();

        assert_eq!(fence.finish_start(second, false), StartVerdict::Current);
        assert_eq!(fence.finish_start(first, false), StartVerdict::Stale);
    }

    #[test]
    fn a_start_failure_after_cancel_is_stale() {
        let mut fence = TurnFence::default();
        let turn = fence.begin_turn();
        fence.begin_start();
        fence.cancel_start();
        assert_eq!(fence.finish_start(turn, false), StartVerdict::Stale);
        assert!(!fence.starting());
    }

    #[test]
    fn a_stop_then_a_new_message_replaces_the_pending_start() {
        let mut fence = TurnFence::default();
        let first = fence.begin_turn();
        fence.begin_start();
        // 用户在启动期间点了停止（启动仍在后台），随后又发了新消息 = 新回合 + 新启动。
        let second = fence.begin_turn();
        fence.begin_start();
        assert_eq!(
            fence.finish_start(first, true),
            StartVerdict::Stale,
            "旧启动过期，它带的停止标记不能套到新启动上"
        );
        assert!(fence.starting());
        assert_eq!(fence.finish_start(second, false), StartVerdict::Current);
    }
}
