//! 认证顺序：私钥 → agent → 密码 → keyboard-interactive。
//!
//! 只试服务器声明支持的方法（russh 认证失败时会带回剩余可用方法），每一步的网络
//! 往返都有超时；等用户输入的时间不算在超时里（提问走 [`Interactor`]）。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use russh::client::{AuthResult, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::{
    decode_secret_key, load_secret_key, Algorithm, HashAlg, PrivateKey, PrivateKeyWithHashAlg,
};
use russh::{MethodKind, MethodSet};
use tracing::{debug, info, warn};

use crate::error::{reason_keys as rk, AuthCauses, Reason, ZsshError};
use crate::model::AuthMethod;
use crate::ssh::connection::ClientHandler;
use crate::ssh::interact::{Interactor, PromptAnswers, PromptItem, PromptKind, PromptRequest};

/// 认证每一步的网络往返超时。
pub const AUTH_STEP_TIMEOUT: Duration = Duration::from_secs(15);
/// 连 agent（命名管道 / Pageant）的超时。russh 的命名管道连接在管道忙时会一直重试。
const AGENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// 存的密码被服务器拒绝后，重新询问密码时对话框里的说明（中英双语：核心库不知道界面语言）。
const SAVED_PASSWORD_REJECTED_NOTE: &str = "保存的密码被服务器拒绝，请重新输入。\nThe saved password was rejected by the server; please enter it again.";
/// keyboard-interactive 最多来回多少轮，防止服务器带着我们转圈。
const MAX_KEYBOARD_ROUNDS: usize = 20;

/// 主机的认证配置：用哪种方法，以及保险库里已经有的材料。
#[derive(Clone)]
pub struct AuthConfig {
    pub method: AuthMethod,
    /// `method = key` 时要用的私钥
    pub key: Option<KeySource>,
    /// 保险库里的密码；`None` 表示要问用户
    pub password: Option<String>,
}

impl AuthConfig {
    /// 用密码认证。`None` 表示保险库里没有，连接时问用户。
    pub fn password(password: Option<String>) -> Self {
        Self {
            method: AuthMethod::Password,
            key: None,
            password,
        }
    }

    /// 用私钥认证。
    pub fn key(source: KeySource) -> Self {
        Self {
            method: AuthMethod::Key,
            key: Some(source),
            password: None,
        }
    }

    /// 用 ssh-agent 认证。
    pub fn agent() -> Self {
        Self {
            method: AuthMethod::Agent,
            key: None,
            password: None,
        }
    }
}

/// `Debug` 手写：密码和口令不能进日志。
impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthConfig")
            .field("method", &self.method)
            .field("key", &self.key)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// 私钥从哪来。
#[derive(Clone)]
pub enum KeySource {
    /// 磁盘上的私钥文件
    File {
        path: PathBuf,
        passphrase: Option<String>,
    },
    /// 保险库里存的私钥内容
    Inline {
        pem: String,
        passphrase: Option<String>,
    },
}

impl std::fmt::Debug for KeySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File { path, passphrase } => f
                .debug_struct("KeySource::File")
                .field("path", path)
                .field("passphrase", &passphrase.as_ref().map(|_| "<redacted>"))
                .finish(),
            Self::Inline { passphrase, .. } => f
                .debug_struct("KeySource::Inline")
                .field("pem", &"<redacted>")
                .field("passphrase", &passphrase.as_ref().map(|_| "<redacted>"))
                .finish(),
        }
    }
}

/// 认证成功时，用户勾了"保存"的密码或口令。调用方负责落库。
#[derive(Clone)]
pub struct RememberedSecret {
    pub kind: PromptKind,
    pub secret: String,
}

impl std::fmt::Debug for RememberedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RememberedSecret")
            .field("kind", &self.kind)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// 一次认证的结果。
#[derive(Debug, Clone, Default)]
pub struct AuthOutcome {
    /// 认证成功、且用户勾了"保存"的密码 / 口令
    pub remembered: Option<RememberedSecret>,
}

/// agent 客户端统一成一种类型（命名管道和 Pageant 的流类型不一样）。
type AnyAgent = AgentClient<Box<dyn AgentStream + Send + Unpin + 'static>>;

/// 按设计文档 4.3 的顺序认证。
///
/// 只试服务器声明支持的方法；主机配置的方法失败之后，服务器还支持密码或
/// keyboard-interactive 就继续试。全部失败时报出试过哪些方法。
pub(crate) async fn authenticate(
    handle: &mut Handle<ClientHandler>,
    username: &str,
    target: &str,
    config: &AuthConfig,
    legacy_algorithms: bool,
    interactor: &Arc<dyn Interactor>,
) -> Result<AuthOutcome, ZsshError> {
    let mut outcome = AuthOutcome::default();

    // authenticate_none 不会真的认证，只用来拿服务器支持的方法列表。
    let mut supported = match step(handle.authenticate_none(username)).await? {
        AuthResult::Success => {
            info!("服务器接受 none 认证");
            return Ok(outcome);
        }
        AuthResult::Failure {
            remaining_methods, ..
        } => remaining_methods,
    };
    if supported.is_empty() {
        // 服务器没报可用方法：没有信息可用，按"都试一遍"更实用
        debug!("服务器没有声明可用的认证方法，按默认顺序尝试");
        supported = MethodSet::client_supported();
    } else {
        debug!(?supported, "服务器声明支持的认证方法");
    }

    // （中文说法, 稳定代码）：代码给界面按语言重新生成，见 `ZsshError::AllAuthFailed`。
    let mut tried: Vec<(String, String)> = Vec::new();
    let mut causes = AuthCauses::default();

    // 1. 私钥
    if config.method == AuthMethod::Key {
        match &config.key {
            Some(source) if supports(&supported, MethodKind::PublicKey) => {
                match publickey_auth(
                    handle,
                    username,
                    target,
                    source,
                    legacy_algorithms,
                    interactor,
                    &mut outcome,
                )
                .await?
                {
                    KeyAttempt::Answered(AuthResult::Success) => return Ok(outcome),
                    KeyAttempt::Answered(AuthResult::Failure {
                        remaining_methods,
                        partial_success,
                    }) => {
                        supported = remaining_methods;
                        // 部分成功（私钥过了、还要第二步）不算被拒：之后问验证码是「本来就要交互」。
                        if !partial_success {
                            interactor.credential_rejected();
                        }
                        let (name, code) = source_name(source);
                        tried.push((format!("私钥（{name}）"), code));
                    }
                    // 过程出错（不是服务器明确拒钥）：继续试后面的方法，但把原因留到最终报错里。
                    KeyAttempt::Errored(err) => {
                        interactor.credential_rejected();
                        let (name, code) = source_name(source);
                        tried.push((
                            format!("私钥（{name}，认证过程出错：{err}）"),
                            key_error_code(&code),
                        ));
                        causes.key = Some(err);
                    }
                }
            }
            Some(_) => {
                debug!("服务器不支持 publickey，跳过私钥");
                interactor.credential_rejected();
            }
            None => {
                warn!("主机配置成私钥认证但没有私钥材料");
                interactor.credential_rejected();
                tried.push(("私钥（主机没配私钥）".to_owned(), "key_missing".to_owned()));
            }
        }
    }

    // 2. ssh-agent
    if config.method == AuthMethod::Agent {
        if supports(&supported, MethodKind::PublicKey) {
            match agent_auth(handle, username, legacy_algorithms).await {
                Ok(Some(AuthResult::Success)) => return Ok(outcome),
                Ok(Some(AuthResult::Failure {
                    remaining_methods,
                    partial_success,
                })) => {
                    supported = remaining_methods;
                    if !partial_success {
                        interactor.credential_rejected();
                    }
                    tried.push((
                        "ssh-agent（密钥都不被接受）".to_owned(),
                        "agent_rejected".to_owned(),
                    ));
                }
                Ok(None) => {
                    interactor.credential_rejected();
                    tried.push((
                        "ssh-agent（没有可用密钥）".to_owned(),
                        "agent_no_keys".to_owned(),
                    ));
                }
                Err(err) => {
                    warn!(%err, "ssh-agent 用不了，继续试后面的方法");
                    interactor.credential_rejected();
                    tried.push((format!("ssh-agent（{err}）"), "agent_error".to_owned()));
                    causes.agent = Some(err);
                }
            }
        } else {
            debug!("服务器不支持 publickey，跳过 ssh-agent");
            interactor.credential_rejected();
        }
    }

    // 3. 密码
    if supports(&supported, MethodKind::Password) {
        let prompted = config.password.is_none();
        let password = match config.password.clone() {
            Some(password) => password,
            None => {
                let answers = interactor
                    .prompt(PromptRequest::single(
                        PromptKind::Password,
                        target,
                        "请输入密码",
                        true,
                    ))
                    .await
                    .ok_or(ZsshError::Cancelled)?;
                set_remembered(&mut outcome, PromptKind::Password, &answers);
                first_answer(&answers)
            }
        };

        // 被拒的是保险库里存的密码（不是用户这次现输的）：服务器上的密码多半后来改过，
        // 下面要让用户重新输一次，而不是直接判失败。
        let mut saved_password_rejected = false;
        match step(handle.authenticate_password(username, password)).await? {
            AuthResult::Success => {
                // 密码不对过的话，这时才轮到它被记下来
                if !prompted {
                    outcome.remembered = None;
                }
                return Ok(outcome);
            }
            AuthResult::Failure {
                remaining_methods,
                partial_success,
            } => {
                supported = remaining_methods;
                // 密码不对，别记了
                outcome.remembered = None;
                if !partial_success {
                    interactor.credential_rejected();
                    saved_password_rejected = !prompted;
                }
            }
        }
        tried.push(("密码".to_owned(), "password".to_owned()));

        // 存的密码被拒：按「每次询问」的流程再问用户一次，只问这一次。
        // 新输入的密码只用于这次连接——`can_remember = false`（界面不给「保存」勾选框），
        // 这里也不记进 `outcome.remembered`：要不要覆盖保险库里的旧密码是产品决定，不能静默回写。
        // 用户取消（或无人值守的实现不回答）就当这次密码认证失败，接着往下走，和以前一样。
        if saved_password_rejected && supports(&supported, MethodKind::Password) {
            let mut request =
                PromptRequest::single(PromptKind::Password, target, "请输入密码", false);
            request.instructions = SAVED_PASSWORD_REJECTED_NOTE.to_owned();
            if let Some(answers) = interactor.prompt(request).await {
                match step(handle.authenticate_password(username, first_answer(&answers))).await? {
                    AuthResult::Success => return Ok(outcome),
                    AuthResult::Failure {
                        remaining_methods, ..
                    } => supported = remaining_methods,
                }
            }
        }
    }

    // 4. keyboard-interactive（服务器发什么题就答什么，支持多轮）
    if supports(&supported, MethodKind::KeyboardInteractive) {
        match keyboard_interactive_auth(handle, username, target, interactor).await? {
            KeyboardOutcome::Success => return Ok(outcome),
            KeyboardOutcome::Failed => tried.push((
                "keyboard-interactive".to_owned(),
                "keyboard_interactive".to_owned(),
            )),
        }
    }

    Err(ZsshError::AllAuthFailed {
        tried: if tried.is_empty() {
            "没有可试的方法".to_owned()
        } else {
            tried
                .iter()
                .map(|(text, _)| text.as_str())
                .collect::<Vec<_>>()
                .join("、")
        },
        remaining: describe_methods(&supported),
        tried_codes: tried
            .iter()
            .map(|(_, code)| code.as_str())
            .collect::<Vec<_>>()
            .join("\u{1f}"),
        remaining_codes: method_codes(&supported),
        causes: (causes.agent.is_some() || causes.key.is_some()).then(|| Box::new(causes)),
    })
}

/// 认证每一步都套一层超时。等用户输入的时间不在里面（提问发生在调用之前）。
async fn step<T>(
    future: impl std::future::Future<Output = Result<T, russh::Error>>,
) -> Result<T, ZsshError> {
    match tokio::time::timeout(AUTH_STEP_TIMEOUT, future).await {
        Ok(result) => Ok(result?),
        Err(_) => Err(ZsshError::AuthTimeout {
            seconds: AUTH_STEP_TIMEOUT.as_secs(),
        }),
    }
}

fn supports(methods: &MethodSet, method: MethodKind) -> bool {
    methods.contains(&method)
}

/// 把方法列表写成给人看的文字（报错和日志里用）。
fn describe_methods(methods: &MethodSet) -> String {
    if methods.is_empty() {
        return "没有声明".to_owned();
    }
    methods
        .iter()
        .map(|method| match method {
            MethodKind::None => "none",
            MethodKind::Password => "密码",
            MethodKind::PublicKey => "公钥",
            MethodKind::HostBased => "host-based",
            MethodKind::KeyboardInteractive => "keyboard-interactive",
            MethodKind::GssapiWithMic => "gssapi",
        })
        .collect::<Vec<_>>()
        .join("、")
}

/// 私钥来源的（中文名, 稳定代码）：文件名是用户数据，进代码里原样保留（`key_file:<名字>`）。
fn source_name(source: &KeySource) -> (String, String) {
    match source {
        KeySource::File { path, .. } => match path.file_name() {
            Some(name) => {
                let name = name.to_string_lossy().into_owned();
                let code = format!("key_file:{name}");
                (name, code)
            }
            None => ("<私钥文件>".to_owned(), "key_file".to_owned()),
        },
        KeySource::Inline { .. } => ("保险库里的私钥".to_owned(), "key_vault".to_owned()),
    }
}

/// 服务器还支持的方法的稳定代码（`\u{1f}` 分隔）：没声明时是空串。
fn method_codes(methods: &MethodSet) -> String {
    methods
        .iter()
        .map(|method| match method {
            MethodKind::None => "none",
            MethodKind::Password => "password",
            MethodKind::PublicKey => "publickey",
            MethodKind::HostBased => "host-based",
            MethodKind::KeyboardInteractive => "keyboard-interactive",
            MethodKind::GssapiWithMic => "gssapi",
        })
        .collect::<Vec<_>>()
        .join("\u{1f}")
}

/// 一次私钥认证的结果。
enum KeyAttempt {
    /// 服务器给了明确答复（成功、或拒绝 / 部分成功）。
    Answered(AuthResult),
    /// 这次没得到答复，但连接还在、也不是超时（比如签名出错）：继续试别的方法，
    /// 错误留着，最终"全部失败"时带上。
    Errored(ZsshError),
}

/// 私钥认证过程出的错要不要直接上抛。
///
/// 连接已经断了、传输层出错、超时：后面的方法（密码等）也一定失败，最后用户只会看到
/// 别的错误，真正的原因就丢了，所以直接上抛。其他过程错误（签名失败这类只和这把钥
/// 匙有关的）才继续试后面的方法。`handle_closed` 来自 `Handle::is_closed`，比错误
/// 种类更可靠；认不得的错误种类按"继续试"处理，和原来的行为一致。
fn is_fatal_key_error(err: &ZsshError, handle_closed: bool) -> bool {
    if handle_closed {
        return true;
    }
    match err {
        ZsshError::AuthTimeout { .. } => true,
        ZsshError::Ssh(inner) => matches!(
            inner,
            russh::Error::Disconnect
                | russh::Error::HUP
                | russh::Error::ConnectionTimeout
                | russh::Error::KeepaliveTimeout
                | russh::Error::InactivityTimeout
                | russh::Error::SendError
                | russh::Error::RecvError
                | russh::Error::IO(_)
                | russh::Error::Elapsed(_)
                | russh::Error::Join(_)
                | russh::Error::PacketAuth
                | russh::Error::DecryptionError
                | russh::Error::PacketSize(_)
                | russh::Error::Kex
                | russh::Error::KexInit
                | russh::Error::StrictKeyExchangeViolation { .. }
        ),
        _ => false,
    }
}

/// 私钥那一项"认证过程出错"的稳定代码：文件名照旧跟在后面（`key_error:<名字>`）。
fn key_error_code(source_code: &str) -> String {
    match source_code.split_once(':') {
        Some(("key_file", name)) => format!("key_error:{name}"),
        _ => "key_error".to_owned(),
    }
}

/// 私钥认证。服务器明确答复（含拒绝）返回 `Answered`；过程出错但不致命返回 `Errored`；
/// 致命错误（断链、传输层出错、超时）直接 `Err`。
async fn publickey_auth(
    handle: &mut Handle<ClientHandler>,
    username: &str,
    target: &str,
    source: &KeySource,
    legacy_algorithms: bool,
    interactor: &Arc<dyn Interactor>,
    outcome: &mut AuthOutcome,
) -> Result<KeyAttempt, ZsshError> {
    let key = load_key(source, target, interactor, outcome).await?;
    let choice = rsa_hash_choice(handle, legacy_algorithms).await?;
    // 服务器只认 SHA-1 而用户没开兼容时直接报错，不往下试密码：
    // 不然原因会被后面的密码框盖住，用户不知道为什么私钥没用上。
    let hash_alg = hash_for_key(choice, &key.algorithm(), &source_name(source).0)?;

    let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg);
    match step(handle.authenticate_publickey(username, key)).await {
        // russh 在连接中途断掉时不报错，而是把"没等到答复"伪装成一次空方法列表的拒绝；
        // 连接已关就不是服务器拒钥，后面也不可能再成功。
        Ok(AuthResult::Failure { .. }) if handle.is_closed() => {
            warn!("私钥认证时连接断开了，不再试后面的方法");
            Err(ZsshError::Ssh(russh::Error::HUP))
        }
        Ok(result) => Ok(KeyAttempt::Answered(result)),
        Err(err) if is_fatal_key_error(&err, handle.is_closed()) => {
            warn!(%err, "私钥认证时连接出了问题，不再试后面的方法");
            Err(err)
        }
        Err(err) => {
            warn!(%err, "私钥认证过程出错，继续试后面的方法");
            Ok(KeyAttempt::Errored(err))
        }
    }
}

/// 服务器 server-sig-algs 对 RSA 签名的态度，决定 RSA 密钥用哪种哈希。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RsaHashChoice {
    /// 用这个哈希签名；`None` 就是 SHA-1 的 ssh-rsa。非 RSA 密钥会忽略它。
    Use(Option<HashAlg>),
    /// 服务器只接受 SHA-1 的 ssh-rsa，而这台主机没打开旧算法兼容。
    Sha1Refused,
}

/// 向服务器问 RSA 签名算法（russh 会等 SSH_MSG_EXT_INFO），再按兼容开关决定。
async fn rsa_hash_choice(
    handle: &mut Handle<ClientHandler>,
    legacy_algorithms: bool,
) -> Result<RsaHashChoice, ZsshError> {
    let advertised = step(handle.best_supported_rsa_hash()).await?;
    Ok(choose_rsa_hash(advertised, legacy_algorithms))
}

/// RSA 签名哈希的取舍，`advertised` 是 russh `best_supported_rsa_hash` 的结果。
///
/// - `Some(Some(_))`：服务器在 server-sig-algs 里列了 rsa-sha2-512/256，按它选的用
///   （russh 已经按 512 > 256 排好）。
/// - `Some(None)`：服务器只列了 ssh-rsa，明确不支持 SHA-2。默认不肯用 SHA-1 签名，
///   只有打开旧算法兼容才照旧用 SHA-1——和 `connection.rs` 里主机密钥算法的
///   "现代默认、显式兼容"一致。
/// - `None`：服务器没发 server-sig-algs（OpenSSH 7.2 之前没有这个扩展），或者列表里
///   没有任何 RSA 算法，russh 区分不了这两种。这时没有"只支持 SHA-1"的证据，仍按
///   原来的做法用 SHA-512：现在的服务器基本都接受；真是只认 SHA-1 的老服务器，
///   服务器会拒绝这次签名，认证按普通失败继续往下走，不会静默降级到 SHA-1。
fn choose_rsa_hash(advertised: Option<Option<HashAlg>>, legacy_algorithms: bool) -> RsaHashChoice {
    match advertised {
        Some(Some(hash)) => RsaHashChoice::Use(Some(hash)),
        Some(None) if legacy_algorithms => RsaHashChoice::Use(None),
        Some(None) => RsaHashChoice::Sha1Refused,
        None => RsaHashChoice::Use(Some(HashAlg::Sha512)),
    }
}

/// 把取舍落到具体密钥上：只有 RSA 密钥会受 SHA-1 限制，Ed25519 / ECDSA 照常用。
fn hash_for_key(
    choice: RsaHashChoice,
    algorithm: &Algorithm,
    key_label: &str,
) -> Result<Option<HashAlg>, ZsshError> {
    match choice {
        RsaHashChoice::Use(hash) => Ok(hash),
        RsaHashChoice::Sha1Refused if !matches!(algorithm, Algorithm::Rsa { .. }) => Ok(None),
        RsaHashChoice::Sha1Refused => Err(ZsshError::RsaSha1Refused {
            key: key_label.to_owned(),
        }),
    }
}

/// 第一次读私钥失败，是不是只因为「钥匙加了密、而没存口令」。
///
/// 只有这种情况，接下来问口令才是「本来就缺这项」；存了口令还打不开、文件读不到、
/// 内容坏了，都是凭据本身有问题。OpenSSH 格式没口令时报 `KeyIsEncrypted`；PKCS#8 / PEM / PPK
/// 加密的钥匙没口令时报的是普通解析错误，只能看文本头认出来。
fn only_the_passphrase_is_missing(
    source: &KeySource,
    saved: Option<&str>,
    first_error: &russh::keys::Error,
) -> bool {
    if saved.is_some() {
        return false;
    }
    if matches!(first_error, russh::keys::Error::KeyIsEncrypted) {
        return true;
    }
    match source {
        KeySource::File { path, .. } => std::fs::read_to_string(path)
            .map(|text| key_text_is_encrypted(&text))
            .unwrap_or(false),
        KeySource::Inline { pem, .. } => key_text_is_encrypted(pem),
    }
}

/// 私钥文本自己声明了加密：PKCS#8 `ENCRYPTED PRIVATE KEY`、传统 PEM 的 `Proc-Type: 4,ENCRYPTED`、
/// PuTTY PPK 的 `Encryption:` 不是 `none`。
pub(crate) fn key_text_is_encrypted(text: &str) -> bool {
    // 拆开写：整段私钥头会被公开前的隐私扫描当成私钥。
    text.contains(concat!("-----BEGIN ENCRYPTED ", "PRIVATE KEY-----"))
        || text.contains("Proc-Type: 4,ENCRYPTED")
        || text.lines().any(|line| {
            line.strip_prefix("Encryption:")
                .is_some_and(|value| !value.trim().eq_ignore_ascii_case("none"))
        })
}

/// 读私钥；口令不够（或者不对）就问用户。
async fn load_key(
    source: &KeySource,
    target: &str,
    interactor: &Arc<dyn Interactor>,
    outcome: &mut AuthOutcome,
) -> Result<PrivateKey, ZsshError> {
    type Loader = Box<dyn Fn(Option<&str>) -> Result<PrivateKey, russh::keys::Error> + Send>;

    let (load, passphrase): (Loader, Option<String>) = match source {
        KeySource::File { path, passphrase } => {
            let path = path.clone();
            (
                Box::new(move |pass| load_secret_key(&path, pass)),
                passphrase.clone(),
            )
        }
        KeySource::Inline { pem, passphrase } => {
            let pem = pem.clone();
            (
                Box::new(move |pass| decode_secret_key(&pem, pass)),
                passphrase.clone(),
            )
        }
    };

    match load(passphrase.as_deref()) {
        Ok(key) => {
            info!(algorithm = %key.algorithm(), "私钥已读入");
            Ok(key)
        }
        Err(first_error) => {
            // 保险库里的口令没有或者不对：问用户，答完再试一次
            debug!(error = %first_error, "私钥第一次读取失败，问用户要口令");
            if !only_the_passphrase_is_missing(source, passphrase.as_deref(), &first_error) {
                // 存的口令不对、文件读不到、钥匙坏了：这不是「没填口令」，问口令只是兜底重试。
                interactor.credential_rejected();
            }
            let answers = interactor
                .prompt(PromptRequest::single(
                    PromptKind::Passphrase,
                    target,
                    "请输入私钥口令",
                    true,
                ))
                .await
                .ok_or(ZsshError::Cancelled)?;
            let secret = first_answer(&answers);
            let key = load(Some(&secret)).map_err(|err| {
                ZsshError::KeyLoad(
                    Reason::new(
                        rk::KEY_PASSPHRASE_STILL_WRONG,
                        format!("用这个口令还是打不开私钥：{err}"),
                    )
                    .with("cause", &err),
                )
            })?;
            // 私钥能读出来说明口令是对的，认证失败也值得记下来
            set_remembered(outcome, PromptKind::Passphrase, &answers);
            info!(algorithm = %key.algorithm(), "私钥已读入");
            Ok(key)
        }
    }
}

/// 用户勾了"保存"就记下来，等认证成功之后由调用方落库。
fn set_remembered(outcome: &mut AuthOutcome, kind: PromptKind, answers: &PromptAnswers) {
    if answers.remember {
        outcome.remembered = Some(RememberedSecret {
            kind,
            secret: first_answer(answers),
        });
    }
}

fn first_answer(answers: &PromptAnswers) -> String {
    answers.answers.first().cloned().unwrap_or_default()
}

/// 依次试 agent 里的每一把公钥。
async fn agent_auth(
    handle: &mut Handle<ClientHandler>,
    username: &str,
    legacy_algorithms: bool,
) -> Result<Option<AuthResult>, ZsshError> {
    let mut agent = connect_agent().await.ok_or_else(|| {
        ZsshError::AgentUnavailable(Reason::new(
            rk::AGENT_PIPE_UNREACHABLE,
            "没连上 OpenSSH agent 的命名管道，也没连上 Pageant",
        ))
    })?;

    let identities = agent.request_identities().await.map_err(|err| {
        ZsshError::AgentUnavailable(
            Reason::new(
                rk::AGENT_LIST_FAILED,
                format!("列 agent 里的密钥失败：{err}"),
            )
            .with("cause", &err),
        )
    })?;
    if identities.is_empty() {
        return Ok(None);
    }
    info!(count = identities.len(), "agent 里的密钥数量");

    let choice = rsa_hash_choice(handle, legacy_algorithms).await?;
    let mut last: Option<AuthResult> = None;
    // agent 里常常混着 RSA 和 Ed25519：RSA 用不了就跳过，别的密钥照常试；
    // 一把都没试成时才把原因报出去。
    let mut refused: Option<ZsshError> = None;
    for identity in identities {
        let public = identity.public_key().into_owned();
        debug!(algorithm = %public.algorithm(), "试 agent 里的一把密钥");
        let hash_alg = match hash_for_key(choice, &public.algorithm(), "agent 里的 RSA 密钥") {
            Ok(hash_alg) => hash_alg,
            Err(err) => {
                warn!(%err, "跳过 agent 里的这把 RSA 密钥");
                refused = Some(err);
                continue;
            }
        };

        // 签名由 agent 完成，私钥不离开 agent
        match handle
            .authenticate_publickey_with(username, public, hash_alg, &mut agent)
            .await
        {
            Ok(AuthResult::Success) => return Ok(Some(AuthResult::Success)),
            Ok(failure @ AuthResult::Failure { .. }) => last = Some(failure),
            Err(err) => warn!(%err, "agent 签名失败，试下一把"),
        }
    }
    match (last, refused) {
        (None, Some(err)) => Err(err),
        (last, _) => Ok(last),
    }
}

/// Windows 用命名管道，失败再试 Pageant；Unix 用 `SSH_AUTH_SOCK`（设计文档 4.3）。
async fn connect_agent() -> Option<AnyAgent> {
    #[cfg(windows)]
    {
        let pipe = tokio::time::timeout(
            AGENT_CONNECT_TIMEOUT,
            AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent"),
        )
        .await;

        match pipe {
            Ok(Ok(client)) => {
                debug!("连上 OpenSSH agent 命名管道");
                return Some(client.dynamic());
            }
            Ok(Err(err)) => debug!(%err, "OpenSSH agent 命名管道连不上"),
            Err(_) => debug!("OpenSSH agent 命名管道连接超时"),
        }

        match AgentClient::connect_pageant().await {
            Ok(client) => {
                debug!("连上 Pageant");
                return Some(client.dynamic());
            }
            Err(err) => debug!(%err, "Pageant 连不上"),
        }
    }

    #[cfg(unix)]
    match tokio::time::timeout(AGENT_CONNECT_TIMEOUT, AgentClient::connect_env()).await {
        Ok(Ok(client)) => {
            debug!("连上 SSH_AUTH_SOCK agent");
            return Some(client.dynamic());
        }
        Ok(Err(err)) => debug!(%err, "SSH_AUTH_SOCK agent 连不上"),
        Err(_) => debug!("SSH_AUTH_SOCK agent 连接超时"),
    }

    None
}

enum KeyboardOutcome {
    Success,
    /// 服务器拒绝了，调用方继续看还有没有别的方法
    Failed,
}

/// keyboard-interactive：服务器问什么就原样转给前端，支持多轮。
async fn keyboard_interactive_auth(
    handle: &mut Handle<ClientHandler>,
    username: &str,
    target: &str,
    interactor: &Arc<dyn Interactor>,
) -> Result<KeyboardOutcome, ZsshError> {
    let mut response = step(handle.authenticate_keyboard_interactive_start(username, None)).await?;

    for _round in 0..MAX_KEYBOARD_ROUNDS {
        match response {
            KeyboardInteractiveAuthResponse::Success => return Ok(KeyboardOutcome::Success),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(KeyboardOutcome::Failed),
            KeyboardInteractiveAuthResponse::InfoRequest {
                name,
                instructions,
                prompts,
            } => {
                // 空提问按协议直接回空，不用打扰用户
                if prompts.is_empty() {
                    response =
                        step(handle.authenticate_keyboard_interactive_respond(Vec::new())).await?;
                    continue;
                }

                let request = PromptRequest {
                    kind: PromptKind::KeyboardInteractive,
                    target: target.to_owned(),
                    name,
                    instructions,
                    prompts: prompts
                        .iter()
                        .map(|prompt| PromptItem {
                            text: prompt.prompt.clone(),
                            echo: prompt.echo,
                        })
                        .collect(),
                    can_remember: false,
                };
                let answers = interactor
                    .prompt(request)
                    .await
                    .ok_or(ZsshError::Cancelled)?;
                if answers.answers.len() != prompts.len() {
                    return Err(ZsshError::InvalidInput(
                        Reason::new(
                            rk::ANSWER_COUNT_MISMATCH,
                            format!(
                                "回答数（{}）和提问数（{}）对不上",
                                answers.answers.len(),
                                prompts.len()
                            ),
                        )
                        .with("answers", answers.answers.len())
                        .with("prompts", prompts.len()),
                    ));
                }
                response =
                    step(handle.authenticate_keyboard_interactive_respond(answers.answers)).await?;
            }
        }
    }

    Err(ZsshError::AllAuthFailed {
        tried: format!("keyboard-interactive 超过 {MAX_KEYBOARD_ROUNDS} 轮"),
        remaining: "keyboard-interactive".to_owned(),
        tried_codes: format!("keyboard_rounds:{MAX_KEYBOARD_ROUNDS}"),
        remaining_codes: "keyboard-interactive".to_owned(),
        causes: None,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;
    use crate::ssh::interact::{BoxFuture, HostKeyDecision, HostKeyInfo};

    /// 预设回答的模拟交互，顺便记录被问了什么。
    #[derive(Debug)]
    struct MockInteractor {
        /// 指纹问题的回答
        host_key_answer: HostKeyDecision,
        /// 被问了几次指纹
        host_key_asks: AtomicUsize,
        /// 每次提问按顺序取一个回答；`None` 表示用户取消
        answers: Mutex<VecDeque<Option<String>>>,
        /// 记录收到的问题
        prompts: Mutex<Vec<PromptRequest>>,
    }

    impl MockInteractor {
        fn new(host_key_answer: HostKeyDecision, answers: Vec<Option<&str>>) -> Self {
            Self {
                host_key_answer,
                host_key_asks: AtomicUsize::new(0),
                answers: Mutex::new(
                    answers
                        .into_iter()
                        .map(|answer| answer.map(str::to_owned))
                        .collect(),
                ),
                prompts: Mutex::new(Vec::new()),
            }
        }

        fn asked_kinds(&self) -> Vec<PromptKind> {
            self.prompts
                .lock()
                .expect("锁")
                .iter()
                .map(|request| request.kind)
                .collect()
        }
    }

    impl Interactor for MockInteractor {
        fn confirm_host_key(&self, _info: HostKeyInfo) -> BoxFuture<'_, HostKeyDecision> {
            Box::pin(async move {
                self.host_key_asks.fetch_add(1, Ordering::Relaxed);
                self.host_key_answer
            })
        }

        fn prompt(&self, request: PromptRequest) -> BoxFuture<'_, Option<PromptAnswers>> {
            Box::pin(async move {
                self.prompts.lock().expect("锁").push(request);
                match self.answers.lock().expect("锁").pop_front() {
                    Some(Some(answer)) => Some(PromptAnswers {
                        answers: vec![answer],
                        remember: true,
                    }),
                    // 没有预设回答或者预设了 None：当作用户取消
                    Some(None) | None => None,
                }
            })
        }
    }

    /// 临时目录里的私钥文件，测试结束删掉。
    struct TempKey {
        path: PathBuf,
        /// 放在最后：目录守卫最后 drop，连同 .pub 一起删掉。
        _dir: crate::test_util::TestDir,
    }

    /// 用系统自带的 ssh-keygen 现生成一把钥匙，用完就删。
    ///
    /// 返回 `None` 表示这台机器上没有 ssh-keygen，调用方跳过。
    fn generate_key(tag: &str, passphrase: &str) -> Option<TempKey> {
        let dir = crate::test_util::TestDir::new(&format!("auth-{tag}"));
        let path = dir.join("key");

        let output = std::process::Command::new("ssh-keygen")
            .args(["-t", "ed25519", "-q", "-N", passphrase, "-f"])
            .arg(&path)
            .output()
            .ok()?;
        if !output.status.success() {
            println!(
                "跳过：ssh-keygen 没能生成密钥（{}）",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            return None;
        }
        Some(TempKey { path, _dir: dir })
    }

    #[test]
    fn loading_an_unencrypted_key_from_file() {
        let Some(key_file) = generate_key("plain", "") else {
            return;
        };
        let mock = Arc::new(MockInteractor::new(HostKeyDecision::TrustOnce, Vec::new()));
        let mut outcome = AuthOutcome::default();

        let key = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(load_key(
                &KeySource::File {
                    path: key_file.path.clone(),
                    passphrase: None,
                },
                "ops@web-1:22",
                &(Arc::clone(&mock) as Arc<dyn Interactor>),
                &mut outcome,
            ))
            .expect("没有口令的私钥应该直接读出来");

        assert_eq!(key.algorithm().to_string(), "ssh-ed25519");
        assert!(mock.asked_kinds().is_empty(), "能读出来就不该问用户");
        assert!(outcome.remembered.is_none());
    }

    #[test]
    fn passphrase_prompt_unlocks_an_encrypted_key() {
        let Some(key_file) = generate_key("encrypted", "test-passphrase-1") else {
            return;
        };
        let mock = Arc::new(MockInteractor::new(
            HostKeyDecision::TrustOnce,
            vec![Some("test-passphrase-1")],
        ));
        let mut outcome = AuthOutcome::default();

        let key = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(load_key(
                &KeySource::File {
                    path: key_file.path.clone(),
                    passphrase: None,
                },
                "ops@web-1:22",
                &(Arc::clone(&mock) as Arc<dyn Interactor>),
                &mut outcome,
            ))
            .expect("问出口令之后应该能读出来");

        assert_eq!(key.algorithm().to_string(), "ssh-ed25519");
        assert_eq!(mock.asked_kinds(), vec![PromptKind::Passphrase]);
        // 用户勾了保存：认证成功之后由调用方落库
        let remembered = outcome.remembered.expect("应该记下要保存的口令");
        assert_eq!(remembered.kind, PromptKind::Passphrase);
        assert_eq!(remembered.secret, "test-passphrase-1");
    }

    #[test]
    fn vault_passphrase_does_not_trigger_a_prompt() {
        let Some(key_file) = generate_key("vault-pass", "vault-passphrase") else {
            return;
        };
        let mock = Arc::new(MockInteractor::new(
            HostKeyDecision::TrustOnce,
            vec![Some("不该用到这个")],
        ));
        let mut outcome = AuthOutcome::default();

        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(load_key(
                &KeySource::File {
                    path: key_file.path.clone(),
                    passphrase: Some("vault-passphrase".to_owned()),
                },
                "ops@web-1:22",
                &(Arc::clone(&mock) as Arc<dyn Interactor>),
                &mut outcome,
            ))
            .expect("保险库里的口令应该够用");

        assert!(mock.asked_kinds().is_empty(), "保险库里有口令就不该弹窗");
    }

    #[test]
    fn the_passphrase_prompt_names_the_host_but_never_a_local_path() {
        let Some(key_file) = generate_key("target", "the-passphrase") else {
            return;
        };
        let mock = Arc::new(MockInteractor::new(
            HostKeyDecision::TrustOnce,
            vec![Some("the-passphrase")],
        ));
        let mut outcome = AuthOutcome::default();

        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(load_key(
                &KeySource::File {
                    path: key_file.path.clone(),
                    passphrase: None,
                },
                "ops@web-1:22",
                &(Arc::clone(&mock) as Arc<dyn Interactor>),
                &mut outcome,
            ))
            .expect("口令对，应该读出来");

        let prompts = mock.prompts.lock().expect("锁");
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].kind, PromptKind::Passphrase);
        assert_eq!(prompts[0].target, "ops@web-1:22");
        let path = key_file.path.to_string_lossy().into_owned();
        assert!(
            !format!("{:?}", prompts[0]).contains(&path),
            "提问里不能带本机的私钥路径"
        );
    }

    #[test]
    fn cancelling_the_passphrase_prompt_cancels_the_connection() {
        let Some(key_file) = generate_key("cancel", "some-passphrase") else {
            return;
        };
        let mock = Arc::new(MockInteractor::new(HostKeyDecision::TrustOnce, vec![None]));
        let mut outcome = AuthOutcome::default();

        let result = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(load_key(
                &KeySource::File {
                    path: key_file.path.clone(),
                    passphrase: None,
                },
                "ops@web-1:22",
                &(Arc::clone(&mock) as Arc<dyn Interactor>),
                &mut outcome,
            ));

        assert!(
            matches!(result, Err(ZsshError::Cancelled)),
            "取消应该终止连接，实际 {result:?}"
        );
    }

    #[test]
    fn wrong_passphrase_after_the_prompt_is_reported() {
        let Some(key_file) = generate_key("wrong", "right-passphrase") else {
            return;
        };
        let mock = Arc::new(MockInteractor::new(
            HostKeyDecision::TrustOnce,
            vec![Some("wrong-passphrase")],
        ));
        let mut outcome = AuthOutcome::default();

        let result = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(load_key(
                &KeySource::File {
                    path: key_file.path.clone(),
                    passphrase: None,
                },
                "ops@web-1:22",
                &(Arc::clone(&mock) as Arc<dyn Interactor>),
                &mut outcome,
            ));

        match result {
            Err(ZsshError::KeyLoad(message)) => {
                assert!(message.zh().contains("口令"), "错误信息要提口令：{message}")
            }
            other => panic!("应该是读私钥失败，实际 {other:?}"),
        }
    }

    #[test]
    fn inline_key_is_loaded_from_the_string() {
        let Some(key_file) = generate_key("inline", "") else {
            return;
        };
        let pem = std::fs::read_to_string(&key_file.path).expect("读私钥文本");
        let mock = Arc::new(MockInteractor::new(HostKeyDecision::TrustOnce, Vec::new()));
        let mut outcome = AuthOutcome::default();

        let key = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(load_key(
                &KeySource::Inline {
                    pem,
                    passphrase: None,
                },
                "ops@web-1:22",
                &(Arc::clone(&mock) as Arc<dyn Interactor>),
                &mut outcome,
            ))
            .expect("保险库里的私钥内容应该能读出来");

        assert_eq!(key.algorithm().to_string(), "ssh-ed25519");
    }

    #[tokio::test]
    async fn connecting_to_agent_never_hangs() {
        // 本机 agent 服务没跑的时候必须很快返回，不能卡在重试上
        let started = std::time::Instant::now();
        let agent = connect_agent().await;
        let elapsed = started.elapsed();
        println!(
            "agent 可用：{}，耗时 {:.1} 秒",
            agent.is_some(),
            elapsed.as_secs_f64()
        );
        assert!(
            elapsed < std::time::Duration::from_secs(30),
            "连 agent 不该一直挂着"
        );
    }

    #[test]
    fn rsa_hash_prefers_what_the_server_advertises() {
        for legacy in [false, true] {
            assert_eq!(
                choose_rsa_hash(Some(Some(HashAlg::Sha512)), legacy),
                RsaHashChoice::Use(Some(HashAlg::Sha512))
            );
            assert_eq!(
                choose_rsa_hash(Some(Some(HashAlg::Sha256)), legacy),
                RsaHashChoice::Use(Some(HashAlg::Sha256))
            );
        }
    }

    #[test]
    fn rsa_sha1_only_server_is_refused_unless_legacy() {
        assert_eq!(
            choose_rsa_hash(Some(None), false),
            RsaHashChoice::Sha1Refused,
            "默认不能因为服务器只列了 ssh-rsa 就用 SHA-1 签名"
        );
        assert_eq!(
            choose_rsa_hash(Some(None), true),
            RsaHashChoice::Use(None),
            "兼容模式保持原样：服务器只认 ssh-rsa 时用 SHA-1"
        );
    }

    #[test]
    fn rsa_hash_without_server_sig_algs_defaults_to_sha512() {
        for legacy in [false, true] {
            assert_eq!(
                choose_rsa_hash(None, legacy),
                RsaHashChoice::Use(Some(HashAlg::Sha512)),
                "没有 server-sig-algs 时没有只认 SHA-1 的证据，按现代默认"
            );
        }
    }

    #[test]
    fn sha1_refusal_only_affects_rsa_keys() {
        let rsa = Algorithm::Rsa { hash: None };
        let error = hash_for_key(RsaHashChoice::Sha1Refused, &rsa, "id_rsa")
            .expect_err("RSA 密钥遇到只认 SHA-1 的服务器要报错");
        assert!(
            matches!(&error, ZsshError::RsaSha1Refused { key } if key == "id_rsa"),
            "实际：{error:?}"
        );
        let message = error.to_string();
        assert!(message.contains("id_rsa"), "要说清是哪把密钥：{message}");
        assert!(message.contains("SHA-1"), "要说清原因：{message}");
        assert!(
            message.contains("旧算法兼容"),
            "要告诉用户可以打开兼容开关：{message}"
        );

        for other in [
            Algorithm::Ed25519,
            Algorithm::Ecdsa {
                curve: russh::keys::EcdsaCurve::NistP256,
            },
        ] {
            assert_eq!(
                hash_for_key(RsaHashChoice::Sha1Refused, &other, "id_other").ok(),
                Some(None),
                "{other:?} 不受影响"
            );
        }

        assert_eq!(
            hash_for_key(RsaHashChoice::Use(Some(HashAlg::Sha256)), &rsa, "id_rsa").ok(),
            Some(Some(HashAlg::Sha256))
        );
        assert_eq!(
            hash_for_key(RsaHashChoice::Use(None), &rsa, "id_rsa").ok(),
            Some(None),
            "兼容模式下照旧用 SHA-1"
        );
    }

    #[test]
    fn only_connection_level_failures_stop_the_remaining_methods() {
        let ssh = |err: russh::Error| ZsshError::Ssh(err);
        let io = || {
            russh::Error::IO(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "reset",
            ))
        };
        // 断链、传输层出错、超时：后面的方法一定也失败，要直接上抛。
        for fatal in [
            ZsshError::AuthTimeout { seconds: 15 },
            ssh(russh::Error::Disconnect),
            ssh(russh::Error::HUP),
            ssh(russh::Error::SendError),
            ssh(russh::Error::RecvError),
            ssh(russh::Error::ConnectionTimeout),
            ssh(russh::Error::KeepaliveTimeout),
            ssh(io()),
            ssh(russh::Error::DecryptionError),
        ] {
            assert!(is_fatal_key_error(&fatal, false), "应上抛：{fatal:?}");
        }
        // 只和这把钥匙有关的过程错误：照旧继续试后面的方法。
        for local in [
            ssh(russh::Error::CouldNotReadKey),
            ssh(russh::Error::RequestDenied),
            ssh(russh::Error::UnsupportedAuthMethod),
            ssh(russh::Error::NoAuthMethod),
            ZsshError::Cancelled,
        ] {
            assert!(!is_fatal_key_error(&local, false), "应继续：{local:?}");
        }
        // 句柄已经关了：不管错误长什么样都是断链。
        assert!(is_fatal_key_error(
            &ssh(russh::Error::CouldNotReadKey),
            true
        ));
    }

    #[test]
    fn key_error_code_keeps_the_file_name_only() {
        assert_eq!(
            key_error_code("key_file:id_ed25519"),
            "key_error:id_ed25519"
        );
        assert_eq!(key_error_code("key_file"), "key_error");
        assert_eq!(key_error_code("key_vault"), "key_error");
    }

    /// 进程内服务器上的整条认证流程：私钥被拒要照旧回退，断链要直接上抛。
    mod against_server {
        use russh::keys::ssh_key::LineEnding;

        use super::*;
        use crate::ssh::connection::{ConnectParams, Connection};
        use crate::ssh::fake_server::{self, AuthMode};
        use crate::ssh::known_hosts::KnownHosts;

        fn vault_key() -> KeySource {
            let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("生成密钥");
            KeySource::Inline {
                pem: key
                    .to_openssh(LineEnding::LF)
                    .expect("导出密钥")
                    .to_string(),
                passphrase: None,
            }
        }

        /// 用私钥认证连一台指定行为的服务器；`password` 是被问到密码时的回答。
        async fn connect_with_key(
            mode: AuthMode,
            password: Option<&str>,
        ) -> (
            fake_server::FakeServer,
            Arc<MockInteractor>,
            Result<Connection, ZsshError>,
        ) {
            let server = fake_server::spawn_with(vec![fake_server::ed25519_key()], mode).await;
            let dir = crate::test_util::TestDir::new("auth-server");
            let interactor = Arc::new(MockInteractor::new(
                HostKeyDecision::TrustOnce,
                vec![password],
            ));
            let result = Connection::connect(
                ConnectParams {
                    host: server.addr.ip().to_string(),
                    port: server.addr.port(),
                    username: "tester".to_owned(),
                    auth: AuthConfig::key(vault_key()),
                    known_hosts: KnownHosts::new(dir.join("known_hosts")),
                    legacy_algorithms: false,
                },
                interactor.clone(),
            )
            .await;
            (server, interactor, result)
        }

        /// 保险库里存着 `saved` 的密码连一台只认 `correct` 的服务器；`answers` 是被问到密码时的回答。
        async fn connect_with_saved_password(
            correct: &'static str,
            saved: &str,
            answers: Vec<Option<&str>>,
        ) -> (
            fake_server::FakeServer,
            Arc<MockInteractor>,
            Result<Connection, ZsshError>,
        ) {
            let server = fake_server::spawn_with(
                vec![fake_server::ed25519_key()],
                AuthMode::OnlyThisPassword(correct),
            )
            .await;
            let dir = crate::test_util::TestDir::new("auth-saved-password");
            let interactor = Arc::new(MockInteractor::new(HostKeyDecision::TrustOnce, answers));
            let result = Connection::connect(
                ConnectParams {
                    host: server.addr.ip().to_string(),
                    port: server.addr.port(),
                    username: "tester".to_owned(),
                    auth: AuthConfig::password(Some(saved.to_owned())),
                    known_hosts: KnownHosts::new(dir.join("known_hosts")),
                    legacy_algorithms: false,
                },
                interactor.clone(),
            )
            .await;
            (server, interactor, result)
        }

        #[tokio::test]
        async fn a_rejected_saved_password_asks_the_user_once_and_does_not_write_back() {
            let (server, interactor, result) =
                connect_with_saved_password("new-pw", "old-pw", vec![Some("new-pw")]).await;
            let connection = result.expect("重新输入正确的密码后应该连上");
            assert_eq!(server.counters.passwords(), 2, "存的密码一次，新输入的一次");
            assert_eq!(interactor.asked_kinds(), vec![PromptKind::Password]);
            let request = interactor.prompts.lock().expect("锁")[0].clone();
            assert!(!request.can_remember, "不能让界面给「保存」勾选框");
            assert!(
                request.instructions.contains("保存的密码被服务器拒绝"),
                "要告诉用户为什么又问一次：{}",
                request.instructions
            );
            assert!(
                connection.remembered_secret().is_none(),
                "新密码只用于这次连接，不能交给调用方回写"
            );
        }

        #[tokio::test]
        async fn a_correct_saved_password_never_asks() {
            let (server, interactor, result) =
                connect_with_saved_password("same-pw", "same-pw", vec![Some("unused")]).await;
            assert!(result.is_ok(), "{:?}", result.err());
            assert_eq!(server.counters.passwords(), 1);
            assert!(interactor.asked_kinds().is_empty());
        }

        #[tokio::test]
        async fn declining_the_second_ask_is_an_ordinary_password_failure() {
            let (server, interactor, result) =
                connect_with_saved_password("new-pw", "old-pw", vec![None]).await;
            let Err(ZsshError::AllAuthFailed { tried_codes, .. }) = result else {
                panic!("用户不输入就是密码认证失败");
            };
            assert_eq!(tried_codes, "password");
            assert_eq!(server.counters.passwords(), 1);
            assert_eq!(interactor.asked_kinds().len(), 1);
        }

        #[tokio::test]
        async fn the_second_ask_is_not_repeated_when_the_new_password_is_also_wrong() {
            let (server, interactor, result) = connect_with_saved_password(
                "new-pw",
                "old-pw",
                vec![Some("still-wrong"), Some("never-asked")],
            )
            .await;
            assert!(matches!(result, Err(ZsshError::AllAuthFailed { .. })));
            assert_eq!(server.counters.passwords(), 2);
            assert_eq!(interactor.asked_kinds().len(), 1, "只再问一次，不循环");
        }

        #[tokio::test]
        async fn a_rejected_key_still_falls_back_to_the_password() {
            let (server, interactor, result) = connect_with_key(
                AuthMode::PublickeyRejectedPasswordAllowed,
                Some("pw-for-test"),
            )
            .await;
            assert!(result.is_ok(), "{:?}", result.err());
            assert_eq!(server.counters.publickey.load(Ordering::SeqCst), 1);
            assert_eq!(server.counters.passwords(), 1, "拒钥之后要回退到密码");
            assert_eq!(interactor.asked_kinds(), vec![PromptKind::Password]);
        }

        #[tokio::test]
        async fn a_rejected_key_on_a_publickey_only_server_reports_all_methods_failed() {
            let (server, _interactor, result) =
                connect_with_key(AuthMode::PublickeyOnlyRejecting, None).await;
            let Err(ZsshError::AllAuthFailed {
                tried,
                tried_codes,
                causes,
                ..
            }) = result
            else {
                panic!("应该是认证全部失败");
            };
            assert!(tried.contains("私钥"), "{tried}");
            assert_eq!(tried_codes, "key_vault", "明确被拒不算过程出错");
            assert!(causes.is_none());
            assert_eq!(server.counters.passwords(), 0, "服务器没声明密码就不试");
        }

        #[tokio::test]
        async fn a_connection_lost_during_key_auth_is_reported_not_hidden() {
            let (server, interactor, result) =
                connect_with_key(AuthMode::DropOnPublickey, Some("pw-for-test")).await;
            let err = match result {
                Err(err) => err,
                Ok(_) => panic!("掉线了不能算认证成功"),
            };
            assert!(
                matches!(err, ZsshError::Ssh(russh::Error::HUP)),
                "应上抛断链原因，实际：{err:?}"
            );
            assert_eq!(server.counters.publickey.load(Ordering::SeqCst), 1);
            assert_eq!(server.counters.passwords(), 0, "断链后不该再试密码");
            assert!(interactor.asked_kinds().is_empty(), "也不该再问用户要密码");
        }
    }
}
