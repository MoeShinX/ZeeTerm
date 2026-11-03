//! 把请求构造、HTTP 传输、SSE 解析和流事件拼起来的入口：[`stream_chat`]。

use std::fmt;

use super::anthropic::{self, AnthropicDecoder, AnthropicOptions};
use super::error::ProviderError;
use super::openai_chat::{self, OpenAiDecoder, OpenAiOptions};
use super::sse::{SseEvent, SseParser};
use super::transport::{ByteStream, HttpRequest, Transport};
use super::types::{AssistantTurn, ChatRequest, StreamEvent, TurnAccumulator};

/// 接口类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    /// OpenAI 兼容 Chat Completions。
    OpenAiChat,
    /// Anthropic Messages。
    Anthropic,
}

/// API Key。`Debug` / `Display` 都不会露出内容。
#[derive(Clone, PartialEq, Eq, Default)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.is_empty() {
            "ApiKey(empty)"
        } else {
            "ApiKey(***)"
        })
    }
}

/// 一个接口的连接配置。
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key: ApiKey,
    /// 额外的请求头（比如 OpenRouter 的 `HTTP-Referer`）。值可能是密钥，不打印。
    pub extra_headers: Vec<(String, String)>,
    pub openai: OpenAiOptions,
    pub anthropic: AnthropicOptions,
}

impl ProviderConfig {
    pub fn new(kind: ProviderKind, base_url: impl Into<String>, api_key: ApiKey) -> Self {
        Self {
            kind,
            base_url: base_url.into(),
            api_key,
            extra_headers: Vec::new(),
            openai: OpenAiOptions::default(),
            anthropic: AnthropicOptions::default(),
        }
    }
}

/// 构造 HTTP 请求（地址、头、请求体）。
pub fn build_http_request(
    config: &ProviderConfig,
    request: &ChatRequest,
) -> Result<HttpRequest, ProviderError> {
    let (url, body, mut headers) = match config.kind {
        ProviderKind::OpenAiChat => {
            let mut headers = Vec::new();
            if !config.api_key.is_empty() {
                headers.push((
                    "authorization".to_string(),
                    format!("Bearer {}", config.api_key.expose()),
                ));
            }
            (
                openai_chat::endpoint_url(&config.base_url),
                openai_chat::build_body(request, &config.openai),
                headers,
            )
        }
        ProviderKind::Anthropic => {
            let mut headers = vec![(
                "anthropic-version".to_string(),
                config.anthropic.version.clone(),
            )];
            if !config.api_key.is_empty() {
                headers.push(("x-api-key".to_string(), config.api_key.expose().to_string()));
            }
            (
                anthropic::endpoint_url(&config.base_url),
                anthropic::build_body(request, &config.anthropic),
                headers,
            )
        }
    };
    headers.push(("content-type".to_string(), "application/json".to_string()));
    headers.push(("accept".to_string(), "text/event-stream".to_string()));
    headers.extend(config.extra_headers.iter().cloned());
    let body = serde_json::to_vec(&body).map_err(|err| {
        ProviderError::from_transport(&super::error::TransportError::Other(format!(
            "请求体序列化失败：{err}"
        )))
    })?;
    Ok(HttpRequest { url, headers, body })
}

enum Decoder {
    OpenAi(OpenAiDecoder),
    Anthropic(AnthropicDecoder),
}

impl Decoder {
    fn push(&mut self, event: &SseEvent) -> Vec<StreamEvent> {
        match self {
            Decoder::OpenAi(d) => d.push(event),
            Decoder::Anthropic(d) => d.push(event),
        }
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        match self {
            Decoder::OpenAi(d) => d.finish(),
            Decoder::Anthropic(d) => d.finish(),
        }
    }

    fn is_done(&self) -> bool {
        match self {
            Decoder::OpenAi(d) => d.is_done(),
            Decoder::Anthropic(d) => d.is_done(),
        }
    }
}

/// 错误响应体最多读这么多（只为了拿到错误码和 message）。
const ERROR_BODY_LIMIT: usize = 64 * 1024;

async fn read_limited(body: &mut dyn ByteStream, limit: usize) -> Vec<u8> {
    let mut out = Vec::new();
    while out.len() < limit {
        match body.next_chunk().await {
            Ok(Some(chunk)) => out.extend_from_slice(&chunk),
            // 读失败就用已经读到的：错误提示不需要完整的响应体。
            Ok(None) | Err(_) => break,
        }
    }
    out.truncate(limit);
    out
}

/// 响应体的开头看起来像不像 SSE。
fn looks_like_sse_start(first_byte: u8) -> bool {
    // `data:` / `event:` / `id:` / `retry:` / 注释 `:`
    matches!(first_byte, b'd' | b'e' | b'i' | b'r' | b':')
}

/// 发请求、读流、解码，把每个 [`StreamEvent`] 交给 `on_event`，流正常收完后返回拼好的回合。
///
/// - 出错时（HTTP 错误、传输中断、流里的 `error` 事件、响应不是 SSE、流没收完整就断了）
///   返回 `Err`；流里的 `error` 事件会先通过 `on_event` 交出去。
/// - 取消：丢掉这个 future 即可（连接随之关闭），不会再有事件发出。
/// - 错误里不会带 API Key（服务端有时会回显）。
pub async fn stream_chat(
    transport: &dyn Transport,
    config: &ProviderConfig,
    request: &ChatRequest,
    on_event: &mut (dyn FnMut(StreamEvent) + Send),
) -> Result<AssistantTurn, ProviderError> {
    run(transport, config, request, on_event)
        .await
        .map_err(|mut err| {
            err.scrub(config.api_key.expose());
            err
        })
}

async fn run(
    transport: &dyn Transport,
    config: &ProviderConfig,
    request: &ChatRequest,
    on_event: &mut (dyn FnMut(StreamEvent) + Send),
) -> Result<AssistantTurn, ProviderError> {
    let http_request = build_http_request(config, request)?;
    tracing::debug!(request = ?request, kind = ?config.kind, "发送内置助手请求");
    let mut response = transport
        .post(http_request)
        .await
        .map_err(|err| ProviderError::from_transport(&err))?;

    if !(200..300).contains(&response.status) {
        let body = read_limited(response.body.as_mut(), ERROR_BODY_LIMIT).await;
        return Err(ProviderError::from_http(
            response.status,
            &response.headers,
            &body,
        ));
    }

    let content_type = response.header("content-type").map(str::to_string);
    let declared_sse = content_type
        .as_deref()
        .is_some_and(|ct| ct.to_ascii_lowercase().contains("text/event-stream"));

    let mut decoder = match config.kind {
        ProviderKind::OpenAiChat => Decoder::OpenAi(OpenAiDecoder::new()),
        ProviderKind::Anthropic => Decoder::Anthropic(AnthropicDecoder::new()),
    };
    let mut parser = SseParser::new();
    let mut acc = TurnAccumulator::new();
    // 头里没声明 SSE 时，看第一个非空白字节决定是不是真的 SSE（有的网关不带这个头）。
    let mut verified = declared_sse;
    let mut got_bytes = false;
    // 开头一小段，万一不是 SSE 用来描述它。
    let mut head: Vec<u8> = Vec::new();

    macro_rules! deliver {
        ($events:expr) => {
            for event in $events {
                acc.push(&event);
                if let StreamEvent::Error { error } = &event {
                    let mut error = error.clone();
                    error.scrub(config.api_key.expose());
                    on_event(StreamEvent::Error {
                        error: error.clone(),
                    });
                    return Err(error);
                }
                on_event(event);
            }
        };
    }

    loop {
        let chunk = response
            .body
            .next_chunk()
            .await
            .map_err(|err| ProviderError::from_transport(&err))?;
        let Some(chunk) = chunk else { break };
        if chunk.is_empty() {
            continue;
        }
        got_bytes = true;
        if head.len() < 4096 {
            head.extend_from_slice(&chunk[..chunk.len().min(4096 - head.len())]);
        }
        if !verified {
            match chunk.iter().find(|b| !b.is_ascii_whitespace()) {
                // 全是空白：下一块再看。
                None => continue,
                Some(&first) if looks_like_sse_start(first) => verified = true,
                Some(_) => {
                    // HTML 错误页、普通 JSON 之类：多读一点用来描述它。
                    let mut sniffed = chunk;
                    sniffed.extend(read_limited(response.body.as_mut(), 16 * 1024).await);
                    return Err(ProviderError::not_sse(content_type.as_deref(), &sniffed));
                }
            }
        }
        for sse in parser.push(&chunk) {
            deliver!(decoder.push(&sse));
            if decoder.is_done() {
                break;
            }
        }
        if decoder.is_done() {
            break;
        }
    }

    if !decoder.is_done() {
        for sse in parser.finish() {
            deliver!(decoder.push(&sse));
        }
    }
    if !got_bytes || !parser.saw_sse_syntax() {
        return Err(ProviderError::not_sse(content_type.as_deref(), &head));
    }
    deliver!(decoder.finish()?);
    Ok(acc.finish())
}
