//! 「测试连接」（V2-U13）：用表单里当前的值做一次连接测试，不必先保存。
//!
//! - **SSH**：解析地址 → 经代理 / 跳板机 TCP 连接 → 握手 → 认证 → 成功即断开。走和真实连接同一套
//!   拨号和握手代码（[`Connection::handshake_and_authenticate`]），但是**独立连接**：不进连接池、不影响
//!   已经打开的标签，也不弹任何确认框；
//! - **主机指纹**：测试里的 [`Interactor`] 对没信任的指纹（没见过 / 只存过别种类型）一律**拒绝并记下指纹**：
//!   握手做到出示指纹为止，不进行任何认证——密码、keyboard-interactive 的回答发给还没核对过的主机，
//!   中间人就能拿到明文（V2-C7a S1）。这是**有预期的主动停止**：记录绑定到本次测试、具体哪一跳、具体指纹，
//!   只有那一跳因这条记录而返回的 `HostKeyRejected` 才算「网络与 SSH 响应检测通过，登录待首次连接验证」
//!   （[`TestResultKind::Partial`]）；超时、断开、签名错误等其它错误不会被误当成通过。跳板链上哪一跳没信任，
//!   就在那一跳停下（它之后的各跳都不连）。指纹和已存的不一致 / 吊销 / 信任库读不了时按失败显示、在认证之前停止。
//!   整个过程不会调用 `KnownHosts::append / replace / remove`；
//! - **Telnet**：经代理 / 跳板机把 TCP 连上就算成功（读到第一个字节或 2 秒内没数据都算），不做自动登录；
//! - **串口**：由界面层按参数打开端口再关闭（串口驱动在 `zssh-protocols`），这里只给结果的文案；
//! - 整次测试 10 秒超时，超时的结果写明卡在哪个阶段。日志和结果里都没有密码。
//!
//! 可单测的部分（阶段划分、错误分类、结果文案）都是纯函数；联网部分用进程内迷你 SSH 服务器 / 假 Telnet
//! 设备在界面层的测试里验证。

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;

use crate::error::{reason_keys, Reason, ZsshError};
use crate::ssh::connection::{ChainHop, Connection};
use crate::ssh::dialer::{connect_direct, connect_via_proxy, BoxedStream, ProxyConfig};
use crate::ssh::interact::{
    BoxFuture, HopProgress, HostKeyDecision, HostKeyInfo, HostKeyStatus, Interactor, PromptAnswers,
    PromptKind, PromptRequest,
};

/// 整次测试的时限。
pub const TEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Telnet：连上之后最多等这么久看对方有没有先发数据。
pub const TELNET_FIRST_BYTE_WAIT: Duration = Duration::from_secs(2);

/// 测试走到了哪个阶段（失败时就是出问题的阶段；超时时是卡住的阶段）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestStage {
    /// 域名解析。
    Resolve,
    /// 建立 TCP 连接。
    Connect,
    /// 经代理连接（代理握手）。
    Proxy,
    /// 经跳板机（含跳板机自己的连接 / 认证 / 转发通道）。
    Jump,
    /// SSH 握手（协商算法）。
    Handshake,
    /// 校验主机指纹。
    HostKey,
    /// 用户认证。
    Auth,
    /// 打开串口。
    Open,
}

impl TestStage {
    /// 稳定的英文代码（界面据此出英文阶段名）。
    pub fn code(self) -> &'static str {
        match self {
            Self::Resolve => "resolve",
            Self::Connect => "connect",
            Self::Proxy => "proxy",
            Self::Jump => "jump",
            Self::Handshake => "handshake",
            Self::HostKey => "host_key",
            Self::Auth => "auth",
            Self::Open => "open",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Resolve => "域名解析",
            Self::Connect => "建立连接",
            Self::Proxy => "经代理连接",
            Self::Jump => "经跳板机",
            Self::Handshake => "SSH 握手",
            Self::HostKey => "校验主机指纹",
            Self::Auth => "认证",
            Self::Open => "打开串口",
        }
    }
}

/// 一次测试的结论。界面的颜色 / 图标只由它（加上各项检查）决定，不再由 `ok` / `stage` 这类字段的组合推断。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestResultKind {
    /// 这次检查范围内的每一项都做完并通过。
    Complete,
    /// 已做的项目通过，剩下的项目要等以后（首次正式连接、人工输入验证码……）才能验证；不是失败。
    Partial,
    /// 某一项失败。
    Failed,
    /// 用户中途取消（什么都没证明）。
    Cancelled,
}

/// 单项检查的状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckState {
    /// 检查过，通过。
    Passed,
    /// 本次没法验证，留给以后（例如首次连接时核对身份后才登录）。
    Pending,
    /// 检查过，失败。
    Failed,
    /// 不适用：这个协议没有这一项，或前面已经失败 / 停下，没有机会走到这一项。
    NotApplicable,
}

/// 服务器身份（主机密钥）的核对情况。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityState {
    /// 和已保存的记录一致。
    Matched,
    /// 没有这台主机的记录：看到了主机密钥，但没有核对过（不等于信任）。
    Unconfirmed,
    /// 和已保存的记录不一致。
    Changed,
    /// 命中了 `@revoked`。
    Revoked,
    /// 信任库读不了，无法判断。
    Unreadable,
    /// 只保存过这台主机别种类型的密钥，这次出示的类型没有记录。
    OtherKeyTypes,
}

/// 测试停下（或结束）在拨号链的哪一跳。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckedHop {
    /// 从 0 开始，在整条链（跳板机在前、目标在最后）里的位置。
    pub index: usize,
    /// 界面上这一跳的名字（「跳板机 web-1」「目标 db」）。
    pub label: String,
}

/// 一组检查的状态（构造结果和错误分类共用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checks {
    /// 网络与 SSH 响应：TCP 连得上、对方回应 SSH 并出示了主机密钥。
    pub connectivity: CheckState,
    pub identity: Option<IdentityState>,
    pub authentication: CheckState,
}

impl Checks {
    /// 连不上 / 握手没做完：网络与 SSH 响应没通过，后面的项目没有机会检查。
    const NOT_REACHED: Self = Self {
        connectivity: CheckState::Failed,
        identity: None,
        authentication: CheckState::NotApplicable,
    };

    /// 对方出示了主机密钥，但身份核对没过 / 没做：在认证之前停下。
    const fn stopped_at_identity(identity: Option<IdentityState>) -> Self {
        Self {
            connectivity: CheckState::Passed,
            identity,
            authentication: CheckState::NotApplicable,
        }
    }

    /// 身份核对通过，认证这一步失败。
    const AUTH_FAILED: Self = Self {
        connectivity: CheckState::Passed,
        identity: Some(IdentityState::Matched),
        authentication: CheckState::Failed,
    };
}

/// 一次测试的结果（界面直接显示）。
///
/// 显示等级只看 `kind`（和各项检查），由界面的一个映射函数决定；`failing_stage` 只是诊断信息，
/// 不参与着色。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestOutcome {
    pub kind: TestResultKind,
    /// 网络与 SSH 响应（Telnet：TCP 连通；串口：能打开）。
    pub connectivity: CheckState,
    /// 服务器身份；没有这一项的协议（Telnet、串口）和没走到这一步的失败是 `None`。
    pub identity: Option<IdentityState>,
    /// 登录认证。
    pub authentication: CheckState,
    /// 失败时出问题的阶段（诊断用）。
    pub failing_stage: Option<TestStage>,
    /// 停下 / 结束在哪一跳；串口和没有目标的情况是 `None`。
    pub checked_hop: Option<CheckedHop>,
    /// 停下的那一跳之后没有测试的各跳（名字，按链的顺序；Telnet 经跳板机时包含 Telnet 目标）。
    pub untested_hops: Vec<String>,
    /// 按钮栏上面那一行。
    pub reason: Reason,
    /// 「复制详情」复制的多行文字（目标、路线、阶段、耗时、指纹、完整原因；没有密码）。
    pub detail: String,
    pub elapsed_ms: u64,
}

/// SSH 测试：整条拨号链（跳板机在前，目标在最后），每一跳带着它自己的认证和 known_hosts 视图。
pub struct SshTestPlan {
    pub hops: Vec<ChainHop>,
}

/// Telnet 测试怎么到达目标。
pub enum TcpRoute {
    Direct,
    Proxy(ProxyConfig),
    /// 经这条 SSH 跳板链（最外层在前）的最后一跳去开 `direct-tcpip` 通道。
    Jump(Vec<ChainHop>),
}

/// Telnet 测试：TCP 能不能连上目标。
pub struct TcpTestPlan {
    pub host: String,
    pub port: u16,
    pub route: TcpRoute,
}

// ---- 错误分类（纯函数）----

/// 一跳里失败发生在哪一段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailurePhase {
    /// 建立 TCP / 代理连接。`via_proxy`：这一跳是经代理连的。
    Connect { via_proxy: bool },
    /// 在上一跳的 SSH 连接上开 `direct-tcpip` 转发通道。
    Forward,
    /// SSH 握手 + 指纹 + 认证。
    Handshake,
}

/// 这一跳是目标还是跳板机。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HopRole {
    Target,
    Jump,
}

/// 认证过程中服务器 / 本地想要交互输入的情况（测试不会弹窗，只记下问了什么）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Interaction {
    pub prompted: Option<PromptKind>,
    /// 提问之前，同一跳已经有凭据没成（被拒、读不了）：这次提问是认证在往下一种方法退，
    /// 不是「本来就没填」。见 [`Interactor::credential_rejected`]。
    pub after_rejection: bool,
}

/// 分类之后的失败：阶段 + 一句原因（带稳定 key 和参数，界面按语言出整句）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub stage: TestStage,
    pub message: Reason,
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 一条只有中文原文的失败（原文来自别处，已经是整句）。
fn plain(stage: TestStage, message: String) -> Failure {
    Failure {
        stage,
        message: Reason::text(message),
    }
}

/// 把一次失败分成阶段并写成一条原因。`label` 是这一跳在界面上的名字（「跳板机 web-1」「目标 db」）。
pub fn classify_failure(
    error: &ZsshError,
    phase: FailurePhase,
    role: HopRole,
    label: &str,
    interaction: Interaction,
) -> Failure {
    let inner = match phase {
        FailurePhase::Connect { via_proxy } => classify_connect(error, via_proxy),
        FailurePhase::Forward => {
            let reason = forward_reason(error);
            Failure {
                stage: TestStage::Jump,
                message: Reason::new(
                    reason_keys::CT_JUMP_FORWARD,
                    format!("跳板机转发失败：{}", reason.zh()),
                )
                .caused_by_as("reason", &reason),
            }
        }
        FailurePhase::Handshake => classify_handshake(error, interaction),
    };
    // 跳板机那一跳的问题统一挂在「经跳板机」下，说清是哪台；代理失败保持原样（和是目标还是跳板无关）。
    if role == HopRole::Jump && !matches!(inner.stage, TestStage::Proxy | TestStage::Jump) {
        return Failure {
            stage: TestStage::Jump,
            message: Reason::new(
                reason_keys::CT_LABELED,
                format!("{label}：{}", inner.message.zh()),
            )
            .with("label", label)
            .caused_by_as("inner", &inner.message),
        };
    }
    inner
}

/// 一次失败说明了哪几项检查是什么状态（和 [`classify_failure`] 看同一个错误，但不被「跳板机」前缀之类的包装影响）。
///
/// 对方出示了主机密钥之后才失败的（指纹变化 / 吊销 / 信任库读不了 / 认证失败）算「网络与 SSH 响应」通过。
pub fn failure_checks(error: &ZsshError, phase: FailurePhase) -> Checks {
    if phase != FailurePhase::Handshake {
        return Checks::NOT_REACHED;
    }
    match error {
        ZsshError::HostKeyChanged(_) => Checks::stopped_at_identity(Some(IdentityState::Changed)),
        ZsshError::HostKeyRevoked(_) => Checks::stopped_at_identity(Some(IdentityState::Revoked)),
        ZsshError::HostKeyStore(_) => Checks::stopped_at_identity(Some(IdentityState::Unreadable)),
        // 没有预期的拒绝（见 [`TestInteractor::expected_stop_for`]）：身份没核对，但也不是已知的哪种。
        ZsshError::HostKeyRejected { .. } => {
            Checks::stopped_at_identity(Some(IdentityState::Unconfirmed))
        }
        ZsshError::HostKeyRekeyMismatch { .. } => Checks::stopped_at_identity(None),
        ZsshError::AuthFailed
        | ZsshError::AllAuthFailed { .. }
        | ZsshError::AuthTimeout { .. }
        | ZsshError::KeyLoad(_)
        | ZsshError::AgentUnavailable(_)
        | ZsshError::RsaSha1Refused { .. }
        | ZsshError::Cancelled => Checks::AUTH_FAILED,
        _ => Checks::NOT_REACHED,
    }
}

fn forward_reason(error: &ZsshError) -> Reason {
    match error {
        ZsshError::InvalidInput(reason) => reason.clone(),
        ZsshError::ConnectTimeout { host, seconds, .. } => Reason::new(
            reason_keys::CT_JUMP_TIMEOUT,
            format!("经跳板机连接 {host} 超过 {seconds} 秒没有响应"),
        )
        .with("host", host)
        .with("seconds", seconds),
        other => wrapped(other),
    }
}

/// 把一个核心错误原样包成一条原因：中文是它的 `Display`（压成一行），英文由界面按它的码重新生成。
fn wrapped(error: &ZsshError) -> Reason {
    Reason::new(reason_keys::CT_WRAPPED, one_line(&error.to_string())).caused_by(error)
}

fn classify_connect(error: &ZsshError, via_proxy: bool) -> Failure {
    match error {
        ZsshError::Proxy { proxy, detail } => Failure {
            stage: TestStage::Proxy,
            message: Reason::new(
                reason_keys::CT_PROXY_FAILED,
                format!("代理失败（{proxy}）：{}", one_line(detail.zh())),
            )
            .with("proxy", proxy)
            .caused_by_as("detail", detail),
        },
        // 经代理时，能不能解析 / 连上的是代理地址本身。
        ZsshError::Dns { host, .. } if via_proxy => Failure {
            stage: TestStage::Proxy,
            message: Reason::new(
                reason_keys::CT_PROXY_DNS,
                format!("代理失败：无法解析代理地址 {host}"),
            )
            .with("host", host),
        },
        ZsshError::ConnectTimeout {
            host,
            port,
            seconds,
        } if via_proxy => Failure {
            stage: TestStage::Proxy,
            message: Reason::new(
                reason_keys::CT_PROXY_TIMEOUT,
                format!("代理失败：连接代理 {host}:{port} 超过 {seconds} 秒没有响应"),
            )
            .with("host", host)
            .with("port", port)
            .with("seconds", seconds),
        },
        ZsshError::Connect {
            host, port, source, ..
        } if via_proxy => {
            let (zh, io_reason) = io_reason(source);
            Failure {
                stage: TestStage::Proxy,
                message: Reason::new(
                    reason_keys::CT_PROXY_CONNECT,
                    format!("代理失败：连不上代理 {host}:{port}（{zh}）"),
                )
                .with("host", host)
                .with("port", port)
                .with("io_reason", io_reason),
            }
        }
        ZsshError::Dns { host, .. } => Failure {
            stage: TestStage::Resolve,
            message: Reason::new(
                reason_keys::CT_DNS,
                format!("域名解析失败：{host}（检查地址有没有写对、本机 DNS 是否正常）"),
            )
            .with("host", host),
        },
        ZsshError::ConnectTimeout {
            host,
            port,
            seconds,
        } => Failure {
            stage: TestStage::Connect,
            message: Reason::new(
                reason_keys::CT_TIMEOUT,
                format!("连接超时：{host}:{port} 超过 {seconds} 秒没有响应"),
            )
            .with("host", host)
            .with("port", port)
            .with("seconds", seconds),
        },
        ZsshError::Connect {
            host, port, source, ..
        } => Failure {
            stage: TestStage::Connect,
            message: match source.kind() {
                std::io::ErrorKind::ConnectionRefused => Reason::new(
                    reason_keys::CT_REFUSED,
                    format!("连接被拒绝：{host}:{port} 上没有程序在监听，或者被防火墙拒绝了"),
                )
                .with("host", host)
                .with("port", port),
                _ => {
                    let (zh, io_reason) = io_reason(source);
                    Reason::new(
                        reason_keys::CT_CONNECT_FAILED,
                        format!("无法连接 {host}:{port}（{zh}）"),
                    )
                    .with("host", host)
                    .with("port", port)
                    .with("io_reason", io_reason)
                }
            },
        },
        other => Failure {
            stage: TestStage::Connect,
            message: Reason::new(
                reason_keys::CT_CONNECT_OTHER,
                format!("无法建立连接：{}", one_line(&other.to_string())),
            )
            .caused_by(other),
        },
    }
}

/// 网络层原因：返回（中文说法, 给界面的参数值）。常见的几种给稳定代码（`kind:<名>`），
/// 其余是系统错误原文（跟系统语言，原样保留）。
fn io_reason(source: &std::io::Error) -> (String, String) {
    let (zh, code) = match source.kind() {
        std::io::ErrorKind::ConnectionRefused => ("连接被拒绝", "kind:refused"),
        std::io::ErrorKind::TimedOut => ("超时", "kind:timed_out"),
        std::io::ErrorKind::HostUnreachable => ("主机不可达", "kind:host_unreachable"),
        std::io::ErrorKind::NetworkUnreachable => ("网络不可达", "kind:network_unreachable"),
        std::io::ErrorKind::ConnectionReset => ("连接被重置", "kind:reset"),
        _ => {
            let text = one_line(&source.to_string());
            return (text.clone(), text);
        }
    };
    (zh.to_owned(), code.to_owned())
}

fn classify_handshake(error: &ZsshError, interaction: Interaction) -> Failure {
    let host_key = |message: Reason| Failure {
        stage: TestStage::HostKey,
        message,
    };
    let auth = |message: Reason| Failure {
        stage: TestStage::Auth,
        message,
    };
    match error {
        ZsshError::HostKeyChanged(info) => host_key(
            Reason::new(
                reason_keys::CT_KEY_CHANGED,
                format!(
                    "主机密钥与保存的不一致：已保存 {}，本次收到 {}（记录在 {}）",
                    info.stored, info.presented, info.stored_in
                ),
            )
            .with("stored", &info.stored)
            .with("presented", &info.presented)
            .with("stored_in", &info.stored_in)
            .with_all(info.source.params()),
        ),
        ZsshError::HostKeyRevoked(info) => host_key(
            Reason::new(
                reason_keys::CT_KEY_REVOKED,
                format!(
                    "主机密钥已被标记为吊销（@revoked）：记录 {}，本次收到 {}",
                    info.stored, info.presented
                ),
            )
            .with("stored", &info.stored)
            .with("presented", &info.presented),
        ),
        ZsshError::HostKeyRejected { fingerprint, .. } => host_key(
            Reason::new(
                reason_keys::CT_KEY_REJECTED,
                format!("主机密钥被拒绝（指纹 {fingerprint}）"),
            )
            .with("fingerprint", fingerprint),
        ),
        ZsshError::HostKeyRekeyMismatch { first, again, .. } => host_key(
            Reason::new(
                reason_keys::CT_KEY_REKEY,
                format!("握手中途主机密钥变了：第一次 {first}，这次 {again}"),
            )
            .with("first", first)
            .with("again", again),
        ),
        ZsshError::HostKeyStore(reason) => host_key(
            Reason::new(reason_keys::CT_WRAPPED, one_line(reason.zh())).caused_by(reason),
        ),
        ZsshError::AuthFailed => auth(Reason::new(
            reason_keys::CT_AUTH_FAILED,
            "认证失败：用户名或密码错误",
        )),
        ZsshError::AllAuthFailed {
            tried,
            remaining,
            tried_codes,
            remaining_codes,
            causes,
        } => {
            let mut reason = Reason::new(
                reason_keys::CT_AUTH_ALL,
                format!(
                    "认证失败：试过 {}，服务器还支持 {}",
                    one_line(tried),
                    one_line(remaining)
                ),
            )
            .with("tried_codes", tried_codes)
            .with("remaining_codes", remaining_codes);
            if let Some(causes) = causes {
                if let Some(agent) = &causes.agent {
                    reason = reason.caused_by_as("agent", agent);
                }
                if let Some(key) = &causes.key {
                    reason = reason.caused_by_as("key", key);
                }
            }
            auth(reason)
        }
        ZsshError::AuthTimeout { seconds } => auth(
            Reason::new(
                reason_keys::CT_AUTH_TIMEOUT,
                format!("认证超时（每一步最多 {seconds} 秒）"),
            )
            .with("seconds", seconds),
        ),
        ZsshError::KeyLoad(reason) => auth(
            Reason::new(
                reason_keys::CT_KEY_LOAD,
                format!("读私钥失败：{}", one_line(reason.zh())),
            )
            .caused_by_as("detail", reason),
        ),
        ZsshError::AgentUnavailable(reason) => auth(
            Reason::new(
                reason_keys::CT_AGENT,
                format!("ssh-agent 用不了：{}", one_line(reason.zh())),
            )
            .caused_by_as("detail", reason),
        ),
        ZsshError::RsaSha1Refused { .. } => auth(Reason::new(
            reason_keys::CT_RSA_SHA1,
            "服务器只接受 SHA-1 的 ssh-rsa 签名：换 Ed25519 / ECDSA 密钥，或打开「旧算法兼容」",
        )),
        // 配置的凭据先没成、认证退到了要人输入：失败的是那份凭据，不是「没填」。
        ZsshError::Cancelled if interaction.after_rejection && interaction.prompted.is_some() => {
            auth(Reason::new(
                reason_keys::CT_AUTH_REJECTED_THEN_PROMPT,
                "认证失败：主机配置的凭据没有被服务器接受（或读不了），服务器转而要求输入别的凭据；测试连接不会弹窗询问",
            ))
        }
        ZsshError::Cancelled => match interaction.prompted {
            Some(PromptKind::Password) => auth(Reason::new(
                reason_keys::CT_NEED_PASSWORD,
                "认证失败：需要密码，但没有填写（测试连接不会弹窗询问）",
            )),
            Some(PromptKind::Passphrase) => auth(Reason::new(
                reason_keys::CT_NEED_PASSPHRASE,
                "认证失败：私钥有口令，但没有可用的口令（测试连接不会弹窗询问）",
            )),
            Some(PromptKind::KeyboardInteractive) => auth(Reason::new(
                reason_keys::CT_NEED_INTERACTIVE,
                "服务器要求交互式验证（例如动态口令），测试连接无法代替你回答；服务器本身是可以连通的",
            )),
            None => auth(Reason::new(reason_keys::CT_CANCELLED, "已取消")),
        },
        ZsshError::NoCommonAlgorithm(info) => Failure {
            stage: TestStage::Handshake,
            message: Reason::new(
                reason_keys::CT_NO_ALGO,
                format!(
                    "和服务器协商不出共同的 {} 算法（老服务器可以试试「旧算法兼容」）",
                    info.kind
                ),
            )
            .with("kind", &info.kind),
        },
        ZsshError::ConnectTimeout { seconds, .. } => Failure {
            stage: TestStage::Handshake,
            message: Reason::new(
                reason_keys::CT_HANDSHAKE_TIMEOUT,
                format!("SSH 握手超过 {seconds} 秒没有完成"),
            )
            .with("seconds", seconds),
        },
        ZsshError::Ssh(russh::Error::Disconnect) => Failure {
            stage: TestStage::Handshake,
            message: Reason::new(
                reason_keys::CT_DISCONNECTED,
                "服务器在握手时断开了连接（地址 / 端口上跑的可能不是 SSH 服务）",
            ),
        },
        other => Failure {
            stage: TestStage::Handshake,
            message: Reason::new(
                reason_keys::CT_HANDSHAKE_OTHER,
                format!("SSH 握手失败：{}", one_line(&other.to_string())),
            )
            .caused_by(other),
        },
    }
}

// ---- 结果文案（纯函数）----

/// SSH 成功的那一行：身份核对过、登录也成功，才会走到这里。
pub fn ssh_success_headline(elapsed_ms: u64, version: Option<&str>) -> Reason {
    match version {
        Some(version) => Reason::new(
            reason_keys::CT_OK_VERSION,
            format!("连接与登录验证通过，耗时 {elapsed_ms} ms，服务器 {version}"),
        )
        .with("elapsed", elapsed_ms)
        .with("version", version),
        None => Reason::new(
            reason_keys::CT_OK,
            format!("连接与登录验证通过，耗时 {elapsed_ms} ms"),
        )
        .with("elapsed", elapsed_ms),
    }
}

/// 测试按预期停在了一个还没核对过身份的主机上（没有记录，或只存过别种类型的密钥）：已经证明有响应端
/// 出示了主机密钥，登录要等首次正式连接核对身份后才验证。`other_types`：这台主机只保存过别种类型的密钥；
/// `hop_label`：停下的是跳板机时它的名字（目标就是 `None`）。
///
/// 只说「网络与 SSH 响应检测通过」：这时只拿到了主机密钥，没有完成整个握手，也没有核对过服务器身份。
pub fn identity_pending_headline(other_types: bool, hop_label: Option<&str>) -> Reason {
    match (other_types, hop_label) {
        (false, None) => Reason::new(
            reason_keys::CT_PARTIAL_UNCONFIRMED,
            "网络与 SSH 响应检测通过，登录待首次连接验证",
        ),
        (true, None) => Reason::new(
            reason_keys::CT_PARTIAL_OTHER_TYPES,
            "网络与 SSH 响应检测通过，但服务器出示的密钥类型和已保存的不同，登录待正式连接时核对",
        ),
        (false, Some(label)) => Reason::new(
            reason_keys::CT_PARTIAL_UNCONFIRMED_JUMP,
            format!("{label}：可达，后续目标未测试（其服务器身份尚未确认，登录待首次连接验证）"),
        )
        .with("label", label),
        (true, Some(label)) => Reason::new(
            reason_keys::CT_PARTIAL_OTHER_TYPES_JUMP,
            format!(
                "{label}：可达，后续目标未测试（它出示的密钥类型和已保存的不同，登录待正式连接时核对）"
            ),
        )
        .with("label", label),
    }
}

/// 身份核对通过，但登录要密码 / 口令 / 验证码而测试连接不弹窗询问：已完成的项目通过，登录待人工验证。
/// 不是认证失败，更不能说成密码错误。
pub fn login_interactive_headline(hop_label: Option<&str>) -> Reason {
    match hop_label {
        None => Reason::new(
            reason_keys::CT_PARTIAL_INTERACTIVE,
            "网络、SSH 响应与服务器身份检测通过，登录需要交互验证（测试连接不会弹窗询问）",
        ),
        Some(label) => Reason::new(
            reason_keys::CT_PARTIAL_INTERACTIVE_JUMP,
            format!(
                "{label}：可达且身份已核对，后续目标未测试（它的登录需要交互验证，测试连接不会弹窗询问）"
            ),
        )
        .with("label", label),
    }
}

/// Telnet 成功的那一行。
pub fn tcp_success_headline(elapsed_ms: u64, first_byte: FirstByte) -> Reason {
    let (key, note) = match first_byte {
        FirstByte::Data => (reason_keys::CT_TCP_DATA, "对方已回应"),
        FirstByte::Silent => (
            reason_keys::CT_TCP_SILENT,
            "对方 2 秒内没有主动发数据，Telnet 设备常见",
        ),
        FirstByte::Closed => (reason_keys::CT_TCP_CLOSED, "但对方随即关闭了连接"),
    };
    Reason::new(
        key,
        format!("连接成功，耗时 {elapsed_ms} ms（TCP 已连通，未做登录；{note}）"),
    )
    .with("elapsed", elapsed_ms)
}

/// Telnet 连上之后对方的表现。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FirstByte {
    Data,
    Silent,
    Closed,
}

/// 这次测试涉及的整条链：每一跳（含 Telnet 经跳板机时的 Telnet 目标）的名字，按链的顺序。
/// 结果里的「停在哪一跳 / 哪些没测」都从它来。
struct Chain {
    labels: Vec<String>,
}

impl Chain {
    fn checked_hop(&self, index: Option<usize>) -> Option<CheckedHop> {
        let index = index?;
        self.labels.get(index).map(|label| CheckedHop {
            index,
            label: label.clone(),
        })
    }

    /// `index` 那一跳之后没有测试的各跳；没有停下的位置（`None`）时整条链都没测。
    fn untested_after(&self, index: Option<usize>) -> Vec<String> {
        let start = index.map_or(0, |index| index + 1);
        self.labels.iter().skip(start).cloned().collect()
    }

    /// 停下的那一跳是不是跳板机（它之后还有各跳）。
    fn is_jump_hop(&self, index: usize) -> bool {
        index + 1 < self.labels.len()
    }
}

/// 没有「停在哪一跳」可言时（串口、没有目标）的空链。
fn no_chain() -> Chain {
    Chain { labels: Vec::new() }
}

/// 一次完整通过的结果。
fn complete_outcome(
    reason: Reason,
    detail: String,
    elapsed_ms: u64,
    checks: Checks,
    chain: &Chain,
    last_hop: Option<usize>,
) -> TestOutcome {
    TestOutcome {
        kind: TestResultKind::Complete,
        connectivity: checks.connectivity,
        identity: checks.identity,
        authentication: checks.authentication,
        failing_stage: None,
        checked_hop: chain.checked_hop(last_hop),
        untested_hops: Vec::new(),
        reason,
        detail,
        elapsed_ms,
    }
}

/// 已做的项目通过、剩下的要以后验证（不是失败）。
fn partial_outcome(
    reason: Reason,
    detail: String,
    elapsed_ms: u64,
    checks: Checks,
    chain: &Chain,
    stopped_at: usize,
) -> TestOutcome {
    TestOutcome {
        kind: TestResultKind::Partial,
        connectivity: checks.connectivity,
        identity: checks.identity,
        authentication: checks.authentication,
        failing_stage: None,
        checked_hop: chain.checked_hop(Some(stopped_at)),
        untested_hops: chain.untested_after(Some(stopped_at)),
        reason,
        detail,
        elapsed_ms,
    }
}

/// 某一项失败。`failed_at`：失败发生在第几跳（不知道就 `None`）。
fn failure_outcome(
    failure: &Failure,
    detail: String,
    elapsed_ms: u64,
    checks: Checks,
    chain: &Chain,
    failed_at: Option<usize>,
) -> TestOutcome {
    TestOutcome {
        kind: TestResultKind::Failed,
        connectivity: checks.connectivity,
        identity: checks.identity,
        authentication: checks.authentication,
        failing_stage: Some(failure.stage),
        checked_hop: chain.checked_hop(failed_at),
        untested_hops: chain.untested_after(failed_at),
        // 原因本身已经带着阶段的说法（「域名解析失败」「连接被拒绝」「认证失败」），不再重复前缀。
        reason: failure.message.clone(),
        detail,
        elapsed_ms,
    }
}

/// 串口打开成功。
pub fn serial_success(device: &str, elapsed_ms: u64) -> TestOutcome {
    let reason = Reason::new(
        reason_keys::CT_SERIAL_OK,
        format!("串口 {device} 可以打开（已立即关闭），耗时 {elapsed_ms} ms"),
    )
    .with("device", device)
    .with("elapsed", elapsed_ms);
    let detail = format!("测试连接\n串口：{device}\n结果：可以打开\n耗时：{elapsed_ms} ms");
    complete_outcome(
        reason,
        detail,
        elapsed_ms,
        Checks {
            connectivity: CheckState::Passed,
            identity: None,
            authentication: CheckState::NotApplicable,
        },
        &no_chain(),
        None,
    )
}

/// 串口打开失败：`reason` 是串口驱动层分类好的中文原因（「COM3 不存在…」「COM3 被别的程序占用…」）。
pub fn serial_failure(device: &str, reason: &str, elapsed_ms: u64) -> TestOutcome {
    let failure = plain(TestStage::Open, one_line(reason));
    let detail = format!(
        "测试连接\n串口：{device}\n结果：失败（阶段：{}）\n原因：{}",
        failure.stage.label(),
        failure.message.zh()
    );
    failure_outcome(
        &failure,
        detail,
        elapsed_ms,
        Checks::NOT_REACHED,
        &no_chain(),
        None,
    )
}

/// 用户在测试中途点了取消：什么都没证明，所有项目都是「没做」。
pub fn cancelled() -> TestOutcome {
    TestOutcome {
        kind: TestResultKind::Cancelled,
        connectivity: CheckState::NotApplicable,
        identity: None,
        authentication: CheckState::NotApplicable,
        failing_stage: None,
        checked_hop: None,
        untested_hops: Vec::new(),
        reason: Reason::new(reason_keys::CT_CANCELLED_TEST, "已取消测试"),
        detail: "测试连接\n结果：已取消".to_owned(),
        elapsed_ms: 0,
    }
}

// ---- 测试用的 Interactor ----

/// 对一个没信任的指纹「按预期主动拒绝」的记录。只有同一跳、同一地址端口、同一指纹的 `HostKeyRejected`
/// 才会被当成它造成的停止（见 [`TestInteractor::expected_stop_for`]）。
#[derive(Clone)]
struct ExpectedStop {
    hop: usize,
    info: HostKeyInfo,
    /// 这台主机只存过别种类型的密钥。
    other_types: bool,
}

/// 测试过程中的观察：现在在第几跳、问过什么、按预期拒绝了哪个没信任的指纹、现在走到哪一步。
#[derive(Default)]
struct Observed {
    /// 当前正在连的是第几跳（从 0 开始）。
    hop: usize,
    /// 第一个按预期拒绝的没信任指纹（连接随即被拒绝，所以最多一个）。
    expected_stop: Option<ExpectedStop>,
    /// 谁问了需要输入的问题：（第几跳, 问的是什么）。
    prompted: Option<(usize, PromptKind)>,
    /// 哪些跳已经有凭据没成（被拒 / 读不了），见 [`Interactor::credential_rejected`]。
    rejected_hops: Vec<usize>,
    stage: Option<TestStage>,
}

/// 对任何没信任的指纹都拒绝（不写 known_hosts）；任何需要输入的提问一律取消，不弹窗。
///
/// 一个实例只服务一次测试（一个 attempt）：观察到的东西不会带到下一次测试里。
struct TestInteractor {
    seen: Mutex<Observed>,
}

impl TestInteractor {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Observed::default()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Observed> {
        self.seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn set_stage(&self, stage: TestStage) {
        self.lock().stage = Some(stage);
    }

    /// 开始连第 `hop` 跳。
    fn begin_hop(&self, hop: usize) {
        self.lock().hop = hop;
    }

    fn interaction(&self) -> Interaction {
        let seen = self.lock();
        Interaction {
            prompted: seen.prompted.map(|(_, kind)| kind),
            after_rejection: seen
                .prompted
                .is_some_and(|(hop, _)| seen.rejected_hops.contains(&hop)),
        }
    }

    /// 第 `hop` 跳的握手失败是不是「按预期的主动停止」，是的话它停在了哪个没信任的指纹。
    ///
    /// 只认两种且都要对得上记录：
    /// - 这一跳因为自己记下的没信任指纹被我们拒绝，错误是同地址、同端口、同指纹的 `HostKeyRejected`；
    /// - 这一跳问过需要输入的问题（密码 / 口令 / 验证码），被我们取消，错误是 `Cancelled`，
    ///   而且提问之前这一跳没有凭据被拒过：私钥被拒后退到问密码、存的密码不对后退到
    ///   keyboard-interactive、存的口令打不开钥匙——这些是真的认证失败，不能说成「需要交互」。
    ///
    /// 超时、断开、签名错误、别的指纹被拒绝、别的跳的提问……一律不是。
    fn expected_stop_for(&self, hop: usize, error: &ZsshError) -> Option<PendingStop> {
        let seen = self.lock();
        match error {
            ZsshError::HostKeyRejected {
                host,
                port,
                fingerprint,
            } => {
                let stop = seen.expected_stop.as_ref()?;
                (stop.hop == hop
                    && stop.info.host == *host
                    && stop.info.port == *port
                    && stop.info.fingerprint == *fingerprint)
                    .then(|| PendingStop::Unconfirmed {
                        info: stop.info.clone(),
                        other_types: stop.other_types,
                    })
            }
            ZsshError::Cancelled => match seen.prompted {
                Some((asked_hop, kind))
                    if asked_hop == hop && !seen.rejected_hops.contains(&hop) =>
                {
                    Some(PendingStop::Interactive(kind))
                }
                _ => None,
            },
            _ => None,
        }
    }
}

impl Interactor for TestInteractor {
    fn confirm_host_key(&self, info: HostKeyInfo) -> BoxFuture<'_, HostKeyDecision> {
        let decision = match info.status {
            // 没见过 / 只存过别的类型：不放行，握手到此为止，把这次拒绝记成「按预期的停止」给结果用。
            // 放行的话下一步就是把保存的密码发过去；「本次放行」不等于「核对过」。
            HostKeyStatus::Unknown | HostKeyStatus::OtherKeyTypesSaved => {
                let other_types = info.status == HostKeyStatus::OtherKeyTypesSaved;
                let mut seen = self.lock();
                let hop = seen.hop;
                seen.expected_stop.get_or_insert(ExpectedStop {
                    hop,
                    info,
                    other_types,
                });
                HostKeyDecision::Reject
            }
            // 指纹变了 / 吊销：绝不放行（也不更新已存的）。
            HostKeyStatus::Changed | HostKeyStatus::Revoked => HostKeyDecision::Reject,
        };
        Box::pin(async move { decision })
    }

    fn prompt(&self, request: PromptRequest) -> BoxFuture<'_, Option<PromptAnswers>> {
        let mut seen = self.lock();
        let hop = seen.hop;
        seen.prompted = Some((hop, request.kind));
        drop(seen);
        Box::pin(async { None })
    }

    fn hop_started(&self, _progress: HopProgress) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    fn credential_rejected(&self) {
        let mut seen = self.lock();
        let hop = seen.hop;
        if !seen.rejected_hops.contains(&hop) {
            seen.rejected_hops.push(hop);
        }
    }

    fn authentication_started(&self) -> BoxFuture<'_, ()> {
        self.set_stage(TestStage::Auth);
        Box::pin(async {})
    }
}

// ---- 拨号 ----

/// 按预期停在一个「还没法继续验证登录」的位置（不是失败）。
#[derive(Clone)]
enum PendingStop {
    /// 对方出示了主机密钥，但这台主机没有记录（或只有别种类型）：不核对身份就不往下认证。
    Unconfirmed {
        info: HostKeyInfo,
        other_types: bool,
    },
    /// 身份已核对，但认证要交互输入（密码没填、私钥口令、验证码）；测试连接不弹窗。
    Interactive(PromptKind),
}

/// 一跳没有顺利建立的原因。
enum HopStop {
    /// 第几跳、分类后的失败、各项检查状态、原始错误文字。
    Failed(usize, Failure, Checks, String),
    /// 第几跳按预期停下。
    Pending(usize, PendingStop),
}

fn fail(index: usize, failure: Failure, checks: Checks, error: &ZsshError) -> Box<HopStop> {
    fail_with(index, failure, checks, one_line(&error.to_string()))
}

fn fail_with(index: usize, failure: Failure, checks: Checks, raw: String) -> Box<HopStop> {
    Box::new(HopStop::Failed(index, failure, checks, raw))
}

/// 已经建立的各跳连接。掉出作用域（任务被取消、超时、出错）时**目标先于跳板机**释放。
struct Established(Vec<Connection>);

impl Drop for Established {
    fn drop(&mut self) {
        while self.0.pop().is_some() {}
    }
}

/// 按顺序连每一跳（只有第一跳走自己的代理，后面各跳走在前一跳的 SSH 通道里，同 `connect_chain`）。
/// `last_is_target`：最后一跳是目标（SSH 测试）还是也只是跳板机（Telnet 经跳板机测试）。
async fn connect_hops(
    hops: Vec<ChainHop>,
    last_is_target: bool,
    interactor: &Arc<TestInteractor>,
) -> Result<Established, Box<HopStop>> {
    let total = hops.len();
    let dyn_interactor: Arc<dyn Interactor> = interactor.clone();
    let mut established = Established(Vec::with_capacity(total));
    for (index, hop) in hops.into_iter().enumerate() {
        interactor.begin_hop(index);
        let role = if last_is_target && index + 1 == total {
            HopRole::Target
        } else {
            HopRole::Jump
        };
        let via_proxy = index == 0 && hop.proxy.is_some();
        interactor.set_stage(if via_proxy {
            TestStage::Proxy
        } else {
            TestStage::Connect
        });
        let stream: BoxedStream = match established.0.last() {
            None => match &hop.proxy {
                Some(proxy) => connect_via_proxy(proxy, &hop.host, hop.port)
                    .await
                    .map_err(|err| {
                        let phase = FailurePhase::Connect { via_proxy: true };
                        let failure = classify_failure(
                            &err,
                            phase,
                            role,
                            &hop.label,
                            interactor.interaction(),
                        );
                        fail(index, failure, failure_checks(&err, phase), &err)
                    })?,
                None => Box::new(connect_direct(&hop.host, hop.port).await.map_err(|err| {
                    let phase = FailurePhase::Connect { via_proxy: false };
                    let failure =
                        classify_failure(&err, phase, role, &hop.label, interactor.interaction());
                    fail(index, failure, failure_checks(&err, phase), &err)
                })?),
            },
            Some(previous) => {
                interactor.set_stage(TestStage::Jump);
                previous
                    .open_direct_tcpip(&hop.host, hop.port)
                    .await
                    .map_err(|err| {
                        let phase = FailurePhase::Forward;
                        let failure = classify_failure(
                            &err,
                            phase,
                            role,
                            &hop.label,
                            interactor.interaction(),
                        );
                        fail(index, failure, failure_checks(&err, phase), &err)
                    })?
            }
        };
        interactor.set_stage(TestStage::Handshake);
        let connection = Connection::handshake_and_authenticate(
            stream,
            &hop.host,
            hop.port,
            &hop.username,
            &hop.auth,
            hop.known_hosts.clone(),
            hop.legacy_algorithms,
            &dyn_interactor,
        )
        .await
        .map_err(|err| {
            // 先看是不是我们自己按预期造成的停止（只认这一跳、对得上记录的那一种）。
            if let Some(stop) = interactor.expected_stop_for(index, &err) {
                return Box::new(HopStop::Pending(index, stop));
            }
            let phase = FailurePhase::Handshake;
            let failure = classify_failure(&err, phase, role, &hop.label, interactor.interaction());
            fail(index, failure, failure_checks(&err, phase), &err)
        })?;
        established.0.push(connection);
    }
    Ok(established)
}

/// 某一跳没有顺利建立：按预期停下的是 Partial，其余是 Failed。
fn stop_outcome(stop: HopStop, chain: &Chain, header: &str, elapsed_ms: u64) -> TestOutcome {
    match stop {
        HopStop::Pending(hop, stop) => pending_outcome(&stop, hop, chain, header, elapsed_ms),
        HopStop::Failed(hop, failure, checks, raw) => {
            let detail = failure_detail(header, &failure, elapsed_ms, Some(&raw));
            failure_outcome(&failure, detail, elapsed_ms, checks, chain, Some(hop))
        }
    }
}

/// 一次按预期的停止的完整结果。`chain`：整条链的名字；`hop`：停在第几跳。
fn pending_outcome(
    stop: &PendingStop,
    hop: usize,
    chain: &Chain,
    header: &str,
    elapsed_ms: u64,
) -> TestOutcome {
    // 停下的是跳板机（它之后还有各跳）时标题里带它的名字；停在目标上不带。
    let hop_label = chain
        .is_jump_hop(hop)
        .then(|| chain.labels.get(hop).map(String::as_str))
        .flatten();
    let untested = chain.untested_after(Some(hop));
    let untested_line = if untested.is_empty() {
        String::new()
    } else {
        format!("\n没有测试的后续各跳：{}", untested.join("、"))
    };
    match stop {
        PendingStop::Unconfirmed { info, other_types } => {
            let reason = identity_pending_headline(*other_types, hop_label);
            let identity = if *other_types {
                IdentityState::OtherKeyTypes
            } else {
                IdentityState::Unconfirmed
            };
            let saved = if info.saved_keys.is_empty() {
                String::new()
            } else {
                let types = info
                    .saved_keys
                    .iter()
                    .map(|key| key.key_type.as_str())
                    .collect::<Vec<_>>()
                    .join("、");
                format!("\n已保存的密钥类型：{types}")
            };
            let note = if *other_types {
                "这台主机以前只保存过别种类型的密钥，这次出示的类型没有记录；可能是服务器换了密钥，也可能不是同一台服务器。测试连接没有核对它，也没有登录；请正式连接时仔细核对指纹。"
            } else {
                "测试连接只做到收到主机密钥为止，没有核对服务器身份，也不会把密码或验证码发给还没核对过的主机。首次正式连接时会让你确认指纹，确认后才验证登录。"
            };
            let detail = format!(
                "{header}\n结果：部分完成（网络与 SSH 响应检测通过，登录待验证）\n耗时：{elapsed_ms} ms\n停在：{}\n主机：{}:{}\n密钥类型：{}\n指纹：{}（已看到，尚未信任，没有保存）{saved}{untested_line}\n说明：{note}",
                chain.labels.get(hop).map_or("", String::as_str),
                info.host,
                info.port,
                info.key_type,
                info.fingerprint
            );
            partial_outcome(
                reason,
                detail,
                elapsed_ms,
                Checks {
                    connectivity: CheckState::Passed,
                    identity: Some(identity),
                    authentication: CheckState::Pending,
                },
                chain,
                hop,
            )
        }
        PendingStop::Interactive(kind) => {
            let reason = login_interactive_headline(hop_label);
            let asked = match kind {
                PromptKind::Password => "登录密码（没有填写）",
                PromptKind::Passphrase => "私钥口令（没有可用的口令）",
                PromptKind::KeyboardInteractive => "服务器的交互式验证（例如动态口令）",
            };
            let detail = format!(
                "{header}\n结果：部分完成（网络、SSH 响应与服务器身份检测通过，登录需要交互验证）\n耗时：{elapsed_ms} ms\n停在：{}\n服务器要求：{asked}{untested_line}\n说明：测试连接不会弹窗询问，也不能代替你回答；这不是密码错误，用正式连接登录即可。",
                chain.labels.get(hop).map_or("", String::as_str),
            );
            partial_outcome(
                reason,
                detail,
                elapsed_ms,
                Checks {
                    connectivity: CheckState::Passed,
                    identity: Some(IdentityState::Matched),
                    authentication: CheckState::Pending,
                },
                chain,
                hop,
            )
        }
    }
}

/// 路线的文字：`直连` / `经代理 socks5 h:p` / `经跳板机 a → b`。
fn route_text(hops: &[ChainHop]) -> String {
    let mut parts = Vec::new();
    if let Some(proxy) = hops.first().and_then(|hop| hop.proxy.as_ref()) {
        parts.push(format!("经代理 {}", proxy.label()));
    }
    let jumps: Vec<&str> = hops
        .iter()
        .take(hops.len().saturating_sub(1))
        .map(|hop| hop.label.as_str())
        .collect();
    if !jumps.is_empty() {
        parts.push(format!("经 {}", jumps.join(" → ")));
    }
    if parts.is_empty() {
        "直连".to_owned()
    } else {
        parts.join("，")
    }
}

/// 超时：卡在哪个阶段、卡在第几跳。超时不是「按预期的停止」——哪怕之前已经记过没信任的指纹。
fn timeout_failure(timeout: Duration, interactor: &TestInteractor) -> (Failure, Checks, usize) {
    let (stage, hop) = {
        let seen = interactor.lock();
        (seen.stage.unwrap_or(TestStage::Connect), seen.hop)
    };
    let failure = Failure {
        stage,
        message: Reason::new(
            reason_keys::CT_TIMEOUT_STAGE,
            format!(
                "连接测试超过 {} 秒仍没有完成（卡在：{}）",
                timeout.as_secs(),
                stage.label()
            ),
        )
        .with("seconds", timeout.as_secs())
        .with("stage", stage.code()),
    };
    let checks = match stage {
        TestStage::HostKey => Checks::stopped_at_identity(None),
        TestStage::Auth => Checks::AUTH_FAILED,
        _ => Checks::NOT_REACHED,
    };
    (failure, checks, hop)
}

/// 失败 / 超时的详情文字。
fn failure_detail(header: &str, failure: &Failure, elapsed_ms: u64, raw: Option<&str>) -> String {
    let raw = raw.map_or_else(String::new, |raw| format!("\n原始错误：{raw}"));
    format!(
        "{header}\n结果：失败（阶段：{}）\n耗时：{elapsed_ms} ms\n原因：{}{raw}",
        failure.stage.label(),
        failure.message
    )
}

/// 测试一条 SSH 链（整次 10 秒超时）。
pub async fn test_ssh(plan: SshTestPlan) -> TestOutcome {
    test_ssh_with(plan, TEST_TIMEOUT).await
}

/// 同 [`test_ssh`]，时限可调（测试里缩短）。
pub async fn test_ssh_with(plan: SshTestPlan, timeout: Duration) -> TestOutcome {
    let started = Instant::now();
    let target = plan.hops.last().map(|hop| {
        format!(
            "{}:{}（用户 {}）",
            hop.host,
            hop.port,
            if hop.username.is_empty() {
                "（空）"
            } else {
                &hop.username
            }
        )
    });
    let route = route_text(&plan.hops);
    let header = format!(
        "测试连接（SSH）\n目标：{}\n路线：{route}",
        target.unwrap_or_else(|| "（没有目标）".to_owned())
    );
    if plan.hops.is_empty() {
        let failure = Failure {
            stage: TestStage::Connect,
            message: Reason::new(reason_keys::CT_NO_TARGET, "没有要连接的目标"),
        };
        return failure_outcome(
            &failure,
            format!("{header}\n结果：失败"),
            0,
            Checks::NOT_REACHED,
            &no_chain(),
            None,
        );
    }
    let interactor = TestInteractor::new();
    let chain = Chain {
        labels: plan.hops.iter().map(|hop| hop.label.clone()).collect(),
    };
    let result = tokio::time::timeout(timeout, connect_hops(plan.hops, true, &interactor)).await;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    match result {
        Ok(Ok(established)) => {
            let version = established
                .0
                .last()
                .and_then(|connection| connection.server_version().map(str::to_owned));
            let target_fingerprint = established
                .0
                .last()
                .map(|connection| connection.server_key().fingerprint.clone());
            // 先断开再给结果：目标先关，再关跳板机。
            for connection in established.0.iter().rev() {
                let _ = connection.disconnect().await;
            }
            let last_hop = established.0.len().checked_sub(1);
            drop(established);
            let reason = ssh_success_headline(elapsed_ms, version.as_deref());
            let detail = format!(
                "{header}\n结果：连接与登录验证通过\n耗时：{elapsed_ms} ms\n服务器：{}\n指纹：{}",
                version.as_deref().unwrap_or("（没取到版本）"),
                match &target_fingerprint {
                    Some(fp) => format!("{fp}（已信任）"),
                    None => "（没取到）".to_owned(),
                }
            );
            complete_outcome(
                reason,
                detail,
                elapsed_ms,
                Checks {
                    connectivity: CheckState::Passed,
                    identity: Some(IdentityState::Matched),
                    authentication: CheckState::Passed,
                },
                &chain,
                last_hop,
            )
        }
        Ok(Err(stop)) => stop_outcome(*stop, &chain, &header, elapsed_ms),
        Err(_) => {
            let (failure, checks, hop) = timeout_failure(timeout, &interactor);
            let detail = failure_detail(&header, &failure, elapsed_ms, None);
            failure_outcome(&failure, detail, elapsed_ms, checks, &chain, Some(hop))
        }
    }
}

/// 测试 Telnet：经代理 / 跳板机把 TCP 连上就算成功。
pub async fn test_tcp(plan: TcpTestPlan) -> TestOutcome {
    test_tcp_with(plan, TEST_TIMEOUT, TELNET_FIRST_BYTE_WAIT).await
}

pub async fn test_tcp_with(
    plan: TcpTestPlan,
    timeout: Duration,
    first_byte_wait: Duration,
) -> TestOutcome {
    let started = Instant::now();
    let TcpTestPlan { host, port, route } = plan;
    let route_label = match &route {
        TcpRoute::Direct => "直连".to_owned(),
        TcpRoute::Proxy(proxy) => format!("经代理 {}", proxy.label()),
        TcpRoute::Jump(hops) => format!(
            "经跳板机 {}",
            hops.iter()
                .map(|hop| hop.label.as_str())
                .collect::<Vec<_>>()
                .join(" → ")
        ),
    };
    let header = format!("测试连接（Telnet）\n目标：{host}:{port}\n路线：{route_label}");
    // 整条链：各跳板机在前，Telnet 目标在最后（跳板机停下时「后续目标未测试」就指它）。
    let mut labels = match &route {
        TcpRoute::Jump(hops) => hops.iter().map(|hop| hop.label.clone()).collect(),
        _ => Vec::new(),
    };
    let target_index = labels.len();
    labels.push(format!("{host}:{port}"));
    let chain = Chain { labels };
    let interactor = TestInteractor::new();
    let attempt = async {
        let (stream, _keep): (BoxedStream, Option<Established>) = match route {
            TcpRoute::Direct => {
                interactor.set_stage(TestStage::Connect);
                let socket = connect_direct(&host, port).await.map_err(|err| {
                    let phase = FailurePhase::Connect { via_proxy: false };
                    fail(
                        target_index,
                        classify_failure(&err, phase, HopRole::Target, "", Interaction::default()),
                        failure_checks(&err, phase),
                        &err,
                    )
                })?;
                (Box::new(socket), None)
            }
            TcpRoute::Proxy(proxy) => {
                interactor.set_stage(TestStage::Proxy);
                let stream = connect_via_proxy(&proxy, &host, port)
                    .await
                    .map_err(|err| {
                        let phase = FailurePhase::Connect { via_proxy: true };
                        fail(
                            target_index,
                            classify_failure(
                                &err,
                                phase,
                                HopRole::Target,
                                "",
                                Interaction::default(),
                            ),
                            failure_checks(&err, phase),
                            &err,
                        )
                    })?;
                (stream, None)
            }
            TcpRoute::Jump(hops) => {
                let connections = connect_hops(hops, false, &interactor).await?;
                interactor.set_stage(TestStage::Jump);
                interactor.begin_hop(target_index);
                let originator = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
                let channel = match connections.0.last() {
                    Some(last) => last
                        .open_forward_channel(&host, port, originator)
                        .await
                        .map_err(|err| {
                            let phase = FailurePhase::Forward;
                            fail(
                                target_index,
                                classify_failure(
                                    &err,
                                    phase,
                                    HopRole::Target,
                                    "",
                                    Interaction::default(),
                                ),
                                failure_checks(&err, phase),
                                &err,
                            )
                        })?,
                    None => {
                        let err = ZsshError::InvalidInput("没有跳板机".into());
                        return Err(fail(
                            target_index,
                            Failure {
                                stage: TestStage::Jump,
                                message: Reason::new(reason_keys::CT_NO_JUMP, "没有跳板机"),
                            },
                            Checks::NOT_REACHED,
                            &err,
                        ));
                    }
                };
                (Box::new(channel), Some(connections))
            }
        };
        let mut stream = stream;
        // 连上之后看对方有没有先发数据：有、没有、立刻关掉，都算连通。
        let mut buffer = [0u8; 256];
        let first = match tokio::time::timeout(first_byte_wait, stream.read(&mut buffer)).await {
            Ok(Ok(0)) => FirstByte::Closed,
            Ok(Ok(_)) => FirstByte::Data,
            Ok(Err(err)) => {
                let message = if err.kind() == std::io::ErrorKind::ConnectionReset {
                    Reason::new(reason_keys::CT_RESET_AFTER, "连上之后被对方重置了连接")
                } else {
                    Reason::new(
                        reason_keys::CT_READ_FAILED,
                        format!("连上之后读取失败：{}", one_line(&err.to_string())),
                    )
                    .with("cause", one_line(&err.to_string()))
                };
                return Err(fail_with(
                    target_index,
                    Failure {
                        stage: TestStage::Connect,
                        message,
                    },
                    Checks::NOT_REACHED,
                    one_line(&err.to_string()),
                ));
            }
            Err(_) => FirstByte::Silent,
        };
        Ok::<_, Box<HopStop>>(first)
    };
    let result = tokio::time::timeout(timeout, attempt).await;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    match result {
        Ok(Ok(first)) => {
            let reason = tcp_success_headline(elapsed_ms, first);
            let detail = format!("{header}\n结果：连接成功\n耗时：{elapsed_ms} ms\n{reason}");
            complete_outcome(
                reason,
                detail,
                elapsed_ms,
                // Telnet 只测 TCP 连得上：没有主机密钥，也没有登录这一项。
                Checks {
                    connectivity: CheckState::Passed,
                    identity: None,
                    authentication: CheckState::NotApplicable,
                },
                &chain,
                Some(target_index),
            )
        }
        Ok(Err(stop)) => stop_outcome(*stop, &chain, &header, elapsed_ms),
        Err(_) => {
            let (failure, checks, hop) = timeout_failure(timeout, &interactor);
            let detail = failure_detail(&header, &failure, elapsed_ms, None);
            failure_outcome(&failure, detail, elapsed_ms, checks, &chain, Some(hop))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{HostKeyMismatch, HostKeyRevokedInfo, NoCommonAlgorithmInfo};

    fn refused() -> ZsshError {
        ZsshError::Connect {
            host: "192.0.2.10".into(),
            port: 22,
            tried: "192.0.2.10:22".into(),
            source: std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
        }
    }

    fn classify(error: &ZsshError, phase: FailurePhase, role: HopRole) -> Failure {
        classify_failure(error, phase, role, "跳板机 jump-1", Interaction::default())
    }

    #[test]
    fn name_resolution_refusal_and_timeout_are_told_apart() {
        let direct = FailurePhase::Connect { via_proxy: false };
        let dns = ZsshError::Dns {
            host: "no-such.example".into(),
            source: std::io::Error::other("lookup failed"),
        };
        let f = classify(&dns, direct, HopRole::Target);
        assert_eq!(f.stage, TestStage::Resolve);
        assert!(f.message.contains("域名解析失败") && f.message.contains("no-such.example"));

        let f = classify(&refused(), direct, HopRole::Target);
        assert_eq!(f.stage, TestStage::Connect);
        assert!(f.message.contains("连接被拒绝"), "{}", f.message);

        let timeout = ZsshError::ConnectTimeout {
            host: "192.0.2.10".into(),
            port: 22,
            seconds: 15,
        };
        let f = classify(&timeout, direct, HopRole::Target);
        assert_eq!(f.stage, TestStage::Connect);
        assert!(f.message.contains("连接超时") && f.message.contains("15"));

        let unreachable = ZsshError::Connect {
            host: "192.0.2.10".into(),
            port: 22,
            tried: String::new(),
            source: std::io::Error::from(std::io::ErrorKind::NetworkUnreachable),
        };
        assert!(classify(&unreachable, direct, HopRole::Target)
            .message
            .contains("网络不可达"));
    }

    #[test]
    fn proxy_problems_are_proxy_failures_whatever_the_underlying_error() {
        let via = FailurePhase::Connect { via_proxy: true };
        let handshake = ZsshError::Proxy {
            proxy: "socks5 127.0.0.1:1080".into(),
            detail: "认证被拒绝".into(),
        };
        let f = classify(&handshake, via, HopRole::Target);
        assert_eq!(f.stage, TestStage::Proxy);
        assert!(f.message.contains("代理失败") && f.message.contains("认证被拒绝"));
        // 连不上代理本身（refused / dns / timeout）也算代理失败，不能说成「目标连接被拒绝」。
        for error in [
            refused(),
            ZsshError::Dns {
                host: "proxy.example".into(),
                source: std::io::Error::other("x"),
            },
            ZsshError::ConnectTimeout {
                host: "proxy.example".into(),
                port: 1080,
                seconds: 15,
            },
        ] {
            let f = classify(&error, via, HopRole::Target);
            assert_eq!(f.stage, TestStage::Proxy, "{error}");
            assert!(f.message.starts_with("代理失败"), "{}", f.message);
        }
        // 第一跳是跳板机且它经代理：仍然是代理失败，不套「跳板机」前缀。
        let f = classify(&handshake, via, HopRole::Jump);
        assert_eq!(f.stage, TestStage::Proxy);
        assert!(!f.message.contains("跳板机 jump-1"));
    }

    #[test]
    fn a_failure_on_a_jump_host_names_the_jump_host() {
        let f = classify(
            &refused(),
            FailurePhase::Connect { via_proxy: false },
            HopRole::Jump,
        );
        assert_eq!(f.stage, TestStage::Jump);
        assert!(
            f.message.starts_with("跳板机 jump-1：连接被拒绝"),
            "{}",
            f.message
        );
        let f = classify(
            &ZsshError::AuthFailed,
            FailurePhase::Handshake,
            HopRole::Jump,
        );
        assert_eq!(f.stage, TestStage::Jump);
        assert!(f.message.contains("跳板机 jump-1") && f.message.contains("认证失败"));
        // 转发通道没开成（跳板机禁了 TCP 转发）
        let forward = ZsshError::InvalidInput("服务器禁止了 TCP 转发".into());
        let f = classify(&forward, FailurePhase::Forward, HopRole::Target);
        assert_eq!(f.stage, TestStage::Jump);
        assert!(f.message.contains("跳板机转发失败") && f.message.contains("禁止了 TCP 转发"));
    }

    #[test]
    fn host_key_problems_are_host_key_failures_with_both_fingerprints() {
        let changed = ZsshError::HostKeyChanged(Box::new(HostKeyMismatch {
            host: "192.0.2.10".into(),
            port: 22,
            key_type: "ssh-ed25519".into(),
            stored: "SHA256:old".into(),
            presented: "SHA256:new".into(),
            stored_in: "ZeeTerm 自己的 known_hosts".into(),
            advice: "x".into(),
            source: crate::error::HostKeySource::Own,
        }));
        let f = classify(&changed, FailurePhase::Handshake, HopRole::Target);
        assert_eq!(f.stage, TestStage::HostKey);
        assert!(f.message.contains("主机密钥与保存的不一致"));
        assert!(f.message.contains("SHA256:old") && f.message.contains("SHA256:new"));
        let revoked = ZsshError::HostKeyRevoked(Box::new(HostKeyRevokedInfo {
            host: "h".into(),
            port: 22,
            stored: "SHA256:old".into(),
            presented: "SHA256:new".into(),
            stored_in: "f".into(),
            advice: "x".into(),
            source: crate::error::HostKeySource::Own,
        }));
        assert!(classify(&revoked, FailurePhase::Handshake, HopRole::Target)
            .message
            .contains("吊销"));
    }

    #[test]
    fn authentication_failures_say_what_was_wrong() {
        let f = classify(
            &ZsshError::AuthFailed,
            FailurePhase::Handshake,
            HopRole::Target,
        );
        assert_eq!(f.stage, TestStage::Auth);
        assert_eq!(f.message, "认证失败：用户名或密码错误");
        let all = ZsshError::AllAuthFailed {
            tried: "publickey, password".into(),
            remaining: "keyboard-interactive".into(),
            tried_codes: String::new(),
            remaining_codes: String::new(),
            causes: None,
        };
        let f = classify(&all, FailurePhase::Handshake, HopRole::Target);
        assert!(
            f.message.contains("publickey, password") && f.message.contains("keyboard-interactive")
        );
        // 测试连接不弹窗：要密码 / 口令 / 二次验证时按问的是什么说明。
        let cancelled = |kind| {
            classify_failure(
                &ZsshError::Cancelled,
                FailurePhase::Handshake,
                HopRole::Target,
                "",
                Interaction {
                    prompted: kind,
                    after_rejection: false,
                },
            )
        };
        assert!(cancelled(Some(PromptKind::Password))
            .message
            .contains("需要密码"));
        assert!(cancelled(Some(PromptKind::Passphrase))
            .message
            .contains("口令"));
        assert!(cancelled(Some(PromptKind::KeyboardInteractive))
            .message
            .contains("动态口令"));
        assert_eq!(cancelled(None).message, "已取消");
        let algo = ZsshError::NoCommonAlgorithm(Box::new(NoCommonAlgorithmInfo {
            host: "h".into(),
            port: 22,
            kind: "Kex".into(),
            ours: "a".into(),
            theirs: "b".into(),
        }));
        let f = classify(&algo, FailurePhase::Handshake, HopRole::Target);
        assert_eq!(f.stage, TestStage::Handshake);
        assert!(f.message.contains("旧算法兼容"));
        // 不是 SSH 服务
        let f = classify(
            &ZsshError::Ssh(russh::Error::Disconnect),
            FailurePhase::Handshake,
            HopRole::Target,
        );
        assert_eq!(f.stage, TestStage::Handshake);
        assert!(f.message.contains("不是 SSH"));
    }

    #[test]
    fn headlines_for_every_kind_of_success() {
        assert_eq!(
            ssh_success_headline(123, Some("SSH-2.0-OpenSSH_9.6")),
            "连接与登录验证通过，耗时 123 ms，服务器 SSH-2.0-OpenSSH_9.6"
        );
        assert_eq!(
            ssh_success_headline(5, None),
            "连接与登录验证通过，耗时 5 ms"
        );
        let first = identity_pending_headline(false, None);
        assert_eq!(first, "网络与 SSH 响应检测通过，登录待首次连接验证");
        let other = identity_pending_headline(true, None);
        assert!(
            other.contains("密钥类型") && other.contains("登录待"),
            "{other}"
        );
        assert!(
            !other.contains("首次"),
            "只存过别种类型的不能当成首次：{other}"
        );
        let jump = identity_pending_headline(false, Some("跳板机 edge-1"));
        assert!(
            jump.starts_with("跳板机 edge-1：可达，后续目标未测试"),
            "{jump}"
        );
        let interactive = login_interactive_headline(None);
        assert!(interactive.contains("登录需要交互验证"), "{interactive}");
        assert!(
            !interactive.contains("密码错误") && !interactive.contains("失败"),
            "{interactive}"
        );
        assert!(login_interactive_headline(Some("跳板机 edge-1")).contains("后续目标未测试"));
        assert_eq!(
            tcp_success_headline(12, FirstByte::Silent),
            "连接成功，耗时 12 ms（TCP 已连通，未做登录；对方 2 秒内没有主动发数据，Telnet 设备常见）"
        );
        assert!(tcp_success_headline(12, FirstByte::Data).contains("对方已回应"));
        assert!(tcp_success_headline(12, FirstByte::Closed).contains("随即关闭"));
    }

    #[test]
    fn a_failure_headline_is_the_reason_and_keeps_the_stage() {
        let f = Failure {
            stage: TestStage::Auth,
            message: "认证失败：用户名或密码错误".into(),
        };
        let outcome = failure_outcome(&f, "d".into(), 7, Checks::AUTH_FAILED, &no_chain(), None);
        assert_eq!(outcome.kind, TestResultKind::Failed);
        assert_eq!(outcome.failing_stage, Some(TestStage::Auth));
        assert_eq!(outcome.reason, "认证失败：用户名或密码错误");
        assert_eq!(outcome.authentication, CheckState::Failed);
        assert_eq!(outcome.identity, Some(IdentityState::Matched));
    }

    #[test]
    fn serial_results_name_the_device_and_the_reason() {
        let ok = serial_success("COM3", 12);
        assert_eq!(ok.kind, TestResultKind::Complete);
        assert!(ok.reason.contains("COM3 可以打开"));
        let bad = serial_failure("COM3", "COM3 不存在：设备没有插好，或者设备名写错了", 3);
        assert_eq!(bad.kind, TestResultKind::Failed);
        assert_eq!(bad.failing_stage, Some(TestStage::Open));
        assert!(bad.reason.contains("不存在"));
        assert!(bad.detail.contains("串口：COM3"));
        let busy = serial_failure("COM3", "COM3 被别的程序占用（或者没有访问权限）", 3);
        assert!(busy.reason.contains("被别的程序占用"));
    }

    #[test]
    fn a_cancelled_test_proves_nothing() {
        let outcome = cancelled();
        assert_eq!(outcome.kind, TestResultKind::Cancelled);
        assert_eq!(outcome.connectivity, CheckState::NotApplicable);
        assert_eq!(outcome.authentication, CheckState::NotApplicable);
        assert_eq!(outcome.identity, None);
        assert_eq!(outcome.failing_stage, None);
    }

    #[test]
    fn failure_checks_follow_how_far_the_handshake_got() {
        let handshake = FailurePhase::Handshake;
        let changed = ZsshError::HostKeyChanged(Box::new(HostKeyMismatch {
            host: "h".into(),
            port: 22,
            key_type: "ssh-ed25519".into(),
            stored: "a".into(),
            presented: "b".into(),
            stored_in: "f".into(),
            advice: String::new(),
            source: crate::error::HostKeySource::Own,
        }));
        let checks = failure_checks(&changed, handshake);
        assert_eq!(checks.identity, Some(IdentityState::Changed));
        assert_eq!(checks.connectivity, CheckState::Passed);
        assert_eq!(checks.authentication, CheckState::NotApplicable);
        let revoked = ZsshError::HostKeyRevoked(Box::new(HostKeyRevokedInfo {
            host: "h".into(),
            port: 22,
            stored: "a".into(),
            presented: "b".into(),
            stored_in: "f".into(),
            advice: String::new(),
            source: crate::error::HostKeySource::Own,
        }));
        assert_eq!(
            failure_checks(&revoked, handshake).identity,
            Some(IdentityState::Revoked)
        );
        let unreadable = ZsshError::HostKeyStore(Reason::text("x"));
        assert_eq!(
            failure_checks(&unreadable, handshake).identity,
            Some(IdentityState::Unreadable)
        );
        // 认证失败：身份已经核对过。
        assert_eq!(
            failure_checks(&ZsshError::AuthFailed, handshake),
            Checks::AUTH_FAILED
        );
        // 握手没做完 / 网络层失败：什么都没证明。
        for error in [
            ZsshError::Ssh(russh::Error::Disconnect),
            ZsshError::ConnectTimeout {
                host: "h".into(),
                port: 22,
                seconds: 15,
            },
        ] {
            assert_eq!(failure_checks(&error, handshake), Checks::NOT_REACHED);
        }
        assert_eq!(
            failure_checks(&refused(), FailurePhase::Connect { via_proxy: false }),
            Checks::NOT_REACHED
        );
    }

    #[test]
    fn the_route_text_describes_direct_proxy_and_jump_chains() {
        use crate::model::ProxyKind;
        use crate::ssh::auth::AuthConfig;
        use crate::ssh::known_hosts::KnownHosts;
        let hop = |label: &str, proxy: Option<ProxyConfig>| ChainHop {
            label: label.to_owned(),
            host: "192.0.2.1".to_owned(),
            port: 22,
            username: "u".to_owned(),
            proxy,
            auth: AuthConfig::agent(),
            known_hosts: KnownHosts::new(std::env::temp_dir().join("zssh-test-unused")),
            legacy_algorithms: false,
        };
        let proxy = ProxyConfig {
            kind: ProxyKind::Socks5,
            host: "127.0.0.1".into(),
            port: 1080,
            username: None,
            password: Some("never-shown".into()),
        };
        assert_eq!(route_text(&[hop("目标 a", None)]), "直连");
        assert_eq!(
            route_text(&[hop("目标 a", Some(proxy.clone()))]),
            "经代理 socks5 127.0.0.1:1080"
        );
        let text = route_text(&[
            hop("跳板机 j1", Some(proxy)),
            hop("跳板机 j2", None),
            hop("目标 a", None),
        ]);
        assert_eq!(
            text,
            "经代理 socks5 127.0.0.1:1080，经 跳板机 j1 → 跳板机 j2"
        );
        assert!(!text.contains("never-shown"));
    }

    #[test]
    fn the_test_interactor_rejects_untrusted_keys_and_never_answers_prompts() {
        let interactor = TestInteractor::new();
        let info = |status| HostKeyInfo {
            host: "192.0.2.10".into(),
            port: 22,
            key_type: "ssh-ed25519".into(),
            fingerprint: "SHA256:abc".into(),
            status,
            stored_fingerprint: None,
            saved_keys: Vec::new(),
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("运行时");
        rt.block_on(async {
            assert_eq!(
                interactor
                    .confirm_host_key(info(HostKeyStatus::Unknown))
                    .await,
                HostKeyDecision::Reject,
                "没见过：不放行，放行就会接着把密码发过去"
            );
            assert_eq!(
                interactor
                    .confirm_host_key(info(HostKeyStatus::OtherKeyTypesSaved))
                    .await,
                HostKeyDecision::Reject
            );
            for status in [HostKeyStatus::Changed, HostKeyStatus::Revoked] {
                assert_eq!(
                    interactor.confirm_host_key(info(status)).await,
                    HostKeyDecision::Reject
                );
            }
            let answer = interactor
                .prompt(PromptRequest::single(
                    PromptKind::Password,
                    "ops@web-1:22",
                    "Password:",
                    true,
                ))
                .await;
            assert!(answer.is_none());
        });
        let seen = interactor.lock();
        let stop = seen
            .expected_stop
            .as_ref()
            .expect("记下了第一个没信任的指纹");
        assert_eq!(stop.info.status, HostKeyStatus::Unknown);
        assert!(!stop.other_types, "记的是第一个，不被后面的覆盖");
        assert_eq!(seen.prompted, Some((0, PromptKind::Password)));
    }

    #[test]
    fn a_hung_stage_is_named_in_the_timeout_message() {
        let interactor = TestInteractor::new();
        interactor.set_stage(TestStage::Handshake);
        let (f, checks, hop) = timeout_failure(Duration::from_secs(10), &interactor);
        assert_eq!(f.stage, TestStage::Handshake);
        assert!(f.message.contains("10 秒") && f.message.contains("SSH 握手"));
        assert_eq!(checks, Checks::NOT_REACHED);
        assert_eq!(hop, 0);
    }

    fn rejected(host: &str, port: u16, fingerprint: &str) -> ZsshError {
        ZsshError::HostKeyRejected {
            host: host.into(),
            port,
            fingerprint: fingerprint.into(),
        }
    }

    /// 「按预期的停止」只认对得上记录的那一种：同一跳、同地址端口、同指纹的拒绝；
    /// 其它任何错误（哪怕这次测试里确实拒绝过一个指纹）都不能被当成通过。
    #[test]
    fn only_the_rejection_we_caused_counts_as_an_expected_stop() {
        let interactor = TestInteractor::new();
        interactor.begin_hop(1);
        let info = HostKeyInfo {
            host: "192.0.2.10".into(),
            port: 22,
            key_type: "ssh-ed25519".into(),
            fingerprint: "SHA256:abc".into(),
            status: HostKeyStatus::Unknown,
            stored_fingerprint: None,
            saved_keys: Vec::new(),
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("运行时");
        rt.block_on(interactor.confirm_host_key(info));

        // 对得上：是。
        assert!(matches!(
            interactor.expected_stop_for(1, &rejected("192.0.2.10", 22, "SHA256:abc")),
            Some(PendingStop::Unconfirmed {
                other_types: false,
                ..
            })
        ));
        // 别的跳 / 别的地址 / 别的端口 / 别的指纹的拒绝：不是。
        for (hop, error) in [
            (0, rejected("192.0.2.10", 22, "SHA256:abc")),
            (1, rejected("192.0.2.11", 22, "SHA256:abc")),
            (1, rejected("192.0.2.10", 2222, "SHA256:abc")),
            (1, rejected("192.0.2.10", 22, "SHA256:other")),
        ] {
            assert!(interactor.expected_stop_for(hop, &error).is_none());
        }
        // 超时、断开、握手别的错误、签名之类：就算记录还在也不是。
        for error in [
            ZsshError::ConnectTimeout {
                host: "192.0.2.10".into(),
                port: 22,
                seconds: 15,
            },
            ZsshError::Ssh(russh::Error::Disconnect),
            ZsshError::AuthFailed,
            ZsshError::HostKeyRekeyMismatch {
                host: "192.0.2.10".into(),
                port: 22,
                first: "a".into(),
                again: "b".into(),
            },
        ] {
            assert!(interactor.expected_stop_for(1, &error).is_none(), "{error}");
        }
        // 没问过任何问题时，Cancelled 也不是预期的停止。
        assert!(interactor
            .expected_stop_for(1, &ZsshError::Cancelled)
            .is_none());
        // 一个新的测试（新的 interactor）带不走旧测试的记录。
        let fresh = TestInteractor::new();
        fresh.begin_hop(1);
        assert!(fresh
            .expected_stop_for(1, &rejected("192.0.2.10", 22, "SHA256:abc"))
            .is_none());
    }

    #[test]
    fn a_cancelled_prompt_is_expected_only_on_the_hop_that_asked() {
        let interactor = TestInteractor::new();
        interactor.begin_hop(2);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("运行时");
        rt.block_on(interactor.prompt(PromptRequest::single(
            PromptKind::Passphrase,
            "ops@web-1:22",
            "Passphrase:",
            false,
        )));
        assert!(matches!(
            interactor.expected_stop_for(2, &ZsshError::Cancelled),
            Some(PendingStop::Interactive(PromptKind::Passphrase))
        ));
        assert!(interactor
            .expected_stop_for(1, &ZsshError::Cancelled)
            .is_none());
    }
}

#[cfg(test)]
mod untrusted_host_tests {
    //! 「测试连接」不能把凭据发给没核对过身份的主机（V2-C7a S1），并且把结果说准确：
    //! 用进程内迷你 SSH 服务器数认证请求、比较信任文件的字节。

    use russh::keys::HashAlg;

    use super::*;
    use crate::ssh::auth::AuthConfig;
    use crate::ssh::fake_server::{self, AuthMode, FakeServer};
    use crate::ssh::known_hosts::KnownHosts;

    const PASSWORD: &str = "never-leave-this-machine";

    fn hop(label: &str, server: &FakeServer, known: &std::path::Path) -> ChainHop {
        hop_with(
            label,
            server,
            known,
            AuthConfig::password(Some(PASSWORD.to_owned())),
        )
    }

    fn hop_with(
        label: &str,
        server: &FakeServer,
        known: &std::path::Path,
        auth: AuthConfig,
    ) -> ChainHop {
        ChainHop {
            label: label.to_owned(),
            host: server.addr.ip().to_string(),
            port: server.addr.port(),
            username: "tester".to_owned(),
            proxy: None,
            auth,
            known_hosts: KnownHosts::new(known),
            legacy_algorithms: false,
        }
    }

    fn trust(server: &FakeServer, known: &std::path::Path) {
        KnownHosts::new(known)
            .append(
                &server.addr.ip().to_string(),
                server.addr.port(),
                &server.host_keys[0],
            )
            .expect("预先信任");
    }

    fn fingerprint(server: &FakeServer) -> String {
        server.host_keys[0].fingerprint(HashAlg::Sha256).to_string()
    }

    fn known_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("known_hosts")
    }

    #[tokio::test]
    async fn an_unknown_host_is_a_partial_result_and_gets_no_authentication_at_all() {
        let server = fake_server::spawn(vec![fake_server::ed25519_key()]).await;
        let dir = tempfile::tempdir().expect("临时目录");
        let known = known_path(&dir);
        let outcome = test_ssh(SshTestPlan {
            hops: vec![hop("目标 lab-1", &server, &known)],
        })
        .await;
        assert_eq!(
            server.counters.auth_requests(),
            0,
            "未知指纹阶段：none / 公钥 / 密码 / 键盘交互一个请求都不能发"
        );
        assert_eq!(outcome.kind, TestResultKind::Partial, "{}", outcome.reason);
        assert_eq!(outcome.connectivity, CheckState::Passed);
        assert_eq!(outcome.identity, Some(IdentityState::Unconfirmed));
        assert_eq!(outcome.authentication, CheckState::Pending);
        assert_eq!(outcome.failing_stage, None, "不是失败，没有出问题的阶段");
        assert_eq!(
            outcome.reason,
            "网络与 SSH 响应检测通过，登录待首次连接验证"
        );
        let hop = outcome.checked_hop.as_ref().expect("停在目标上");
        assert_eq!((hop.index, hop.label.as_str()), (0, "目标 lab-1"));
        assert!(outcome.untested_hops.is_empty());
        // 详情里有指纹、密钥类型，写明「已看到」而不是「已信任」，没有保存，也没有密码。
        assert!(outcome.detail.contains(&fingerprint(&server)));
        assert!(outcome.detail.contains("ssh-ed25519"), "{}", outcome.detail);
        assert!(outcome.detail.contains("尚未信任，没有保存"));
        assert!(!outcome.detail.contains(PASSWORD));
        assert!(!known.exists(), "指纹也不能顺手保存");
    }

    #[tokio::test]
    async fn a_host_that_only_has_other_key_types_saved_waits_for_a_check_and_is_not_a_first_time()
    {
        let server = fake_server::spawn(vec![fake_server::ed25519_key()]).await;
        let dir = tempfile::tempdir().expect("临时目录");
        let known = known_path(&dir);
        // 这台主机以前只存过 ECDSA；服务器这回出示的是 Ed25519（中间人最爱用的情形）。
        let stored = fake_server::ecdsa_key();
        KnownHosts::new(&known)
            .append(
                &server.addr.ip().to_string(),
                server.addr.port(),
                stored.public_key(),
            )
            .expect("预先保存");
        let before = std::fs::read(&known).expect("读");
        let outcome = test_ssh(SshTestPlan {
            hops: vec![hop("目标 lab-1", &server, &known)],
        })
        .await;
        assert_eq!(server.counters.auth_requests(), 0);
        assert_eq!(outcome.kind, TestResultKind::Partial);
        assert_eq!(outcome.identity, Some(IdentityState::OtherKeyTypes));
        assert_eq!(outcome.authentication, CheckState::Pending);
        assert!(outcome.reason.contains("登录待"), "{}", outcome.reason);
        assert!(
            !outcome.reason.contains("首次"),
            "只存过别种类型，不能说成首次：{}",
            outcome.reason
        );
        assert!(
            outcome.detail.contains("ecdsa-sha2-nistp256"),
            "详情里列出已保存的类型：{}",
            outcome.detail
        );
        assert_eq!(
            std::fs::read(&known).expect("读"),
            before,
            "不静默接受新类型"
        );
    }

    #[tokio::test]
    async fn a_trusted_host_is_tested_for_real_and_is_complete() {
        let server = fake_server::spawn(vec![fake_server::ed25519_key()]).await;
        let dir = tempfile::tempdir().expect("临时目录");
        let known = known_path(&dir);
        trust(&server, &known);
        let before = std::fs::read(&known).expect("读");
        let outcome = test_ssh(SshTestPlan {
            hops: vec![hop("目标 lab-1", &server, &known)],
        })
        .await;
        assert_eq!(outcome.kind, TestResultKind::Complete, "{}", outcome.reason);
        assert_eq!(outcome.connectivity, CheckState::Passed);
        assert_eq!(outcome.identity, Some(IdentityState::Matched));
        assert_eq!(outcome.authentication, CheckState::Passed);
        assert!(outcome.reason.contains("连接与登录验证通过"));
        assert_eq!(server.counters.passwords(), 1, "已信任：照常用密码登录");
        assert!(outcome.detail.contains("已信任"), "{}", outcome.detail);
        assert_eq!(std::fs::read(&known).expect("读"), before);
    }

    #[tokio::test]
    async fn a_trusted_host_that_refuses_the_login_is_a_failure_in_the_auth_stage() {
        let server = fake_server::spawn_with(
            vec![fake_server::ed25519_key()],
            AuthMode::PublickeyOnlyRejecting,
        )
        .await;
        let dir = tempfile::tempdir().expect("临时目录");
        let known = known_path(&dir);
        trust(&server, &known);
        let outcome = test_ssh(SshTestPlan {
            hops: vec![hop("目标 lab-1", &server, &known)],
        })
        .await;
        assert_eq!(outcome.kind, TestResultKind::Failed, "{}", outcome.reason);
        assert_eq!(outcome.failing_stage, Some(TestStage::Auth));
        assert_eq!(outcome.identity, Some(IdentityState::Matched));
        assert_eq!(outcome.authentication, CheckState::Failed);
        assert_eq!(outcome.connectivity, CheckState::Passed);
    }

    #[tokio::test]
    async fn a_missing_password_on_a_trusted_host_is_partial_interactive_not_a_wrong_password() {
        let server = fake_server::spawn(vec![fake_server::ed25519_key()]).await;
        let dir = tempfile::tempdir().expect("临时目录");
        let known = known_path(&dir);
        trust(&server, &known);
        let outcome = test_ssh(SshTestPlan {
            hops: vec![hop_with(
                "目标 lab-1",
                &server,
                &known,
                AuthConfig::password(None),
            )],
        })
        .await;
        assert_eq!(
            server.counters.secret_attempts(),
            0,
            "没有密码：没有东西可发，也不会弹窗问"
        );
        assert_eq!(outcome.kind, TestResultKind::Partial, "{}", outcome.reason);
        assert_eq!(outcome.connectivity, CheckState::Passed);
        assert_eq!(outcome.identity, Some(IdentityState::Matched));
        assert_eq!(outcome.authentication, CheckState::Pending);
        assert_eq!(outcome.failing_stage, None);
        assert!(
            outcome.reason.contains("登录需要交互验证"),
            "{}",
            outcome.reason
        );
        for wrong in ["密码错误", "认证失败", "失败"] {
            assert!(
                !outcome.reason.contains(wrong) && !outcome.detail.contains("结果：失败"),
                "不能说成{wrong}：{}",
                outcome.reason
            );
        }
        assert!(
            outcome.detail.contains("不是密码错误"),
            "{}",
            outcome.detail
        );
    }

    /// 配置的凭据先没成、认证退到要人输入：是真的认证失败，不能说成「需要交互验证」（部分通过）。
    fn assert_rejected_then_prompt_fails(outcome: &TestOutcome) {
        assert_eq!(outcome.kind, TestResultKind::Failed, "{}", outcome.reason);
        assert_eq!(outcome.authentication, CheckState::Failed);
        assert_eq!(outcome.failing_stage, Some(TestStage::Auth));
        assert_eq!(
            outcome.reason.key(),
            reason_keys::CT_AUTH_REJECTED_THEN_PROMPT
        );
        assert!(
            !outcome.reason.contains("交互验证") && !outcome.detail.contains("不是密码错误"),
            "{} / {}",
            outcome.reason,
            outcome.detail
        );
    }

    fn inline_key(passphrase: Option<&str>) -> crate::ssh::auth::KeySource {
        use russh::keys::ssh_key::LineEnding;
        let key = fake_server::ed25519_key();
        crate::ssh::auth::KeySource::Inline {
            pem: key.to_openssh(LineEnding::LF).expect("导出").to_string(),
            passphrase: passphrase.map(str::to_owned),
        }
    }

    #[tokio::test]
    async fn a_rejected_key_that_falls_back_to_a_password_prompt_is_a_failure() {
        let server = fake_server::spawn_with(
            vec![fake_server::ed25519_key()],
            AuthMode::PublickeyRejectedPasswordAllowed,
        )
        .await;
        let dir = tempfile::tempdir().expect("临时目录");
        let known = known_path(&dir);
        trust(&server, &known);
        let outcome = test_ssh(SshTestPlan {
            hops: vec![hop_with(
                "目标 lab-1",
                &server,
                &known,
                AuthConfig::key(inline_key(None)),
            )],
        })
        .await;
        assert_eq!(
            server
                .counters
                .publickey
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "钥匙确实发过、被拒"
        );
        assert_eq!(server.counters.passwords(), 0, "测试不会代填密码");
        assert_rejected_then_prompt_fails(&outcome);
    }

    #[tokio::test]
    async fn a_wrong_saved_password_followed_by_a_keyboard_prompt_is_a_failure() {
        let server = fake_server::spawn_with(
            vec![fake_server::ed25519_key()],
            AuthMode::PasswordRejectedKeyboardPrompts,
        )
        .await;
        let dir = tempfile::tempdir().expect("临时目录");
        let known = known_path(&dir);
        trust(&server, &known);
        let outcome = test_ssh(SshTestPlan {
            hops: vec![hop_with(
                "目标 lab-1",
                &server,
                &known,
                AuthConfig::password(Some("wrong-for-test".to_owned())),
            )],
        })
        .await;
        assert_eq!(server.counters.passwords(), 1, "存的密码发过、被拒");
        assert_rejected_then_prompt_fails(&outcome);
    }

    #[tokio::test]
    async fn a_missing_key_file_is_a_failure_not_a_passphrase_question() {
        let server = fake_server::spawn(vec![fake_server::ed25519_key()]).await;
        let dir = tempfile::tempdir().expect("临时目录");
        let known = known_path(&dir);
        trust(&server, &known);
        let outcome = test_ssh(SshTestPlan {
            hops: vec![hop_with(
                "目标 lab-1",
                &server,
                &known,
                AuthConfig::key(crate::ssh::auth::KeySource::File {
                    path: dir.path().join("no-such-key"),
                    passphrase: None,
                }),
            )],
        })
        .await;
        assert_eq!(outcome.kind, TestResultKind::Failed, "{}", outcome.reason);
        assert!(!outcome.reason.contains("交互验证"), "{}", outcome.reason);
    }

    #[tokio::test]
    async fn an_encrypted_key_without_a_saved_passphrase_is_still_partial_interactive() {
        use russh::keys::ssh_key::LineEnding;
        // 要声明 publickey 的服务器：默认那台不收公钥，私钥会被直接跳过（那是真的失败）。
        let server = fake_server::spawn_with(
            vec![fake_server::ed25519_key()],
            AuthMode::PublickeyRejectedPasswordAllowed,
        )
        .await;
        let dir = tempfile::tempdir().expect("临时目录");
        let known = known_path(&dir);
        trust(&server, &known);
        let encrypted = fake_server::ed25519_key()
            .encrypt(&mut rand::rng(), "pass-for-test")
            .expect("加密");
        let outcome = test_ssh(SshTestPlan {
            hops: vec![hop_with(
                "目标 lab-1",
                &server,
                &known,
                AuthConfig::key(crate::ssh::auth::KeySource::Inline {
                    pem: encrypted
                        .to_openssh(LineEnding::LF)
                        .expect("导出")
                        .to_string(),
                    passphrase: None,
                }),
            )],
        })
        .await;
        assert_eq!(
            server
                .counters
                .publickey
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(outcome.kind, TestResultKind::Partial, "{}", outcome.reason);
        assert_eq!(outcome.authentication, CheckState::Pending);
    }

    #[test]
    fn encrypted_key_texts_are_recognised_by_their_headers() {
        // 私钥头拆开写：整段会被公开前的隐私扫描当成私钥。
        assert!(crate::ssh::auth::key_text_is_encrypted(concat!(
            "-----BEGIN ENCRYPTED ",
            "PRIVATE KEY-----\nAAAA\n"
        )));
        assert!(crate::ssh::auth::key_text_is_encrypted(concat!(
            "-----BEGIN RSA ",
            "PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\n"
        )));
        assert!(crate::ssh::auth::key_text_is_encrypted(
            "PuTTY-User-Key-File-3: ssh-ed25519\nEncryption: aes256-cbc\n"
        ));
        assert!(!crate::ssh::auth::key_text_is_encrypted(
            "PuTTY-User-Key-File-3: ssh-ed25519\nEncryption: none\n"
        ));
        assert!(!crate::ssh::auth::key_text_is_encrypted(concat!(
            "-----BEGIN OPENSSH ",
            "PRIVATE KEY-----\nAAAA\n"
        )));
    }

    #[tokio::test]
    async fn a_changed_fingerprint_fails_before_authentication_and_leaves_the_file_alone() {
        let server = fake_server::spawn(vec![fake_server::ed25519_key()]).await;
        let dir = tempfile::tempdir().expect("临时目录");
        let known = known_path(&dir);
        // 先给这台主机存一把同类型的别的密钥：服务器的密钥是现场生成的，一定对不上。
        let other = fake_server::ed25519_key();
        KnownHosts::new(&known)
            .append(
                &server.addr.ip().to_string(),
                server.addr.port(),
                other.public_key(),
            )
            .expect("预先保存");
        let before = std::fs::read(&known).expect("读");
        let outcome = test_ssh(SshTestPlan {
            hops: vec![hop("目标 lab-1", &server, &known)],
        })
        .await;
        assert_eq!(server.counters.auth_requests(), 0, "认证之前就停");
        assert_eq!(outcome.kind, TestResultKind::Failed);
        assert_eq!(outcome.identity, Some(IdentityState::Changed));
        assert_eq!(outcome.failing_stage, Some(TestStage::HostKey));
        assert_eq!(outcome.authentication, CheckState::NotApplicable);
        assert_eq!(
            std::fs::read(&known).expect("读"),
            before,
            "已保存的指纹一个字节都不能动"
        );
    }

    #[tokio::test]
    async fn a_revoked_key_fails_before_authentication() {
        let server = fake_server::spawn(vec![fake_server::ed25519_key()]).await;
        let dir = tempfile::tempdir().expect("临时目录");
        let known = known_path(&dir);
        trust(&server, &known);
        let line = std::fs::read_to_string(&known).expect("读");
        std::fs::write(&known, format!("@revoked {line}")).expect("改成吊销");
        let before = std::fs::read(&known).expect("读");
        let outcome = test_ssh(SshTestPlan {
            hops: vec![hop("目标 lab-1", &server, &known)],
        })
        .await;
        assert_eq!(server.counters.auth_requests(), 0);
        assert_eq!(outcome.kind, TestResultKind::Failed, "{}", outcome.reason);
        assert_eq!(outcome.identity, Some(IdentityState::Revoked));
        assert_eq!(outcome.authentication, CheckState::NotApplicable);
        assert_eq!(std::fs::read(&known).expect("读"), before);
    }

    #[tokio::test]
    async fn an_unreadable_trust_store_fails_before_authentication() {
        let server = fake_server::spawn(vec![fake_server::ed25519_key()]).await;
        let dir = tempfile::tempdir().expect("临时目录");
        // 信任文件的位置其实是个目录：读不了，不能退化成「没见过」。
        let known = known_path(&dir);
        std::fs::create_dir_all(&known).expect("建目录");
        let outcome = test_ssh(SshTestPlan {
            hops: vec![hop("目标 lab-1", &server, &known)],
        })
        .await;
        assert_eq!(server.counters.auth_requests(), 0);
        assert_eq!(outcome.kind, TestResultKind::Failed, "{}", outcome.reason);
        assert_eq!(outcome.identity, Some(IdentityState::Unreadable));
        assert_eq!(outcome.failing_stage, Some(TestStage::HostKey));
        assert!(known.is_dir(), "没动它");
    }

    #[tokio::test]
    async fn an_untrusted_target_behind_a_trusted_jump_host_stops_at_the_target() {
        let jump = fake_server::spawn(vec![fake_server::ed25519_key()]).await;
        let target = fake_server::spawn(vec![fake_server::ed25519_key()]).await;
        let dir = tempfile::tempdir().expect("临时目录");
        let known = known_path(&dir);
        trust(&jump, &known);
        let outcome = test_ssh(SshTestPlan {
            hops: vec![
                hop("跳板机 edge-1", &jump, &known),
                hop("目标 lab-1", &target, &known),
            ],
        })
        .await;
        assert_eq!(jump.counters.passwords(), 1, "已信任的跳板机照常登录");
        assert_eq!(target.counters.auth_requests(), 0);
        assert_eq!(outcome.kind, TestResultKind::Partial, "{}", outcome.reason);
        assert_eq!(outcome.identity, Some(IdentityState::Unconfirmed));
        let hop = outcome.checked_hop.as_ref().expect("停在目标上");
        assert_eq!((hop.index, hop.label.as_str()), (1, "目标 lab-1"));
        assert!(outcome.untested_hops.is_empty());
        assert!(outcome.detail.contains(&fingerprint(&target)));
    }

    #[tokio::test]
    async fn an_untrusted_jump_host_stops_the_chain_with_zero_connections_after_it() {
        let jump = fake_server::spawn(vec![fake_server::ed25519_key()]).await;
        let target = fake_server::spawn(vec![fake_server::ed25519_key()]).await;
        let dir = tempfile::tempdir().expect("临时目录");
        let known = known_path(&dir);
        // 目标已经信任了，但跳板机没有：跳板机之后的每一步都不能发。
        trust(&target, &known);
        let outcome = test_ssh(SshTestPlan {
            hops: vec![
                hop("跳板机 edge-1", &jump, &known),
                hop("目标 lab-1", &target, &known),
            ],
        })
        .await;
        assert_eq!(jump.counters.auth_requests(), 0, "跳板机也不认证");
        assert_eq!(
            target.counters.accepted(),
            0,
            "连跳板机都没过，不会去碰目标：连接数为 0"
        );
        assert_eq!(outcome.kind, TestResultKind::Partial, "{}", outcome.reason);
        let hop = outcome.checked_hop.as_ref().expect("停在跳板机上");
        assert_eq!((hop.index, hop.label.as_str()), (0, "跳板机 edge-1"));
        assert_eq!(outcome.untested_hops, vec!["目标 lab-1".to_owned()]);
        assert!(
            outcome
                .reason
                .starts_with("跳板机 edge-1：可达，后续目标未测试"),
            "{}",
            outcome.reason
        );
    }

    #[tokio::test]
    async fn a_telnet_target_behind_an_untrusted_jump_host_is_not_called_reachable() {
        let jump = fake_server::spawn(vec![fake_server::ed25519_key()]).await;
        let dir = tempfile::tempdir().expect("临时目录");
        let known = known_path(&dir);
        let outcome = test_tcp(TcpTestPlan {
            host: "192.0.2.50".to_owned(),
            port: 23,
            route: TcpRoute::Jump(vec![hop("跳板机 edge-1", &jump, &known)]),
        })
        .await;
        assert_eq!(jump.counters.auth_requests(), 0);
        assert_eq!(outcome.kind, TestResultKind::Partial, "{}", outcome.reason);
        assert_eq!(outcome.untested_hops, vec!["192.0.2.50:23".to_owned()]);
        assert!(outcome.reason.contains("后续目标未测试"));
        assert!(!known.exists());
    }

    #[tokio::test]
    async fn a_telnet_target_behind_a_trusted_jump_host_is_a_plain_tcp_check() {
        let jump = fake_server::spawn(vec![fake_server::ed25519_key()]).await;
        let target = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定");
        let addr = target.local_addr().expect("地址");
        tokio::spawn(async move {
            // 接受后直接关掉：算连通（Closed），不需要 Telnet 协议。
            let _ = target.accept().await;
        });
        let dir = tempfile::tempdir().expect("临时目录");
        let known = known_path(&dir);
        trust(&jump, &known);
        let outcome = test_tcp_with(
            TcpTestPlan {
                host: addr.ip().to_string(),
                port: addr.port(),
                route: TcpRoute::Jump(vec![hop("跳板机 edge-1", &jump, &known)]),
            },
            Duration::from_secs(5),
            Duration::from_millis(300),
        )
        .await;
        assert_eq!(outcome.kind, TestResultKind::Complete, "{}", outcome.reason);
        assert_eq!(outcome.identity, None, "Telnet 没有主机密钥这一项");
        assert_eq!(outcome.authentication, CheckState::NotApplicable);
        let hop = outcome.checked_hop.as_ref().expect("Telnet 目标");
        assert_eq!(hop.index, 1);
    }

    /// 一个只接受 TCP 连接然后什么都不做 / 立刻关掉的「服务器」：它让握手卡住或断开，
    /// 但从来没有出示过主机密钥，也就没有任何「预期的停止」可言。
    #[tokio::test]
    async fn a_hung_or_dropped_handshake_is_never_turned_into_a_partial_result() {
        let dir = tempfile::tempdir().expect("临时目录");
        let known = known_path(&dir);
        let silent = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定");
        let silent_addr = silent.local_addr().expect("地址");
        tokio::spawn(async move {
            let _held = silent.accept().await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let dropper = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定");
        let dropper_addr = dropper.local_addr().expect("地址");
        tokio::spawn(async move {
            if let Ok((socket, _)) = dropper.accept().await {
                drop(socket);
            }
        });
        let make = |addr: std::net::SocketAddr| ChainHop {
            label: "目标 lab-1".to_owned(),
            host: addr.ip().to_string(),
            port: addr.port(),
            username: "tester".to_owned(),
            proxy: None,
            auth: AuthConfig::password(Some(PASSWORD.to_owned())),
            known_hosts: KnownHosts::new(&known),
            legacy_algorithms: false,
        };
        let hung = test_ssh_with(
            SshTestPlan {
                hops: vec![make(silent_addr)],
            },
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(hung.kind, TestResultKind::Failed, "{}", hung.reason);
        assert_eq!(hung.failing_stage, Some(TestStage::Handshake));
        assert_eq!(hung.connectivity, CheckState::Failed);
        assert_eq!(hung.identity, None);
        let dropped = test_ssh(SshTestPlan {
            hops: vec![make(dropper_addr)],
        })
        .await;
        assert_eq!(dropped.kind, TestResultKind::Failed, "{}", dropped.reason);
        assert_eq!(dropped.identity, None);
        assert_ne!(dropped.connectivity, CheckState::Passed);
    }

    #[tokio::test]
    async fn a_server_with_several_host_keys_shows_the_type_we_already_saved() {
        // 服务器有 Ed25519 和 ECDSA 两把主机密钥；这台主机以前只存过 ECDSA。
        // 默认顺序（Ed25519 优先）会出示 Ed25519，落到「只存过别种类型」而被拦下；
        // 把已保存的类型排到协商偏好最前，服务器就会出示 ECDSA，直接核对通过。
        let ecdsa = fake_server::ecdsa_key();
        let server = fake_server::spawn(vec![fake_server::ed25519_key(), ecdsa.clone()]).await;
        let dir = tempfile::tempdir().expect("临时目录");
        let known = known_path(&dir);
        KnownHosts::new(&known)
            .append(
                &server.addr.ip().to_string(),
                server.addr.port(),
                ecdsa.public_key(),
            )
            .expect("预先信任");
        let outcome = test_ssh(SshTestPlan {
            hops: vec![hop("目标 lab-1", &server, &known)],
        })
        .await;
        assert_eq!(outcome.kind, TestResultKind::Complete, "{}", outcome.reason);
        assert_eq!(outcome.identity, Some(IdentityState::Matched));
        assert_eq!(server.counters.passwords(), 1);
    }

    /// 测试过不等于核对过：测试之后正式连接照样要问指纹（不继承测试里「看到过」的结果），
    /// 用户确认保存之后再测，才是 Complete；这一整套里测试自己从没写过信任文件。
    #[tokio::test]
    async fn testing_never_authorises_the_normal_connection_that_follows() {
        struct Asker {
            asked: Mutex<usize>,
            decision: HostKeyDecision,
        }
        impl Interactor for Asker {
            fn confirm_host_key(&self, _info: HostKeyInfo) -> BoxFuture<'_, HostKeyDecision> {
                *self
                    .asked
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) += 1;
                let decision = self.decision;
                Box::pin(async move { decision })
            }
            fn prompt(&self, _request: PromptRequest) -> BoxFuture<'_, Option<PromptAnswers>> {
                Box::pin(async { None })
            }
        }
        let server = fake_server::spawn(vec![fake_server::ed25519_key()]).await;
        let dir = tempfile::tempdir().expect("临时目录");
        let known = known_path(&dir);
        let tested = test_ssh(SshTestPlan {
            hops: vec![hop("目标 lab-1", &server, &known)],
        })
        .await;
        assert_eq!(tested.kind, TestResultKind::Partial);
        assert!(!known.exists(), "测试没有留下任何信任记录");
        // 正式连接：照样要确认；用户选「信任并保存」。
        let asker = Arc::new(Asker {
            asked: Mutex::new(0),
            decision: HostKeyDecision::TrustAndSave,
        });
        let formal =
            Connection::connect_chain(vec![hop("目标 lab-1", &server, &known)], asker.clone())
                .await;
        assert!(formal.is_ok());
        assert_eq!(
            *asker
                .asked
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            1,
            "测试过也要重新问"
        );
        // 保存之后：再测就是 Complete，正式连接不再问。
        let tested = test_ssh(SshTestPlan {
            hops: vec![hop("目标 lab-1", &server, &known)],
        })
        .await;
        assert_eq!(tested.kind, TestResultKind::Complete, "{}", tested.reason);
        let again = Arc::new(Asker {
            asked: Mutex::new(0),
            decision: HostKeyDecision::Reject,
        });
        let formal =
            Connection::connect_chain(vec![hop("目标 lab-1", &server, &known)], again.clone())
                .await;
        assert!(formal.is_ok());
        assert_eq!(
            *again
                .asked
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            0,
            "已核对的主机不重复问"
        );
    }

    /// 同一份目标配置和信任快照：测试连接和正式连接走同一个握手函数，所以出示的主机密钥、
    /// 身份分类完全一致（不另造一份协商规则）。
    #[tokio::test]
    async fn the_test_and_a_normal_connection_see_the_same_host_key_and_status() {
        struct Recorder(Mutex<Vec<HostKeyInfo>>);
        impl Interactor for Recorder {
            fn confirm_host_key(&self, info: HostKeyInfo) -> BoxFuture<'_, HostKeyDecision> {
                self.0
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(info);
                Box::pin(async { HostKeyDecision::Reject })
            }
            fn prompt(&self, _request: PromptRequest) -> BoxFuture<'_, Option<PromptAnswers>> {
                Box::pin(async { None })
            }
        }
        // 服务器有两把密钥；先什么都没存，再只存 ECDSA 的。
        let ecdsa = fake_server::ecdsa_key();
        let server = fake_server::spawn(vec![fake_server::ed25519_key(), ecdsa.clone()]).await;
        let dir = tempfile::tempdir().expect("临时目录");
        let known = known_path(&dir);
        for saved in [None, Some(ecdsa.public_key().clone())] {
            if let Some(key) = &saved {
                KnownHosts::new(&known)
                    .append(&server.addr.ip().to_string(), server.addr.port(), key)
                    .expect("保存");
            }
            let recorder = Arc::new(Recorder(Mutex::new(Vec::new())));
            let formal = Connection::connect_chain(
                vec![hop("目标 lab-1", &server, &known)],
                recorder.clone(),
            )
            .await;
            let tested = test_ssh(SshTestPlan {
                hops: vec![hop("目标 lab-1", &server, &known)],
            })
            .await;
            let asked = recorder
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            match saved {
                None => {
                    assert!(formal.is_err(), "没有记录：正式连接要先问，这里回答拒绝");
                    let info = asked.first().expect("正式连接问了指纹");
                    assert_eq!(info.status, HostKeyStatus::Unknown);
                    assert_eq!(tested.identity, Some(IdentityState::Unconfirmed));
                    assert!(tested.detail.contains(&info.fingerprint));
                    assert!(tested.detail.contains(&info.key_type));
                }
                Some(_) => {
                    assert!(formal.is_ok(), "已保存的类型：正式连接直接核对通过");
                    assert!(asked.is_empty(), "不再问");
                    assert_eq!(tested.identity, Some(IdentityState::Matched));
                    assert_eq!(tested.kind, TestResultKind::Complete);
                }
            }
        }
    }
}
