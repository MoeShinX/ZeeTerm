//! 错误分类：给出面向用户的中文原因，以及「是否可重试」。
//!
//! 来源有三处：HTTP 非 2xx 的响应、传输层失败（连不上 / 超时 / 流中断）、流里出现的
//! 错误事件（HTTP 已经是 200 之后才报的）。分类依据：
//! - Anthropic：<https://platform.claude.com/docs/en/api/errors>
//! - OpenAI：错误体是 `{"error":{"message","type","code"}}`，429 里 `insufficient_quota`
//!   是额度用完（重试没用），其余 429 才是限流；
//! - DeepSeek：<https://api-docs.deepseek.com/quick_start/error_codes>（402 余额不足）。
//!
//! 原则：认证错误、地址错误、额度、参数错误一律不重试；限流、过载、5xx、连接问题、
//! 流中断可以重试。服务端原文放在 `detail` 里（截短、抹掉 Key），不混进 `message`。

use std::fmt;
use std::time::Duration;

use serde_json::Value;

/// `detail` 最多保留这么多字符。
const DETAIL_MAX_CHARS: usize = 300;
/// `retry-after` 最多认这么久，再大的值当成对方在发疯。
const RETRY_AFTER_MAX: Duration = Duration::from_secs(3600);

/// 错误的大类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// 401 / 403：Key 不对或没权限。
    Auth,
    /// 404：地址或模型名不对。
    NotFound,
    /// 429：限流。
    RateLimited,
    /// 额度用完 / 欠费（402，或带额度字样的 429 / 403）。
    Quota,
    /// 输入超出上下文窗口（A3 据此触发压缩）。
    ContextTooLong,
    /// 其它 4xx：请求本身有问题。
    BadRequest,
    /// 服务商过载（529 / 503 / 流里的 overloaded_error）。
    Overloaded,
    /// 5xx。
    Server,
    /// 连不上。
    Connect,
    /// 超时。
    Timeout,
    /// 流中途断了 / 回复不完整。
    Interrupted,
    /// 响应不是 SSE（比如网关返回了 HTML 错误页）。
    NotSse,
    /// 3xx 跳转。
    Redirect,
    /// 认不出来。
    Unknown,
}

/// provider 层的错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError {
    pub kind: ErrorKind,
    /// 面向用户的中文原因。
    pub message: String,
    /// 服务端给的原文（已截短、抹掉 Key）；没有就是 None。
    pub detail: Option<String>,
    /// 稍后重试有没有意义。
    pub retryable: bool,
    /// 对方要求的等待时间（`retry-after`）。
    pub retry_after: Option<Duration>,
    /// HTTP 状态码（有的话）。
    pub status: Option<u16>,
    /// 服务端给的错误码 / 错误类型原文（`error.code`，没有就取 `error.type`），例如 Tokenway 的
    /// `zeeterm_requires_topup`。给界面按码识别用，别拿 `message` 做字符串匹配。
    pub code: Option<String>,
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)?;
        if let Some(detail) = &self.detail {
            write!(f, "（服务端返回：{detail}）")?;
        }
        Ok(())
    }
}

impl std::error::Error for ProviderError {}

impl ProviderError {
    fn new(kind: ErrorKind, message: String, retryable: bool) -> Self {
        Self {
            kind,
            message,
            detail: None,
            retryable,
            retry_after: None,
            status: None,
            code: None,
        }
    }

    /// 把 Key 从 `message` / `detail` 里抹掉（服务端有时会在报错里回显 Key 的一部分或全部）。
    pub fn scrub(&mut self, secret: &str) {
        if secret.is_empty() {
            return;
        }
        self.message = self.message.replace(secret, "***");
        if let Some(detail) = &mut self.detail {
            *detail = detail.replace(secret, "***");
        }
    }

    /// 流中途断开 / 没收完整就结束。
    pub fn interrupted(why: &str) -> Self {
        let mut err = Self::new(
            ErrorKind::Interrupted,
            "连接中途断了，回复不完整。".to_string(),
            true,
        );
        err.detail = short(why);
        err
    }

    /// 传输层失败。
    pub fn from_transport(error: &TransportError) -> Self {
        match error {
            TransportError::Connect(why) => {
                let mut err = Self::new(
                    ErrorKind::Connect,
                    "连不上服务器。请检查 Base URL、网络和代理设置。".to_string(),
                    true,
                );
                err.detail = short(why);
                err
            }
            TransportError::Timeout(why) => {
                let mut err =
                    Self::new(ErrorKind::Timeout, "等服务器响应超时了。".to_string(), true);
                err.detail = short(why);
                err
            }
            TransportError::Interrupted(why) => Self::interrupted(why),
            TransportError::Other(why) => {
                let mut err = Self::new(ErrorKind::Unknown, "请求没能发出去。".to_string(), false);
                err.detail = short(why);
                err
            }
        }
    }

    /// 非 2xx 响应。`headers` 的名字要求已是小写。
    pub fn from_http(status: u16, headers: &[(String, String)], body: &[u8]) -> Self {
        let parsed = parse_error_body(body);
        let hint = parsed.hint_text();
        let retry_after = retry_after(headers);

        let (kind, message, retryable) = match status {
            300..=399 => {
                let location = header(headers, "location").unwrap_or_default();
                let mut text = format!(
                    "接口地址发生了跳转（HTTP {status}）。请把 Base URL 改成跳转后的最终地址"
                );
                if !location.is_empty() {
                    text.push_str(&format!("（跳转到：{}）", short(location).unwrap_or_default()));
                }
                text.push('。');
                (ErrorKind::Redirect, text, false)
            }
            402 | 403 | 429 if is_quota_text(&hint) => (
                ErrorKind::Quota,
                format!("账户额度不足或欠费了（HTTP {status}）。请到服务商那里充值，或换一个 Key。"),
                false,
            ),
            402 => (
                ErrorKind::Quota,
                "账户余额不足或欠费了（HTTP 402）。请到服务商那里充值，或换一个 Key。".to_string(),
                false,
            ),
            401 | 403 => {
                let mut text = format!(
                    "API Key 不对或没有权限（HTTP {status}）。请检查 Key 有没有填错、是否过期，以及这个 Key 能不能用这个模型。"
                );
                if parsed.is_html {
                    text.push_str("返回的是网页，也可能是网关 / 防火墙拦截了请求。");
                }
                (ErrorKind::Auth, text, false)
            }
            404 => (
                ErrorKind::NotFound,
                "接口地址或模型名不对（HTTP 404）。请检查 Base URL（通常要带 /v1 这样的版本段）和模型名。"
                    .to_string(),
                false,
            ),
            408 => (
                ErrorKind::Timeout,
                "服务器等请求等得超时了（HTTP 408）。".to_string(),
                true,
            ),
            429 => {
                let mut text = "请求太频繁，被服务商限流了（HTTP 429）".to_string();
                if let Some(wait) = retry_after {
                    text.push_str(&format!("，建议 {} 秒后再试", wait.as_secs().max(1)));
                }
                text.push('。');
                (ErrorKind::RateLimited, text, true)
            }
            400 | 413 | 422 if is_context_text(&hint) => (
                ErrorKind::ContextTooLong,
                "对话太长，超出了模型的上下文窗口。".to_string(),
                false,
            ),
            413 => (
                ErrorKind::BadRequest,
                "请求体太大，服务商拒收了（HTTP 413）。".to_string(),
                false,
            ),
            529 => (
                ErrorKind::Overloaded,
                "服务商那边暂时过载了（HTTP 529），稍后再试。".to_string(),
                true,
            ),
            503 => (
                ErrorKind::Overloaded,
                "服务商那边暂时不可用或过载了（HTTP 503），稍后再试。".to_string(),
                true,
            ),
            501 => (
                ErrorKind::Server,
                "服务商不支持这个请求（HTTP 501）。请检查接口类型选对了没有。".to_string(),
                false,
            ),
            500..=599 => (
                ErrorKind::Server,
                format!("服务商内部出错了（HTTP {status}），稍后再试。"),
                true,
            ),
            400..=499 => (
                ErrorKind::BadRequest,
                format!("服务商拒绝了这次请求（HTTP {status}）：参数或内容不合法。"),
                false,
            ),
            _ => (
                ErrorKind::Unknown,
                format!("服务商返回了意料之外的状态码（HTTP {status}）。"),
                false,
            ),
        };
        let mut err = Self::new(kind, message, retryable);
        err.status = Some(status);
        err.retry_after = retry_after;
        err.detail = parsed.display_detail();
        err.code = parsed.code_text();
        err
    }

    /// 流里的错误事件（或 200 响应体里的错误对象）。`kind_text` 是错误的 `type` / `code`。
    pub fn from_stream_error(kind_text: &str, message: &str) -> Self {
        let hint = format!("{kind_text} {message}").to_lowercase();
        let kind_lower = kind_text.to_lowercase();
        let (kind, text, retryable) = if is_quota_text(&hint) || kind_lower == "billing_error" {
            (
                ErrorKind::Quota,
                "账户额度不足或欠费了。请到服务商那里充值，或换一个 Key。",
                false,
            )
        } else if is_context_text(&hint) {
            (
                ErrorKind::ContextTooLong,
                "对话太长，超出了模型的上下文窗口。",
                false,
            )
        } else if kind_lower.contains("overloaded") {
            (
                ErrorKind::Overloaded,
                "服务商那边暂时过载了，稍后再试。",
                true,
            )
        } else if kind_lower.contains("rate_limit") {
            (ErrorKind::RateLimited, "请求太频繁，被服务商限流了。", true)
        } else if kind_lower == "api_error" || kind_lower.contains("server_error") {
            (ErrorKind::Server, "服务商内部出错了，稍后再试。", true)
        } else if kind_lower.contains("timeout") {
            (ErrorKind::Timeout, "服务商那边处理超时了。", true)
        } else if kind_lower.contains("authentication") || kind_lower.contains("permission") {
            (
                ErrorKind::Auth,
                "API Key 不对或没有权限。请检查 Key 有没有填错、是否过期。",
                false,
            )
        } else if kind_lower.contains("not_found") {
            (
                ErrorKind::NotFound,
                "接口地址或模型名不对。请检查 Base URL 和模型名。",
                false,
            )
        } else if kind_lower.contains("invalid_request") || kind_lower.contains("request_too_large")
        {
            (
                ErrorKind::BadRequest,
                "服务商拒绝了这次请求：参数或内容不合法。",
                false,
            )
        } else {
            (ErrorKind::Unknown, "服务商在回复中途报了错。", false)
        };
        let mut err = Self::new(kind, text.to_string(), retryable);
        err.detail = short(message);
        err.code = Some(kind_text.trim().to_owned()).filter(|code| !code.is_empty());
        err
    }

    /// HTTP 200 但内容不是 SSE。`body` 是已读到的开头一段。
    pub fn not_sse(content_type: Option<&str>, body: &[u8]) -> Self {
        let parsed = parse_error_body(body);
        // 有的网关忽略 `stream: true`，直接用 200 回一个错误 JSON：按里面的错误类型分类。
        if let Some((kind_text, message)) = parsed.error_pair() {
            let mut err = Self::from_stream_error(&kind_text, &message);
            err.status = Some(200);
            return err;
        }
        let what = if parsed.is_html {
            "返回了一个网页（HTML）"
        } else if body.iter().all(u8::is_ascii_whitespace) {
            "返回了空内容"
        } else if parsed.is_json {
            "返回了一个普通 JSON，不是流式数据"
        } else {
            "返回的内容不是流式数据"
        };
        let mut err = Self::new(
            ErrorKind::NotSse,
            format!(
                "服务器{what}。通常是 Base URL 填错了（填成了网页地址），或者被网关 / 防火墙拦截了。"
            ),
            false,
        );
        err.status = Some(200);
        let mut detail = String::new();
        if let Some(ct) = content_type {
            detail.push_str(&format!("content-type: {ct}"));
        }
        if !parsed.is_html && !body.iter().all(u8::is_ascii_whitespace) {
            if !detail.is_empty() {
                detail.push('；');
            }
            detail.push_str(&String::from_utf8_lossy(body));
        }
        err.detail = short(&detail);
        err
    }
}

/// 传输层错误（由 [`super::transport::Transport`] 的实现给出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// 建立连接失败（含 DNS、TLS、代理握手）。
    Connect(String),
    /// 超时。
    Timeout(String),
    /// 已经收到响应后，读响应体时断了。
    Interrupted(String),
    /// 其它。
    Other(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Connect(why)
            | TransportError::Timeout(why)
            | TransportError::Interrupted(why)
            | TransportError::Other(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for TransportError {}

/// 截短并去掉换行，放进 `detail`。
fn short(text: &str) -> Option<String> {
    let flat: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(DETAIL_MAX_CHARS + 1)
        .collect();
    if flat.is_empty() {
        return None;
    }
    if flat.chars().count() > DETAIL_MAX_CHARS {
        let cut: String = flat.chars().take(DETAIL_MAX_CHARS).collect();
        Some(format!("{cut}…"))
    } else {
        Some(flat)
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// `retry-after`（秒数）或 `retry-after-ms`（OpenAI 会发）。HTTP 日期格式不解析，当没有。
fn retry_after(headers: &[(String, String)]) -> Option<Duration> {
    let parsed = if let Some(ms) = header(headers, "retry-after-ms") {
        ms.trim().parse::<f64>().ok().map(|v| v / 1000.0)
    } else {
        header(headers, "retry-after").and_then(|secs| secs.trim().parse::<f64>().ok())
    }?;
    if !parsed.is_finite() || parsed < 0.0 {
        return None;
    }
    // 先在浮点数上夹到上限再转 Duration：`Duration::from_secs_f64` 对超出范围的值（`1e300`）会 panic，
    // 先转再取 min 就来不及了；服务端发来的头不能让对话线程崩溃。
    let capped = parsed.min(RETRY_AFTER_MAX.as_secs_f64());
    Duration::try_from_secs_f64(capped).ok()
}

fn is_quota_text(lower_or_not: &str) -> bool {
    let lower = lower_or_not.to_lowercase();
    // 不单独认 "quota"：有的服务（如 Gemini）把按分钟的限流也叫 quota exceeded，那种该重试。
    [
        "insufficient_quota",
        "insufficient balance",
        "insufficient_balance",
        "exceeded your current quota",
        "billing",
        "credit balance",
        "out of credit",
        "余额",
        "欠费",
        "额度不足",
    ]
    .iter()
    .any(|key| lower.contains(key))
}

fn is_context_text(lower_or_not: &str) -> bool {
    let lower = lower_or_not.to_lowercase();
    [
        "context_length_exceeded",
        "maximum context length",
        "context window",
        "prompt is too long",
        "input is too long",
        "too many tokens",
        "reduce the length",
        "input length",
        "exceeds the model's maximum",
    ]
    .iter()
    .any(|key| lower.contains(key))
}

/// 从错误响应体里挖出的信息。
#[derive(Default)]
struct ParsedBody {
    is_html: bool,
    is_json: bool,
    /// `error.code`（数字也转成文字）。
    code: String,
    error_type: String,
    message: String,
    raw: String,
}

impl ParsedBody {
    /// 用来做关键词判断的文本（类型 + 消息，或没解析出来时的原文）。
    fn hint_text(&self) -> String {
        if self.is_html {
            return String::new();
        }
        format!("{} {} {}", self.error_type, self.message, self.raw)
    }

    /// 错误码：优先 `error.code`，没有就取类型。
    fn code_text(&self) -> Option<String> {
        let code = if self.code.is_empty() {
            self.error_type
                .split_whitespace()
                .next()
                .unwrap_or_default()
        } else {
            self.code.as_str()
        };
        Some(code.to_owned()).filter(|code| !code.is_empty())
    }

    fn display_detail(&self) -> Option<String> {
        if self.is_html {
            return None;
        }
        if !self.message.is_empty() {
            return short(&self.message);
        }
        short(&self.raw)
    }

    fn error_pair(&self) -> Option<(String, String)> {
        if self.is_json && (!self.error_type.is_empty() || !self.message.is_empty()) {
            Some((self.error_type.clone(), self.message.clone()))
        } else {
            None
        }
    }
}

fn parse_error_body(body: &[u8]) -> ParsedBody {
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim_start_matches('\u{feff}').trim();
    let mut parsed = ParsedBody::default();
    let lower_head: String = trimmed.chars().take(64).collect::<String>().to_lowercase();
    if lower_head.starts_with('<') || lower_head.contains("<html") {
        parsed.is_html = true;
        return parsed;
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => {
            parsed.is_json = true;
            // OpenAI / Anthropic：{"error": {"message", "type", "code"}}；
            // 有的网关：{"error": "文字"} 或 {"message": "文字"}、FastAPI 的 {"detail": "文字"}。
            let error = value.get("error");
            let pick = |obj: &Value, key: &str| -> String {
                match obj.get(key) {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Number(n)) => n.to_string(),
                    _ => String::new(),
                }
            };
            match error {
                Some(Value::Object(_)) => {
                    let obj = error.unwrap_or(&Value::Null);
                    parsed.message = pick(obj, "message");
                    let kind = pick(obj, "type");
                    let code = pick(obj, "code");
                    parsed.code = code.clone();
                    parsed.error_type = if kind.is_empty() { code.clone() } else { kind };
                    if !code.is_empty() && !parsed.error_type.contains(&code) {
                        parsed.error_type.push(' ');
                        parsed.error_type.push_str(&code);
                    }
                }
                Some(Value::String(s)) => parsed.message = s.clone(),
                _ => {
                    parsed.message = pick(&value, "message");
                    if parsed.message.is_empty() {
                        parsed.message = pick(&value, "detail");
                    }
                }
            }
        }
        Err(_) => parsed.raw = trimmed.chars().take(DETAIL_MAX_CHARS * 2).collect(),
    }
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(status: u16, headers: &[(&str, &str)], body: &str) -> ProviderError {
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        ProviderError::from_http(status, &headers, body.as_bytes())
    }

    #[test]
    fn auth_errors_are_not_retryable() {
        for status in [401, 403] {
            let err = http(
                status,
                &[],
                r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error","code":"invalid_api_key"}}"#,
            );
            assert_eq!(err.kind, ErrorKind::Auth, "{status}");
            assert!(!err.retryable);
            assert!(err.message.contains("API Key"), "{}", err.message);
            assert_eq!(err.detail.as_deref(), Some("Incorrect API key provided"));
        }
    }

    #[test]
    fn not_found_points_at_url_and_model() {
        let err = http(
            404,
            &[],
            r#"{"error":{"message":"The model does not exist"}}"#,
        );
        assert_eq!(err.kind, ErrorKind::NotFound);
        assert!(!err.retryable);
        assert!(err.message.contains("Base URL") && err.message.contains("模型"));
    }

    #[test]
    fn rate_limit_carries_retry_after() {
        let err = http(
            429,
            &[("retry-after", "7")],
            r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
        );
        assert_eq!(err.kind, ErrorKind::RateLimited);
        assert!(err.retryable);
        assert_eq!(err.retry_after, Some(Duration::from_secs(7)));
        assert!(err.message.contains("7 秒"), "{}", err.message);
    }

    #[test]
    fn retry_after_ms_and_clamp() {
        let err = http(429, &[("retry-after-ms", "1500")], "{}");
        assert_eq!(err.retry_after, Some(Duration::from_millis(1500)));
        let err = http(429, &[("retry-after", "999999")], "{}");
        assert_eq!(err.retry_after, Some(RETRY_AFTER_MAX));
        // HTTP 日期格式不解析。
        let err = http(
            429,
            &[("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT")],
            "{}",
        );
        assert_eq!(err.retry_after, None);
        assert!(err.retryable);
    }

    /// 2026-11-11 审查：`Duration::from_secs_f64(1e300)` 会 panic，对话卡在"进行中"。
    #[test]
    fn absurd_retry_after_values_never_panic() {
        for header_name in ["retry-after", "retry-after-ms"] {
            // 超大值夹到上限
            for huge in [
                "1e300",
                "1e18",
                "1.7976931348623157e308",
                "99999999999999999999",
            ] {
                let err = http(429, &[(header_name, huge)], "{}");
                assert_eq!(
                    err.retry_after,
                    Some(RETRY_AFTER_MAX),
                    "{header_name}: {huge}"
                );
            }
            // 负数、NaN、无穷当成没有
            for bad in ["-1", "-1e300", "NaN", "inf", "-inf", "", "abc"] {
                let err = http(429, &[(header_name, bad)], "{}");
                assert_eq!(err.retry_after, None, "{header_name}: {bad:?}");
                assert!(err.retryable, "没有等待时间也照样可重试");
            }
        }
    }

    #[test]
    fn quota_429_is_not_a_rate_limit() {
        let err = http(
            429,
            &[],
            r#"{"error":{"message":"You exceeded your current quota","type":"insufficient_quota","code":"insufficient_quota"}}"#,
        );
        assert_eq!(err.kind, ErrorKind::Quota);
        assert!(!err.retryable);
    }

    #[test]
    fn payment_required_is_quota() {
        let err = http(402, &[], r#"{"error":{"message":"Insufficient Balance"}}"#);
        assert_eq!(err.kind, ErrorKind::Quota);
        assert!(!err.retryable);
        // 网关把额度不足报成 403 时也要认出来，不能说成 Key 错。
        let err = http(403, &[], r#"{"error":{"message":"用户额度不足"}}"#);
        assert_eq!(err.kind, ErrorKind::Quota);
    }

    #[test]
    fn server_errors_are_retryable() {
        for status in [500, 502, 503, 504, 529] {
            let err = http(status, &[], "");
            assert!(err.retryable, "{status}");
            assert!(matches!(
                err.kind,
                ErrorKind::Server | ErrorKind::Overloaded
            ));
        }
        // 501 是「不支持」，重试没用。
        assert!(!http(501, &[], "").retryable);
    }

    #[test]
    fn context_overflow_is_recognized() {
        let err = http(
            400,
            &[],
            r#"{"error":{"message":"This model's maximum context length is 128000 tokens","code":"context_length_exceeded"}}"#,
        );
        assert_eq!(err.kind, ErrorKind::ContextTooLong);
        assert!(!err.retryable);
        let err = http(
            400,
            &[],
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 maximum"}}"#,
        );
        assert_eq!(err.kind, ErrorKind::ContextTooLong);
        // 普通 400 不是上下文问题。
        assert_eq!(
            http(400, &[], r#"{"error":{"message":"bad param"}}"#).kind,
            ErrorKind::BadRequest
        );
    }

    #[test]
    fn html_error_page_is_not_echoed() {
        let err = http(
            403,
            &[],
            "<!DOCTYPE html><html><body>Access denied</body></html>",
        );
        assert_eq!(err.kind, ErrorKind::Auth);
        assert_eq!(err.detail, None);
        assert!(err.message.contains("网页"));
        let err = http(502, &[], "<html>Bad gateway</html>");
        assert_eq!(err.detail, None);
    }

    #[test]
    fn redirect_names_the_target() {
        let err = http(
            301,
            &[("location", "https://example.test/v1/chat/completions")],
            "",
        );
        assert_eq!(err.kind, ErrorKind::Redirect);
        assert!(!err.retryable);
        assert!(err.message.contains("example.test"));
    }

    #[test]
    fn transport_errors_map_to_chinese_reasons() {
        let err = ProviderError::from_transport(&TransportError::Connect("dns".into()));
        assert_eq!(err.kind, ErrorKind::Connect);
        assert!(err.retryable && err.message.contains("连不上"));
        let err = ProviderError::from_transport(&TransportError::Timeout("t".into()));
        assert_eq!(err.kind, ErrorKind::Timeout);
        assert!(err.retryable);
        let err = ProviderError::from_transport(&TransportError::Interrupted("reset".into()));
        assert_eq!(err.kind, ErrorKind::Interrupted);
        assert!(err.retryable);
        let err = ProviderError::from_transport(&TransportError::Other("x".into()));
        assert!(!err.retryable);
    }

    #[test]
    fn stream_error_types() {
        let e = ProviderError::from_stream_error("overloaded_error", "Overloaded");
        assert_eq!((e.kind, e.retryable), (ErrorKind::Overloaded, true));
        let e = ProviderError::from_stream_error("api_error", "Internal server error");
        assert_eq!((e.kind, e.retryable), (ErrorKind::Server, true));
        let e = ProviderError::from_stream_error("rate_limit_error", "x");
        assert_eq!((e.kind, e.retryable), (ErrorKind::RateLimited, true));
        let e = ProviderError::from_stream_error("authentication_error", "x");
        assert_eq!((e.kind, e.retryable), (ErrorKind::Auth, false));
        let e = ProviderError::from_stream_error("invalid_request_error", "x");
        assert_eq!((e.kind, e.retryable), (ErrorKind::BadRequest, false));
        let e = ProviderError::from_stream_error("", "something odd");
        assert_eq!((e.kind, e.retryable), (ErrorKind::Unknown, false));
    }

    #[test]
    fn not_sse_distinguishes_html_json_and_empty() {
        let e = ProviderError::not_sse(Some("text/html"), b"<html>login</html>");
        assert_eq!(e.kind, ErrorKind::NotSse);
        assert!(e.message.contains("网页"));
        assert!(!e.retryable);
        let e = ProviderError::not_sse(Some("application/json"), br#"{"id":"x","choices":[]}"#);
        assert_eq!(e.kind, ErrorKind::NotSse);
        assert!(e.message.contains("JSON"));
        let e = ProviderError::not_sse(None, b"  \n");
        assert!(e.message.contains("空内容"));
        // 200 里带错误对象：按错误类型分类。
        let e = ProviderError::not_sse(
            Some("application/json"),
            br#"{"error":{"type":"overloaded_error","message":"busy"}}"#,
        );
        assert_eq!(e.kind, ErrorKind::Overloaded);
        assert!(e.retryable);
    }

    #[test]
    fn scrub_removes_the_key_everywhere() {
        let mut err = http(
            401,
            &[],
            r#"{"error":{"message":"Invalid key sk-test-SECRET-123 provided"}}"#,
        );
        err.scrub("sk-test-SECRET-123");
        assert!(!err.to_string().contains("SECRET"), "{err}");
        assert!(err.to_string().contains("***"));
    }

    #[test]
    fn detail_is_truncated() {
        let long = "x".repeat(2000);
        let err = http(500, &[], &format!(r#"{{"error":{{"message":"{long}"}}}}"#));
        let detail = err.detail.unwrap_or_default();
        assert!(
            detail.chars().count() <= DETAIL_MAX_CHARS + 1,
            "{}",
            detail.len()
        );
        assert!(detail.ends_with('…'));
    }
}
