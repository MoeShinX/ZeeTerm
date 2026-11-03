//! ZeeTerm 内置助手（V2-A0）：自带对话循环，直接对接第三方模型接口。
//!
//! - [`catalog`]（A1）：模型能力识别；
//! - [`provider`]（A2）：两种接口的请求构造与流式解析；
//! - [`config`]（A3）：接口配置的数据类型；
//! - [`BuiltinConversation`]（A3）：对话循环，实现 [`crate::ai::agent::AgentConversation`]，
//!   工具调用进程内走工具桥（[`crate::ai::bridge::Bridge::call_tool`]）；
//! - [`mapping`] / [`context`] / [`prompt`] / [`transcript`]（A3）：能力到请求参数的映射、上下文预算
//!   与压缩、系统提示词、完整对话记录（续接用）。

pub mod catalog;
pub mod config;
pub mod context;
pub mod conversation;
pub mod mapping;
pub mod prompt;
pub mod provider;
pub mod transcript;

pub use conversation::{
    BuiltinConversation, BuiltinNotice, BuiltinParams, BuiltinSignal, BuiltinTuning, FailureInfo,
    NoticeKind, TurnUsage, HISTORY_AGENT,
};
