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
pub mod log;

pub use driver::{
    ChannelBackend, LoopbackBackend, POLL_BYTE_BUDGET, SessionBackend, SessionCommand,
    SessionDriver, SessionEvent, SessionExit, map_core_event,
};
pub use keys::{
    KeyInput, MouseAction, MouseButton, MouseInput, encode_key, encode_mouse, encode_paste,
};
pub use log::SessionLog;

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
    log: Option<crate::log::SessionLog>,
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
            log: None,
        }
    }

    /// 挂上一个会话日志；只记录显示过滤之后、交给终端的字节。
    pub fn set_log(&mut self, log: crate::log::SessionLog) {
        self.log = Some(log);
    }

    /// 当前是否在记录；重连后据此继续记录。
    pub fn has_log(&self) -> bool {
        self.log.is_some()
    }

    /// 取回日志并停止记录；调用方负责 `finish` 刷盘。
    pub fn take_log(&mut self) -> Option<crate::log::SessionLog> {
        self.log.take()
    }

    /// 切换到新代次；旧代次的迟到数据随后会被丢弃。
    pub fn set_generation(&mut self, generation: u64) {
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
        if let Some(log) = &mut self.log {
            log.write(&filtered.data);
        }

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

    fn apply_filtered(&mut self, filtered: Filtered) {
        if filtered.suppressed > 0 {
            self.gate.on_ack(filtered.suppressed);
            self.ledger.filtered += filtered.suppressed;
            self.ledger.acked += filtered.suppressed;
        }
        if let Some(log) = &mut self.log {
            log.write(&filtered.data);
        }
        let consumed = self.model.feed(&filtered.data);
        if consumed > 0 {
            self.gate.on_ack(consumed);
            self.ledger.consumed += consumed;
            self.ledger.acked += consumed;
        }
    }

    /// 过滤器的定时器：压住的尾巴到时间后放行，并按消费后的字节 ack。
    pub fn tick(&mut self) -> PumpOutcome {
        let filtered = self.filter.tick();
        if filtered.suppressed > 0 {
            self.gate.on_ack(filtered.suppressed);
            self.ledger.filtered += filtered.suppressed;
            self.ledger.acked += filtered.suppressed;
        }
        if let Some(log) = &mut self.log {
            log.write(&filtered.data);
        }
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

    /// 界面自己要显示的文字（"正在连接…"、断线提示）：直接进终端，
    /// 不经过滤、不记日志、不进字节账本，也不碰流控门。
    pub fn feed_local(&mut self, bytes: &[u8]) {
        self.model.feed(bytes);
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
    fn pump_logs_only_display_bytes() {
        let path = std::env::temp_dir().join(format!("zssh-pump-log-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let mut pump = pump(DropHidden);
        pump.set_log(crate::log::SessionLog::create(&path, 1024).unwrap());
        pump.on_data(7, b"HIDDEN secret bytes");
        pump.on_data(7, b"ok\r\n");
        let log = pump.take_log().expect("日志应还在");
        assert_eq!(log.finish(), 4);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ok\r\n");
        let _ = std::fs::remove_file(&path);
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
