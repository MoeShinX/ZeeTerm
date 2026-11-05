//! 「测试连接」（V2-U13）：用表单里当前的值做一次连接测试，不必先保存。
//!
//! - **SSH**：解析地址 → 经代理 / 跳板机 TCP 连接 → 握手 → 认证 → 成功即断开。走和真实连接同一套
//!   拨号和握手代码（[`Connection::handshake_and_authenticate`]），但是**独立连接**：不进连接池、不影响
//!   已经打开的标签，也不弹任何确认框；
//! - **主机指纹**：测试里的 [`Interactor`] 对没信任的指纹（没见过 / 只存过别种类型）一律**拒绝并记下指纹**：
//!   握手做到出示指纹为止，不进行任何认证——密码、keyboard-interactive 的回答发给还没核对过的主机，
//!   中间人就能拿到明文（V2-C7a S1）。结果写「主机指纹 … 尚未信任，没有测试登录；请先正式连接一次并核对指纹」；
//!   跳板链上哪一跳没信任，就在那一跳停下（它之后的各跳都不连）。指纹和已存的不一致时同样拒绝，按失败显示
//!   「主机密钥与保存的不一致」。整个过程不会调用 `KnownHosts::append / replace`；
//! - **Telnet**：经代理 / 跳板机把 TCP 连上就算成功（读到第一个字节或 2 秒内没数据都算），不做自动登录；
//! - **串口**：由界面层按参数打开端口再关闭（串口驱动在 `zssh-protocols`），这里只给结果的文案；
//! - 整次测试 10 秒超时，超时的结果写明卡在哪个阶段。日志和结果里都没有密码。
//!
//! 可单测的部分（阶段划分、错误分类、结果文案）都是纯函数；联网部分用进程内迷你 SSH 服务器 / 假 Telnet
//! 设备在界面层的测试里验证。

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;

use crate::error::ZsshError;
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

/// 一次测试的结果（界面直接显示）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestOutcome {
    pub ok: bool,
    /// 按钮栏上面那一行。
    pub headline: String,
    /// 「复制详情」复制的多行文字（目标、路线、阶段、耗时、完整原因；没有密码）。
    pub detail: String,
    pub elapsed_ms: u64,
    /// 失败时出问题的阶段。
    pub stage: Option<TestStage>,
    /// 连接成功但指纹尚未信任：本次看到的指纹（`SHA256:…`）。
    pub untrusted_fingerprint: Option<String>,
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
}

/// 分类之后的失败：阶段 + 一句中文原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub stage: TestStage,
    pub message: String,
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 把一次失败分成阶段并写成中文。`label` 是这一跳在界面上的名字（「跳板机 web-1」「目标 db」）。
pub fn classify_failure(
    error: &ZsshError,
    phase: FailurePhase,
    role: HopRole,
    label: &str,
    interaction: Interaction,
) -> Failure {
    let inner = match phase {
        FailurePhase::Connect { via_proxy } => classify_connect(error, via_proxy),
        FailurePhase::Forward => Failure {
            stage: TestStage::Jump,
            message: format!("跳板机转发失败：{}", forward_reason(error)),
        },
        FailurePhase::Handshake => classify_handshake(error, interaction),
    };
    // 跳板机那一跳的问题统一挂在「经跳板机」下，说清是哪台；代理失败保持原样（和是目标还是跳板无关）。
    if role == HopRole::Jump && !matches!(inner.stage, TestStage::Proxy | TestStage::Jump) {
        return Failure {
            stage: TestStage::Jump,
            message: format!("{label}：{}", inner.message),
        };
    }
    inner
}

fn forward_reason(error: &ZsshError) -> String {
    match error {
        ZsshError::InvalidInput(message) => message.zh().to_owned(),
        ZsshError::ConnectTimeout { host, seconds, .. } => {
            format!("经跳板机连接 {host} 超过 {seconds} 秒没有响应")
        }
        other => one_line(&other.to_string()),
    }
}

fn classify_connect(error: &ZsshError, via_proxy: bool) -> Failure {
    match error {
        ZsshError::Proxy { proxy, detail } => Failure {
            stage: TestStage::Proxy,
            message: format!("代理失败（{proxy}）：{}", one_line(detail.zh())),
        },
        // 经代理时，能不能解析 / 连上的是代理地址本身。
        ZsshError::Dns { host, .. } if via_proxy => Failure {
            stage: TestStage::Proxy,
            message: format!("代理失败：无法解析代理地址 {host}"),
        },
        ZsshError::ConnectTimeout {
            host,
            port,
            seconds,
        } if via_proxy => Failure {
            stage: TestStage::Proxy,
            message: format!("代理失败：连接代理 {host}:{port} 超过 {seconds} 秒没有响应"),
        },
        ZsshError::Connect {
            host, port, source, ..
        } if via_proxy => Failure {
            stage: TestStage::Proxy,
            message: format!(
                "代理失败：连不上代理 {host}:{port}（{}）",
                io_reason(source)
            ),
        },
        ZsshError::Dns { host, .. } => Failure {
            stage: TestStage::Resolve,
            message: format!("域名解析失败：{host}（检查地址有没有写对、本机 DNS 是否正常）"),
        },
        ZsshError::ConnectTimeout {
            host,
            port,
            seconds,
        } => Failure {
            stage: TestStage::Connect,
            message: format!("连接超时：{host}:{port} 超过 {seconds} 秒没有响应"),
        },
        ZsshError::Connect {
            host, port, source, ..
        } => Failure {
            stage: TestStage::Connect,
            message: match source.kind() {
                std::io::ErrorKind::ConnectionRefused => {
                    format!("连接被拒绝：{host}:{port} 上没有程序在监听，或者被防火墙拒绝了")
                }
                _ => format!("无法连接 {host}:{port}（{}）", io_reason(source)),
            },
        },
        other => Failure {
            stage: TestStage::Connect,
            message: format!("无法建立连接：{}", one_line(&other.to_string())),
        },
    }
}

fn io_reason(source: &std::io::Error) -> String {
    match source.kind() {
        std::io::ErrorKind::ConnectionRefused => "连接被拒绝".to_owned(),
        std::io::ErrorKind::TimedOut => "超时".to_owned(),
        std::io::ErrorKind::HostUnreachable => "主机不可达".to_owned(),
        std::io::ErrorKind::NetworkUnreachable => "网络不可达".to_owned(),
        std::io::ErrorKind::ConnectionReset => "连接被重置".to_owned(),
        _ => one_line(&source.to_string()),
    }
}

fn classify_handshake(error: &ZsshError, interaction: Interaction) -> Failure {
    let host_key = |message: String| Failure {
        stage: TestStage::HostKey,
        message,
    };
    let auth = |message: String| Failure {
        stage: TestStage::Auth,
        message,
    };
    match error {
        ZsshError::HostKeyChanged(info) => host_key(format!(
            "主机密钥与保存的不一致：已保存 {}，本次收到 {}（记录在 {}）",
            info.stored, info.presented, info.stored_in
        )),
        ZsshError::HostKeyRevoked(info) => host_key(format!(
            "主机密钥已被标记为吊销（@revoked）：记录 {}，本次收到 {}",
            info.stored, info.presented
        )),
        ZsshError::HostKeyRejected { fingerprint, .. } => {
            host_key(format!("主机密钥被拒绝（指纹 {fingerprint}）"))
        }
        ZsshError::HostKeyRekeyMismatch { first, again, .. } => host_key(format!(
            "握手中途主机密钥变了：第一次 {first}，这次 {again}"
        )),
        ZsshError::HostKeyStore(message) => host_key(one_line(message.zh())),
        ZsshError::AuthFailed => auth("认证失败：用户名或密码错误".to_owned()),
        ZsshError::AllAuthFailed {
            tried, remaining, ..
        } => auth(format!(
            "认证失败：试过 {}，服务器还支持 {}",
            one_line(tried),
            one_line(remaining)
        )),
        ZsshError::AuthTimeout { seconds } => auth(format!("认证超时（每一步最多 {seconds} 秒）")),
        ZsshError::KeyLoad(message) => auth(format!("读私钥失败：{}", one_line(message.zh()))),
        ZsshError::AgentUnavailable(message) => {
            auth(format!("ssh-agent 用不了：{}", one_line(message.zh())))
        }
        ZsshError::RsaSha1Refused { .. } => auth(
            "服务器只接受 SHA-1 的 ssh-rsa 签名：换 Ed25519 / ECDSA 密钥，或打开「旧算法兼容」"
                .to_owned(),
        ),
        ZsshError::Cancelled => match interaction.prompted {
            Some(PromptKind::Password) => auth(
                "认证失败：需要密码，但没有填写（测试连接不会弹窗询问）".to_owned(),
            ),
            Some(PromptKind::Passphrase) => auth(
                "认证失败：私钥有口令，但没有可用的口令（测试连接不会弹窗询问）".to_owned(),
            ),
            Some(PromptKind::KeyboardInteractive) => auth(
                "服务器要求交互式验证（例如动态口令），测试连接无法代替你回答；服务器本身是可以连通的"
                    .to_owned(),
            ),
            None => auth("已取消".to_owned()),
        },
        ZsshError::NoCommonAlgorithm(info) => Failure {
            stage: TestStage::Handshake,
            message: format!(
                "和服务器协商不出共同的 {} 算法（老服务器可以试试「旧算法兼容」）",
                info.kind
            ),
        },
        ZsshError::ConnectTimeout { seconds, .. } => Failure {
            stage: TestStage::Handshake,
            message: format!("SSH 握手超过 {seconds} 秒没有完成"),
        },
        ZsshError::Ssh(russh::Error::Disconnect) => Failure {
            stage: TestStage::Handshake,
            message: "服务器在握手时断开了连接（地址 / 端口上跑的可能不是 SSH 服务）".to_owned(),
        },
        other => Failure {
            stage: TestStage::Handshake,
            message: format!("SSH 握手失败：{}", one_line(&other.to_string())),
        },
    }
}

// ---- 结果文案（纯函数）----

fn version_suffix(version: Option<&str>) -> String {
    version.map_or_else(String::new, |v| format!("，服务器 {v}"))
}

/// SSH 成功的那一行（走到这里的主机指纹都已经信任，见 [`untrusted_stop_headline`]）。
pub fn ssh_success_headline(elapsed_ms: u64, version: Option<&str>) -> String {
    format!("连接成功，耗时 {elapsed_ms} ms{}", version_suffix(version))
}

/// 主机指纹没信任、测试停在认证之前的那一行。`other_types`：这台主机只保存过别种类型的密钥；
/// `hop_label`：停下的是跳板机时它的名字（目标就是 `None`）。
pub fn untrusted_stop_headline(
    fingerprint: &str,
    other_types: bool,
    hop_label: Option<&str>,
) -> String {
    let subject = if other_types {
        format!("这台主机只保存过别种类型的密钥，本次指纹 {fingerprint} 尚未信任")
    } else {
        format!("主机指纹 {fingerprint} 尚未信任")
    };
    let note = "没有测试登录；请先正式连接一次并核对指纹";
    match hop_label {
        Some(label) => format!("{label}：{subject}，{note}"),
        None => format!("{subject}，{note}"),
    }
}

/// Telnet 成功的那一行。
pub fn tcp_success_headline(elapsed_ms: u64, first_byte: FirstByte) -> String {
    let note = match first_byte {
        FirstByte::Data => "对方已回应",
        FirstByte::Silent => "对方 2 秒内没有主动发数据，Telnet 设备常见",
        FirstByte::Closed => "但对方随即关闭了连接",
    };
    format!("连接成功，耗时 {elapsed_ms} ms（TCP 已连通，未做登录；{note}）")
}

/// Telnet 连上之后对方的表现。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FirstByte {
    Data,
    Silent,
    Closed,
}

fn success(headline: String, detail: String, elapsed_ms: u64) -> TestOutcome {
    TestOutcome {
        ok: true,
        headline,
        detail,
        elapsed_ms,
        stage: None,
        untrusted_fingerprint: None,
    }
}

fn failure_outcome(failure: &Failure, detail: String, elapsed_ms: u64) -> TestOutcome {
    TestOutcome {
        ok: false,
        // 原因本身已经带着阶段的说法（「域名解析失败」「连接被拒绝」「认证失败」），不再重复前缀。
        headline: failure.message.clone(),
        detail,
        elapsed_ms,
        stage: Some(failure.stage),
        untrusted_fingerprint: None,
    }
}

/// 串口打开成功。
pub fn serial_success(device: &str, elapsed_ms: u64) -> TestOutcome {
    let headline = format!("串口 {device} 可以打开（已立即关闭），耗时 {elapsed_ms} ms");
    let detail = format!("测试连接\n串口：{device}\n结果：可以打开\n耗时：{elapsed_ms} ms");
    success(headline, detail, elapsed_ms)
}

/// 串口打开失败：`reason` 是串口驱动层分类好的中文原因（「COM3 不存在…」「COM3 被别的程序占用…」）。
pub fn serial_failure(device: &str, reason: &str, elapsed_ms: u64) -> TestOutcome {
    let failure = Failure {
        stage: TestStage::Open,
        message: one_line(reason),
    };
    let detail = format!(
        "测试连接\n串口：{device}\n结果：失败（阶段：{}）\n原因：{}",
        failure.stage.label(),
        failure.message
    );
    failure_outcome(&failure, detail, elapsed_ms)
}

/// 用户在测试中途点了取消。
pub fn cancelled() -> TestOutcome {
    TestOutcome {
        ok: false,
        headline: "已取消测试".to_owned(),
        detail: "测试连接\n结果：已取消".to_owned(),
        elapsed_ms: 0,
        stage: None,
        untrusted_fingerprint: None,
    }
}

// ---- 测试用的 Interactor ----

/// 测试过程中的观察：问过什么、停在了哪个没信任的指纹、现在走到哪一步。
#[derive(Default)]
struct Observed {
    /// 第一个没信任的指纹（连接随即被拒绝，所以最多一个）；`bool`：这台主机只存过别种类型的密钥。
    untrusted: Option<(HostKeyInfo, bool)>,
    prompted: Option<PromptKind>,
    stage: Option<TestStage>,
}

/// 对任何没信任的指纹都拒绝（不写 known_hosts）；任何需要输入的提问一律取消，不弹窗。
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

    fn interaction(&self) -> Interaction {
        Interaction {
            prompted: self.lock().prompted,
        }
    }
}

impl Interactor for TestInteractor {
    fn confirm_host_key(&self, info: HostKeyInfo) -> BoxFuture<'_, HostKeyDecision> {
        let decision = match info.status {
            // 没见过 / 只存过别的类型：不放行，握手到此为止，只记下指纹给结果用。放行的话下一步就是
            // 把保存的密码发过去；「本次放行」不等于「核对过」。
            HostKeyStatus::Unknown | HostKeyStatus::OtherKeyTypesSaved => {
                let other = info.status == HostKeyStatus::OtherKeyTypesSaved;
                self.lock().untrusted.get_or_insert((info, other));
                HostKeyDecision::Reject
            }
            // 指纹变了 / 吊销：绝不放行（也不更新已存的）。
            HostKeyStatus::Changed | HostKeyStatus::Revoked => HostKeyDecision::Reject,
        };
        Box::pin(async move { decision })
    }

    fn prompt(&self, request: PromptRequest) -> BoxFuture<'_, Option<PromptAnswers>> {
        self.lock().prompted = Some(request.kind);
        Box::pin(async { None })
    }

    fn hop_started(&self, _progress: HopProgress) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    fn authentication_started(&self) -> BoxFuture<'_, ()> {
        self.set_stage(TestStage::Auth);
        Box::pin(async {})
    }
}

// ---- 拨号 ----

type Fail = (Failure, String);

fn fail(failure: Failure, error: &ZsshError) -> Fail {
    let raw = one_line(&error.to_string());
    (failure, raw)
}

/// 按顺序连每一跳（只有第一跳走自己的代理，后面各跳走在前一跳的 SSH 通道里，同 `connect_chain`）。
/// `last_is_target`：最后一跳是目标（SSH 测试）还是也只是跳板机（Telnet 经跳板机测试）。
async fn connect_hops(
    hops: Vec<ChainHop>,
    last_is_target: bool,
    interactor: &Arc<TestInteractor>,
) -> Result<Vec<Connection>, Fail> {
    let total = hops.len();
    let dyn_interactor: Arc<dyn Interactor> = interactor.clone();
    let mut established: Vec<Connection> = Vec::with_capacity(total);
    for (index, hop) in hops.into_iter().enumerate() {
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
        let stream: BoxedStream = match established.last() {
            None => match &hop.proxy {
                Some(proxy) => connect_via_proxy(proxy, &hop.host, hop.port)
                    .await
                    .map_err(|err| {
                        let failure = classify_failure(
                            &err,
                            FailurePhase::Connect { via_proxy: true },
                            role,
                            &hop.label,
                            interactor.interaction(),
                        );
                        fail(failure, &err)
                    })?,
                None => Box::new(connect_direct(&hop.host, hop.port).await.map_err(|err| {
                    let failure = classify_failure(
                        &err,
                        FailurePhase::Connect { via_proxy: false },
                        role,
                        &hop.label,
                        interactor.interaction(),
                    );
                    fail(failure, &err)
                })?),
            },
            Some(previous) => {
                interactor.set_stage(TestStage::Jump);
                previous
                    .open_direct_tcpip(&hop.host, hop.port)
                    .await
                    .map_err(|err| {
                        let failure = classify_failure(
                            &err,
                            FailurePhase::Forward,
                            role,
                            &hop.label,
                            interactor.interaction(),
                        );
                        fail(failure, &err)
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
            let failure = classify_failure(
                &err,
                FailurePhase::Handshake,
                role,
                &hop.label,
                interactor.interaction(),
            );
            fail(failure, &err)
        })?;
        established.push(connection);
    }
    Ok(established)
}

/// 每一跳的（地址, 端口, 名字），测试被拒绝在某一跳时按地址找回它的名字。目标不算：它不需要"跳板机 x："前缀。
type HopLabels = Vec<(String, u16, String)>;

/// `last_is_target`：最后一跳是目标（不收集）还是也只是跳板机。
fn hop_labels(hops: &[ChainHop], last_is_target: bool) -> HopLabels {
    let jumps = if last_is_target {
        hops.len().saturating_sub(1)
    } else {
        hops.len()
    };
    hops.iter()
        .take(jumps)
        .map(|hop| (hop.host.clone(), hop.port, hop.label.clone()))
        .collect()
}

/// 测试停在没信任的指纹上：拼出"没有测试登录"的结果。不是成功（什么都没测到），也不是红色失败
/// （`stage` 留空，界面用中性色）。没有停在指纹上就是 `None`。
fn untrusted_stop_outcome(
    interactor: &TestInteractor,
    header: &str,
    labels: &HopLabels,
    elapsed_ms: u64,
) -> Option<TestOutcome> {
    let (info, other_types) = interactor.lock().untrusted.clone()?;
    let hop_label = labels
        .iter()
        .find(|(host, port, _)| *host == info.host && *port == info.port)
        .map(|(_, _, label)| label.as_str());
    let headline = untrusted_stop_headline(&info.fingerprint, other_types, hop_label);
    let detail = format!(
        "{header}\n结果：没有测试登录（主机指纹尚未信任）\n耗时：{elapsed_ms} ms\n主机：{}:{}\n指纹：{}（尚未信任，没有保存）\n说明：测试连接只做到握手和出示指纹，不会把密码或验证码发给还没核对过的主机；请先正式连接一次并核对指纹。",
        info.host, info.port, info.fingerprint
    );
    Some(TestOutcome {
        ok: false,
        headline,
        detail,
        elapsed_ms,
        stage: None,
        untrusted_fingerprint: Some(info.fingerprint),
    })
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

fn timeout_failure(timeout: Duration, interactor: &TestInteractor) -> Failure {
    let stage = interactor.lock().stage.unwrap_or(TestStage::Connect);
    Failure {
        stage,
        message: format!(
            "连接测试超过 {} 秒仍没有完成（卡在：{}）",
            timeout.as_secs(),
            stage.label()
        ),
    }
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
            message: "没有要连接的目标".to_owned(),
        };
        return failure_outcome(&failure, format!("{header}\n结果：失败"), 0);
    }
    let interactor = TestInteractor::new();
    let jump_labels = hop_labels(&plan.hops, true);
    let result = tokio::time::timeout(timeout, connect_hops(plan.hops, true, &interactor)).await;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    match result {
        Ok(Ok(connections)) => {
            let version = connections
                .last()
                .and_then(|connection| connection.server_version().map(str::to_owned));
            let target_fingerprint = connections
                .last()
                .map(|connection| connection.server_key().fingerprint.clone());
            // 先断开再给结果：目标先关，再关跳板机。
            for connection in connections.iter().rev() {
                let _ = connection.disconnect().await;
            }
            drop(connections);
            let headline = ssh_success_headline(elapsed_ms, version.as_deref());
            let detail = format!(
                "{header}\n结果：连接成功\n耗时：{elapsed_ms} ms\n服务器：{}\n指纹：{}",
                version.as_deref().unwrap_or("（没取到版本）"),
                match &target_fingerprint {
                    Some(fp) => format!("{fp}（已信任）"),
                    None => "（没取到）".to_owned(),
                }
            );
            success(headline, detail, elapsed_ms)
        }
        Ok(Err((failure, raw))) => {
            if let Some(outcome) =
                untrusted_stop_outcome(&interactor, &header, &jump_labels, elapsed_ms)
            {
                return outcome;
            }
            let detail = format!(
                "{header}\n结果：失败（阶段：{}）\n耗时：{elapsed_ms} ms\n原因：{}\n原始错误：{raw}",
                failure.stage.label(),
                failure.message
            );
            failure_outcome(&failure, detail, elapsed_ms)
        }
        Err(_) => {
            let failure = timeout_failure(timeout, &interactor);
            let detail = format!(
                "{header}\n结果：失败（阶段：{}）\n耗时：{elapsed_ms} ms\n原因：{}",
                failure.stage.label(),
                failure.message
            );
            failure_outcome(&failure, detail, elapsed_ms)
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
    let jump_labels = match &route {
        TcpRoute::Jump(hops) => hop_labels(hops, false),
        _ => Vec::new(),
    };
    let interactor = TestInteractor::new();
    let attempt = async {
        let (stream, _keep): (BoxedStream, Vec<Connection>) = match route {
            TcpRoute::Direct => {
                interactor.set_stage(TestStage::Connect);
                let socket = connect_direct(&host, port).await.map_err(|err| {
                    fail(
                        classify_failure(
                            &err,
                            FailurePhase::Connect { via_proxy: false },
                            HopRole::Target,
                            "",
                            Interaction::default(),
                        ),
                        &err,
                    )
                })?;
                (Box::new(socket), Vec::new())
            }
            TcpRoute::Proxy(proxy) => {
                interactor.set_stage(TestStage::Proxy);
                let stream = connect_via_proxy(&proxy, &host, port)
                    .await
                    .map_err(|err| {
                        fail(
                            classify_failure(
                                &err,
                                FailurePhase::Connect { via_proxy: true },
                                HopRole::Target,
                                "",
                                Interaction::default(),
                            ),
                            &err,
                        )
                    })?;
                (stream, Vec::new())
            }
            TcpRoute::Jump(hops) => {
                let connections = connect_hops(hops, false, &interactor).await?;
                interactor.set_stage(TestStage::Jump);
                let originator = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
                let channel = match connections.last() {
                    Some(last) => last
                        .open_forward_channel(&host, port, originator)
                        .await
                        .map_err(|err| {
                            fail(
                                classify_failure(
                                    &err,
                                    FailurePhase::Forward,
                                    HopRole::Target,
                                    "",
                                    Interaction::default(),
                                ),
                                &err,
                            )
                        })?,
                    None => {
                        let err = ZsshError::InvalidInput("没有跳板机".into());
                        return Err(fail(
                            Failure {
                                stage: TestStage::Jump,
                                message: "没有跳板机".to_owned(),
                            },
                            &err,
                        ));
                    }
                };
                (Box::new(channel), connections)
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
                    "连上之后被对方重置了连接".to_owned()
                } else {
                    format!("连上之后读取失败：{}", one_line(&err.to_string()))
                };
                return Err((
                    Failure {
                        stage: TestStage::Connect,
                        message,
                    },
                    one_line(&err.to_string()),
                ));
            }
            Err(_) => FirstByte::Silent,
        };
        Ok::<_, Fail>(first)
    };
    let result = tokio::time::timeout(timeout, attempt).await;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    match result {
        Ok(Ok(first)) => {
            let headline = tcp_success_headline(elapsed_ms, first);
            let detail = format!("{header}\n结果：连接成功\n耗时：{elapsed_ms} ms\n{headline}");
            success(headline, detail, elapsed_ms)
        }
        Ok(Err((failure, raw))) => {
            if let Some(outcome) =
                untrusted_stop_outcome(&interactor, &header, &jump_labels, elapsed_ms)
            {
                return outcome;
            }
            let detail = format!(
                "{header}\n结果：失败（阶段：{}）\n耗时：{elapsed_ms} ms\n原因：{}\n原始错误：{raw}",
                failure.stage.label(),
                failure.message
            );
            failure_outcome(&failure, detail, elapsed_ms)
        }
        Err(_) => {
            let failure = timeout_failure(timeout, &interactor);
            let detail = format!(
                "{header}\n结果：失败（阶段：{}）\n耗时：{elapsed_ms} ms\n原因：{}",
                failure.stage.label(),
                failure.message
            );
            failure_outcome(&failure, detail, elapsed_ms)
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
                Interaction { prompted: kind },
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
            "连接成功，耗时 123 ms，服务器 SSH-2.0-OpenSSH_9.6"
        );
        assert_eq!(ssh_success_headline(5, None), "连接成功，耗时 5 ms");
        let first = untrusted_stop_headline("SHA256:abc", false, None);
        assert_eq!(
            first,
            "主机指纹 SHA256:abc 尚未信任，没有测试登录；请先正式连接一次并核对指纹"
        );
        let other = untrusted_stop_headline("SHA256:abc", true, None);
        assert!(other.contains("别种类型的密钥") && other.contains("没有测试登录"));
        let jump = untrusted_stop_headline("SHA256:abc", false, Some("跳板机 edge-1"));
        assert!(
            jump.starts_with("跳板机 edge-1：主机指纹 SHA256:abc 尚未信任"),
            "{jump}"
        );
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
        let outcome = failure_outcome(&f, "d".into(), 7);
        assert!(!outcome.ok);
        assert_eq!(outcome.stage, Some(TestStage::Auth));
        assert_eq!(outcome.headline, "认证失败：用户名或密码错误");
    }

    #[test]
    fn serial_results_name_the_device_and_the_reason() {
        let ok = serial_success("COM3", 12);
        assert!(ok.ok && ok.headline.contains("COM3 可以打开"));
        let bad = serial_failure("COM3", "COM3 不存在：设备没有插好，或者设备名写错了", 3);
        assert!(!bad.ok);
        assert_eq!(bad.stage, Some(TestStage::Open));
        assert!(bad.headline.contains("不存在"));
        assert!(bad.detail.contains("串口：COM3"));
        let busy = serial_failure("COM3", "COM3 被别的程序占用（或者没有访问权限）", 3);
        assert!(busy.headline.contains("被别的程序占用"));
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
        let (first, other_types) = seen.untrusted.as_ref().expect("记下了第一个没信任的指纹");
        assert_eq!(first.status, HostKeyStatus::Unknown);
        assert!(!other_types, "记的是第一个，不被后面的覆盖");
        assert_eq!(seen.prompted, Some(PromptKind::Password));
    }

    #[test]
    fn a_hung_stage_is_named_in_the_timeout_message() {
        let interactor = TestInteractor::new();
        interactor.set_stage(TestStage::Handshake);
        let f = timeout_failure(Duration::from_secs(10), &interactor);
        assert_eq!(f.stage, TestStage::Handshake);
        assert!(f.message.contains("10 秒") && f.message.contains("SSH 握手"));
    }
}

#[cfg(test)]
mod untrusted_host_tests {
    //! 「测试连接」不能把凭据发给没信任的主机（V2-C7a S1）：用进程内服务器数认证请求。

    use russh::keys::HashAlg;

    use super::*;
    use crate::ssh::auth::AuthConfig;
    use crate::ssh::fake_server::{self, FakeServer};
    use crate::ssh::known_hosts::KnownHosts;

    fn hop(label: &str, server: &FakeServer, known: &std::path::Path) -> ChainHop {
        ChainHop {
            label: label.to_owned(),
            host: server.addr.ip().to_string(),
            port: server.addr.port(),
            username: "tester".to_owned(),
            proxy: None,
            auth: AuthConfig::password(Some("never-leave-this-machine".to_owned())),
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

    const STOP_NOTE: &str = "没有测试登录；请先正式连接一次并核对指纹";

    #[tokio::test]
    async fn an_unknown_host_gets_a_handshake_and_a_fingerprint_but_no_secret() {
        let server = fake_server::spawn(vec![fake_server::ed25519_key()]).await;
        let dir = tempfile::tempdir().expect("临时目录");
        let known = dir.path().join("known_hosts");
        let outcome = test_ssh(SshTestPlan {
            hops: vec![hop("目标 lab-1", &server, &known)],
        })
        .await;
        assert_eq!(
            server.counters.secret_attempts(),
            0,
            "密码 / 键盘交互一次都不能发"
        );
        assert!(!outcome.ok, "没登录就不能报成功：{}", outcome.headline);
        assert!(outcome.headline.contains(STOP_NOTE), "{}", outcome.headline);
        assert!(outcome.headline.contains(&fingerprint(&server)));
        assert_eq!(outcome.untrusted_fingerprint, Some(fingerprint(&server)));
        assert!(outcome.detail.contains(&fingerprint(&server)));
        assert!(!outcome.detail.contains("never-leave-this-machine"));
        assert!(!known.exists(), "指纹也不能顺手保存");
    }

    #[tokio::test]
    async fn a_host_that_only_has_other_key_types_saved_also_gets_no_secret() {
        let server = fake_server::spawn(vec![fake_server::ed25519_key()]).await;
        let dir = tempfile::tempdir().expect("临时目录");
        let known = dir.path().join("known_hosts");
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
        assert_eq!(server.counters.secret_attempts(), 0);
        assert!(!outcome.ok);
        assert!(outcome.headline.contains(STOP_NOTE), "{}", outcome.headline);
        assert!(
            outcome.headline.contains("别种类型"),
            "{}",
            outcome.headline
        );
        assert_eq!(std::fs::read(&known).expect("读"), before);
    }

    #[tokio::test]
    async fn a_trusted_host_is_tested_for_real() {
        let server = fake_server::spawn(vec![fake_server::ed25519_key()]).await;
        let dir = tempfile::tempdir().expect("临时目录");
        let known = dir.path().join("known_hosts");
        trust(&server, &known);
        let outcome = test_ssh(SshTestPlan {
            hops: vec![hop("目标 lab-1", &server, &known)],
        })
        .await;
        assert!(outcome.ok, "{}", outcome.headline);
        assert_eq!(server.counters.passwords(), 1, "已信任：照常用密码登录");
        assert_eq!(outcome.untrusted_fingerprint, None);
        assert!(outcome.detail.contains("已信任"), "{}", outcome.detail);
    }

    #[tokio::test]
    async fn an_untrusted_target_behind_a_trusted_jump_host_gets_no_secret() {
        let jump = fake_server::spawn(vec![fake_server::ed25519_key()]).await;
        let target = fake_server::spawn(vec![fake_server::ed25519_key()]).await;
        let dir = tempfile::tempdir().expect("临时目录");
        let known = dir.path().join("known_hosts");
        trust(&jump, &known);
        let outcome = test_ssh(SshTestPlan {
            hops: vec![
                hop("跳板机 edge-1", &jump, &known),
                hop("目标 lab-1", &target, &known),
            ],
        })
        .await;
        assert_eq!(jump.counters.passwords(), 1, "已信任的跳板机照常登录");
        assert_eq!(target.counters.secret_attempts(), 0);
        assert!(!outcome.ok);
        assert!(outcome.headline.contains(STOP_NOTE), "{}", outcome.headline);
        assert_eq!(outcome.untrusted_fingerprint, Some(fingerprint(&target)));
    }

    #[tokio::test]
    async fn a_server_with_several_host_keys_shows_the_type_we_already_saved() {
        // 服务器有 Ed25519 和 ECDSA 两把主机密钥；这台主机以前只存过 ECDSA。
        // 默认顺序（Ed25519 优先）会出示 Ed25519，落到「只存过别种类型」而被拦下；
        // 把已保存的类型排到协商偏好最前，服务器就会出示 ECDSA，直接核对通过。
        let ecdsa = fake_server::ecdsa_key();
        let server = fake_server::spawn(vec![fake_server::ed25519_key(), ecdsa.clone()]).await;
        let dir = tempfile::tempdir().expect("临时目录");
        let known = dir.path().join("known_hosts");
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
        assert!(outcome.ok, "{}", outcome.headline);
        assert_eq!(server.counters.passwords(), 1);
        assert_eq!(outcome.untrusted_fingerprint, None);
    }

    #[tokio::test]
    async fn without_a_saved_key_the_default_order_still_applies() {
        let ecdsa = fake_server::ecdsa_key();
        let ed = fake_server::ed25519_key();
        let server = fake_server::spawn(vec![ecdsa, ed.clone()]).await;
        let dir = tempfile::tempdir().expect("临时目录");
        let known = dir.path().join("known_hosts");
        let outcome = test_ssh(SshTestPlan {
            hops: vec![hop("目标 lab-1", &server, &known)],
        })
        .await;
        // 没存过任何类型：保持默认的 Ed25519 优先，指纹就是 Ed25519 那把。
        assert_eq!(
            outcome.untrusted_fingerprint,
            Some(ed.public_key().fingerprint(HashAlg::Sha256).to_string())
        );
    }

    #[tokio::test]
    async fn an_untrusted_jump_host_stops_the_whole_chain_before_any_secret() {
        let jump = fake_server::spawn(vec![fake_server::ed25519_key()]).await;
        let target = fake_server::spawn(vec![fake_server::ed25519_key()]).await;
        let dir = tempfile::tempdir().expect("临时目录");
        let known = dir.path().join("known_hosts");
        // 目标已经信任了，但跳板机没有：跳板机之后的每一步都不能发秘密。
        trust(&target, &known);
        let outcome = test_ssh(SshTestPlan {
            hops: vec![
                hop("跳板机 edge-1", &jump, &known),
                hop("目标 lab-1", &target, &known),
            ],
        })
        .await;
        assert_eq!(jump.counters.secret_attempts(), 0);
        assert_eq!(
            target.counters.accepted(),
            0,
            "连跳板机都没过，不会去碰目标"
        );
        assert!(!outcome.ok);
        assert!(
            outcome.headline.contains("跳板机 edge-1"),
            "{}",
            outcome.headline
        );
        assert!(outcome.headline.contains(STOP_NOTE), "{}", outcome.headline);
    }
}
