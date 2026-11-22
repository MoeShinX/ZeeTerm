//! Anthropic Messages（`POST {base}/v1/messages`）：请求体构造 + SSE 流解析。
//!
//! 依据（2026-11-03 查的官方文档，链接汇总在报告里）：
//! - 流式事件：<https://platform.claude.com/docs/en/build-with-claude/streaming>
//! - 思考：<https://platform.claude.com/docs/en/build-with-claude/thinking> 与
//!   `extended-thinking`、`effort` 三页
//! - 缓存：<https://platform.claude.com/docs/en/build-with-claude/prompt-caching>
//!
//! # 请求
//! - 地址：Base URL 以 `/v1/messages` 结尾就原样用；以 `/v1` 结尾补 `/messages`；
//!   其余（`https://api.anthropic.com` 这种）补 `/v1/messages`。
//! - 头：`x-api-key`、`anthropic-version: 2023-06-01`、`content-type: application/json`。
//!   Key 为空就不发 `x-api-key`。
//! - `max_tokens` 必填，直接用 `max_output_tokens`（调用方保证 > 0）。
//! - `system`：`cache_hint` 时写成带 `cache_control: {"type":"ephemeral"}` 的文本块数组
//!   （断点放在系统提示词末尾，工具定义在它前面，一并被缓存）；否则是普通字符串。
//!   `cache_hint` 时还会在最后一条消息的最后一个块上再打一个断点，让工具循环里每一轮
//!   都能读到前面所有历史的缓存。两个断点，不超过上限 4 个。低于模型的最短缓存长度时接口
//!   自己忽略，不报错。
//! - `messages`：连续的 user / tool 结果合并成一条 `user` 消息，`tool_result` 块排在
//!   文本块**前面**（接口要求紧跟在 `tool_use` 之后的 user 消息以 `tool_result` 开头，
//!   并行调用的所有结果要在同一条消息里）。assistant 消息的块顺序：思考块 → 文本 → `tool_use`。
//! - 思考块回传：必须原样带 `signature`，`redacted_thinking` 原样带 `data`，空文本的思考块
//!   （`display: omitted`）也要带；没有签名的思考块（别家接口产生的）接口会拒绝，所以丢掉。
//! - 思考参数（官方文档的现状）：
//!   - `Budget(n)` → `thinking: {type: "enabled", budget_tokens: n}`，`n` 限制在
//!     `[1024, max_tokens-1]`（文档：下限 1024，必须小于 `max_tokens`）；`max_tokens` ≤ 1024
//!     放不下就不发。**Claude 4.7 及以后的模型不再接受 `enabled`，会 400**，所以只该对
//!     能力表里标「预算式思考」的老模型用 `Budget`。
//!   - `Effort(e)` → `thinking: {type: "adaptive", display: "summarized"}` +
//!     `output_config: {effort: e}`（文档的迁移写法；`effort` 可选
//!     low/medium/high/xhigh/max，各模型支持的子集由 A1 能力识别给出）。`display` 默认
//!     `summarized`：Claude 5 系列默认 `omitted`（不返回思考文本），不显式要就看不到思考
//!     内容；可用 [`AnthropicOptions::thinking_display`] 改掉或设 `None` 不发。
//!   - `Off` → 什么都不发。个别模型（Opus 5.5、Fable 等）思考是强制开的，发 `disabled`
//!     会 400；不发就是模型默认行为。
//!   - 开了思考就不发 `temperature`：文档写明新模型（4.7+/5 系列）任何非默认的
//!     `temperature` / `top_p` / `top_k` 都会 400，老模型开思考时也不支持改温度。
//!
//! # 流
//! 事件：`message_start`（usage）→ 每个块 `content_block_start` / `content_block_delta`* /
//! `content_block_stop` → `message_delta`（`stop_reason`、累计 usage）→ `message_stop`；
//! 中间可有 `ping`；`error` 事件可以出现在 HTTP 200 之后。增量类型：`text_delta`、
//! `thinking_delta`、`signature_delta`（在思考块的 `content_block_stop` 之前）、
//! `input_json_delta`（`partial_json` 片段，块结束时才是完整 JSON）。未知事件 / 块类型
//! （`server_tool_use`、`citations_delta` ……）按文档要求忽略。

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use super::error::ProviderError;
use super::openai_chat::temperature_json;
use super::sse::SseEvent;
use super::types::{
    AssistantMessage, ChatRequest, Message, ReasoningBlock, ReasoningParam, StopReason,
    StreamEvent, Usage,
};

/// 默认的接口版本头。
pub const DEFAULT_VERSION: &str = "2023-06-01";
/// 思考预算的下限（官方文档）。
const MIN_THINKING_BUDGET: u32 = 1024;

/// Anthropic 接口的差异开关。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicOptions {
    /// `anthropic-version` 头。
    pub version: String,
    /// 自适应思考时 `thinking.display` 的取值；`None` = 不发。
    pub thinking_display: Option<String>,
}

impl Default for AnthropicOptions {
    fn default() -> Self {
        Self {
            version: DEFAULT_VERSION.to_string(),
            thinking_display: Some("summarized".to_string()),
        }
    }
}

/// 完整地址。
pub fn endpoint_url(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if base.ends_with("/v1/messages") {
        base.to_string()
    } else if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

fn cache_marker() -> Value {
    json!({"type": "ephemeral"})
}

fn assistant_content(message: &AssistantMessage) -> Vec<Value> {
    let mut blocks = Vec::new();
    for block in &message.reasoning {
        match block {
            ReasoningBlock::Thinking {
                text,
                signature: Some(signature),
            } => blocks.push(json!({
                "type": "thinking",
                "thinking": text,
                "signature": signature,
            })),
            // 没有签名的思考块接口不认，丢掉。
            ReasoningBlock::Thinking {
                signature: None, ..
            } => {}
            ReasoningBlock::Redacted { data } => {
                blocks.push(json!({"type": "redacted_thinking", "data": data}))
            }
        }
    }
    // 接口不接受空文本块。
    if !message.text.is_empty() {
        blocks.push(json!({"type": "text", "text": message.text}));
    }
    for call in &message.tool_calls {
        blocks.push(json!({
            "type": "tool_use",
            "id": call.id,
            "name": call.name,
            "input": call.arguments,
        }));
    }
    blocks
}

fn build_messages(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut at = 0;
    while at < messages.len() {
        match &messages[at] {
            Message::Assistant(assistant) => {
                let content = assistant_content(assistant);
                if !content.is_empty() {
                    out.push(json!({"role": "assistant", "content": content}));
                }
                at += 1;
            }
            _ => {
                // 一串连续的 user / tool 结果 → 一条 user 消息；tool_result 在前。
                let mut results: Vec<Value> = Vec::new();
                let mut texts: Vec<Value> = Vec::new();
                while let Some(message) = messages.get(at) {
                    match message {
                        Message::User { text } => {
                            texts.push(json!({"type": "text", "text": text}));
                        }
                        Message::ToolResult {
                            call_id,
                            content,
                            is_error,
                        } => {
                            let mut block = Map::new();
                            block.insert("type".into(), json!("tool_result"));
                            block.insert("tool_use_id".into(), json!(call_id));
                            block.insert("content".into(), json!(content));
                            if *is_error {
                                block.insert("is_error".into(), json!(true));
                            }
                            results.push(Value::Object(block));
                        }
                        Message::Assistant(_) => break,
                    }
                    at += 1;
                }
                results.extend(texts);
                if !results.is_empty() {
                    out.push(json!({"role": "user", "content": results}));
                }
            }
        }
    }
    out
}

/// 构造请求体。
pub fn build_body(request: &ChatRequest, options: &AnthropicOptions) -> Value {
    let mut messages = build_messages(&request.messages);
    if request.cache_hint {
        // 最后一条消息的最后一个块打断点（最后一条一定是 user 侧：文本或 tool_result，都支持）。
        if let Some(last_block) = messages
            .last_mut()
            .and_then(|message| message.get_mut("content"))
            .and_then(Value::as_array_mut)
            .and_then(|blocks| blocks.last_mut())
            .and_then(Value::as_object_mut)
        {
            last_block.insert("cache_control".into(), cache_marker());
        }
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert("max_tokens".into(), json!(request.max_output_tokens));
    body.insert("stream".into(), json!(true));
    if !request.system.is_empty() {
        if request.cache_hint {
            body.insert(
                "system".into(),
                json!([{
                    "type": "text",
                    "text": request.system,
                    "cache_control": cache_marker(),
                }]),
            );
        } else {
            body.insert("system".into(), json!(request.system));
        }
    }
    body.insert("messages".into(), Value::Array(messages));
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.input_schema,
                })
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }

    let mut thinking_on = false;
    match &request.reasoning {
        ReasoningParam::Budget(budget) if request.max_output_tokens > MIN_THINKING_BUDGET => {
            let budget = (*budget).clamp(MIN_THINKING_BUDGET, request.max_output_tokens - 1);
            body.insert(
                "thinking".into(),
                json!({"type": "enabled", "budget_tokens": budget}),
            );
            thinking_on = true;
        }
        ReasoningParam::Effort(effort) => {
            let mut thinking = Map::new();
            thinking.insert("type".into(), json!("adaptive"));
            if let Some(display) = &options.thinking_display {
                thinking.insert("display".into(), json!(display));
            }
            body.insert("thinking".into(), Value::Object(thinking));
            body.insert("output_config".into(), json!({"effort": effort}));
            thinking_on = true;
        }
        _ => {}
    }
    if let (Some(temperature), false) = (request.temperature, thinking_on) {
        body.insert("temperature".into(), temperature_json(temperature));
    }
    Value::Object(body)
}

#[derive(Debug)]
enum Block {
    Text,
    Thinking,
    ToolUse {
        id: String,
    },
    /// 不认识的块（`server_tool_use` 之类）：它的增量一并忽略。
    Ignored,
}

/// Anthropic SSE 流的解码器。
#[derive(Debug, Default)]
pub struct AnthropicDecoder {
    blocks: HashMap<u64, Block>,
    usage: Usage,
    stopped: bool,
    message_stopped: bool,
    errored: bool,
}

fn merge_usage(usage: &mut Usage, source: &Value) {
    let get = |key: &str| source.get(key).and_then(Value::as_u64);
    if let Some(v) = get("input_tokens") {
        usage.input = v;
    }
    if let Some(v) = get("output_tokens") {
        usage.output = v;
    }
    if let Some(v) = get("cache_read_input_tokens") {
        usage.cache_read = v;
    }
    if let Some(v) = get("cache_creation_input_tokens") {
        usage.cache_write = v;
    }
}

fn map_stop(reason: &str) -> StopReason {
    match reason {
        "end_turn" | "stop_sequence" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "refusal" => StopReason::ContentFilter,
        other => StopReason::Other(other.to_string()),
    }
}

impl AnthropicDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// `message_stop` 到了（或出错了），后面不用再读。
    pub fn is_done(&self) -> bool {
        self.message_stopped || self.errored
    }

    pub fn push(&mut self, event: &SseEvent) -> Vec<StreamEvent> {
        let Ok(value) = serde_json::from_str::<Value>(event.data.trim()) else {
            return Vec::new();
        };
        // 事件类型以数据里的 `type` 为准，缺了才看 SSE 的 `event:` 行。
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .or(event.event.as_deref())
            .unwrap_or("");
        let mut out = Vec::new();
        match kind {
            "message_start" => {
                if let Some(usage) = value.pointer("/message/usage").filter(|u| u.is_object()) {
                    merge_usage(&mut self.usage, usage);
                    out.push(StreamEvent::usage(self.usage));
                }
            }
            "content_block_start" => self.on_block_start(&value, &mut out),
            "content_block_delta" => self.on_block_delta(&value, &mut out),
            "content_block_stop" => {
                let index = value.get("index").and_then(Value::as_u64);
                if let Some(Block::ToolUse { id }) = index.and_then(|i| self.blocks.remove(&i)) {
                    out.push(StreamEvent::ToolCallEnd { id });
                }
            }
            "message_delta" => {
                if let Some(usage) = value.get("usage").filter(|u| u.is_object()) {
                    merge_usage(&mut self.usage, usage);
                    out.push(StreamEvent::usage(self.usage));
                }
                if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stopped = true;
                    out.push(StreamEvent::Stop {
                        reason: map_stop(reason),
                    });
                }
            }
            "message_stop" => self.message_stopped = true,
            "error" => {
                let error = value.get("error");
                let kind = error
                    .and_then(|e| e.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let message = error
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                self.errored = true;
                out.push(StreamEvent::Error {
                    error: ProviderError::from_stream_error(kind, message),
                });
            }
            // ping 和将来新增的事件类型。
            _ => {}
        }
        out
    }

    pub fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        if self.stopped || self.errored {
            return Ok(Vec::new());
        }
        Err(ProviderError::interrupted(
            "流在 message_delta / message_stop 之前就关了",
        ))
    }

    fn on_block_start(&mut self, value: &Value, out: &mut Vec<StreamEvent>) {
        let Some(index) = value.get("index").and_then(Value::as_u64) else {
            return;
        };
        let block = value.get("content_block").unwrap_or(&Value::Null);
        let text_of = |key: &str| block.get(key).and_then(Value::as_str).unwrap_or("");
        match block.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                self.blocks.insert(index, Block::Text);
                if !text_of("text").is_empty() {
                    out.push(StreamEvent::TextDelta {
                        text: text_of("text").to_string(),
                    });
                }
            }
            "thinking" => {
                self.blocks.insert(index, Block::Thinking);
                if !text_of("thinking").is_empty() {
                    out.push(StreamEvent::ReasoningDelta {
                        text: text_of("thinking").to_string(),
                    });
                }
                if !text_of("signature").is_empty() {
                    out.push(StreamEvent::ReasoningSignature {
                        signature: text_of("signature").to_string(),
                    });
                }
            }
            "redacted_thinking" => {
                self.blocks.insert(index, Block::Ignored);
                out.push(StreamEvent::RedactedReasoning {
                    data: text_of("data").to_string(),
                });
            }
            "tool_use" => {
                let id = text_of("id").to_string();
                let name = text_of("name").to_string();
                self.blocks.insert(index, Block::ToolUse { id: id.clone() });
                out.push(StreamEvent::ToolCallStart { id, name });
            }
            _ => {
                self.blocks.insert(index, Block::Ignored);
            }
        }
    }

    fn on_block_delta(&mut self, value: &Value, out: &mut Vec<StreamEvent>) {
        let Some(index) = value.get("index").and_then(Value::as_u64) else {
            return;
        };
        let delta = value.get("delta").unwrap_or(&Value::Null);
        let field = |key: &str| delta.get(key).and_then(Value::as_str).unwrap_or("");
        match delta.get("type").and_then(Value::as_str).unwrap_or("") {
            "text_delta" if matches!(self.blocks.get(&index), Some(Block::Text)) => {
                if !field("text").is_empty() {
                    out.push(StreamEvent::TextDelta {
                        text: field("text").to_string(),
                    });
                }
            }
            "thinking_delta" if matches!(self.blocks.get(&index), Some(Block::Thinking)) => {
                if !field("thinking").is_empty() {
                    out.push(StreamEvent::ReasoningDelta {
                        text: field("thinking").to_string(),
                    });
                }
            }
            "signature_delta" if matches!(self.blocks.get(&index), Some(Block::Thinking)) => {
                out.push(StreamEvent::ReasoningSignature {
                    signature: field("signature").to_string(),
                });
            }
            "input_json_delta" => {
                if let Some(Block::ToolUse { id }) = self.blocks.get(&index) {
                    if !field("partial_json").is_empty() {
                        out.push(StreamEvent::ToolCallArgsDelta {
                            id: id.clone(),
                            json_fragment: field("partial_json").to_string(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::agent::builtin::provider::types::{ToolCall, ToolSpec};

    fn request() -> ChatRequest {
        ChatRequest {
            model: "claude-test".into(),
            system: "你是助手".into(),
            messages: vec![Message::User {
                text: "查看磁盘".into(),
            }],
            tools: vec![],
            max_output_tokens: 8192,
            reasoning: ReasoningParam::Off,
            temperature: None,
            cache_hint: false,
        }
    }

    fn tool() -> ToolSpec {
        ToolSpec {
            name: "run_command".into(),
            description: "run a command".into(),
            input_schema: json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}),
        }
    }

    #[test]
    fn minimal_body_snapshot() {
        let body = build_body(&request(), &AnthropicOptions::default());
        assert_eq!(
            body,
            json!({
                "model": "claude-test",
                "max_tokens": 8192,
                "stream": true,
                "system": "你是助手",
                "messages": [{"role": "user", "content": [{"type": "text", "text": "查看磁盘"}]}],
            })
        );
    }

    #[test]
    fn cache_hint_marks_system_and_last_block() {
        let mut req = request();
        req.cache_hint = true;
        req.tools = vec![tool()];
        let body = build_body(&req, &AnthropicOptions::default());
        assert_eq!(
            body["system"],
            json!([{"type": "text", "text": "你是助手", "cache_control": {"type": "ephemeral"}}])
        );
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        // 工具定义本身不单独打断点（系统提示词的断点已经覆盖了它前面的工具）。
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(body["tools"][0]["input_schema"], tool().input_schema);
    }

    #[test]
    fn tool_round_trip_snapshot_with_thinking_blocks() {
        let mut req = request();
        req.tools = vec![tool()];
        req.messages = vec![
            Message::User {
                text: "看下磁盘".into(),
            },
            Message::Assistant(AssistantMessage {
                text: "我来查".into(),
                reasoning: vec![
                    ReasoningBlock::Thinking {
                        text: "先 df".into(),
                        signature: Some("sigA".into()),
                    },
                    ReasoningBlock::Thinking {
                        text: "无签名".into(),
                        signature: None,
                    },
                    ReasoningBlock::Redacted { data: "enc".into() },
                ],
                tool_calls: vec![
                    ToolCall {
                        id: "toolu_1".into(),
                        name: "run_command".into(),
                        arguments: json!({"command":"df -h"}),
                    },
                    ToolCall {
                        id: "toolu_2".into(),
                        name: "run_command".into(),
                        arguments: json!({"command":"free"}),
                    },
                ],
            }),
            Message::ToolResult {
                call_id: "toolu_1".into(),
                content: "ok".into(),
                is_error: false,
            },
            Message::ToolResult {
                call_id: "toolu_2".into(),
                content: "boom".into(),
                is_error: true,
            },
            Message::User {
                text: "继续".into(),
            },
        ];
        let body = build_body(&req, &AnthropicOptions::default());
        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": [{"type": "text", "text": "看下磁盘"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "先 df", "signature": "sigA"},
                    {"type": "redacted_thinking", "data": "enc"},
                    {"type": "text", "text": "我来查"},
                    {"type": "tool_use", "id": "toolu_1", "name": "run_command", "input": {"command": "df -h"}},
                    {"type": "tool_use", "id": "toolu_2", "name": "run_command", "input": {"command": "free"}},
                ]},
                // 并行调用的两个结果在同一条 user 消息里，tool_result 在文本前面。
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"},
                    {"type": "tool_result", "tool_use_id": "toolu_2", "content": "boom", "is_error": true},
                    {"type": "text", "text": "继续"},
                ]},
            ])
        );
    }

    #[test]
    fn empty_thinking_text_with_signature_is_still_sent() {
        let mut req = request();
        req.messages.push(Message::Assistant(AssistantMessage {
            text: String::new(),
            reasoning: vec![ReasoningBlock::Thinking {
                text: String::new(),
                signature: Some("s".into()),
            }],
            tool_calls: vec![ToolCall {
                id: "t".into(),
                name: "stat".into(),
                arguments: json!({}),
            }],
        }));
        let body = build_body(&req, &AnthropicOptions::default());
        let blocks = &body["messages"][1]["content"];
        assert_eq!(
            blocks[0],
            json!({"type": "thinking", "thinking": "", "signature": "s"})
        );
        assert_eq!(blocks[1]["type"], "tool_use");
        // 没有空文本块。
        assert_eq!(blocks.as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn budget_maps_to_enabled_thinking_and_is_clamped() {
        let mut req = request();
        req.temperature = Some(0.5);
        req.reasoning = ReasoningParam::Budget(4000);
        let body = build_body(&req, &AnthropicOptions::default());
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 4000})
        );
        assert!(body.get("temperature").is_none(), "开思考不发 temperature");
        assert!(body.get("output_config").is_none());

        req.reasoning = ReasoningParam::Budget(100);
        assert_eq!(
            build_body(&req, &AnthropicOptions::default())["thinking"]["budget_tokens"],
            1024
        );
        req.reasoning = ReasoningParam::Budget(50_000);
        assert_eq!(
            build_body(&req, &AnthropicOptions::default())["thinking"]["budget_tokens"],
            8191
        );
        // 放不下最小预算就不发。
        req.max_output_tokens = 1024;
        assert!(build_body(&req, &AnthropicOptions::default())
            .get("thinking")
            .is_none());
    }

    #[test]
    fn effort_maps_to_adaptive_thinking_and_output_config() {
        let mut req = request();
        req.reasoning = ReasoningParam::Effort("xhigh".into());
        let body = build_body(&req, &AnthropicOptions::default());
        assert_eq!(
            body["thinking"],
            json!({"type": "adaptive", "display": "summarized"})
        );
        assert_eq!(body["output_config"], json!({"effort": "xhigh"}));
        let no_display = AnthropicOptions {
            thinking_display: None,
            ..Default::default()
        };
        assert_eq!(
            build_body(&req, &no_display)["thinking"],
            json!({"type": "adaptive"})
        );
    }

    #[test]
    fn off_sends_nothing_and_keeps_temperature() {
        let mut req = request();
        req.temperature = Some(0.3);
        let body = build_body(&req, &AnthropicOptions::default());
        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
        assert_eq!(body["temperature"], json!(0.3));
    }

    #[test]
    fn empty_system_is_omitted() {
        let mut req = request();
        req.system.clear();
        req.cache_hint = true;
        assert!(build_body(&req, &AnthropicOptions::default())
            .get("system")
            .is_none());
    }

    #[test]
    fn endpoint_rules() {
        assert_eq!(
            endpoint_url("https://api.example.test"),
            "https://api.example.test/v1/messages"
        );
        assert_eq!(
            endpoint_url("https://api.example.test/"),
            "https://api.example.test/v1/messages"
        );
        assert_eq!(
            endpoint_url("https://api.example.test/v1"),
            "https://api.example.test/v1/messages"
        );
        assert_eq!(
            endpoint_url("https://api.example.test/v1/messages"),
            "https://api.example.test/v1/messages"
        );
        assert_eq!(
            endpoint_url("https://gw.test/anthropic"),
            "https://gw.test/anthropic/v1/messages"
        );
    }
}
