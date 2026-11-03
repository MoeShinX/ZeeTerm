//! 终端延迟徽标的端到端（不需要真服务器）。
//!
//! 进程内迷你 SSH 服务器只监听本机回环；前面再放一个"加延迟"的 TCP 中继，这样往返时间是我们
//! 自己定的（每个方向加多少毫秒），断言才有意义：测到的值要 > 0、不小于人为加的延迟、可重复；
//! 中继停止转发时是"超时"而不是假数字；连接断开后后台测量任务自己停下。

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use russh::keys::{Algorithm, PrivateKey};
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, MethodKind, MethodSet};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::sleep;
use zssh_core::ssh::latency::{level, Level, ProbeKind, Sample};
use zssh_core::ssh::Connection;

use common::{TempKnownHosts, TestServer};

struct MiniSsh;

impl server::Handler for MiniSsh {
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

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }
}

/// 中继的控制开关。
#[derive(Clone, Default)]
struct Relay {
    /// 停止转发但不关连接：服务器"不回话"。
    blackhole: Arc<AtomicBool>,
    /// 直接关掉两端：连接"断了"。
    kill: Arc<AtomicBool>,
}

/// 单方向转发：读到一段就等 `delay` 再写（同一个任务里顺序做，字节顺序不变）。
async fn pump(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    delay: Duration,
    relay: Relay,
) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = tokio::select! {
            n = from.read(&mut buf) => match n { Ok(0) | Err(_) => return, Ok(n) => n },
            _ = poll_flag(&relay.kill) => return,
        };
        sleep(delay).await;
        if relay.kill.load(Ordering::SeqCst) {
            return;
        }
        if relay.blackhole.load(Ordering::SeqCst) {
            continue;
        }
        if to.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
}

async fn poll_flag(flag: &AtomicBool) {
    while !flag.load(Ordering::SeqCst) {
        sleep(Duration::from_millis(20)).await;
    }
}

struct Harness {
    relay: Relay,
    connection: Arc<Connection>,
    _known_hosts: TempKnownHosts,
}

/// 起迷你 SSH 服务器 + 每个方向加 `one_way` 延迟的中继，连上去。
async fn start(one_way: Duration) -> Harness {
    let server_listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定");
    let server_addr = server_listener.local_addr().expect("端口");
    let config = Arc::new(server::Config {
        inactivity_timeout: None,
        auth_rejection_time: Duration::from_millis(1),
        keys: vec![PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("主机密钥")],
        ..Default::default()
    });
    tokio::spawn(async move {
        let Ok((socket, _)) = server_listener.accept().await else {
            return;
        };
        if let Ok(running) = server::run_stream(config, socket, MiniSsh).await {
            let _ = running.await;
        }
    });

    let relay = Relay::default();
    let relay_listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定中继");
    let relay_addr = relay_listener.local_addr().expect("端口");
    let relay_for_task = relay.clone();
    tokio::spawn(async move {
        let Ok((client, _)) = relay_listener.accept().await else {
            return;
        };
        let Ok(upstream) = TcpStream::connect(server_addr).await else {
            return;
        };
        let (client_r, client_w) = client.into_split();
        let (up_r, up_w) = upstream.into_split();
        tokio::spawn(pump(client_r, up_w, one_way, relay_for_task.clone()));
        tokio::spawn(pump(up_r, client_w, one_way, relay_for_task));
    });

    let known_hosts = TempKnownHosts::new("latency");
    let connection = TestServer::fake("127.0.0.1", relay_addr.port(), "tester", "pw")
        .connect(&known_hosts)
        .await
        .expect("连接迷你服务器");
    Harness {
        relay,
        connection: Arc::new(connection),
        _known_hosts: known_hosts,
    }
}

#[tokio::test]
async fn a_real_round_trip_is_positive_repeatable_and_not_faster_than_the_link() {
    let harness = start(Duration::from_millis(25)).await;
    let mut seen = Vec::new();
    for _ in 0..4 {
        let rtt = harness
            .connection
            .measure_rtt(Duration::from_secs(5))
            .await
            .expect("连接还在")
            .expect("服务器会回复（失败也算一次往返）");
        seen.push(rtt);
    }
    for rtt in &seen {
        // 每个方向各加 25ms：往返至少 50ms；不会离谱地慢。
        assert!(*rtt >= Duration::from_millis(50), "{rtt:?}");
        assert!(*rtt < Duration::from_secs(2), "{rtt:?}");
    }
    // 可重复：同一条链路上几次的结果在同一个量级。
    let (min, max) = (seen.iter().min().unwrap(), seen.iter().max().unwrap());
    assert!(*max < *min * 4, "几次测量差得太远：{seen:?}");
    assert_eq!(level(Sample::Rtt(*min)), Level::Good);
}

#[tokio::test]
async fn an_unreachable_server_is_a_timeout_not_a_made_up_number() {
    let harness = start(Duration::from_millis(1)).await;
    assert!(harness
        .connection
        .measure_rtt(Duration::from_secs(2))
        .await
        .expect("连接还在")
        .is_some());
    harness.relay.blackhole.store(true, Ordering::SeqCst);
    let started = Instant::now();
    let result = harness
        .connection
        .measure_rtt(Duration::from_millis(400))
        .await
        .expect("没断，只是不回话");
    assert_eq!(result, None);
    assert!(started.elapsed() >= Duration::from_millis(400));
}

#[tokio::test]
async fn the_monitor_measures_only_while_watched_and_stops_after_a_disconnect() {
    let harness = start(Duration::from_millis(10)).await;
    let monitor = Arc::clone(harness.connection.latency());
    assert_eq!(monitor.kind(), ProbeKind::Ssh);
    harness.connection.start_latency_probe();
    // 重复调用不会再起一个任务。
    harness.connection.start_latency_probe();

    // 没有窗格看得见：不测。
    sleep(Duration::from_millis(700)).await;
    assert!(monitor.snapshot().stats.is_empty(), "不可见时不该发请求");

    // 看得见：很快有第一个值，等一个间隔后有第二个。
    let mut viewer = monitor.viewer();
    viewer.set_visible(true);
    let deadline = Instant::now() + Duration::from_secs(8);
    while monitor.snapshot().stats.len() < 2 {
        assert!(Instant::now() < deadline, "8 秒内应当至少测到两次");
        sleep(Duration::from_millis(100)).await;
    }
    let snap = monitor.snapshot();
    let Some(Sample::Rtt(first)) = snap.stats.last() else {
        panic!("最近一次应当是正常往返：{snap:?}");
    };
    assert!(first >= Duration::from_millis(20), "{first:?}");
    assert!(snap.stats.average().is_some());
    assert_eq!(snap.stats.lost(), (0, snap.stats.len()));
    assert!(!snap.stopped);

    // 对端断开：任务自己停下，不再有新样本。
    harness.relay.kill.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(15);
    while !monitor.snapshot().stopped {
        assert!(Instant::now() < deadline, "断开后测量任务没有停下");
        sleep(Duration::from_millis(100)).await;
    }
    let count = monitor.snapshot().stats.len();
    sleep(Duration::from_millis(300)).await;
    assert_eq!(monitor.snapshot().stats.len(), count);
}
