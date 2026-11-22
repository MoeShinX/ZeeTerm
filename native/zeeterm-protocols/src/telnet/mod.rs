//! Telnet 会话后端。
//!
//! - [`codec`]：IAC 状态机（跨读取保存状态）和各种帧的构造。
//! - [`negotiate`]：选项协商（ECHO / SGA / BINARY / NAWS / TTYPE）。
//! - [`autologin`]：自动登录（识别用户名 / 密码提示）。
//! - [`session`]：把上面几块接到一个字节流和 `SessionBackend` 上。
//!
//! 用户键入的处理（回车模式、本地回显）和串口共用，在 [`crate::input`]。

pub mod autologin;
pub mod codec;
pub mod negotiate;
pub mod session;

pub use session::{TelnetParams, spawn_telnet};
