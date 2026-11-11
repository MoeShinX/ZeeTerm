//! 原生会话泵：核心事件流 -> 显示过滤 -> 终端模型 -> 流控 ACK。
//!
//! 这一段对应 Tauri 入口里的 `session_loop` + `emit_display`：
//! - 流控门的 `on_sent` 由发送方（核心转发任务；回环后端不需要流控）记账，
//!   泵只负责 ack，绝不重复记账；
//! - 显示过滤吞掉的字节由过滤侧立刻 ack；
//! - 交给模拟器的字节在 `feed` 真正消费后 ack；
//! - 旧代次的迟到回调直接丢弃，不能计到新代次头上；
//! - 隐藏标签照样消费，不依赖绘制帧。
//!
//! 这里不负责 SSH 连接、认证、连接池；那些仍由 `zssh-core` 提供。

use std::sync::Arc;
use std::time::Instant;

use zssh_core::ai::display::{DisplayFilter as CoreDisplayFilter, DisplayOutput};
use zssh_core::ssh::flow::FlowGate;
use zssh_terminal::{Size, Snapshot, TerminalModel};

pub mod driver;
pub mod keys;
pub mod schedule;

pub use driver::{
    ChannelBackend, LoopbackBackend, POLL_BYTE_BUDGET, RejectReason, SessionBackend,
    SessionCommand, SessionDriver, SessionEvent, SessionExit, WriteOutcome, map_core_event,
};
pub use keys::{
    KeyInput, MouseAction, MouseButton, MouseInput, altgr_text, encode_key, encode_mouse,
    encode_paste,
};
pub use schedule::{FRAME_BYTE_BUDGET, FrameScheduler, PollOutcome};

/// 过滤结果：`data` 给终端，`suppressed` 是被吞掉、需要由过滤侧 ack 的字节数。
pub struct Filtered {
    pub data: Vec<u8>,
    pub suppressed: usize,
}

impl Filtered {
    pub fn empty() -> Self {
        Self {
            data: Vec::new(),
            suppressed: 0,
        }
    }
}

pub trait ByteFilter {
    fn push(&mut self, bytes: &[u8]) -> Filtered;
    /// 定时器调用：压住的尾巴到时间后放行。
    fn tick(&mut self) -> Filtered;

    /// 会话结束：压着的尾巴不会再等到后续数据，全部放行。默认没有压着的东西。
    fn flush(&mut self) -> Filtered {
        Filtered::empty()
    }

    /// AI 命令马上要写进终端（V2-G10b）：让过滤器记住它，包装脚本的回显和标记行就不会显示给用户。
    /// 默认不处理（测试用的过滤器、直通过滤器都不认 AI 命令）。
    fn arm(&mut self, _marker: &str, _payload: &str, _command: &str) -> Filtered {
        Filtered::empty()
    }

    /// AI 命令超时返回、命令还在终端里跑：过滤器退出完整过滤（不压用户之后的输入）。
    fn release(&mut self, _marker: &str) -> Filtered {
        Filtered::empty()
    }
}

/// 不过滤：原样交给终端。
pub struct PassThrough;

impl ByteFilter for PassThrough {
    fn push(&mut self, bytes: &[u8]) -> Filtered {
        Filtered {
            data: bytes.to_vec(),
            suppressed: 0,
        }
    }

    fn tick(&mut self) -> Filtered {
        Filtered {
            data: Vec::new(),
            suppressed: 0,
        }
    }
}

/// 生产用显示过滤：直接包 `zssh_core::ai::display::DisplayFilter`。
pub struct CoreFilter(CoreDisplayFilter);

impl CoreFilter {
    pub fn new() -> Self {
        Self(CoreDisplayFilter::new())
    }
}

impl Default for CoreFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl ByteFilter for CoreFilter {
    fn push(&mut self, bytes: &[u8]) -> Filtered {
        let now = Instant::now();
        let mut out = self.0.push_at(bytes, now);
        merge(&mut out, self.0.poll_at(now));
        Filtered {
            data: out.data,
            suppressed: out.suppressed,
        }
    }

    fn tick(&mut self) -> Filtered {
        let out = self.0.poll_at(Instant::now());
        Filtered {
            data: out.data,
            suppressed: out.suppressed,
        }
    }

    fn flush(&mut self) -> Filtered {
        let out = self.0.flush();
        Filtered {
            data: out.data,
            suppressed: out.suppressed,
        }
    }

    fn arm(&mut self, marker: &str, payload: &str, command: &str) -> Filtered {
        let out = self.0.arm(marker, payload, command);
        Filtered {
            data: out.data,
            suppressed: out.suppressed,
        }
    }

    fn release(&mut self, marker: &str) -> Filtered {
        let out = self.0.release(marker);
        Filtered {
            data: out.data,
            suppressed: out.suppressed,
        }
    }
}

fn merge(into: &mut DisplayOutput, later: DisplayOutput) {
    into.data.extend_from_slice(&later.data);
    into.suppressed += later.suppressed;
}

/// 一段会话的字节账本；用于对账和压力测试。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ledger {
    /// 收到的原始字节。
    pub raw: usize,
    /// 显示过滤吞掉的字节。
    pub filtered: usize,
    /// 交给终端模型的字节。
    pub consumed: usize,
    /// 已经 ack 的字节（过滤 + 消费）。
    pub acked: usize,
    /// 因为代次不符丢弃的批次。
    pub stale_batches: usize,
}

impl Ledger {
    /// 原始 = 过滤 + 消费。
    pub fn is_balanced(&self) -> bool {
        self.raw == self.filtered + self.consumed && self.acked == self.raw
    }
}

/// 终端对远端查询的协议回复：要原样写回产生查询的那个会话。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalReplies {
    /// 产生这些回复的连接代次。
    pub generation: u64,
    pub bytes: Vec<u8>,
}

pub struct PumpOutcome {
    /// 代次是否匹配；`false` 表示这批数据被丢弃。
    pub accepted: bool,
    pub consumed: usize,
    pub filtered: usize,
}

pub struct SessionPump<F: ByteFilter> {
    model: TerminalModel,
    gate: Arc<FlowGate>,
    filter: F,
    generation: u64,
    ledger: Ledger,
    /// 会话日志（纯文本，去掉控制序列）：只有这一套，写文件在日志自己的线程里，不拖终端。
    /// 设置里开了「记录会话日志」时自动挂上，手动「开始记录」也是同一个对象、同一个目录。
    plain_log: Option<zssh_core::ssh::session_log::SessionLog>,
    /// 累计收到的 AI 命令注入次数（`ai_arm`）：AI 往终端里写命令不经窗格的写路径，
    /// 输入跟踪靠这个计数发现"有人替用户在输入行里写了东西"。
    ai_arms: u64,
}

impl<F: ByteFilter> SessionPump<F> {
    pub fn new(
        size: Size,
        history: usize,
        gate: Arc<FlowGate>,
        filter: F,
        generation: u64,
    ) -> Self {
        Self {
            model: TerminalModel::new(size, history),
            gate,
            filter,
            generation,
            ledger: Ledger::default(),
            plain_log: None,
            ai_arms: 0,
        }
    }

    /// 取走自上次以来终端识别出的 shell 标记（OSC 133 / 633 / 7）。
    /// 只有远端输出会产生标记，`feed_local` 的本地文字不会。
    pub fn take_shell_events(&mut self) -> Vec<zssh_terminal::ShellEvent> {
        self.model.take_shell_events()
    }

    /// 光标和屏幕模式的轻量读数。
    pub fn screen_probe(&self) -> zssh_terminal::ScreenProbe {
        self.model.screen_probe()
    }

    /// 屏幕第 `row` 行的单元格（保留尾部空格）。
    pub fn line_cells(&self, row: usize) -> Option<Vec<zssh_terminal::LineCell>> {
        self.model.line_cells(row)
    }

    /// 累计的 AI 命令注入次数。
    pub fn ai_arm_count(&self) -> u64 {
        self.ai_arms
    }

    /// 挂上会话日志；只记录显示过滤之后、交给终端的字节。
    pub fn set_plain_log(&mut self, log: zssh_core::ssh::session_log::SessionLog) {
        self.plain_log = Some(log);
    }

    pub fn has_plain_log(&self) -> bool {
        self.plain_log.is_some()
    }

    /// 取回纯文本日志（丢掉它就会把剩下的写完、关文件）。
    pub fn take_plain_log(&mut self) -> Option<zssh_core::ssh::session_log::SessionLog> {
        self.plain_log.take()
    }

    fn log_write(&mut self, data: &[u8]) {
        if let Some(log) = &self.plain_log {
            log.write(data);
        }
    }

    /// 切换到新代次；旧代次的迟到数据随后会被丢弃。
    pub fn set_generation(&mut self, generation: u64) {
        // 还没取走的回复是上一个代次的查询产生的，不能带着新代次发出去。
        let _ = self.model.take_replies();
        self.generation = generation;
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn on_data(&mut self, generation: u64, bytes: &[u8]) -> PumpOutcome {
        if generation != self.generation {
            self.ledger.stale_batches += 1;
            return PumpOutcome {
                accepted: false,
                consumed: 0,
                filtered: 0,
            };
        }

        self.ledger.raw += bytes.len();
        // 不在这里 on_sent：真实会话里核心转发任务在投递事件前已经记过账
        //（发送方记账、消费方 ack）；这里再记一次会让未确认量每个字节多一份，
        // 累计约 1 MiB 后读通道永久暂停。

        let filtered = self.filter.push(bytes);
        if filtered.suppressed > 0 {
            // 过滤侧确认：这些字节不会到达终端，必须由这里替界面 ack。
            self.gate.on_ack(filtered.suppressed);
            self.ledger.filtered += filtered.suppressed;
            self.ledger.acked += filtered.suppressed;
        }
        self.log_write(&filtered.data);

        let consumed = self.model.feed(&filtered.data);
        if consumed > 0 {
            // 同步消费完成，立刻 ack；隐藏标签也走这条路径，不看绘制帧。
            self.gate.on_ack(consumed);
            self.ledger.consumed += consumed;
            self.ledger.acked += consumed;
        }

        PumpOutcome {
            accepted: true,
            consumed,
            filtered: filtered.suppressed,
        }
    }

    /// AI 命令马上要写进终端：过滤器先记住它（V2-G10b）。过滤器放出来的上一条命令压着的尾巴
    /// 照常进终端，被吞的字节照常 ack。
    pub fn ai_arm(&mut self, generation: u64, marker: &str, payload: &str, command: &str) {
        if generation != self.generation {
            self.ledger.stale_batches += 1;
            return;
        }
        self.ai_arms += 1;
        let filtered = self.filter.arm(marker, payload, command);
        self.apply_filtered(filtered);
    }

    /// AI 命令超时返回：过滤器退出完整过滤。
    pub fn ai_release(&mut self, generation: u64, marker: &str) {
        if generation != self.generation {
            self.ledger.stale_batches += 1;
            return;
        }
        let filtered = self.filter.release(marker);
        self.apply_filtered(filtered);
    }

    fn apply_filtered(&mut self, filtered: Filtered) -> PumpOutcome {
        if filtered.suppressed > 0 {
            self.gate.on_ack(filtered.suppressed);
            self.ledger.filtered += filtered.suppressed;
            self.ledger.acked += filtered.suppressed;
        }
        self.log_write(&filtered.data);
        let consumed = self.model.feed(&filtered.data);
        if consumed > 0 {
            self.gate.on_ack(consumed);
            self.ledger.consumed += consumed;
            self.ledger.acked += consumed;
        }
        PumpOutcome {
            accepted: true,
            consumed,
            filtered: filtered.suppressed,
        }
    }

    /// 过滤器的定时器：压住的尾巴到时间后放行，并按消费后的字节 ack。
    pub fn tick(&mut self) -> PumpOutcome {
        let filtered = self.filter.tick();
        self.apply_filtered(filtered)
    }

    /// 终端的同步更新（`CSI ?2026h`）超时了就结束它，见 [`zssh_terminal::TerminalModel::expire_sync`]。
    /// 返回屏幕有没有因此变化。
    pub fn expire_sync(&mut self) -> bool {
        self.model.expire_sync()
    }

    /// 会话结束：过滤器压着的尾巴不会再有后续数据可等，当场放行（按消费后的字节 ack）。
    pub fn flush(&mut self) -> PumpOutcome {
        let filtered = self.filter.flush();
        self.apply_filtered(filtered)
    }

    /// 界面自己要显示的文字（"正在连接…"、断线提示）：直接进终端，
    /// 不经过滤、不记日志、不进字节账本，也不碰流控门。
    pub fn feed_local(&mut self, bytes: &[u8]) {
        // 本地文字不是远端的输出，里面的查询序列不产生协议回复。
        self.model.feed_local(bytes);
    }

    /// 取走终端对远端查询（DSR / DA 等）产生的协议回复，连同它们所属的连接代次。
    ///
    /// 不依赖界面重绘：每次喂完输出（包括隐藏标签、非活动窗格）调用方都该取一次，写回这个会话。
    /// 回复属于产生它的会话，调用方必须用同一个代次交给写端，写端会丢掉代次不符的。
    pub fn take_replies(&mut self) -> Option<TerminalReplies> {
        let bytes = self.model.take_replies();
        if bytes.is_empty() {
            return None;
        }
        Some(TerminalReplies {
            generation: self.generation,
            bytes,
        })
    }

    /// 队列满而丢掉的回复条数（诊断用）。
    pub fn dropped_replies(&self) -> u64 {
        self.model.dropped_replies()
    }

    pub fn resize(&mut self, size: Size) {
        self.model.resize(size);
    }

    /// 改回滚上限（设置里的回滚行数），对正在用的终端立刻生效。
    pub fn set_history(&mut self, history: usize) {
        self.model.set_history(history);
    }

    /// 当前的回滚上限。
    pub fn history_limit(&self) -> usize {
        self.model.history_limit()
    }

    pub fn snapshot(&self) -> Snapshot {
        self.model.snapshot()
    }

    pub fn history_size(&self) -> usize {
        self.model.history_size()
    }

    pub fn app_cursor(&self) -> bool {
        self.model.app_cursor()
    }

    pub fn bracketed_paste(&self) -> bool {
        self.model.bracketed_paste()
    }

    pub fn mouse_mode(&self) -> zssh_terminal::MouseMode {
        self.model.mouse_mode()
    }

    pub fn cursor_visible(&self) -> Option<(usize, usize)> {
        self.model.cursor_visible()
    }

    /// 正数滚回历史，负数回到当前。
    pub fn scroll(&mut self, delta: i32) {
        self.model.scroll(delta);
    }

    pub fn scroll_to_top(&mut self) {
        self.model.scroll_to_top();
    }

    pub fn scroll_to_bottom(&mut self) {
        self.model.scroll_to_bottom();
    }

    pub fn display_offset(&self) -> usize {
        self.model.display_offset()
    }

    /// 把 `old` 泵里的终端（屏幕 + 回滚缓冲）接过来，`old` 拿到这边原来的（通常是空的）。
    /// 重连时用：连接、流控、过滤器、代次都是新的，终端内容沿用旧的。
    pub fn adopt_terminal<G: ByteFilter>(&mut self, old: &mut SessionPump<G>) {
        std::mem::swap(&mut self.model, &mut old.model);
    }

    /// 接新会话之前复位旧会话遗留的终端状态（备用屏幕、鼠标上报……），不清屏不清回滚。
    pub fn reset_terminal_for_new_session(&mut self) {
        self.model.reset_for_new_session();
    }

    /// 清屏：本地清掉屏幕和回滚，只留光标所在行在第一行。不向远端发任何东西。
    pub fn clear_screen(&mut self) {
        self.model.clear_keep_cursor_line();
    }

    /// 在屏幕和回滚里搜字面量，见 [`zssh_terminal::TerminalModel::search`]。
    pub fn search(&self, query: &str, case_sensitive: bool) -> Vec<zssh_terminal::SearchMatch> {
        self.model.search(query, case_sensitive)
    }

    /// 滚到网格行 `line` 可见（见 [`zssh_terminal::TerminalModel::scroll_to_line`]）。
    pub fn scroll_to_line(&mut self, line: i32) {
        self.model.scroll_to_line(line);
    }

    /// 文字内容的修订号（滚动不算）。
    pub fn content_revision(&self) -> u64 {
        self.model.content_revision()
    }

    pub fn alt_screen(&self) -> bool {
        self.model.alt_screen()
    }

    /// 可见区两点之间的文字，用于复制。
    pub fn selection_text(&self, start: (usize, usize), end: (usize, usize)) -> String {
        self.model.text_between(start, end)
    }

    pub fn ledger(&self) -> Ledger {
        self.ledger
    }

    pub fn gate(&self) -> &FlowGate {
        &self.gate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DropHidden;

    impl ByteFilter for DropHidden {
        fn push(&mut self, bytes: &[u8]) -> Filtered {
            // 测试用：以 HIDDEN 开头的批次整批吞掉，模拟显示过滤。
            if bytes.starts_with(b"HIDDEN") {
                Filtered {
                    data: Vec::new(),
                    suppressed: bytes.len(),
                }
            } else {
                Filtered {
                    data: bytes.to_vec(),
                    suppressed: 0,
                }
            }
        }

        fn tick(&mut self) -> Filtered {
            Filtered {
                data: Vec::new(),
                suppressed: 0,
            }
        }
    }

    fn pump<F: ByteFilter>(filter: F) -> SessionPump<F> {
        SessionPump::new(
            Size { rows: 10, cols: 40 },
            1_000,
            Arc::new(FlowGate::new()),
            filter,
            7,
        )
    }

    #[test]
    fn pass_through_conserves_bytes_and_acks() {
        let mut pump = pump(PassThrough);
        pump.on_data(7, b"hello\r\n");
        pump.on_data(7, b"world\r\n");

        let ledger = pump.ledger();
        assert_eq!(ledger.raw, 14);
        assert_eq!(ledger.consumed, 14);
        assert_eq!(ledger.filtered, 0);
        assert!(ledger.is_balanced());
        assert_eq!(pump.gate().unacked(), 0);
        assert_eq!(pump.snapshot().rows[0].text, "hello");
        assert_eq!(pump.snapshot().rows[1].text, "world");
    }

    #[test]
    fn filtered_bytes_are_acked_by_the_filter_side() {
        let mut pump = pump(DropHidden);
        pump.on_data(7, b"HIDDEN secret bytes");
        pump.on_data(7, b"ok\r\n");

        let ledger = pump.ledger();
        assert_eq!(ledger.raw, 19 + 4);
        assert_eq!(ledger.filtered, 19);
        assert_eq!(ledger.consumed, 4);
        assert!(ledger.is_balanced());
        assert_eq!(pump.gate().unacked(), 0);
        assert_eq!(pump.snapshot().rows[0].text, "ok");
    }

    /// B-1：核心转发任务先 `on_sent`，泵只 ack。累计输出超过高水位很多倍后，
    /// 未确认量必须回到 0，读通道的门不能卡住。
    #[test]
    fn core_side_on_sent_is_not_double_counted() {
        let gate = Arc::new(FlowGate::new());
        let mut pump = SessionPump::new(
            Size { rows: 10, cols: 40 },
            1_000,
            Arc::clone(&gate),
            PassThrough,
            1,
        );
        let chunk = vec![b'x'; 64 * 1024];
        let total = 3 * 1024 * 1024;
        let mut sent = 0;
        while sent < total {
            // 与 `shell.rs::send_data` 一致：发送方记账，随后消费方处理并 ack。
            gate.on_sent(chunk.len());
            pump.on_data(1, &chunk);
            sent += chunk.len();
        }

        assert_eq!(gate.unacked(), 0, "泵不能再记一次账");
        assert!(gate.peak_unacked() <= chunk.len());
        assert!(pump.ledger().is_balanced());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let ready = runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_millis(200), gate.wait_ready()).await
        });
        assert_eq!(ready, Ok(true), "输出超过 2 MiB 后读通道不应暂停");
    }

    #[test]
    fn stale_generation_is_dropped_without_ack() {
        let mut pump = pump(PassThrough);
        pump.set_generation(8);
        let outcome = pump.on_data(7, b"old session data");

        assert!(!outcome.accepted);
        assert_eq!(pump.ledger().stale_batches, 1);
        assert_eq!(pump.snapshot().rows[0].text, "");
        assert_eq!(pump.gate().unacked(), 0);
    }

    #[test]
    fn hidden_tab_consumes_without_any_snapshot() {
        let mut pump = pump(PassThrough);
        let chunk = b"line\r\n";
        for _ in 0..10_000 {
            pump.on_data(7, chunk);
        }
        // 全程没有调用 snapshot：消费与 ACK 不依赖绘制。
        assert_eq!(pump.ledger().consumed, 60_000);
        assert!(pump.ledger().is_balanced());
        assert_history_bounded(&pump);
    }

    fn assert_history_bounded(pump: &SessionPump<PassThrough>) {
        assert!(pump.history_size() <= 1_000);
    }

    #[test]
    fn core_display_filter_passes_plain_text() {
        let mut pump = SessionPump::new(
            Size { rows: 10, cols: 40 },
            1_000,
            Arc::new(FlowGate::new()),
            CoreFilter::new(),
            1,
        );
        pump.on_data(1, b"hello\r\n");
        assert_eq!(pump.snapshot().rows[0].text, "hello");
        assert!(pump.ledger().is_balanced());
        assert_eq!(pump.gate().unacked(), 0);
    }

    #[test]
    fn terminal_queries_become_replies_tagged_with_the_generation() {
        let mut pump = pump(PassThrough);
        assert_eq!(pump.take_replies(), None);

        pump.on_data(7, b"hi\x1b[6n\x1b[5n");
        let replies = pump.take_replies().expect("有两条回复");
        assert_eq!(replies.generation, 7);
        assert_eq!(replies.bytes, b"\x1b[1;3R\x1b[0n");
        assert_eq!(pump.take_replies(), None, "取走之后没有了");
        // 回复不影响字节账本。
        assert!(pump.ledger().is_balanced());
    }

    #[test]
    fn stale_data_and_old_generation_replies_never_leave_the_pump() {
        let mut pump = pump(PassThrough);
        // 旧代次的迟到数据整批丢弃，里面的查询没有人回答。
        pump.set_generation(8);
        pump.on_data(7, b"\x1b[5n");
        assert_eq!(pump.take_replies(), None);

        // 还没取走的回复遇到换代次：作废，不能顶着新代次发出去。
        pump.on_data(8, b"\x1b[5n");
        pump.set_generation(9);
        assert_eq!(pump.take_replies(), None);
        pump.on_data(9, b"\x1b[5n");
        assert_eq!(pump.take_replies().map(|r| r.generation), Some(9));
    }

    #[test]
    fn a_new_pump_can_adopt_the_old_terminal_and_old_generation_data_stays_out() {
        let mut old = pump(PassThrough);
        old.on_data(7, b"old screen\r\n");
        let mut fresh = SessionPump::new(
            Size { rows: 10, cols: 40 },
            1_000,
            Arc::new(FlowGate::new()),
            PassThrough,
            8,
        );
        fresh.adopt_terminal(&mut old);
        fresh.reset_terminal_for_new_session();
        assert_eq!(fresh.snapshot().rows[0].text, "old screen");
        // 新会话的数据接在后面；旧代次迟到的数据被丢弃，账本只记新会话的。
        fresh.on_data(8, b"new\r\n");
        assert!(!fresh.on_data(7, b"late\r\n").accepted);
        let rows: Vec<_> = fresh
            .snapshot()
            .rows
            .iter()
            .map(|r| r.text.clone())
            .collect();
        assert_eq!(&rows[..2], ["old screen", "new"]);
        assert!(fresh.ledger().is_balanced());
        assert_eq!(fresh.ledger().raw, 5);
        // 被换走的是空终端，不带旧内容。
        assert_eq!(old.snapshot().rows[0].text, "");
    }

    #[test]
    fn clear_screen_is_local_only_and_keeps_the_cursor_line() {
        let mut pump = pump(PassThrough);
        for i in 0..30 {
            pump.on_data(7, format!("line{i}\r\n").as_bytes());
        }
        pump.on_data(7, b"$ ");
        assert!(pump.history_size() > 0);
        pump.clear_screen();
        assert_eq!(pump.history_size(), 0);
        assert_eq!(pump.snapshot().rows[0].text, "$");
        assert_eq!(pump.take_replies(), None, "清屏不向远端发任何东西");
        // 账本没有因为清屏变化：它只是本地操作。
        assert!(pump.ledger().is_balanced());
    }

    #[test]
    fn local_notices_do_not_ask_the_remote() {
        let mut pump = pump(PassThrough);
        pump.feed_local(b"\x1b[6n notice");
        assert_eq!(pump.take_replies(), None);
    }

    #[test]
    fn display_filtered_bytes_do_not_reach_the_terminal_so_they_get_no_reply() {
        let mut pump = pump(DropHidden);
        pump.on_data(7, b"HIDDEN \x1b[5n");
        assert_eq!(pump.take_replies(), None);
    }

    /// 远端输出里的 shell 标记经泵到达调用方；界面自己写的本地文字和旧代次的迟到数据都不产生标记。
    #[test]
    fn shell_marks_come_from_remote_output_only_and_respect_the_generation() {
        let mut pump = pump(PassThrough);
        pump.feed_local(b"connecting \x1b]133;B\x07\r\n");
        pump.on_data(6, b"\x1b]133;A\x07stale generation");
        assert!(pump.take_shell_events().is_empty());

        pump.on_data(7, b"$ \x1b]133;B\x07ls");
        let events = pump.take_shell_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].mark.kind, zssh_terminal::ShellMarkKind::PromptEnd);
        let probe = pump.screen_probe();
        assert_eq!((probe.cursor_row, probe.cursor_col), (1, 4));
        assert_eq!((events[0].anchor.row, events[0].anchor.col), (1, 2));
        let cells = pump.line_cells(1).expect("第 1 行");
        assert_eq!(cells[2].ch, 'l');
        assert!(pump.take_shell_events().is_empty());
        assert!(pump.ledger().is_balanced());
    }

    #[test]
    fn ai_arms_are_counted_only_for_the_current_generation() {
        let mut pump = pump(PassThrough);
        assert_eq!(pump.ai_arm_count(), 0);
        pump.ai_arm(7, "m", "p", "ls");
        pump.ai_arm(6, "m", "p", "ls");
        assert_eq!(pump.ai_arm_count(), 1);
    }

    #[test]
    fn scroll_moves_the_viewport() {
        let mut pump = pump(PassThrough);
        for i in 0..50 {
            pump.on_data(7, format!("line{i}\r\n").as_bytes());
        }
        let bottom = pump.snapshot().rows[0].text.clone();
        pump.scroll(3);
        let scrolled = pump.snapshot().rows[0].text.clone();
        assert_ne!(bottom, scrolled, "滚回历史后可见区应变化");
        assert!(pump.ledger().is_balanced());
    }

    #[test]
    fn mouse_mode_follows_decset_and_decreset() {
        let mut pump = pump(PassThrough);
        assert!(!pump.mouse_mode().reports_buttons());

        pump.on_data(7, b"\x1b[?1002h\x1b[?1006h");
        let mode = pump.mouse_mode();
        assert!(mode.drag && mode.sgr);
        assert!(!mode.click);
        assert!(!mode.motion);
        assert!(mode.reports_motion());

        pump.on_data(7, b"\x1b[?1002l\x1b[?1006l");
        assert!(!pump.mouse_mode().reports_buttons());
    }

    #[test]
    fn plain_log_gets_the_display_bytes_as_text_without_colors_or_hidden_bytes() {
        let guard = tempfile::Builder::new()
            .prefix("zssh-pump-plain-")
            .tempdir()
            .unwrap();
        let dir = guard.path().to_path_buf();

        let mut pump = pump(DropHidden);
        let log = zssh_core::ssh::session_log::SessionLog::create(&dir, "web-1", "t").unwrap();
        let path = log.path().expect("同步创建的日志有路径").to_path_buf();
        pump.set_plain_log(log);
        assert!(pump.has_plain_log());
        pump.on_data(7, b"HIDDEN secret bytes");
        pump.on_data(7, b"\x1b[31mred\x1b[0m line\r\n");
        // 丢掉日志不再等写线程（R10），要确认落盘就显式等它收尾。
        let log = pump.take_plain_log().expect("日志应还在");
        assert!(log.close_and_wait(std::time::Duration::from_secs(10)));
        assert!(!pump.has_plain_log());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "red line\n");
    }

    #[test]
    fn resize_keeps_pump_usable() {
        let mut pump = pump(PassThrough);
        pump.on_data(7, b"abc\r\n");
        pump.resize(Size { rows: 4, cols: 20 });
        pump.on_data(7, b"def\r\n");
        let snapshot = pump.snapshot();
        assert_eq!(snapshot.size, Size { rows: 4, cols: 20 });
        assert_eq!(snapshot.rows[1].text, "def");
    }

    /// 手动跑：`cargo test -p zssh-session pump_replay_100mib -- --ignored --nocapture`
    #[test]
    #[ignore = "100 MiB 回放：过滤 + 流控 + 模型的吞吐测试"]
    fn pump_replay_100mib() {
        let gate = Arc::new(FlowGate::new());
        let mut pump = SessionPump::new(
            Size {
                rows: 50,
                cols: 200,
            },
            10_000,
            gate,
            CoreFilter::new(),
            1,
        );
        let chunk = {
            let mut line = Vec::new();
            for i in 0..16 {
                line.extend_from_slice(
                    format!("line {i} 中文 emoji \u{1f600} \x1b[31mred\x1b[0m\r\n").as_bytes(),
                );
            }
            line
        };
        let total = 100 * 1024 * 1024;
        let started = Instant::now();
        let mut fed = 0usize;
        while fed < total {
            pump.on_data(1, &chunk);
            pump.tick();
            fed += chunk.len();
        }
        let elapsed = started.elapsed();
        println!("pump fed {fed} bytes in {elapsed:?}");
        let ledger = pump.ledger();
        assert_eq!(ledger.raw, fed);
        assert!(ledger.is_balanced(), "账本不平衡：{ledger:?}");
        assert!(pump.snapshot().history_size <= 10_000);
        assert!(
            elapsed < std::time::Duration::from_secs(120),
            "回放太慢：{elapsed:?}"
        );
    }
}
