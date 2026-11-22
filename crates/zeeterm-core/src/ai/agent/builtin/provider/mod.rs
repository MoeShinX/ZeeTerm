//! 内置助手的接口层（V2-A2）：两种接口的请求构造与流式解析。
//!
//! - [`openai_chat`]：OpenAI 兼容 Chat Completions；
//! - [`anthropic`]：Anthropic Messages；
//! - [`stream_chat`]：发请求、读 SSE、解码成 provider 无关的 [`StreamEvent`]；
//! - [`Transport`]：HTTP 层的小接口（真实实现用 `reqwest`，测试用桩回放录好的字节）；
//! - [`ProviderError`]：带中文原因和「是否可重试」的错误分类；
//! - [`bridge_tool_specs`]：从工具桥的工具清单生成 [`ToolSpec`]。
//!
//! 对话循环、重试节奏、上下文预算是 A3 的事；这一层只做「一次请求 → 一串事件」。

pub mod anthropic;
pub mod client;
pub mod error;
pub mod openai_chat;
pub mod sse;
pub mod tools;
pub mod transport;
pub mod types;

pub use anthropic::AnthropicOptions;
pub use client::{
    build_http_request, build_models_request, stream_chat, ApiKey, ProviderConfig, ProviderKind,
};
pub use error::{ErrorKind, ProviderError, TransportError};
pub use openai_chat::{OpenAiOptions, Switch};
pub use tools::bridge_tool_specs;
pub use transport::{ByteStream, HttpRequest, HttpResponse, ReqwestTransport, Transport};
pub use types::{
    AssembledToolCall, AssistantMessage, AssistantTurn, ChatRequest, Message, ReasoningBlock,
    ReasoningParam, StopReason, StreamEvent, ToolCall, ToolSpec, TurnAccumulator, Usage,
};

#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;
