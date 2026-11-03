//! "用 Tokenway 连接"：浏览器授权（OAuth 授权码 + PKCE + 本机回环，RFC 8252 / 7636）。
//!
//! 流程：生成 `code_verifier` / `code_challenge`（S256）/ `state` → 在 `127.0.0.1` 随机端口起一次性监听
//! → 用系统浏览器打开预设里的 `connect_url`（带 `redirect_uri`、`state`、`code_challenge`、设备名）
//! → 等回调（核对 `state`）→ 用授权码 + `code_verifier` POST `token_url` 换这台设备的 API 令牌。
//!
//! 授权 / 换令牌的地址只能来自验过签的预设清单（调用方传进来），用户在界面上改不了；设备名不带
//! 用户名；令牌不进日志、不进错误文字。

use std::sync::Arc;
use std::time::Duration;

use reqwest::Url;
use tokio::sync::Notify;

use super::api::{exchange_token, ApiError, ApiErrorKind, Authorized, TokenRequest};
use super::loopback::{Callback, CallbackListener};
use super::presets::{AuthorizeUrls, UrlPolicy};
use crate::ai::agent::builtin::provider::Transport;
use crate::sync::onedrive::pkce;

/// 用户 5 分钟内没有完成授权就放弃等待。
pub const AUTHORIZE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectError {
    #[error("你在网页上拒绝了授权，没有连接 Tokenway")]
    Denied,
    #[error("5 分钟内没有完成授权，已经停止等待，可以重新连接")]
    Timeout,
    #[error("已取消连接")]
    Cancelled,
    #[error("无法在本机开启授权回调的监听：{0}")]
    Listen(String),
    #[error("无法打开系统浏览器：{0}")]
    Browser(String),
    /// `state` 不对、缺授权码等：回调被丢弃。
    #[error("授权失败，请重试")]
    Invalid,
    #[error("{0}")]
    Api(ApiError),
    #[error("内部错误：{0}")]
    Internal(String),
}

impl From<ApiError> for ConnectError {
    fn from(err: ApiError) -> Self {
        Self::Api(err)
    }
}

impl ConnectError {
    /// 是"用户自己取消 / 拒绝"这类，界面不用当错误显示（红字）。
    pub fn is_user_choice(&self) -> bool {
        matches!(self, Self::Denied | Self::Cancelled)
    }
}

/// 打开浏览器的方式。产品里是系统默认浏览器；测试里换成记录地址 / 自动完成授权的假浏览器。
pub trait UrlOpener: Send + Sync {
    fn open(&self, url: &str) -> Result<(), String>;
}

/// 进度（界面显示"正在等待浏览器授权…"之类）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// 监听开好、浏览器已经打开，等用户在网页上点"授权"。
    WaitingForBrowser,
    /// 收到授权码，正在换令牌。
    Exchanging,
}

pub struct AuthorizeParams<'a> {
    pub urls: AuthorizeUrls<'a>,
    /// 授权页上显示的设备名（不带用户名）。
    pub device: &'a str,
    /// 邀请码：只能来自预设清单里的注册地址，ZeeTerm 自己不写死。
    pub aff: Option<&'a str>,
    pub policy: &'a UrlPolicy,
    pub timeout: Duration,
}

/// 拼授权页地址。
pub fn build_connect_url(
    connect_url: &str,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
    device: &str,
    aff: Option<&str>,
) -> Result<String, ConnectError> {
    let mut url = Url::parse(connect_url)
        .map_err(|_| ConnectError::Internal("授权页地址不合法".to_owned()))?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("state", state)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256");
        if !device.is_empty() {
            pairs.append_pair("device", device);
        }
        if let Some(aff) = aff {
            pairs.append_pair("aff", aff);
        }
    }
    Ok(url.into())
}

fn internal(err: impl std::fmt::Debug) -> ConnectError {
    ConnectError::Internal(format!("{err:?}"))
}

/// 跑完整个授权。`cancel` 被通知时立刻放弃（监听关闭）。
pub async fn authorize(
    params: AuthorizeParams<'_>,
    opener: &dyn UrlOpener,
    transport: &dyn Transport,
    cancel: &Notify,
    on_stage: &(dyn Fn(Stage) + Send + Sync),
) -> Result<Authorized, ConnectError> {
    // 地址先过白名单：连浏览器都不打开。
    for url in [params.urls.connect, params.urls.token] {
        params.policy.check(url).map_err(|why| {
            ConnectError::Api(ApiError {
                kind: ApiErrorKind::Refused,
                message: format!("预设里的地址不合格，没有连接（{why}）"),
                status: None,
                code: None,
            })
        })?;
    }
    let pkce = pkce::generate().map_err(internal)?;
    let state = pkce::random_state().map_err(internal)?;
    let listener = CallbackListener::bind().await?;
    let redirect_uri = listener.redirect_uri();
    let device = clean_device(params.device);
    let url = build_connect_url(
        params.urls.connect,
        &redirect_uri,
        &state,
        &pkce.challenge,
        &device,
        params.aff,
    )?;
    // 先开好监听、再开浏览器：浏览器跳回来的时候端口一定已经在等了。
    opener.open(&url).map_err(ConnectError::Browser)?;
    on_stage(Stage::WaitingForBrowser);

    let callback = listener.wait(&state, params.timeout, cancel).await?;
    let code = match callback {
        Callback::Code(code) => code,
        Callback::Denied => return Err(ConnectError::Denied),
        Callback::Invalid => return Err(ConnectError::Invalid),
    };
    on_stage(Stage::Exchanging);
    let request = TokenRequest {
        code: &code,
        code_verifier: &pkce.verifier,
        redirect_uri: &redirect_uri,
        device: &device,
    };
    let exchange = exchange_token(transport, params.policy, params.urls.token, &request);
    tokio::select! {
        result = exchange => Ok(result?),
        _ = cancel.notified() => Err(ConnectError::Cancelled),
    }
}

/// 设备名最长 40 个字符（Tokenway 侧的上限），去掉控制字符和会破坏页面的符号。
pub fn clean_device(device: &str) -> String {
    let cleaned: String = device
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '<' | '>' | '"' | '\'' | '`' | '\\'))
        .collect();
    cleaned
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(40)
        .collect()
}

/// 默认的设备名：计算机名的友好化版本，**不含用户名**（Windows 的计算机名里常带人名，所以界面
/// 上让用户可以改）。`computer_name` 通常是 `COMPUTERNAME` 环境变量。
pub fn friendly_device_name(computer_name: Option<&str>) -> String {
    let raw = computer_name.unwrap_or_default().trim();
    // Windows 默认生成的 `DESKTOP-7K3FQ2L` / `LAPTOP-…`：后面那串是随机码，没有意义。
    let upper = raw.to_ascii_uppercase();
    for (prefix, label) in [
        ("DESKTOP-", "Windows 台式机"),
        ("LAPTOP-", "Windows 笔记本"),
    ] {
        if upper.starts_with(prefix) {
            return label.to_owned();
        }
    }
    let spaced: String = raw
        .chars()
        .map(|c| if matches!(c, '-' | '_') { ' ' } else { c })
        .collect();
    let name = clean_device(&spaced);
    if name.is_empty() {
        "Windows 电脑".to_owned()
    } else {
        name
    }
}

/// 从预设里的注册地址取邀请码（`?aff=…`）；只认简单的字母数字串。
pub fn aff_from_register_url(register_url: &str) -> Option<String> {
    let url = Url::parse(register_url).ok()?;
    url.query_pairs()
        .find(|(key, _)| key == "aff")
        .map(|(_, value)| value.into_owned())
        .filter(|aff| {
            !aff.is_empty()
                && aff.len() <= 64
                && aff
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        })
}

/// 方便界面持有的取消句柄。
pub type CancelHandle = Arc<Notify>;

#[cfg(test)]
mod tests;
