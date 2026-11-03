//! Telnet 和串口共用的会话写端。
//!
//! 和 SSH 的写端（`SshBackend`）同一个套路：`send` 在界面线程上调用，绝不阻塞——命令队列有界，
//! 满了就丢弃并提示；关闭走独立的信号，不排在可能卡住的写入后面。

use std::sync::mpsc::Sender;

use tokio::sync::{mpsc, watch};
use zssh_session::{SessionBackend, SessionCommand, SessionEvent};

/// 命令队列容量（和 SSH 会话一致）。
pub const COMMAND_QUEUE: usize = 256;

/// 会话任务收到的命令：通用的会话命令，加串口的 Break。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendCommand {
    Session(SessionCommand),
    /// 发送 Break 信号。只有串口会话有意义，Telnet 会话忽略。
    Break,
    /// 终端对远端查询的协议回复（DSR / DA 等）：照原样写给远端，不当成用户输入——
    /// 不触发自动登录的"用户已输入"、不做回车换行转换、不本地回显。
    Reply(Vec<u8>),
}

/// 往界面送事件：数据先记进流控门再发。会话任务 / 线程各持一份。
#[derive(Clone)]
pub struct EventOut {
    pub events: Sender<SessionEvent>,
    pub generation: u64,
    pub gate: std::sync::Arc<zssh_core::ssh::FlowGate>,
}

impl EventOut {
    /// 返回 `false` 表示界面那头已经没了。
    pub fn data(&self, bytes: Vec<u8>) -> bool {
        if bytes.is_empty() {
            return true;
        }
        // 先记账再发：ack 有可能在发送返回之前就回来。
        self.gate.on_sent(bytes.len());
        self.events
            .send(SessionEvent::Data {
                generation: self.generation,
                bytes,
            })
            .is_ok()
    }

    pub fn notice(&self, text: &str) {
        let _ = self.events.send(SessionEvent::Notice {
            generation: self.generation,
            text: text.to_owned(),
        });
    }

    pub fn exit(&self) {
        let _ = self.events.send(SessionEvent::Exit {
            generation: self.generation,
            code: None,
        });
    }

    pub fn error(&self, message: String) {
        let _ = self.events.send(SessionEvent::Error {
            generation: self.generation,
            message,
        });
    }
}

pub struct StreamBackend {
    commands: mpsc::Sender<BackendCommand>,
    closing: watch::Sender<bool>,
    notices: Sender<SessionEvent>,
    generation: u64,
    /// 已经提示过"输入被丢弃"，队列恢复之前不重复刷屏。
    warned: bool,
}

/// 后端的另一半：会话任务拿着它收命令、看关闭信号。
pub struct BackendEnds {
    pub commands: mpsc::Receiver<BackendCommand>,
    pub closing: watch::Receiver<bool>,
}

impl StreamBackend {
    pub fn new(
        notices: Sender<SessionEvent>,
        generation: u64,
        capacity: usize,
    ) -> (Self, BackendEnds) {
        let (commands, command_rx) = mpsc::channel(capacity);
        let (closing, closing_rx) = watch::channel(false);
        (
            Self {
                commands,
                closing,
                notices,
                generation,
                warned: false,
            },
            BackendEnds {
                commands: command_rx,
                closing: closing_rx,
            },
        )
    }

    fn enqueue(&mut self, command: BackendCommand) {
        let is_write = matches!(command, BackendCommand::Session(SessionCommand::Write(_)));
        match self.commands.try_send(command) {
            Ok(()) => self.warned = false,
            Err(mpsc::error::TrySendError::Full(_)) => {
                // 改窗口大小丢了无所谓（下一次还会发）；输入丢了要让用户知道。
                if is_write && !self.warned {
                    self.warned = true;
                    let _ = self.notices.send(SessionEvent::Notice {
                        generation: self.generation,
                        text: "\r\n\x1b[31m[远端暂时不接收输入，已丢弃部分按键]\x1b[0m\r\n"
                            .to_owned(),
                    });
                }
            }
            // 会话任务已经结束；界面会通过 Exit/Error 事件知道原因。
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

impl SessionBackend for StreamBackend {
    fn send(&mut self, command: SessionCommand) {
        if matches!(command, SessionCommand::Close) {
            let _ = self.closing.send(true);
            return;
        }
        self.enqueue(BackendCommand::Session(command));
    }

    fn send_break(&mut self) {
        self.enqueue(BackendCommand::Break);
    }

    fn send_reply(&mut self, generation: u64, bytes: Vec<u8>) {
        if generation != self.generation {
            return;
        }
        // 满了就丢：协议回复不弹"已丢弃按键"（那是给用户输入看的），也不能卡住界面线程。
        let _ = self.commands.try_send(BackendCommand::Reply(bytes));
    }
}

impl Drop for StreamBackend {
    /// 窗格关掉、后端被丢弃：当作关闭，会话任务 / 线程不能留着占着端口或连接。
    fn drop(&mut self) {
        let _ = self.closing.send(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zssh_terminal::Size;

    fn parts() -> (
        StreamBackend,
        BackendEnds,
        std::sync::mpsc::Receiver<SessionEvent>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let (backend, ends) = StreamBackend::new(tx, 3, 2);
        (backend, ends, rx)
    }

    #[test]
    fn commands_reach_the_session_in_order() {
        let (mut backend, mut ends, _rx) = parts();
        backend.send(SessionCommand::Write(b"a".to_vec()));
        backend.send_break();
        assert_eq!(
            ends.commands.try_recv().unwrap(),
            BackendCommand::Session(SessionCommand::Write(b"a".to_vec()))
        );
        assert_eq!(ends.commands.try_recv().unwrap(), BackendCommand::Break);
    }

    #[test]
    fn a_full_queue_drops_input_with_one_notice_and_never_blocks() {
        let (mut backend, mut ends, rx) = parts();
        for _ in 0..5 {
            backend.send(SessionCommand::Write(b"x".to_vec()));
        }
        let notices: Vec<_> = rx.try_iter().collect();
        assert_eq!(notices.len(), 1, "只提示一次：{notices:?}");
        assert!(matches!(
            notices[0],
            SessionEvent::Notice { generation: 3, .. }
        ));
        // 队列腾出来之后恢复正常，再满又会提示。
        ends.commands.try_recv().unwrap();
        backend.send(SessionCommand::Write(b"y".to_vec()));
        for _ in 0..3 {
            backend.send(SessionCommand::Write(b"x".to_vec()));
        }
        assert_eq!(rx.try_iter().count(), 1);
    }

    #[test]
    fn replies_reach_the_session_only_with_the_current_generation() {
        let (mut backend, mut ends, rx) = parts();
        backend.send_reply(3, b"[0n".to_vec());
        backend.send_reply(2, b"stale".to_vec());
        assert_eq!(
            ends.commands.try_recv().unwrap(),
            BackendCommand::Reply(b"[0n".to_vec())
        );
        assert!(ends.commands.try_recv().is_err(), "旧代次的回复被丢掉");
        assert_eq!(rx.try_iter().count(), 0);
    }

    #[test]
    fn a_full_queue_drops_replies_silently_and_never_blocks() {
        let (mut backend, _ends, rx) = parts();
        for _ in 0..5 {
            backend.send_reply(3, b"x".to_vec());
        }
        assert_eq!(
            rx.try_iter().count(),
            0,
            "回复丢了不提示，也不影响 warned 状态"
        );
        // 用户输入这时满了照常提示一次。
        backend.send(SessionCommand::Write(b"k".to_vec()));
        assert_eq!(rx.try_iter().count(), 1);
    }

    #[test]
    fn a_dropped_resize_is_silent() {
        let (mut backend, _ends, rx) = parts();
        for _ in 0..5 {
            backend.send(SessionCommand::Resize(Size { rows: 5, cols: 5 }));
        }
        assert_eq!(rx.try_iter().count(), 0);
    }

    #[test]
    fn close_and_drop_both_signal_the_session() {
        let (mut backend, ends, _rx) = parts();
        assert!(!*ends.closing.borrow());
        backend.send(SessionCommand::Close);
        assert!(*ends.closing.borrow());

        let (backend, ends, _rx) = parts();
        drop(backend);
        assert!(*ends.closing.borrow());
    }
}
