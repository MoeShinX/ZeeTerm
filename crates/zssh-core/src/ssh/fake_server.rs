//! 只给单元测试用的进程内 SSH 服务器：监听 127.0.0.1、端口由系统分配、主机密钥现场生成。
//!
//! 它不提供 shell，只做三件事：握手（可配多把不同类型的主机密钥）、数各种认证请求收到了几次、
//! 替跳板链做 `direct-tcpip` 转发。认证计数是为了断言"没信任的主机根本收不到密码"。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use russh::keys::ssh_key::EcdsaCurve;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::server::{self, Auth, Msg, Session};
use russh::{Channel, MethodKind, MethodSet};
use tokio::net::TcpListener;

/// 服务器各种认证请求收到的次数。
#[derive(Default)]
pub(crate) struct Counters {
    pub accepted: AtomicUsize,
    pub none: AtomicUsize,
    pub password: AtomicUsize,
    pub keyboard_interactive: AtomicUsize,
    pub publickey: AtomicUsize,
}

impl Counters {
    /// 需要发出秘密的认证请求（密码 / keyboard-interactive）一共收到几次。
    pub fn secret_attempts(&self) -> usize {
        self.password.load(Ordering::SeqCst) + self.keyboard_interactive.load(Ordering::SeqCst)
    }

    /// 任何认证请求（none / 公钥 / 密码 / 键盘交互）一共收到几次：没核对过身份的主机一次都不该收到。
    pub fn auth_requests(&self) -> usize {
        self.none.load(Ordering::SeqCst)
            + self.publickey.load(Ordering::SeqCst)
            + self.secret_attempts()
    }

    pub fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    pub fn passwords(&self) -> usize {
        self.password.load(Ordering::SeqCst)
    }
}

pub(crate) struct FakeServer {
    pub addr: SocketAddr,
    pub counters: Arc<Counters>,
    /// 服务器的主机公钥，顺序同 `spawn` 的参数。
    pub host_keys: Vec<PublicKey>,
}

/// 服务器怎么对待认证请求（默认是原来的行为：公钥一律拒、密码和键盘交互一律收）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum AuthMode {
    /// 公钥拒绝，声明还能用密码 / 键盘交互；密码和键盘交互一律接受。
    #[default]
    PasswordAccepting,
    /// 只声明公钥一种方法，公钥一律拒绝；密码类请求也拒绝（并计数）。
    PublickeyOnlyRejecting,
    /// 公钥拒绝，但声明里仍有密码；密码接受。和默认的区别是声明里带 publickey，
    /// 让客户端真的发出公钥请求再被拒。
    PublickeyRejectedPasswordAllowed,
    /// 收到公钥请求就掐断连接（模拟认证过程中掉线）。
    DropOnPublickey,
    /// 密码一律拒绝，声明还能用键盘交互；键盘交互先出一道题（模拟动态口令），答了就接受。
    PasswordRejectedKeyboardPrompts,
}

struct Handler {
    counters: Arc<Counters>,
    mode: AuthMode,
}

fn methods(mode: AuthMode) -> Option<MethodSet> {
    let kinds: &[MethodKind] = match mode {
        AuthMode::PasswordAccepting => &[MethodKind::Password, MethodKind::KeyboardInteractive],
        AuthMode::PublickeyOnlyRejecting => &[MethodKind::PublicKey],
        AuthMode::PublickeyRejectedPasswordAllowed | AuthMode::DropOnPublickey => {
            &[MethodKind::PublicKey, MethodKind::Password]
        }
        AuthMode::PasswordRejectedKeyboardPrompts => {
            &[MethodKind::Password, MethodKind::KeyboardInteractive]
        }
    };
    Some(MethodSet::from(kinds))
}

impl server::Handler for Handler {
    type Error = russh::Error;

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        self.counters.none.fetch_add(1, Ordering::SeqCst);
        Ok(Auth::Reject {
            proceed_with_methods: methods(self.mode),
            partial_success: false,
        })
    }

    async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
        self.counters.password.fetch_add(1, Ordering::SeqCst);
        if matches!(
            self.mode,
            AuthMode::PublickeyOnlyRejecting | AuthMode::PasswordRejectedKeyboardPrompts
        ) {
            return Ok(Auth::Reject {
                proceed_with_methods: methods(self.mode),
                partial_success: false,
            });
        }
        Ok(Auth::Accept)
    }

    async fn auth_publickey(&mut self, _user: &str, _key: &PublicKey) -> Result<Auth, Self::Error> {
        self.counters.publickey.fetch_add(1, Ordering::SeqCst);
        if self.mode == AuthMode::DropOnPublickey {
            return Err(russh::Error::Disconnect);
        }
        Ok(Auth::Reject {
            proceed_with_methods: methods(self.mode),
            partial_success: false,
        })
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        _user: &str,
        _submethods: &str,
        response: Option<server::Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        self.counters
            .keyboard_interactive
            .fetch_add(1, Ordering::SeqCst);
        if self.mode == AuthMode::PasswordRejectedKeyboardPrompts && response.is_none() {
            return Ok(Auth::Partial {
                name: "".into(),
                instructions: "".into(),
                prompts: vec![("Verification code: ".into(), false)].into(),
            });
        }
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

/// 一把随机的 Ed25519 主机密钥。
pub(crate) fn ed25519_key() -> PrivateKey {
    PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("生成 Ed25519 密钥")
}

/// 一把随机的 ECDSA P-256 主机密钥。
pub(crate) fn ecdsa_key() -> PrivateKey {
    PrivateKey::random(
        &mut rand::rng(),
        Algorithm::Ecdsa {
            curve: EcdsaCurve::NistP256,
        },
    )
    .expect("生成 ECDSA 密钥")
}

/// 起一台一直接受新连接的服务器，主机密钥就是 `keys`（客户端偏好哪种就出示哪种）。
pub(crate) async fn spawn(keys: Vec<PrivateKey>) -> FakeServer {
    spawn_with(keys, AuthMode::default()).await
}

/// 同 [`spawn`]，并指定认证行为。
pub(crate) async fn spawn_with(keys: Vec<PrivateKey>, mode: AuthMode) -> FakeServer {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定本机端口");
    let addr = listener.local_addr().expect("取端口");
    let host_keys = keys.iter().map(|key| key.public_key().clone()).collect();
    let config = Arc::new(server::Config {
        inactivity_timeout: None,
        auth_rejection_time: Duration::from_millis(1),
        keys,
        ..Default::default()
    });
    let counters = Arc::new(Counters::default());
    let shared = Arc::clone(&counters);
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            shared.accepted.fetch_add(1, Ordering::SeqCst);
            let config = Arc::clone(&config);
            let handler = Handler {
                counters: Arc::clone(&shared),
                mode,
            };
            tokio::spawn(async move {
                if let Ok(running) = server::run_stream(config, socket, handler).await {
                    let _ = running.await;
                }
            });
        }
    });
    FakeServer {
        addr,
        counters,
        host_keys,
    }
}
