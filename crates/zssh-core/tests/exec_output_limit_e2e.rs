//! `Connection::exec` 的输出上限端到端（不需要真服务器）。
//!
//! 进程内迷你 SSH 服务器只监听本机回环：按命令名决定往 stdout / stderr 灌多少数据。
//! 验证：超限明确报错（不是返回截断内容）、报错后同一条连接仍可用、
//! 正常大小的输出和非 0 退出码的行为不变。

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use russh::keys::{Algorithm, PrivateKey};
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use tokio::net::TcpListener;
use zssh_core::error::{reason_keys, ZsshError};
use zssh_core::ssh::Connection;
use zssh_core::ErrorWithCode;

use common::{TempKnownHosts, TestServer};

/// 服务器每次发的块（远小于通道窗口，保证发得出去）。
const CHUNK: usize = 16 * 1024;

struct Flood;

impl server::Handler for Flood {
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

    /// 命令格式 `<stream> <字节数> <退出码>`：往 stdout（`out`）或 stderr（`err`）灌这么多字节。
    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).into_owned();
        let mut parts = command.split_whitespace();
        let stream = parts.next().unwrap_or("out").to_owned();
        let mut left: usize = parts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
        let code: u32 = parts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
        session.channel_success(channel)?;
        while left > 0 {
            let take = left.min(CHUNK);
            let block = vec![b'x'; take];
            if stream == "err" {
                session.extended_data(channel, 1, block)?;
            } else {
                session.data(channel, block)?;
            }
            left -= take;
        }
        session.exit_status_request(channel, code)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }
}

struct Harness {
    connection: Connection,
    _known_hosts: TempKnownHosts,
}

async fn start() -> Harness {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定");
    let addr = listener.local_addr().expect("端口");
    let config = Arc::new(server::Config {
        inactivity_timeout: None,
        auth_rejection_time: Duration::from_millis(1),
        keys: vec![PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("主机密钥")],
        ..Default::default()
    });
    tokio::spawn(async move {
        let Ok((socket, _)) = listener.accept().await else {
            return;
        };
        let handler = Flood;
        if let Ok(running) = server::run_stream(config, socket, handler).await {
            let _ = running.await;
        }
    });
    let known_hosts = TempKnownHosts::new("exec-output-limit-e2e");
    let connection = TestServer::fake("127.0.0.1", addr.port(), "tester", "pw")
        .connect(&known_hosts)
        .await
        .expect("连接迷你服务器");
    Harness {
        connection,
        _known_hosts: known_hosts,
    }
}

fn assert_too_large(err: &ZsshError, stream: &str) {
    let ZsshError::Shell(reason) = err else {
        panic!("应当是 Shell 原因：{err:?}");
    };
    assert_eq!(reason.key(), reason_keys::EXEC_OUTPUT_TOO_LARGE, "{err}");
    assert!(
        err.params().contains(&("stream", stream.to_owned())),
        "{:?}",
        err.params()
    );
}

#[tokio::test]
async fn output_within_the_limit_is_returned_in_full() {
    let harness = start().await;
    let out = harness
        .connection
        .exec_with_limit("out 100000 0", 100_000)
        .await
        .expect("恰好等于上限放行");
    assert_eq!(out.len(), 100_000);
    // 默认上限下，一般大小的输出照常返回。
    let out = harness.connection.exec("out 2048 0").await.unwrap();
    assert_eq!(out.len(), 2048);
}

#[tokio::test]
async fn stdout_over_the_limit_fails_instead_of_returning_truncated_output() {
    let harness = start().await;
    let err = harness
        .connection
        .exec_with_limit("out 1000000 0", 64 * 1024)
        .await
        .expect_err("超限必须报错，不能把截断的内容当成功");
    assert_too_large(&err, "stdout");
}

#[tokio::test]
async fn stderr_over_the_limit_fails_even_when_stdout_is_small() {
    let harness = start().await;
    let err = harness
        .connection
        .exec_with_limit("err 1000000 1", 64 * 1024)
        .await
        .expect_err("stderr 也有上限");
    assert_too_large(&err, "stderr");
}

#[tokio::test]
async fn connection_stays_usable_after_an_overflow() {
    let harness = start().await;
    let _ = harness
        .connection
        .exec_with_limit("out 1000000 0", 32 * 1024)
        .await
        .expect_err("超限");
    // 超限只废掉这一条 exec 通道，连接本身没坏：同一条连接马上还能跑下一条。
    let out = harness
        .connection
        .exec("out 10 0")
        .await
        .expect("连接仍可用");
    assert_eq!(out.len(), 10);
}

#[tokio::test]
async fn nonzero_exit_still_reports_the_command_failure() {
    let harness = start().await;
    let err = harness
        .connection
        .exec("err 5 3")
        .await
        .expect_err("退出码 3");
    let ZsshError::Shell(reason) = &err else {
        panic!("{err:?}");
    };
    assert_eq!(reason.key(), reason_keys::EXEC_NONZERO);
}
