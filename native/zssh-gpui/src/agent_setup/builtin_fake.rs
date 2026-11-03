//! 内置助手的假接口（调试版自动化 / 单元测试用，**不联网**）：一个进程内的 [`Transport`]，
//! 按请求里的 Key 和最后一条用户消息决定回什么。设了 `ZSSH_BUILTIN_FAKE`（只在调试版里读，
//! 见 `dev_hooks`）时，面板和设置页的"测试"按钮都改走它，界面上显示的仍是示例地址
//! `https://api.example.com/v1`，截图里不会出现任何真实地址。
//!
//! 约定（Key 是明显的假值）：
//! - Key 以 `sk-test-bad` 开头：认证失败（401）；`sk-test-topup`：余额不足（402，错误码
//!   `zeeterm_requires_topup`）；`sk-test-nomodel`：模型不存在（404）；别的都能用；
//! - `GET …/models` 返回几个常见模型（OpenAI 兼容）或 Claude 系列（Anthropic）；
//! - 最后一条用户消息是"回复 OK"（测试按钮）→ 回 `OK`；以 `tool:` 开头 → 先调一次 `list_targets`，
//!   工具结果回来后再给结论；以 `flaky:` 开头 → 第一次请求说到一半断开（触发重试），第二次说完；
//!   以 `usage <数字>` 开头 → 把请求的输入 token 报成这个数（演示用量警示色）；
//!   其余原样回一句 `收到：…`。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use serde_json::{Value, json};
use zssh_core::ai::agent::builtin::provider::{
    ByteStream, HttpRequest, HttpResponse, Transport, TransportError,
};
use zssh_core::ssh::BoxFuture;

/// 调试版里设了 `ZSSH_BUILTIN_FAKE` 才有；release 里恒为 `None`。
pub fn transport() -> Option<Arc<dyn Transport>> {
    crate::dev_hooks::var("ZSSH_BUILTIN_FAKE")?;
    static FAKE: OnceLock<Arc<FakeTransport>> = OnceLock::new();
    let fake = FAKE.get_or_init(|| Arc::new(FakeTransport::default()));
    Some(Arc::clone(fake) as Arc<dyn Transport>)
}

#[derive(Default)]
pub struct FakeTransport {
    /// `flaky:` 场景的请求计数。
    flaky_posts: AtomicUsize,
    /// 收到过的 POST 请求数（测试断言用）。
    posts: AtomicUsize,
}

impl FakeTransport {
    #[cfg(test)]
    pub fn post_count(&self) -> usize {
        self.posts.load(Ordering::SeqCst)
    }
}

struct Body {
    chunks: VecDeque<Vec<u8>>,
    then_error: Option<TransportError>,
}

impl ByteStream for Body {
    fn next_chunk<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<Vec<u8>>, TransportError>> {
        Box::pin(async move {
            if let Some(chunk) = self.chunks.pop_front() {
                return Ok(Some(chunk));
            }
            match self.then_error.take() {
                Some(err) => Err(err),
                None => Ok(None),
            }
        })
    }
}

fn response(status: u16, content_type: &str, body: Vec<u8>) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![("content-type".to_owned(), content_type.to_owned())],
        body: Box::new(Body {
            chunks: VecDeque::from([body]),
            then_error: None,
        }),
    }
}

fn header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn api_key(request: &HttpRequest) -> String {
    if let Some(value) = header(request, "authorization") {
        return value.trim_start_matches("Bearer ").trim().to_owned();
    }
    header(request, "x-api-key").unwrap_or_default().to_owned()
}

fn is_anthropic(request: &HttpRequest) -> bool {
    header(request, "anthropic-version").is_some()
}

/// 按 Key 约定给出错误响应（Key 没问题返回 `None`）。
fn key_error(request: &HttpRequest) -> Option<HttpResponse> {
    let key = api_key(request);
    let anthropic = is_anthropic(request);
    let error = |status: u16, kind: &str, code: Option<&str>, message: &str| {
        let body = if anthropic {
            json!({"type": "error", "error": {"type": kind, "message": message}})
        } else {
            json!({"error": {"message": message, "type": kind, "code": code}})
        };
        response(status, "application/json", body.to_string().into_bytes())
    };
    if key.starts_with("sk-test-bad") {
        return Some(error(
            401,
            if anthropic {
                "authentication_error"
            } else {
                "invalid_request_error"
            },
            Some("invalid_api_key"),
            "invalid x-api-key",
        ));
    }
    if key.starts_with("sk-test-topup") {
        return Some(error(
            402,
            "insufficient_quota",
            Some("zeeterm_requires_topup"),
            "balance is not enough",
        ));
    }
    let is_chat = request.url.ends_with("/chat/completions") || request.url.ends_with("/messages");
    if key.starts_with("sk-test-nomodel") && is_chat {
        return Some(error(
            404,
            "not_found_error",
            Some("model_not_found"),
            "model not found",
        ));
    }
    None
}

fn models_body(anthropic: bool) -> Vec<u8> {
    let body = if anthropic {
        json!({"data": [
            {"id": "claude-sonnet-5-5", "display_name": "Claude Sonnet 5.5", "type": "model"},
            {"id": "claude-haiku-4-5", "display_name": "Claude Haiku 4.5", "type": "model"},
        ]})
    } else {
        json!({"object": "list", "data": [
            {"id": "gpt-5.5", "object": "model"},
            {"id": "deepseek-v4-flash", "object": "model"},
            {"id": "my-chat-model", "object": "model"},
        ]})
    };
    body.to_string().into_bytes()
}

/// 请求体里最后一条用户文字，以及它后面有没有出现工具结果。
fn last_user_and_tool_state(body: &Value) -> (String, bool) {
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let mut text = String::new();
    let mut tool_after = false;
    for message in messages.iter().rev() {
        let role = message["role"].as_str().unwrap_or("");
        let content = &message["content"];
        let is_tool_result = role == "tool"
            || content
                .as_array()
                .is_some_and(|parts| parts.iter().any(|p| p["type"] == "tool_result"));
        if is_tool_result {
            tool_after = true;
            continue;
        }
        if role == "user" {
            text = match content {
                Value::String(s) => s.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join(""),
                _ => String::new(),
            };
            break;
        }
    }
    (text, tool_after)
}

fn sse(event: Option<&str>, data: Value) -> Vec<u8> {
    let mut out = String::new();
    if let Some(event) = event {
        out.push_str(&format!("event: {event}\n"));
    }
    out.push_str(&format!("data: {data}\n\n"));
    out.into_bytes()
}

/// 一次回复要说什么、要不要调工具。
struct Reply {
    text_parts: Vec<String>,
    tool: Option<(&'static str, Value)>,
    input_tokens: u64,
    output_tokens: u64,
    /// 说到一半断开（`flaky:` 的第一次）。
    break_after_text: bool,
}

fn plan(text: &str, tool_after: bool, first_flaky: bool, messages: usize) -> Reply {
    let base_input = 900 + 60 * messages as u64;
    let mut reply = Reply {
        text_parts: Vec::new(),
        tool: None,
        input_tokens: base_input,
        output_tokens: 24,
        break_after_text: false,
    };
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("usage ") {
        let number: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(n) = number.parse::<u64>() {
            reply.input_tokens = n;
        }
    }
    if tool_after {
        reply.text_parts = vec!["看过了：".into(), "这个对话里有一台可操作的目标。".into()];
    } else if trimmed == "回复 OK" {
        reply.text_parts = vec!["OK".into()];
        reply.output_tokens = 2;
    } else if trimmed.starts_with("tool:") {
        reply.text_parts = vec!["我先看一下有哪些目标。".into()];
        reply.tool = Some(("list_targets", json!({})));
    } else if trimmed.starts_with("flaky:") {
        if first_flaky {
            reply.text_parts = vec!["我先看一下，".into(), "这一段会被撤回。".into()];
            reply.break_after_text = true;
        } else {
            reply.text_parts = vec!["我先看一下，".into(), "这次完整说完了。".into()];
        }
    } else {
        reply.text_parts = vec![format!("收到：{trimmed}")];
    }
    reply
}

fn openai_stream(reply: &Reply) -> (Vec<Vec<u8>>, Option<TransportError>) {
    let mut chunks = vec![sse(
        None,
        json!({"id": "fake-1", "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}}]}),
    )];
    for part in &reply.text_parts {
        chunks.push(sse(
            None,
            json!({"id": "fake-1", "choices": [{"index": 0, "delta": {"content": part}}]}),
        ));
    }
    if reply.break_after_text {
        return (
            chunks,
            Some(TransportError::Interrupted("连接被对方关闭".to_owned())),
        );
    }
    let finish = if let Some((name, args)) = &reply.tool {
        chunks.push(sse(
            None,
            json!({"id": "fake-1", "choices": [{"index": 0, "delta": {"tool_calls": [{
                "index": 0, "id": "call_fake_1", "type": "function",
                "function": {"name": name, "arguments": args.to_string()}
            }]}}]}),
        ));
        "tool_calls"
    } else {
        "stop"
    };
    chunks.push(sse(
        None,
        json!({"id": "fake-1", "choices": [{"index": 0, "delta": {}, "finish_reason": finish}],
               "usage": {"prompt_tokens": reply.input_tokens, "completion_tokens": reply.output_tokens,
                         "total_tokens": reply.input_tokens + reply.output_tokens}}),
    ));
    chunks.push(b"data: [DONE]\n\n".to_vec());
    (chunks, None)
}

fn anthropic_stream(reply: &Reply) -> (Vec<Vec<u8>>, Option<TransportError>) {
    let mut chunks = vec![
        sse(
            Some("message_start"),
            json!({"type": "message_start", "message": {"id": "msg_fake_1", "type": "message",
                "role": "assistant", "model": "fake", "content": [], "stop_reason": null,
                "usage": {"input_tokens": reply.input_tokens, "output_tokens": 1}}}),
        ),
        sse(
            Some("content_block_start"),
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text", "text": ""}}),
        ),
    ];
    for part in &reply.text_parts {
        chunks.push(sse(
            Some("content_block_delta"),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": part}}),
        ));
    }
    if reply.break_after_text {
        return (
            chunks,
            Some(TransportError::Interrupted("连接被对方关闭".to_owned())),
        );
    }
    chunks.push(sse(
        Some("content_block_stop"),
        json!({"type": "content_block_stop", "index": 0}),
    ));
    let stop = if let Some((name, args)) = &reply.tool {
        chunks.push(sse(
            Some("content_block_start"),
            json!({"type": "content_block_start", "index": 1, "content_block":
                   {"type": "tool_use", "id": "toolu_fake_1", "name": name, "input": {}}}),
        ));
        chunks.push(sse(
            Some("content_block_delta"),
            json!({"type": "content_block_delta", "index": 1,
                   "delta": {"type": "input_json_delta", "partial_json": args.to_string()}}),
        ));
        chunks.push(sse(
            Some("content_block_stop"),
            json!({"type": "content_block_stop", "index": 1}),
        ));
        "tool_use"
    } else {
        "end_turn"
    };
    chunks.push(sse(
        Some("message_delta"),
        json!({"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": null},
               "usage": {"output_tokens": reply.output_tokens}}),
    ));
    chunks.push(sse(Some("message_stop"), json!({"type": "message_stop"})));
    (chunks, None)
}

impl Transport for FakeTransport {
    fn post<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            self.posts.fetch_add(1, Ordering::SeqCst);
            if let Some(error) = key_error(&request) {
                return Ok(error);
            }
            let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let (text, tool_after) = last_user_and_tool_state(&body);
            let messages = body["messages"].as_array().map_or(0, Vec::len);
            let first_flaky = text.trim().starts_with("flaky:")
                && self.flaky_posts.fetch_add(1, Ordering::SeqCst) == 0;
            let reply = plan(&text, tool_after, first_flaky, messages);
            let (chunks, then_error) = if is_anthropic(&request) {
                anthropic_stream(&reply)
            } else {
                openai_stream(&reply)
            };
            Ok(HttpResponse {
                status: 200,
                headers: vec![("content-type".to_owned(), "text/event-stream".to_owned())],
                body: Box::new(Body {
                    chunks: chunks.into(),
                    then_error,
                }),
            })
        })
    }

    fn get<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            if let Some(error) = key_error(&request) {
                return Ok(error);
            }
            Ok(response(
                200,
                "application/json",
                models_body(is_anthropic(&request)),
            ))
        })
    }
}
