//! ZeeTerm 的 Telnet 与串口会话后端。
//!
//! 两种后端都接到 `zssh-session` 的会话抽象：对外是一个 [`SessionBackend`]（写端）加一个
//! `SessionEvent` 接收端和一个 `FlowGate`，和 SSH 会话完全同形，终端标签、分屏、广播、日志、
//! 高亮都不用改。本 crate 不依赖界面，也不碰主机库：怎么拨号（直连 / 代理 / 跳板机）、
//! 用哪台主机的设置，由 `zssh-gpui` 那一层决定。
//!
//! [`SessionBackend`]: zssh_session::SessionBackend

pub mod backend;
pub mod input;
pub mod serial;
pub mod telnet;
pub mod text;

pub use backend::StreamBackend;
