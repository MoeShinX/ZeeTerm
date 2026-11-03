//! provider 无关的请求 / 事件模型。
//!
//! 对话循环（A3）只认这一组类型；两种接口（OpenAI 兼容 Chat Completions、Anthropic
//! Messages）各自在 `openai_chat.rs` / `anthropic.rs` 里翻译成线上格式。

use std::fmt;

use serde_json::Value;

use super::error::ProviderError;

/// 工具定义（名称、描述、参数的 JSON Schema）。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// 参数的 JSON Schema（顶层应是 `{"type":"object", ...}`）。
    pub input_schema: Value,
}

/// 思考参数。各 provider 自己映射，映射不了就不发（见各 provider 文件头的规则）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningParam {
    /// 明确关闭思考：有开关的接口发关闭（DeepSeek 的 `thinking: disabled`），没有开关的
    /// 什么都不发。用户要的是"不思考"才用它；"用户没选"请用 [`ReasoningParam::Unspecified`]。
    Off,
    /// 用户没指定：什么思考参数都不发，包括 DeepSeek 的开关（模型按服务端的默认行为走）。
    Unspecified,
    /// 思考等级，原样交给 provider（`low` / `medium` / `high` / `xhigh` / `max` ……）。
    /// 具体哪些等级可用由能力识别（A1）决定，这里不校验。
    Effort(String),
    /// 思考预算（token）。
    Budget(u32),
}

/// assistant 回复里的一个思考块（回传给接口用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningBlock {
    /// 普通思考块。`signature` 是 Anthropic 的完整性签名：回传时必须原样带上，
    /// 没有签名的块（别家接口来的）Anthropic 会拒绝，所以回传时会丢掉。
    Thinking {
        text: String,
        signature: Option<String>,
    },
    /// Anthropic 的 `redacted_thinking`：不透明的加密数据，回传时必须原样带上。
    Redacted { data: String },
}

/// assistant 发起的一次工具调用。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// 参数（JSON 对象）。
    pub arguments: Value,
}

/// assistant 的一条历史消息。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AssistantMessage {
    pub text: String,
    pub reasoning: Vec<ReasoningBlock>,
    pub tool_calls: Vec<ToolCall>,
}

/// 对话历史里的一条消息。
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    User {
        text: String,
    },
    Assistant(AssistantMessage),
    /// 工具结果。`is_error` 在 Anthropic 里对应 `tool_result.is_error`；OpenAI 的 `tool`
    /// 消息没有这个字段，只发 `content`（出错原因写在文本里，由调用方负责）。
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
}

/// 一次请求。
#[derive(Clone, PartialEq)]
pub struct ChatRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_output_tokens: u32,
    pub reasoning: ReasoningParam,
    pub temperature: Option<f32>,
    /// 提示接口缓存前缀（系统提示词、历史）。Anthropic 会打 `cache_control` 断点；
    /// OpenAI 兼容各家自动缓存，这个开关对它们无效。
    pub cache_hint: bool,
}

/// `Debug` 手写：只给规模，不打印系统提示词、对话和工具结果（日志里不能出现请求正文）。
impl fmt::Debug for ChatRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatRequest")
            .field("model", &self.model)
            .field("system_chars", &self.system.chars().count())
            .field("messages", &self.messages.len())
            .field("tools", &self.tools.len())
            .field("max_output_tokens", &self.max_output_tokens)
            .field("reasoning", &self.reasoning)
            .field("temperature", &self.temperature)
            .field("cache_hint", &self.cache_hint)
            .finish()
    }
}

/// token 用量。
///
/// 口径统一为「三类互不重叠」，总输入 = `input + cache_read + cache_write`：
/// - Anthropic 的 `input_tokens` 本来就不含缓存；
/// - OpenAI 兼容的 `prompt_tokens` 含缓存命中，这里扣掉 `cached_tokens` 后放进 `input`。
///
/// 流里可能出现多次，**后一次是累计值，替换前一次**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Usage {
    /// 这次请求喂给模型的总 token（含缓存读写）。
    pub fn total_input(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }
}

/// 回复结束的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// 正常说完（含命中停止序列）。
    EndTurn,
    /// 模型要调用工具，等结果。
    ToolUse,
    /// 输出到了长度上限被截断。
    MaxTokens,
    /// 被内容过滤 / 模型拒绝回答。
    ContentFilter,
    /// 别的（原文，比如 `pause_turn`、`insufficient_system_resource`）。
    Other(String),
}

/// 流式事件（provider 无关）。
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    /// 当前思考块的签名（Anthropic，签名在块结束前到；到了就意味着这个思考块结束）。
    /// 卡里没列这个事件：多轮工具调用时 Anthropic 要求把思考块连签名一起回传，
    /// 不带出来就没法续。
    ReasoningSignature {
        signature: String,
    },
    /// Anthropic 的 `redacted_thinking` 块（同上，回传要用）。
    RedactedReasoning {
        data: String,
    },
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallArgsDelta {
        id: String,
        json_fragment: String,
    },
    ToolCallEnd {
        id: String,
    },
    Usage {
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
    },
    Stop {
        reason: StopReason,
    },
    Error {
        error: ProviderError,
    },
}

impl StreamEvent {
    pub(crate) fn usage(usage: Usage) -> StreamEvent {
        StreamEvent::Usage {
            input: usage.input,
            output: usage.output,
            cache_read: usage.cache_read,
            cache_write: usage.cache_write,
        }
    }
}

/// 一个回合里 assistant 的一次工具调用（流收完后的样子）。
#[derive(Debug, Clone, PartialEq)]
pub struct AssembledToolCall {
    pub id: String,
    pub name: String,
    /// 解析好的参数；模型给的不是合法 JSON（比如被长度上限截断）时是 `Err(原文)`。
    pub arguments: Result<Value, String>,
}

/// 流收完后拼好的 assistant 回合。
#[derive(Debug, Clone, PartialEq)]
pub struct AssistantTurn {
    pub text: String,
    pub reasoning: Vec<ReasoningBlock>,
    pub tool_calls: Vec<AssembledToolCall>,
    pub usage: Usage,
    pub stop: Option<StopReason>,
}

impl AssistantTurn {
    /// 变成可以放回对话历史的 assistant 消息。参数不是合法 JSON 的调用，历史里记成空对象
    /// `{}`（两种接口都要求参数是 JSON 对象，原文塞进去会让下一次请求直接 400）；
    /// 调用方应当给这次调用回一条「参数不是合法 JSON」的错误结果。
    pub fn to_message(&self) -> AssistantMessage {
        AssistantMessage {
            text: self.text.clone(),
            reasoning: self.reasoning.clone(),
            tool_calls: self
                .tool_calls
                .iter()
                .map(|call| ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call
                        .arguments
                        .clone()
                        .unwrap_or_else(|_| Value::Object(Default::default())),
                })
                .collect(),
        }
    }
}

#[derive(Debug)]
struct PendingCall {
    id: String,
    name: String,
    args: String,
}

/// 把 [`StreamEvent`] 序列拼成 [`AssistantTurn`]。
#[derive(Debug, Default)]
pub struct TurnAccumulator {
    text: String,
    reasoning: Vec<ReasoningBlock>,
    /// 最后一个思考块还没被签名封口，后面的思考增量继续往里追加。
    reasoning_open: bool,
    calls: Vec<PendingCall>,
    usage: Usage,
    stop: Option<StopReason>,
}

impl TurnAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::TextDelta { text } => self.text.push_str(text),
            StreamEvent::ReasoningDelta { text } => {
                if self.reasoning_open {
                    if let Some(ReasoningBlock::Thinking { text: buf, .. }) =
                        self.reasoning.last_mut()
                    {
                        buf.push_str(text);
                        return;
                    }
                }
                self.reasoning.push(ReasoningBlock::Thinking {
                    text: text.clone(),
                    signature: None,
                });
                self.reasoning_open = true;
            }
            StreamEvent::ReasoningSignature { signature } => {
                if self.reasoning_open {
                    if let Some(ReasoningBlock::Thinking { signature: sig, .. }) =
                        self.reasoning.last_mut()
                    {
                        *sig = Some(signature.clone());
                    }
                } else {
                    // `display: omitted` 时思考文本是空的，只有一个签名。
                    self.reasoning.push(ReasoningBlock::Thinking {
                        text: String::new(),
                        signature: Some(signature.clone()),
                    });
                }
                self.reasoning_open = false;
            }
            StreamEvent::RedactedReasoning { data } => {
                self.reasoning
                    .push(ReasoningBlock::Redacted { data: data.clone() });
                self.reasoning_open = false;
            }
            StreamEvent::ToolCallStart { id, name } => self.calls.push(PendingCall {
                id: id.clone(),
                name: name.clone(),
                args: String::new(),
            }),
            StreamEvent::ToolCallArgsDelta { id, json_fragment } => {
                if let Some(call) = self.calls.iter_mut().rev().find(|call| &call.id == id) {
                    call.args.push_str(json_fragment);
                }
            }
            StreamEvent::ToolCallEnd { .. } => {}
            StreamEvent::Usage {
                input,
                output,
                cache_read,
                cache_write,
            } => {
                self.usage = Usage {
                    input: *input,
                    output: *output,
                    cache_read: *cache_read,
                    cache_write: *cache_write,
                }
            }
            StreamEvent::Stop { reason } => self.stop = Some(reason.clone()),
            StreamEvent::Error { .. } => {}
        }
    }

    pub fn finish(self) -> AssistantTurn {
        let tool_calls = self
            .calls
            .into_iter()
            .map(|call| {
                // 没有参数的工具，两家都可能一个片段都不发（空串），按 `{}` 算。
                let raw = call.args.trim();
                let arguments = if raw.is_empty() {
                    Ok(Value::Object(Default::default()))
                } else {
                    match serde_json::from_str::<Value>(raw) {
                        Ok(value @ Value::Object(_)) => Ok(value),
                        _ => Err(call.args.clone()),
                    }
                };
                AssembledToolCall {
                    id: call.id,
                    name: call.name,
                    arguments,
                }
            })
            .collect();
        AssistantTurn {
            text: self.text,
            reasoning: self.reasoning,
            tool_calls,
            usage: self.usage,
            stop: self.stop,
        }
    }
}
