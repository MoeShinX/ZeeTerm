use std::io;

mod reason;

pub use reason::{keys as reason_keys, Entity, HopStage, Reason};

/// 错误的稳定标识：界面按错误码挑文案，不解析 `Display` 的中文原文。
///
/// 错误码（`ssh.auth_failed`、`sftp.permission_denied` 这种）一旦发布就是契约：只能新增，
/// 不能改名——改了会让已经按旧码写好的界面文案对不上，英文界面又退回中文。
/// 参数键名同样是契约，含义和这里给的值对应（路径、主机、端口、秒数……）。
pub trait ErrorWithCode: std::fmt::Display {
    /// 稳定的错误码，形如 `sftp.permission_denied`。
    fn code(&self) -> &'static str;

    /// 结构化参数：键名稳定、顺序固定，没有参数时返回空表。
    ///
    /// 数字也转成十进制文本：界面只做"把值填进模板"，不做类型转换。
    fn params(&self) -> Vec<(&'static str, String)> {
        Vec::new()
    }

    /// 这个错误包着的内层错误（拨号链某一跳失败时是那一跳的具体原因）。
    /// 界面据此按当前语言把内层也生成整句，不用拼好的中文。
    fn cause(&self) -> Option<&dyn ErrorWithCode> {
        None
    }
}

/// `zssh-core` 的统一错误类型。
///
/// 连接失败的原因要能区分开（DNS、超时、拒绝连接、认证），因为界面要按原因
/// 给出不同的提示。
#[derive(Debug, thiserror::Error)]
pub enum ZsshError {
    #[error("快速连接串缺少用户名，格式应为 user@host[:port]")]
    QuickTargetMissingUser,

    #[error("快速连接串的主机名为空")]
    QuickTargetEmptyHost,

    #[error("快速连接串的端口非法：{0}（应为 1-65535）")]
    QuickTargetBadPort(String),

    #[error("无法解析快速连接串：{0}")]
    QuickTargetFormat(Reason),

    /// 域名解析失败。单独一类，因为此时 TCP 根本没发出去。
    #[error("无法解析主机名 {host}: {source}")]
    Dns {
        host: String,
        #[source]
        source: io::Error,
    },

    #[error("连接 {host}:{port} 超时（{seconds} 秒）")]
    ConnectTimeout {
        host: String,
        port: u16,
        seconds: u64,
    },

    /// 代理握手失败（SOCKS5 或 HTTP CONNECT）。
    #[error("代理 {proxy}：{detail}")]
    Proxy { proxy: String, detail: Reason },

    /// 拨号链里某一跳失败。报错必须写清是第几跳、连的是谁、哪一步、内层是什么错。
    #[error("第 {index} 跳（{label}）：{}失败：{source}", .stage.zh())]
    Hop {
        index: usize,
        label: String,
        stage: HopStage,
        #[source]
        source: Box<ZsshError>,
    },

    #[error("连接 {host}:{port} 失败（试过 {tried}）：{source}")]
    Connect {
        host: String,
        port: u16,
        /// 按顺序试过的地址
        tried: String,
        #[source]
        source: io::Error,
    },

    #[error("SSH 协议错误：{0}")]
    Ssh(#[from] russh::Error),

    /// 握手完成但没拿到主机密钥，正常不应该发生。
    #[error("服务器没有提供主机密钥")]
    MissingServerKey,

    /// 服务器明确拒绝了这次认证（`authenticate_password` 返回 false）。
    #[error("认证失败：用户名或密码错误")]
    AuthFailed,

    #[error("打开 shell 失败：{0}")]
    Shell(Reason),

    /// 开 shell 时 X11 转发没成功（服务器拒绝或没回复）。只在核心库内部用来触发"换一条
    /// 普通通道继续"，不会作为开终端失败交给界面。
    #[error("X11 转发没有开起来：{0}")]
    X11(#[from] crate::x11::X11Failure),

    /// 服务器没有开启 SFTP 子系统（`Subsystem sftp` 被注释掉、或者精简系统里根本没有）。
    #[error(
        "这台服务器没有开启 SFTP：sys 子系统请求被拒。请确认服务器装了 sftp 子系统（OpenSSH 的 /usr/lib/openssh/sftp-server，配置里 `Subsystem sftp` 没有被注释掉）"
    )]
    SftpUnavailable,

    /// 服务器拒绝开新通道（多半是到了 `MaxSessions` 上限）。上层据此改走专用连接。
    #[error("服务器拒绝了新的通道（可能已达 MaxSessions 上限）：{0}")]
    ChannelLimit(String),

    #[error("数据库错误：{0}")]
    Database(#[from] rusqlite::Error),

    #[error("找不到{}（id={id}）", .entity.zh())]
    NotFound { entity: Entity, id: i64 },

    #[error("输入不合法：{0}")]
    InvalidInput(Reason),

    /// 我们自己代码里的不变量被打破了（正常不该发生）。库代码不能用
    /// `unwrap()` / `expect()`，这类"理论上到不了"的分支走这里。
    #[error("内部错误：{0}")]
    Internal(Reason),

    /// 删除凭据前要提示用户先改掉这些主机。
    #[error("凭据「{name}」还被这些主机引用：{hosts}")]
    IdentityInUse { name: String, hosts: String },

    #[error("代理「{name}」还被这些主机引用：{hosts}")]
    ProxyInUse { name: String, hosts: String },

    #[error("敏感信息处理失败：{0}")]
    Secret(Reason),

    /// 用户在弹窗里点了取消（或者超时没回答）
    #[error("已取消")]
    Cancelled,

    #[error("认证超时（每一步最多 {seconds} 秒）")]
    AuthTimeout { seconds: u64 },

    #[error(
        "认证失败，试过这些方法：{tried}
服务器还支持：{remaining}"
    )]
    AllAuthFailed { tried: String, remaining: String },

    #[error("读私钥失败：{0}")]
    KeyLoad(Reason),

    #[error("ssh-agent 用不了：{0}")]
    AgentUnavailable(Reason),

    /// 服务器只接受 SHA-1 的 ssh-rsa 签名，而主机没打开旧算法兼容。
    #[error(
        "服务器只接受 SHA-1 的 ssh-rsa 签名，{key}是 RSA 密钥，默认不再用 SHA-1 签名。         可以在主机设置的\"高级\"里打开旧算法兼容，或者换用 Ed25519 / ECDSA 密钥"
    )]
    RsaSha1Refused { key: String },

    #[error("known_hosts 读写失败：{0}")]
    HostKeyStore(Reason),

    #[error("主机 {host}:{port} 的密钥被拒绝（指纹 {fingerprint}）")]
    HostKeyRejected {
        host: String,
        port: u16,
        fingerprint: String,
    },

    /// 详情放在 box 里：这个变体字段多，不装箱会把整个 `ZsshError` 撑大。
    #[error("{0}")]
    HostKeyChanged(Box<HostKeyMismatch>),

    /// 详情放在 box 里（同 `HostKeyChanged`）：字段多，不装箱会让每个 `Result<_, ZsshError>` 都很大。
    #[error("{0}")]
    HostKeyRevoked(Box<HostKeyRevokedInfo>),

    /// 同一条连接上第二次收到的主机密钥和第一次不一致（rekey）
    #[error("rekey 时主机密钥变了：第一次 {first}，这次 {again}，已断开")]
    HostKeyRekeyMismatch {
        host: String,
        port: u16,
        first: String,
        again: String,
    },

    /// 同上，详情装箱。
    #[error("{0}")]
    NoCommonAlgorithm(Box<NoCommonAlgorithmInfo>),
}

/// 错误码与结构化参数（界面按这两个来做文案，见 [trait `ErrorWithCode`](ErrorWithCode)）。
///
/// 代码只负责"这个错是什么、有哪些参数"；怎么措辞、用哪种语言，交给界面层。
impl ErrorWithCode for ZsshError {
    fn code(&self) -> &'static str {
        match self {
            ZsshError::QuickTargetMissingUser => "ssh.quick_target_missing_user",
            ZsshError::QuickTargetEmptyHost => "ssh.quick_target_empty_host",
            ZsshError::QuickTargetBadPort(_) => "ssh.quick_target_bad_port",
            ZsshError::QuickTargetFormat(_) => "ssh.quick_target_format",
            ZsshError::Dns { .. } => "ssh.dns_failed",
            ZsshError::ConnectTimeout { .. } => "ssh.connect_timeout",
            ZsshError::Proxy { .. } => "ssh.proxy_failed",
            ZsshError::Hop { .. } => "ssh.hop_failed",
            ZsshError::Connect { .. } => "ssh.connect_failed",
            ZsshError::Ssh(_) => "ssh.protocol_error",
            ZsshError::MissingServerKey => "ssh.missing_host_key",
            ZsshError::AuthFailed => "ssh.auth_failed",
            ZsshError::Shell(_) => "ssh.shell_failed",
            ZsshError::X11(_) => "ssh.x11_forward_failed",
            ZsshError::SftpUnavailable => "ssh.sftp_unavailable",
            ZsshError::ChannelLimit(_) => "ssh.channel_limit",
            ZsshError::Database(_) => "store.database_error",
            ZsshError::NotFound { .. } => "store.not_found",
            ZsshError::InvalidInput(_) => "core.invalid_input",
            ZsshError::Internal(_) => "core.internal",
            ZsshError::IdentityInUse { .. } => "store.identity_in_use",
            ZsshError::ProxyInUse { .. } => "store.proxy_in_use",
            ZsshError::Secret(_) => "store.secret_error",
            ZsshError::Cancelled => "core.cancelled",
            ZsshError::AuthTimeout { .. } => "ssh.auth_timeout",
            ZsshError::AllAuthFailed { .. } => "ssh.all_auth_failed",
            ZsshError::KeyLoad(_) => "ssh.key_load_failed",
            ZsshError::AgentUnavailable(_) => "ssh.agent_unavailable",
            ZsshError::RsaSha1Refused { .. } => "ssh.rsa_sha1_refused",
            ZsshError::HostKeyStore(_) => "ssh.known_hosts_failed",
            ZsshError::HostKeyRejected { .. } => "ssh.host_key_rejected",
            ZsshError::HostKeyChanged(_) => "ssh.host_key_changed",
            ZsshError::HostKeyRevoked(_) => "ssh.host_key_revoked",
            ZsshError::HostKeyRekeyMismatch { .. } => "ssh.host_key_rekey_mismatch",
            ZsshError::NoCommonAlgorithm(_) => "ssh.no_common_algorithm",
        }
    }

    fn params(&self) -> Vec<(&'static str, String)> {
        match self {
            ZsshError::QuickTargetMissingUser
            | ZsshError::QuickTargetEmptyHost
            | ZsshError::MissingServerKey
            | ZsshError::AuthFailed
            | ZsshError::SftpUnavailable
            | ZsshError::Cancelled => Vec::new(),

            ZsshError::QuickTargetBadPort(port) => vec![("port", port.clone())],
            ZsshError::QuickTargetFormat(reason) => reason_params(reason),
            ZsshError::Dns { host, source } => {
                vec![("host", host.clone()), ("detail", source.to_string())]
            }
            ZsshError::ConnectTimeout {
                host,
                port,
                seconds,
            } => vec![
                ("host", host.clone()),
                ("port", port.to_string()),
                ("seconds", seconds.to_string()),
            ],
            ZsshError::Proxy { proxy, detail } => {
                let mut out = vec![("proxy", proxy.clone())];
                out.extend(reason_params(detail));
                out
            }
            ZsshError::Hop {
                index,
                label,
                stage,
                source,
            } => {
                let mut out = vec![
                    ("index", index.to_string()),
                    ("label", label.clone()),
                    ("stage", stage.key().to_owned()),
                    ("detail", source.to_string()),
                ];
                if let HopStage::Tunnel { via } = stage {
                    out.push(("via", via.clone()));
                }
                out
            }
            ZsshError::Connect {
                host,
                port,
                tried,
                source,
            } => vec![
                ("host", host.clone()),
                ("port", port.to_string()),
                ("tried", tried.clone()),
                ("detail", source.to_string()),
            ],
            ZsshError::Ssh(inner) => vec![("detail", inner.to_string())],
            ZsshError::Shell(reason) => reason_params(reason),
            ZsshError::X11(failure) => vec![("detail", failure.to_string())],
            ZsshError::ChannelLimit(detail) => vec![("detail", detail.clone())],
            ZsshError::Database(err) => vec![("detail", err.to_string())],
            ZsshError::NotFound { entity, id } => {
                vec![("entity", entity.key().to_owned()), ("id", id.to_string())]
            }
            ZsshError::InvalidInput(reason)
            | ZsshError::Internal(reason)
            | ZsshError::Secret(reason)
            | ZsshError::KeyLoad(reason)
            | ZsshError::AgentUnavailable(reason)
            | ZsshError::HostKeyStore(reason) => reason_params(reason),
            ZsshError::IdentityInUse { name, hosts } | ZsshError::ProxyInUse { name, hosts } => {
                vec![("name", name.clone()), ("hosts", hosts.clone())]
            }
            ZsshError::AuthTimeout { seconds } => vec![("seconds", seconds.to_string())],
            ZsshError::AllAuthFailed { tried, remaining } => {
                vec![("tried", tried.clone()), ("remaining", remaining.clone())]
            }
            ZsshError::RsaSha1Refused { key } => vec![("key", key.clone())],
            ZsshError::HostKeyRejected {
                host,
                port,
                fingerprint,
            } => vec![
                ("host", host.clone()),
                ("port", port.to_string()),
                ("fingerprint", fingerprint.clone()),
            ],
            ZsshError::HostKeyChanged(info) => vec![
                ("host", info.host.clone()),
                ("port", info.port.to_string()),
                ("key_type", info.key_type.clone()),
                ("stored", info.stored.clone()),
                ("presented", info.presented.clone()),
                ("stored_in", info.stored_in.clone()),
                ("advice", info.advice.clone()),
            ],
            ZsshError::HostKeyRevoked(info) => vec![
                ("host", info.host.clone()),
                ("port", info.port.to_string()),
                ("stored", info.stored.clone()),
                ("presented", info.presented.clone()),
                ("stored_in", info.stored_in.clone()),
                ("advice", info.advice.clone()),
            ],
            ZsshError::HostKeyRekeyMismatch {
                host,
                port,
                first,
                again,
            } => vec![
                ("host", host.clone()),
                ("port", port.to_string()),
                ("first", first.clone()),
                ("again", again.clone()),
            ],
            ZsshError::NoCommonAlgorithm(info) => vec![
                ("host", info.host.clone()),
                ("port", info.port.to_string()),
                ("kind", info.kind.clone()),
                ("ours", info.ours.clone()),
                ("theirs", info.theirs.clone()),
            ],
        }
    }

    fn cause(&self) -> Option<&dyn ErrorWithCode> {
        match self {
            ZsshError::Hop { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

/// 带 [`Reason`] 的错误的参数：`reason` 是 key，`detail` 是中文原文（中文界面和兜底用），
/// 后面跟这条原因自己的参数。
fn reason_params(reason: &Reason) -> Vec<(&'static str, String)> {
    let mut out = vec![
        ("reason", reason.key().to_owned()),
        ("detail", reason.zh().to_owned()),
    ];
    out.extend(reason.params().iter().cloned());
    out
}

/// 主机密钥被标记为已吊销（@revoked）的详情。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKeyRevokedInfo {
    pub host: String,
    pub port: u16,
    pub stored: String,
    pub presented: String,
    pub stored_in: String,
    pub advice: String,
}

impl std::fmt::Display for HostKeyRevokedInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "主机 {}:{} 的密钥被标记为已吊销（@revoked）
记录：{}（记录在 {}）
本次收到：{}
{}",
            self.host, self.port, self.stored, self.stored_in, self.presented, self.advice
        )
    }
}

/// 和服务器协商不出共同算法的详情。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoCommonAlgorithmInfo {
    pub host: String,
    pub port: u16,
    pub kind: String,
    pub ours: String,
    pub theirs: String,
}

impl std::fmt::Display for NoCommonAlgorithmInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "和 {}:{} 协商不出共同的 {} 算法
服务器提供：{}
我们提供：{}
老服务器可以在主机设置的\"高级\"里打开旧算法兼容",
            self.host, self.port, self.kind, self.theirs, self.ours
        )
    }
}

/// 主机密钥和已保存的记录不一致时的详情。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKeyMismatch {
    pub host: String,
    pub port: u16,
    pub key_type: String,
    /// 已保存的指纹
    pub stored: String,
    /// 本次收到的指纹
    pub presented: String,
    /// 旧记录在哪儿：自己的文件，还是系统的哪个文件
    pub stored_in: String,
    /// 怎么处理：自己能清，还是在系统文件里、得用户自己执行命令
    pub advice: String,
}

impl std::fmt::Display for HostKeyMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "主机 {}:{} 的 {} 密钥指纹和已保存的不一样\n已保存：{}（记录在 {}）\n本次收到：{}\n{}",
            self.host,
            self.port,
            self.key_type,
            self.stored,
            self.stored_in,
            self.presented,
            self.advice
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// clippy 的 `result_large_err` 从 128 字节起报。这里守着这个上限：新加的成员要是
    /// 把 `ZsshError` 撑大，就先把它装箱（`HostKeyChanged`、`HostKeyRevoked`、
    /// `NoCommonAlgorithm` 就是这么处理的），而不是给整个项目关掉那条规则。
    #[test]
    fn error_type_stays_below_the_large_err_threshold() {
        let size = std::mem::size_of::<ZsshError>();
        assert!(size < 128, "ZsshError 已经 {size} 字节：大的成员要装箱");
    }

    /// 错误码是界面文案的契约：这里把**能直接构造**的变体都列出来，既守住具体取值，
    /// 也顺手查一遍不会两个变体撞同一个码（撞了界面就会挑错文案）。
    ///（`Ssh` / `Database` / `X11` 要借外部错误类型，不在这张表里。）
    #[test]
    fn every_error_variant_has_a_unique_stable_code() {
        let changed = Box::new(HostKeyMismatch {
            host: "web-1".into(),
            port: 22,
            key_type: "ssh-ed25519".into(),
            stored: "SHA256:old".into(),
            presented: "SHA256:new".into(),
            stored_in: "own".into(),
            advice: "clear".into(),
        });
        let revoked = Box::new(HostKeyRevokedInfo {
            host: "web-1".into(),
            port: 22,
            stored: "SHA256:old".into(),
            presented: "SHA256:new".into(),
            stored_in: "own".into(),
            advice: "clear".into(),
        });
        let no_common = Box::new(NoCommonAlgorithmInfo {
            host: "web-1".into(),
            port: 22,
            kind: "kex".into(),
            ours: "a".into(),
            theirs: "b".into(),
        });
        let samples: Vec<ZsshError> = vec![
            ZsshError::QuickTargetMissingUser,
            ZsshError::QuickTargetEmptyHost,
            ZsshError::QuickTargetBadPort("0".into()),
            ZsshError::QuickTargetFormat("bad".into()),
            ZsshError::Dns {
                host: "web-1".into(),
                source: io::Error::other("dns"),
            },
            ZsshError::ConnectTimeout {
                host: "web-1".into(),
                port: 22,
                seconds: 10,
            },
            ZsshError::Proxy {
                proxy: "socks5".into(),
                detail: "refused".into(),
            },
            ZsshError::Hop {
                index: 2,
                label: "jump-1".into(),
                stage: HopStage::Connect,
                source: Box::new(ZsshError::AuthFailed),
            },
            ZsshError::Connect {
                host: "web-1".into(),
                port: 22,
                tried: "10.0.0.1".into(),
                source: io::Error::other("refused"),
            },
            ZsshError::MissingServerKey,
            ZsshError::AuthFailed,
            ZsshError::Shell("closed".into()),
            ZsshError::SftpUnavailable,
            ZsshError::ChannelLimit("limit".into()),
            ZsshError::NotFound {
                entity: Entity::Host,
                id: 3,
            },
            ZsshError::InvalidInput("input".into()),
            ZsshError::Internal("bug".into()),
            ZsshError::IdentityInUse {
                name: "id".into(),
                hosts: "web-1".into(),
            },
            ZsshError::ProxyInUse {
                name: "proxy".into(),
                hosts: "web-1".into(),
            },
            ZsshError::Secret("secret".into()),
            ZsshError::Cancelled,
            ZsshError::AuthTimeout { seconds: 15 },
            ZsshError::AllAuthFailed {
                tried: "password".into(),
                remaining: "publickey".into(),
            },
            ZsshError::KeyLoad("load".into()),
            ZsshError::AgentUnavailable("agent".into()),
            ZsshError::RsaSha1Refused {
                key: "ssh-rsa".into(),
            },
            ZsshError::HostKeyStore("store".into()),
            ZsshError::HostKeyRejected {
                host: "web-1".into(),
                port: 22,
                fingerprint: "SHA256:x".into(),
            },
            ZsshError::HostKeyChanged(changed),
            ZsshError::HostKeyRevoked(revoked),
            ZsshError::HostKeyRekeyMismatch {
                host: "web-1".into(),
                port: 22,
                first: "SHA256:1".into(),
                again: "SHA256:2".into(),
            },
            ZsshError::NoCommonAlgorithm(no_common),
        ];
        let expected = [
            "ssh.quick_target_missing_user",
            "ssh.quick_target_empty_host",
            "ssh.quick_target_bad_port",
            "ssh.quick_target_format",
            "ssh.dns_failed",
            "ssh.connect_timeout",
            "ssh.proxy_failed",
            "ssh.hop_failed",
            "ssh.connect_failed",
            "ssh.missing_host_key",
            "ssh.auth_failed",
            "ssh.shell_failed",
            "ssh.sftp_unavailable",
            "ssh.channel_limit",
            "store.not_found",
            "core.invalid_input",
            "core.internal",
            "store.identity_in_use",
            "store.proxy_in_use",
            "store.secret_error",
            "core.cancelled",
            "ssh.auth_timeout",
            "ssh.all_auth_failed",
            "ssh.key_load_failed",
            "ssh.agent_unavailable",
            "ssh.rsa_sha1_refused",
            "ssh.known_hosts_failed",
            "ssh.host_key_rejected",
            "ssh.host_key_changed",
            "ssh.host_key_revoked",
            "ssh.host_key_rekey_mismatch",
            "ssh.no_common_algorithm",
        ];
        assert_eq!(samples.len(), expected.len(), "每个变体都要有断言");
        for (err, want) in samples.iter().zip(expected) {
            assert_eq!(err.code(), want, "{err:?}");
        }
        let mut codes: Vec<&str> = samples.iter().map(|err| err.code()).collect();
        let before = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), before, "错误码不能重复");
    }

    /// 带参数的错误要给界面留下结构化入口（不是让界面去解析中文）。
    #[test]
    fn parameterized_errors_expose_stable_params() {
        let err = ZsshError::ConnectTimeout {
            host: "web-1".into(),
            port: 2222,
            seconds: 8,
        };
        assert_eq!(
            err.params(),
            vec![
                ("host", "web-1".to_owned()),
                ("port", "2222".to_owned()),
                ("seconds", "8".to_owned()),
            ]
        );
        assert!(ZsshError::AuthFailed.params().is_empty());
        assert_eq!(
            ZsshError::HostKeyRejected {
                host: "web-1".into(),
                port: 22,
                fingerprint: "SHA256:x".into(),
            }
            .params(),
            vec![
                ("host", "web-1".to_owned()),
                ("port", "22".to_owned()),
                ("fingerprint", "SHA256:x".to_owned()),
            ]
        );
    }
    /// 带原因的错误：中文 `Display` 和以前逐字相同；界面要的 key 和参数在 `params()` 里。
    #[test]
    fn reasons_keep_the_chinese_display_and_expose_key_and_params() {
        let err = ZsshError::InvalidInput(
            Reason::new(reason_keys::PORT_RANGE, "端口要在 1-65535 之间").with("port", 0),
        );
        assert_eq!(err.to_string(), "输入不合法：端口要在 1-65535 之间");
        assert_eq!(err.code(), "core.invalid_input");
        assert_eq!(
            err.params(),
            vec![
                ("reason", "input.port_range".to_owned()),
                ("detail", "端口要在 1-65535 之间".to_owned()),
                ("port", "0".to_owned()),
            ]
        );
        // 没有 key 的文字（只给内部诊断用）也有 Display 和 params。
        let plain = ZsshError::Internal("不变量".into());
        assert_eq!(plain.to_string(), "内部错误：不变量");
        assert_eq!(plain.params()[0], ("reason", reason_keys::TEXT.to_owned()));
    }

    #[test]
    fn reason_keys_are_unique_and_stable_in_shape() {
        let mut keys: Vec<&str> = reason_keys::ALL.to_vec();
        let before = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), before, "reason key 不能重复");
        assert!(keys.iter().all(|key| {
            key.chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_'))
        }));
    }

    /// 拨号链某一跳：Display 和以前预拼的中文逐字相同，内层错误能从 `cause()` 取到。
    #[test]
    fn a_hop_failure_carries_the_inner_error() {
        let inner = ZsshError::AuthFailed;
        let connect = ZsshError::Hop {
            index: 1,
            label: "目标 web-1".into(),
            stage: HopStage::Connect,
            source: Box::new(ZsshError::AuthFailed),
        };
        assert_eq!(
            connect.to_string(),
            format!("第 1 跳（目标 web-1）：连接失败：{inner}")
        );
        let tunnel = ZsshError::Hop {
            index: 2,
            label: "web-2".into(),
            stage: HopStage::Tunnel {
                via: "jump-1".into(),
            },
            source: Box::new(ZsshError::Cancelled),
        };
        assert_eq!(
            tunnel.to_string(),
            "第 2 跳（web-2）：经跳板机 jump-1 打开转发通道失败：已取消"
        );
        assert_eq!(connect.cause().map(|c| c.code()), Some("ssh.auth_failed"));
        let params = tunnel.params();
        assert!(params.contains(&("stage", "tunnel".to_owned())));
        assert!(params.contains(&("via", "jump-1".to_owned())));
        assert!(ZsshError::AuthFailed.cause().is_none());
    }

    #[test]
    fn not_found_names_its_entity_by_a_stable_key() {
        let err = ZsshError::NotFound {
            entity: Entity::Host,
            id: 3,
        };
        assert_eq!(err.to_string(), "找不到主机（id=3）");
        assert_eq!(
            err.params(),
            vec![("entity", "host".to_owned()), ("id", "3".to_owned())]
        );
    }
}
