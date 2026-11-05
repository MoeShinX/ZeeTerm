//! X11 转发：SSH 通道里的 X11 连接 → 本机 X 服务器。
//!
//! 分工：
//! - [`display`]：`DISPLAY` 字符串解析成本机 TCP 端点；
//! - [`cookie`]：假 cookie、X11 握手包（setup 包）里的 cookie 替换；
//! - [`registry`]：连接级的分发表（假 cookie → 本机显示端点和真 cookie），连接池共享同一条
//!   SSH 连接时，每个开了 X11 的 shell 各占一项；
//! - [`relay`]：一条 X11 通道的转发任务（读握手包、查表、换 cookie、双向拷贝）；
//! - [`failure`]：失败分类和终端里的一句中文提示；
//! - [`local`]：本机 X 服务器（VcXsrv）的检测、安装、自动启动和设置项。

pub mod cookie;
pub mod display;
pub mod failure;
pub mod local;
pub mod registry;
pub(crate) mod relay;

pub use cookie::MIT_MAGIC_COOKIE;
pub use display::{parse_display, XEndpoint};
pub use failure::{set_notice_renderer, X11Failure};
pub use registry::{
    FailureSink, X11Hub, X11Registration, X11Request, X11Route, MAX_CHANNELS_PER_REGISTRATION,
    MAX_CHANNELS_TOTAL, MAX_REGISTRATIONS,
};

/// 一个 shell 请求 X11 转发的结果，给界面显示用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum X11Outcome {
    /// 这个 shell 没有要求 X11。
    NotRequested,
    /// 服务器同意了，转发已登记；`display` 是本机 X 服务器的地址（`主机:端口`）。
    Active { display: String },
    /// 没有开起来，shell 照常以普通终端打开；原因同时已经写进终端。
    Skipped(X11Failure),
}
