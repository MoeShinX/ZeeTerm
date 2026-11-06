//! 真实 SSH 会话入口：主机库的「连接」按钮从这里开一条会话。
//!
//! 连接和认证走 `zssh_core`；本模块只负责：
//! - 把数据库里的主机/代理/凭据解析成拨号链；
//! - 在进程级 Tokio runtime 上连接、开 shell（界面线程只拿结果，不阻塞）；
//! - 把 `ShellWriter` / `ShellEventStream` 接到 `SessionBackend` 与 `SessionEvent`。
//!
//! 主机指纹和密码提问走 [`crate::ssh_interact`]，由窗口里的应用内对话框回答（V2-U29）。

use std::future::Future;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::runtime::Runtime;
use tokio::sync::{mpsc, watch};

use zssh_core::ai::pty::TerminalState;
use zssh_core::ai::terminal::TerminalRing;
use zssh_core::model::{Host, ProxyKind, ProxyMode};
use zssh_core::ssh::{
    BoxFuture, ChainHop, Connection, FlowGate, KeySource, KnownHosts, ProxyConfig, ShellEvent,
    ShellEventStream, ShellWriter,
};
use zssh_core::store::Database;
use zssh_session::{SessionBackend, SessionCommand, SessionEvent, map_core_event};
use zssh_terminal::Size;

use crate::ai_terminals::{TermCommand, terminals};
use crate::conn_pool::{Acquired, PRODUCTION_POLL, Pool, PoolConfig, PoolItem};
use crate::error_text::error_text;
use crate::settings::data_dir;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// 已认证连接的复用池：同一台主机（配置未变）的新窗口直接开 shell，不重新握手；
/// 同一份配置同时来的建连请求合并成一次握手；所有使用者放手之后按「连接空闲断开」设置计时关闭。
/// 细节见 [`crate::conn_pool`]。
static POOL: OnceLock<Pool<SshSessionFactory>> = OnceLock::new();

/// 命令队列容量。界面线程用 `try_send`，远端不读 stdin 时队列满了就丢弃并提示，
/// 不能为了等它而卡住界面（也不能无限堆内存）。
const COMMAND_QUEUE: usize = 256;

/// 在已有连接上开 shell 的超时：池里的连接半死时不能一直等到 keepalive 才发现。
const OPEN_SHELL_TIMEOUT: Duration = Duration::from_secs(20);

/// 关闭时给 EOF/close 帧的时间，写端卡住也不能拖住收尾。
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

fn pool() -> &'static Pool<SshSessionFactory> {
    POOL.get_or_init(|| {
        Pool::new(PoolConfig {
            idle_timeout: Duration::from_secs(u64::from(crate::app_prefs::POOL_IDLE_DEFAULT)),
            poll_interval: PRODUCTION_POLL,
        })
    })
}

/// 设置页的「连接空闲断开」：所有使用者放手之后，连接再空转多久关闭（0 = 立刻）。
/// 运行中改它，已经空闲的连接从改动那一刻起按新值重新计时。启动时和每次设置变动后调用。
pub fn set_pool_idle_secs(secs: u32) {
    pool().set_idle_timeout(Duration::from_secs(u64::from(secs)));
}

/// 连接池的键：目标主机 id + 整条拨号链的内容摘要。
///
/// 摘要覆盖每一跳的地址、用户名、老算法开关、代理（类型/地址/账号/密码）、认证方式
/// 和凭据内容（密码、私钥路径与文件状态、内嵌私钥、口令）。改了凭据、代理或跳板机
/// 之后摘要必然变，不会命中旧连接。摘要只在内存里比较，不记日志。
fn pool_key(target_id: i64, hops: &[ChainHop]) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hops.len().hash(&mut hasher);
    for hop in hops {
        hop.host.hash(&mut hasher);
        hop.port.hash(&mut hasher);
        hop.username.hash(&mut hasher);
        hop.legacy_algorithms.hash(&mut hasher);
        match &hop.proxy {
            None => 0u8.hash(&mut hasher),
            Some(proxy) => {
                1u8.hash(&mut hasher);
                proxy_label(&proxy.kind).hash(&mut hasher);
                proxy.host.hash(&mut hasher);
                proxy.port.hash(&mut hasher);
                proxy.username.hash(&mut hasher);
                proxy.password.hash(&mut hasher);
            }
        }
        format!("{:?}", hop.auth.method).hash(&mut hasher);
        hop.auth.password.hash(&mut hasher);
        match &hop.auth.key {
            None => 0u8.hash(&mut hasher),
            Some(KeySource::File { path, passphrase }) => {
                1u8.hash(&mut hasher);
                path.hash(&mut hasher);
                passphrase.hash(&mut hasher);
                // 同一路径换了文件内容（长度或修改时间变了）也算新凭据。
                if let Ok(meta) = std::fs::metadata(path) {
                    meta.len().hash(&mut hasher);
                    meta.modified().ok().hash(&mut hasher);
                }
            }
            Some(KeySource::Inline { pem, passphrase }) => {
                2u8.hash(&mut hasher);
                pem.hash(&mut hasher);
                passphrase.hash(&mut hasher);
            }
        }
    }
    format!("{target_id}|{:016x}", hasher.finish())
}

pub(crate) fn runtime() -> Result<&'static Runtime, String> {
    if let Some(runtime) = RUNTIME.get() {
        return Ok(runtime);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("zssh-ssh")
        .build()
        .map_err(|err| format!("{}{err}", t(BackendMsg::RuntimeCreateFailedPrefix)))?;
    let _ = RUNTIME.set(runtime);
    RUNTIME
        .get()
        .ok_or_else(|| t(BackendMsg::RuntimeInitFailed).to_owned())
}

use crate::i18n::{Msg, t};

/// SSH 后端自己的文案（运行时失败、往终端里写的提示）。
#[derive(Clone, Copy, Debug)]
enum BackendMsg {
    RuntimeCreateFailedPrefix,
    RuntimeInitFailed,
    InputDropped,
    ConnectTaskEnded,
    // ---- 连接失败的前缀和拨号链每一跳的标签（核心库错误正文走 `error_text`） ----
    PrepareFailed,
    OpenHostDbFailed,
    HopTarget,
    HopProxy,
    HopJump,
    OpenShellTimeout,
    OpenShellFailed,
    NotSshHostPrefix,
    NotSshHostSuffix,
    ConnectFailed,
    WriteRemoteFailed,
    CustomProxyMissingPrefix,
    CustomProxyMissingSuffix,
}

impl Msg for BackendMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::RuntimeCreateFailedPrefix => "创建 SSH 运行时失败：",
            Self::RuntimeInitFailed => "SSH 运行时初始化失败",
            Self::InputDropped => "\r\n\x1b[31m[远端暂时不接收输入，已丢弃部分按键]\x1b[0m\r\n",
            Self::ConnectTaskEnded => "连接任务意外结束",
            Self::PrepareFailed => "准备连接失败：",
            Self::OpenHostDbFailed => "打开主机库失败：",
            Self::HopTarget => "目标 ",
            Self::HopProxy => "代理 ",
            Self::HopJump => "跳板机 ",
            Self::OpenShellTimeout => "打开 shell 超时",
            Self::OpenShellFailed => "打开 shell 失败：",
            Self::NotSshHostPrefix => "这是一台 ",
            Self::NotSshHostSuffix => " 主机，没有 SSH 连接",
            Self::ConnectFailed => "连接失败：",
            Self::WriteRemoteFailed => "写入远端失败：",
            Self::CustomProxyMissingPrefix => "主机「",
            Self::CustomProxyMissingSuffix => "」选了指定代理，但没有选是哪个代理",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::RuntimeCreateFailedPrefix => "Failed to create the SSH runtime: ",
            Self::RuntimeInitFailed => "SSH runtime initialization failed",
            Self::InputDropped => {
                "\r\n\x1b[31m[The remote side is not accepting input right now; some keystrokes were dropped]\x1b[0m\r\n"
            }
            Self::ConnectTaskEnded => "The connection task ended unexpectedly",
            Self::PrepareFailed => "Could not prepare the connection: ",
            Self::OpenHostDbFailed => "Could not open the host database: ",
            Self::HopTarget => "Target ",
            Self::HopProxy => "Proxy ",
            Self::HopJump => "Jump host ",
            Self::OpenShellTimeout => "Timed out opening a shell",
            Self::OpenShellFailed => "Could not open a shell: ",
            Self::NotSshHostPrefix => "This is a ",
            Self::NotSshHostSuffix => " host; it has no SSH connection",
            Self::ConnectFailed => "Connection failed: ",
            Self::WriteRemoteFailed => "Could not write to the remote end: ",
            Self::CustomProxyMissingPrefix => "Host “",
            Self::CustomProxyMissingSuffix => "” uses a specific proxy, but none is selected",
        }
    }
}

/// 真实会话写端：命令转给 SSH 会话任务。
///
/// `send` 在界面线程上调用，绝不阻塞：命令队列有界，满了就丢并提示；
/// 关闭走独立的信号，不排在可能卡住的写入后面。
pub struct SshBackend {
    commands: mpsc::Sender<TermCommand>,
    closing: watch::Sender<bool>,
    /// 窗口大小是"最新状态"：只有最后一次有意义，所以不排进命令队列（队列满会被丢掉），
    /// 而是写进这个槽，会话任务有空时取最新的一个发给远端。
    resizes: watch::Sender<Option<Size>>,
    notices: Sender<SessionEvent>,
    generation: u64,
    /// 已经提示过"输入被丢弃"，队列恢复之前不重复刷屏。
    warned: bool,
    /// 这个 shell 在 AI 终端登记表里的编号（测试里直接造的后端没有）。
    terminal_id: Option<String>,
}

impl SshBackend {
    fn new(
        capacity: usize,
        notices: Sender<SessionEvent>,
        generation: u64,
    ) -> (Self, mpsc::Receiver<TermCommand>, SessionSignals) {
        let (commands, command_rx) = mpsc::channel(capacity);
        let (closing, closing_rx) = watch::channel(false);
        let (resizes, resizes_rx) = watch::channel(None);
        (
            Self {
                commands,
                closing,
                resizes,
                notices,
                generation,
                warned: false,
                terminal_id: None,
            },
            command_rx,
            SessionSignals {
                closing: closing_rx,
                resizes: resizes_rx,
            },
        )
    }

    /// 命令队列的发送端：AI 终端登记表拿它往这个 shell 里写包装脚本、发 Ctrl+C。
    fn command_sender(&self) -> mpsc::Sender<TermCommand> {
        self.commands.clone()
    }
}

impl SessionBackend for SshBackend {
    fn send(&mut self, command: SessionCommand) {
        match command {
            SessionCommand::Close => {
                let _ = self.closing.send(true);
                return;
            }
            // 窗口大小只认最新值，不进（可能已满的）命令队列，所以不会因为输入堵塞而丢。
            SessionCommand::Resize(size) => {
                self.resizes.send_replace(Some(size));
                return;
            }
            SessionCommand::Write(_) => {}
        }
        match self.commands.try_send(TermCommand::Session(command)) {
            Ok(()) => self.warned = false,
            Err(mpsc::error::TrySendError::Full(dropped)) => {
                // 输入丢了要让用户知道。
                if matches!(dropped, TermCommand::Session(SessionCommand::Write(_))) && !self.warned
                {
                    self.warned = true;
                    let _ = self.notices.send(SessionEvent::Notice {
                        generation: self.generation,
                        text: t(BackendMsg::InputDropped).to_owned(),
                    });
                }
            }
            // 会话任务已经结束；界面会通过 Exit/Error 事件知道原因。
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    fn terminal_id(&self) -> Option<String> {
        self.terminal_id.clone()
    }

    fn send_reply(&mut self, generation: u64, bytes: Vec<u8>) {
        if generation != self.generation {
            return;
        }
        // 满了就丢：协议回复不是用户输入，不弹"已丢弃按键"，也不能卡住界面线程。
        if self.commands.try_send(TermCommand::Reply(bytes)).is_err() {
            tracing::debug!("终端协议回复没有送出去：命令队列已满或会话已结束");
        }
    }
}

/// 一台已认证的连接：复制标签时在同一个 SSH 连接上再开一个 shell。
pub struct SshSessionFactory {
    connection: Arc<Connection>,
}

impl PoolItem for SshSessionFactory {
    fn is_closed(&self) -> bool {
        self.connection.is_closed()
    }

    /// 使用者 = 拿着工厂本身的（终端标签、SFTP 窗口、转发、状态采集……）加上只拿着
    /// `Arc<Connection>` 的（shell 会话任务、SFTP 通道、AI 工具的 exec / SFTP 通道、远端转发……）。
    /// 池里的一份工厂（1）和工厂自己握的一份连接（1）都扣掉。延迟测量、主机系统探测只持弱引用或
    /// 一次性持有，不算长期使用者。
    fn outside_holders(this: &Arc<Self>) -> usize {
        (Arc::strong_count(this) - 1) + (Arc::strong_count(&this.connection) - 1)
    }

    fn close(self: Arc<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            let _ = self.connection.disconnect().await;
        })
    }
}

/// 一次打开的结果：连接工厂、写端、事件接收端和会话自己的流控门。
///
/// `SessionPump` 必须用这个门：它的 ack 才会传回 SSH 读任务。
pub struct Opened {
    /// SSH 会话才有：复制标签、SFTP 复用同一条已认证连接。Telnet / 串口会话没有。
    pub factory: Option<Arc<SshSessionFactory>>,
    pub backend: Box<dyn SessionBackend>,
    pub events: Receiver<SessionEvent>,
    pub gate: Arc<FlowGate>,
    /// 终端右上角延迟徽标用的监视器：Telnet 会话自带；SSH 会话为 `None`，窗格从已认证连接上取
    /// （同一条连接上的窗格共用一份）。串口和经跳板机的 Telnet 没有。
    pub latency: Option<Arc<zssh_core::ssh::latency::LatencyMonitor>>,
}

pub type OpenResult = Result<Opened, String>;

/// 后台正在连接/开 shell：界面线程每个 tick 来取一次结果。丢掉它就取消任务。
pub struct PendingOpen {
    rx: Receiver<OpenResult>,
    abort: Option<tokio::task::AbortHandle>,
}

impl PendingOpen {
    /// 结果到了就返回；还没到返回 `None`。
    pub fn try_take(&self) -> Option<OpenResult> {
        match self.rx.try_recv() {
            Ok(result) => Some(result),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Some(Err(t(BackendMsg::ConnectTaskEnded).to_owned()))
            }
        }
    }
}

impl Drop for PendingOpen {
    fn drop(&mut self) {
        // 窗格在连接完成前被关掉：别让它继续弹认证框或占着连接。
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
    }
}

fn spawn_open<Fut>(future: Fut) -> PendingOpen
where
    Fut: Future<Output = OpenResult> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    match runtime() {
        Ok(runtime) => {
            let handle = runtime.spawn(async move {
                let _ = tx.send(future.await);
            });
            PendingOpen {
                rx,
                abort: Some(handle.abort_handle()),
            }
        }
        Err(err) => {
            let _ = tx.send(Err(err));
            PendingOpen { rx, abort: None }
        }
    }
}

/// 测试用：直接给出结果的"后台任务"，不需要 runtime。
#[cfg(test)]
pub(crate) fn ready(result: OpenResult) -> PendingOpen {
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = tx.send(result);
    PendingOpen { rx, abort: None }
}

/// 在已认证连接上后台开一个 shell。
pub fn begin_open(factory: Arc<SshSessionFactory>, size: Size, generation: u64) -> PendingOpen {
    spawn_open(async move {
        let (backend, events, gate) = factory.open_async(size, generation).await?;
        Ok(Opened {
            factory: Some(factory),
            backend: Box::new(backend),
            events,
            gate,
            latency: None,
        })
    })
}

/// 后台连接一个快速连接的目标（`user@host:port` + 密码，不在主机库里）并开一个终端会话。
pub fn begin_connect_quick(
    target: &zssh_core::model::QuickTarget,
    password: String,
    size: Size,
    generation: u64,
) -> PendingOpen {
    let target = target.clone();
    spawn_open(async move {
        // 读库是同步 IO：放到阻塞线程池（审查 M-13）。
        let (key, hops) = tokio::task::spawn_blocking(move || prepare_quick(&target, password))
            .await
            .map_err(|err| format!("{}{err}", t(BackendMsg::PrepareFailed)))??;
        let Acquired {
            item: factory,
            reused: _,
        } = acquire_factory(pool(), crate::ssh_interact::interactor(), key, hops, None).await?;
        let (backend, events, gate) = factory.open_async(size, generation).await?;
        Ok(Opened {
            factory: Some(factory),
            backend: Box::new(backend),
            events,
            gate,
            latency: None,
        })
    })
}

/// 快速连接只有一跳：目标本身，走默认代理（0.2.14：「跟随默认代理」包括快速连接）。
fn prepare_quick(
    target: &zssh_core::model::QuickTarget,
    password: String,
) -> Result<(String, Vec<ChainHop>), String> {
    let db = Database::open(&data_dir().join("zssh.db"))
        .map_err(|err| format!("{}{}", t(BackendMsg::OpenHostDbFailed), error_text(&err)))?;
    let proxy = match db
        .default_proxy_id()
        .map_err(|err| crate::error_text::error_text(&err))?
    {
        Some(id) => {
            let login = db
                .proxy_login(id)
                .map_err(|err| crate::error_text::error_text(&err))?;
            Some(ProxyConfig {
                kind: login.kind,
                host: login.host,
                port: login.port,
                username: login.username,
                password: login.password,
            })
        }
        None => None,
    };
    Ok(quick_hops(target, password, proxy, known_hosts()))
}

/// 快速连接的拨号链和连接池键（纯函数：不碰数据库，测试直接用）。
fn quick_hops(
    target: &zssh_core::model::QuickTarget,
    password: String,
    proxy: Option<ProxyConfig>,
    known_hosts: KnownHosts,
) -> (String, Vec<ChainHop>) {
    let hops = vec![ChainHop {
        label: format!("{}{}", t(BackendMsg::HopTarget), target.display()),
        host: target.host.clone(),
        port: target.port,
        username: target.username.clone(),
        proxy,
        // 空密码按「没带」处理：连接时照常弹提问。
        auth: zssh_core::ssh::AuthConfig::password((!password.is_empty()).then_some(password)),
        known_hosts,
        legacy_algorithms: false,
    }];
    // 快速连接没有主机 id：键里的 id 用 0，摘要仍包含地址、用户名、密码，不会撞上别的连接。
    (pool_key(0, &hops), hops)
}

/// 后台连接一台已保存的主机并开一个终端会话。按主机的协议分流：SSH 走原来的认证连接，
/// Telnet / 串口走 [`crate::protocol_backend`]。新标签、重连、复制标签都从这里进，所以三种协议自动通用。
pub fn begin_connect(host_id: i64, size: Size, generation: u64) -> PendingOpen {
    use crate::protocol_backend::{HostPlan, open_serial_host, open_telnet, plan_for_host};
    spawn_open(async move {
        match plan_for_host(host_id)? {
            HostPlan::Ssh => connect_async(host_id, size, generation).await,
            HostPlan::Telnet(plan) => open_telnet(*plan, size, generation).await,
            HostPlan::Serial(options) => open_serial_host(options, size, generation).await,
        }
    })
}

impl SshSessionFactory {
    /// 联网测试自己建连接时用（正式路径只走连接池）。
    #[cfg(test)]
    pub(crate) fn from_connection(connection: Connection) -> Arc<Self> {
        Arc::new(Self {
            connection: Arc::new(connection),
        })
    }

    /// 这条已经认证过的连接：终端之外的通道（SFTP 等）开在它上面，不重新握手。
    pub fn connection(&self) -> Arc<Connection> {
        Arc::clone(&self.connection)
    }

    /// 在现有连接上开一个新 shell，不重新握手认证。
    pub async fn open_async(
        &self,
        size: Size,
        generation: u64,
    ) -> Result<(SshBackend, Receiver<SessionEvent>, Arc<FlowGate>), String> {
        self.open_async_with(size, generation, None).await
    }

    /// 同 [`Self::open_async`]；`x11` 给了就请求 X11 转发（失败不致命，终端里有提示）。
    pub async fn open_async_with(
        &self,
        size: Size,
        generation: u64,
        x11: Option<crate::x11_glue::X11Attempt>,
    ) -> Result<(SshBackend, Receiver<SessionEvent>, Arc<FlowGate>), String> {
        // 延迟徽标：这条连接的测量任务在第一次开 shell 时起（重复调用只起一次，窗格不可见时任务暂停）。
        self.connection.start_latency_probe();
        // 核心转发路径往环形缓冲里旁路复制最近输出：AI 的 `read_terminal` 和结束标记解析读它。
        let ring = TerminalRing::shared();
        // X11 被拒后要再开一条普通通道，两次各最多等一轮回复。
        let open_timeout = if x11.is_some() {
            OPEN_SHELL_TIMEOUT * 3
        } else {
            OPEN_SHELL_TIMEOUT
        };
        let shell = tokio::time::timeout(
            open_timeout,
            crate::x11_glue::open_shell(
                &self.connection,
                size.cols as u16,
                size.rows as u16,
                Some(Arc::clone(&ring)),
                x11,
            ),
        )
        .await
        .map_err(|_| t(BackendMsg::OpenShellTimeout).to_owned())?
        .map_err(|err| format!("{}{}", t(BackendMsg::OpenShellFailed), error_text(&err)))?;
        let gate = shell.flow_handle();
        let (writer, stream) = shell.split();
        let (events, event_rx) = std::sync::mpsc::channel();
        let (mut backend, command_rx, signals) =
            SshBackend::new(COMMAND_QUEUE, events.clone(), generation);
        let state = Arc::new(TerminalState::new());
        let entry = terminals().register(
            ring,
            Arc::clone(&state),
            backend.command_sender(),
            Some(Arc::clone(&self.connection)),
        );
        backend.terminal_id = Some(entry.id.clone());
        tokio::spawn(run_session(
            Arc::clone(&self.connection),
            writer,
            stream,
            command_rx,
            signals,
            events,
            generation,
            Some(state),
            entry.id.clone(),
        ));
        Ok((backend, event_rx, gate))
    }
}

/// 把库里的一台主机解析成连接池键和拨号链。数据库句柄不是 `Send`，
/// 所有查询在这个同步函数里做完，不能带着它跨 `await`。
fn prepare(host_id: i64) -> Result<(String, Vec<ChainHop>), String> {
    let db = Database::open(&data_dir().join("zssh.db"))
        .map_err(|err| format!("{}{}", t(BackendMsg::OpenHostDbFailed), error_text(&err)))?;
    prepare_from_db(&db, host_id)
}

pub(crate) fn prepare_from_db(
    db: &Database,
    host_id: i64,
) -> Result<(String, Vec<ChainHop>), String> {
    // SFTP 之类直接要 SSH 连接的入口也走这里：Telnet / 串口主机没有 SSH 连接可拿。
    let protocol = db
        .host_protocol_get(host_id)
        .map_err(|err| crate::error_text::error_text(&err))?
        .protocol;
    if protocol != zssh_core::model::Protocol::Ssh {
        return Err(format!(
            "{}{}{}",
            t(BackendMsg::NotSshHostPrefix),
            crate::ops_text::protocol_label(protocol),
            t(BackendMsg::NotSshHostSuffix)
        ));
    }
    let hops = build_chain(db, host_id)?;
    Ok((pool_key(host_id, &hops), hops))
}

/// 拿这个键的已认证连接：池里有就复用；同一份配置有人正在握手就等那一次；否则握手并放进池。
/// `host_id` 是库里的主机（快速连接没有）：新握手出来的连接会在后台探一次远端系统，主机树里的图标据此换成
/// 发行版标志（见 [`crate::host_os`]）；复用池里的连接不再探。
///
/// `interactor` 单独传是为了测试能换成不弹对话框的；生产里是 [`crate::ssh_interact::interactor`]。
async fn acquire_factory(
    pool: &Pool<SshSessionFactory>,
    interactor: Arc<dyn zssh_core::ssh::Interactor>,
    key: String,
    hops: Vec<ChainHop>,
    host_id: Option<i64>,
) -> Result<Acquired<SshSessionFactory>, String> {
    pool.acquire(&key, move || async move {
        // 拨号任务被中止（标签关了、连接取消）时，正在等回答的指纹 / 密码对话框一并收起（见 `ssh_interact::scoped`）。
        let (interactor, cancel) = crate::ssh_interact::scoped(interactor);
        let connected = Connection::connect_chain(hops, interactor).await;
        cancel.disarm();
        let connection = connected
            .map_err(|err| format!("{}{}", t(BackendMsg::ConnectFailed), error_text(&err)))?;
        let factory = Arc::new(SshSessionFactory {
            connection: Arc::new(connection),
        });
        if let Some(host_id) = host_id {
            crate::host_os::spawn_probe(host_id, Arc::clone(&factory.connection));
        }
        Ok(factory)
    })
    .await
}

/// 连接一台已保存的主机并开一个终端会话（池里有同配置的活连接就复用）。
async fn connect_async(host_id: i64, size: Size, generation: u64) -> OpenResult {
    // 读库是同步 IO：放到阻塞线程池，别占住只有 2 个 worker 的 SSH runtime（审查 M-13）。
    let (key, hops) = tokio::task::spawn_blocking(move || prepare(host_id))
        .await
        .map_err(|err| format!("{}{err}", t(BackendMsg::PrepareFailed)))??;
    // 这台主机开了 X11 转发时，连上之后、开 shell 之前才准备本机 X 服务器（连不上就不白拉起）。
    let x11_wanted = crate::x11_glue::wanted(host_id);
    let Acquired {
        item: factory,
        reused,
    } = acquire_factory(
        pool(),
        crate::ssh_interact::interactor(),
        key.clone(),
        hops.clone(),
        Some(host_id),
    )
    .await?;
    let x11 = crate::x11_glue::attempt(x11_wanted).await;
    let (factory, (backend, events, gate)) =
        match factory.open_async_with(size, generation, x11).await {
            Ok(opened) => (factory, opened),
            // 复用的连接已经死了：摘掉旧条目，重新握手一次。刚握手出来的连接开 shell 失败是真失败。
            Err(_) if reused => {
                pool().forget(&key, &factory);
                let fresh = acquire_factory(
                    pool(),
                    crate::ssh_interact::interactor(),
                    key,
                    hops,
                    Some(host_id),
                )
                .await?
                .item;
                let x11 = crate::x11_glue::attempt(x11_wanted).await;
                let opened = fresh.open_async_with(size, generation, x11).await?;
                (fresh, opened)
            }
            Err(err) => return Err(err),
        };
    Ok(Opened {
        factory: Some(factory),
        backend: Box::new(backend),
        events,
        gate,
        latency: None,
    })
}

/// 拿一台主机的已认证连接，只建连接、不开 shell（SFTP 窗口用）。
///
/// 池里已经有同配置的活连接就直接复用；同时来的请求合并成一次握手；否则拨号链重新握手。
pub async fn factory_for_host(host_id: i64) -> Result<Arc<SshSessionFactory>, String> {
    // 同 `connect_async`：同步读库不能占 SSH runtime 的 worker（审查 M-13）。
    let (key, hops) = tokio::task::spawn_blocking(move || prepare(host_id))
        .await
        .map_err(|err| format!("{}{err}", t(BackendMsg::PrepareFailed)))??;
    acquire_factory(
        pool(),
        crate::ssh_interact::interactor(),
        key,
        hops,
        Some(host_id),
    )
    .await
    .map(|acquired| acquired.item)
}

/// 会话里对 shell 写半边的抽象；真实实现是 [`ShellWriter`]，测试里换成假的。
trait ShellIo: Send + Sync {
    fn write(&self, data: Vec<u8>) -> BoxFuture<'_, Result<(), String>>;
    fn resize(&self, cols: u16, rows: u16) -> BoxFuture<'_, Result<(), String>>;
    fn close(&self) -> BoxFuture<'_, ()>;
}

impl ShellIo for ShellWriter {
    fn write(&self, data: Vec<u8>) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            ShellWriter::write(self, &data)
                .await
                .map_err(|err| crate::error_text::error_text(&err))
        })
    }

    fn resize(&self, cols: u16, rows: u16) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            ShellWriter::resize(self, cols, rows)
                .await
                .map_err(|err| crate::error_text::error_text(&err))
        })
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let _ = ShellWriter::close(self).await;
        })
    }
}

/// 会话里对 shell 输出事件流的抽象。
trait ShellSource: Send {
    fn next_event(&mut self) -> BoxFuture<'_, Option<ShellEvent>>;
}

impl ShellSource for ShellEventStream {
    fn next_event(&mut self) -> BoxFuture<'_, Option<ShellEvent>> {
        Box::pin(self.next())
    }
}

/// 等正在进行的写入；没有就永远不返回（`select!` 里这一支就不会被选中）。
async fn finish_in_flight(
    in_flight: &mut Option<BoxFuture<'_, Result<(), String>>>,
) -> Result<(), String> {
    match in_flight {
        Some(write) => write.await,
        None => std::future::pending().await,
    }
}

/// 会话任务收的两路"最新状态"信号：关闭、窗口大小。都不走命令队列，所以不会被堵住的输入挡在后面或挤掉。
pub(crate) struct SessionSignals {
    closing: watch::Receiver<bool>,
    resizes: watch::Receiver<Option<Size>>,
}

/// 会话任务入口。持有 `_connection` 是为了会话活着时连接不被释放。
#[allow(clippy::too_many_arguments)]
async fn run_session(
    _connection: Arc<Connection>,
    writer: ShellWriter,
    mut stream: ShellEventStream,
    mut commands: mpsc::Receiver<TermCommand>,
    mut signals: SessionSignals,
    events: Sender<SessionEvent>,
    generation: u64,
    terminal: Option<Arc<TerminalState>>,
    terminal_id: String,
) {
    session_loop(
        &writer,
        &mut stream,
        &mut commands,
        &mut signals,
        &events,
        generation,
        terminal.as_deref(),
    )
    .await;
    // 会话结束：AI 终端登记表里摘掉它（停止按钮不能再对着一个没了的终端发 Ctrl+C）。
    terminals().remove(&terminal_id);
    // 丢掉事件流：它会关掉流控门并结束读写任务。
    drop(stream);
}

/// 会话任务的主循环（对应 0.2.14 `session_loop` 的 C16 修复）。
///
/// 写入（按键、改窗口大小）一次只进行一个，作为"在途"的 future 和关闭信号、读事件
/// 放在同一个 `select!` 里：远端不再补充 SSH 窗口时写入会一直等，Close 和输出都不能
/// 被它挡住。在途写入没完成之前不取命令队列，写入顺序不会乱，队列满了发送方丢弃并提示。
/// 写失败会发 `Error` 事件让界面显示原因，然后结束会话。
async fn session_loop<W: ShellIo, S: ShellSource>(
    writer: &W,
    stream: &mut S,
    commands: &mut mpsc::Receiver<TermCommand>,
    signals: &mut SessionSignals,
    events: &Sender<SessionEvent>,
    generation: u64,
    terminal: Option<&TerminalState>,
) {
    let mut in_flight: Option<BoxFuture<'_, Result<(), String>>> = None;
    loop {
        tokio::select! {
            // 关闭不排在队列和写入后面。发送端被丢掉（后端没了）同样当作关闭。
            _ = signals.closing.changed() => break,
            // 窗口大小和普通写入一样一次只进行一个（在途写入没完成前不取）；取的永远是最新的尺寸，
            // 堵塞期间改了多次也只发最后一次。发送端没了就是后端被丢掉，按关闭处理。
            changed = signals.resizes.changed(), if in_flight.is_none() => {
                if changed.is_err() {
                    break;
                }
                let latest = *signals.resizes.borrow_and_update();
                if let Some(size) = latest {
                    in_flight = Some(writer.resize(size.cols as u16, size.rows as u16));
                }
            }
            result = finish_in_flight(&mut in_flight) => {
                in_flight = None;
                if let Err(err) = result {
                    let _ = events.send(SessionEvent::Error {
                        generation,
                        message: format!("{}{err}", t(BackendMsg::WriteRemoteFailed)),
                    });
                    break;
                }
            }
            event = stream.next_event() => {
                match event {
                    Some(event) => {
                        let exit = matches!(event, ShellEvent::Exit(_) | ShellEvent::Closed);
                        // AI 终端执行要看的状态（备用屏幕 + 提示符尾部）：在原始数据上旁路喂，
                        // 不受显示过滤影响。
                        if let (Some(terminal), ShellEvent::Data(bytes)) = (terminal, &event) {
                            terminal.feed(bytes);
                        }
                        let _ = events.send(map_core_event(event, generation));
                        if exit {
                            break;
                        }
                    }
                    None => {
                        let _ = events.send(SessionEvent::Exit {
                            generation,
                            code: None,
                        });
                        break;
                    }
                }
            }
            command = commands.recv(), if in_flight.is_none() => {
                match command {
                    Some(TermCommand::Session(SessionCommand::Write(bytes)))
                    | Some(TermCommand::Reply(bytes)) => {
                        in_flight = Some(writer.write(bytes));
                    }
                    // 正常情况下窗口大小走 `signals.resizes`，这里只是兜底（别处直接往队列里塞的）。
                    Some(TermCommand::Session(SessionCommand::Resize(size))) => {
                        in_flight = Some(writer.resize(size.cols as u16, size.rows as u16));
                    }
                    Some(TermCommand::Session(SessionCommand::Close)) | None => break,
                    Some(TermCommand::AiWrite { payload, marker, command, slot }) => {
                        // 真正写之前最后核对一次（审计 C04）：排队期间点了停止 / 移出了目标，
                        // 这条旧命令就丢掉，调用方拿到原因，终端里什么都不出现。
                        let Some(claim) = slot.claim() else {
                            tracing::debug!("AI 命令在排队期间失效，没有写进终端");
                            continue;
                        };
                        // 先让显示过滤器记住这条命令，再写（回显紧跟着写入）。事件和输出数据同走一个
                        // 通道，这条一定排在回显之前。
                        let _ = events.send(SessionEvent::AiArm {
                            generation,
                            marker,
                            payload: String::from_utf8_lossy(&payload).into_owned(),
                            command,
                        });
                        let write = writer.write(payload);
                        in_flight = Some(Box::pin(async move {
                            let written = write.await;
                            claim.finish(written.clone());
                            written
                        }));
                    }
                    Some(TermCommand::AiTimedOut { marker }) => {
                        let _ = events.send(SessionEvent::AiRelease { generation, marker });
                    }
                }
            }
        }
    }
    // 丢掉没写完的在途写入，再发 EOF/close；写端卡住也只等一小会儿。
    drop(in_flight);
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, writer.close()).await;
}

/// 用户自己的 `~/.ssh/known_hosts`（存在才返回）。0.2.14 把它当只读第二来源，
/// 里面的 `@revoked` 记录要继续生效。
fn user_known_hosts(home: Option<PathBuf>) -> Option<PathBuf> {
    let path = home?.join(".ssh").join("known_hosts");
    path.exists().then_some(path)
}

/// 自己的 known_hosts（数据目录里，可写）+ 用户那份只读的。
///
/// 调试构建里设了 `ZSSH_DATA_DIR`（自动化的隔离环境）时不读用户那份，测试不碰真实的 `~/.ssh`。
pub(crate) fn known_hosts() -> KnownHosts {
    let mut known_hosts = KnownHosts::new(data_dir().join("known_hosts"));
    let isolated = cfg!(debug_assertions) && std::env::var_os("ZSSH_DATA_DIR").is_some();
    // Windows 用 USERPROFILE，macOS / Linux 用 HOME；两处都先看，缺哪个都不会崩。
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    if !isolated && let Some(user_file) = user_known_hosts(home) {
        known_hosts = known_hosts.with_read_only(user_file);
    }
    known_hosts
}

/// 拨号链：跳板机从最外层到目标；代理只挂在第一跳。
fn build_chain(db: &Database, host_id: i64) -> Result<Vec<ChainHop>, String> {
    let hosts = db
        .host_chain(host_id)
        .map_err(|err| crate::error_text::error_text(&err))?;
    let default_proxy_id = db
        .default_proxy_id()
        .map_err(|err| crate::error_text::error_text(&err))?;
    let known_hosts = known_hosts();
    let total = hosts.len();
    let mut hops = Vec::with_capacity(total);
    for (index, host) in hosts.iter().enumerate() {
        let login = db
            .host_login(host.id)
            .map_err(|err| crate::error_text::error_text(&err))?;
        let proxy = resolve_proxy(db, host, default_proxy_id)?;
        let label = match (&proxy, index + 1 == total) {
            (Some(proxy), _) if index == 0 => {
                format!("{}{}", t(BackendMsg::HopProxy), proxy_label(&proxy.kind))
            }
            (_, true) => format!("{}{}", t(BackendMsg::HopTarget), host.name),
            _ => format!("{}{}", t(BackendMsg::HopJump), host.name),
        };
        hops.push(ChainHop {
            label,
            host: login.host,
            port: login.port,
            username: login.username,
            proxy,
            auth: login.auth,
            known_hosts: known_hosts.clone(),
            legacy_algorithms: login.legacy_algorithms,
        });
    }
    Ok(hops)
}

pub(crate) fn resolve_proxy(
    db: &Database,
    host: &Host,
    default_proxy_id: Option<i64>,
) -> Result<Option<ProxyConfig>, String> {
    let proxy_id = match host.proxy_mode {
        ProxyMode::None => return Ok(None),
        ProxyMode::Default => match default_proxy_id {
            Some(id) => id,
            None => return Ok(None),
        },
        ProxyMode::Custom => host.proxy_id.ok_or_else(|| {
            format!(
                "{}{}{}",
                t(BackendMsg::CustomProxyMissingPrefix),
                host.name,
                t(BackendMsg::CustomProxyMissingSuffix)
            )
        })?,
    };
    let login = db
        .proxy_login(proxy_id)
        .map_err(|err| crate::error_text::error_text(&err))?;
    Ok(Some(ProxyConfig {
        kind: login.kind,
        host: login.host,
        port: login.port,
        username: login.username,
        password: login.password,
    }))
}

fn proxy_label(kind: &ProxyKind) -> &'static str {
    match kind {
        ProxyKind::Socks5 => "socks5",
        ProxyKind::Http => "http",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use zssh_core::ssh::AuthConfig;

    fn hop(host: &str, auth: AuthConfig, proxy: Option<ProxyConfig>) -> ChainHop {
        ChainHop {
            label: "t".to_owned(),
            host: host.to_owned(),
            port: 22,
            username: "u".to_owned(),
            proxy,
            auth,
            known_hosts: KnownHosts::new(std::env::temp_dir().join("zssh-test-unused-kh")),
            legacy_algorithms: false,
        }
    }

    fn proxy(port: u16, password: &str) -> ProxyConfig {
        ProxyConfig {
            kind: ProxyKind::Socks5,
            host: "p.example".to_owned(),
            port,
            username: Some("pu".to_owned()),
            password: Some(password.to_owned()),
        }
    }

    #[test]
    fn pool_key_follows_credentials_proxy_and_jump_chain() {
        let base = vec![hop("a", AuthConfig::password(Some("pw1".into())), None)];
        let same = vec![hop("a", AuthConfig::password(Some("pw1".into())), None)];
        assert_eq!(pool_key(1, &base), pool_key(1, &same));
        assert_ne!(pool_key(1, &base), pool_key(2, &base), "目标主机不同");

        let new_password = vec![hop("a", AuthConfig::password(Some("pw2".into())), None)];
        assert_ne!(pool_key(1, &base), pool_key(1, &new_password));

        let other_auth = vec![hop("a", AuthConfig::agent(), None)];
        assert_ne!(pool_key(1, &base), pool_key(1, &other_auth), "认证方式");

        let inline_a = vec![hop(
            "a",
            AuthConfig::key(KeySource::Inline {
                pem: "KEY-A".into(),
                passphrase: None,
            }),
            None,
        )];
        let inline_b = vec![hop(
            "a",
            AuthConfig::key(KeySource::Inline {
                pem: "KEY-B".into(),
                passphrase: None,
            }),
            None,
        )];
        assert_ne!(pool_key(1, &inline_a), pool_key(1, &inline_b), "私钥内容");

        let with_proxy = vec![hop(
            "a",
            AuthConfig::password(Some("pw1".into())),
            Some(proxy(1080, "x")),
        )];
        let proxy_port = vec![hop(
            "a",
            AuthConfig::password(Some("pw1".into())),
            Some(proxy(1081, "x")),
        )];
        let proxy_pass = vec![hop(
            "a",
            AuthConfig::password(Some("pw1".into())),
            Some(proxy(1080, "y")),
        )];
        assert_ne!(pool_key(1, &base), pool_key(1, &with_proxy), "加了代理");
        assert_ne!(
            pool_key(1, &with_proxy),
            pool_key(1, &proxy_port),
            "代理端口"
        );
        assert_ne!(
            pool_key(1, &with_proxy),
            pool_key(1, &proxy_pass),
            "代理密码"
        );

        let jumped = vec![
            hop("jump", AuthConfig::password(Some("jp".into())), None),
            hop("a", AuthConfig::password(Some("pw1".into())), None),
        ];
        let jump_changed = vec![
            hop("jump", AuthConfig::password(Some("jp2".into())), None),
            hop("a", AuthConfig::password(Some("pw1".into())), None),
        ];
        assert_ne!(pool_key(1, &base), pool_key(1, &jumped), "多了一跳");
        assert_ne!(
            pool_key(1, &jumped),
            pool_key(1, &jump_changed),
            "跳板机凭据"
        );
    }

    #[test]
    fn pool_key_does_not_expose_secrets() {
        let key = pool_key(
            7,
            &[hop(
                "a",
                AuthConfig::password(Some("super-secret-pw".into())),
                None,
            )],
        );
        assert!(!key.contains("super-secret-pw"));
        assert!(key.starts_with("7|"));
    }

    #[test]
    fn user_known_hosts_only_when_the_file_exists() {
        let dir = std::env::temp_dir().join(format!("zssh-kh-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(user_known_hosts(None), None);
        assert_eq!(user_known_hosts(Some(dir.clone())), None);
        std::fs::create_dir_all(dir.join(".ssh")).unwrap();
        assert_eq!(user_known_hosts(Some(dir.clone())), None);
        std::fs::write(dir.join(".ssh").join("known_hosts"), "").unwrap();
        assert_eq!(
            user_known_hosts(Some(dir.clone())),
            Some(dir.join(".ssh").join("known_hosts"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    #[derive(Clone, Copy)]
    enum WriteMode {
        /// 永远不完成：模拟远端不再补充 SSH 窗口。
        Block,
        Fail,
        Ok,
    }

    struct FakeIo {
        mode: WriteMode,
        written: Arc<Mutex<Vec<Vec<u8>>>>,
        /// 远端实际收到的窗口大小（列、行），按收到的顺序。
        resized: Arc<Mutex<Vec<(u16, u16)>>>,
        closed: Arc<AtomicBool>,
    }

    impl ShellIo for FakeIo {
        fn write(&self, data: Vec<u8>) -> BoxFuture<'_, Result<(), String>> {
            Box::pin(async move {
                match self.mode {
                    WriteMode::Block => std::future::pending().await,
                    WriteMode::Fail => Err("broken pipe".to_owned()),
                    WriteMode::Ok => {
                        self.written.lock().unwrap().push(data);
                        Ok(())
                    }
                }
            })
        }

        fn resize(&self, cols: u16, rows: u16) -> BoxFuture<'_, Result<(), String>> {
            Box::pin(async move {
                self.resized.lock().unwrap().push((cols, rows));
                Ok(())
            })
        }

        fn close(&self) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                self.closed.store(true, Ordering::SeqCst);
            })
        }
    }

    struct FakeSource(mpsc::UnboundedReceiver<ShellEvent>);

    impl ShellSource for FakeSource {
        fn next_event(&mut self) -> BoxFuture<'_, Option<ShellEvent>> {
            Box::pin(self.0.recv())
        }
    }

    struct Harness {
        backend: SshBackend,
        io: FakeIo,
        source: FakeSource,
        commands: mpsc::Receiver<TermCommand>,
        closing: SessionSignals,
        events_tx: Sender<SessionEvent>,
        events_rx: Receiver<SessionEvent>,
        shell_tx: mpsc::UnboundedSender<ShellEvent>,
    }

    fn harness(mode: WriteMode, capacity: usize) -> Harness {
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let (backend, commands, closing) = SshBackend::new(capacity, events_tx.clone(), 1);
        let (shell_tx, shell_rx) = mpsc::unbounded_channel();
        Harness {
            backend,
            io: FakeIo {
                mode,
                written: Arc::new(Mutex::new(Vec::new())),
                resized: Arc::new(Mutex::new(Vec::new())),
                closed: Arc::new(AtomicBool::new(false)),
            },
            source: FakeSource(shell_rx),
            commands,
            closing,
            events_tx,
            events_rx,
            shell_tx,
        }
    }

    #[test]
    fn close_and_output_are_not_blocked_by_a_stuck_write() {
        let Harness {
            mut backend,
            io,
            mut source,
            mut commands,
            mut closing,
            events_tx,
            events_rx,
            shell_tx,
        } = harness(WriteMode::Block, 8);
        backend.send(SessionCommand::Write(b"big paste".to_vec()));
        shell_tx
            .send(ShellEvent::Data(b"still readable".to_vec()))
            .unwrap();

        test_runtime().block_on(async {
            let run = session_loop(
                &io,
                &mut source,
                &mut commands,
                &mut closing,
                &events_tx,
                1,
                None,
            );
            tokio::pin!(run);
            // 在途写入永远不完成；输出照样能出来。
            let outcome = tokio::time::timeout(Duration::from_millis(200), &mut run).await;
            assert!(outcome.is_err(), "写入卡住时会话任务应仍在运行");
            assert_eq!(
                events_rx.try_recv(),
                Ok(SessionEvent::Data {
                    generation: 1,
                    bytes: b"still readable".to_vec(),
                })
            );

            // Close 走独立信号，能打断卡住的写入。
            backend.send(SessionCommand::Close);
            tokio::time::timeout(Duration::from_secs(2), &mut run)
                .await
                .expect("Close 必须能抢占卡住的写入");
        });
        assert!(io.closed.load(Ordering::SeqCst), "收尾要关 shell 写端");
    }

    #[test]
    fn write_failure_ends_the_session_and_reports_why() {
        let Harness {
            mut backend,
            io,
            mut source,
            mut commands,
            mut closing,
            events_tx,
            events_rx,
            shell_tx: _keep_source_open,
            ..
        } = harness(WriteMode::Fail, 8);
        backend.send(SessionCommand::Write(b"x".to_vec()));
        test_runtime().block_on(async {
            tokio::time::timeout(
                Duration::from_secs(2),
                session_loop(
                    &io,
                    &mut source,
                    &mut commands,
                    &mut closing,
                    &events_tx,
                    1,
                    None,
                ),
            )
            .await
            .expect("写失败后会话应结束");
        });
        match events_rx.try_recv() {
            Ok(SessionEvent::Error {
                generation: 1,
                message,
            }) => assert!(message.contains("broken pipe"), "{message}"),
            other => panic!("应该收到 Error 事件，实际 {other:?}"),
        }
    }

    #[test]
    fn writes_keep_order_and_remote_exit_ends_the_loop() {
        let Harness {
            mut backend,
            io,
            mut source,
            mut commands,
            mut closing,
            events_tx,
            events_rx,
            shell_tx,
        } = harness(WriteMode::Ok, 8);
        for chunk in ["a", "b", "c"] {
            backend.send(SessionCommand::Write(chunk.as_bytes().to_vec()));
        }
        test_runtime().block_on(async {
            let run = session_loop(
                &io,
                &mut source,
                &mut commands,
                &mut closing,
                &events_tx,
                1,
                None,
            );
            tokio::pin!(run);
            // 先让写入都发完，再发远端退出。
            let _ = tokio::time::timeout(Duration::from_millis(100), &mut run).await;
            shell_tx.send(ShellEvent::Exit(Some(3))).unwrap();
            tokio::time::timeout(Duration::from_secs(2), &mut run)
                .await
                .expect("远端退出后会话应结束");
        });
        assert_eq!(
            *io.written.lock().unwrap(),
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
        );
        assert_eq!(
            events_rx.try_recv(),
            Ok(SessionEvent::Exit {
                generation: 1,
                code: Some(3),
            })
        );
    }

    #[test]
    fn protocol_replies_are_written_in_order_with_input_and_old_generations_are_dropped() {
        let Harness {
            mut backend,
            io,
            mut source,
            mut commands,
            mut closing,
            events_tx,
            events_rx,
            shell_tx,
        } = harness(WriteMode::Ok, 8);
        backend.send(SessionCommand::Write(b"a".to_vec()));
        backend.send_reply(1, b"\x1b[0n".to_vec());
        // 重连前旧会话的迟到回复：代次不符，不能写进这条连接。
        backend.send_reply(0, b"stale".to_vec());
        backend.send(SessionCommand::Write(b"b".to_vec()));
        test_runtime().block_on(async {
            let run = session_loop(
                &io,
                &mut source,
                &mut commands,
                &mut closing,
                &events_tx,
                1,
                None,
            );
            tokio::pin!(run);
            let _ = tokio::time::timeout(Duration::from_millis(100), &mut run).await;
            shell_tx.send(ShellEvent::Exit(None)).unwrap();
            tokio::time::timeout(Duration::from_secs(2), &mut run)
                .await
                .expect("远端退出后会话应结束");
        });
        assert_eq!(
            *io.written.lock().unwrap(),
            vec![b"a".to_vec(), b"\x1b[0n".to_vec(), b"b".to_vec()]
        );
        drop(events_rx);
    }

    #[test]
    fn a_full_queue_drops_protocol_replies_without_the_dropped_keys_notice() {
        let mut h = harness(WriteMode::Ok, 2);
        for _ in 0..6 {
            h.backend.send_reply(1, b"\x1b[0n".to_vec());
        }
        assert!(h.events_rx.try_recv().is_err(), "回复丢了不提示");
        let mut queued = 0;
        while h.commands.try_recv().is_ok() {
            queued += 1;
        }
        assert_eq!(queued, 2);
    }

    /// 写入要等"放行"信号才完成的假 shell：模拟远端一时不补充 SSH 窗口（输入堵住），解除拥塞后继续。
    struct CongestedIo {
        released: watch::Receiver<bool>,
        written: Arc<Mutex<Vec<Vec<u8>>>>,
        resized: Arc<Mutex<Vec<(u16, u16)>>>,
    }

    impl ShellIo for CongestedIo {
        fn write(&self, data: Vec<u8>) -> BoxFuture<'_, Result<(), String>> {
            Box::pin(async move {
                let mut released = self.released.clone();
                let _ = released.wait_for(|open| *open).await;
                self.written.lock().unwrap().push(data);
                Ok(())
            })
        }

        fn resize(&self, cols: u16, rows: u16) -> BoxFuture<'_, Result<(), String>> {
            Box::pin(async move {
                self.resized.lock().unwrap().push((cols, rows));
                Ok(())
            })
        }

        fn close(&self) -> BoxFuture<'_, ()> {
            Box::pin(async {})
        }
    }

    /// R06：命令队列被输入堵满、远端又不收输入的时候只改一次窗口大小，之后窗口不再变——
    /// 拥塞解除后远端必须最终收到新尺寸。窗口大小是"最新状态"，不能和按键一起被静默丢掉。
    #[test]
    fn a_resize_sent_while_the_input_queue_is_full_still_reaches_the_remote() {
        let (events_tx, _events_rx) = std::sync::mpsc::channel();
        let (mut backend, mut commands, mut closing) = SshBackend::new(2, events_tx.clone(), 1);
        let (release, released) = watch::channel(false);
        let io = CongestedIo {
            released,
            written: Arc::new(Mutex::new(Vec::new())),
            resized: Arc::new(Mutex::new(Vec::new())),
        };
        let (_shell_tx, shell_rx) = mpsc::unbounded_channel();
        let mut source = FakeSource(shell_rx);

        backend.send(SessionCommand::Write(b"k".to_vec()));

        test_runtime().block_on(async {
            let run = session_loop(
                &io,
                &mut source,
                &mut commands,
                &mut closing,
                &events_tx,
                1,
                None,
            );
            tokio::pin!(run);
            // 第一个按键已经在途（远端堵着，写入永远不完成）。
            let outcome = tokio::time::timeout(Duration::from_millis(100), &mut run).await;
            assert!(outcome.is_err());

            // 之后输入把命令队列塞满（容量 2），多出来的按键被丢；随后只改一次窗口大小。
            for _ in 0..6 {
                backend.send(SessionCommand::Write(b"k".to_vec()));
            }
            backend.send(SessionCommand::Resize(Size {
                rows: 40,
                cols: 120,
            }));
            // 远端还堵着：窗口大小排不上，但会话仍在运行。
            let outcome = tokio::time::timeout(Duration::from_millis(100), &mut run).await;
            assert!(outcome.is_err());
            assert!(io.resized.lock().unwrap().is_empty());

            // 解除拥塞，什么都不再操作：远端最终收到新尺寸，而且只收到一次。
            release.send(true).unwrap();
            let outcome = tokio::time::timeout(Duration::from_millis(300), &mut run).await;
            assert!(outcome.is_err());
            assert_eq!(*io.resized.lock().unwrap(), vec![(120, 40)]);

            backend.send(SessionCommand::Close);
            tokio::time::timeout(Duration::from_secs(2), &mut run)
                .await
                .expect("Close 要能结束会话");
        });
    }

    /// 窗口连续拖动时只有最后一个尺寸有意义：堵塞期间的多次改动合并成一次，远端收到的是最新值。
    #[test]
    fn resizes_while_congested_coalesce_into_the_latest_size() {
        let (events_tx, _events_rx) = std::sync::mpsc::channel();
        let (mut backend, mut commands, mut closing) = SshBackend::new(2, events_tx.clone(), 1);
        let (release, released) = watch::channel(false);
        let io = CongestedIo {
            released,
            written: Arc::new(Mutex::new(Vec::new())),
            resized: Arc::new(Mutex::new(Vec::new())),
        };
        let (_shell_tx, shell_rx) = mpsc::unbounded_channel();
        let mut source = FakeSource(shell_rx);
        backend.send(SessionCommand::Write(b"k".to_vec()));

        test_runtime().block_on(async {
            let run = session_loop(
                &io,
                &mut source,
                &mut commands,
                &mut closing,
                &events_tx,
                1,
                None,
            );
            tokio::pin!(run);
            // 第一个按键在途、远端堵着；之后窗口被连续拖了几下。
            let _ = tokio::time::timeout(Duration::from_millis(100), &mut run).await;
            for cols in [90usize, 100, 110, 132] {
                backend.send(SessionCommand::Resize(Size { rows: 30, cols }));
            }
            let _ = tokio::time::timeout(Duration::from_millis(100), &mut run).await;
            assert!(io.resized.lock().unwrap().is_empty());
            release.send(true).unwrap();
            let _ = tokio::time::timeout(Duration::from_millis(300), &mut run).await;
            assert_eq!(*io.resized.lock().unwrap(), vec![(132, 30)]);
            backend.send(SessionCommand::Close);
            let _ = tokio::time::timeout(Duration::from_secs(2), &mut run).await;
        });
    }

    /// 窗口大小不占命令队列：队列满了的时候改尺寸既不丢，也不再占用输入的位置。
    #[test]
    fn a_resize_does_not_take_a_slot_in_the_input_queue() {
        let mut h = harness(WriteMode::Ok, 2);
        h.backend.send(SessionCommand::Write(b"a".to_vec()));
        h.backend
            .send(SessionCommand::Resize(Size { rows: 10, cols: 50 }));
        h.backend.send(SessionCommand::Write(b"b".to_vec()));
        let mut queued = Vec::new();
        while let Ok(command) = h.commands.try_recv() {
            queued.push(command);
        }
        assert!(
            matches!(
                queued.as_slice(),
                [
                    TermCommand::Session(SessionCommand::Write(first)),
                    TermCommand::Session(SessionCommand::Write(second)),
                ] if first == b"a" && second == b"b"
            ),
            "两个按键都进了队列：{queued:?}"
        );
    }

    #[test]
    fn dropping_the_backend_closes_the_session() {
        let Harness {
            backend,
            io,
            mut source,
            mut commands,
            mut closing,
            events_tx,
            ..
        } = harness(WriteMode::Ok, 8);
        drop(backend);
        test_runtime().block_on(async {
            tokio::time::timeout(
                Duration::from_secs(2),
                session_loop(
                    &io,
                    &mut source,
                    &mut commands,
                    &mut closing,
                    &events_tx,
                    1,
                    None,
                ),
            )
            .await
            .expect("后端被丢掉后会话应结束");
        });
        assert!(io.closed.load(Ordering::SeqCst));
    }

    #[test]
    fn full_command_queue_drops_input_and_warns_once() {
        let mut h = harness(WriteMode::Ok, 2);
        for _ in 0..6 {
            h.backend.send(SessionCommand::Write(b"k".to_vec()));
        }
        // 队列只有 2 格：其余被丢弃，提示只发一次。
        let mut notices = 0;
        while let Ok(event) = h.events_rx.try_recv() {
            assert!(matches!(event, SessionEvent::Notice { generation: 1, .. }));
            notices += 1;
        }
        assert_eq!(notices, 1);
        let mut queued = 0;
        while h.commands.try_recv().is_ok() {
            queued += 1;
        }
        assert_eq!(queued, 2);

        // 队列腾出来、发送成功过一次之后，再满了会重新提示。
        h.backend.send(SessionCommand::Write(b"k".to_vec()));
        h.backend.send(SessionCommand::Write(b"k".to_vec()));
        h.backend.send(SessionCommand::Write(b"k".to_vec()));
        assert!(matches!(
            h.events_rx.try_recv(),
            Ok(SessionEvent::Notice { .. })
        ));
    }

    /// 测试用的有效性：头 `valid_checks` 次 `check` 通过，之后失败（模拟排队期间用户点了停止）。
    struct CountedGuard {
        valid_checks: std::sync::atomic::AtomicUsize,
    }

    impl zssh_core::ssh::connection::ExecGuard for CountedGuard {
        fn check(&self) -> Result<(), String> {
            let left = self.valid_checks.load(Ordering::SeqCst);
            if left == 0 {
                return Err("用户点了停止，这次操作已取消".to_owned());
            }
            self.valid_checks.store(left - 1, Ordering::SeqCst);
            Ok(())
        }

        fn invalidated(&self) -> BoxFuture<'_, String> {
            Box::pin(std::future::pending())
        }

        fn stop_requested(&self) -> bool {
            false
        }
    }

    fn ai_write(slot: zssh_core::ai::bridge::terminal_write::AiWriteSlot) -> TermCommand {
        TermCommand::AiWrite {
            payload: b"echo hi\r".to_vec(),
            marker: "__ZSSH_m__".to_owned(),
            command: "echo hi".to_owned(),
            slot,
        }
    }

    #[test]
    fn an_ai_write_arms_the_display_filter_and_then_writes() {
        use zssh_core::ai::bridge::terminal_write::deliver_ai_write;
        let Harness {
            backend,
            io,
            mut source,
            mut commands,
            mut closing,
            events_tx,
            events_rx,
            shell_tx: _keep_source_open,
            ..
        } = harness(WriteMode::Ok, 8);
        let guard = Arc::new(CountedGuard {
            valid_checks: std::sync::atomic::AtomicUsize::new(usize::MAX),
        });
        let result = test_runtime().block_on(async {
            let sender = backend.command_sender();
            let deliver = deliver_ai_write(&sender, guard, ai_write);
            let run = session_loop(
                &io,
                &mut source,
                &mut commands,
                &mut closing,
                &events_tx,
                1,
                None,
            );
            tokio::pin!(run);
            tokio::select! {
                result = deliver => result,
                _ = &mut run => panic!("会话任务不该先结束"),
            }
        });
        assert_eq!(result, Ok(()), "写完了才返回");
        assert_eq!(*io.written.lock().unwrap(), vec![b"echo hi\r".to_vec()]);
        match events_rx.try_recv() {
            Ok(SessionEvent::AiArm {
                generation: 1,
                marker,
                command,
                payload,
            }) => {
                assert_eq!(marker, "__ZSSH_m__");
                assert_eq!(command, "echo hi");
                assert_eq!(payload, "echo hi\r");
            }
            other => panic!("写之前要先通知显示过滤器：{other:?}"),
        }
    }

    #[test]
    fn an_ai_write_that_went_stale_in_the_queue_is_neither_armed_nor_written() {
        use zssh_core::ai::bridge::terminal_write::deliver_ai_write;
        let Harness {
            backend,
            io,
            mut source,
            mut commands,
            mut closing,
            events_tx,
            events_rx,
            shell_tx: _keep_source_open,
            ..
        } = harness(WriteMode::Ok, 8);
        // 入队前的核对通过，会话任务认领时的核对失败：排队期间点了停止。
        let guard = Arc::new(CountedGuard {
            valid_checks: std::sync::atomic::AtomicUsize::new(1),
        });
        let result = test_runtime().block_on(async {
            let sender = backend.command_sender();
            let deliver = deliver_ai_write(&sender, guard, ai_write);
            let run = session_loop(
                &io,
                &mut source,
                &mut commands,
                &mut closing,
                &events_tx,
                1,
                None,
            );
            tokio::pin!(run);
            tokio::select! {
                result = deliver => result,
                _ = &mut run => panic!("会话任务不该先结束"),
            }
        });
        assert!(
            result.as_ref().is_err_and(|reason| reason.contains("停止")),
            "调用方要拿到原因：{result:?}"
        );
        assert!(io.written.lock().unwrap().is_empty(), "终端里什么都不能写");
        assert!(
            events_rx.try_recv().is_err(),
            "没写的命令也不能让显示过滤器记住它"
        );
    }

    #[test]
    fn a_timed_out_ai_command_releases_the_filter_and_raw_output_feeds_the_terminal_state() {
        let Harness {
            backend,
            io,
            mut source,
            mut commands,
            mut closing,
            events_tx,
            events_rx,
            shell_tx,
        } = harness(WriteMode::Ok, 8);
        let state = TerminalState::new();
        test_runtime().block_on(async {
            let sender = backend.command_sender();
            sender
                .send(TermCommand::AiTimedOut {
                    marker: "__ZSSH_t__".to_owned(),
                })
                .await
                .expect("入队");
            shell_tx
                .send(ShellEvent::Data(b"user@host:~$ ".to_vec()))
                .expect("发输出");
            let run = session_loop(
                &io,
                &mut source,
                &mut commands,
                &mut closing,
                &events_tx,
                1,
                Some(&state),
            );
            tokio::pin!(run);
            let _ = tokio::time::timeout(Duration::from_millis(150), &mut run).await;
        });
        let events: Vec<SessionEvent> = events_rx.try_iter().collect();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, SessionEvent::AiRelease { generation: 1, .. })),
            "要通知显示过滤器退出完整过滤：{events:?}"
        );
        assert!(
            state.is_idle(),
            "输出要旁路喂给终端执行状态（提示符 = 空闲）"
        );
    }

    #[test]
    fn close_signal_bypasses_a_full_queue() {
        let mut h = harness(WriteMode::Ok, 1);
        h.backend.send(SessionCommand::Write(b"k".to_vec()));
        h.backend.send(SessionCommand::Write(b"k".to_vec()));
        h.backend.send(SessionCommand::Close);
        assert!(*h.closing.closing.borrow_and_update());
    }

    fn quick_target(port: u16) -> zssh_core::model::QuickTarget {
        zssh_core::model::QuickTarget {
            username: "tester".to_owned(),
            host: "127.0.0.1".to_owned(),
            port,
        }
    }

    #[test]
    fn a_quick_target_is_a_single_hop_to_the_typed_address() {
        let kh = || KnownHosts::new(std::env::temp_dir().join("zssh-test-unused-kh"));
        let (key, hops) = quick_hops(&quick_target(2222), "pw".to_owned(), None, kh());
        assert_eq!(hops.len(), 1);
        assert_eq!(
            (
                hops[0].host.as_str(),
                hops[0].port,
                hops[0].username.as_str()
            ),
            ("127.0.0.1", 2222, "tester")
        );
        assert!(hops[0].proxy.is_none());
        // 同一个目标 + 同一个密码是同一条连接；换密码或端口就不是。
        let (same, _) = quick_hops(&quick_target(2222), "pw".to_owned(), None, kh());
        let (other_password, _) = quick_hops(&quick_target(2222), "pw2".to_owned(), None, kh());
        let (other_port, _) = quick_hops(&quick_target(2223), "pw".to_owned(), None, kh());
        assert_eq!(key, same);
        assert_ne!(key, other_password);
        assert_ne!(key, other_port);
        // 空密码不带密码（连接时才问），键也和带密码的不同。
        let (empty, empty_hops) = quick_hops(&quick_target(2222), String::new(), None, kh());
        assert_ne!(key, empty);
        assert!(empty_hops[0].auth.password.is_none());
    }

    #[test]
    fn a_quick_hop_connects_to_a_local_server_and_opens_a_shell() {
        let runtime = runtime().expect("runtime");
        let addr = runtime.block_on(crate::test_ssh_server::spawn());
        let known_hosts = std::env::temp_dir().join(format!(
            "zssh-g07b-quick-kh-{}-{}",
            std::process::id(),
            addr.port()
        ));
        let (_, hops) = quick_hops(
            &quick_target(addr.port()),
            "pw".to_owned(),
            None,
            KnownHosts::new(&known_hosts),
        );
        struct TrustOnce;
        impl zssh_core::ssh::Interactor for TrustOnce {
            fn confirm_host_key(
                &self,
                _info: zssh_core::ssh::HostKeyInfo,
            ) -> BoxFuture<'_, zssh_core::ssh::HostKeyDecision> {
                Box::pin(async { zssh_core::ssh::HostKeyDecision::TrustOnce })
            }
            fn prompt(
                &self,
                _request: zssh_core::ssh::PromptRequest,
            ) -> BoxFuture<'_, Option<zssh_core::ssh::PromptAnswers>> {
                Box::pin(async { None })
            }
        }
        let outcome = runtime.block_on(async {
            let connection = Connection::connect_chain(hops, Arc::new(TrustOnce))
                .await
                .map_err(|err| crate::error_text::error_text(&err))?;
            let factory = SshSessionFactory::from_connection(connection);
            let (backend, events, _gate) =
                factory.open_async(Size { rows: 24, cols: 80 }, 1).await?;
            // 写端要活到读完：它一掉，会话任务就当成界面关了窗格。
            let _backend = backend;
            let mut seen = String::new();
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while std::time::Instant::now() < deadline && !seen.contains("MINI_READY") {
                if let Ok(zssh_session::SessionEvent::Data { bytes: data, .. }) =
                    events.recv_timeout(Duration::from_millis(100))
                {
                    seen.push_str(&String::from_utf8_lossy(&data));
                }
            }
            Ok::<_, String>(seen)
        });
        let _ = std::fs::remove_file(&known_hosts);
        let seen = outcome.expect("快速连接的拨号链能连上本机测试服务器");
        assert!(seen.contains("MINI_READY"), "应该进了 shell：{seen}");
    }

    // ---- 连接池（R07 空闲回收、R08 并发建连合并）：真实 SSH 握手，对着进程内测试服务器 ----

    use crate::test_ssh_server::{spawn_counting, trust_once, trusting_hops};

    fn pool_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zssh-pool-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("临时目录");
        dir
    }

    fn test_pool(idle: Duration) -> Pool<SshSessionFactory> {
        Pool::new(PoolConfig {
            idle_timeout: idle,
            poll_interval: Duration::from_millis(50),
        })
    }

    async fn acquire_test(
        pool: &Pool<SshSessionFactory>,
        key: &str,
        hops: Vec<ChainHop>,
    ) -> Result<Acquired<SshSessionFactory>, String> {
        acquire_factory(pool, trust_once(), key.to_owned(), hops, None).await
    }

    /// 等到条件成立，最多 `limit`；返回最后一次的结果。
    async fn eventually(limit: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            if condition() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// R08：同一份配置同时来十个建连请求，服务器只看到一次握手；配置不同则各握各的。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ten_concurrent_connects_to_one_host_handshake_once() {
        let dir = pool_dir("r08");
        let (addr, probe) = spawn_counting().await;
        let pool = test_pool(Duration::from_secs(60));
        let hops = trusting_hops(addr, &dir);
        let key = pool_key(9001, &hops);

        let mut tasks = Vec::new();
        for _ in 0..10 {
            let pool = pool.clone();
            let key = key.clone();
            let hops = trusting_hops(addr, &dir);
            tasks.push(tokio::spawn(async move {
                acquire_test(&pool, &key, hops).await
            }));
        }
        let mut factories = Vec::new();
        for task in tasks {
            factories.push(task.await.expect("任务").expect("连上").item);
        }
        assert_eq!(probe.accepted(), 1, "十个请求只能握一次手");
        assert!(
            factories
                .iter()
                .all(|factory| Arc::ptr_eq(factory, &factories[0]))
        );

        // 在这条连接上照常开 shell：握手一次，之后按需开通道。
        let (_backend, _events, _gate) = factories[0]
            .open_async(Size { rows: 24, cols: 80 }, 1)
            .await
            .expect("开 shell");
        assert_eq!(probe.accepted(), 1);

        // 密码换了 = 配置变了 = 另一个键：不会误用旧连接。
        let mut changed = trusting_hops(addr, &dir);
        changed[0].auth = zssh_core::ssh::AuthConfig::password(Some("another".to_owned()));
        let changed_key = pool_key(9001, &changed);
        assert_ne!(changed_key, key);
        let other = acquire_test(&pool, &changed_key, changed)
            .await
            .expect("连上")
            .item;
        assert!(!Arc::ptr_eq(&other, &factories[0]));
        assert_eq!(probe.accepted(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R08：握手失败不会留下永远在等的占位——同键的下一次请求重新拨号并成功。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_failed_connect_does_not_block_the_next_attempt() {
        let dir = pool_dir("r08-fail");
        // 一个没人监听的端口：连接被拒绝。
        let dead = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("占一个端口");
            let addr = listener.local_addr().expect("端口");
            drop(listener);
            addr
        };
        let (addr, probe) = spawn_counting().await;
        let pool = test_pool(Duration::from_secs(60));

        let results = tokio::join!(
            acquire_test(&pool, "k", trusting_hops(dead, &dir)),
            acquire_test(&pool, "k", trusting_hops(dead, &dir)),
            acquire_test(&pool, "k", trusting_hops(dead, &dir)),
        );
        for result in [results.0, results.1, results.2] {
            assert!(result.is_err(), "拨不通应该报错");
        }
        assert_eq!(pool.len(), 0, "失败的占位不能留着");

        let retried = tokio::time::timeout(
            Duration::from_secs(20),
            acquire_test(&pool, "k", trusting_hops(addr, &dir)),
        )
        .await
        .expect("重试不能永久等待")
        .expect("重试连上");
        assert!(!retried.reused);
        assert_eq!(probe.accepted(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R07：所有使用者放手之后才开始计时，到点真的断开服务器那一端的连接；超时值不同，断开时刻不同。
    async fn idle_close_scenario(idle: Duration) -> Duration {
        let dir = pool_dir(&format!("r07-{}", idle.as_millis()));
        let (addr, probe) = spawn_counting().await;
        let pool = test_pool(idle);
        let hops = trusting_hops(addr, &dir);
        let key = pool_key(9002, &hops);
        let factory = acquire_test(&pool, &key, hops).await.expect("连上").item;
        let backup = Arc::clone(&factory);

        // 还有使用者：放着远超超时也不断。
        tokio::time::sleep(idle + Duration::from_millis(500)).await;
        assert_eq!(probe.live(), 1, "有使用者，不能断");
        assert!(!factory.connection.is_closed());

        // 一个放手，另一个还在：仍不断。
        drop(factory);
        tokio::time::sleep(idle + Duration::from_millis(300)).await;
        assert_eq!(probe.live(), 1, "还有使用者，不能断");

        // 最后一个放手：从现在计时。
        let released = std::time::Instant::now();
        drop(backup);
        assert!(
            eventually(idle + Duration::from_secs(5), || probe.live() == 0).await,
            "放手满 {idle:?} 之后连接应该断开"
        );
        let elapsed = released.elapsed();
        let _ = std::fs::remove_dir_all(&dir);
        elapsed
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_short_idle_timeout_closes_the_real_connection_soon_after_the_last_user_leaves() {
        let elapsed = idle_close_scenario(Duration::from_millis(400)).await;
        assert!(
            elapsed >= Duration::from_millis(350),
            "不能早于超时：{elapsed:?}"
        );
        assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_long_idle_timeout_keeps_the_real_connection_longer() {
        let elapsed = idle_close_scenario(Duration::from_millis(2500)).await;
        assert!(
            elapsed >= Duration::from_millis(2400),
            "不能早于超时：{elapsed:?}"
        );
        assert!(elapsed < Duration::from_secs(8), "{elapsed:?}");
    }

    /// R07：只握着 `Arc<Connection>`（AI 工具的 exec 通道、转发、状态采集这类不握工厂的使用者）
    /// 同样算使用者，超时再短也不能在它用着的时候断开。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn holding_only_the_connection_handle_keeps_the_connection_open() {
        let dir = pool_dir("r07-conn");
        let (addr, probe) = spawn_counting().await;
        let pool = test_pool(Duration::from_millis(200));
        let hops = trusting_hops(addr, &dir);
        let key = pool_key(9003, &hops);
        let factory = acquire_test(&pool, &key, hops).await.expect("连上").item;
        let connection = factory.connection();
        drop(factory);

        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(probe.live(), 1, "转发之类还拿着连接，不能断");
        assert!(!connection.is_closed());

        drop(connection);
        assert!(
            eventually(Duration::from_secs(5), || probe.live() == 0).await,
            "放手后应该断开"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R07：开着 SFTP 通道（远端编辑器就是这样，不握工厂）时，空闲回收不能把连接关掉；通道关掉之后才回收。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_open_sftp_channel_keeps_the_connection_from_being_reaped() {
        let dir = pool_dir("r07-sftp");
        let addr = crate::test_ssh_server::spawn_sftp().await;
        let pool = test_pool(Duration::from_millis(200));
        let hops = trusting_hops(addr, &dir);
        let key = pool_key(9004, &hops);
        let factory = acquire_test(&pool, &key, hops).await.expect("连上").item;
        let connection = factory.connection();
        let watcher = Arc::downgrade(&connection);
        let channel = zssh_core::sftp::SftpChannel::open(&connection)
            .await
            .expect("开 SFTP 通道");
        drop(connection);
        drop(factory);

        tokio::time::sleep(Duration::from_millis(1500)).await;
        channel
            .realpath(".")
            .await
            .expect("SFTP 还在用，连接不能被空闲回收关掉");
        assert!(watcher.upgrade().is_some());

        drop(channel);
        assert!(
            eventually(Duration::from_secs(5), || watcher.upgrade().is_none()).await,
            "通道关掉之后，连接应该被回收"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 设置页的「连接空闲断开」确实接到了全局连接池。
    #[test]
    fn the_idle_setting_reaches_the_global_pool() {
        let before = pool().idle_timeout();
        let prefs = |value: &'static str| {
            crate::app_prefs::AppPrefs::from_lookup(move |key| {
                (key == zssh_core::store::settings::POOL_IDLE_TIMEOUT_KEY).then(|| value.to_owned())
            })
        };
        prefs("37").apply_pool_idle();
        assert_eq!(pool().idle_timeout(), Duration::from_secs(37));
        prefs("0").apply_pool_idle();
        assert_eq!(pool().idle_timeout(), Duration::ZERO);
        pool().set_idle_timeout(before);
    }
}
