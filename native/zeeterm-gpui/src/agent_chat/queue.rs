//! 「待发送」队列（V2-U1 第 2 条）：AI 回合进行中用户还想补充消息，三种驱动在回合里都会回
//! `AgentError::Busy`，所以不能直接发；原生这边先排队，回合结束后合并成一条发出。
//!
//! 这里只放纯状态（没有 GPUI、没有驱动），好用单测把每条规则钉住：
//! - 可以连续加多条、编辑、删除；正在编辑的那条保存之前不自动发（免得把写了一半的内容发出去）；
//! - 自动发出 = 按顺序合并成一条用户消息；用户点了「停止」就「扣住」，不自动发，留在队列里等用户决定；
//! - 「打断并发送」：先走停止链，再等旧回合的收尾事件（`TurnEnd`）到了才发，旧回合迟到的东西
//!   就不会落进新回合；旧回合一直没有收尾事件时，超过 [`WIND_DOWN_LIMIT`] 也放行（驱动不保证
//!   一定会发）。

use std::time::{Duration, Instant};

/// 合并多条排队消息时的分隔：空一行，模型读起来是几段话，不是一句话被硬拼在一起。
const JOIN: &str = "\n\n";

/// 打断后等旧回合收尾事件的上限。
pub const WIND_DOWN_LIMIT: Duration = Duration::from_secs(8);

/// 一条排队的消息。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Queued {
    pub id: u64,
    pub text: String,
}

#[derive(Debug, Default)]
pub struct PendingQueue {
    items: Vec<Queued>,
    next_id: u64,
    /// 用户点了停止 / 发送失败：不自动发，等用户决定（「现在发送」或删掉）。
    held: bool,
    /// 正在编辑的那条（保存或取消之前不自动发）。
    editing: Option<u64>,
    /// 「打断并发送」之后，等旧回合收尾事件的起点。
    winding_down: Option<Instant>,
}

impl PendingQueue {
    pub fn items(&self) -> &[Queued] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn is_held(&self) -> bool {
        self.held
    }

    pub fn editing(&self) -> Option<u64> {
        self.editing
    }

    pub fn is_winding_down(&self) -> bool {
        self.winding_down.is_some()
    }

    /// 加一条；只有空白的不收，返回新条的编号。
    pub fn push(&mut self, text: &str) -> Option<u64> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        self.next_id += 1;
        let id = self.next_id;
        self.items.push(Queued {
            id,
            text: text.to_owned(),
        });
        Some(id)
    }

    /// 编辑一条：改成空白等于删除。编号不存在返回 false。结束编辑状态。
    pub fn edit(&mut self, id: u64, text: &str) -> bool {
        let Some(index) = self.items.iter().position(|item| item.id == id) else {
            return false;
        };
        if self.editing == Some(id) {
            self.editing = None;
        }
        let text = text.trim();
        if text.is_empty() {
            self.items.remove(index);
        } else {
            self.items[index].text = text.to_owned();
        }
        true
    }

    /// 删除一条。
    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.items.len();
        self.items.retain(|item| item.id != id);
        if self.editing == Some(id) {
            self.editing = None;
        }
        self.items.len() != before
    }

    pub fn text_of(&self, id: u64) -> Option<&str> {
        self.items
            .iter()
            .find(|item| item.id == id)
            .map(|item| item.text.as_str())
    }

    /// 开始编辑某一条（同一时间只编辑一条）。
    pub fn begin_edit(&mut self, id: u64) -> bool {
        if self.text_of(id).is_none() {
            return false;
        }
        self.editing = Some(id);
        true
    }

    /// 不保存地结束编辑。
    pub fn cancel_edit(&mut self) {
        self.editing = None;
    }

    /// 扣住：不再自动发（点了停止、发送失败）。
    pub fn hold(&mut self) {
        self.held = true;
    }

    /// 放开扣住（用户明确要发了，或新回合开始）。
    pub fn release(&mut self) {
        self.held = false;
    }

    /// 清空（新对话）。
    pub fn clear(&mut self) {
        self.items.clear();
        self.held = false;
        self.editing = None;
        self.winding_down = None;
    }

    /// 开始等旧回合的收尾事件（「打断并发送」用）。
    pub fn start_wind_down(&mut self, now: Instant) {
        self.winding_down = Some(now);
    }

    /// 旧回合的收尾事件到了。
    pub fn finish_wind_down(&mut self) {
        self.winding_down = None;
    }

    /// 现在是不是可以把队列自动发出去：没有回合在跑、没扣住、没在编辑、没在等旧回合收尾。
    pub fn ready_to_flush(&self, turn_running: bool, now: Instant) -> bool {
        if turn_running || self.items.is_empty() || self.held || self.editing.is_some() {
            return false;
        }
        match self.winding_down {
            Some(since) => now.duration_since(since) >= WIND_DOWN_LIMIT,
            None => true,
        }
    }

    /// 取出全部排队消息，按顺序合并成一条；队列清空，放开扣住和等待状态。
    pub fn take_merged(&mut self) -> Option<String> {
        if self.items.is_empty() {
            return None;
        }
        let merged = self
            .items
            .drain(..)
            .map(|item| item.text)
            .collect::<Vec<_>>()
            .join(JOIN);
        self.held = false;
        self.editing = None;
        self.winding_down = None;
        Some(merged)
    }

    /// 发不出去（智能体不可用等）：把合并的文字放回队首并扣住，不丢。
    pub fn put_back(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.next_id += 1;
        self.items.insert(
            0,
            Queued {
                id: self.next_id,
                text: text.to_owned(),
            },
        );
        self.held = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_queue_in_order_and_blank_ones_are_ignored() {
        let mut queue = PendingQueue::default();
        assert_eq!(queue.push("   \n"), None);
        let a = queue.push("  先看日志 ").expect("收下");
        let b = queue.push("再看磁盘").expect("收下");
        assert_ne!(a, b);
        let texts: Vec<_> = queue.items().iter().map(|i| i.text.as_str()).collect();
        assert_eq!(texts, ["先看日志", "再看磁盘"]);
    }

    #[test]
    fn edit_changes_the_text_and_blank_edit_deletes() {
        let mut queue = PendingQueue::default();
        let a = queue.push("one").expect("收下");
        let b = queue.push("two").expect("收下");
        assert!(queue.begin_edit(a));
        assert!(queue.edit(a, "uno"));
        assert_eq!(queue.text_of(a), Some("uno"));
        assert_eq!(queue.editing(), None, "保存之后结束编辑");
        assert!(queue.edit(b, "   "));
        assert_eq!(queue.text_of(b), None, "改成空白 = 删除");
        assert!(!queue.edit(999, "x"), "不存在的编号");
    }

    #[test]
    fn remove_drops_one_and_keeps_the_rest_in_order() {
        let mut queue = PendingQueue::default();
        let a = queue.push("a").expect("收下");
        let _b = queue.push("b").expect("收下");
        let c = queue.push("c").expect("收下");
        assert!(queue.remove(a));
        assert!(!queue.remove(a), "第二次删不到");
        assert!(queue.remove(c));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.items()[0].text, "b");
    }

    #[test]
    fn a_finished_turn_flushes_everything_as_one_message_in_order() {
        let mut queue = PendingQueue::default();
        queue.push("第一条");
        queue.push("第二条");
        let now = Instant::now();
        assert!(!queue.ready_to_flush(true, now), "回合还在跑");
        assert!(queue.ready_to_flush(false, now));
        assert_eq!(queue.take_merged().as_deref(), Some("第一条\n\n第二条"));
        assert!(queue.is_empty());
        assert!(!queue.ready_to_flush(false, now), "空队列没有东西可发");
        assert_eq!(queue.take_merged(), None);
    }

    #[test]
    fn a_held_queue_waits_for_the_user_even_when_the_turn_is_over() {
        let mut queue = PendingQueue::default();
        queue.push("补充");
        queue.hold();
        let now = Instant::now();
        assert!(!queue.ready_to_flush(false, now), "点了停止：不自动发");
        assert_eq!(queue.len(), 1, "留在队列里");
        // 用户点「现在发送」：放开并取出
        queue.release();
        assert!(queue.ready_to_flush(false, now));
    }

    #[test]
    fn an_open_edit_blocks_the_flush_until_it_is_saved_or_cancelled() {
        let mut queue = PendingQueue::default();
        let id = queue.push("写到一半").expect("收下");
        let now = Instant::now();
        queue.begin_edit(id);
        assert!(!queue.ready_to_flush(false, now));
        queue.cancel_edit();
        assert!(queue.ready_to_flush(false, now));
    }

    #[test]
    fn interrupt_waits_for_the_old_turn_end_then_sends() {
        let mut queue = PendingQueue::default();
        queue.push("改成先查网络");
        let start = Instant::now();
        queue.start_wind_down(start);
        assert!(queue.is_winding_down());
        assert!(
            !queue.ready_to_flush(false, start + Duration::from_secs(1)),
            "旧回合还没收尾：不发，免得旧回合迟到的东西落进新回合"
        );
        queue.finish_wind_down();
        assert!(queue.ready_to_flush(false, start + Duration::from_secs(1)));
    }

    #[test]
    fn interrupt_does_not_wait_forever_for_a_driver_that_never_ends_the_turn() {
        let mut queue = PendingQueue::default();
        queue.push("继续");
        let start = Instant::now();
        queue.start_wind_down(start);
        assert!(!queue.ready_to_flush(false, start + WIND_DOWN_LIMIT - Duration::from_millis(1)));
        assert!(queue.ready_to_flush(false, start + WIND_DOWN_LIMIT));
    }

    #[test]
    fn a_message_that_could_not_be_sent_goes_back_to_the_front_and_is_held() {
        let mut queue = PendingQueue::default();
        queue.push("后来的");
        queue.put_back("先前的合并内容");
        assert_eq!(queue.items()[0].text, "先前的合并内容");
        assert!(queue.is_held(), "发不出去不能自动再试，免得死循环");
        assert!(!queue.ready_to_flush(false, Instant::now()));
    }

    #[test]
    fn clear_forgets_everything_including_hold_and_wait() {
        let mut queue = PendingQueue::default();
        let id = queue.push("x").expect("收下");
        queue.begin_edit(id);
        queue.hold();
        queue.start_wind_down(Instant::now());
        queue.clear();
        assert!(queue.is_empty());
        assert!(!queue.is_held() && !queue.is_winding_down() && queue.editing().is_none());
    }

    #[test]
    fn take_merged_releases_hold_and_wait() {
        let mut queue = PendingQueue::default();
        queue.push("x");
        queue.hold();
        queue.start_wind_down(Instant::now());
        assert!(queue.take_merged().is_some());
        assert!(!queue.is_held() && !queue.is_winding_down());
    }
}
