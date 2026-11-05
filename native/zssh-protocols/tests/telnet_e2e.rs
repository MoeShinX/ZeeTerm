//! Telnet 端到端：本机回环上的假 Telnet 服务器 ↔ 真实的会话任务 ↔ 事件流 / 流控门 / 终端泵。

// 测试 / 示例代码：失败就让它 panic，不包装成错误返回（生产代码的 unwrap / expect 由 clippy 拦）。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use zssh_core::model::{LocalEcho, TelnetOptions, TextEncoding};
use zssh_core::ssh::FlowGate;
use zssh_core::ssh::flow::HIGH_WATERMARK;
use zssh_protocols::StreamBackend;
use zssh_protocols::lang::{Language, plaintext_notice_in, raw_tcp_notice_in};
use zssh_protocols::telnet::codec::{
    DO, DONT, IAC, NOP, OPT_BINARY, OPT_ECHO, OPT_NAWS, OPT_SGA, OPT_TTYPE, SB, SE, WILL, WONT,
    naws_frame,
};
use zssh_protocols::telnet::{TelnetParams, spawn_telnet};
use zssh_session::{
    CoreFilter, SessionBackend, SessionCommand, SessionDriver, SessionEvent, SessionPump,
};
use zssh_terminal::Size;

const WAIT: Duration = Duration::from_secs(5);

struct Harness {
    backend: StreamBackend,
    events: std::sync::mpsc::Receiver<SessionEvent>,
    gate: Arc<FlowGate>,
    server: TcpStream,
}

async fn connect(params: TelnetParams) -> Harness {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("监听");
    let addr = listener.local_addr().expect("地址");
    let client = TcpStream::connect(addr).await.expect("连接");
    let (server, _) = listener.accept().await.expect("接受");
    let (backend, events, gate) = spawn_telnet(client, params, 1, Box::new(()));
    Harness {
        backend,
        events,
        gate,
        server,
    }
}

fn params(options: TelnetOptions) -> TelnetParams {
    TelnetParams::new(options, String::new(), None, 80, 24)
}

/// 从服务器这一侧读，直到累计的字节里出现 `needle`（或超时失败）。
async fn read_until(server: &mut TcpStream, needle: &[u8]) -> Vec<u8> {
    let mut seen = Vec::new();
    let deadline = Instant::now() + WAIT;
    let mut buffer = [0u8; 4096];
    while !contains(&seen, needle) {
        let left = deadline.saturating_duration_since(Instant::now());
        let read = tokio::time::timeout(left, server.read(&mut buffer))
            .await
            .unwrap_or_else(|_| panic!("等不到 {needle:?}，已收到 {seen:?}"))
            .expect("读");
        assert!(read > 0, "对端关闭了，已收到 {seen:?}");
        seen.extend_from_slice(&buffer[..read]);
    }
    seen
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// 一直读事件，直到收到的数据文本里出现 `needle`；返回目前收到的全部数据文本。
fn wait_text(harness: &Harness, needle: &str) -> String {
    let mut text = String::new();
    let deadline = Instant::now() + WAIT;
    while !text.contains(needle) {
        let left = deadline.saturating_duration_since(Instant::now());
        match harness.events.recv_timeout(left) {
            Ok(SessionEvent::Data { bytes, .. }) => {
                harness.gate.on_ack(bytes.len());
                text.push_str(&String::from_utf8_lossy(&bytes));
            }
            Ok(SessionEvent::Notice { text: notice, .. }) => text.push_str(&notice),
            Ok(SessionEvent::Exit { .. } | SessionEvent::Error { .. }) => {
                panic!("会话提前结束，已收到 {text:?}")
            }
            Ok(_) => {}
            Err(_) => panic!("等不到 {needle:?}，已收到 {text:?}"),
        }
    }
    text
}

fn wait_event(harness: &Harness, pick: impl Fn(&SessionEvent) -> bool) -> SessionEvent {
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let event = harness.events.recv_timeout(left).expect("等事件超时");
        if let SessionEvent::Data { bytes, .. } = &event {
            harness.gate.on_ack(bytes.len());
        }
        if pick(&event) {
            return event;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_session_starts_with_the_plaintext_warning_and_offers_options() {
    let mut harness = connect(params(TelnetOptions::default())).await;
    let first = harness.events.recv_timeout(WAIT).expect("第一个事件");
    assert_eq!(
        first,
        SessionEvent::Notice {
            generation: 1,
            text: plaintext_notice_in(Language::Zh)
        }
    );
    // 连接后主动提供：NAWS、TTYPE、SGA，并要求对端 SGA。
    let wire = read_until(&mut harness.server, &[IAC, DO, OPT_SGA]).await;
    for expected in [
        [IAC, WILL, OPT_NAWS],
        [IAC, WILL, OPT_TTYPE],
        [IAC, WILL, OPT_SGA],
    ] {
        assert!(contains(&wire, &expected), "{wire:?} 里没有 {expected:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn negotiation_naws_ttype_and_resize_reach_the_server() {
    let mut harness = connect(TelnetParams::new(
        TelnetOptions::default(),
        String::new(),
        None,
        100,
        30,
    ))
    .await;
    let mut script = vec![IAC, DO, OPT_NAWS, IAC, DO, OPT_TTYPE, IAC, WILL, OPT_ECHO];
    script.extend_from_slice(&[IAC, DO, OPT_BINARY, IAC, DO, 39, IAC, WILL, 34]);
    script.extend_from_slice(&[IAC, SB, OPT_TTYPE, 1, IAC, SE]);
    harness.server.write_all(&script).await.unwrap();

    let mut expected_ttype = vec![IAC, SB, OPT_TTYPE, 0];
    expected_ttype.extend_from_slice(b"xterm-256color");
    expected_ttype.extend_from_slice(&[IAC, SE]);
    let wire = read_until(&mut harness.server, &expected_ttype).await;
    assert!(
        contains(&wire, &naws_frame(100, 30)),
        "初始窗口大小：{wire:?}"
    );
    assert!(contains(&wire, &[IAC, DO, OPT_ECHO]), "接受 ECHO：{wire:?}");
    assert!(
        contains(&wire, &[IAC, WILL, OPT_BINARY]),
        "应答 BINARY：{wire:?}"
    );
    assert!(
        contains(&wire, &[IAC, WONT, 39]),
        "拒绝 NEW-ENVIRON：{wire:?}"
    );
    assert!(contains(&wire, &[IAC, DONT, 34]), "拒绝 LINEMODE：{wire:?}");

    harness.backend.send(SessionCommand::Resize(Size {
        rows: 40,
        cols: 120,
    }));
    read_until(&mut harness.server, &naws_frame(120, 40)).await;

    // 宽 255 的窗口：NAWS 里的 255 必须转义。
    harness.backend.send(SessionCommand::Resize(Size {
        rows: 24,
        cols: 255,
    }));
    read_until(&mut harness.server, &naws_frame(255, 24)).await;
}

/// R06：对端不读、输入把命令队列塞满的时候只改一次窗口大小；对端恢复读取以后，
/// 最新的窗口大小仍然必须到达（窗口大小是最新状态，不能和按键一起被静默丢掉）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resize_sent_while_the_input_queue_is_full_still_reaches_the_server() {
    let mut harness = connect(TelnetParams::new(
        TelnetOptions::default(),
        String::new(),
        None,
        100,
        30,
    ))
    .await;
    harness
        .server
        .write_all(&[IAC, DO, OPT_NAWS])
        .await
        .unwrap();
    read_until(&mut harness.server, &naws_frame(100, 30)).await;

    // 服务器从此不读：一直发大块输入，直到界面这边收到"已丢弃按键"的提示——命令队列确实满了。
    let chunk = vec![b'k'; 32 * 1024];
    let deadline = Instant::now() + WAIT;
    let mut full = false;
    while !full {
        assert!(Instant::now() < deadline, "队列一直没满");
        for _ in 0..64 {
            harness.backend.send(SessionCommand::Write(chunk.clone()));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        while let Ok(event) = harness.events.try_recv() {
            if let SessionEvent::Notice { text, .. } = &event
                && text.contains("已丢弃")
            {
                full = true;
            }
        }
    }

    // 只改这一次窗口大小，之后不再碰窗口。
    harness.backend.send(SessionCommand::Resize(Size {
        rows: 50,
        cols: 132,
    }));

    // 服务器恢复读取：把堆积的输入读完（量很大，只在滚动窗口里找，不整个攒起来），
    // 最新的窗口大小要出现在流里。
    let needle = naws_frame(132, 50);
    let mut window: Vec<u8> = Vec::new();
    let mut buffer = vec![0u8; 64 * 1024];
    while !contains(&window, &needle) {
        // 堆积的输入读完以后流就安静了：几秒没有新字节还没看到窗口大小，就是它丢了。
        let read = tokio::time::timeout(Duration::from_secs(3), harness.server.read(&mut buffer))
            .await
            .expect("堆积的输入读完以后一直没等到最新的窗口大小")
            .expect("读");
        assert!(read > 0, "对端提前关闭了");
        window.extend_from_slice(&buffer[..read]);
        let keep = needle.len() - 1;
        if window.len() > keep {
            let cut = window.len() - keep;
            if !contains(&window, &needle) {
                window.drain(..cut);
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn naws_off_never_sends_a_window_size() {
    let mut harness = connect(params(TelnetOptions {
        naws: false,
        ..Default::default()
    }))
    .await;
    harness
        .server
        .write_all(&[IAC, DO, OPT_NAWS])
        .await
        .unwrap();
    let wire = read_until(&mut harness.server, &[IAC, WONT, OPT_NAWS]).await;
    assert!(!contains(&wire, &[IAC, SB, OPT_NAWS]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_echo_is_automatic_off_when_the_server_will_echo() {
    // 服务器不协商 ECHO：自动模式本地回显。
    let mut harness = connect(params(TelnetOptions::default())).await;
    harness
        .backend
        .send(SessionCommand::Write(b"ab\r".to_vec()));
    let wire = read_until(&mut harness.server, b"ab\r\n").await;
    assert!(contains(&wire, b"ab\r\n"), "默认回车发 CRLF：{wire:?}");
    wait_text(&harness, "ab\r\n");

    // 服务器声明 WILL ECHO：本地回显关闭，键入只发出去、不显示。
    harness
        .server
        .write_all(&[IAC, WILL, OPT_ECHO])
        .await
        .unwrap();
    read_until(&mut harness.server, &[IAC, DO, OPT_ECHO]).await;
    harness
        .backend
        .send(SessionCommand::Write(b"secret".to_vec()));
    read_until(&mut harness.server, b"secret").await;
    harness.server.write_all(b"[after]").await.unwrap();
    let text = wait_text(&harness, "[after]");
    assert!(!text.contains("secret"), "不该本地回显：{text:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forced_local_echo_modes_ignore_the_negotiation() {
    let mut harness = connect(params(TelnetOptions {
        local_echo: LocalEcho::On,
        ..Default::default()
    }))
    .await;
    harness
        .server
        .write_all(&[IAC, WILL, OPT_ECHO])
        .await
        .unwrap();
    read_until(&mut harness.server, &[IAC, DO, OPT_ECHO]).await;
    harness.backend.send(SessionCommand::Write(b"hi".to_vec()));
    wait_text(&harness, "hi");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn newline_modes_and_iac_in_user_input() {
    let mut harness = connect(params(TelnetOptions {
        newline: zssh_core::model::TelnetNewline::Cr,
        encoding: TextEncoding::Latin1,
        local_echo: LocalEcho::Off,
        ..Default::default()
    }))
    .await;
    // Latin1 的 ÿ 是 0xFF：发出去必须写成 IAC IAC。
    harness
        .backend
        .send(SessionCommand::Write("a\u{ff}b\r".as_bytes().to_vec()));
    read_until(&mut harness.server, b"a\xff\xffb\r").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_login_answers_the_prompts_and_never_shows_the_password() {
    let mut options = TelnetOptions::default();
    options.auto_login.enabled = true;
    let mut harness = connect(TelnetParams::new(
        options,
        "operator".to_owned(),
        Some("hunter2".to_owned()),
        80,
        24,
    ))
    .await;
    harness
        .server
        .write_all(b"Welcome\r\nhost login: ")
        .await
        .unwrap();
    read_until(&mut harness.server, b"operator\r\n").await;
    harness
        .server
        .write_all(b"operator\r\nPassword: ")
        .await
        .unwrap();
    read_until(&mut harness.server, b"hunter2\r\n").await;
    harness
        .server
        .write_all(b"\r\nLast login: today\r\n$ ")
        .await
        .unwrap();
    let text = wait_text(&harness, "$ ");
    assert!(!text.contains("hunter2"), "密码不能出现在屏幕上：{text:?}");
    // 登录之后再出现提示不会重复答。
    harness.server.write_all(b"\r\nlogin: ").await.unwrap();
    wait_text(&harness, "login: ");
    let mut buffer = [0u8; 64];
    assert!(
        tokio::time::timeout(Duration::from_millis(300), harness.server.read(&mut buffer))
            .await
            .is_err(),
        "不该再有任何发送"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typing_before_the_prompt_cancels_auto_login() {
    let mut options = TelnetOptions::default();
    options.auto_login.enabled = true;
    let mut harness = connect(TelnetParams::new(
        options,
        "operator".to_owned(),
        Some("hunter2".to_owned()),
        80,
        24,
    ))
    .await;
    harness.backend.send(SessionCommand::Write(b"x".to_vec()));
    read_until(&mut harness.server, b"x").await;
    harness.server.write_all(b"login: ").await.unwrap();
    wait_text(&harness, "login: ");
    let mut buffer = [0u8; 64];
    // 连接开头的协商字节可能还在路上，但用户名不能出现。
    let got = tokio::time::timeout(Duration::from_millis(300), harness.server.read(&mut buffer))
        .await
        .ok()
        .and_then(Result::ok)
        .map(|n| buffer[..n].to_vec())
        .unwrap_or_default();
    assert!(!contains(&got, b"operator"), "{got:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_closing_the_connection_ends_the_session() {
    let harness = connect(params(TelnetOptions::default())).await;
    let Harness {
        mut server,
        events,
        gate,
        backend,
    } = harness;
    // 先读掉开头的协商字节再正常关闭：带着没读的数据关 socket 会变成连接重置（那是另一条错误路径）。
    read_until(&mut server, &[IAC, DO, OPT_SGA]).await;
    server.shutdown().await.unwrap();
    drop(server);
    let deadline = Instant::now() + WAIT;
    loop {
        let event = events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("应当收到退出事件");
        if matches!(
            event,
            SessionEvent::Exit {
                generation: 1,
                code: None
            }
        ) {
            break;
        }
    }
    // 会话结束后流控门关闭。
    let deadline = Instant::now() + WAIT;
    while !gate.is_closed() {
        assert!(Instant::now() < deadline, "会话结束后流控门应当关闭");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(backend);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_command_shuts_the_connection_down() {
    let mut harness = connect(params(TelnetOptions::default())).await;
    harness.backend.send(SessionCommand::Close);
    let mut buffer = [0u8; 256];
    let deadline = Instant::now() + WAIT;
    // 先读掉连接开头的协商字节，最后应当读到 EOF。
    loop {
        let read = tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            harness.server.read(&mut buffer),
        )
        .await
        .expect("等 EOF 超时")
        .expect("读");
        if read == 0 {
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_backend_closes_the_connection() {
    let Harness {
        backend,
        mut server,
        ..
    } = connect(params(TelnetOptions::default())).await;
    drop(backend);
    let mut buffer = [0u8; 256];
    let deadline = Instant::now() + WAIT;
    loop {
        let read = tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            server.read(&mut buffer),
        )
        .await
        .expect("等 EOF 超时")
        .expect("读");
        if read == 0 {
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gbk_output_is_converted_even_when_a_character_is_split_across_packets() {
    let mut harness = connect(params(TelnetOptions {
        encoding: TextEncoding::Gbk,
        ..Default::default()
    }))
    .await;
    harness.server.write_all(b"a\xd6").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    harness.server.write_all(b"\xd0b").await.unwrap();
    wait_text(&harness, "a中b");

    harness
        .backend
        .send(SessionCommand::Write("中\r".as_bytes().to_vec()));
    read_until(&mut harness.server, b"\xd6\xd0\r\n").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_tcp_mode_does_no_telnet_processing_at_all() {
    let mut harness = connect(params(TelnetOptions {
        raw_tcp: true,
        local_echo: LocalEcho::Off,
        newline: zssh_core::model::TelnetNewline::Lf,
        ..Default::default()
    }))
    .await;
    let first = harness.events.recv_timeout(WAIT).unwrap();
    assert_eq!(
        first,
        SessionEvent::Notice {
            generation: 1,
            text: raw_tcp_notice_in(Language::Zh)
        }
    );
    // 对端发来的 IAC 序列原样交给终端，也不回任何协商。
    harness
        .server
        .write_all(b"\xff\xfb\x01hello")
        .await
        .unwrap();
    let event = wait_event(&harness, |e| matches!(e, SessionEvent::Data { .. }));
    assert_eq!(
        event,
        SessionEvent::Data {
            generation: 1,
            bytes: b"\xff\xfb\x01hello".to_vec()
        }
    );
    // 发出去的 0xFF 不转义；一开始也没有任何协商字节。
    harness
        .backend
        .send(SessionCommand::Write(b"\xffx\r".to_vec()));
    let mut seen = Vec::new();
    let mut buffer = [0u8; 64];
    while seen.len() < 3 {
        let read = tokio::time::timeout(WAIT, harness.server.read(&mut buffer))
            .await
            .expect("读超时")
            .unwrap();
        seen.extend_from_slice(&buffer[..read]);
    }
    assert_eq!(seen, b"\xffx\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_connections_get_a_nop_keepalive_but_raw_ones_do_not() {
    let mut telnet_params = params(TelnetOptions::default());
    telnet_params.keepalive = Duration::from_millis(100);
    let mut harness = connect(telnet_params).await;
    read_until(&mut harness.server, &[IAC, NOP]).await;

    let mut raw_params = params(TelnetOptions {
        raw_tcp: true,
        ..Default::default()
    });
    raw_params.keepalive = Duration::from_millis(100);
    let mut raw = connect(raw_params).await;
    let mut buffer = [0u8; 16];
    assert!(
        tokio::time::timeout(Duration::from_millis(500), raw.server.read(&mut buffer))
            .await
            .is_err(),
        "纯 TCP 不能发 NOP"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fast_server_cannot_make_the_client_buffer_without_bound() {
    const TOTAL: usize = 8 * 1024 * 1024;
    let Harness {
        backend,
        events,
        gate,
        server,
    } = connect(params(TelnetOptions::default())).await;
    let (mut server_read, mut server_write) = server.into_split();
    let writer = tokio::spawn(async move {
        let chunk = vec![b'x'; 16 * 1024];
        let mut sent = 0usize;
        while sent < TOTAL {
            server_write.write_all(&chunk).await.expect("写");
            sent += chunk.len();
        }
        server_write
    });
    // 吃掉客户端开头的协商字节，别让服务器那一侧的接收缓冲干扰。
    tokio::spawn(async move {
        let mut sink = [0u8; 1024];
        while server_read.read(&mut sink).await.is_ok_and(|n| n > 0) {}
    });

    // 一个 ack 都不给：会话必须在高水位附近停止读取，服务器的写被 TCP 窗口挡住。
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        gate.unacked() <= HIGH_WATERMARK + 8192,
        "未确认量越过了高水位：{}",
        gate.unacked()
    );
    assert!(!writer.is_finished(), "8 MiB 不该已经全部发完");

    // 像界面那样一边收一边 ack，数据一个字节都不能少。
    let received = tokio::task::spawn_blocking(move || {
        let mut received = 0usize;
        let deadline = Instant::now() + Duration::from_secs(30);
        while received < TOTAL {
            let left = deadline.saturating_duration_since(Instant::now());
            match events.recv_timeout(left).expect("等数据超时") {
                SessionEvent::Data { bytes, .. } => {
                    received += bytes.len();
                    gate.on_ack(bytes.len());
                }
                SessionEvent::Notice { .. } => {}
                other => panic!("意外事件 {other:?}"),
            }
        }
        (received, gate.peak_unacked())
    })
    .await
    .unwrap();
    assert_eq!(received.0, TOTAL);
    assert!(received.1 <= HIGH_WATERMARK + 8192, "峰值 {}", received.1);
    writer.await.unwrap();
    drop(backend);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_flows_through_the_real_terminal_pump_and_the_ledger_balances() {
    let Harness {
        backend,
        events,
        gate,
        mut server,
    } = connect(params(TelnetOptions::default())).await;
    let pump = SessionPump::new(
        Size { rows: 6, cols: 40 },
        100,
        Arc::clone(&gate),
        CoreFilter::new(),
        1,
    );
    let mut driver = SessionDriver::new(pump, events);
    server
        .write_all(b"\xff\xfb\x03\xff\xfa\x18\x01\xff\xf0login: ")
        .await
        .unwrap();
    server.write_all(b"\x1b[1mbold\x1b[0m\r\n").await.unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        driver.poll();
        let screen: String = driver
            .pump()
            .snapshot()
            .rows
            .iter()
            .map(|row| row.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        if screen.contains("login:") && screen.contains("bold") {
            break;
        }
        assert!(Instant::now() < deadline, "屏幕上没有出现输出：{screen:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let ledger = driver.pump().ledger();
    assert!(ledger.is_balanced(), "账本不平衡：{ledger:?}");
    assert_eq!(gate.unacked(), 0);
    drop(backend);
}
