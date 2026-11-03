//! X11 转发连真实 sshd 跑一遍（只读，不装包、不改服务器配置）。
//!
//! 流程：开带 X11 的 shell → 在远端用 bash 的 `/dev/tcp` 扮演 X 程序，按 `xauth list` 里 sshd
//! 登记的 cookie 手写一个 X11 握手包连 `$DISPLAY` → 进程内假 X 服务器应当收到被换成真值的 cookie，
//! 远端读到假 X 服务器的"成功"回复。服务器没开 X11Forwarding 或没有 xauth 时跳过并说明（不算失败）。
//!
//! 环境变量 `ZSSH_TEST_HOST` 等不全时跳过。

// 测试 / 示例代码：失败就让它 panic，不包装成错误返回（生产代码的 unwrap / expect 由 clippy 拦）。
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use zssh_core::ssh::{ShellEvent, ShellSession, DEFAULT_BUFFER_LIMIT};
use zssh_core::x11::cookie::{setup_auth, setup_packet_len, SetupLen};
use zssh_core::x11::{X11Failure, X11Outcome, X11Request, X11Route, XEndpoint};

use common::{serial, TempKnownHosts, TestServer};

const TIMEOUT: Duration = Duration::from_secs(30);

/// 一个最小的假 X 服务器：记下握手包里的 cookie，回复"成功"。
async fn fake_x() -> (u16, Arc<Mutex<Vec<Vec<u8>>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定");
    let port = listener.local_addr().expect("端口").port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let sink = Arc::clone(&sink);
            tokio::spawn(async move {
                let mut buffered = Vec::new();
                let mut chunk = [0u8; 1024];
                let len = loop {
                    if let SetupLen::Total(len) = setup_packet_len(&buffered) {
                        if buffered.len() >= len {
                            break len;
                        }
                    }
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buffered.extend_from_slice(&chunk[..n]),
                    }
                };
                if let Some((_, data)) = setup_auth(&buffered[..len]) {
                    sink.lock().unwrap().push(data.to_vec());
                }
                let _ = socket.write_all(&[1, 0, 11, 0, 0, 0, 0, 0]).await;
                let mut rest = [0u8; 64];
                let _ = socket.read(&mut rest).await;
            });
        }
    });
    (port, seen)
}

async fn read_until(shell: &mut ShellSession, needle: &str) -> Result<String, String> {
    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while !text.contains(needle) {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, shell.next_event()).await {
            Ok(Some(ShellEvent::Data(data))) => text.push_str(&String::from_utf8_lossy(&data)),
            Ok(Some(ShellEvent::Exit(_))) => {}
            _ => return Err(format!("没等到 {needle:?}，收到：{text:?}")),
        }
    }
    Ok(text)
}

#[tokio::test]
async fn x11_forwarding_through_a_real_sshd_swaps_the_cookie() {
    let _serial = serial().await;
    let server = match TestServer::from_env() {
        Ok(server) => server,
        Err(reason) => {
            println!("跳过 x11_forwarding_through_a_real_sshd_swaps_the_cookie：{reason}");
            return;
        }
    };
    let known_hosts = TempKnownHosts::new("x11-live");
    let connection = server.connect(&known_hosts).await.expect("连接测试服务器");

    let (port, seen) = fake_x().await;
    let real = vec![0xC3; 16];
    let (mut shell, outcome) = connection
        .open_shell_x11(
            120,
            30,
            DEFAULT_BUFFER_LIMIT,
            None,
            Ok(X11Request {
                route: X11Route {
                    endpoint: XEndpoint {
                        host: "127.0.0.1".into(),
                        port,
                        screen: 0,
                    },
                    real_cookie: Some(real.clone()),
                },
                on_failure: None,
            }),
        )
        .await
        .expect("开 shell");

    match outcome {
        X11Outcome::Active { .. } => {}
        X11Outcome::Skipped(X11Failure::ServerRefused) => {
            println!("跳过：测试服务器没有开启 X11Forwarding 或没有 xauth（提示已写进终端）");
            return;
        }
        other => panic!("预期 Active：{other:?}"),
    }

    // 远端：取 DISPLAY 的端口和 xauth 里 sshd 登记的 cookie，手写握手包连过去，打印回复字节。
    let script = concat!(
        "d=${DISPLAY#*:}; d=${d%%.*}; p=$((6000+d)); ",
        "c=$(xauth list \"$DISPLAY\" 2>/dev/null | awk '{print $3}' | head -1); ",
        "echo X11_COOKIE_LEN=${#c}; ",
        "exec 3<>/dev/tcp/127.0.0.1/$p && ",
        "printf '\\x6c\\x00\\x0b\\x00\\x00\\x00\\x12\\x00\\x10\\x00\\x00\\x00MIT-MAGIC-COOKIE-1\\x00\\x00'\"$(echo $c | sed 's/../\\\\x&/g')\" >&3 && ",
        "echo X11_REPLY=$(head -c 8 <&3 | od -An -tu1 | tr -s ' ' '_')\r"
    );
    shell.write(script.as_bytes()).await.expect("写命令");
    // 回显里也有命令本身，所以只等"=数字"这种只会出现在真正输出里的片段。
    if let Err(message) = read_until(&mut shell, "X11_COOKIE_LEN=32").await {
        panic!(
            "sshd 登记的假 cookie 应当是 32 个十六进制字符：{}",
            server.sanitize(&message)
        );
    }
    if let Err(message) = read_until(&mut shell, "_1_0_11_0_0_0_0_0").await {
        panic!(
            "远端没有读到假 X 服务器的成功回复：{}",
            server.sanitize(&message)
        );
    }

    let cookies = seen.lock().unwrap().clone();
    assert_eq!(
        cookies,
        vec![real],
        "本机 X 服务器收到的必须是换过的真 cookie"
    );

    // 关掉终端后登记应当撤销。
    drop(shell);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while connection.x11_hub().registration_count() != 0 {
        assert!(tokio::time::Instant::now() < deadline, "登记没有撤销");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
