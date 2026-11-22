//! Tokenway 的三个 JSON 接口：授权码换令牌、余额状态、停用，以及网关聊天失败的分类。
//!
//! 接口约定见 Tokenway 仓库的 `ZEETERM-DEPLOYMENT.md` 第二节：错误体统一是
//! `{"error":{"code":"…","message":"中文说明"}}`。请求都走调用方给的 [`Transport`]（"软件网络"代理、
//! 不跟随重定向），地址一律先过 [`UrlPolicy`]；API Key 只出现在 `Authorization` 头和换令牌
//! 响应体里，不进日志、不进错误文字。

use std::time::Duration;

use serde_json::{json, Value};

use super::presets::UrlPolicy;
use crate::ai::agent::builtin::conversation::FailureInfo;
use crate::ai::agent::builtin::provider::{ApiKey, ErrorKind, HttpRequest, Transport};

/// 每个请求最多等多久。
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// 响应体上限（这三个接口的响应都很小）。
const BODY_LIMIT: usize = 256 * 1024;

/// 网关因为"ZeeTerm 令牌只能用充值余额"而拒绝时的错误码。
pub const REQUIRES_TOPUP_CODE: &str = "zeeterm_requires_topup";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiErrorKind {
    /// 地址不合格（不是 https / 域名不在白名单），根本没发请求。
    Refused,
    /// 连不上 / 超时 / 响应读到一半断了。
    Network,
    /// 429。
    RateLimited,
    /// 401：令牌已被停用、过期，或者根本不是 ZeeTerm 令牌。
    InvalidToken,
    /// 授权码无效、已用过、已过期、PKCE 对不上。
    InvalidGrant,
    /// 5xx / 503 暂时不可用。
    Unavailable,
    /// 其它 4xx。
    Rejected,
    /// 响应不是约定的格式。
    BadResponse,
}

/// 接口调用失败：给界面看的中文原因 + 结构化的类别。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub kind: ApiErrorKind,
    pub message: String,
    pub status: Option<u16>,
    /// 服务端的错误码原文。
    pub code: Option<String>,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    fn new(kind: ApiErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status: None,
            code: None,
        }
    }

    pub fn network(detail: &str) -> Self {
        Self::new(
            ApiErrorKind::Network,
            format!(
                "连不上 Tokenway（网络不通或代理不可用）：{}",
                clean(detail, 120)
            ),
        )
    }
}

/// 服务端文字进界面之前：去控制字符、截短。
fn clean(text: &str, max_chars: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// HTTP 非 2xx → 错误。`body` 是响应体（可能为空 / 不是 JSON）。
pub fn error_from_response(status: u16, body: &[u8]) -> ApiError {
    let parsed: Option<Value> = serde_json::from_slice(body).ok();
    let error = parsed.as_ref().and_then(|v| v.get("error"));
    let code = error
        .and_then(|e| e.get("code"))
        .and_then(Value::as_str)
        .map(|c| clean(c, 64))
        .filter(|c| !c.is_empty());
    let server_message = error
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .map(|m| clean(m, 120))
        .filter(|m| !m.is_empty());
    let (kind, message) = match (status, code.as_deref()) {
        (401, _) | (_, Some("invalid_token")) => (
            ApiErrorKind::InvalidToken,
            "Tokenway 授权已失效（可能在网站上改过密码或停用了授权），请重新连接".to_owned(),
        ),
        (429, _) | (_, Some("rate_limited")) => (
            ApiErrorKind::RateLimited,
            "请求太频繁，请稍后再试".to_owned(),
        ),
        (_, Some("invalid_grant")) => (
            ApiErrorKind::InvalidGrant,
            "授权码无效、已用过或已过期，请重新连接".to_owned(),
        ),
        (500..=599, _) | (_, Some("unavailable" | "server_error" | "status_unavailable")) => (
            ApiErrorKind::Unavailable,
            "Tokenway 暂时不可用，请稍后再试".to_owned(),
        ),
        _ => (
            ApiErrorKind::Rejected,
            match server_message {
                Some(text) => format!("Tokenway 拒绝了请求（HTTP {status}）：{text}"),
                None => format!("Tokenway 拒绝了请求（HTTP {status}）"),
            },
        ),
    };
    ApiError {
        kind,
        message,
        status: Some(status),
        code,
    }
}

async fn send(
    transport: &dyn Transport,
    policy: &UrlPolicy,
    get: bool,
    url: &str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> Result<(u16, Vec<u8>), ApiError> {
    policy.check(url).map_err(|why| {
        ApiError::new(
            ApiErrorKind::Refused,
            format!("没有发出请求：地址不合格（{why}）"),
        )
    })?;
    let request = HttpRequest {
        url: url.to_owned(),
        headers,
        body,
    };
    let run = async {
        let mut response = if get {
            transport.get(request).await
        } else {
            transport.post(request).await
        }
        .map_err(|err| ApiError::network(&err.to_string()))?;
        let mut collected = Vec::new();
        loop {
            match response.body.next_chunk().await {
                Ok(Some(chunk)) => {
                    collected.extend_from_slice(&chunk);
                    if collected.len() > BODY_LIMIT {
                        return Err(ApiError::new(
                            ApiErrorKind::BadResponse,
                            "Tokenway 的响应太大，已放弃",
                        ));
                    }
                }
                Ok(None) => break,
                Err(err) => return Err(ApiError::network(&err.to_string())),
            }
        }
        Ok((response.status, collected))
    };
    tokio::time::timeout(REQUEST_TIMEOUT, run)
        .await
        .map_err(|_| ApiError::network("请求超时"))?
}

fn json_headers() -> Vec<(String, String)> {
    vec![
        ("content-type".to_owned(), "application/json".to_owned()),
        ("accept".to_owned(), "application/json".to_owned()),
    ]
}

// ---------------------------------------------------------------------------
// 换令牌
// ---------------------------------------------------------------------------

/// 换令牌的结果。`Debug` 里看不到 Key。
#[derive(Debug, Clone)]
pub struct Authorized {
    pub api_key: ApiKey,
    pub token_id: Option<String>,
    /// 服务端给的接入地址（已经过白名单检查，不合格的当没给）。
    pub openai_base: Option<String>,
    pub anthropic_base: Option<String>,
    pub expires_at: Option<String>,
    pub quota_limit: Option<f64>,
}

pub struct TokenRequest<'a> {
    pub code: &'a str,
    pub code_verifier: &'a str,
    pub redirect_uri: &'a str,
    pub device: &'a str,
}

fn key_looks_sane(key: &str) -> bool {
    !key.is_empty() && key.len() <= 512 && !key.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// `POST token_url`：授权码 + `code_verifier` 换这台设备的 API 令牌。
pub async fn exchange_token(
    transport: &dyn Transport,
    policy: &UrlPolicy,
    token_url: &str,
    request: &TokenRequest<'_>,
) -> Result<Authorized, ApiError> {
    let body = json!({
        "grant_type": "authorization_code",
        "code": request.code,
        "code_verifier": request.code_verifier,
        "redirect_uri": request.redirect_uri,
        "device": request.device,
    });
    let (status, bytes) = send(
        transport,
        policy,
        false,
        token_url,
        json_headers(),
        body.to_string().into_bytes(),
    )
    .await?;
    if !(200..300).contains(&status) {
        return Err(error_from_response(status, &bytes));
    }
    let bad = || {
        ApiError::new(
            ApiErrorKind::BadResponse,
            "Tokenway 返回的内容不是约定的格式",
        )
    };
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| bad())?;
    let key = value
        .get("api_key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|key| key_looks_sane(key))
        .ok_or_else(bad)?;
    let base = |field: &str| {
        value
            .get(field)
            .and_then(Value::as_str)
            .filter(|url| policy.check(url).is_ok())
            .map(|url| url.trim_end_matches('/').to_owned())
    };
    let token_id = match value.get("token_id") {
        Some(Value::String(text)) => Some(clean(text, 64)),
        Some(Value::Number(number)) => Some(number.to_string()),
        _ => None,
    };
    Ok(Authorized {
        api_key: ApiKey::new(key),
        token_id,
        openai_base: base("base_url_openai"),
        anthropic_base: base("base_url_anthropic"),
        expires_at: value
            .get("expires_at")
            .and_then(Value::as_str)
            .map(|text| clean(text, 40)),
        quota_limit: value.get("quota_limit").and_then(Value::as_f64),
    })
}

// ---------------------------------------------------------------------------
// 余额
// ---------------------------------------------------------------------------

/// `GET status_url` 的结果（金额换算成"分"的整数，避免浮点显示误差）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Balance {
    /// ZeeTerm 现在还能花多少（已扣掉没用完的赠送、并考虑了这枚令牌自己的额度上限）。
    pub usable_cents: i64,
    /// 账户里充值的部分。
    pub paid_cents: i64,
    /// 账户总余额（含赠送）。
    pub total_cents: i64,
    pub quota_limit_cents: Option<i64>,
    pub used_cents: i64,
    pub expires_at: Option<String>,
    pub needs_topup: bool,
}

pub fn yuan_to_cents(yuan: f64) -> i64 {
    if !yuan.is_finite() {
        return 0;
    }
    (yuan * 100.0).round() as i64
}

/// `¥12.34`（负数当 0）。
pub fn format_yuan(cents: i64) -> String {
    let cents = cents.max(0);
    format!("¥{}.{:02}", cents / 100, cents % 100)
}

impl Balance {
    /// 面板上显示的短文字。
    pub fn label(&self) -> String {
        if self.needs_topup {
            "需充值".to_owned()
        } else {
            format!("余额 {}", format_yuan(self.usable_cents))
        }
    }

    /// 悬停说明。
    pub fn detail(&self) -> String {
        let mut text = format!(
            "ZeeTerm 可用 {}；账户余额 {}（其中充值 {}）",
            format_yuan(self.usable_cents),
            format_yuan(self.total_cents),
            format_yuan(self.paid_cents)
        );
        if let Some(limit) = self.quota_limit_cents {
            text.push_str(&format!(
                "；这台设备的授权上限 {}，已用 {}",
                format_yuan(limit),
                format_yuan(self.used_cents)
            ));
        }
        if self.needs_topup {
            text.push_str("。ZeePilot 只能用充值余额，赠送额度可在网页和其他客户端试用");
        }
        text
    }
}

pub fn parse_balance(bytes: &[u8]) -> Result<Balance, ApiError> {
    let bad = || {
        ApiError::new(
            ApiErrorKind::BadResponse,
            "Tokenway 返回的余额不是约定的格式",
        )
    };
    let value: Value = serde_json::from_slice(bytes).map_err(|_| bad())?;
    let number = |field: &str| value.get(field).and_then(Value::as_f64);
    let usable = number("zeeterm_usable").ok_or_else(bad)?;
    let needs_topup = value
        .get("needs_topup")
        .and_then(Value::as_bool)
        .unwrap_or(usable <= 0.0);
    Ok(Balance {
        usable_cents: yuan_to_cents(usable).max(0),
        paid_cents: yuan_to_cents(number("balance_paid").unwrap_or(usable)).max(0),
        total_cents: yuan_to_cents(number("balance_total").unwrap_or(usable)).max(0),
        quota_limit_cents: number("quota_limit").map(yuan_to_cents),
        used_cents: yuan_to_cents(number("used").unwrap_or(0.0)).max(0),
        expires_at: value
            .get("expires_at")
            .and_then(Value::as_str)
            .map(|text| clean(text, 40)),
        needs_topup,
    })
}

/// `GET status_url`（`Authorization: Bearer <令牌>`）。
pub async fn fetch_status(
    transport: &dyn Transport,
    policy: &UrlPolicy,
    status_url: &str,
    key: &ApiKey,
) -> Result<Balance, ApiError> {
    let mut headers = json_headers();
    headers.push((
        "authorization".to_owned(),
        format!("Bearer {}", key.expose()),
    ));
    let (status, bytes) = send(transport, policy, true, status_url, headers, Vec::new()).await?;
    if !(200..300).contains(&status) {
        return Err(error_from_response(status, &bytes));
    }
    parse_balance(&bytes)
}

// ---------------------------------------------------------------------------
// 停用
// ---------------------------------------------------------------------------

/// `POST revoke_url`：用令牌本身证明持有，停用这枚 ZeeTerm 令牌。
pub async fn revoke(
    transport: &dyn Transport,
    policy: &UrlPolicy,
    revoke_url: &str,
    key: &ApiKey,
) -> Result<(), ApiError> {
    let body = json!({ "api_key": key.expose() });
    let (status, bytes) = send(
        transport,
        policy,
        false,
        revoke_url,
        json_headers(),
        body.to_string().into_bytes(),
    )
    .await?;
    if (200..300).contains(&status) {
        Ok(())
    } else {
        Err(error_from_response(status, &bytes))
    }
}

// ---------------------------------------------------------------------------
// 对话失败的分类
// ---------------------------------------------------------------------------

/// 网关对话失败后，界面要给什么提示 / 按钮。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatFailureAction {
    /// `zeeterm_requires_topup`：ZeeTerm 令牌没有可用的充值余额 → "去充值"。
    RequiresTopup,
    /// 401：令牌被停用 / 过期 / 无效 → "重新连接"。不自动删本地配置。
    AuthorizationInvalid,
    /// 429：限流。
    RateLimited,
}

/// 认 Tokenway 的几种失败。别的失败（网络、5xx、模型不存在……）返回 `None`，按通用错误显示。
pub fn classify_chat_failure(failure: &FailureInfo) -> Option<ChatFailureAction> {
    if failure.code.as_deref() == Some(REQUIRES_TOPUP_CODE) {
        return Some(ChatFailureAction::RequiresTopup);
    }
    match failure.status {
        Some(401) => Some(ChatFailureAction::AuthorizationInvalid),
        Some(429) => Some(ChatFailureAction::RateLimited),
        _ => (failure.kind == ErrorKind::RateLimited).then_some(ChatFailureAction::RateLimited),
    }
}

/// 余额显示的刷新节流：最多每 `min_interval` 一次（成败都算一次，网络不通时别每个节拍都去连）。
#[derive(Debug, Clone)]
pub struct RefreshGate {
    min_interval: Duration,
    last: Option<std::time::Instant>,
}

impl RefreshGate {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            last: None,
        }
    }

    /// 现在可以刷新吗？可以就记下这一次。
    pub fn try_start(&mut self, now: std::time::Instant) -> bool {
        if let Some(last) = self.last {
            if now.saturating_duration_since(last) < self.min_interval {
                return false;
            }
        }
        self.last = Some(now);
        true
    }

    /// 忘掉上次的时间（换了令牌 / 重新连接后立刻允许刷新）。
    pub fn reset(&mut self) {
        self.last = None;
    }
}

/// 余额刷新的最小间隔。
pub const BALANCE_MIN_INTERVAL: Duration = Duration::from_secs(60);

#[cfg(test)]
mod tests;
