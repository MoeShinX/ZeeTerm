//! 测试用进程内 SSH 服务器：只监听本机回环、端口由系统分配、主机密钥现场生成。
//!
//! 它把真实的 russh 传输和 `zssh_core` 的 shell 通道/流控接起来，但不需要外部服务器。
//! shell 是个迷你解释器，按回车处理一行：
//! - `flood`：发出 [`FLOOD_BYTES`] 字节再发 `FLOOD_DONE`（远超流控高水位，验证不会卡死）；
//! - `bye`：退出码 3，然后关闭通道；
//! - `drop`：不发退出码，直接断开整条 TCP 连接；
//! - 其他：回 `ECHO:<行>`；收到 0x03 回 `GOT_CTRL_C`。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use russh::keys::{Algorithm, PrivateKey};
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use tokio::net::TcpListener;

/// `flood` 命令发出的字节数。
pub const FLOOD_BYTES: usize = 6 * 1024 * 1024;

struct MiniShell {
    line: Vec<u8>,
}

impl server::Handler for MiniShell {
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
        session.data(channel, b"MINI_READY\r\n".to_vec())
    }

    async fn window_change_request(
        &mut self,
        _channel: ChannelId,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        for &byte in data {
            match byte {
                0x03 => session.data(channel, b"GOT_CTRL_C\r\n".to_vec())?,
                b'\r' => {
                    let line =
                        String::from_utf8_lossy(&std::mem::take(&mut self.line)).into_owned();
                    self.run_line(&line, channel, session)?;
                }
                other => self.line.push(other),
            }
        }
        Ok(())
    }
}

impl MiniShell {
    fn run_line(
        &mut self,
        line: &str,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), russh::Error> {
        match line {
            "flood" => {
                // 分块发送并等窗口：和真实服务器一样受 SSH 窗口与客户端读取速度约束。
                let handle = session.handle();
                tokio::spawn(async move {
                    let chunk = vec![b'x'; 16 * 1024 - 2];
                    let mut sent = 0usize;
                    while sent < FLOOD_BYTES {
                        let mut piece = chunk.clone();
                        piece.extend_from_slice(b"\r\n");
                        sent += piece.len();
                        if handle.data(channel, piece).await.is_err() {
                            return;
                        }
                    }
                    let _ = handle.data(channel, b"FLOOD_DONE\r\n".to_vec()).await;
                });
                Ok(())
            }
            "bye" => {
                session.exit_status_request(channel, 3)?;
                session.eof(channel)?;
                session.close(channel)
            }
            "drop" => Err(russh::Error::Disconnect),
            other => session.data(channel, format!("ECHO:{other}\r\n").into_bytes()),
        }
    }
}

/// 起服务器（接一个连接），返回监听地址。
pub async fn spawn() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定本机端口");
    let addr = listener.local_addr().expect("取端口");
    let config = Arc::new(server::Config {
        inactivity_timeout: None,
        auth_rejection_time: Duration::from_millis(1),
        keys: vec![PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("生成主机密钥")],
        ..Default::default()
    });
    tokio::spawn(async move {
        let Ok((socket, _)) = listener.accept().await else {
            return;
        };
        let handler = MiniShell { line: Vec::new() };
        if let Ok(running) = server::run_stream(config, socket, handler).await {
            let _ = running.await;
        }
    });
    addr
}

/// 只做 `direct-tcpip` 转发的迷你跳板机：把通道接到目标地址的 TCP 连接上（验证 Telnet 经 SSH 跳板）。
struct Forwarder;

impl server::Handler for Forwarder {
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

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host_to_connect: &str,
        port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let target = format!("{host_to_connect}:{port_to_connect}");
        match tokio::net::TcpStream::connect(&target).await {
            Ok(mut upstream) => {
                reply.accept().await;
                tokio::spawn(async move {
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
                });
            }
            Err(_) => reply.reject(russh::ChannelOpenFailure::ConnectFailed).await,
        }
        Ok(())
    }
}

/// 起一个只转发的 SSH 跳板机（接一个连接），返回监听地址。
pub async fn spawn_forwarding() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定本机端口");
    let addr = listener.local_addr().expect("取端口");
    let config = Arc::new(server::Config {
        inactivity_timeout: None,
        auth_rejection_time: Duration::from_millis(1),
        keys: vec![PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("生成主机密钥")],
        ..Default::default()
    });
    tokio::spawn(async move {
        let Ok((socket, _)) = listener.accept().await else {
            return;
        };
        if let Ok(running) = server::run_stream(config, socket, Forwarder).await {
            let _ = running.await;
        }
    });
    addr
}
