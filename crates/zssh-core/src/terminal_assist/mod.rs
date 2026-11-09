//! 终端输入辅助的纯逻辑部分：判断"现在是不是停在 shell 输入行上、本地输入和远端屏幕是否一致"。
//!
//! 这一层不依赖任何界面框架、不写 SSH：
//! - 输入是三类事实：shell 标记（OSC 133 / 633 / 7，由终端模型识别）、本地发出去的写入（带来源和结果）、
//!   终端屏幕的读数（光标、输入行的单元格）；
//! - 输出是 [`InputTracker`] 的状态（[`InputState`]）和供候选请求核对的 [`RequestIdentity`]。
//!
//! 它**不改变任何按键行为**：Tab、Enter、方向键照原样交给 shell，这里只旁路观察、必要时失同步。
//! 失同步（Desynced）之后绝不补发退格或补偿文本，只等屏幕能唯一确认或下一个 Prompt。
//!
//! 位置的三种单位不能混用，所以各有各的类型：[`ByteIdx`]（输入文本的 UTF-8 字节下标）、
//! [`Utf16Idx`]（输入法的 UTF-16 范围）、[`TermCol`]（终端列号，宽字符占两列）。
//!
//! 候选这一层建在它上面：[`history`]（只驻内存、按目标分域的命令历史）、[`sensitive`]（敏感命令的纯函数规则）、
//! [`suggest`]（严格前缀检索）；窗格的接入点是 [`InputTracker::identity`] / [`InputTracker::is_current`] /
//! [`InputTracker::begin_accept`]，接受只追加后缀、不发回车。

pub mod cells;
pub mod echo;
pub mod history;
pub mod integration;
pub mod sensitive;
pub mod state;
pub mod suggest;
pub mod text_index;
pub mod write;

pub use cells::{read_input_line, CellKind, InputCell, InputLine, ReadError, MAX_TRACKED_BYTES};
pub use echo::{EchoModel, EchoVerdict};
pub use history::{HistoryHit, HistoryStore, Provenance, Recorded, RejectedBecause};
pub use state::{
    AcceptRefusal, DesyncReason, HandoffKey, InputAnchor, InputOrigin, InputState, InputTracker,
    KeyClass, MarkAnchor, MarkKind, Phase, RequestIdentity, RestrictionKind, Restrictions,
    ScreenSource, ScreenView, SubmittedCommand, TrackerContext, TrackerEvent, WriteSource,
};
pub use suggest::{Candidate, CandidateSource, QuickSource, SettleTimer};
pub use text_index::{byte_to_utf16, utf16_to_byte, ByteIdx, TermCol, Utf16Idx};
pub use write::{Disposition, IntentOrigin, RejectReason, WriteIntent, WriteOutcome};

/// 本地产品设置的键（只在本机，不参与同步，见 `sync::settings::SETTINGS_SYNC_CLASS`）。
pub const ENABLED_KEY: &str = "terminal_assist.enabled";
pub const HISTORY_ENABLED_KEY: &str = "terminal_assist.history_enabled";
/// 用户自己绑定的"选择建议"键（按键写法，如 `ctrl-alt-s`；空 = 没绑）。默认不绑定任何键；只在本机，不同步。
pub const SELECT_KEY: &str = "terminal_assist.select_key";
