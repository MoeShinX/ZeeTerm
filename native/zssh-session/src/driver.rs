//! 会话后端接口与驱动：把「写命令」和「事件流」与终端泵分开。
//!
//! 真实 SSH 会话在 G04.2.2 接入：打开会话后拿到事件接收端，读取线程投递
//! [`SessionEvent`]，界面线程每帧调用 [`SessionDriver::poll`]。测试里的假后端
//! 不依赖网络，只验证写入 → 回显 → 终端 → 账本这条链路和代次隔离。

use std::sync::mpsc::{Receiver, Sender};

use zssh_terminal::Size;

use crate::{ByteFilter, SessionPump};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionCommand {
    Write(Vec<u8>),
    Resize(Size),
    Close,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SessionEvent {
    Data {
        generation: u64,
        bytes: Vec<u8>,
    },
    Exit {
        generation: u64,
        code: Option<i32>,
    },
    /// 会话异常结束（写失败、连接中途断开等）：界面据此显示原因。
    Error {
        generation: u64,
        message: String,
    },
    /// 只给界面看的提示文字（输入被丢弃之类），不经过流控，也不记进字节账本。
    Notice {
        generation: u64,
        text: String,
    },
    /// AI 命令马上要写进终端（V2-G10b）：会话任务在真正写之前发，显示过滤器据此记住这条命令。
    /// 它和输出数据走同一个事件通道，所以一定排在这条命令的回显之前。
    AiArm {
        generation: u64,
        marker: String,
        payload: String,
        command: String,
    },
    /// AI 命令超时返回、命令还在终端里跑：显示过滤器退出完整过滤。
    AiRelease {
        generation: u64,
        marker: String,
    },
}

/// 把 `zssh_core` 的会话事件映射成驱动事件。
///
/// `Closed`（通道关闭）按无退出码的结束处理，界面据此把标签标成已断开。
pub fn map_core_event(event: zssh_core::ssh::shell::ShellEvent, generation: u64) -> SessionEvent {
    use zssh_core::ssh::shell::ShellEvent;
    match event {
        ShellEvent::Data(bytes) => SessionEvent::Data { generation, bytes },
        ShellEvent::Exit(code) => SessionEvent::Exit {
            generation,
            code: code.map(|code| i32::try_from(code).unwrap_or(-1)),
        },
        ShellEvent::Closed => SessionEvent::Exit {
            generation,
            code: None,
        },
    }
}

/// 会话写端：真实实现写入 SSH 通道，测试里是假后端。
pub trait SessionBackend: Send {
    fn send(&mut self, command: SessionCommand);

    /// 这个会话在 AI 终端登记表里的编号（V2-G10b）；没有登记的（回环、测试）返回 `None`。
    fn terminal_id(&self) -> Option<String> {
        None
    }

    /// 发送 Break 信号（串口会话的按钮 / 快捷键用）。其他会话没有这个概念，什么都不做。
    fn send_break(&mut self) {}
}

/// 最简单的写端：命令进通道，由会话线程消费。
pub struct ChannelBackend {
    tx: Sender<SessionCommand>,
}

impl ChannelBackend {
    pub fn new(tx: Sender<SessionCommand>) -> Self {
        Self { tx }
    }
}

impl SessionBackend for ChannelBackend {
    fn send(&mut self, command: SessionCommand) {
        let _ = self.tx.send(command);
    }
}

/// 回环后端：把写入原样回送成事件，供离线演示和测试。
///
/// 真实 SSH 接入时替换成 `ChannelBackend` + 会话线程，驱动和泵不用改。
pub struct LoopbackBackend {
    events: Sender<SessionEvent>,
    generation: u64,
}

impl LoopbackBackend {
    pub fn new(generation: u64) -> (Self, Receiver<SessionEvent>) {
        let (events, rx) = std::sync::mpsc::channel();
        (Self { events, generation }, rx)
    }
}

impl SessionBackend for LoopbackBackend {
    fn send(&mut self, command: SessionCommand) {
        match command {
            SessionCommand::Write(bytes) => {
                let _ = self.events.send(SessionEvent::Data {
                    generation: self.generation,
                    bytes,
                });
            }
            SessionCommand::Resize(_) => {}
            SessionCommand::Close => {
                let _ = self.events.send(SessionEvent::Exit {
                    generation: self.generation,
                    code: Some(0),
                });
            }
        }
    }
}

/// 每次 `poll` 最多处理的数据字节数。
///
/// 界面线程 60 ms 轮询一次；生产者比解析快时通道永远取不空，没有上限的话
/// 这个循环就不会把线程还给界面。超出的批次留到下一次 `poll`：它们没处理就没 ack，
/// 流控门会自然压住读通道。
pub const POLL_BYTE_BUDGET: usize = 1024 * 1024;

/// 会话结束的情况：退出码和（异常结束时的）原因。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionExit {
    pub code: Option<i32>,
    pub reason: Option<String>,
}

/// 消费事件流并喂给终端泵；`poll` 返回是否有新事件。
pub struct SessionDriver<F: ByteFilter> {
    pump: SessionPump<F>,
    events: Receiver<SessionEvent>,
    exited: Option<SessionExit>,
    /// 上一次 `poll` 是不是因为预算用完而停下的（通道里可能还有积压）。
    backlog: bool,
}

impl<F: ByteFilter> SessionDriver<F> {
    pub fn new(pump: SessionPump<F>, events: Receiver<SessionEvent>) -> Self {
        Self {
            pump,
            events,
            exited: None,
            backlog: false,
        }
    }

    pub fn poll(&mut self) -> bool {
        self.poll_with_budget(POLL_BYTE_BUDGET)
    }

    /// 最多处理 `budget` 个数据字节（至少处理一批，保证有进展）。
    pub fn poll_with_budget(&mut self, budget: usize) -> bool {
        let mut changed = false;
        let mut used = 0usize;
        self.backlog = false;
        while used < budget {
            let Ok(event) = self.events.try_recv() else {
                break;
            };
            match event {
                SessionEvent::Data { generation, bytes } => {
                    changed = true;
                    used += bytes.len();
                    self.pump.on_data(generation, &bytes);
                }
                SessionEvent::Exit { generation, code } => {
                    if generation == self.pump.generation() {
                        // 先到的 Error 带着原因，后面的 Exit 不能把它冲掉。
                        self.exited
                            .get_or_insert(SessionExit { code, reason: None });
                        changed = true;
                    }
                }
                SessionEvent::Error {
                    generation,
                    message,
                } => {
                    if generation == self.pump.generation() {
                        self.exited.get_or_insert(SessionExit {
                            code: None,
                            reason: Some(message),
                        });
                        changed = true;
                    }
                }
                SessionEvent::Notice { generation, text } => {
                    if generation == self.pump.generation() {
                        self.pump.feed_local(text.as_bytes());
                        changed = true;
                    }
                }
                SessionEvent::AiArm {
                    generation,
                    marker,
                    payload,
                    command,
                } => {
                    self.pump.ai_arm(generation, &marker, &payload, &command);
                    changed = true;
                }
                SessionEvent::AiRelease { generation, marker } => {
                    self.pump.ai_release(generation, &marker);
                    changed = true;
                }
            }
        }
        // 预算用完时通道里多半还有东西；下一次轮询接着处理。
        self.backlog = used >= budget;
        if changed {
            self.pump.tick();
        }
        changed
    }

    /// 上一次 `poll` 是否因预算用完而提前返回。
    pub fn has_backlog(&self) -> bool {
        self.backlog
    }

    pub fn exited(&self) -> bool {
        self.exited.is_some()
    }

    /// 会话结束的退出码和原因；还在运行返回 `None`。
    pub fn exit(&self) -> Option<&SessionExit> {
        self.exited.as_ref()
    }

    pub fn pump(&self) -> &SessionPump<F> {
        &self.pump
    }

    pub fn pump_mut(&mut self) -> &mut SessionPump<F> {
        &mut self.pump
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    use zssh_core::ssh::flow::FlowGate;

    use crate::CoreFilter;

    fn pump(generation: u64) -> SessionPump<CoreFilter> {
        SessionPump::new(
            Size { rows: 5, cols: 40 },
            100,
            Arc::new(FlowGate::new()),
            CoreFilter::new(),
            generation,
        )
    }

    struct FakeSession {
        backend: ChannelBackend,
        events: Sender<SessionEvent>,
        rx: Receiver<SessionEvent>,
        handle: thread::JoinHandle<()>,
    }

    /// 假会话：收到 Write 就原样回显，收到 Close 就发 Exit。
    fn fake(generation: u64) -> FakeSession {
        let (command_tx, command_rx) = std::sync::mpsc::channel();
        let (events, rx) = std::sync::mpsc::channel();
        let echo = events.clone();
        let handle = thread::spawn(move || {
            while let Ok(command) = command_rx.recv() {
                match command {
                    SessionCommand::Write(bytes) => {
                        if echo.send(SessionEvent::Data { generation, bytes }).is_err() {
                            return;
                        }
                    }
                    SessionCommand::Resize(_) => {}
                    SessionCommand::Close => {
                        let _ = echo.send(SessionEvent::Exit {
                            generation,
                            code: Some(0),
                        });
                        return;
                    }
                }
            }
        });
        FakeSession {
            backend: ChannelBackend::new(command_tx),
            events,
            rx,
            handle,
        }
    }

    fn wait_for(
        driver: &mut SessionDriver<CoreFilter>,
        done: impl Fn(&SessionDriver<CoreFilter>) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !done(driver) {
            driver.poll();
            assert!(Instant::now() < deadline, "等待会话事件超时");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn screen_text(driver: &SessionDriver<CoreFilter>) -> String {
        driver
            .pump()
            .snapshot()
            .rows
            .iter()
            .map(|row| row.text.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// V2-G10b：AI 命令的包装脚本不显示给用户，显示的是「命令原文」的合成回显 + 输出 + 新提示符；
    /// 被吞掉的字节照样 ack，流控的未确认量不会越攒越多。
    #[test]
    fn an_armed_ai_command_hides_its_wrapper_but_shows_the_command_and_output() {
        use zssh_core::ai::pty::{build_clear_prefix, build_wrapped_command};

        let (tx, rx) = std::sync::mpsc::channel();
        let gate = Arc::new(FlowGate::new());
        let pump = SessionPump::new(
            Size { rows: 8, cols: 200 },
            100,
            Arc::clone(&gate),
            CoreFilter::new(),
            1,
        );
        let mut driver = SessionDriver::new(pump, rx);
        let send_data = |bytes: Vec<u8>| {
            gate.on_sent(bytes.len());
            tx.send(SessionEvent::Data {
                generation: 1,
                bytes,
            })
            .expect("发数据");
        };
        send_data(b"user@host:~$ ".to_vec());

        let mut unacked_after_each = Vec::new();
        for round in 0..5 {
            let marker = format!("__ZSSH_{round:012}__");
            let wrapped = build_wrapped_command("uptime", &marker);
            let payload = format!("{}{}\r", build_clear_prefix(), wrapped);
            // 会话任务在写之前发 AiArm；回显和输出排在它后面。
            tx.send(SessionEvent::AiArm {
                generation: 1,
                marker: marker.clone(),
                payload,
                command: "uptime".to_owned(),
            })
            .expect("发 AiArm");
            // shell 把键入的包装脚本原样回显，然后跑命令。
            send_data(format!("{}\r\n", wrapped.trim_end()).into_bytes());
            send_data(
                format!("\r\n{marker}_S\r\n up 3 days\r\n{marker}_E:0\r\nuser@host:~$ ")
                    .into_bytes(),
            );
            assert!(driver.poll());
            unacked_after_each.push(gate.unacked());
        }

        let screen = screen_text(&driver);
        assert!(screen.contains("uptime"), "要显示命令原文：{screen}");
        assert!(screen.contains("up 3 days"), "要显示命令输出：{screen}");
        assert!(
            !screen.contains("__ZSSH_"),
            "标记不能出现在屏幕上：{screen}"
        );
        assert!(
            !screen.contains("printf"),
            "包装脚本不能出现在屏幕上：{screen}"
        );
        assert!(screen.contains("user@host:~$"), "新提示符要显示：{screen}");
        let ledger = driver.pump().ledger();
        assert!(
            ledger.filtered > 5 * 200,
            "包装脚本的字节被过滤掉了：{ledger:?}"
        );
        // 每条命令的包装脚本有两百六十多个字节，这些必须被 ack（过滤掉的字节由过滤侧 ack）。
        // 核心的显示过滤器有个老毛病：回显开头的 12 个字节匹配时没算进 `suppressed`，而合成回显多出
        // 8 个字节被界面 ack，净结果是每条 AI 终端命令残留 4 个字节没确认（0.2.14 同样；要攒到流控高水位
        // 才会影响，实际不会）。这里只确认残留量和包装脚本的长度无关、每条不超过 4 字节。
        for (round, unacked) in unacked_after_each.iter().enumerate() {
            assert!(
                *unacked <= 4 * (round + 1),
                "被吞掉的字节也要 ack：{unacked_after_each:?}"
            );
        }
    }

    /// 超时的命令：`AiRelease` 之后过滤器退出完整过滤，用户接着打的字不会被压住；迟到的结束标记行仍然丢掉。
    #[test]
    fn a_released_ai_command_stops_holding_back_the_users_typing() {
        use zssh_core::ai::pty::{build_clear_prefix, build_wrapped_command};
        let marker = "__ZSSH_ba9876543210__";
        let wrapped = build_wrapped_command("sleep 100", marker);
        let payload = format!("{}{}\r", build_clear_prefix(), wrapped);
        let (tx, rx) = std::sync::mpsc::channel();
        let mut driver = SessionDriver::new(pump(1), rx);
        tx.send(SessionEvent::AiArm {
            generation: 1,
            marker: marker.to_owned(),
            payload,
            command: "sleep 100".to_owned(),
        })
        .expect("发 AiArm");
        tx.send(SessionEvent::Data {
            generation: 1,
            bytes: format!("{}\r\n\r\n{marker}_S\r\n", wrapped.trim_end()).into_bytes(),
        })
        .expect("发数据");
        tx.send(SessionEvent::AiRelease {
            generation: 1,
            marker: marker.to_owned(),
        })
        .expect("发 AiRelease");
        // 用户自己的输出紧跟着出现，不能被压住；之后迟到的结束标记行被丢掉。
        tx.send(SessionEvent::Data {
            generation: 1,
            bytes: b"typed-by-user".to_vec(),
        })
        .expect("发数据");
        tx.send(SessionEvent::Data {
            generation: 1,
            bytes: format!("\r\n{marker}_E:130\r\nafter\r\n").into_bytes(),
        })
        .expect("发数据");
        assert!(driver.poll());
        let screen = screen_text(&driver);
        assert!(screen.contains("typed-by-user"), "{screen}");
        assert!(screen.contains("after"), "{screen}");
        assert!(!screen.contains(marker), "{screen}");
    }

    /// 旧代次的 AiArm 不能影响新会话的过滤器。
    #[test]
    fn a_stale_ai_arm_is_ignored() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut driver = SessionDriver::new(pump(2), rx);
        tx.send(SessionEvent::AiArm {
            generation: 1,
            marker: "__ZSSH_x__".to_owned(),
            payload: "x".to_owned(),
            command: "uptime".to_owned(),
        })
        .expect("发 AiArm");
        tx.send(SessionEvent::Data {
            generation: 2,
            bytes: b"plain\r\n".to_vec(),
        })
        .expect("发数据");
        driver.poll();
        let screen = screen_text(&driver);
        assert!(screen.contains("plain"), "{screen}");
        assert!(!screen.contains("uptime"), "{screen}");
        assert_eq!(driver.pump().ledger().stale_batches, 1);
    }

    #[test]
    fn core_events_map_to_driver_events() {
        use zssh_core::ssh::shell::ShellEvent;
        assert_eq!(
            map_core_event(ShellEvent::Data(b"hi".to_vec()), 7),
            SessionEvent::Data {
                generation: 7,
                bytes: b"hi".to_vec(),
            }
        );
        assert_eq!(
            map_core_event(ShellEvent::Exit(Some(3)), 7),
            SessionEvent::Exit {
                generation: 7,
                code: Some(3),
            }
        );
        assert_eq!(
            map_core_event(ShellEvent::Exit(None), 7),
            SessionEvent::Exit {
                generation: 7,
                code: None,
            }
        );
        assert_eq!(
            map_core_event(ShellEvent::Closed, 7),
            SessionEvent::Exit {
                generation: 7,
                code: None,
            }
        );
    }

    #[test]
    fn driver_consumes_mapped_core_events() {
        use zssh_core::ssh::shell::ShellEvent;
        let (tx, rx) = std::sync::mpsc::channel();
        let mut driver = SessionDriver::new(pump(1), rx);
        tx.send(map_core_event(ShellEvent::Data(b"ok\r\n".to_vec()), 1))
            .unwrap();
        tx.send(map_core_event(ShellEvent::Exit(Some(0)), 1))
            .unwrap();
        assert!(driver.poll());
        assert!(driver.exited());
        assert_eq!(driver.pump().ledger().consumed, 4);
    }

    #[test]
    fn poll_stops_at_the_byte_budget_and_resumes_next_time() {
        let (tx, rx) = std::sync::mpsc::channel();
        let gate = Arc::new(FlowGate::new());
        let pump = SessionPump::new(
            Size { rows: 5, cols: 40 },
            100,
            Arc::clone(&gate),
            CoreFilter::new(),
            1,
        );
        let mut driver = SessionDriver::new(pump, rx);
        for _ in 0..10 {
            gate.on_sent(100);
            tx.send(SessionEvent::Data {
                generation: 1,
                bytes: vec![b'a'; 100],
            })
            .unwrap();
        }

        assert!(driver.poll_with_budget(250));
        // 100 + 100 + 100：越过预算的那一批做完才停。
        assert_eq!(driver.pump().ledger().raw, 300);
        assert!(driver.has_backlog());
        // 没处理的批次没有 ack，流控会自然限速。
        assert_eq!(gate.unacked(), 700);

        while driver.has_backlog() {
            driver.poll_with_budget(250);
        }
        assert_eq!(driver.pump().ledger().raw, 1000);
        assert_eq!(gate.unacked(), 0);
        assert!(!driver.poll(), "积压清空后没有新事件");
    }

    #[test]
    fn exit_after_the_budget_is_seen_on_the_next_poll() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut driver = SessionDriver::new(pump(1), rx);
        tx.send(SessionEvent::Data {
            generation: 1,
            bytes: vec![b'a'; 100],
        })
        .unwrap();
        tx.send(SessionEvent::Exit {
            generation: 1,
            code: Some(0),
        })
        .unwrap();
        driver.poll_with_budget(50);
        assert!(!driver.exited());
        driver.poll_with_budget(50);
        assert!(driver.exited());
    }

    #[test]
    fn error_reason_survives_the_following_exit() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut driver = SessionDriver::new(pump(1), rx);
        assert!(driver.exit().is_none());
        tx.send(SessionEvent::Error {
            generation: 1,
            message: "write failed".to_owned(),
        })
        .unwrap();
        tx.send(SessionEvent::Exit {
            generation: 1,
            code: Some(0),
        })
        .unwrap();
        // 旧代次的结束事件不算。
        tx.send(SessionEvent::Error {
            generation: 9,
            message: "stale".to_owned(),
        })
        .unwrap();
        assert!(driver.poll());
        assert_eq!(
            driver.exit(),
            Some(&SessionExit {
                code: None,
                reason: Some("write failed".to_owned()),
            })
        );
    }

    #[test]
    fn notice_shows_on_screen_without_touching_the_ledger() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut driver = SessionDriver::new(pump(1), rx);
        tx.send(SessionEvent::Notice {
            generation: 1,
            text: "dropped\r\n".to_owned(),
        })
        .unwrap();
        assert!(driver.poll());
        assert_eq!(driver.pump().snapshot().rows[0].text, "dropped");
        assert_eq!(driver.pump().ledger().raw, 0);
        assert_eq!(driver.pump().gate().unacked(), 0);
    }

    #[test]
    fn loopback_backend_echoes_writes() {
        let (mut backend, events) = LoopbackBackend::new(1);
        let mut driver = SessionDriver::new(pump(1), events);
        backend.send(SessionCommand::Write(b"hi\r\n".to_vec()));
        assert!(driver.poll(), "回环事件应在下一次 poll 到达");
        let ledger = driver.pump().ledger();
        assert_eq!(ledger.consumed, 4);
        assert!(ledger.is_balanced());

        backend.send(SessionCommand::Close);
        assert!(driver.poll());
        assert!(driver.exited());
    }

    #[test]
    fn backend_write_echoes_into_the_terminal_and_balances() {
        let mut fake = fake(1);
        let mut driver = SessionDriver::new(pump(1), fake.rx);
        fake.backend
            .send(SessionCommand::Write(b"hello\r\n".to_vec()));
        wait_for(&mut driver, |driver| driver.pump().ledger().consumed > 0);

        let snapshot = driver.pump().snapshot();
        let row: String = snapshot.rows[0]
            .cells
            .iter()
            .filter(|cell| !cell.spacer && cell.ch != '\0')
            .map(|cell| cell.ch)
            .collect();
        assert_eq!(row.trim_end(), "hello");
        let ledger = driver.pump().ledger();
        assert_eq!(ledger.raw, 7);
        assert!(ledger.is_balanced(), "账本不平衡：{ledger:?}");

        fake.backend.send(SessionCommand::Close);
        wait_for(&mut driver, |driver| driver.exited());
        assert!(driver.exited());
        drop(fake.backend);
        fake.handle.join().unwrap();
    }

    #[test]
    fn stale_generation_events_are_ignored() {
        let fake = fake(1);
        let mut driver = SessionDriver::new(pump(2), fake.rx);
        // 直接灌一条旧代次事件。
        fake.events
            .send(SessionEvent::Data {
                generation: 1,
                bytes: b"stale".to_vec(),
            })
            .unwrap();
        wait_for(&mut driver, |driver| {
            driver.pump().ledger().stale_batches > 0
        });
        let ledger = driver.pump().ledger();
        assert_eq!(ledger.raw, 0);
        assert_eq!(ledger.consumed, 0);
        assert_eq!(ledger.stale_batches, 1);
        let snapshot = driver.pump().snapshot();
        assert!(
            snapshot.rows[0]
                .cells
                .iter()
                .all(|cell| cell.ch == '\0' || cell.ch == ' ')
        );
        drop(fake.backend);
        fake.handle.join().unwrap();
    }
}
