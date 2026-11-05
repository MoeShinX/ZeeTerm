//! 核心库错误的界面文案（V2-I18N 的最后一块）。
//!
//! 核心库只给出稳定错误码和结构化参数（`zssh_core::error::ErrorWithCode`）；这里按当前
//! 界面语言挑模板、把参数填进去。核心库的 `Display`（中文原文）留给日志、诊断和兜底：
//! 认不得的错误码就用原文上屏，宁可显示中文，也不能丢信息或露一个 `{path}` 占位符。
//!
//! 模板里的占位符和 `params()` 的键名一一对应；错误码是契约，见核心库的
//! `ErrorWithCode::code()`。
//!
//! 带 `Reason` 的错误（`InvalidInput`、`KeyLoad`、`Shell`……）的 `detail` 不再是核心里写死的
//! 中文：参数里有 `reason`（稳定 key），英文界面按 key 取 `reason_text` 里的整句；中文界面直接
//! 用核心给的中文原文（和以前逐字相同）。拨号链的 `Hop` 带着内层错误，这里递归生成整句。

use zssh_core::error::ErrorWithCode;

use crate::i18n::{Lang, Msg, current};

/// 一条核心错误的中英文模板。一个变体对应一个错误码（或少数措辞完全相同的码）。
#[derive(Clone, Copy, Debug)]
enum CoreErrorMsg {
    QuickTargetMissingUser,
    QuickTargetEmptyHost,
    QuickTargetBadPort,
    QuickTargetFormat,
    DnsFailed,
    ConnectTimeout,
    ProxyFailed,
    HopFailed,
    ConnectFailed,
    SshProtocolError,
    MissingServerKey,
    AuthFailed,
    ShellFailed,
    X11Failed,
    SftpUnavailableSys,
    ChannelLimit,
    DatabaseError,
    NotFoundEntity,
    InvalidInput,
    Internal,
    IdentityInUse,
    ProxyInUse,
    SecretError,
    CoreCancelled,
    AuthTimeout,
    AllAuthFailed,
    KeyLoadFailed,
    AgentUnavailable,
    RsaSha1Refused,
    KnownHostsFailed,
    HostKeyRejected,
    HostKeyChanged,
    HostKeyRevoked,
    HostKeyRekeyMismatch,
    NoCommonAlgorithm,
    SftpReplaceFailed,
    SftpNoSubsystem,
    SftpPermissionDenied,
    SftpNotFound,
    SftpAlreadyExists,
    SftpRemoteNoSpace,
    SftpLocalNoSpace,
    SftpDisconnected,
    SftpNonUtf8Name,
    SftpTooLarge,
    SftpRemoteStatus,
    SftpRemoteError,
    SftpLocalError,
    SftpTargetBusy,
    SftpRenameTargetExists,
    SftpNoFreeName,
    SftpCancelled,
}

impl Msg for CoreErrorMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::QuickTargetMissingUser => "快速连接串缺少用户名，格式应为 user@host[:port]",
            Self::QuickTargetEmptyHost => "快速连接串的主机名为空",
            Self::QuickTargetBadPort => "快速连接串的端口非法：{port}（应为 1-65535）",
            Self::QuickTargetFormat => "无法解析快速连接串：{detail}",
            Self::DnsFailed => "无法解析主机名 {host}: {detail}",
            Self::ConnectTimeout => "连接 {host}:{port} 超时（{seconds} 秒）",
            Self::ProxyFailed => "代理 {proxy}：{detail}",
            Self::HopFailed => "第 {index} 跳（{label}）：{detail}",
            Self::ConnectFailed => "连接 {host}:{port} 失败（试过 {tried}）：{detail}",
            Self::SshProtocolError => "SSH 协议错误：{detail}",
            Self::MissingServerKey => "服务器没有提供主机密钥",
            Self::AuthFailed => "认证失败：用户名或密码错误",
            Self::ShellFailed => "打开 shell 失败：{detail}",
            Self::X11Failed => "X11 转发没有开起来：{detail}",
            Self::SftpUnavailableSys => {
                "这台服务器没有开启 SFTP：sys 子系统请求被拒。请确认服务器装了 sftp 子系统（OpenSSH 的 /usr/lib/openssh/sftp-server，配置里 `Subsystem sftp` 没有被注释掉）"
            }
            Self::ChannelLimit => "服务器拒绝了新的通道（可能已达 MaxSessions 上限）：{detail}",
            Self::DatabaseError => "数据库错误：{detail}",
            Self::NotFoundEntity => "找不到{entity}（id={id}）",
            Self::InvalidInput => "输入不合法：{detail}",
            Self::Internal => "内部错误：{detail}",
            Self::IdentityInUse => "凭据「{name}」还被这些主机引用：{hosts}",
            Self::ProxyInUse => "代理「{name}」还被这些主机引用：{hosts}",
            Self::SecretError => "敏感信息处理失败：{detail}",
            Self::CoreCancelled => "已取消",
            Self::AuthTimeout => "认证超时（每一步最多 {seconds} 秒）",
            Self::AllAuthFailed => "认证失败，试过这些方法：{tried}\n服务器还支持：{remaining}",
            Self::KeyLoadFailed => "读私钥失败：{detail}",
            Self::AgentUnavailable => "ssh-agent 用不了：{detail}",
            Self::RsaSha1Refused => {
                "服务器只接受 SHA-1 的 ssh-rsa 签名，{key}是 RSA 密钥，默认不再用 SHA-1 签名。         可以在主机设置的\"高级\"里打开旧算法兼容，或者换用 Ed25519 / ECDSA 密钥"
            }
            Self::KnownHostsFailed => "known_hosts 读写失败：{detail}",
            Self::HostKeyRejected => "主机 {host}:{port} 的密钥被拒绝（指纹 {fingerprint}）",
            Self::HostKeyChanged => {
                "主机 {host}:{port} 的 {key_type} 密钥指纹和已保存的不一样\n已保存：{stored}（记录在 {stored_in}）\n本次收到：{presented}\n{advice}"
            }
            Self::HostKeyRevoked => {
                "主机 {host}:{port} 的密钥被标记为已吊销（@revoked）\n记录：{stored}（记录在 {stored_in}）\n本次收到：{presented}\n{advice}"
            }
            Self::HostKeyRekeyMismatch => {
                "rekey 时主机密钥变了：第一次 {first}，这次 {again}，已断开"
            }
            Self::NoCommonAlgorithm => {
                "和 {host}:{port} 协商不出共同的 {kind} 算法\n服务器提供：{theirs}\n我们提供：{ours}\n老服务器可以在主机设置的\"高级\"里打开旧算法兼容"
            }
            Self::SftpReplaceFailed => "{message}",
            Self::SftpNoSubsystem => {
                "这台服务器没有开启 SFTP：子系统请求被拒。请确认服务器装了 sftp 子系统（OpenSSH 的 /usr/lib/openssh/sftp-server，配置里 `Subsystem sftp` 没有被注释掉；通道在握手时断开的 话也可能是网络断了——{detail}）"
            }
            Self::SftpPermissionDenied => "没有权限：{path}",
            Self::SftpNotFound => "文件不存在：{path}",
            Self::SftpAlreadyExists => "已经存在：{path}",
            Self::SftpRemoteNoSpace => "远端磁盘空间不足：{path}",
            Self::SftpLocalNoSpace => "本地磁盘空间不足：{path}",
            Self::SftpDisconnected => "连接断了：{detail}",
            Self::SftpNonUtf8Name => "这个文件名不是 UTF-8（{name}），当前实现不能用原始字节操作它",
            Self::SftpTooLarge => "{path} 有 {size} 字节，超过 {limit} 字节的上限",
            Self::SftpRemoteStatus => "远端返回了不认识的错误码（{code}）：{message}",
            Self::SftpRemoteError => "远端错误：{detail}",
            Self::SftpLocalError => "本地文件操作失败（{path}）：{message}",
            Self::SftpTargetBusy => "另一个传输任务正在写 {path}，等它结束后再试",
            Self::SftpRenameTargetExists => "新名字 {name} 也已存在，请换一个名字再试",
            Self::SftpNoFreeName => {
                "{path} 旁边的自动改名（试了 {tried} 个名字）都已被占用，请手动换个名字，或先清理同名文件"
            }
            Self::SftpCancelled => "操作被取消",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::QuickTargetMissingUser => {
                "Quick connect is missing a user name (expected user@host[:port])"
            }
            Self::QuickTargetEmptyHost => "Quick connect has an empty host name",
            Self::QuickTargetBadPort => {
                "Quick connect has an invalid port: {port} (expected 1-65535)"
            }
            Self::QuickTargetFormat => "Cannot parse the quick connect string: {detail}",
            Self::DnsFailed => "Cannot resolve host name {host}: {detail}",
            Self::ConnectTimeout => "Timed out connecting to {host}:{port} ({seconds}s)",
            Self::ProxyFailed => "Proxy {proxy}: {detail}",
            Self::HopFailed => "Hop {index} ({label}): {detail}",
            Self::ConnectFailed => "Failed to connect to {host}:{port} (tried {tried}): {detail}",
            Self::SshProtocolError => "SSH protocol error: {detail}",
            Self::MissingServerKey => "The server did not offer a host key",
            Self::AuthFailed => "Authentication failed: wrong user name or password",
            Self::ShellFailed => "Failed to open a shell: {detail}",
            Self::X11Failed => "Could not start X11 forwarding: {detail}",
            Self::SftpUnavailableSys => {
                "This server has not enabled SFTP: the sys subsystem request was refused. Make sure the server has the sftp subsystem installed (OpenSSH's /usr/lib/openssh/sftp-server, with `Subsystem sftp` uncommented in the server config)"
            }
            Self::ChannelLimit => {
                "The server refused a new channel (possibly the MaxSessions limit): {detail}"
            }
            Self::DatabaseError => "Database error: {detail}",
            Self::NotFoundEntity => "Cannot find {entity} (id={id})",
            Self::InvalidInput => "Invalid input: {detail}",
            Self::Internal => "Internal error: {detail}",
            Self::IdentityInUse => "Credential \"{name}\" is still used by these hosts: {hosts}",
            Self::ProxyInUse => "Proxy \"{name}\" is still used by these hosts: {hosts}",
            Self::SecretError => "Failed to handle secret data: {detail}",
            Self::CoreCancelled => "Cancelled",
            Self::AuthTimeout => "Authentication timed out (at most {seconds}s per step)",
            Self::AllAuthFailed => {
                "Authentication failed; tried these methods: {tried}\nThe server also supports: {remaining}"
            }
            Self::KeyLoadFailed => "Failed to read the private key: {detail}",
            Self::AgentUnavailable => "ssh-agent is unavailable: {detail}",
            Self::RsaSha1Refused => {
                "The server only accepts SHA-1 ssh-rsa signatures, and {key} is an RSA key; SHA-1 signing is disabled by default. Enable legacy algorithm support in the host's Advanced settings, or use an Ed25519 / ECDSA key"
            }
            Self::KnownHostsFailed => "Failed to read or write known_hosts: {detail}",
            Self::HostKeyRejected => {
                "The host key for {host}:{port} was rejected (fingerprint {fingerprint})"
            }
            Self::HostKeyChanged => {
                "The {key_type} host key fingerprint for {host}:{port} does not match the saved one\nSaved: {stored} (recorded in {stored_in})\nReceived: {presented}\n{advice}"
            }
            Self::HostKeyRevoked => {
                "The host key for {host}:{port} is marked as revoked (@revoked)\nRecorded: {stored} (recorded in {stored_in})\nReceived: {presented}\n{advice}"
            }
            Self::HostKeyRekeyMismatch => {
                "The host key changed during rekey: first {first}, now {again}; disconnected"
            }
            Self::NoCommonAlgorithm => {
                "Cannot negotiate a common {kind} algorithm with {host}:{port}\nServer offers: {theirs}\nWe offer: {ours}\nFor older servers, enable legacy algorithm support in the host's Advanced settings"
            }
            Self::SftpReplaceFailed => "{message}",
            Self::SftpNoSubsystem => {
                "This server has not enabled SFTP: the subsystem request was refused. Make sure the server has the sftp subsystem installed (OpenSSH's /usr/lib/openssh/sftp-server, with `Subsystem sftp` uncommented in the server config; if the channel dropped during the handshake, the network may also be down — {detail})"
            }
            Self::SftpPermissionDenied => "Permission denied: {path}",
            Self::SftpNotFound => "No such file or directory: {path}",
            Self::SftpAlreadyExists => "Already exists: {path}",
            Self::SftpRemoteNoSpace => "Not enough space on the remote disk: {path}",
            Self::SftpLocalNoSpace => "Not enough space on the local disk: {path}",
            Self::SftpDisconnected => "Connection lost: {detail}",
            Self::SftpNonUtf8Name => {
                "This file name is not UTF-8 ({name}); the current implementation cannot operate on it with the original bytes"
            }
            Self::SftpTooLarge => "{path} is {size} bytes, over the {limit}-byte limit",
            Self::SftpRemoteStatus => {
                "The remote returned an unrecognized status code ({code}): {message}"
            }
            Self::SftpRemoteError => "Remote error: {detail}",
            Self::SftpLocalError => "Local file operation failed ({path}): {message}",
            Self::SftpTargetBusy => {
                "Another transfer is writing {path}; try again after it finishes"
            }
            Self::SftpRenameTargetExists => {
                "The new name {name} already exists; choose another name"
            }
            Self::SftpNoFreeName => {
                "Every automatic rename near {path} (tried {tried} names) is taken; choose a name manually or clean up the same-name files first"
            }
            Self::SftpCancelled => "Operation cancelled",
        }
    }
}

/// 错误码 → 模板。认不得的码返回 `None`，由调用方退回核心库原文。
fn message_for(code: &str) -> Option<CoreErrorMsg> {
    Some(match code {
        "ssh.quick_target_missing_user" => CoreErrorMsg::QuickTargetMissingUser,
        "ssh.quick_target_empty_host" => CoreErrorMsg::QuickTargetEmptyHost,
        "ssh.quick_target_bad_port" => CoreErrorMsg::QuickTargetBadPort,
        "ssh.quick_target_format" => CoreErrorMsg::QuickTargetFormat,
        "ssh.dns_failed" => CoreErrorMsg::DnsFailed,
        "ssh.connect_timeout" => CoreErrorMsg::ConnectTimeout,
        "ssh.proxy_failed" => CoreErrorMsg::ProxyFailed,
        "ssh.hop_failed" => CoreErrorMsg::HopFailed,
        "ssh.connect_failed" => CoreErrorMsg::ConnectFailed,
        "ssh.protocol_error" => CoreErrorMsg::SshProtocolError,
        "ssh.missing_host_key" => CoreErrorMsg::MissingServerKey,
        "ssh.auth_failed" => CoreErrorMsg::AuthFailed,
        "ssh.shell_failed" => CoreErrorMsg::ShellFailed,
        "ssh.x11_forward_failed" => CoreErrorMsg::X11Failed,
        "ssh.sftp_unavailable" => CoreErrorMsg::SftpUnavailableSys,
        "ssh.channel_limit" | "sftp.channel_limit" => CoreErrorMsg::ChannelLimit,
        "store.database_error" => CoreErrorMsg::DatabaseError,
        "store.not_found" => CoreErrorMsg::NotFoundEntity,
        "core.invalid_input" | "sftp.invalid_input" => CoreErrorMsg::InvalidInput,
        "core.internal" | "sftp.internal" => CoreErrorMsg::Internal,
        "store.identity_in_use" => CoreErrorMsg::IdentityInUse,
        "store.proxy_in_use" => CoreErrorMsg::ProxyInUse,
        "store.secret_error" => CoreErrorMsg::SecretError,
        "core.cancelled" => CoreErrorMsg::CoreCancelled,
        "ssh.auth_timeout" => CoreErrorMsg::AuthTimeout,
        "ssh.all_auth_failed" => CoreErrorMsg::AllAuthFailed,
        "ssh.key_load_failed" => CoreErrorMsg::KeyLoadFailed,
        "ssh.agent_unavailable" => CoreErrorMsg::AgentUnavailable,
        "ssh.rsa_sha1_refused" => CoreErrorMsg::RsaSha1Refused,
        "ssh.known_hosts_failed" => CoreErrorMsg::KnownHostsFailed,
        "ssh.host_key_rejected" => CoreErrorMsg::HostKeyRejected,
        "ssh.host_key_changed" => CoreErrorMsg::HostKeyChanged,
        "ssh.host_key_revoked" => CoreErrorMsg::HostKeyRevoked,
        "ssh.host_key_rekey_mismatch" => CoreErrorMsg::HostKeyRekeyMismatch,
        "ssh.no_common_algorithm" => CoreErrorMsg::NoCommonAlgorithm,
        "sftp.replace_failed" => CoreErrorMsg::SftpReplaceFailed,
        "sftp.no_subsystem" => CoreErrorMsg::SftpNoSubsystem,
        "sftp.permission_denied" => CoreErrorMsg::SftpPermissionDenied,
        "sftp.not_found" => CoreErrorMsg::SftpNotFound,
        "sftp.already_exists" => CoreErrorMsg::SftpAlreadyExists,
        "sftp.remote_no_space" => CoreErrorMsg::SftpRemoteNoSpace,
        "sftp.local_no_space" => CoreErrorMsg::SftpLocalNoSpace,
        "sftp.disconnected" => CoreErrorMsg::SftpDisconnected,
        "sftp.non_utf8_name" => CoreErrorMsg::SftpNonUtf8Name,
        "sftp.too_large" => CoreErrorMsg::SftpTooLarge,
        "sftp.remote_status" => CoreErrorMsg::SftpRemoteStatus,
        "sftp.remote_error" => CoreErrorMsg::SftpRemoteError,
        "sftp.local_error" => CoreErrorMsg::SftpLocalError,
        "sftp.target_busy" => CoreErrorMsg::SftpTargetBusy,
        "sftp.rename_target_exists" => CoreErrorMsg::SftpRenameTargetExists,
        "sftp.no_free_name" => CoreErrorMsg::SftpNoFreeName,
        "sftp.cancelled" => CoreErrorMsg::SftpCancelled,
        _ => return None,
    })
}

/// 核心错误 → 当前界面语言的一句话。
///
/// 错误码认得就把模板填好后返回；认不得、或者模板缺少对应参数，都退回核心库的
/// `Display` 原文——上屏中文也比露出一行 `{path}` 或丢掉细节强。
pub(crate) fn error_text<E: ErrorWithCode>(err: &E) -> String {
    error_text_in(current(), err)
}

/// 指定语言生成文案（测试用；界面走 [`error_text`] 读当前语言）。
pub(crate) fn error_text_in<E: ErrorWithCode + ?Sized>(lang: Lang, err: &E) -> String {
    let Some(msg) = message_for(err.code()) else {
        return err.to_string();
    };
    let template = match lang {
        Lang::Zh => msg.zh(),
        Lang::En => msg.en(),
    };
    let mut params = err.params();
    localize_params(lang, err, &mut params);
    fill(template, &params).unwrap_or_else(|| err.to_string())
}

/// 一条 [`Reason`] 当前语言的整句（不带"输入不合法："这类前缀）。表单旁边只显示原因时用。
pub(crate) fn reason_text(reason: &zssh_core::error::Reason) -> String {
    reason_text_in(current(), reason)
}

pub(crate) fn reason_text_in(lang: Lang, reason: &zssh_core::error::Reason) -> String {
    if lang == Lang::En {
        let mut params = vec![("reason", reason.key().to_owned())];
        params.extend(reason.params().iter().cloned());
        if let Some(sentence) = crate::reason_text::sentence(reason.key(), &params) {
            return sentence;
        }
    }
    reason.zh().to_owned()
}

/// 把参数里"要按语言出词"的几项换成当前语言：`Reason` 的整句、`NotFound` 的对象名、`Hop` 的
/// 步骤和内层错误。用户数据（路径、主机名……）和第三方库 / 系统错误原文不动。
fn localize_params<E: ErrorWithCode + ?Sized>(
    lang: Lang,
    err: &E,
    params: &mut Vec<(&'static str, String)>,
) {
    fn get<'a>(params: &'a [(&'static str, String)], name: &str) -> Option<&'a str> {
        params
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }
    fn set(params: &mut [(&'static str, String)], name: &str, value: String) {
        if let Some(slot) = params.iter_mut().find(|(key, _)| *key == name) {
            slot.1 = value;
        }
    }

    if lang == Lang::En {
        if let Some(key) = get(params, "reason") {
            if let Some(sentence) = crate::reason_text::sentence(key, params) {
                set(params, "detail", sentence);
            }
        }
    }
    if err.code() == "store.not_found" {
        if let Some(noun) = get(params, "entity").map(|key| entity_noun(lang, key)) {
            set(params, "entity", noun);
        }
    }
    if let Some(cause) = err.cause() {
        let inner = error_text_in(lang, cause);
        let stage_msg = match get(params, "stage") {
            Some("handshake") => ExtraMsg::StageHandshake,
            Some("tunnel") => ExtraMsg::StageTunnel,
            _ => ExtraMsg::StageConnect,
        };
        let stage = fill(
            pick(lang, &stage_msg),
            &[("via", get(params, "via").unwrap_or_default().to_owned())],
        )
        .unwrap_or_default();
        let detail = fill(
            pick(lang, &ExtraMsg::HopDetail),
            &[("stage", stage), ("inner", inner)],
        )
        .unwrap_or_default();
        set(params, "detail", detail);
    }
}

/// 在一张 `Msg` 表里按语言取词。
fn pick(lang: Lang, msg: &impl Msg) -> &'static str {
    match lang {
        Lang::Zh => msg.zh(),
        Lang::En => msg.en(),
    }
}

/// `NotFound` 的对象名（核心给的是稳定英文标识）。认不得的标识原样显示。
fn entity_noun(lang: Lang, key: &str) -> String {
    let msg = match key {
        "group" => ExtraMsg::EntityGroup,
        "host" => ExtraMsg::EntityHost,
        "identity" => ExtraMsg::EntityIdentity,
        "proxy" => ExtraMsg::EntityProxy,
        "port_forward" => ExtraMsg::EntityPortForward,
        "ai_endpoint" => ExtraMsg::EntityAiEndpoint,
        other => return other.to_owned(),
    };
    pick(lang, &msg).to_owned()
}

/// 不属于某一个错误码的几段词：`NotFound` 的对象名、拨号链某一跳出错的步骤、整句的拼法。
#[derive(Clone, Copy)]
enum ExtraMsg {
    EntityGroup,
    EntityHost,
    EntityIdentity,
    EntityProxy,
    EntityPortForward,
    EntityAiEndpoint,
    StageConnect,
    StageHandshake,
    StageTunnel,
    HopDetail,
}

impl Msg for ExtraMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::EntityGroup => "分组",
            Self::EntityHost => "主机",
            Self::EntityIdentity => "凭据",
            Self::EntityProxy => "代理",
            Self::EntityPortForward => "端口转发",
            Self::EntityAiEndpoint => "ZeePilot 接口",
            Self::StageConnect => "连接",
            Self::StageHandshake => "SSH 握手或认证",
            Self::StageTunnel => "经跳板机 {via} 打开转发通道",
            Self::HopDetail => "{stage}失败：{inner}",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::EntityGroup => "group",
            Self::EntityHost => "host",
            Self::EntityIdentity => "credential",
            Self::EntityProxy => "proxy",
            Self::EntityPortForward => "port forward",
            Self::EntityAiEndpoint => "ZeePilot endpoint",
            Self::StageConnect => "Connection",
            Self::StageHandshake => "SSH handshake or authentication",
            Self::StageTunnel => "Opening a forwarding channel through jump host {via}",
            Self::HopDetail => "{stage} failed: {inner}",
        }
    }
}

/// 把 `{key}` 占位符换成参数值；有占位符对不上参数时返回 `None`。
///
/// 参数值里允许出现 `{`（路径、远端消息都可能带），所以不能靠"替换后再看有没有花括号"
/// 判断，而是按占位符逐个查表。
pub(crate) fn fill(template: &str, params: &[(&'static str, String)]) -> Option<String> {
    let mut out = String::with_capacity(template.len() + params.len() * 16);
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let end = rest[start..].find('}')?;
        let key = &rest[start + 1..start + end];
        let value = params.iter().find(|(name, _)| *name == key)?;
        out.push_str(&value.1);
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use zssh_core::error::{ErrorWithCode, ZsshError};
    use zssh_core::sftp::error::SftpError;

    use super::{CoreErrorMsg, error_text, error_text_in, fill, message_for};
    use crate::i18n::{Lang, Msg};

    /// 每一条模板都要有两个语言、且不能是空串。
    #[test]
    fn every_template_has_both_languages() {
        let all = [
            CoreErrorMsg::QuickTargetMissingUser,
            CoreErrorMsg::QuickTargetEmptyHost,
            CoreErrorMsg::QuickTargetBadPort,
            CoreErrorMsg::QuickTargetFormat,
            CoreErrorMsg::DnsFailed,
            CoreErrorMsg::ConnectTimeout,
            CoreErrorMsg::ProxyFailed,
            CoreErrorMsg::HopFailed,
            CoreErrorMsg::ConnectFailed,
            CoreErrorMsg::SshProtocolError,
            CoreErrorMsg::MissingServerKey,
            CoreErrorMsg::AuthFailed,
            CoreErrorMsg::ShellFailed,
            CoreErrorMsg::X11Failed,
            CoreErrorMsg::SftpUnavailableSys,
            CoreErrorMsg::ChannelLimit,
            CoreErrorMsg::DatabaseError,
            CoreErrorMsg::NotFoundEntity,
            CoreErrorMsg::InvalidInput,
            CoreErrorMsg::Internal,
            CoreErrorMsg::IdentityInUse,
            CoreErrorMsg::ProxyInUse,
            CoreErrorMsg::SecretError,
            CoreErrorMsg::CoreCancelled,
            CoreErrorMsg::AuthTimeout,
            CoreErrorMsg::AllAuthFailed,
            CoreErrorMsg::KeyLoadFailed,
            CoreErrorMsg::AgentUnavailable,
            CoreErrorMsg::RsaSha1Refused,
            CoreErrorMsg::KnownHostsFailed,
            CoreErrorMsg::HostKeyRejected,
            CoreErrorMsg::HostKeyChanged,
            CoreErrorMsg::HostKeyRevoked,
            CoreErrorMsg::HostKeyRekeyMismatch,
            CoreErrorMsg::NoCommonAlgorithm,
            CoreErrorMsg::SftpReplaceFailed,
            CoreErrorMsg::SftpNoSubsystem,
            CoreErrorMsg::SftpPermissionDenied,
            CoreErrorMsg::SftpNotFound,
            CoreErrorMsg::SftpAlreadyExists,
            CoreErrorMsg::SftpRemoteNoSpace,
            CoreErrorMsg::SftpLocalNoSpace,
            CoreErrorMsg::SftpDisconnected,
            CoreErrorMsg::SftpNonUtf8Name,
            CoreErrorMsg::SftpTooLarge,
            CoreErrorMsg::SftpRemoteStatus,
            CoreErrorMsg::SftpRemoteError,
            CoreErrorMsg::SftpLocalError,
            CoreErrorMsg::SftpTargetBusy,
            CoreErrorMsg::SftpRenameTargetExists,
            CoreErrorMsg::SftpNoFreeName,
            CoreErrorMsg::SftpCancelled,
        ];
        for msg in all {
            assert!(!msg.zh().is_empty(), "{msg:?} 缺中文");
            assert!(!msg.en().is_empty(), "{msg:?} 缺英文");
            // 英文模板里不能再漏中文（参数值可能是中文，那是数据；模板本身必须是英文）。
            assert!(
                !msg.en().chars().any(|ch| {
                    let code = ch as u32;
                    (0x4E00..=0x9FFF).contains(&code)
                }),
                "{msg:?} 的英文模板里有中文"
            );
        }
    }

    /// 中文模板必须和核心库 `Display` 一字不差：语言是中文时，用户看到的和以前完全一样。
    #[test]
    fn chinese_text_matches_the_core_display() {
        let denied = SftpError::PermissionDenied {
            path: "/etc/shadow".into(),
        };
        assert_eq!(error_text_in(Lang::Zh, &denied), denied.to_string());

        let too_large = SftpError::TooLarge {
            path: "/var/log/big".into(),
            size: 4096,
            limit: 1024,
        };
        assert_eq!(error_text_in(Lang::Zh, &too_large), too_large.to_string());

        let no_subsystem = SftpError::NoSubsystem("eof".into());
        assert_eq!(
            error_text_in(Lang::Zh, &no_subsystem),
            no_subsystem.to_string()
        );

        let timeout = ZsshError::ConnectTimeout {
            host: "web-1".into(),
            port: 22,
            seconds: 10,
        };
        assert_eq!(error_text_in(Lang::Zh, &timeout), timeout.to_string());

        let auth = ZsshError::AllAuthFailed {
            tried: "password, publickey".into(),
            remaining: "keyboard-interactive".into(),
        };
        assert_eq!(error_text_in(Lang::Zh, &auth), auth.to_string());

        let rsa = ZsshError::RsaSha1Refused {
            key: "id_rsa".into(),
        };
        assert_eq!(error_text_in(Lang::Zh, &rsa), rsa.to_string());

        let changed = ZsshError::HostKeyChanged(Box::new(zssh_core::error::HostKeyMismatch {
            host: "web-1".into(),
            port: 22,
            key_type: "ssh-ed25519".into(),
            stored: "SHA256:old".into(),
            presented: "SHA256:new".into(),
            stored_in: "own".into(),
            advice: "clear".into(),
        }));
        assert_eq!(error_text_in(Lang::Zh, &changed), changed.to_string());

        let no_common =
            ZsshError::NoCommonAlgorithm(Box::new(zssh_core::error::NoCommonAlgorithmInfo {
                host: "web-1".into(),
                port: 22,
                kind: "kex".into(),
                ours: "curve25519".into(),
                theirs: "diffie-hellman-group1".into(),
            }));
        assert_eq!(error_text_in(Lang::Zh, &no_common), no_common.to_string());
    }

    /// 英文界面要出英文模板，参数填进去；核心库的中文原文不再出现在文案里。
    #[test]
    fn english_text_uses_the_english_template_with_params() {
        let denied = SftpError::PermissionDenied {
            path: "/etc/shadow".into(),
        };
        let text = error_text_in(Lang::En, &denied);
        assert_eq!(text, "Permission denied: /etc/shadow");
        assert!(!text.contains("没有权限"), "{text}");

        let too_large = SftpError::TooLarge {
            path: "/var/log/big".into(),
            size: 4096,
            limit: 1024,
        };
        assert_eq!(
            error_text_in(Lang::En, &too_large),
            "/var/log/big is 4096 bytes, over the 1024-byte limit"
        );

        let timeout = ZsshError::ConnectTimeout {
            host: "web-1".into(),
            port: 2222,
            seconds: 10,
        };
        assert_eq!(
            error_text_in(Lang::En, &timeout),
            "Timed out connecting to web-1:2222 (10s)"
        );

        let changed = ZsshError::HostKeyChanged(Box::new(zssh_core::error::HostKeyMismatch {
            host: "web-1".into(),
            port: 22,
            key_type: "ssh-ed25519".into(),
            stored: "SHA256:old".into(),
            presented: "SHA256:new".into(),
            stored_in: "own".into(),
            advice: "clear".into(),
        }));
        let text = error_text_in(Lang::En, &changed);
        assert!(
            text.starts_with("The ssh-ed25519 host key fingerprint for web-1:22"),
            "{text}"
        );
        assert!(
            text.contains("SHA256:old") && text.contains("SHA256:new"),
            "{text}"
        );
    }

    /// 默认入口按进程当前语言（测试进程没切过语言 = 中文）。
    #[test]
    fn default_entry_uses_the_current_language() {
        assert_eq!(crate::i18n::current(), Lang::Zh);
        let denied = SftpError::PermissionDenied {
            path: "/etc".into(),
        };
        assert_eq!(error_text(&denied), denied.to_string());
    }

    /// 认不得的错误码退回核心库原文（不能崩、不能丢信息）。
    #[test]
    fn unknown_codes_fall_back_to_the_core_display() {
        struct Unknown;

        impl fmt::Display for Unknown {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "某个新错误的原文")
            }
        }

        impl ErrorWithCode for Unknown {
            fn code(&self) -> &'static str {
                "demo.unknown"
            }
        }

        let err = Unknown;
        assert_eq!(error_text_in(Lang::Zh, &err), "某个新错误的原文");
        assert_eq!(error_text_in(Lang::En, &err), "某个新错误的原文");
    }

    /// 码认得但参数对不上（新库换了参数名）：退回原文，不能露出 `{path}`。
    #[test]
    fn missing_params_fall_back_to_the_core_display() {
        struct Broken;

        impl fmt::Display for Broken {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "没有权限：/etc")
            }
        }

        impl ErrorWithCode for Broken {
            fn code(&self) -> &'static str {
                "sftp.permission_denied"
            }
        }

        assert_eq!(error_text_in(Lang::En, &Broken), "没有权限：/etc");
    }

    #[test]
    fn placeholder_filling_handles_braces_in_values() {
        assert_eq!(
            fill("{path}", &[("path", "C:\\a{b}.txt".to_owned())]),
            Some("C:\\a{b}.txt".to_owned())
        );
        assert_eq!(fill("no params", &[]), Some("no params".to_owned()));
        assert_eq!(fill("{path}", &[("other", "x".to_owned())]), None);
    }

    /// 核心库发出来的码都要有模板：这里把已知的 55 个码列出来（契约清单）。
    #[test]
    fn every_known_code_has_a_template() {
        let codes = [
            "ssh.quick_target_missing_user",
            "ssh.quick_target_empty_host",
            "ssh.quick_target_bad_port",
            "ssh.quick_target_format",
            "ssh.dns_failed",
            "ssh.connect_timeout",
            "ssh.proxy_failed",
            "ssh.hop_failed",
            "ssh.connect_failed",
            "ssh.protocol_error",
            "ssh.missing_host_key",
            "ssh.auth_failed",
            "ssh.shell_failed",
            "ssh.x11_forward_failed",
            "ssh.sftp_unavailable",
            "ssh.channel_limit",
            "store.database_error",
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
            "sftp.replace_failed",
            "sftp.no_subsystem",
            "sftp.channel_limit",
            "sftp.permission_denied",
            "sftp.not_found",
            "sftp.already_exists",
            "sftp.remote_no_space",
            "sftp.local_no_space",
            "sftp.disconnected",
            "sftp.non_utf8_name",
            "sftp.too_large",
            "sftp.remote_status",
            "sftp.remote_error",
            "sftp.local_error",
            "sftp.target_busy",
            "sftp.rename_target_exists",
            "sftp.no_free_name",
            "sftp.cancelled",
            "sftp.invalid_input",
            "sftp.internal",
        ];
        assert_eq!(codes.len(), 55, "码清单要跟着契约更新");
        for code in codes {
            assert!(message_for(code).is_some(), "{code} 缺模板");
        }
    }
    fn has_cjk(text: &str) -> bool {
        text.chars()
            .any(|ch| matches!(ch as u32, 0x4E00..=0x9FFF | 0x3000..=0x303F | 0xFF00..=0xFFEF))
    }

    /// 把参数值（第三方 / 系统错误原文，可能是系统语言的）拿掉后，整句不能再有汉字。
    fn english_apart_from(text: &str, data: &[&str]) -> bool {
        let mut rest = text.to_owned();
        for piece in data {
            rest = rest.replace(piece, "");
        }
        !has_cjk(&rest)
    }

    fn param(err: &ZsshError, name: &str) -> String {
        err.params()
            .into_iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value)
            .unwrap_or_default()
    }

    /// 每个会上屏的原因 key 都要有英文整句（`TEXT` 是故意不上屏的内部诊断，不要求）。
    #[test]
    fn every_reason_key_has_an_english_sentence() {
        for key in zssh_core::error::reason_keys::ALL {
            if *key == zssh_core::error::reason_keys::TEXT {
                continue;
            }
            let text = crate::reason_text::english(key).unwrap_or_else(|| panic!("{key} 缺英文"));
            assert!(!text.is_empty() && !has_cjk(text), "{key}: {text}");
        }
    }

    fn database_with_key_file(path: &str) -> (zssh_core::store::Database, i64) {
        let db = zssh_core::store::Database::open_in_memory().expect("内存库");
        let identity = db
            .identity_save(&zssh_core::model::IdentityDraft {
                id: None,
                name: "deploy".to_owned(),
                kind: zssh_core::model::IdentityKind::KeyFile,
                key_path: Some(path.to_owned()),
                secret: None,
                passphrase: None,
            })
            .expect("存凭据");
        (db, identity.id)
    }

    /// 公钥安装对话框：不存在的私钥文件，英文界面是完整英文句子，路径参数原样在。
    #[test]
    fn a_missing_private_key_file_reads_as_english_with_its_path() {
        let path = r"C:\keys\no-such-key";
        let (db, id) = database_with_key_file(path);
        let err = db.identity_public_key(id).unwrap_err();
        let text = error_text_in(Lang::En, &err);
        assert!(
            text.starts_with(&format!(
                "Invalid input: Cannot read the private key file {path}: "
            )),
            "{text}"
        );
        // 后面的原因是系统给的错误文本（跟系统语言），不翻译。
        assert!(
            english_apart_from(&text, &[&param(&err, "cause")]),
            "{text}"
        );
        // 中文界面和核心库的中文原文逐字相同。
        assert_eq!(error_text_in(Lang::Zh, &err), err.to_string());
    }

    /// 不支持的私钥内容、错误口令：原因整句英文。
    #[test]
    fn unreadable_private_key_content_and_wrong_passphrase_read_as_english() {
        let junk = zssh_core::ssh::keygen::public_key_of("not a private key", None).unwrap_err();
        let generated = zssh_core::ssh::keygen::generate(
            zssh_core::ssh::keygen::KeyAlgorithm::Ed25519,
            "c",
            Some("right"),
        )
        .expect("生成");
        let wrong =
            zssh_core::ssh::keygen::public_key_of(&generated.private_openssh, Some("wrong"))
                .unwrap_err();
        for err in [junk, wrong] {
            let text = error_text_in(Lang::En, &err);
            assert!(
                text.starts_with(
                    "Invalid input: Cannot read this private key (wrong passphrase, or not a supported private key format): "
                ),
                "{text}"
            );
            assert!(
                english_apart_from(&text, &[&param(&err, "cause")]),
                "{text}"
            );
            assert_eq!(error_text_in(Lang::Zh, &err), err.to_string());
        }
    }

    /// 主机表单保存的校验：名称空、端口 0、地址带空格。
    #[test]
    fn host_form_validation_reads_as_english() {
        use zssh_core::model::{AuthMethod, HostDraft, ProxyMode};
        let db = zssh_core::store::Database::open_in_memory().expect("内存库");
        let draft = |name: &str, host: &str, port: u16| HostDraft {
            protocol: None,
            id: None,
            group_id: None,
            name: name.to_owned(),
            host: host.to_owned(),
            port,
            username: "ops".to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: None,
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: ProxyMode::None,
            tags: Vec::new(),
            sort: 0,
            legacy_algorithms: false,
        };
        let cases = [
            (
                draft("  ", "192.0.2.7", 22),
                "Invalid input: The host's name cannot be empty",
            ),
            (
                draft("a", "192.0.2.7", 0),
                "Invalid input: The port must be between 1 and 65535",
            ),
            (
                draft("a", "bad host", 22),
                "Invalid input: The address cannot contain whitespace or control characters: only a domain name, IPv4 or IPv6 address is allowed",
            ),
        ];
        for (request, want) in cases {
            let err = db.host_save(&request).unwrap_err();
            assert_eq!(error_text_in(Lang::En, &err), want);
            assert_eq!(error_text_in(Lang::Zh, &err), err.to_string());
        }
    }

    /// 带参数的原因：用户数据原样留着，其余是英文。
    #[test]
    fn reasons_with_params_keep_user_data_verbatim() {
        use zssh_core::error::{Reason, reason_keys as rk};
        let err = ZsshError::InvalidInput(
            Reason::new(rk::IDENTITY_NOT_KEY, "（中文原文）").with("name", "生产密钥"),
        );
        assert_eq!(
            error_text_in(Lang::En, &err),
            "Invalid input: Credential \"生产密钥\" is not a private key credential and cannot be used for key authentication"
        );
        // 没有 key 的文字（内部诊断）：两种语言都原样。
        let plain = ZsshError::Internal("内部断言".into());
        assert_eq!(error_text_in(Lang::En, &plain), "Internal error: 内部断言");
        // key 认得但参数对不上：退回中文原文，不露 `{name}`。
        let broken = ZsshError::InvalidInput(Reason::new(rk::IDENTITY_NOT_KEY, "（中文原文）"));
        assert_eq!(
            error_text_in(Lang::En, &broken),
            "Invalid input: （中文原文）"
        );
    }

    #[test]
    fn not_found_names_the_thing_in_each_language() {
        use zssh_core::error::Entity;
        for entity in [
            Entity::Group,
            Entity::Host,
            Entity::Identity,
            Entity::Proxy,
            Entity::PortForward,
            Entity::AiEndpoint,
        ] {
            let err = ZsshError::NotFound { entity, id: 3 };
            assert_eq!(error_text_in(Lang::Zh, &err), err.to_string(), "{entity:?}");
            let en = error_text_in(Lang::En, &err);
            assert!(
                en.starts_with("Cannot find ") && en.ends_with("(id=3)"),
                "{en}"
            );
            assert!(!has_cjk(&en), "{en}");
        }
    }

    /// 连接错误：跳点的内层错误按语言逐层生成，不再是预拼好的中文。
    #[test]
    fn hop_errors_are_composed_per_language() {
        use zssh_core::error::HopStage;
        let hop = |stage| ZsshError::Hop {
            index: 1,
            label: "Target web-1".into(),
            stage,
            source: Box::new(ZsshError::Connect {
                host: "127.0.0.1".into(),
                port: 1,
                tried: "127.0.0.1:1".into(),
                source: std::io::Error::other("refused"),
            }),
        };
        let connect = hop(HopStage::Connect);
        assert_eq!(error_text_in(Lang::Zh, &connect), connect.to_string());
        assert_eq!(
            error_text_in(Lang::En, &connect),
            "Hop 1 (Target web-1): Connection failed: Failed to connect to 127.0.0.1:1 (tried 127.0.0.1:1): refused"
        );
        let tunnel = hop(HopStage::Tunnel {
            via: "jump-1".into(),
        });
        assert_eq!(error_text_in(Lang::Zh, &tunnel), tunnel.to_string());
        assert!(error_text_in(Lang::En, &tunnel).starts_with(
            "Hop 1 (Target web-1): Opening a forwarding channel through jump host jump-1 failed: "
        ));
        let handshake = ZsshError::Hop {
            index: 2,
            label: "web-1".into(),
            stage: HopStage::Handshake,
            source: Box::new(ZsshError::AuthFailed),
        };
        assert_eq!(error_text_in(Lang::Zh, &handshake), handshake.to_string());
        assert_eq!(
            error_text_in(Lang::En, &handshake),
            "Hop 2 (web-1): SSH handshake or authentication failed: Authentication failed: wrong user name or password"
        );
    }

    /// 代理握手失败 + IPv6 提示：英文补一句出口提示。
    #[test]
    fn proxy_failures_with_the_ipv6_hint_read_as_english() {
        use zssh_core::error::{Reason, reason_keys as rk};
        let reason = Reason::new(rk::SOCKS_HOST_UNREACHABLE, "代理说目标主机不可达。（提示）")
            .with("ipv6_hint", "1");
        let err = ZsshError::Proxy {
            proxy: "socks5 127.0.0.1:1080".into(),
            detail: reason,
        };
        let text = error_text_in(Lang::En, &err);
        assert!(
            text.starts_with(
                "Proxy socks5 127.0.0.1:1080: The proxy says the target host is unreachable. The target is an IPv6 address"
            ),
            "{text}"
        );
        assert!(!has_cjk(&text), "{text}");
    }
}
