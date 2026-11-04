//! 测试用进程内 SSH 服务器：只监听本机回环、端口由系统分配、主机密钥现场生成。
//!
//! 它把真实的 russh 传输和 `zssh_core` 的 shell 通道/流控接起来，但不需要外部服务器。
//! shell 是个迷你解释器，按回车处理一行：
//! - `flood`：发出 [`FLOOD_BYTES`] 字节再发 `FLOOD_DONE`（远超流控高水位，验证不会卡死）；
//! - `bye`：退出码 3，然后关闭通道；
//! - `drop`：不发退出码，直接断开整条 TCP 连接；
//! - `cpr`：清屏、把光标挪到第 5 行第 12 列，再发一条光标位置查询（`ESC[6n`）；
//! - `da`：发一条主设备属性查询（`ESC[c`）；
//! - 其他：回 `ECHO:<行>`；收到 0x03 回 `GOT_CTRL_C`；
//! - 收到一段以 ESC 开头的控制序列（终端对查询的回复）：回 `GOT_REPLY:<序列，ESC 写成 ^[>`。
//!
//! `exec` 请求默认被拒绝；用 [`spawn_answering_exec`] 起的服务器对任何 `exec` 回一段固定输出再退出码 0
//! （系统探测命令的端到端测试用）。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use russh::keys::{Algorithm, PrivateKey};
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use tokio::net::TcpListener;

/// 各个测试服务器的主机公钥（按监听地址）：「测试连接」不信任陌生指纹，测试要先把它写进 known_hosts。
fn host_keys()
-> &'static std::sync::Mutex<std::collections::HashMap<SocketAddr, russh::keys::PublicKey>> {
    static KEYS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<SocketAddr, russh::keys::PublicKey>>,
    > = std::sync::OnceLock::new();
    KEYS.get_or_init(Default::default)
}

fn remember_host_key(addr: SocketAddr, key: &PrivateKey) {
    host_keys()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(addr, key.public_key().clone());
}

/// 把 `addr` 上那台测试服务器的主机密钥写进 `known_hosts`（让它成为已信任的主机）。
#[allow(dead_code)]
pub fn trust_host_key(addr: SocketAddr, known_hosts: &std::path::Path) {
    let key = host_keys()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&addr)
        .cloned()
        .expect("这个地址不是 test_ssh_server 起的服务器");
    zssh_core::ssh::KnownHosts::new(known_hosts)
        .append(&addr.ip().to_string(), addr.port(), &key)
        .expect("预先信任测试服务器");
}

/// `flood` 命令发出的字节数。
pub const FLOOD_BYTES: usize = 6 * 1024 * 1024;

struct MiniShell {
    line: Vec<u8>,
    /// 正在收的一段以 ESC 开头的控制序列（客户端终端对查询的回复）。
    reply: Option<Vec<u8>>,
    /// `Some`：只接受这个密码（测认证失败用）；`None`：任何密码都收。
    password: Option<String>,
    /// `Some`：对 `exec` 请求回这段输出（退出码 0）；`None`：拒绝 `exec`。
    exec_output: Option<String>,
}

impl server::Handler for MiniShell {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Reject {
            proceed_with_methods: Some(MethodSet::from(&[MethodKind::Password][..])),
            partial_success: false,
        })
    }

    async fn auth_password(&mut self, _user: &str, password: &str) -> Result<Auth, Self::Error> {
        match &self.password {
            Some(expected) if expected != password => Ok(Auth::Reject {
                proceed_with_methods: Some(MethodSet::from(&[MethodKind::Password][..])),
                partial_success: false,
            }),
            _ => Ok(Auth::Accept),
        }
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

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        _data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(output) = self.exec_output.clone() else {
            return session.channel_failure(channel);
        };
        session.channel_success(channel)?;
        session.data(channel, output.into_bytes())?;
        session.exit_status_request(channel, 0)?;
        session.eof(channel)?;
        session.close(channel)
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
            if let Some(reply) = &mut self.reply {
                reply.push(byte);
                // `ESC [ ... <终止字节>`：终止字节在 0x40..=0x7e（参数和中间字节都比它小）。
                if reply.len() >= 3 && (0x40..=0x7e).contains(&byte) {
                    let text = String::from_utf8_lossy(reply).replace('\x1b', "^[");
                    self.reply = None;
                    session.data(channel, format!("GOT_REPLY:{text}\r\n").into_bytes())?;
                }
                continue;
            }
            if byte == 0x1b {
                self.reply = Some(vec![byte]);
                continue;
            }
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
            "cpr" => session.data(channel, b"\x1b[H\x1b[2J\x1b[5;12H\x1b[6n".to_vec()),
            "da" => session.data(channel, b"\x1b[c".to_vec()),
            other => session.data(channel, format!("ECHO:{other}\r\n").into_bytes()),
        }
    }
}

/// 起服务器（接一个连接），返回监听地址。
pub async fn spawn() -> SocketAddr {
    spawn_with(None, None).await
}

/// 同 [`spawn`]，但对任何 `exec` 请求回 `output` 再以退出码 0 结束。
#[allow(dead_code)]
pub async fn spawn_answering_exec(output: &str) -> SocketAddr {
    spawn_with(None, Some(output.to_owned())).await
}

/// 只接受 `password` 的服务器（别的密码认证失败）。
#[allow(dead_code)]
pub async fn spawn_requiring_password(password: &str) -> SocketAddr {
    spawn_with(Some(password.to_owned()), None).await
}

async fn spawn_with(password: Option<String>, exec_output: Option<String>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定本机端口");
    let addr = listener.local_addr().expect("取端口");
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("生成主机密钥");
    remember_host_key(addr, &host_key);
    let config = Arc::new(server::Config {
        inactivity_timeout: None,
        auth_rejection_time: Duration::from_millis(1),
        keys: vec![host_key],
        ..Default::default()
    });
    tokio::spawn(async move {
        let Ok((socket, _)) = listener.accept().await else {
            return;
        };
        let handler = MiniShell {
            line: Vec::new(),
            reply: None,
            password,
            exec_output,
        };
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
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("生成主机密钥");
    remember_host_key(addr, &host_key);
    let config = Arc::new(server::Config {
        inactivity_timeout: None,
        auth_rejection_time: Duration::from_millis(1),
        keys: vec![host_key],
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

/// 数连接的服务器（[`spawn_counting`]）的计数：握手了几次、现在还连着几条。
#[derive(Default)]
pub struct ServerProbe {
    accepted: std::sync::atomic::AtomicUsize,
    live: std::sync::atomic::AtomicUsize,
}

impl ServerProbe {
    /// 累计接受了多少条 TCP 连接（每条 = 一次握手 + 认证）。
    pub fn accepted(&self) -> usize {
        self.accepted.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 现在还没断开的连接数。
    pub fn live(&self) -> usize {
        self.live.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// 同 [`spawn`]，但一直接受新连接，并数握手次数和存活连接数（连接池测试用）。
#[allow(dead_code)]
pub async fn spawn_counting() -> (SocketAddr, Arc<ServerProbe>) {
    use std::sync::atomic::Ordering;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定本机端口");
    let addr = listener.local_addr().expect("取端口");
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("生成主机密钥");
    remember_host_key(addr, &host_key);
    let config = Arc::new(server::Config {
        inactivity_timeout: None,
        auth_rejection_time: Duration::from_millis(1),
        keys: vec![host_key],
        ..Default::default()
    });
    let probe = Arc::new(ServerProbe::default());
    let counted = Arc::clone(&probe);
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            counted.accepted.fetch_add(1, Ordering::SeqCst);
            counted.live.fetch_add(1, Ordering::SeqCst);
            let config = Arc::clone(&config);
            let counted = Arc::clone(&counted);
            tokio::spawn(async move {
                let handler = MiniShell {
                    line: Vec::new(),
                    reply: None,
                    password: None,
                    exec_output: None,
                };
                if let Ok(running) = server::run_stream(config, socket, handler).await {
                    let _ = running.await;
                }
                counted.live.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    (addr, probe)
}

/// 测试里的交互器：主机密钥首次信任，不问密码。
#[allow(dead_code)]
pub fn trust_once() -> Arc<dyn zssh_core::ssh::Interactor> {
    use zssh_core::ssh::{
        BoxFuture, HostKeyDecision, HostKeyInfo, Interactor, PromptAnswers, PromptRequest,
    };

    struct TrustOnce;
    impl Interactor for TrustOnce {
        fn confirm_host_key(&self, _info: HostKeyInfo) -> BoxFuture<'_, HostKeyDecision> {
            Box::pin(async { HostKeyDecision::TrustOnce })
        }
        fn prompt(&self, _request: PromptRequest) -> BoxFuture<'_, Option<PromptAnswers>> {
            Box::pin(async { None })
        }
    }
    Arc::new(TrustOnce)
}

/// 连 [`spawn`] 系列服务器的拨号链（一跳，密码任意）；`dir` 放临时的 known_hosts。
#[allow(dead_code)]
pub fn trusting_hops(addr: SocketAddr, dir: &std::path::Path) -> Vec<zssh_core::ssh::ChainHop> {
    use zssh_core::ssh::{ChainHop, KnownHosts};

    vec![ChainHop {
        label: "测试服务器".to_owned(),
        host: addr.ip().to_string(),
        port: addr.port(),
        username: "tester".to_owned(),
        proxy: None,
        auth: zssh_core::ssh::AuthConfig::password(Some("pw".to_owned())),
        known_hosts: KnownHosts::new(dir.join(format!("kh-{}", addr.port()))),
        legacy_algorithms: false,
    }]
}

/// 连上 [`spawn`] 系列起的服务器（任何密码都收，主机密钥首次信任）；`dir` 放临时的 known_hosts。
#[allow(dead_code)]
pub async fn connect_trusting(
    addr: SocketAddr,
    dir: &std::path::Path,
) -> zssh_core::ssh::connection::Connection {
    zssh_core::ssh::connection::Connection::connect_chain(trusting_hops(addr, dir), trust_once())
        .await
        .expect("连上本机测试服务器")
}

// ---------------------------------------------------------------------------
// SFTP 子系统：一棵固定的假目录树（只读），验证文件视图的首屏逻辑（家目录、起始目录、回退）。
// ---------------------------------------------------------------------------

/// [`spawn_sftp`] 的登录目录（`realpath(".")` 的结果）。
pub const SFTP_HOME: &str = "/home/tester";

/// 假目录树：目录 -> 里面的条目（名字、是不是目录）。
fn sftp_tree(dir: &str) -> Option<Vec<(&'static str, bool)>> {
    match dir {
        "/home/tester" => Some(vec![("notes.txt", false), ("work", true)]),
        "/srv/www" => Some(vec![("releases", true), ("app.tar.gz", false)]),
        "/srv/www/releases" => Some(vec![("2026-11-01", true), ("deploy.sh", false)]),
        _ => None,
    }
}

struct SftpFiles {
    /// 已经把内容读完的目录句柄：再读一次回 EOF。
    drained: std::collections::HashSet<String>,
}

impl russh_sftp::server::Handler for SftpFiles {
    type Error = russh_sftp::protocol::StatusCode;

    fn unimplemented(&self) -> Self::Error {
        russh_sftp::protocol::StatusCode::OpUnsupported
    }

    async fn init(
        &mut self,
        _version: u32,
        _extensions: std::collections::HashMap<String, String>,
    ) -> Result<russh_sftp::protocol::Version, Self::Error> {
        Ok(russh_sftp::protocol::Version::new())
    }

    async fn close(
        &mut self,
        id: u32,
        _handle: String,
    ) -> Result<russh_sftp::protocol::Status, Self::Error> {
        Ok(russh_sftp::protocol::Status {
            id,
            status_code: russh_sftp::protocol::StatusCode::Ok,
            error_message: "Ok".to_owned(),
            language_tag: "en-US".to_owned(),
        })
    }

    async fn realpath(
        &mut self,
        id: u32,
        path: String,
    ) -> Result<russh_sftp::protocol::Name, Self::Error> {
        let resolved = if path == "." || path.is_empty() {
            SFTP_HOME.to_owned()
        } else {
            path
        };
        Ok(russh_sftp::protocol::Name {
            id,
            files: vec![russh_sftp::protocol::File::dummy(resolved)],
        })
    }

    async fn opendir(
        &mut self,
        id: u32,
        path: String,
    ) -> Result<russh_sftp::protocol::Handle, Self::Error> {
        if sftp_tree(&path).is_none() {
            return Err(russh_sftp::protocol::StatusCode::NoSuchFile);
        }
        self.drained.remove(&path);
        Ok(russh_sftp::protocol::Handle { id, handle: path })
    }

    async fn readdir(
        &mut self,
        id: u32,
        handle: String,
    ) -> Result<russh_sftp::protocol::Name, Self::Error> {
        let entries = sftp_tree(&handle).ok_or(russh_sftp::protocol::StatusCode::NoSuchFile)?;
        if !self.drained.insert(handle) {
            return Err(russh_sftp::protocol::StatusCode::Eof);
        }
        let files = entries
            .into_iter()
            .map(|(name, is_dir)| {
                let attrs = russh_sftp::protocol::FileAttributes {
                    size: Some(if is_dir { 4096 } else { 1024 }),
                    permissions: Some(if is_dir { 0o040_755 } else { 0o100_644 }),
                    mtime: Some(1_730_000_000),
                    ..Default::default()
                };
                russh_sftp::protocol::File::new(name, attrs)
            })
            .collect();
        Ok(russh_sftp::protocol::Name { id, files })
    }
}

/// 接 SFTP 子系统的迷你服务器：登录目录 [`SFTP_HOME`]，另有 `/srv/www`、`/srv/www/releases` 两个目录。
struct SftpShell {
    channels: std::collections::HashMap<ChannelId, Channel<Msg>>,
}

impl server::Handler for SftpShell {
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
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Some(opened) = self.channels.remove(&channel).filter(|_| name == "sftp") else {
            return session.channel_failure(channel);
        };
        session.channel_success(channel)?;
        let handler = SftpFiles {
            drained: std::collections::HashSet::new(),
        };
        russh_sftp::server::run(opened.into_stream(), handler).await;
        Ok(())
    }
}

/// 起一个带 SFTP 子系统的服务器（接一个连接），返回监听地址。
#[allow(dead_code)]
pub async fn spawn_sftp() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定本机端口");
    let addr = listener.local_addr().expect("取端口");
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("生成主机密钥");
    remember_host_key(addr, &host_key);
    let config = Arc::new(server::Config {
        inactivity_timeout: None,
        auth_rejection_time: Duration::from_millis(1),
        keys: vec![host_key],
        ..Default::default()
    });
    tokio::spawn(async move {
        let Ok((socket, _)) = listener.accept().await else {
            return;
        };
        let handler = SftpShell {
            channels: std::collections::HashMap::new(),
        };
        if let Ok(running) = server::run_stream(config, socket, handler).await {
            let _ = running.await;
        }
    });
    addr
}
