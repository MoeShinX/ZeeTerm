//! 端口转发的运行部分：本地转发（ssh -L）和远程转发（ssh -R）。
//!
//! 规则存在 [`crate::store::forwards`]；这里只管跑。每条规则一个任务，任务里用
//! `JoinSet` 收着它的每一条连接：任务被取消（用户点停止）时 `JoinSet` 跟着丢掉，
//! 所有连接一起断，不会留下孤儿。

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinSet;
use tokio::time::{timeout, Instant, Sleep};
use tracing::{debug, warn};

use crate::error::{reason_keys as rk, Reason, ZsshError};
use crate::ssh::connection::{Connection, ForwardedChannel};

/// 多久看一次 SSH 连接还在不在。连接断了转发就没用了，要让界面知道。
const LIVENESS_TICK: Duration = Duration::from_secs(2);
/// 远程转发连本机目标的超时。
const DEST_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 一条本地转发规则同时在转的连接数上限。超过的新连接直接关掉（见 [`run_local`]）。
/// 正常使用（浏览器开一堆标签、数据库连接池）远到不了这个数。
const MAX_LOCAL_CONNECTIONS: usize = 256;
/// `accept` 连续失败时第一次等多久；之后每失败一次翻倍。
const ACCEPT_BACKOFF_MIN: Duration = Duration::from_millis(10);
/// 退避的上限。
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);

/// `accept` 连续失败的退避：10ms 起翻倍、封顶 1s，成功一次就从头算。
///
/// 句柄用光（EMFILE）这类错误不会自己好，不退避的话循环会空转吃满一个核。
#[derive(Debug, Default)]
struct AcceptBackoff {
    failures: u32,
}

/// 一次失败之后该怎么办。
#[derive(Debug, PartialEq, Eq)]
struct AcceptRetry {
    /// 先等多久再接着 accept。
    delay: Duration,
    /// 值得记一条日志：这一串失败的第一次，以及第一次退避到顶。别每次都记，否则会刷屏。
    log: bool,
}

impl AcceptBackoff {
    fn failed(&mut self) -> AcceptRetry {
        self.failures = self.failures.saturating_add(1);
        // 位移到 63 位以上会溢出，封顶之后不用再算
        let shift = (self.failures - 1).min(16);
        let delay = (ACCEPT_BACKOFF_MIN * (1u32 << shift)).min(ACCEPT_BACKOFF_MAX);
        let previous = if self.failures == 1 {
            Duration::ZERO
        } else {
            (ACCEPT_BACKOFF_MIN * (1u32 << (shift - 1))).min(ACCEPT_BACKOFF_MAX)
        };
        AcceptRetry {
            delay,
            log: self.failures == 1 || (delay == ACCEPT_BACKOFF_MAX && previous < delay),
        }
    }

    fn succeeded(&mut self) {
        self.failures = 0;
    }
}

/// 把退避接进 `select!` 循环：失败后先"暂停" accept，等待期间循环里别的分支
/// （断线检查、连接收尾、取消）照常响应，不会被睡眠堵住。
struct AcceptPacer {
    backoff: AcceptBackoff,
    pause: Pin<Box<Sleep>>,
    paused: bool,
}

impl AcceptPacer {
    fn new() -> Self {
        Self {
            backoff: AcceptBackoff::default(),
            pause: Box::pin(tokio::time::sleep(Duration::ZERO)),
            paused: false,
        }
    }

    fn is_paused(&self) -> bool {
        self.paused
    }

    fn ok(&mut self) {
        self.backoff.succeeded();
    }

    /// 记一次失败并开始暂停；`what` 说是哪个监听。
    fn err(&mut self, what: &str, err: &std::io::Error) {
        let retry = self.backoff.failed();
        if retry.log {
            warn!(%err, retry_in = ?retry.delay, "{what} accept 失败，退避重试");
        } else {
            debug!(%err, retry_in = ?retry.delay, "{what} accept 仍然失败");
        }
        self.pause.as_mut().reset(Instant::now() + retry.delay);
        self.paused = true;
    }

    /// 暂停结束时返回。取消安全，放在 `select!` 的分支里用（配 `if pacer.is_paused()`）。
    async fn resumed(&mut self) {
        self.pause.as_mut().await;
        self.paused = false;
    }
}

/// 一条规则的计数，界面轮询着显示。
#[derive(Debug, Default)]
pub struct ForwardStats {
    active: AtomicUsize,
    total: AtomicU64,
    /// 发往目标方向的字节数（从监听那一侧收到的）。
    bytes_sent: AtomicU64,
    /// 从目标回来的字节数。
    bytes_received: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ForwardCounters {
    pub active: usize,
    pub total: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

impl ForwardStats {
    pub fn snapshot(&self) -> ForwardCounters {
        ForwardCounters {
            active: self.active.load(Ordering::Relaxed),
            total: self.total.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            bytes_received: self.bytes_received.load(Ordering::Relaxed),
        }
    }
}

/// 监听地址：`*` 表示所有网卡；IPv6 写法 `[::1]` 去掉方括号。
pub fn listen_host(bind_host: &str) -> &str {
    match bind_host {
        "*" | "" => "0.0.0.0",
        other => other
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .unwrap_or(other),
    }
}

/// 本地转发先在本机绑定端口：被占用时马上报错，不用等到 SSH 连上。
pub async fn bind_local(bind_host: &str, bind_port: u16) -> Result<TcpListener, ZsshError> {
    TcpListener::bind((listen_host(bind_host), bind_port))
        .await
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::AddrInUse {
                ZsshError::InvalidInput(
                    Reason::new(
                        rk::LOCAL_PORT_IN_USE,
                        format!("本机端口 {bind_port} 已经被别的程序占用"),
                    )
                    .with("port", bind_port),
                )
            } else if err.kind() == std::io::ErrorKind::PermissionDenied {
                ZsshError::InvalidInput(
                    Reason::new(
                        rk::LOCAL_PORT_DENIED,
                        format!("没有权限监听本机端口 {bind_port}"),
                    )
                    .with("port", bind_port),
                )
            } else {
                ZsshError::InvalidInput(
                    Reason::new(
                        rk::LOCAL_LISTEN_FAILED,
                        format!("监听 {bind_host}:{bind_port} 失败：{err}"),
                    )
                    .with("host", bind_host)
                    .with("port", bind_port)
                    .with("cause", &err),
                )
            }
        })
}

/// 两头对拷，结束后把字节数记进计数。
async fn pump<A, B>(mut near: A, mut far: B, stats: &ForwardStats)
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    stats.active.fetch_add(1, Ordering::Relaxed);
    stats.total.fetch_add(1, Ordering::Relaxed);
    match tokio::io::copy_bidirectional(&mut near, &mut far).await {
        Ok((sent, received)) => {
            stats.bytes_sent.fetch_add(sent, Ordering::Relaxed);
            stats.bytes_received.fetch_add(received, Ordering::Relaxed);
        }
        // 对端直接断开（RST）很常见，不算转发出错
        Err(err) => debug!(%err, "转发的一条连接异常结束"),
    }
    stats.active.fetch_sub(1, Ordering::Relaxed);
}

fn connection_lost() -> ZsshError {
    ZsshError::Shell(Reason::new(rk::CONNECTION_LOST, "SSH 连接断开了"))
}

/// 本地转发：接受本机连接，每条都经 SSH 开一个 direct-tcpip 通道连到目标。
///
/// 一直跑到 SSH 连接断开（返回原因）或者任务被取消。
pub async fn run_local(
    connection: Arc<Connection>,
    listener: TcpListener,
    dest_host: String,
    dest_port: u16,
    stats: Arc<ForwardStats>,
) -> ZsshError {
    run_local_limited(
        connection,
        listener,
        dest_host,
        dest_port,
        stats,
        MAX_LOCAL_CONNECTIONS,
    )
    .await
}

/// [`run_local`]，同时在转的连接数上限可调（测试用）。
async fn run_local_limited(
    connection: Arc<Connection>,
    listener: TcpListener,
    dest_host: String,
    dest_port: u16,
    stats: Arc<ForwardStats>,
    max_connections: usize,
) -> ZsshError {
    let mut conns = JoinSet::new();
    let mut rejected: u64 = 0;
    let mut tick = tokio::time::interval(LIVENESS_TICK);
    let mut pacer = AcceptPacer::new();
    loop {
        tokio::select! {
            accepted = listener.accept(), if !pacer.is_paused() => {
                let (socket, peer): (TcpStream, SocketAddr) = match accepted {
                    Ok(pair) => {
                        pacer.ok();
                        pair
                    }
                    Err(err) => {
                        pacer.err("本地转发", &err);
                        continue;
                    }
                };
                // 满了就直接关掉这条新连接。本机监听可以对着所有网卡，任何能连上的人（或者一个
                // 失控的程序）都能无限开连接，每条都会在服务器那边占一个通道和一个任务；
                // 上限之内的连接不受影响，有连接收尾后新的又能进来。
                if conns.len() >= max_connections {
                    rejected += 1;
                    // 被压垮时每秒能来成千上万条，只在第 1、2、4、8…次记日志，别把日志刷爆。
                    if rejected.is_power_of_two() {
                        warn!(
                            %peer,
                            limit = max_connections,
                            rejected,
                            "本地转发的并发连接数到上限，新连接被关闭"
                        );
                    }
                    continue;
                }
                let _ = socket.set_nodelay(true);
                let connection = Arc::clone(&connection);
                let dest_host = dest_host.clone();
                let stats = Arc::clone(&stats);
                conns.spawn(async move {
                    match connection.open_forward_channel(&dest_host, dest_port, peer).await {
                        Ok(channel) => pump(socket, channel, &stats).await,
                        Err(err) => debug!(%err, "本地转发开通道失败"),
                    }
                });
            }
            () = pacer.resumed(), if pacer.is_paused() => {}
            _ = tick.tick() => {
                if connection.is_closed() {
                    return connection_lost();
                }
            }
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
        }
    }
}

/// 远程转发：服务器那边连进来的通道（见 [`Connection::start_remote_forward`]），
/// 每条都在本机连目标，连上了才接受通道，连不上就拒绝（服务器那边的客户端立刻知道）。
pub async fn run_remote(
    connection: Arc<Connection>,
    mut incoming: UnboundedReceiver<ForwardedChannel>,
    dest_host: String,
    dest_port: u16,
    stats: Arc<ForwardStats>,
) -> ZsshError {
    let mut conns = JoinSet::new();
    let mut tick = tokio::time::interval(LIVENESS_TICK);
    loop {
        tokio::select! {
            next = incoming.recv() => {
                let Some(forwarded) = next else {
                    return connection_lost();
                };
                let dest_host = dest_host.clone();
                let stats = Arc::clone(&stats);
                conns.spawn(async move {
                    let target = timeout(
                        DEST_CONNECT_TIMEOUT,
                        TcpStream::connect((dest_host.as_str(), dest_port)),
                    )
                    .await;
                    match target {
                        Ok(Ok(socket)) => {
                            let _ = socket.set_nodelay(true);
                            forwarded.reply.accept().await;
                            pump(forwarded.channel.into_stream(), socket, &stats).await;
                        }
                        _ => {
                            debug!(from = %forwarded.originator, "远程转发连不上本机目标");
                            forwarded
                                .reply
                                .reject(russh::ChannelOpenFailure::ConnectFailed)
                                .await;
                        }
                    }
                });
            }
            _ = tick.tick() => {
                if connection.is_closed() {
                    return connection_lost();
                }
            }
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn listen_host_maps_wildcards_and_brackets() {
        assert_eq!(listen_host("*"), "0.0.0.0");
        assert_eq!(listen_host(""), "0.0.0.0");
        assert_eq!(listen_host("[::1]"), "::1");
        assert_eq!(listen_host("127.0.0.1"), "127.0.0.1");
    }

    #[test]
    fn accept_backoff_doubles_up_to_the_cap_and_resets_on_success() {
        let mut backoff = AcceptBackoff::default();
        let delays: Vec<u64> = (0..10)
            .map(|_| u64::try_from(backoff.failed().delay.as_millis()).unwrap())
            .collect();
        assert_eq!(delays, [10, 20, 40, 80, 160, 320, 640, 1000, 1000, 1000]);
        // 失败很多次也不溢出
        for _ in 0..1000 {
            assert_eq!(backoff.failed().delay, ACCEPT_BACKOFF_MAX);
        }
        backoff.succeeded();
        assert_eq!(backoff.failed().delay, ACCEPT_BACKOFF_MIN, "成功一次从头算");
    }

    #[test]
    fn accept_backoff_logs_the_first_failure_and_reaching_the_cap_only() {
        let mut backoff = AcceptBackoff::default();
        let logged: Vec<bool> = (0..12).map(|_| backoff.failed().log).collect();
        // 第 1 次失败、第 8 次（第一次到 1s）记日志，其余不记
        let expected: Vec<bool> = (1..=12).map(|n| n == 1 || n == 8).collect();
        assert_eq!(logged, expected);
        backoff.succeeded();
        assert!(backoff.failed().log, "恢复之后再坏，又是新的一串");
    }

    /// 模拟 `run_local` 的循环形状：accept 一直立刻报错时不能空转，退避等待期间还要能响应关闭。
    #[tokio::test]
    async fn a_failing_accept_is_paced_and_the_wait_does_not_block_shutdown() {
        let mut pacer = AcceptPacer::new();
        let (stop, mut stopped) = tokio::sync::oneshot::channel::<()>();
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = async {
                        counter.fetch_add(1, Ordering::SeqCst);
                        // 假 listener：每次都立刻失败（相当于 EMFILE）
                    }, if !pacer.is_paused() => {
                        pacer.err("测试", &std::io::Error::other("emfile"));
                    }
                    () = pacer.resumed(), if pacer.is_paused() => {}
                    _ = &mut stopped => return,
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let during = attempts.load(Ordering::SeqCst);
        // 不退避的话 300ms 里会转几十万圈；有退避大约 6 次，机器忙时容许偏差
        assert!((2..=12).contains(&during), "300ms 里 accept 了 {during} 次");
        // 此时多半正睡在一次（最长 1s 的）退避里：关闭要马上生效
        let began = std::time::Instant::now();
        stop.send(()).unwrap();
        timeout(Duration::from_millis(500), task)
            .await
            .expect("退避等待不能堵住关闭")
            .unwrap();
        assert!(began.elapsed() < Duration::from_millis(500));
    }

    /// 起一台进程内 SSH 服务器 + 回显目标 + 带上限的本地转发，返回本机监听端口。
    async fn spawn_limited_local_forward(max: usize) -> (u16, tokio::task::JoinHandle<ZsshError>) {
        use crate::ssh::auth::AuthConfig;
        use crate::ssh::connection::ConnectParams;
        use crate::ssh::fake_server;
        use crate::ssh::known_hosts::KnownHosts;

        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = echo.accept().await {
                tokio::spawn(async move {
                    let mut buffer = [0u8; 64];
                    while let Ok(read) = socket.read(&mut buffer).await {
                        if read == 0 || socket.write_all(&buffer[..read]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        let server = fake_server::spawn(vec![fake_server::ed25519_key()]).await;
        let dir = tempfile::tempdir().unwrap();
        let connection = Connection::connect(
            ConnectParams {
                host: server.addr.ip().to_string(),
                port: server.addr.port(),
                username: "tester".to_owned(),
                auth: AuthConfig::password(Some("pw-for-test".to_owned())),
                known_hosts: KnownHosts::new(dir.path().join("known_hosts")),
                legacy_algorithms: false,
            },
            Arc::new(crate::ssh::interact::NonInteractive),
        )
        .await
        .expect("连进程内服务器");
        let listener = bind_local("127.0.0.1", 0).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            // 服务器和临时目录要活到转发结束
            let _keep = (server, dir);
            run_local_limited(
                Arc::new(connection),
                listener,
                "127.0.0.1".to_owned(),
                echo_port,
                Arc::new(ForwardStats::default()),
                max,
            )
            .await
        });
        (port, task)
    }

    /// 连一次并回显一个字节；被转发关掉（读到 EOF / 出错）返回 `false`。
    async fn echo_once(port: u16) -> Option<(bool, TcpStream)> {
        let mut client = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        let _ = client.write_all(b"x").await;
        let mut byte = [0u8; 1];
        match timeout(Duration::from_secs(3), client.read(&mut byte)).await {
            Ok(Ok(1)) => Some((true, client)),
            _ => Some((false, client)),
        }
    }

    #[tokio::test]
    async fn local_forward_refuses_connections_over_the_limit_and_recovers() {
        let (port, task) = spawn_limited_local_forward(2).await;
        let (ok_a, client_a) = echo_once(port).await.expect("连上监听");
        let (ok_b, _client_b) = echo_once(port).await.expect("连上监听");
        assert!(ok_a && ok_b, "上限之内的连接要正常转发");

        let (ok_c, _client_c) = echo_once(port).await.expect("连上监听");
        assert!(!ok_c, "超过上限的连接要被直接关掉");

        // 腾出一个名额之后新连接又能进来（收尾要一点时间，重试几次）。
        drop(client_a);
        let mut recovered = false;
        for _ in 0..50 {
            if matches!(echo_once(port).await, Some((true, _))) {
                recovered = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(recovered, "有连接结束后，新连接应该又能转发");
        task.abort();
    }

    #[tokio::test]
    async fn port_in_use_is_reported_plainly() {
        let first = bind_local("127.0.0.1", 0).await.unwrap();
        let port = first.local_addr().unwrap().port();
        let err = bind_local("127.0.0.1", port).await.unwrap_err();
        assert!(err.to_string().contains("已经被别的程序占用"), "{err}");
    }

    #[tokio::test]
    async fn pump_copies_both_ways_and_counts() {
        let (near_app, near) = tokio::io::duplex(64);
        let (far, mut far_app) = tokio::io::duplex(64);
        let stats = Arc::new(ForwardStats::default());
        let task = {
            let stats = Arc::clone(&stats);
            tokio::spawn(async move { pump(near, far, &stats).await })
        };
        let (mut near_read, mut near_write) = tokio::io::split(near_app);
        near_write.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        far_app.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        far_app.write_all(b"pong!").await.unwrap();
        let mut back = [0u8; 5];
        near_read.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"pong!");
        assert_eq!(stats.snapshot().active, 1);

        near_write.shutdown().await.unwrap();
        far_app.shutdown().await.unwrap();
        drop(far_app);
        drop(near_read);
        drop(near_write);
        task.await.unwrap();
        let counters = stats.snapshot();
        assert_eq!((counters.active, counters.total), (0, 1));
        assert_eq!((counters.bytes_sent, counters.bytes_received), (4, 5));
    }
}
