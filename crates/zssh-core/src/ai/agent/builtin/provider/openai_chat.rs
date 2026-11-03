//! OpenAI 兼容 Chat Completions（`POST {base}/chat/completions`）：请求体构造 + SSE 流解析。
//!
//! 兼容这一族的服务很多（OpenAI 本家、DeepSeek、通义、Kimi、智谱、硅基流动、OpenRouter、
//! Ollama、vLLM、New API 一类网关），各家在细节上有差异，下面逐条写明依据（2026-11-03
//! 查的官方文档，链接汇总在报告里）：
//!
//! # 请求
//! - 地址：`{base_url}/chat/completions`，Base URL 由用户填，**已带版本段**（`/v1`、
//!   智谱的 `/api/paas/v4` ……），这里不补 `/v1`；已经以 `/chat/completions` 结尾就不再追加。
//! - 鉴权：`Authorization: Bearer <key>`；Key 为空（Ollama 之类本地服务）就不发这个头。
//! - `stream: true` + `stream_options.include_usage: true`（OpenAI 文档：开了之后在
//!   `[DONE]` 前多一个 `choices` 为空、带 `usage` 的块；DeepSeek 文档：用量在最后一个内容块）。
//!   个别老服务不认 `stream_options`，可以用 [`OpenAiOptions::include_usage`] 关掉。
//! - 输出上限字段：OpenAI 官方文档里 `max_tokens` 已废弃，且 o 系列 / gpt-5 之后的模型
//!   不接受它，要用 `max_completion_tokens`；而 DeepSeek 等兼容服务只认 `max_tokens`。
//!   规则（[`OpenAiOptions::max_completion_tokens`]，默认 `Auto`）：去掉厂商前缀后，名字是
//!   `o<数字>…` 或 `gpt-<主版本>=5…` 就发 `max_completion_tokens`，其余发 `max_tokens`；
//!   起了别名的网关模型认不出来时，在配置里手动切。
//! - 思考：`ReasoningParam::Effort(e)` → `reasoning_effort: e`（OpenAI 文档允许
//!   none/minimal/low/medium/high/xhigh/max；DeepSeek 只认 none/low/high/max，并把
//!   minimal 当 low、medium/xhigh 当 high——等级列表由 A1 的能力识别给出，这里原样透传）。
//!   `Off` / `Budget` 不发 `reasoning_effort`（Chat Completions 没有 token 预算参数）。
//!   DeepSeek 另有开关 `thinking: {"type": "enabled" | "disabled"}`，默认开：
//!   [`OpenAiOptions::thinking_toggle`] 为 `Auto` 时模型名含 `deepseek` 才发——`Off` 发
//!   `disabled`，`Effort` / `Budget` 发 `enabled`；其它服务一律不发这个字段（不认识的字段，
//!   严格的服务会 400）。
//!
//! # 多轮：assistant 历史里的 `reasoning_content`
//! 任务卡写的是「DeepSeek 多轮时 assistant 历史消息不能带回 `reasoning_content`」。
//! **官方文档现在的说法相反**（<https://api-docs.deepseek.com/guides/thinking_mode>，
//! 2026-11-03 核对）：
//! - 没有工具调用的对话：上一轮的 `reasoning_content` 不用回传，传了会被忽略；
//! - 一旦涉及工具调用：同一用户回合里的每次工具子请求都**必须**把该回合产生的
//!   `reasoning_content` 回传，文档原话是「必须完整传回 API，包括没有工具调用的回合」，
//!   漏传 API 返回 400。
//!
//! 所以这里对 DeepSeek 的做法是：assistant 历史消息里只要有思考文本就带上
//! `reasoning_content`（没有工具调用的回合文档说会被忽略，带着无害；有工具调用的回合不带
//! 会 400）。其它服务默认不带（OpenAI 本家等不认识这个字段）。由
//! [`OpenAiOptions::echo_reasoning_content`] 控制：`Auto` = 模型名含 `deepseek` 才带；
//! Kimi / 智谱等要求回传思考的服务，没在官方文档里逐一核实，需要时在配置里设成 `On`。
//!
//! # 流
//! - 思考字段：`delta.reasoning_content`（DeepSeek、通义、硅基等），`delta.reasoning`
//!   （OpenRouter）；两者都有时取前者。
//! - `delta.tool_calls[]` 按 `index` 增量拼接：`id` / `function.name` 只在首片出现，
//!   `function.arguments` 是分片字符串。个别服务（Gemini 兼容层等）会在同一个 `index`
//!   上连发不同 `id` 的完整调用，这里遇到「同 index、`id` 不同」就当作新的一次调用。
//! - 结束：`finish_reason` 一到就收尾所有工具调用，但 `Stop` 事件要等到 `[DONE]` 或流结束
//!   才发，保证 `Usage`（在 `finish_reason` 之后到）排在 `Stop` 前面。
//! - 个别服务在 `finish_reason: "stop"` 的同时带了工具调用：按 `ToolUse` 报。
//! - 没有 `finish_reason` 也没有 `[DONE]` 就断了：报流中断（可重试）。

use serde_json::{json, Map, Value};

use super::error::ProviderError;
use super::sse::SseEvent;
use super::types::{
    AssistantMessage, ChatRequest, Message, ReasoningBlock, ReasoningParam, StopReason,
    StreamEvent, Usage,
};

/// 三态开关：自动（按模型名判断）/ 强制开 / 强制关。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Switch {
    #[default]
    Auto,
    On,
    Off,
}

impl Switch {
    fn resolve(self, auto: bool) -> bool {
        match self {
            Switch::Auto => auto,
            Switch::On => true,
            Switch::Off => false,
        }
    }
}

/// OpenAI 兼容接口的差异开关。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiOptions {
    /// 输出上限用 `max_completion_tokens`（On）还是 `max_tokens`（Off）；Auto 看模型名。
    pub max_completion_tokens: Switch,
    /// assistant 历史里带不带 `reasoning_content`；Auto = 模型名含 `deepseek`。
    pub echo_reasoning_content: Switch,
    /// 发不发 DeepSeek 风格的 `thinking: {type}` 开关；Auto = 模型名含 `deepseek`。
    pub thinking_toggle: Switch,
    /// 发 `stream_options.include_usage`。
    pub include_usage: bool,
}

impl Default for OpenAiOptions {
    fn default() -> Self {
        Self {
            max_completion_tokens: Switch::Auto,
            echo_reasoning_content: Switch::Auto,
            thinking_toggle: Switch::Auto,
            include_usage: true,
        }
    }
}

/// 模型名去掉厂商前缀（`openai/gpt-5` → `gpt-5`）后转小写。
fn base_name(model: &str) -> String {
    model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .trim()
        .to_lowercase()
}

fn is_deepseek(model: &str) -> bool {
    model.to_lowercase().contains("deepseek")
}

/// `o1` / `o3-mini` / `o4-mini` 一类，或 `gpt-5` 及以后。
fn wants_max_completion_tokens(model: &str) -> bool {
    let name = base_name(model);
    let o_series = name
        .strip_prefix('o')
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| c.is_ascii_digit());
    let gpt_major = name.strip_prefix("gpt-").map(|rest| {
        rest.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u32>()
            .unwrap_or(0)
    });
    o_series || gpt_major.is_some_and(|major| major >= 5)
}

/// 完整地址。
pub fn endpoint_url(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if base.ends_with("/chat/completions") {
        base.to_string()
    } else {
        format!("{base}/chat/completions")
    }
}

/// 温度四舍五入到三位小数：`f32` 直接转 JSON 会变成 `0.699999988…`。
pub(crate) fn temperature_json(value: f32) -> Value {
    json!((f64::from(value) * 1000.0).round() / 1000.0)
}

fn assistant_json(message: &AssistantMessage, echo_reasoning: bool) -> Option<Value> {
    let reasoning: String = message
        .reasoning
        .iter()
        .filter_map(|block| match block {
            ReasoningBlock::Thinking { text, .. } => Some(text.as_str()),
            ReasoningBlock::Redacted { .. } => None,
        })
        .collect();
    let has_reasoning = echo_reasoning && !reasoning.is_empty();
    if message.text.is_empty() && message.tool_calls.is_empty() && !has_reasoning {
        return None;
    }
    let mut object = Map::new();
    object.insert("role".into(), json!("assistant"));
    // 只有工具调用没有正文时 content 给 null（规范允许；有些服务拒绝空串 + tool_calls）。
    object.insert(
        "content".into(),
        if message.text.is_empty() && !message.tool_calls.is_empty() {
            Value::Null
        } else {
            json!(message.text)
        },
    );
    if has_reasoning {
        object.insert("reasoning_content".into(), json!(reasoning));
    }
    if !message.tool_calls.is_empty() {
        let calls: Vec<Value> = message
            .tool_calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "type": "function",
                    "function": {
                        "name": call.name,
                        // OpenAI 要求 arguments 是 JSON 字符串，不是对象。
                        "arguments": call.arguments.to_string(),
                    }
                })
            })
            .collect();
        object.insert("tool_calls".into(), Value::Array(calls));
    }
    Some(Value::Object(object))
}

/// 构造请求体。
pub fn build_body(request: &ChatRequest, options: &OpenAiOptions) -> Value {
    let deepseek = is_deepseek(&request.model);
    let echo_reasoning = options.echo_reasoning_content.resolve(deepseek);
    let thinking_toggle = options.thinking_toggle.resolve(deepseek);

    let mut messages: Vec<Value> = Vec::new();
    if !request.system.is_empty() {
        messages.push(json!({"role": "system", "content": request.system}));
    }
    for message in &request.messages {
        match message {
            Message::User { text } => messages.push(json!({"role": "user", "content": text})),
            Message::Assistant(assistant) => {
                if let Some(value) = assistant_json(assistant, echo_reasoning) {
                    messages.push(value);
                }
            }
            Message::ToolResult {
                call_id, content, ..
            } => messages.push(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": content,
            })),
        }
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert("messages".into(), Value::Array(messages));
    body.insert("stream".into(), json!(true));
    if options.include_usage {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if request.max_output_tokens > 0 {
        let field = if options
            .max_completion_tokens
            .resolve(wants_max_completion_tokens(&request.model))
        {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        body.insert(field.into(), json!(request.max_output_tokens));
    }
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                    }
                })
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    match &request.reasoning {
        ReasoningParam::Effort(effort) => {
            if thinking_toggle {
                body.insert("thinking".into(), json!({"type": "enabled"}));
            }
            body.insert("reasoning_effort".into(), json!(effort));
        }
        ReasoningParam::Budget(_) => {
            if thinking_toggle {
                body.insert("thinking".into(), json!({"type": "enabled"}));
            }
        }
        ReasoningParam::Off => {
            if thinking_toggle {
                body.insert("thinking".into(), json!({"type": "disabled"}));
            }
        }
        ReasoningParam::Unspecified => {}
    }
    if let Some(temperature) = request.temperature {
        body.insert("temperature".into(), temperature_json(temperature));
    }
    Value::Object(body)
}

/// 一次工具调用的拼接槽。
#[derive(Debug)]
struct Slot {
    index: Option<i64>,
    id: String,
    /// id 是我们补的（服务端没给）。
    id_synthesized: bool,
    name: String,
    started: bool,
    ended: bool,
    pending_args: String,
}

/// OpenAI 兼容 SSE 流的解码器。
#[derive(Debug, Default)]
pub struct OpenAiDecoder {
    slots: Vec<Slot>,
    pending_stop: Option<StopReason>,
    stopped: bool,
    done_seen: bool,
    errored: bool,
    synth_counter: u32,
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn usage_from(value: &Value) -> Usage {
    let num = |v: Option<&Value>| v.and_then(Value::as_u64).unwrap_or(0);
    let prompt = num(value.get("prompt_tokens"));
    let details = value.get("prompt_tokens_details");
    let cached = {
        let standard = num(details.and_then(|d| d.get("cached_tokens")));
        if standard > 0 {
            standard
        } else {
            // DeepSeek 的字段名。
            num(value.get("prompt_cache_hit_tokens"))
        }
    };
    let cache_write = num(details.and_then(|d| d.get("cache_write_tokens")));
    Usage {
        input: prompt.saturating_sub(cached).saturating_sub(cache_write),
        output: num(value.get("completion_tokens")),
        cache_read: cached,
        cache_write,
    }
}

fn map_finish(reason: &str, had_tool_calls: bool) -> StopReason {
    match reason {
        "stop" | "end_turn" | "stop_sequence" => {
            if had_tool_calls {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            }
        }
        "tool_calls" | "function_call" | "tool_use" => StopReason::ToolUse,
        "length" | "max_tokens" => StopReason::MaxTokens,
        "content_filter" | "refusal" => StopReason::ContentFilter,
        other => StopReason::Other(other.to_string()),
    }
}

impl OpenAiDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 解码器认为流已经正常收完（`[DONE]` 之后）。
    pub fn is_done(&self) -> bool {
        self.done_seen || self.errored
    }

    pub fn push(&mut self, event: &SseEvent) -> Vec<StreamEvent> {
        let data = event.data.trim();
        if data.is_empty() {
            return Vec::new();
        }
        if data == "[DONE]" {
            self.done_seen = true;
            return self.flush();
        }
        // 不是 JSON 的行（个别服务夹带的状态文本）直接跳过；流是否完整由结尾检查把关。
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            return Vec::new();
        };
        let mut out = Vec::new();

        if let Some(error) = chunk.get("error").filter(|e| !e.is_null()) {
            let (kind, message) = match error {
                Value::Object(_) => {
                    let kind = str_field(error, "type")
                        .or_else(|| str_field(error, "code"))
                        .unwrap_or("");
                    (
                        kind.to_string(),
                        str_field(error, "message").unwrap_or("").to_string(),
                    )
                }
                Value::String(text) => (String::new(), text.clone()),
                _ => (String::new(), String::new()),
            };
            self.errored = true;
            out.push(StreamEvent::Error {
                error: ProviderError::from_stream_error(&kind, &message),
            });
            return out;
        }

        if let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        {
            if let Some(delta) = choice.get("delta") {
                self.on_delta(delta, &mut out);
            }
            if let Some(reason) = str_field(choice, "finish_reason") {
                let had_calls = self.slots.iter().any(|slot| slot.started);
                self.end_open_calls(&mut out);
                if self.pending_stop.is_none() {
                    self.pending_stop = Some(map_finish(reason, had_calls));
                }
            }
        }

        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            out.push(StreamEvent::usage(usage_from(usage)));
        }
        out
    }

    /// 流结束（连接正常关闭）时调用。
    pub fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        if self.stopped || self.errored {
            return Ok(Vec::new());
        }
        if self.pending_stop.is_some() {
            return Ok(self.flush());
        }
        Err(ProviderError::interrupted(
            "流在结束标记（finish_reason / [DONE]）之前就关了",
        ))
    }

    fn on_delta(&mut self, delta: &Value, out: &mut Vec<StreamEvent>) {
        if let Some(text) = str_field(delta, "reasoning_content")
            .filter(|t| !t.is_empty())
            .or_else(|| str_field(delta, "reasoning").filter(|t| !t.is_empty()))
        {
            out.push(StreamEvent::ReasoningDelta {
                text: text.to_string(),
            });
        }
        if let Some(text) = str_field(delta, "content").filter(|t| !t.is_empty()) {
            out.push(StreamEvent::TextDelta {
                text: text.to_string(),
            });
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                self.on_tool_call(call, out);
            }
        }
    }

    fn find_slot(&mut self, index: Option<i64>, id: Option<&str>) -> Option<usize> {
        match index {
            Some(index) => {
                let found = self
                    .slots
                    .iter()
                    .rposition(|slot| slot.index == Some(index) && !slot.ended)?;
                let slot = &self.slots[found];
                match id {
                    // 同一 index 又来了一个不同的 id：新的一次调用。
                    Some(id) if !slot.id_synthesized && slot.id != id => None,
                    _ => Some(found),
                }
            }
            None => match id {
                Some(id) => self
                    .slots
                    .iter()
                    .rposition(|slot| slot.id == id && !slot.ended),
                None => self.slots.iter().rposition(|slot| !slot.ended),
            },
        }
    }

    fn on_tool_call(&mut self, call: &Value, out: &mut Vec<StreamEvent>) {
        let index = call.get("index").and_then(Value::as_i64);
        let id = str_field(call, "id").filter(|id| !id.is_empty());
        let function = call.get("function");
        let name = function
            .and_then(|f| str_field(f, "name"))
            .filter(|n| !n.is_empty());
        let args = match function.and_then(|f| f.get("arguments")) {
            Some(Value::String(text)) => text.clone(),
            // 个别服务直接给对象。
            Some(value @ Value::Object(_)) => value.to_string(),
            _ => String::new(),
        };

        let slot_at = match self.find_slot(index, id) {
            Some(at) => at,
            None => {
                // 新调用：前一个同 index 的调用到此结束。
                if let Some(index) = index {
                    for slot in self.slots.iter_mut() {
                        if slot.index == Some(index) && !slot.ended {
                            slot.ended = true;
                            if slot.started {
                                out.push(StreamEvent::ToolCallEnd {
                                    id: slot.id.clone(),
                                });
                            }
                        }
                    }
                }
                let (id, synthesized) = match id {
                    Some(id) => (id.to_string(), false),
                    None => {
                        self.synth_counter += 1;
                        (format!("call_{}", self.synth_counter), true)
                    }
                };
                self.slots.push(Slot {
                    index,
                    id,
                    id_synthesized: synthesized,
                    name: String::new(),
                    started: false,
                    ended: false,
                    pending_args: String::new(),
                });
                self.slots.len() - 1
            }
        };

        let slot = &mut self.slots[slot_at];
        if !slot.started {
            if let Some(id) = id {
                if slot.id_synthesized {
                    slot.id = id.to_string();
                    slot.id_synthesized = false;
                }
            }
            if slot.name.is_empty() {
                if let Some(name) = name {
                    slot.name = name.to_string();
                }
            }
        }
        // 个别服务每片都重复带 name，只认第一次。
        if !slot.started && !slot.name.is_empty() {
            slot.started = true;
            out.push(StreamEvent::ToolCallStart {
                id: slot.id.clone(),
                name: slot.name.clone(),
            });
            if !slot.pending_args.is_empty() {
                out.push(StreamEvent::ToolCallArgsDelta {
                    id: slot.id.clone(),
                    json_fragment: std::mem::take(&mut slot.pending_args),
                });
            }
        }
        if !args.is_empty() {
            if slot.started {
                out.push(StreamEvent::ToolCallArgsDelta {
                    id: slot.id.clone(),
                    json_fragment: args,
                });
            } else {
                slot.pending_args.push_str(&args);
            }
        }
    }

    fn end_open_calls(&mut self, out: &mut Vec<StreamEvent>) {
        for slot in self.slots.iter_mut().filter(|slot| !slot.ended) {
            slot.ended = true;
            if slot.started {
                out.push(StreamEvent::ToolCallEnd {
                    id: slot.id.clone(),
                });
            }
        }
    }

    /// 收尾：补上还没结束的工具调用，并发 `Stop`。
    fn flush(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if self.stopped {
            return out;
        }
        let had_calls = self.slots.iter().any(|slot| slot.started);
        self.end_open_calls(&mut out);
        // 有 `[DONE]` 却没有 finish_reason：按有没有工具调用推断。
        let reason = self.pending_stop.take().unwrap_or(if had_calls {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        });
        self.stopped = true;
        out.push(StreamEvent::Stop { reason });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::agent::builtin::provider::types::{ToolCall, ToolSpec};

    fn request() -> ChatRequest {
        ChatRequest {
            model: "some-model".into(),
            system: "你是助手".into(),
            messages: vec![Message::User {
                text: "查看磁盘".into(),
            }],
            tools: vec![],
            max_output_tokens: 4096,
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
        let body = build_body(&request(), &OpenAiOptions::default());
        assert_eq!(
            body,
            json!({
                "model": "some-model",
                "messages": [
                    {"role": "system", "content": "你是助手"},
                    {"role": "user", "content": "查看磁盘"},
                ],
                "stream": true,
                "stream_options": {"include_usage": true},
                "max_tokens": 4096,
            })
        );
    }

    #[test]
    fn empty_system_and_zero_max_tokens_are_omitted() {
        let mut req = request();
        req.system.clear();
        req.max_output_tokens = 0;
        let body = build_body(
            &req,
            &OpenAiOptions {
                include_usage: false,
                ..Default::default()
            },
        );
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(1));
        assert!(body.get("max_tokens").is_none());
        assert!(body.get("max_completion_tokens").is_none());
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn full_conversation_with_tool_round_trip_snapshot() {
        let mut req = request();
        req.tools = vec![tool()];
        req.messages = vec![
            Message::User {
                text: "看下磁盘".into(),
            },
            Message::Assistant(AssistantMessage {
                text: String::new(),
                reasoning: vec![],
                tool_calls: vec![
                    ToolCall {
                        id: "call_1".into(),
                        name: "run_command".into(),
                        arguments: json!({"command":"df -h"}),
                    },
                    ToolCall {
                        id: "call_2".into(),
                        name: "run_command".into(),
                        arguments: json!({"command":"free"}),
                    },
                ],
            }),
            Message::ToolResult {
                call_id: "call_1".into(),
                content: "ok 1".into(),
                is_error: false,
            },
            Message::ToolResult {
                call_id: "call_2".into(),
                content: "boom".into(),
                is_error: true,
            },
        ];
        req.temperature = Some(0.7);
        let body = build_body(&req, &OpenAiOptions::default());
        assert_eq!(
            body,
            json!({
                "model": "some-model",
                "messages": [
                    {"role": "system", "content": "你是助手"},
                    {"role": "user", "content": "看下磁盘"},
                    {"role": "assistant", "content": null, "tool_calls": [
                        {"id": "call_1", "type": "function", "function": {"name": "run_command", "arguments": "{\"command\":\"df -h\"}"}},
                        {"id": "call_2", "type": "function", "function": {"name": "run_command", "arguments": "{\"command\":\"free\"}"}},
                    ]},
                    {"role": "tool", "tool_call_id": "call_1", "content": "ok 1"},
                    {"role": "tool", "tool_call_id": "call_2", "content": "boom"},
                ],
                "stream": true,
                "stream_options": {"include_usage": true},
                "max_tokens": 4096,
                "tools": [{"type": "function", "function": {
                    "name": "run_command",
                    "description": "run a command",
                    "parameters": {"type":"object","properties":{"command":{"type":"string"}},"required":["command"]},
                }}],
                "temperature": 0.7,
            })
        );
    }

    #[test]
    fn max_tokens_field_follows_the_model_name() {
        for (model, expect) in [
            ("gpt-4o", "max_tokens"),
            ("gpt-4.1-mini", "max_tokens"),
            ("chatgpt-4o-latest", "max_tokens"),
            ("deepseek-v4-flash", "max_tokens"),
            ("Qwen/Qwen3-235B-A22B", "max_tokens"),
            ("gpt-5", "max_completion_tokens"),
            ("gpt-5.5", "max_completion_tokens"),
            ("openai/gpt-5-mini", "max_completion_tokens"),
            ("o1", "max_completion_tokens"),
            ("o3-mini", "max_completion_tokens"),
            ("openai/o4-mini", "max_completion_tokens"),
            ("ollama-llama3", "max_tokens"),
            ("omni-moderation", "max_tokens"),
        ] {
            let mut req = request();
            req.model = model.into();
            let body = build_body(&req, &OpenAiOptions::default());
            assert!(body.get(expect).is_some(), "{model}: {body}");
            let other = if expect == "max_tokens" {
                "max_completion_tokens"
            } else {
                "max_tokens"
            };
            assert!(body.get(other).is_none(), "{model}");
        }
        // 手动覆盖优先。
        let mut req = request();
        req.model = "gateway-alias".into();
        let body = build_body(
            &req,
            &OpenAiOptions {
                max_completion_tokens: Switch::On,
                ..Default::default()
            },
        );
        assert!(body.get("max_completion_tokens").is_some());
        req.model = "gpt-5".into();
        let body = build_body(
            &req,
            &OpenAiOptions {
                max_completion_tokens: Switch::Off,
                ..Default::default()
            },
        );
        assert!(body.get("max_tokens").is_some());
    }

    #[test]
    fn reasoning_effort_only_for_effort() {
        let mut req = request();
        req.reasoning = ReasoningParam::Effort("high".into());
        assert_eq!(
            build_body(&req, &OpenAiOptions::default())["reasoning_effort"],
            "high"
        );
        for reasoning in [ReasoningParam::Off, ReasoningParam::Budget(2048)] {
            req.reasoning = reasoning;
            let body = build_body(&req, &OpenAiOptions::default());
            assert!(body.get("reasoning_effort").is_none());
            assert!(
                body.get("thinking").is_none(),
                "非 DeepSeek 不发 thinking 开关"
            );
        }
    }

    #[test]
    fn deepseek_thinking_toggle() {
        let mut req = request();
        req.model = "deepseek-v4-flash".into();
        req.reasoning = ReasoningParam::Off;
        let body = build_body(&req, &OpenAiOptions::default());
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
        assert!(body.get("reasoning_effort").is_none());
        req.reasoning = ReasoningParam::Effort("max".into());
        let body = build_body(&req, &OpenAiOptions::default());
        assert_eq!(body["thinking"], json!({"type": "enabled"}));
        assert_eq!(body["reasoning_effort"], "max");
        // 网关改名的模型靠手动开关。
        req.model = "my-gateway-model".into();
        req.reasoning = ReasoningParam::Off;
        assert!(build_body(&req, &OpenAiOptions::default())
            .get("thinking")
            .is_none());
        let body = build_body(
            &req,
            &OpenAiOptions {
                thinking_toggle: Switch::On,
                ..Default::default()
            },
        );
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
    }

    fn history_with_reasoning() -> Vec<Message> {
        vec![
            Message::User { text: "q".into() },
            Message::Assistant(AssistantMessage {
                text: "answer".into(),
                reasoning: vec![
                    ReasoningBlock::Thinking {
                        text: "think ".into(),
                        signature: None,
                    },
                    ReasoningBlock::Thinking {
                        text: "more".into(),
                        signature: Some("sig".into()),
                    },
                    ReasoningBlock::Redacted { data: "xx".into() },
                ],
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "stat".into(),
                    arguments: json!({"path":"/"}),
                }],
            }),
            Message::ToolResult {
                call_id: "c1".into(),
                content: "r".into(),
                is_error: false,
            },
        ]
    }

    #[test]
    fn deepseek_history_echoes_reasoning_content() {
        // 官方文档：涉及工具调用时 reasoning_content 必须传回，漏了会 400。
        let mut req = request();
        req.model = "deepseek-v4-pro".into();
        req.messages = history_with_reasoning();
        let body = build_body(&req, &OpenAiOptions::default());
        let assistant = &body["messages"][2];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["reasoning_content"], "think more");
        assert_eq!(assistant["content"], "answer");
    }

    #[test]
    fn other_vendors_never_see_reasoning_content_by_default() {
        let mut req = request();
        req.model = "gpt-4o".into();
        req.messages = history_with_reasoning();
        let body = build_body(&req, &OpenAiOptions::default());
        assert!(body["messages"][2].get("reasoning_content").is_none());
        // 手动打开（比如 Kimi 一类要求回传的服务）。
        let body = build_body(
            &req,
            &OpenAiOptions {
                echo_reasoning_content: Switch::On,
                ..Default::default()
            },
        );
        assert_eq!(body["messages"][2]["reasoning_content"], "think more");
        // DeepSeek 手动关掉。
        req.model = "deepseek-v4-flash".into();
        let body = build_body(
            &req,
            &OpenAiOptions {
                echo_reasoning_content: Switch::Off,
                ..Default::default()
            },
        );
        assert!(body["messages"][2].get("reasoning_content").is_none());
    }

    #[test]
    fn empty_assistant_message_is_dropped() {
        let mut req = request();
        req.messages
            .push(Message::Assistant(AssistantMessage::default()));
        let body = build_body(&req, &OpenAiOptions::default());
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn temperature_is_rounded() {
        let mut req = request();
        req.temperature = Some(0.7);
        assert_eq!(
            build_body(&req, &OpenAiOptions::default())["temperature"],
            json!(0.7)
        );
    }

    #[test]
    fn endpoint_rules() {
        assert_eq!(
            endpoint_url("https://api.example.test/v1"),
            "https://api.example.test/v1/chat/completions"
        );
        assert_eq!(
            endpoint_url("https://api.example.test/v1/"),
            "https://api.example.test/v1/chat/completions"
        );
        assert_eq!(
            endpoint_url(" https://x.test/api/paas/v4 "),
            "https://x.test/api/paas/v4/chat/completions"
        );
        assert_eq!(
            endpoint_url("https://x.test/v1/chat/completions"),
            "https://x.test/v1/chat/completions"
        );
    }
}
