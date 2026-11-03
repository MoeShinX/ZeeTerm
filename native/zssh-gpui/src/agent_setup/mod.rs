//! 智能体的发现、版本、登录、模型目录和一键下载（G10a）。
//!
//! 面板（`agent_chat_demo`）和设置页（`agents_demo`）读同一份进程级状态，业务判定全部是 core 的，
//! 这里只拼装；对应 0.2.14 的 `src-tauri/src/commands/agent.rs`（发现 / 登录 / 目录 / 下载那几条命令）。

pub mod catalog;
pub mod download;
pub mod fake;
pub mod fake_tools;
mod kind;
pub mod status;

use std::sync::atomic::{AtomicBool, Ordering};

pub use kind::{AgentKind, default_agent};
pub use status::{Availability, InstallState, Snapshot, missing_text};

/// 当前三种智能体的状态；第一次调用会在后台开始检测。
pub fn snapshot() -> Snapshot {
    let store = status::store();
    store.ensure_checked(&crate::settings::data_dir());
    store.snapshot()
}

/// 状态的版本号（任何一项变了就加一）：面板每个节拍比一下，变了才取整份快照。
pub fn version() -> u64 {
    status::store().version()
}

/// AI 面板里点了「去设置」（或版本过低提示里的「下载」）：主窗口下一个节拍打开设置页。
static SETTINGS_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn request_settings() {
    SETTINGS_REQUESTED.store(true, Ordering::SeqCst);
}

/// 取走"想去设置页"的请求（取一次就清掉）。
pub fn take_settings_request() -> bool {
    SETTINGS_REQUESTED.swap(false, Ordering::SeqCst)
}

/// AI 卡片上点了「查看终端」：主窗口下一个节拍切到那个终端所在的标签。
/// （目标编号，执行用的终端编号；终端编号为空就按主机找标签。）
static VIEW_TERMINAL: std::sync::Mutex<Option<(String, Option<String>)>> =
    std::sync::Mutex::new(None);

pub fn request_view_terminal(target_id: String, session_id: Option<String>) {
    *VIEW_TERMINAL.lock().unwrap_or_else(|p| p.into_inner()) = Some((target_id, session_id));
}

/// 取走「查看终端」的请求（取一次就清掉）。
pub fn take_view_terminal_request() -> Option<(String, Option<String>)> {
    VIEW_TERMINAL
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
}

/// AI 面板里点了「重新连接 <主机>」（对话绑定的主机已经没有打开的终端）：主窗口下一个节拍为它开一个标签。
static OPEN_HOST: std::sync::Mutex<Option<i64>> = std::sync::Mutex::new(None);

pub fn request_open_host(host_id: i64) {
    *OPEN_HOST.lock().unwrap_or_else(|p| p.into_inner()) = Some(host_id);
}

/// 取走「重新连接」的请求（取一次就清掉）。
pub fn take_open_host_request() -> Option<i64> {
    OPEN_HOST.lock().unwrap_or_else(|p| p.into_inner()).take()
}

/// 重新检测一个智能体（设置页「重新检测」、改了路径、下载完）。
pub fn refresh(kind: AgentKind) {
    status::store().refresh(kind, &crate::settings::data_dir());
}
