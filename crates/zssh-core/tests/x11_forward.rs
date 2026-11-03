//! X11 转发端到端（不需要真服务器、不需要真 X 服务器）。
//!
//! 进程内迷你 SSH 服务器（只监听本机回环）可以回应 / 拒绝 / 不回应 `x11-req`，并在连接上
//! 主动开 X11 通道，扮演"远端的 X 程序"；进程内假 X 服务器记录握手包里的 cookie 并回显数据。
//! 验证：cookie 被换成真值、数据双向转发、两个 shell 各自路由、未知 cookie 被拒、
//! 被拒后换普通通道、关闭后清理、各种失败的分类和终端提示。

// 测试 / 示例代码：失败就让它 panic，不包装成错误返回（生产代码的 unwrap / expect 由 clippy 拦）。
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::keys::{Algorithm, PrivateKey};
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{sleep, timeout, Instant};
use zssh_core::ssh::{Connection, ShellEvent, ShellSession, DEFAULT_BUFFER_LIMIT};
use zssh_core::x11::cookie::{decode_hex, setup_packet_len, SetupLen, MIT_MAGIC_COOKIE};
use zssh_core::x11::{FailureSink, X11Failure, X11Outcome, X11Request, X11Route, XEndpoint};

use common::{TempKnownHosts, TestServer};

const WAIT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------- 假 SSH 服务器

#[derive(Clone, Copy, PartialEq, Eq)]
enum X11Reply {
    Accept,
    Refuse,
    Silent,
}

struct ServerState {
    x11_reply: Mutex<X11Reply>,
    /// 收到的 x11-req：（通道, 假 cookie 十六进制, 屏幕号, 是否单连接）。
    x11_requests: Mutex<Vec<(ChannelId, String, u32, bool)>>,
    session_channels: AtomicUsize,
    handle: Mutex<Option<server::Handle>>,
}

struct FakeSsh {
    state: Arc<ServerState>,
}

impl server::Handler for FakeSsh {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Reject {
            proceed_with_methods: Some(MethodSet::from(&[MethodKind::Password][..])),
            partial_success: false,
        })
    }

    async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn auth_succeeded(&mut self, session: &mut Session) -> Result<(), Self::Error> {
        *self.state.handle.lock().unwrap() = Some(session.handle());
        Ok(())
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.state.session_channels.fetch_add(1, Ordering::SeqCst);
        reply.accept().await;
        Ok(())
    }

    async fn x11_request(
        &mut self,
        channel: ChannelId,
        single_connection: bool,
        x11_auth_protocol: &str,
        x11_auth_cookie: &str,
        x11_screen_number: u32,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        assert_eq!(x11_auth_protocol, MIT_MAGIC_COOKIE);
        self.state.x11_requests.lock().unwrap().push((
            channel,
            x11_auth_cookie.to_owned(),
            x11_screen_number,
            single_connection,
        ));
        match *self.state.x11_reply.lock().unwrap() {
            X11Reply::Accept => session.channel_success(channel),
            X11Reply::Refuse => session.channel_failure(channel),
            X11Reply::Silent => Ok(()),
        }
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        session.data(channel, b"SHELL_READY\r\n".to_vec())
    }
}

struct Harness {
    state: Arc<ServerState>,
    connection: Connection,
    _known_hosts: TempKnownHosts,
}

async fn start(reply: X11Reply) -> Harness {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定");
    let addr = listener.local_addr().expect("端口");
    let state = Arc::new(ServerState {
        x11_reply: Mutex::new(reply),
        x11_requests: Mutex::new(Vec::new()),
        session_channels: AtomicUsize::new(0),
        handle: Mutex::new(None),
    });
    let config = Arc::new(server::Config {
        inactivity_timeout: None,
        auth_rejection_time: Duration::from_millis(1),
        keys: vec![PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("主机密钥")],
        ..Default::default()
    });
    let handler_state = Arc::clone(&state);
    tokio::spawn(async move {
        let Ok((socket, _)) = listener.accept().await else {
            return;
        };
        let handler = FakeSsh {
            state: handler_state,
        };
        if let Ok(running) = server::run_stream(config, socket, handler).await {
            let _ = running.await;
        }
    });
    let known_hosts = TempKnownHosts::new("x11");
    let connection = TestServer::fake("127.0.0.1", addr.port(), "tester", "pw")
        .connect(&known_hosts)
        .await
        .expect("连接假服务器");
    Harness {
        state,
        connection,
        _known_hosts: known_hosts,
    }
}

impl Harness {
    /// 等服务器收到第 `n` 个 x11-req，返回它的假 cookie（十六进制）。
    async fn fake_cookie(&self, n: usize) -> String {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some((_, cookie, _, _)) = self.state.x11_requests.lock().unwrap().get(n) {
                return cookie.clone();
            }
            assert!(Instant::now() < deadline, "服务器没收到第 {n} 个 x11-req");
            sleep(Duration::from_millis(20)).await;
        }
    }

    /// 扮演远端 X 程序：开一条 X11 通道，按 `chunks` 分片发握手包，返回通道的流。
    async fn x_client(&self, fake_cookie_hex: &str, order: u8) -> russh::ChannelStream<Msg> {
        let handle = self
            .state
            .handle
            .lock()
            .unwrap()
            .clone()
            .expect("认证成功后应当有句柄");
        let channel = handle
            .channel_open_x11("203.0.113.9", 40000)
            .await
            .expect("服务器开 X11 通道");
        let mut stream = channel.into_stream();
        let cookie = decode_hex(fake_cookie_hex).expect("hex");
        let packet = setup_packet(order, &cookie);
        // 分三片发，覆盖客户端的分片缓冲。
        stream.write_all(&packet[..5]).await.unwrap();
        stream.flush().await.unwrap();
        sleep(Duration::from_millis(30)).await;
        stream.write_all(&packet[5..20]).await.unwrap();
        stream.flush().await.unwrap();
        sleep(Duration::from_millis(30)).await;
        stream.write_all(&packet[20..]).await.unwrap();
        stream.flush().await.unwrap();
        stream
    }
}

/// 一个 MIT-MAGIC-COOKIE-1 握手包（版本 11.0，无其他字段）。
fn setup_packet(order: u8, cookie: &[u8]) -> Vec<u8> {
    let name = MIT_MAGIC_COOKIE.as_bytes();
    let pad = |n: usize| (n + 3) & !3;
    let mut packet = vec![order, 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let (name_len, data_len) = (name.len() as u16, cookie.len() as u16);
    let (n, d) = if order == b'l' {
        (name_len.to_le_bytes(), data_len.to_le_bytes())
    } else {
        (name_len.to_be_bytes(), data_len.to_be_bytes())
    };
    packet[6..8].copy_from_slice(&n);
    packet[8..10].copy_from_slice(&d);
    packet.extend_from_slice(name);
    packet.resize(12 + pad(name.len()), 0);
    packet.extend_from_slice(cookie);
    packet.resize(12 + pad(name.len()) + pad(cookie.len()), 0);
    packet
}

// ---------------------------------------------------------------- 假 X 服务器

#[derive(Clone, Copy, PartialEq, Eq)]
enum XMode {
    Accept,
    Reject,
}

struct FakeX {
    port: u16,
    /// 每个连接握手包里的 cookie。
    cookies: Arc<Mutex<Vec<Vec<u8>>>>,
    /// 已经结束的连接数（对端关了或读到 EOF）。
    finished: Arc<AtomicUsize>,
}

async fn fake_x(mode: XMode) -> FakeX {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定");
    let port = listener.local_addr().unwrap().port();
    let cookies = Arc::new(Mutex::new(Vec::new()));
    let finished = Arc::new(AtomicUsize::new(0));
    let (c, f) = (Arc::clone(&cookies), Arc::clone(&finished));
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let (c, f) = (Arc::clone(&c), Arc::clone(&f));
            tokio::spawn(async move {
                let mut buffered = Vec::new();
                let mut chunk = [0u8; 4096];
                let len = loop {
                    match setup_packet_len(&buffered) {
                        SetupLen::Total(len) if buffered.len() >= len => break len,
                        SetupLen::Invalid => {
                            f.fetch_add(1, Ordering::SeqCst);
                            return;
                        }
                        _ => {}
                    }
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => {
                            f.fetch_add(1, Ordering::SeqCst);
                            return;
                        }
                        Ok(n) => buffered.extend_from_slice(&chunk[..n]),
                    }
                };
                let (_, data) = zssh_core::x11::cookie::setup_auth(&buffered[..len]).unwrap();
                c.lock().unwrap().push(data.to_vec());
                let reply: &[u8] = match mode {
                    XMode::Accept => &[1, 0, 11, 0, 0, 0, 0, 0],
                    XMode::Reject => &[0, 4, 11, 0, 0, 0, 1, 0, b'n', b'o', b'p', b'e'],
                };
                if socket.write_all(reply).await.is_err() {
                    f.fetch_add(1, Ordering::SeqCst);
                    return;
                }
                // 握手包后面如果跟着数据，也回显。
                if buffered.len() > len && socket.write_all(&buffered[len..]).await.is_err() {
                    f.fetch_add(1, Ordering::SeqCst);
                    return;
                }
                loop {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if socket.write_all(&chunk[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                f.fetch_add(1, Ordering::SeqCst);
            });
        }
    });
    FakeX {
        port,
        cookies,
        finished,
    }
}

impl FakeX {
    fn route(&self, real: Option<Vec<u8>>) -> X11Route {
        X11Route {
            endpoint: XEndpoint {
                host: "127.0.0.1".to_owned(),
                port: self.port,
                screen: 0,
            },
            real_cookie: real,
        }
    }

    fn seen(&self) -> Vec<Vec<u8>> {
        self.cookies.lock().unwrap().clone()
    }
}

// ---------------------------------------------------------------- 辅助

async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(Instant::now() < deadline, "等待超时：{what}");
        sleep(Duration::from_millis(20)).await;
    }
}

/// 一直收终端输出，直到出现 `needle`。
async fn read_until(shell: &mut ShellSession, needle: &str) -> String {
    let mut text = String::new();
    let deadline = Instant::now() + WAIT;
    while !text.contains(needle) {
        let left = deadline.saturating_duration_since(Instant::now());
        match timeout(left, shell.next_event()).await {
            Ok(Some(ShellEvent::Data(data))) => text.push_str(&String::from_utf8_lossy(&data)),
            Ok(Some(ShellEvent::Exit(_))) => {}
            _ => panic!("没等到 {needle:?}，收到：{text:?}"),
        }
    }
    text
}

async fn open_x11(
    harness: &Harness,
    route: X11Route,
    observer: Option<FailureSink>,
) -> (ShellSession, X11Outcome) {
    harness
        .connection
        .open_shell_x11(
            80,
            24,
            DEFAULT_BUFFER_LIMIT,
            None,
            Ok(X11Request {
                route,
                on_failure: observer,
            }),
        )
        .await
        .expect("开 shell")
}

async fn read_exact(stream: &mut russh::ChannelStream<Msg>, n: usize) -> Vec<u8> {
    let mut out = vec![0u8; n];
    timeout(WAIT, stream.read_exact(&mut out))
        .await
        .expect("读超时")
        .expect("读失败");
    out
}

/// 通道在限期内被关闭（读到 EOF 或出错）。
async fn closes_soon(stream: &mut russh::ChannelStream<Msg>) -> bool {
    let mut buf = [0u8; 64];
    matches!(
        timeout(WAIT, stream.read(&mut buf)).await,
        Ok(Ok(0)) | Ok(Err(_))
    )
}

// ---------------------------------------------------------------- 测试

#[tokio::test]
async fn the_fake_cookie_is_replaced_and_data_flows_both_ways() {
    for order in *b"lB" {
        let harness = start(X11Reply::Accept).await;
        let x = fake_x(XMode::Accept).await;
        let real = vec![0xAB; 16];
        let (mut shell, outcome) = open_x11(&harness, x.route(Some(real.clone())), None).await;
        assert_eq!(
            outcome,
            X11Outcome::Active {
                display: format!("127.0.0.1:{}", x.port)
            }
        );
        read_until(&mut shell, "SHELL_READY").await;

        let fake = harness.fake_cookie(0).await;
        assert_eq!(fake.len(), 32, "16 字节 cookie 的十六进制");
        assert_ne!(decode_hex(&fake).unwrap(), real, "给服务器的必须是假值");
        {
            let requests = harness.state.x11_requests.lock().unwrap();
            assert_eq!(requests[0].2, 0, "屏幕号");
            assert!(!requests[0].3, "不是 single_connection");
        }

        let mut stream = harness.x_client(&fake, order).await;
        assert_eq!(read_exact(&mut stream, 8).await, [1, 0, 11, 0, 0, 0, 0, 0]);
        stream.write_all(b"hello-x-server").await.unwrap();
        assert_eq!(read_exact(&mut stream, 14).await, b"hello-x-server");

        // 本机 X 服务器收到的是真 cookie，假的没有漏过去。
        assert_eq!(x.seen(), vec![real.clone()]);
        assert_eq!(harness.connection.x11_hub().registration_count(), 1);
        drop(shell);
    }
}

#[tokio::test]
async fn two_shells_on_one_connection_are_routed_by_their_own_cookie() {
    let harness = start(X11Reply::Accept).await;
    let (x1, x2) = (fake_x(XMode::Accept).await, fake_x(XMode::Accept).await);
    let (real1, real2) = (vec![0x11; 16], vec![0x22; 16]);
    let (shell1, _) = open_x11(&harness, x1.route(Some(real1.clone())), None).await;
    let (shell2, _) = open_x11(&harness, x2.route(Some(real2.clone())), None).await;
    let (fake1, fake2) = (harness.fake_cookie(0).await, harness.fake_cookie(1).await);
    assert_ne!(fake1, fake2, "每个终端一个假 cookie");
    assert_eq!(harness.connection.x11_hub().registration_count(), 2);

    // 先 2 后 1，再各开第二条，顺序打乱也不能串。
    let mut a = harness.x_client(&fake2, b'l').await;
    let mut b = harness.x_client(&fake1, b'l').await;
    let mut c = harness.x_client(&fake2, b'B').await;
    for stream in [&mut a, &mut b, &mut c] {
        assert_eq!(read_exact(stream, 8).await, [1, 0, 11, 0, 0, 0, 0, 0]);
    }
    assert_eq!(x1.seen(), vec![real1]);
    assert_eq!(x2.seen(), vec![real2.clone(), real2]);

    // 关掉终端 1：它的登记没了，终端 2 不受影响。
    drop(shell1);
    eventually("终端 1 的登记撤销", || {
        harness.connection.x11_hub().registration_count() == 1
    })
    .await;
    a.write_all(b"still-alive").await.unwrap();
    assert_eq!(read_exact(&mut a, 11).await, b"still-alive");
    drop(shell2);
}

#[tokio::test]
async fn an_unknown_cookie_is_closed_and_never_reaches_the_local_server() {
    let harness = start(X11Reply::Accept).await;
    let x = fake_x(XMode::Accept).await;
    let seen: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_seen = Arc::clone(&seen);
    let observer: FailureSink = Arc::new(move |failure: &X11Failure| {
        sink_seen.lock().unwrap().push(failure.code());
    });
    let (mut shell, _) = open_x11(&harness, x.route(Some(vec![1; 16])), Some(observer)).await;
    read_until(&mut shell, "SHELL_READY").await;
    let _ = harness.fake_cookie(0).await;

    let mut stream = harness.x_client(&"ee".repeat(16), b'l').await;
    assert!(closes_soon(&mut stream).await, "不认识的 cookie 必须被关掉");
    assert!(x.seen().is_empty(), "本机 X 服务器一次连接都不该收到");
    eventually("界面收到 unknown_cookie", || {
        seen.lock().unwrap().contains(&"unknown_cookie")
    })
    .await;
    let text = read_until(&mut shell, "认证信息不对").await;
    assert!(text.contains("[ZeeTerm] X11"), "{text:?}");
}

#[tokio::test]
async fn a_non_mit_or_garbage_setup_is_closed() {
    let harness = start(X11Reply::Accept).await;
    let x = fake_x(XMode::Accept).await;
    let (_shell, _) = open_x11(&harness, x.route(None), None).await;
    let _ = harness.fake_cookie(0).await;
    let handle = harness.state.handle.lock().unwrap().clone().unwrap();
    let channel = handle.channel_open_x11("203.0.113.9", 1).await.unwrap();
    let mut stream = channel.into_stream();
    stream.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
    assert!(closes_soon(&mut stream).await);
    assert!(x.seen().is_empty());
}

#[tokio::test]
async fn a_server_that_refuses_x11_gets_a_plain_shell_on_a_fresh_channel() {
    let harness = start(X11Reply::Refuse).await;
    let x = fake_x(XMode::Accept).await;
    let seen: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_seen = Arc::clone(&seen);
    let observer: FailureSink = Arc::new(move |failure: &X11Failure| {
        sink_seen.lock().unwrap().push(failure.code());
    });
    let (mut shell, outcome) = open_x11(&harness, x.route(None), Some(observer)).await;
    assert_eq!(outcome, X11Outcome::Skipped(X11Failure::ServerRefused));

    let text = read_until(&mut shell, "SHELL_READY").await;
    assert!(text.contains("X11Forwarding"), "提示要说明原因：{text:?}");
    assert!(text.contains("xauth"), "{text:?}");
    assert!(
        text.find("X11Forwarding") < text.find("SHELL_READY"),
        "提示在服务器输出之前"
    );
    assert_eq!(
        harness.state.session_channels.load(Ordering::SeqCst),
        2,
        "被拒的通道丢掉，重开了一条"
    );
    assert_eq!(
        harness.state.x11_requests.lock().unwrap().len(),
        1,
        "第二条通道不再请求 X11"
    );
    assert_eq!(harness.connection.x11_hub().registration_count(), 0);
    assert_eq!(*seen.lock().unwrap(), vec!["server_refused"]);

    // 这是一个能正常交互的终端。
    shell.write(b"x").await.expect("能写");
}

#[tokio::test]
async fn a_server_that_never_answers_the_x11_request_also_falls_back() {
    let harness = start(X11Reply::Silent).await;
    let x = fake_x(XMode::Accept).await;
    let (mut shell, outcome) = harness
        .connection
        .open_shell_x11_with_timeout(
            80,
            24,
            DEFAULT_BUFFER_LIMIT,
            None,
            Ok(X11Request {
                route: x.route(None),
                on_failure: None,
            }),
            Duration::from_millis(300),
        )
        .await
        .expect("开 shell");
    match outcome {
        X11Outcome::Skipped(X11Failure::ServerNoReply { .. }) => {}
        other => panic!("预期 ServerNoReply：{other:?}"),
    }
    read_until(&mut shell, "SHELL_READY").await;
    assert_eq!(harness.state.session_channels.load(Ordering::SeqCst), 2);
    assert_eq!(harness.connection.x11_hub().registration_count(), 0);
}

#[tokio::test]
async fn a_failure_known_before_connecting_opens_a_plain_shell_with_the_reason_on_top() {
    let harness = start(X11Reply::Accept).await;
    let failure = X11Failure::NoLocalXServer {
        display: "localhost:6000".into(),
        installable: true,
    };
    let (mut shell, outcome) = harness
        .connection
        .open_shell_x11(80, 24, DEFAULT_BUFFER_LIMIT, None, Err(failure.clone()))
        .await
        .expect("开 shell");
    assert_eq!(outcome, X11Outcome::Skipped(failure));
    let text = read_until(&mut shell, "SHELL_READY").await;
    assert!(text.contains("一键安装 VcXsrv"), "{text:?}");
    assert!(
        harness.state.x11_requests.lock().unwrap().is_empty(),
        "根本没请求 X11"
    );
    assert_eq!(harness.state.session_channels.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn closing_the_shell_cleans_up_registration_and_running_relays() {
    let harness = start(X11Reply::Accept).await;
    let x = fake_x(XMode::Accept).await;
    let (shell, _) = open_x11(&harness, x.route(Some(vec![9; 16])), None).await;
    let fake = harness.fake_cookie(0).await;
    let mut open_stream = harness.x_client(&fake, b'l').await;
    assert_eq!(
        read_exact(&mut open_stream, 8).await,
        [1, 0, 11, 0, 0, 0, 0, 0]
    );
    assert_eq!(x.finished.load(Ordering::SeqCst), 0);

    drop(shell);

    eventually("登记撤销", || {
        harness.connection.x11_hub().registration_count() == 0
    })
    .await;
    // 已经在转发的通道被收掉：远端这头读到关闭，本机 X 服务器那头也结束了。
    assert!(closes_soon(&mut open_stream).await);
    eventually("本机 X 连接结束", || {
        x.finished.load(Ordering::SeqCst) == 1
    })
    .await;
    // 之后再来的 X11 通道（没有任何终端登记了）在打开时就被拒绝。
    let handle = harness.state.handle.lock().unwrap().clone().unwrap();
    assert!(handle.channel_open_x11("203.0.113.9", 1).await.is_err());
    assert_eq!(x.seen().len(), 1, "关闭之后没有新的本机连接");
}

#[tokio::test]
async fn a_missing_local_x_server_is_reported_once_in_the_terminal_and_to_the_ui() {
    let harness = start(X11Reply::Accept).await;
    // 占一个端口再放掉，保证没人监听。
    let closed_port = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    let route = X11Route {
        endpoint: XEndpoint {
            host: "127.0.0.1".into(),
            port: closed_port,
            screen: 0,
        },
        real_cookie: Some(vec![5; 16]),
    };
    let reported: Arc<Mutex<VecDeque<X11Failure>>> = Arc::new(Mutex::new(VecDeque::new()));
    let sink = Arc::clone(&reported);
    let observer: FailureSink = Arc::new(move |failure: &X11Failure| {
        sink.lock().unwrap().push_back(failure.clone());
    });
    let (mut shell, _) = open_x11(&harness, route, Some(observer)).await;
    read_until(&mut shell, "SHELL_READY").await;
    let fake = harness.fake_cookie(0).await;

    for _ in 0..3 {
        let mut stream = harness.x_client(&fake, b'l').await;
        assert!(closes_soon(&mut stream).await);
    }
    let text = read_until(&mut shell, "本机没有可用的 X 服务器").await;
    assert!(
        text.contains(&format!("127.0.0.1:{closed_port}")),
        "{text:?}"
    );
    let reported = reported.lock().unwrap();
    assert_eq!(reported.len(), 1, "同一个原因只提示一次：{reported:?}");
    assert!(matches!(reported[0], X11Failure::NoLocalXServer { .. }));
}

#[tokio::test]
async fn without_a_real_cookie_the_setup_packet_is_passed_through_unchanged() {
    let harness = start(X11Reply::Accept).await;
    let x = fake_x(XMode::Accept).await;
    let (_shell, _) = open_x11(&harness, x.route(None), None).await;
    let fake = harness.fake_cookie(0).await;
    let mut stream = harness.x_client(&fake, b'l').await;
    assert_eq!(read_exact(&mut stream, 8).await, [1, 0, 11, 0, 0, 0, 0, 0]);
    assert_eq!(x.seen(), vec![decode_hex(&fake).unwrap()]);
}

#[tokio::test]
async fn a_local_x_server_that_rejects_the_connection_is_classified_with_advice() {
    let harness = start(X11Reply::Accept).await;
    let x = fake_x(XMode::Reject).await;
    let reported: Arc<Mutex<Vec<X11Failure>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&reported);
    let observer: FailureSink = Arc::new(move |failure: &X11Failure| {
        sink.lock().unwrap().push(failure.clone());
    });
    // 没有真 cookie = 用户自己的 X 服务器。
    let (mut shell, _) = open_x11(&harness, x.route(None), Some(observer)).await;
    let fake = harness.fake_cookie(0).await;
    let mut stream = harness.x_client(&fake, b'l').await;
    // 拒绝的回复照样转给远端程序，让它自己报错。
    assert_eq!(read_exact(&mut stream, 8).await[0], 0);

    let text = read_until(&mut shell, "关闭访问控制").await;
    assert!(text.contains("nope"), "原因要带上：{text:?}");
    let reported = reported.lock().unwrap();
    match &reported[0] {
        X11Failure::LocalXServerRejected {
            reason,
            passthrough,
            ..
        } => {
            assert_eq!(reason, "nope");
            assert!(*passthrough);
        }
        other => panic!("预期 LocalXServerRejected：{other:?}"),
    }
}

#[tokio::test]
async fn too_many_terminals_on_one_connection_fall_back_to_plain_shells() {
    let harness = start(X11Reply::Accept).await;
    let x = fake_x(XMode::Accept).await;
    let mut keep = Vec::new();
    for _ in 0..zssh_core::x11::MAX_REGISTRATIONS {
        keep.push(open_x11(&harness, x.route(None), None).await);
    }
    let (_shell, outcome) = open_x11(&harness, x.route(None), None).await;
    match outcome {
        X11Outcome::Skipped(X11Failure::NotRegistered { .. }) => {}
        other => panic!("预期 NotRegistered：{other:?}"),
    }
    assert_eq!(
        harness.connection.x11_hub().registration_count(),
        zssh_core::x11::MAX_REGISTRATIONS
    );
}

#[tokio::test]
async fn x11_channels_are_refused_when_no_terminal_uses_x11() {
    let harness = start(X11Reply::Accept).await;
    // 先开一个不要 X11 的普通终端，保证握手完成、句柄可用。
    let mut plain = harness
        .connection
        .open_shell(80, 24)
        .await
        .expect("普通终端");
    read_until(&mut plain, "SHELL_READY").await;
    let handle = harness.state.handle.lock().unwrap().clone().unwrap();
    let opened = handle.channel_open_x11("203.0.113.9", 1).await;
    assert!(
        opened.is_err(),
        "没有任何 X11 登记时，服务器开 X11 通道要被拒绝"
    );
}
