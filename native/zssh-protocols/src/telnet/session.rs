//! Telnet 会话任务：把一个字节流（直连的 TCP、代理后的流、跳板机的 `direct-tcpip` 通道都行）
//! 接到 `SessionBackend` / `SessionEvent` / `FlowGate` 上。
//!
//! 一个任务、一个 `select!` 循环：读、写、命令、关闭、保活互不堵塞。
//! - 读之前先问流控门（`FlowGate::wait_ready`）：终端来不及渲染时不再读，TCP 窗口自然收紧，
//!   对端慢下来，不会在这里无界缓冲；
//! - 写（用户键入、协商应答、自动登录、NOP）先进一个有上限的发送缓冲，由循环里的一个分支慢慢写；
//!   缓冲超过上限就暂时不再取命令，命令队列又是有界的，满了界面那边丢弃并提示；
//! - 关闭信号在循环里最优先处理，卡住的写不能拖住它。

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zssh_core::model::{LocalEcho, TelnetOptions};
use zssh_core::ssh::FlowGate;
use zssh_session::{SessionCommand, SessionEvent};

use super::autologin::AutoLogin;
use super::codec::{Decoder, IAC, NOP, escape_iac};
use super::negotiate::{Negotiator, NegotiatorConfig};
use crate::backend::{BackendCommand, BackendEnds, COMMAND_QUEUE, EventOut, StreamBackend};
use crate::input::{InputFilter, telnet_enter_bytes};
use crate::text::{TextDecoder, TextEncoder};

/// 会话开头提示一次：Telnet 是明文协议。
pub const PLAINTEXT_NOTICE: &str = "\x1b[33m[提示] Telnet 不加密：用户名、密码和所有内容都以明文在网络上传输，请只在可信的网络里使用。\x1b[0m\r\n";
/// 纯 TCP 模式的提示。
pub const RAW_TCP_NOTICE: &str =
    "\x1b[33m[提示] 纯 TCP 连接：不做 Telnet 协商，也不加密，内容以明文传输。\x1b[0m\r\n";

/// 空闲多久发一个 NOP 保活。
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
/// 发送缓冲超过这个大小就先不收新命令。
const OUTBOX_SOFT_LIMIT: usize = 64 * 1024;
/// 每次读的缓冲大小。
const READ_BUFFER: usize = 8192;
/// 关闭时给对端发 FIN 的时间，写端卡住也不能拖住收尾。
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

pub struct TelnetParams {
    pub options: TelnetOptions,
    /// 自动登录用的用户名（可以为空）。
    pub username: String,
    /// 自动登录用的密码，取自主机绑定的密码凭据；没有就不答密码提示。
    pub password: Option<String>,
    pub cols: u16,
    pub rows: u16,
    /// 保活 NOP 的间隔，测试里缩短。
    pub keepalive: Duration,
}

impl TelnetParams {
    pub fn new(
        options: TelnetOptions,
        username: String,
        password: Option<String>,
        cols: u16,
        rows: u16,
    ) -> Self {
        Self {
            options,
            username,
            password,
            cols,
            rows,
            keepalive: KEEPALIVE_INTERVAL,
        }
    }
}

/// 启动一个 Telnet 会话任务，返回写端、事件接收端和这个会话的流控门。
///
/// 必须在 Tokio 运行时里调用（内部 `tokio::spawn`）。`keepalive` 是任务活着期间要一直持有的东西
/// （比如跳板机的 SSH 连接）：任务结束时才释放。
pub fn spawn_telnet<S>(
    stream: S,
    params: TelnetParams,
    generation: u64,
    keepalive: Box<dyn Send>,
) -> (
    StreamBackend,
    std::sync::mpsc::Receiver<SessionEvent>,
    Arc<FlowGate>,
)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (events, event_rx) = std::sync::mpsc::channel();
    let (backend, ends) = StreamBackend::new(events.clone(), generation, COMMAND_QUEUE);
    let gate = Arc::new(FlowGate::new());
    let out = EventOut {
        events,
        generation,
        gate: Arc::clone(&gate),
    };
    tokio::spawn(async move {
        run(stream, params, out, ends).await;
        drop(keepalive);
    });
    (backend, event_rx, gate)
}

struct Session {
    options: TelnetOptions,
    out: EventOut,
    decoder: Decoder,
    negotiator: Negotiator,
    filter: InputFilter,
    text_decoder: TextDecoder,
    text_encoder: TextEncoder,
    autologin: Option<AutoLogin>,
    outbox: Vec<u8>,
    enter: &'static [u8],
    last_write: Instant,
}

impl Session {
    fn raw(&self) -> bool {
        self.options.raw_tcp
    }

    /// 现在要不要本地回显：自动模式下对端说了 WILL ECHO 就关；纯 TCP 的自动模式不回显。
    fn local_echo(&self) -> bool {
        match self.options.local_echo {
            LocalEcho::On => true,
            LocalEcho::Off => false,
            LocalEcho::Auto => !self.raw() && !self.negotiator.remote_echo(),
        }
    }

    /// 转成远端编码并（Telnet 模式下）转义 IAC，放进发送缓冲。
    fn queue_text(&mut self, utf8: &[u8]) {
        let encoded = self.text_encoder.encode(utf8);
        if self.raw() {
            self.outbox.extend_from_slice(&encoded);
        } else {
            self.outbox.extend_from_slice(&escape_iac(&encoded));
        }
    }

    fn on_user_write(&mut self, bytes: &[u8]) -> bool {
        if let Some(autologin) = self.autologin.as_mut() {
            autologin.on_user_input();
        }
        let echo_on = self.local_echo();
        let (send, echo) = self.filter.prepare(bytes, self.enter, echo_on);
        self.queue_text(&send);
        self.out.data(echo)
    }

    /// 处理刚读到的一段字节。返回 `false` 表示界面那头没了。
    fn on_incoming(&mut self, data: &[u8]) -> bool {
        let visible = if self.raw() {
            data.to_vec()
        } else {
            let mut visible = Vec::with_capacity(data.len());
            let mut events = Vec::new();
            self.decoder.feed(data, &mut visible, &mut events);
            for event in &events {
                self.negotiator.handle(event, &mut self.outbox);
            }
            self.decoder.set_binary(self.negotiator.binary_in());
            visible
        };
        if visible.is_empty() {
            return true;
        }
        let utf8 = self.text_decoder.decode(&visible);
        let reply = match self.autologin.as_mut() {
            Some(autologin) if autologin.is_active() => {
                autologin.on_output(&String::from_utf8_lossy(&utf8), Instant::now())
            }
            _ => None,
        };
        if let Some(reply) = reply {
            self.queue_text(&reply);
        }
        self.out.data(utf8)
    }

    fn on_command(&mut self, command: BackendCommand) {
        match command {
            BackendCommand::Session(SessionCommand::Resize(size)) => {
                if !self.raw() {
                    let cols = u16::try_from(size.cols).unwrap_or(u16::MAX);
                    let rows = u16::try_from(size.rows).unwrap_or(u16::MAX);
                    self.negotiator.resize(cols, rows, &mut self.outbox);
                }
            }
            // 协议应答照原样发给远端：不是用户输入（不动自动登录、不转换回车、不本地回显）。
            BackendCommand::Reply(bytes) => self.queue_text(&bytes),
            // Write 在循环里单独处理（它可能让界面那头断掉），Close 走关闭信号，Break 对 Telnet 无意义。
            BackendCommand::Session(_) | BackendCommand::Break => {}
        }
    }
}

enum ReadOutcome {
    /// 流控门关了：会话在收尾。
    GateClosed,
    Read(io::Result<usize>),
}

/// 先等流控门放行再读：终端没消化完时，这里不读，TCP 窗口就收紧。
async fn read_when_ready<R: AsyncRead + Unpin>(
    reader: &mut R,
    gate: &FlowGate,
    buffer: &mut [u8],
) -> ReadOutcome {
    if !gate.wait_ready().await {
        return ReadOutcome::GateClosed;
    }
    ReadOutcome::Read(reader.read(buffer).await)
}

async fn run<S>(stream: S, params: TelnetParams, out: EventOut, ends: BackendEnds)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let TelnetParams {
        options,
        username,
        password,
        cols,
        rows,
        keepalive,
    } = params;
    let BackendEnds {
        mut commands,
        mut closing,
    } = ends;
    let (mut reader, mut writer) = tokio::io::split(stream);
    let raw = options.raw_tcp;
    let enter = telnet_enter_bytes(options.newline);

    out.notice(if raw {
        RAW_TCP_NOTICE
    } else {
        PLAINTEXT_NOTICE
    });

    let (negotiator, initial) = Negotiator::new(NegotiatorConfig {
        // 纯 TCP 模式不做任何 Telnet 处理：协商器存在，但不会有事件喂给它，也不发初始提议。
        naws: options.naws && !raw,
        terminal_type: options.terminal_type.clone(),
        cols,
        rows,
    });
    let autologin = options
        .auto_login
        .enabled
        .then(|| {
            AutoLogin::new(
                &username,
                password,
                &options.auto_login.username_patterns,
                &options.auto_login.password_patterns,
                enter,
                Instant::now(),
            )
        })
        .flatten();
    let mut session = Session {
        out,
        decoder: Decoder::default(),
        negotiator,
        filter: InputFilter::default(),
        text_decoder: TextDecoder::new(options.encoding),
        text_encoder: TextEncoder::new(options.encoding),
        autologin,
        outbox: if raw { Vec::new() } else { initial },
        enter,
        last_write: Instant::now(),
        options,
    };

    let mut buffer = vec![0u8; READ_BUFFER];
    let mut ticker = tokio::time::interval(keepalive);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await; // 第一个 tick 立刻到，跳过

    loop {
        tokio::select! {
            // 关闭最优先。发送端被丢掉（后端没了）也当作关闭，StreamBackend 的 Drop 会置位。
            _ = closing.changed() => break,
            written = writer.write(&session.outbox), if !session.outbox.is_empty() => {
                match written {
                    Ok(0) => {
                        session.out.error("写入远端失败：连接已关闭".to_owned());
                        break;
                    }
                    Ok(count) => {
                        session.outbox.drain(..count);
                        session.last_write = Instant::now();
                    }
                    Err(error) => {
                        session.out.error(format!("写入远端失败：{error}"));
                        break;
                    }
                }
            }
            outcome = read_when_ready(&mut reader, &session.out.gate, &mut buffer) => {
                match outcome {
                    ReadOutcome::GateClosed => break,
                    ReadOutcome::Read(Ok(0)) => {
                        session.out.exit();
                        break;
                    }
                    ReadOutcome::Read(Ok(count)) => {
                        if !session.on_incoming(&buffer[..count]) {
                            break;
                        }
                    }
                    ReadOutcome::Read(Err(error)) => {
                        session.out.error(format!("读取远端失败：{error}"));
                        break;
                    }
                }
            }
            command = commands.recv(), if session.outbox.len() < OUTBOX_SOFT_LIMIT => {
                match command {
                    Some(BackendCommand::Session(SessionCommand::Write(bytes))) => {
                        if !session.on_user_write(&bytes) {
                            break;
                        }
                    }
                    Some(other) => session.on_command(other),
                    None => break,
                }
            }
            _ = ticker.tick() => {
                // 空闲才发：有数据在走的连接不需要保活。纯 TCP 模式发 NOP 会污染数据，不发。
                if !session.raw()
                    && session.outbox.is_empty()
                    && session.last_write.elapsed() >= keepalive
                {
                    session.outbox.extend_from_slice(&[IAC, NOP]);
                }
            }
        }
    }

    // 收尾：尽量把 FIN 发出去，写端卡住也只等一小会儿；关门让可能在等的读任务立刻醒。
    let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, writer.shutdown()).await;
    session.out.gate.close();
}
