//! 联网测试共用：连测试服务器、开一条 SFTP 通道、在 `/tmp/zssh-live-*` 下建一个只属于这次
//! 测试的目录，用完（包括 panic）整个删掉。
//!
//! 只在测试里编译。服务器地址、用户、私钥路径只从环境变量读，任何一项缺了或没设
//! `ZSSH_LIVE_FULL=1` 就返回 `None`，调用方打印原因后跳过，不算失败。

use std::sync::Arc;

use zssh_core::model::ProxyKind;
use zssh_core::sftp::{RecursiveDelete, SftpChannel};
use zssh_core::ssh::{
    AuthConfig, BoxFuture, ChainHop, Connection, HostKeyDecision, HostKeyInfo, Interactor,
    KeySource, KnownHosts, PromptAnswers, PromptRequest, ProxyConfig,
};

/// 测试里没有人点对话框：主机指纹仅本次信任，其余提问一律取消。
struct TrustOnce;

impl Interactor for TrustOnce {
    fn confirm_host_key(&self, _info: HostKeyInfo) -> BoxFuture<'_, HostKeyDecision> {
        Box::pin(async { HostKeyDecision::TrustOnce })
    }

    fn prompt(&self, _request: PromptRequest) -> BoxFuture<'_, Option<PromptAnswers>> {
        Box::pin(async { None })
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn proxy_from_env() -> Option<ProxyConfig> {
    let raw = env("ZSSH_TEST_SOCKS5")?;
    let (host, port) = raw.rsplit_once(':')?;
    Some(ProxyConfig {
        kind: ProxyKind::Socks5,
        host: host.to_owned(),
        port: port.parse().ok()?,
        username: None,
        password: None,
    })
}

/// 一条连上测试服务器的 SFTP 通道，加上这次测试专用的远端目录。
pub struct LiveSftp {
    /// 持有连接，通道才不会被提前断开。
    pub connection: Arc<Connection>,
    pub channel: Arc<SftpChannel>,
    /// 远端专用目录，形如 `/tmp/zssh-live-<用例名>-<进程号>`，用例名由调用方起（带上卡号，方便按卡清理）。
    pub dir: String,
    known_hosts: std::path::PathBuf,
}

impl LiveSftp {
    /// 连接并建好专用目录。环境不满足时打印原因返回 `None`。
    pub fn connect(tag: &str) -> Option<Self> {
        if env("ZSSH_LIVE_FULL").as_deref() != Some("1") {
            println!("跳过 {tag}：没有设置 ZSSH_LIVE_FULL=1");
            return None;
        }
        let (Some(host), Some(user), Some(key)) = (
            env("ZSSH_TEST_HOST"),
            env("ZSSH_TEST_USER"),
            env("ZSSH_TEST_KEY_PATH"),
        ) else {
            println!("跳过 {tag}：缺少 ZSSH_TEST_HOST / ZSSH_TEST_USER / ZSSH_TEST_KEY_PATH");
            return None;
        };
        let port = env("ZSSH_TEST_PORT")
            .and_then(|raw| raw.parse::<u16>().ok())
            .unwrap_or(22);
        let known_hosts =
            std::env::temp_dir().join(format!("zssh-live-kh-{}-{tag}", std::process::id()));
        let hop = ChainHop {
            label: "测试服务器".to_owned(),
            host,
            port,
            username: user,
            proxy: proxy_from_env(),
            auth: AuthConfig::key(KeySource::File {
                path: key.into(),
                passphrase: env("ZSSH_TEST_KEY_PASSPHRASE"),
            }),
            known_hosts: KnownHosts::new(&known_hosts),
            legacy_algorithms: false,
        };
        let runtime = crate::ssh_backend::runtime().ok()?;
        let setup = runtime.block_on(async {
            let connection = Arc::new(
                Connection::connect_chain(vec![hop], Arc::new(TrustOnce))
                    .await
                    .map_err(|err| format!("连接测试服务器失败：{err}"))?,
            );
            let channel = Arc::new(
                SftpChannel::open(&connection)
                    .await
                    .map_err(|err| format!("开 SFTP 通道失败：{err}"))?,
            );
            let dir = format!("/tmp/zssh-live-{tag}-{}", std::process::id());
            // 上一次异常退出可能留下同名目录：先清掉再建。
            let _ = RecursiveDelete::new().run(&channel, &dir, |_| {}).await;
            channel
                .create_dir(&dir)
                .await
                .map_err(|err| format!("建专用目录失败：{err}"))?;
            Ok::<_, String>((connection, channel, dir))
        });
        match setup {
            Ok((connection, channel, dir)) => Some(Self {
                connection,
                channel,
                dir,
                known_hosts,
            }),
            Err(message) => {
                // 连不上是环境问题（共用机可能在封禁、限流），不是代码问题：跳过。
                println!("跳过 {tag}：{message}");
                let _ = std::fs::remove_file(&known_hosts);
                None
            }
        }
    }

    /// 专用目录下的路径。
    pub fn path(&self, name: &str) -> String {
        format!("{}/{name}", self.dir)
    }
}

impl Drop for LiveSftp {
    fn drop(&mut self) {
        if let Ok(runtime) = crate::ssh_backend::runtime() {
            let channel = Arc::clone(&self.channel);
            let dir = self.dir.clone();
            runtime.block_on(async move {
                let _ = RecursiveDelete::new().run(&channel, &dir, |_| {}).await;
            });
        }
        let _ = std::fs::remove_file(&self.known_hosts);
    }
}

/// 一条连上测试服务器的已认证连接，包成 `SshSessionFactory`（能开终端 shell），加上这次测试专用的
/// 远端目录。用完（包括 panic）整个删掉。环境不满足时返回 `None`，调用方打印原因后跳过。
pub struct LiveShell {
    pub connection: Arc<Connection>,
    pub factory: Arc<crate::ssh_backend::SshSessionFactory>,
    /// 远端专用目录，形如 `/tmp/zssh-live-<用例名>-<进程号>`。
    pub dir: String,
    known_hosts: std::path::PathBuf,
}

impl LiveShell {
    pub fn connect(tag: &str) -> Option<Self> {
        if env("ZSSH_LIVE_FULL").as_deref() != Some("1") {
            println!("跳过 {tag}：没有设置 ZSSH_LIVE_FULL=1");
            return None;
        }
        let (Some(host), Some(user), Some(key)) = (
            env("ZSSH_TEST_HOST"),
            env("ZSSH_TEST_USER"),
            env("ZSSH_TEST_KEY_PATH"),
        ) else {
            println!("跳过 {tag}：缺少 ZSSH_TEST_HOST / ZSSH_TEST_USER / ZSSH_TEST_KEY_PATH");
            return None;
        };
        let port = env("ZSSH_TEST_PORT")
            .and_then(|raw| raw.parse::<u16>().ok())
            .unwrap_or(22);
        let known_hosts =
            std::env::temp_dir().join(format!("zssh-live-kh-{}-{tag}", std::process::id()));
        let hop = ChainHop {
            label: "测试服务器".to_owned(),
            host,
            port,
            username: user,
            proxy: proxy_from_env(),
            auth: AuthConfig::key(KeySource::File {
                path: key.into(),
                passphrase: env("ZSSH_TEST_KEY_PASSPHRASE"),
            }),
            known_hosts: KnownHosts::new(&known_hosts),
            legacy_algorithms: false,
        };
        let runtime = crate::ssh_backend::runtime().ok()?;
        let connected = runtime.block_on(async {
            Connection::connect_chain(vec![hop], Arc::new(TrustOnce))
                .await
                .map_err(|err| format!("连接测试服务器失败：{err}"))
        });
        match connected {
            Ok(connection) => {
                let factory = crate::ssh_backend::SshSessionFactory::from_connection(connection);
                let connection = factory.connection();
                Some(Self {
                    connection,
                    factory,
                    dir: format!("/tmp/zssh-live-{tag}-{}", std::process::id()),
                    known_hosts,
                })
            }
            Err(message) => {
                // 连不上是环境问题（共用机可能在封禁、限流），不是代码问题：跳过。
                println!("跳过 {tag}：{message}");
                let _ = std::fs::remove_file(&known_hosts);
                None
            }
        }
    }
}

impl Drop for LiveShell {
    fn drop(&mut self) {
        if let Ok(runtime) = crate::ssh_backend::runtime() {
            let connection = Arc::clone(&self.connection);
            let command = format!("rm -rf '{}'", self.dir);
            runtime.block_on(async move {
                let _ = connection
                    .exec_capture(&command, std::time::Duration::from_secs(15), 4096)
                    .await;
            });
        }
        let _ = std::fs::remove_file(&self.known_hosts);
    }
}
