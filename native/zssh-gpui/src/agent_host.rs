//! 原生窗口的智能体对话宿主（G07 起的共享运行时 / 桥接层，G10a 扩成 opencode / Codex / Claude Code 三种）。
//!
//! 一个真实对话由这几样东西拼起来：
//! - [`NativeBridgeHost`]：`zssh-mcp` 调进来的工具在用户的终端里执行 / 读终端 / 走 SFTP / 读写主机记忆，
//!   没有终端时走主机库的 SSH 连接上的 exec 通道；
//! - [`NativeApprover`]：需要确认的命令按 scope 送给各自对话的窗口（`std::sync::mpsc`），答案用 oneshot 回填；
//! - [`NativeMode`] / [`NativeAudit`]：执行模式和审计都读同一个数据目录里的库；
//! - [`AgentHub`] / [`AgentSession`]：全进程一个工具桥 + opencode / codex 各一个共用进程，一个对话 =
//!   一个 scope（Claude Code 另有自己的进程）。三种驱动的进程管理和协议全是 core 的，这里只拼装。
//!
//! 工具桥这一侧（桥、审批、scope、等确认的队列）收在 [`ToolChannel`] 里，不依赖智能体进程，
//! 所以「停止」「目标增减」这些行为可以直接用真实的桥测。
//!
//! 执行宿主 [`NativeBridgeHost`] 和 Tauri 版的 `AppBridgeHost` 同一套语义（V2-G10b）：命令优先在用户的
//! 焦点终端里执行、`read_terminal` 读终端缓冲、停止时向终端发 Ctrl+C、文件工具走 SFTP、长期记忆读写
//! 主机库；没有打开的终端时命令走 `host-<id>` 的后台 exec 通道。终端本身的登记见 [`crate::ai_terminals`]。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};

use zssh_core::ai::agent::builtin::catalog::Catalog;
use zssh_core::ai::agent::builtin::config::AiEndpoint;
use zssh_core::ai::agent::builtin::provider::{ApiKey, Transport};
use zssh_core::ai::agent::builtin::{
    BuiltinConversation, BuiltinParams, BuiltinSignal, BuiltinTuning,
};
use zssh_core::ai::agent::claude::{self, StartOptions};
use zssh_core::ai::agent::codex::{
    self, CodexRuntime, DEFAULT_IDLE_TIMEOUT as CODEX_IDLE_TIMEOUT,
    DEFAULT_STARTUP_TIMEOUT as CODEX_STARTUP_TIMEOUT,
};
use zssh_core::ai::agent::config::{McpServerSpec, build_config_content, read_user_model_config};
use zssh_core::ai::agent::discovery::AgentInstall;
use zssh_core::ai::agent::opencode::{
    AgentRuntime, DEFAULT_IDLE_TIMEOUT as OPENCODE_IDLE_TIMEOUT,
    DEFAULT_STARTUP_TIMEOUT as OPENCODE_STARTUP_TIMEOUT, StartOptions as OpenCodeStartOptions,
    default_config_home,
};
use zssh_core::ai::agent::{AgentConversation, AgentEvent, AgentModelCatalog};
use zssh_core::ai::bridge::terminal_write::{DELIVERED_RESULT_UNKNOWN, deliver_ai_write};
use zssh_core::ai::bridge::{
    ApprovalDecision, ApprovalRequest, Approver, AuditEntry, AuditLog, Bridge, BridgeHost,
    ExecGate, ExecMode, ExecModeProvider, ExecPlacement, ExecTargetReport, ScopeHandle,
    TerminalExecCapture, TerminalExecRequest,
};
use zssh_core::ai::files::{RemoteFs, SftpRemoteFs};
use zssh_core::ai::protocol::{host_target_id, parse_host_target_id};
use zssh_core::ai::shell_kind::ShellKind;
use zssh_core::sftp::SftpChannel;
use zssh_core::ssh::connection::ExecGuard;
use zssh_core::ssh::{BoxFuture, ExecCapture};
use zssh_core::store::Database;
use zssh_session::SessionCommand;

use crate::agent_setup::{AgentKind, fake};
use crate::ai_terminals::{TermCommand, TermEntry, terminals};
use crate::i18n::{Msg, t};

mod reopen;
use reopen::{Reopen, ReopeningConversation};

/// Claude Code 对话的空闲关闭时间：10 分钟（和驱动默认值一致）。
const CLAUDE_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// 本文件（智能体宿主的执行 / 文件 / 记忆接线）自己的界面文案。
///
/// 只放应用自己的固定文案；主机名、终端编号、路径、命令和核心库返回的原文一律原样显示，
/// 不进这张表。带参数的句子保留 `format!`，这里只提供固定片段。只有产品里会到界面 / 模型
/// 的提示才在这里；不可能的内部控制流断言（「共用进程类型不对」之类）保持原样。
#[derive(Clone, Copy, Debug)]
enum AgentHostMsg {
    OpenHostDbFailedPrefix,
    HostTargetOnlyPrefix,
    NoTerminal,
    TerminalBufferLockUnavailable,
    OpenSftpChannelFailedPrefix,
    NoTerminalUseBackground,
    CannotRememberQuickConnect,
    SessionNotConnectedPrefix,
    CurrentExeFailedPrefix,
    NoExeParentDir,
    ToolBridgeBindFailedPrefix,
    ToolBridgeNonblockingFailedPrefix,
    ToolBridgePortFailedPrefix,
    McpBinaryMissingPrefix,
    McpBinaryMissingSuffix,
    OpenSettingsDbFailedPrefix,
    EndpointMissing,
    ReadApiKeyFailedPrefix,
    MissingApiKeyPrefix,
    MissingApiKeySuffix,
    ReopenFailedPrefix,
    ConversationEnded,
}

impl Msg for AgentHostMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::OpenHostDbFailedPrefix => "打开主机库失败：",
            Self::HostTargetOnlyPrefix => "原生窗口只支持主机库里的主机（host-<id>）作为执行目标：",
            Self::NoTerminal => "这台主机现在没有打开的终端标签",
            Self::TerminalBufferLockUnavailable => "终端缓冲锁不可用",
            Self::OpenSftpChannelFailedPrefix => "打开 SFTP 通道失败：",
            Self::NoTerminalUseBackground => "这台主机还没有打开的终端，先走后台通道",
            Self::CannotRememberQuickConnect => "这个目标不在主机库里（快速连接），不能保存记忆",
            Self::SessionNotConnectedPrefix => "会话不存在或者还没连上：",
            Self::CurrentExeFailedPrefix => "拿不到主程序路径：",
            Self::NoExeParentDir => "主程序没有父目录",
            Self::ToolBridgeBindFailedPrefix => "工具桥监听失败：",
            Self::ToolBridgeNonblockingFailedPrefix => "工具桥监听设置非阻塞失败：",
            Self::ToolBridgePortFailedPrefix => "工具桥取端口失败：",
            Self::McpBinaryMissingPrefix => {
                // 文件名跟平台走：Windows 带 .exe，Linux 不带。
                if cfg!(windows) {
                    "没有找到 zssh-mcp.exe（应当和主程序放在同一个目录："
                } else {
                    "没有找到 zssh-mcp（应当和主程序放在同一个目录："
                }
            }
            Self::McpBinaryMissingSuffix => "）",
            Self::OpenSettingsDbFailedPrefix => "打开设置库失败：",
            Self::EndpointMissing => "这个接口已经不存在了，请在智能体下拉里重新选一个。",
            Self::ReadApiKeyFailedPrefix => "读取 API Key 失败：",
            Self::MissingApiKeyPrefix => "接口「",
            Self::MissingApiKeySuffix => "」还没有 API Key：到 设置 → AI 助手 → ZeePilot 里补上。",
            Self::ReopenFailedPrefix => "智能体进程已经退出，重新启动失败：",
            Self::ConversationEnded => "这个对话已经结束",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::OpenHostDbFailedPrefix => "Failed to open the host database: ",
            Self::HostTargetOnlyPrefix => {
                "The native window only supports hosts from the host database (host-<id>) as execution targets: "
            }
            Self::NoTerminal => "This host has no open terminal tab right now",
            Self::TerminalBufferLockUnavailable => "The terminal buffer lock is unavailable",
            Self::OpenSftpChannelFailedPrefix => "Failed to open the SFTP channel: ",
            Self::NoTerminalUseBackground => {
                "This host has no open terminal yet; using the background channel"
            }
            Self::CannotRememberQuickConnect => {
                "This target is not in the host database (quick connect), so notes cannot be saved"
            }
            Self::SessionNotConnectedPrefix => {
                "The session does not exist or is not connected yet: "
            }
            Self::CurrentExeFailedPrefix => "Failed to locate the main executable: ",
            Self::NoExeParentDir => "The main executable has no parent directory",
            Self::ToolBridgeBindFailedPrefix => "Failed to bind the tool bridge listener: ",
            Self::ToolBridgeNonblockingFailedPrefix => {
                "Failed to set the tool bridge listener non-blocking: "
            }
            Self::ToolBridgePortFailedPrefix => "Failed to read the tool bridge port: ",
            Self::McpBinaryMissingPrefix => {
                if cfg!(windows) {
                    "zssh-mcp.exe was not found (it should be in the same directory as the main executable: "
                } else {
                    "zssh-mcp was not found (it should be in the same directory as the main executable: "
                }
            }
            Self::McpBinaryMissingSuffix => ")",
            Self::OpenSettingsDbFailedPrefix => "Failed to open the settings database: ",
            Self::EndpointMissing => {
                "This endpoint no longer exists; pick another one in the agent dropdown."
            }
            Self::ReadApiKeyFailedPrefix => "Failed to read the API key: ",
            Self::MissingApiKeyPrefix => "Endpoint \"",
            Self::MissingApiKeySuffix => {
                "\" has no API key yet: add one under Settings → AI Assistant → ZeePilot."
            }
            Self::ReopenFailedPrefix => "The agent process exited and could not be restarted: ",
            Self::ConversationEnded => "This conversation has ended",
        }
    }
}

// ---------------------------------------------------------------------------
// 执行宿主
// ---------------------------------------------------------------------------

/// 工具桥的执行位置回报（`ExecTargetReport`）按 scope 分路送到各自对话的窗口。
///
/// 桥只有一个、宿主只有一个，但对话可能有好几个（每个窗口一个，独立的 AI 窗口也各有自己的）：回报带着 scope id，按
/// [`Self::attach`] 登记的出口送；对话结束（[`Self::detach`]）之后迟到的回报直接丢掉，
/// 不会落进别的对话，也不会落进同一个标签后来开的新对话。
#[derive(Default)]
pub struct ReportRoutes {
    routes: Mutex<HashMap<String, Sender<ExecTargetReport>>>,
}

impl ReportRoutes {
    fn routes(&self) -> std::sync::MutexGuard<'_, HashMap<String, Sender<ExecTargetReport>>> {
        self.routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn attach(&self, scope_id: &str, reports: Sender<ExecTargetReport>) {
        self.routes().insert(scope_id.to_owned(), reports);
    }

    pub fn detach(&self, scope_id: &str) {
        self.routes().remove(scope_id);
    }

    /// 送出一份回报；对话已经没了（出口摘掉 / 窗口关了）就丢掉。
    pub fn send(&self, report: ExecTargetReport) {
        let route = self.routes().get(&report.scope).cloned();
        if let Some(route) = route {
            let _ = route.send(report);
        }
    }
}

/// 工具桥的执行宿主：`host-<id>` 目标 + 用户已经打开的终端（`term-<n>`）。
///
/// 和 0.2.14 的 `AppBridgeHost` 同一套语义：
/// - 目标是 `host-<id>` 时先找这台主机**焦点终端**（[`crate::ai_terminals::Terminals::for_host`]）：
///   有，命令在用户的终端里执行（空闲 + 已确认的 POSIX shell），`read_terminal` 读它的缓冲，Ctrl+C
///   发给它；终端忙 / 不是 POSIX shell / 开着全屏程序时降级到后台 exec 通道并写明原因；
/// - 这台主机没有任何打开的终端：命令走后台 exec 通道（拿主机的已认证连接，池里没有就重新握手），
///   `read_terminal` 报「没有打开的终端」；
/// - 文件工具开一条 SFTP 通道（终端所在的那条连接，没有终端就用主机的池连接）；
/// - 长期记忆读写主机库里的 `host_memory`。
///
/// 和 0.2.14 的差别（报告里有清单）：未连上的主机不会自动新开一个终端标签。
pub struct NativeBridgeHost {
    reports: Arc<ReportRoutes>,
    data_dir: PathBuf,
    /// 测试里代替 SFTP 通道的内存文件系统（离线验证文件工具接线用）。
    #[cfg(test)]
    test_fs: Mutex<Option<Arc<dyn RemoteFs>>>,
}

impl NativeBridgeHost {
    pub fn new() -> Self {
        Self::with(
            Arc::new(ReportRoutes::default()),
            crate::settings::data_dir(),
        )
    }

    /// 指定回报出口表和数据目录（产品里的 [`AgentHub`] 用；测试用临时目录）。
    pub fn with(reports: Arc<ReportRoutes>, data_dir: PathBuf) -> Self {
        Self {
            reports,
            data_dir,
            #[cfg(test)]
            test_fs: Mutex::new(None),
        }
    }

    /// 测试用：文件工具改走这个内存文件系统，不开 SFTP 通道。
    #[cfg(test)]
    pub fn set_test_fs(&self, fs: Arc<dyn RemoteFs>) {
        *self.test_fs.lock().unwrap_or_else(|p| p.into_inner()) = Some(fs);
    }

    fn open_db(&self) -> Result<Database, String> {
        Database::open(&self.data_dir.join("zssh.db")).map_err(|err| {
            format!(
                "{}{}",
                t(AgentHostMsg::OpenHostDbFailedPrefix),
                crate::error_text::error_text(&err)
            )
        })
    }

    /// 目标对应主机库里的哪台主机：`host-<id>` 直接解析；终端编号看终端登记的主机。
    fn host_id_of(target_id: &str) -> Option<i64> {
        parse_host_target_id(target_id)
            .or_else(|| terminals().get(target_id).and_then(|entry| entry.host_id()))
    }
}

impl Default for NativeBridgeHost {
    fn default() -> Self {
        Self::new()
    }
}

/// 目标不是 `host-<id>` 时给模型 / 界面的原因。
fn host_target_error(target_id: &str) -> String {
    format!("{}{target_id}", t(AgentHostMsg::HostTargetOnlyPrefix))
}

/// 解析 `host-<id>`；别的目标返回中文原因（不 panic，也不去连任何东西）。
fn parse_host_target(target_id: &str) -> Result<i64, String> {
    parse_host_target_id(target_id).ok_or_else(|| host_target_error(target_id))
}

/// 执行位置对应的终端：目标本身就是终端编号，或者是主机、取这台主机的焦点终端。
fn terminal_for(target_id: &str) -> Option<Arc<TermEntry>> {
    terminals().get(target_id).or_else(|| {
        parse_host_target_id(target_id).and_then(|host_id| terminals().for_host(host_id))
    })
}

/// 这个目标上用来开 exec / SFTP 通道的已认证连接：有终端就用终端那条，否则主机库里的池连接。
async fn connection_for(target_id: &str) -> Result<Arc<zssh_core::ssh::Connection>, String> {
    if let Some(entry) = terminals().get(target_id) {
        return entry.connection();
    }
    let host_id = parse_host_target(target_id)?;
    Ok(crate::ssh_backend::factory_for_host(host_id)
        .await?
        .connection())
}

impl BridgeHost for NativeBridgeHost {
    /// Telnet / 串口主机不能让 AI 执行命令：按主机库里的协议判断。不在主机库里的目标
    /// （快速连接）一定是 SSH；库读不了时放行——后面真正连接时还有各自的检查。
    fn target_refusal<'a>(&'a self, target_id: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let host_id = Self::host_id_of(target_id)?;
            let db = self.open_db().ok()?;
            db.host_ai_exec_refusal(host_id).ok().flatten()
        })
    }

    /// 界面绑定的会话优先，其次目标本身就是终端，最后是这台主机的焦点终端；都没有返回 `None`，
    /// 由执行层把 `host-<id>` 原样交给下面的 `exec` / `exec_cancellable`（走后台通道）。
    fn execution_session<'a>(
        &'a self,
        target_id: &'a str,
        bound: Option<&'a str>,
    ) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            if let Some(bound) = bound
                && terminals().get(bound).is_some()
            {
                return Some(bound.to_owned());
            }
            terminal_for(target_id).map(|entry| entry.id.clone())
        })
    }

    fn report_target(&self, report: ExecTargetReport) {
        self.reports.send(report);
    }

    fn exec<'a>(
        &'a self,
        target_id: &'a str,
        command: &'a str,
        wait: Duration,
        max_stream_bytes: usize,
    ) -> BoxFuture<'a, Result<ExecCapture, String>> {
        let target_id = target_id.to_owned();
        let command = command.to_owned();
        Box::pin(async move {
            // 先判目标类型：不是主机也不是终端的不去连任何东西。
            if terminals().get(&target_id).is_none() {
                parse_host_target(&target_id)?;
            }
            let connection = connection_for(&target_id).await?;
            connection
                .exec_capture(&command, wait, max_stream_bytes)
                .await
                .map_err(|err| crate::error_text::error_text(&err))
        })
    }

    fn exec_cancellable<'a>(
        &'a self,
        target_id: &'a str,
        command: &'a str,
        wait: Duration,
        max_stream_bytes: usize,
        gate: &'a ExecGate,
    ) -> BoxFuture<'a, Result<ExecCapture, String>> {
        let target_id = target_id.to_owned();
        let command = command.to_owned();
        Box::pin(async move {
            gate.check()?;
            if terminals().get(&target_id).is_none() {
                parse_host_target(&target_id)?;
            }
            let connection = connection_for(&target_id).await?;
            // 自动连接可能等了很久：这期间点了停止 / 目标被移出，就不再发命令。
            gate.check()?;
            // gate 一直带到发 exec 的边界（开通道期间失效就不再等，通道开好之后
            // 发命令之前再核对一次；发出去之后只有停止才关闭执行通道）。
            connection
                .exec_capture_guarded(&command, wait, max_stream_bytes, gate)
                .await
        })
    }

    fn read_terminal<'a>(
        &'a self,
        target_id: &'a str,
        max_lines: usize,
    ) -> BoxFuture<'a, Result<Vec<String>, String>> {
        Box::pin(async move {
            let entry =
                terminal_for(target_id).ok_or_else(|| t(AgentHostMsg::NoTerminal).to_owned())?;
            let lines = entry
                .ring
                .lock()
                .map_err(|_| t(AgentHostMsg::TerminalBufferLockUnavailable).to_owned())?
                .recent_lines(max_lines);
            Ok(lines)
        })
    }

    fn remote_fs<'a>(
        &'a self,
        target_id: &'a str,
    ) -> BoxFuture<'a, Result<Arc<dyn RemoteFs>, String>> {
        Box::pin(async move {
            #[cfg(test)]
            if let Some(fs) = self
                .test_fs
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
            {
                return Ok(fs);
            }
            let connection = connection_for(
                &terminal_for(target_id)
                    .map_or_else(|| target_id.to_owned(), |entry| entry.id.clone()),
            )
            .await?;
            let channel = SftpChannel::open(&connection).await.map_err(|err| {
                format!(
                    "{}{}",
                    t(AgentHostMsg::OpenSftpChannelFailedPrefix),
                    crate::error_text::error_text(&err)
                )
            })?;
            let fs: Arc<dyn RemoteFs> = Arc::new(SftpRemoteFs::new(Arc::new(channel)));
            Ok(fs)
        })
    }

    fn exec_placement<'a>(&'a self, target_id: &'a str) -> BoxFuture<'a, ExecPlacement> {
        Box::pin(async move {
            let Some(entry) = terminal_for(target_id) else {
                return ExecPlacement::Background {
                    reason: t(AgentHostMsg::NoTerminalUseBackground).to_string(),
                };
            };
            // 第一次需要在终端里执行之前，用后台 exec 通道静默探测一次远端登录 shell 的类型
            // （没探测过绝不就地执行）。
            if entry.state.shell_kind() == ShellKind::Unknown {
                let kind = probe_remote_shell_kind(&entry).await;
                tracing::debug!(?kind, "远端 shell 探测结果");
                entry.state.set_shell_kind(kind);
            }
            match entry.state.try_acquire() {
                Ok(()) => ExecPlacement::Terminal,
                Err(reason) => ExecPlacement::Background { reason },
            }
        })
    }

    fn run_in_terminal<'a>(
        &'a self,
        request: TerminalExecRequest,
    ) -> BoxFuture<'a, Result<TerminalExecCapture, String>> {
        Box::pin(async move { run_in_terminal_on(request).await })
    }

    fn release_terminal<'a>(&'a self, target_id: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Some(entry) = terminal_for(target_id) {
                entry.state.release();
            }
        })
    }

    fn interrupt_terminal<'a>(&'a self, scope_id: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            // 只有这个对话**确实**有 AI 命令在终端里跑才发 Ctrl+C：
            // 没有记录就什么都不做——用户可能正在那个终端里跑自己的东西。
            // 一个对话可能在几个终端里各有一条在跑，每个终端都要打断；同一个终端只发一次
            // （超时后仍在跑的旧命令和新命令可能登记在同一个会话上）。
            let mut sent_to = std::collections::HashSet::new();
            for run in terminals().runs().running_all(scope_id) {
                if !sent_to.insert(run.session_id.clone()) {
                    continue;
                }
                let Some(entry) = terminals().get(&run.session_id) else {
                    continue;
                };
                // Ctrl+C（ETX）。会话已经断开时发送端已关，忽略即可。
                let _ = entry
                    .commands
                    .send(TermCommand::Session(SessionCommand::Write(vec![0x03])))
                    .await;
            }
        })
    }

    fn read_memory<'a>(
        &'a self,
        target_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, String>> {
        Box::pin(async move {
            let Some(host_id) = Self::host_id_of(target_id) else {
                return Ok(None);
            };
            let db = self.open_db()?;
            Ok(db
                .host_memory_get(host_id)
                .map_err(|err| crate::error_text::error_text(&err))?
                .map(|memory| memory.content))
        })
    }

    fn write_memory<'a>(
        &'a self,
        target_id: &'a str,
        notes: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let host_id = Self::host_id_of(target_id)
                .ok_or_else(|| t(AgentHostMsg::CannotRememberQuickConnect).to_string())?;
            self.open_db()?
                .host_memory_set(host_id, notes)
                .map_err(|err| crate::error_text::error_text(&err))?;
            // 主机详情里开着的记忆框要刷新（0.2.14 的 `host-memory-changed` 事件）。
            crate::host_activity::memory_changed(host_id);
            Ok(())
        })
    }

    fn write_backup_keep(&self) -> usize {
        // 每次写入时重新读设置：用户在设置页改了份数，下一笔就生效。
        self.open_db()
            .ok()
            .and_then(|db| db.ai_write_backup_keep().ok())
            .unwrap_or(zssh_core::store::settings::AI_WRITE_BACKUP_KEEP_DEFAULT)
    }
}

// ---------------------------------------------------------------------------
// 在用户的终端里执行
// ---------------------------------------------------------------------------

/// 在用户的终端里执行 AI 命令：把包装脚本交给会话任务写进终端，等结束标记，取回输出和退出码。
///
/// 超时**不中断**命令：把当前截到的输出返回，`timed_out = true`。
///
/// 终端占位由 [`NativeBridgeHost::exec_placement`] 里的 `try_acquire` 抢下；这里在命令结束（拿到标记 /
/// 超时 / 会话断开）后**无论成败都**释放。执行期间在 [`TerminalRuns`](zssh_core::ai::runs::TerminalRuns)
/// 登记一条记录：停止按钮据此知道「这个对话有条命令正在这个终端里跑」，然后才发 Ctrl+C。
async fn run_in_terminal_on(request: TerminalExecRequest) -> Result<TerminalExecCapture, String> {
    let Some(entry) = terminal_for(&request.target_id) else {
        return Err(format!(
            "{}{}",
            t(AgentHostMsg::SessionNotConnectedPrefix),
            request.target_id
        ));
    };
    let runs = terminals().runs();
    runs.register(&request.scope_id, &entry.id, &request.marker);
    // 先登记再核对：停止按钮先作废请求、再按登记表发 Ctrl+C。在这之前按的停止，这里能看到；
    // 在这之后按的，登记表里已经有这条，Ctrl+C 能找到它。
    if let Err(reason) = request.gate.check() {
        runs.clear(&request.scope_id, &request.marker);
        entry.state.release();
        return Err(reason);
    }
    let started = Instant::now();
    // 包装脚本（含前置清输入），末尾自己带回车。走 `TermCommand::AiWrite`：会话任务让显示过滤器
    // 记住它，包装脚本的回显和标记行不会显示给用户，显示的是「提示符 + 命令原文」的合成回显。
    // gate 跟着这条消息进会话的命令队列，会话任务真正写之前再核对（审计 C04）。
    let guard: Arc<dyn ExecGuard> = Arc::new(request.gate.clone());
    let payload = format!("{}\r", request.payload).into_bytes();
    let marker = request.marker.clone();
    let command = request.command.clone();
    let written = deliver_ai_write(&entry.commands, guard, |slot| TermCommand::AiWrite {
        payload,
        marker,
        command,
        slot,
    })
    .await;
    if let Err(reason) = written {
        runs.clear(&request.scope_id, &request.marker);
        entry.state.release();
        return Err(reason);
    }

    // 等结束标记：从环形缓冲的原始文本里找。
    let result = wait_for_marker(&entry, &request.marker, request.wait).await;
    entry.state.release();

    match &result {
        MarkerOutcome::Done { .. } | MarkerOutcome::Gone => {
            runs.clear(&request.scope_id, &request.marker);
        }
        MarkerOutcome::Timeout { .. } => {
            // 告诉会话任务的显示过滤器「这条命令超时了」：退出完整过滤（不再压用户之后的输入），
            // 只留轻量过滤丢迟到的那一行结束标记。
            let _ = entry
                .commands
                .send(TermCommand::AiTimedOut {
                    marker: request.marker.clone(),
                })
                .await;
            // 命令还在跑：留着记录（停止按钮还要用它发 Ctrl+C），起一个观察任务等它自己结束
            // （迟到的结束标记）或会话断开再清。
            watch_late_finish(
                Arc::clone(&entry),
                request.scope_id.clone(),
                request.marker.clone(),
            );
        }
    }

    let duration_ms = started.elapsed().as_millis() as u64;
    match result {
        MarkerOutcome::Done { output, exit_code } => Ok(TerminalExecCapture {
            output,
            exit_code: Some(exit_code),
            duration_ms,
            timed_out: false,
        }),
        MarkerOutcome::Timeout { output } => Ok(TerminalExecCapture {
            output,
            exit_code: None,
            duration_ms,
            timed_out: true,
        }),
        // 命令已经完整写进终端才走到这里：会话没了不等于命令没跑（`reboot`、`exit` 最容易撞上），
        // 必须让调用方知道"已送达、结果未知"，不能报成普通失败让它去换通道重试。
        MarkerOutcome::Gone => Err(DELIVERED_RESULT_UNKNOWN.to_string()),
    }
}

/// 超时返回之后命令还在跑：盯住环形缓冲，等它的结束标记出现（或会话断开）就把
/// 登记表里的记录清掉，免得停止按钮把 Ctrl+C 发给一条早就结束的命令（那会打断用户自己跑的东西）。
///
/// 上限半小时：真跑这么久的命令会让记录一直留着，停止按钮继续对它有效，到点之后不再观察。
fn watch_late_finish(entry: Arc<TermEntry>, scope_id: String, marker: String) {
    tokio::spawn(async move {
        let deadline = Instant::now() + Duration::from_secs(30 * 60);
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let runs = terminals().runs();
            // 记录已经被别人清掉（会话断开等）就不用看了。
            if !runs.is_registered(&scope_id, &marker) {
                return;
            }
            if terminals().get(&entry.id).is_none() {
                runs.clear(&scope_id, &marker);
                return;
            }
            let raw = entry.ring.lock().map(|r| r.raw_text()).unwrap_or_default();
            if zssh_core::ai::pty::find_end_marker(&raw, &marker).is_some()
                || Instant::now() >= deadline
            {
                runs.clear(&scope_id, &marker);
                return;
            }
        }
    });
}

/// 探测远端登录 shell 的类型（移植自 Netcatty 的 `sessionShellKind.cjs`）：在后台 exec 通道里跑一小段
/// POSIX 脚本，拿回登录 shell 路径再分类。探测失败 / 超时 / 认不出一律 `Other`（走后台通道）。
async fn probe_remote_shell_kind(entry: &TermEntry) -> ShellKind {
    use zssh_core::ai::shell_kind::{
        SHELL_PROBE_TIMEOUT, build_shell_probe_command, parse_shell_probe_output,
    };
    let Ok(connection) = entry.connection() else {
        return ShellKind::Other;
    };
    let command = build_shell_probe_command();
    // exec_capture 自己带 wait 超时；外面再包一层，防止通道卡住不返回。
    let capture = match tokio::time::timeout(
        SHELL_PROBE_TIMEOUT + Duration::from_secs(2),
        connection.exec_capture(&command, SHELL_PROBE_TIMEOUT, 64 * 1024),
    )
    .await
    {
        Ok(Ok(capture)) => capture,
        Ok(Err(err)) => {
            tracing::debug!(error = %err, "远端 shell 探测失败，走后台通道");
            return ShellKind::Other;
        }
        Err(_) => {
            tracing::debug!("远端 shell 探测超时，走后台通道");
            return ShellKind::Other;
        }
    };
    parse_shell_probe_output(&capture.stdout).unwrap_or(ShellKind::Other)
}

/// 在环形缓冲里等结束标记的结果。
enum MarkerOutcome {
    Done { output: String, exit_code: i32 },
    Timeout { output: String },
    Gone,
}

/// 轮询终端的原始输出，直到出现结束标记（或超时 / 会话断开）。
async fn wait_for_marker(entry: &TermEntry, marker: &str, wait: Duration) -> MarkerOutcome {
    use zssh_core::ai::pty::{extract_output, find_end_marker};
    let deadline = Instant::now() + wait;
    loop {
        let raw = entry.ring.lock().map(|r| r.raw_text()).unwrap_or_default();
        if let Some(found) = find_end_marker(&raw, marker) {
            return MarkerOutcome::Done {
                output: extract_output(&raw, marker),
                exit_code: found.exit_code,
            };
        }
        if terminals().get(&entry.id).is_none() {
            return MarkerOutcome::Gone;
        }
        if Instant::now() >= deadline {
            // 超时：命令不中断，把当前截到的输出带上。
            return MarkerOutcome::Timeout {
                output: strip_marker_noise(&raw, marker),
            };
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 超时时把已经打出来的输出尽量整理一下（去掉包装脚本本身那几行）。
fn strip_marker_noise(raw: &str, marker: &str) -> String {
    use zssh_core::ai::pty::strip_ansi;
    let start_pattern = format!("{marker}_S");
    let body = match raw.rfind(&start_pattern) {
        Some(pos) => raw[pos..]
            .split_once('\n')
            .map(|(_, rest)| rest)
            .unwrap_or(""),
        None => raw,
    };
    strip_ansi(body).trim().to_string()
}

// ---------------------------------------------------------------------------
// 审批
// ---------------------------------------------------------------------------

/// 送给窗口的一条审批请求（窗口据此弹确认卡片）。
#[derive(Debug, Clone)]
pub struct ApprovalPrompt {
    pub id: u64,
    pub request: ApprovalRequest,
}

/// 待确认请求表：请求 ID → （属于哪个对话的 scope，等答案的一端）。
#[derive(Default)]
struct PendingApprovals {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, (String, oneshot::Sender<ApprovalDecision>)>>,
}

impl PendingApprovals {
    fn pending(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<u64, (String, oneshot::Sender<ApprovalDecision>)>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 登记一个待确认的请求，返回请求 ID 和等答案的一端。
    fn register(&self, scope: &str) -> (u64, oneshot::Receiver<ApprovalDecision>) {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let (sender, receiver) = oneshot::channel();
        self.pending().insert(id, (scope.to_owned(), sender));
        (id, receiver)
    }

    fn take(&self, id: u64) -> Option<oneshot::Sender<ApprovalDecision>> {
        self.pending().remove(&id).map(|(_, sender)| sender)
    }

    fn cancel(&self, id: u64) {
        let _ = self.take(id);
    }
}

/// 审批 future 一结束（回答、超时、scope 销毁）就把请求摘掉：界面再点也不会送到
/// 已经没人等的请求上，`resolve` 也能据此回答"没有这个请求"。
struct PendingGuard {
    pending: Arc<PendingApprovals>,
    id: u64,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.pending.cancel(self.id);
    }
}

/// 工具桥的审批实现：请求走 `std::sync::mpsc` 给窗口，答案走 oneshot 回来。
///
/// 一个桥服务多个对话（每个窗口一个对话，三种智能体共用同一个桥，和 Tauri 版一致）：请求带着
/// 自己的 scope id，按 [`Self::attach_scope`] 登记的出口送到对应对话的窗口；没有登记的 scope
/// 走默认出口（单对话的桥、测试），再没有就按拒绝处理。
pub struct NativeApprover {
    /// 没登记分路的 scope 的出口。
    default_route: Option<Sender<ApprovalPrompt>>,
    routes: Mutex<HashMap<String, Sender<ApprovalPrompt>>>,
    pending: Arc<PendingApprovals>,
}

impl NativeApprover {
    /// 所有请求都送到同一个出口的审批（单对话的桥；测试用）。
    #[cfg(test)]
    pub fn new(prompts: Sender<ApprovalPrompt>) -> Self {
        Self {
            default_route: Some(prompts),
            routes: Mutex::new(HashMap::new()),
            pending: Arc::new(PendingApprovals::default()),
        }
    }

    /// 只按 scope 分路、没有默认出口的审批（共用桥用）。
    pub fn routed() -> Self {
        Self {
            default_route: None,
            routes: Mutex::new(HashMap::new()),
            pending: Arc::new(PendingApprovals::default()),
        }
    }

    fn routes(&self) -> std::sync::MutexGuard<'_, HashMap<String, Sender<ApprovalPrompt>>> {
        self.routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 这个 scope 的确认请求送到 `prompts`。
    pub fn attach_scope(&self, scope_id: &str, prompts: Sender<ApprovalPrompt>) {
        self.routes().insert(scope_id.to_owned(), prompts);
    }

    /// 对话结束：摘掉出口，这个 scope 挂着的确认按拒绝结束，之后再来的请求也直接拒绝。
    pub fn detach_scope(&self, scope_id: &str) {
        self.routes().remove(scope_id);
        self.deny_scope(scope_id);
    }

    /// 这条请求还有人在等吗（没被回答，也没因超时、停止或 scope 销毁被摘掉）。
    pub fn is_live(&self, id: u64) -> bool {
        self.pending.pending().contains_key(&id)
    }

    /// 把所有挂着的确认按拒绝结束，返回结束了几条（测试用；产品里停止只结束自己 scope 的）。
    #[cfg(test)]
    pub fn deny_all(&self) -> usize {
        let senders: Vec<_> = self
            .pending
            .pending()
            .drain()
            .map(|(_, (_, sender))| sender)
            .collect();
        let count = senders.len();
        for sender in senders {
            let _ = sender.send(ApprovalDecision::Denied);
        }
        count
    }

    /// 停止某个对话：只把**它的** scope 挂着的确认按拒绝结束，别的对话（别的标签）不受影响。
    pub fn deny_scope(&self, scope_id: &str) -> usize {
        let senders: Vec<_> = {
            let mut pending = self.pending.pending();
            let ids: Vec<u64> = pending
                .iter()
                .filter(|(_, (scope, _))| scope == scope_id)
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter()
                .filter_map(|id| pending.remove(&id))
                .map(|(_, sender)| sender)
                .collect()
        };
        let count = senders.len();
        for sender in senders {
            let _ = sender.send(ApprovalDecision::Denied);
        }
        count
    }

    /// 界面答了一条确认请求：把答案送给等着的审批。
    ///
    /// 返回这个请求是否存在（可能已经超时 / 被 scope 销毁）。
    pub fn resolve(&self, id: u64, allowed: bool) -> bool {
        match self.pending.take(id) {
            Some(sender) => {
                // 等的一端可能已经走了（超时 / 取消），答案没人收也无所谓。
                let _ = sender.send(if allowed {
                    ApprovalDecision::Allowed
                } else {
                    ApprovalDecision::Denied
                });
                true
            }
            None => false,
        }
    }

    /// 这条请求该送到哪个窗口。
    fn route_of(&self, scope: &str) -> Option<Sender<ApprovalPrompt>> {
        self.routes()
            .get(scope)
            .cloned()
            .or_else(|| self.default_route.clone())
    }
}

impl Approver for NativeApprover {
    fn approve<'a>(&'a self, request: ApprovalRequest) -> BoxFuture<'a, ApprovalDecision> {
        Box::pin(async move {
            // 对话已经结束（出口摘掉了）：别让命令挂住，也别弹到别的对话里。
            let Some(route) = self.route_of(&request.scope) else {
                return ApprovalDecision::Denied;
            };
            let (id, reply) = self.pending.register(&request.scope);
            let _guard = PendingGuard {
                pending: Arc::clone(&self.pending),
                id,
            };
            if route.send(ApprovalPrompt { id, request }).is_err() {
                // 窗口还没起来（或者已经关了）：按拒绝处理，别让命令挂住。
                return ApprovalDecision::Denied;
            }
            // 等的一端没了（界面直接丢了请求）也按拒绝，不无限等。
            reply.await.unwrap_or(ApprovalDecision::Denied)
        })
    }
}

// ---------------------------------------------------------------------------
// 执行模式 / 审计
// ---------------------------------------------------------------------------

/// 应用现在是否锁屏。界面线程在锁定 / 解锁时写（见 `app_lock::set_locked`），工具桥每次请求都读：
/// 锁着时 AI 要在服务器上做的任何事都变成“要确认”（确认卡等解锁后才看得到），不再因为
/// 执行模式是“全部放行 / 只读自动”就直接执行。
static AI_LOCKED: AtomicBool = AtomicBool::new(false);

/// 界面通知工具桥：锁屏状态变了。
pub fn set_ai_locked(locked: bool) {
    AI_LOCKED.store(locked, Ordering::SeqCst);
}

/// 执行模式来源：每次命令都重新读设置库（用户改完马上生效）。
pub struct NativeMode {
    db_path: PathBuf,
}

impl NativeMode {
    /// `data_dir` 是数据目录，库文件固定叫 `zssh.db`（和 Tauri 版同一个库）。
    pub fn new(data_dir: &Path) -> Self {
        Self {
            db_path: data_dir.join("zssh.db"),
        }
    }

    /// 从已经打开的库读模式；读不到按手动（往安全的方向退）。
    fn from_db(db: &Database) -> ExecMode {
        match db.ai_exec_mode() {
            Ok(raw) => ExecMode::from_setting(Some(&raw)),
            Err(err) => {
                tracing::warn!(error = %err, "读 AI 执行模式失败，按手动处理");
                ExecMode::Manual
            }
        }
    }
}

impl ExecModeProvider for NativeMode {
    fn force_confirm(&self) -> bool {
        AI_LOCKED.load(Ordering::SeqCst)
    }

    fn mode(&self) -> ExecMode {
        match Database::open(&self.db_path) {
            Ok(db) => Self::from_db(&db),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    path = %self.db_path.display(),
                    "打开设置库失败，AI 执行模式按手动处理"
                );
                ExecMode::Manual
            }
        }
    }
}

/// 审计落盘：每次写一条时打开库；写失败只记日志，不影响命令执行。
pub struct NativeAudit {
    db_path: PathBuf,
}

impl NativeAudit {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            db_path: data_dir.join("zssh.db"),
        }
    }
}

impl AuditLog for NativeAudit {
    fn record(&self, entry: AuditEntry) {
        match Database::open(&self.db_path) {
            // `Database` 自己实现了 `AuditLog`，写失败会在里面记日志。
            Ok(db) => <Database as AuditLog>::record(&db, entry),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    path = %self.db_path.display(),
                    "AI 审计打不开数据库"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 工具桥（不依赖 claude 进程）
// ---------------------------------------------------------------------------

/// 对话里的一台目标主机：界面选中的、允许 AI 使用的主机。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostTarget {
    pub id: i64,
    pub name: String,
    pub username: String,
    /// 当前聚焦窗格所在的主机：没写 `target_id` 的工具调用落在它身上（0.2.14 的 `current`）。
    pub current: bool,
    /// 用户 `@` 进来的（或焦点切走时自动保留下来的）主机，不是单靠「当前」带进来的。
    pub pinned: bool,
    /// 这台主机不能当 AI 的执行目标时的原因（Telnet / 串口主机，来自 `ProtocolSettings::ai_exec_refusal`，
    /// 和工具桥 `target_refusal` 同一句话）。界面上列出来但置灰并写明原因，不会被同步给桥。
    pub refusal: Option<String>,
    /// 目标是快速连接的这个终端本身（`term-<n>`，没有主机记录；对话锚定在终端上）：`id` 不用。
    pub terminal: Option<String>,
}

impl HostTarget {
    /// 在桥的 scope 里的目标编号：主机是 `host-<id>`，快速连接的终端是它自己的终端编号。
    pub fn scope_id(&self) -> String {
        self.terminal
            .clone()
            .unwrap_or_else(|| host_target_id(self.id))
    }

    /// 普通的被选中主机：不是「当前」，按固定目标处理（独立的 AI 窗口、主机库的主机列表用）。
    pub fn plain(id: i64, name: impl Into<String>, username: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            username: username.into(),
            current: false,
            pinned: true,
            refusal: None,
            terminal: None,
        }
    }
}

/// 一个对话的工具桥一侧：桥、审批、scope 和等用户确认的队列。
///
/// 和智能体进程无关，窗口的「停止」「目标增减」都落在这里，测试也能直接拿真的桥驱动。
/// 桥可以是这个对话自己的（[`ToolChannel::new`]，测试用），也可以是全进程共用的
/// （[`AgentHub`] 里的那一个，三种智能体的对话都挂在上面）。
pub struct ToolChannel {
    bridge: Arc<Bridge>,
    approver: Arc<NativeApprover>,
    reports: Arc<ReportRoutes>,
    scope: ScopeHandle,
    /// 等用户确认的命令（窗口逐个取出弹卡片）。
    approvals: Receiver<ApprovalPrompt>,
    /// 执行层回报的实际执行位置（窗口据此在卡片上标主机、给「查看终端」）。
    target_reports: Receiver<ExecTargetReport>,
}

impl ToolChannel {
    /// 用给定的宿主、审计和模式来源建一个**自己的**桥，并建好这个对话的 scope（测试用；
    /// 产品里的对话都挂在 [`AgentHub`] 的共用桥上）。
    #[cfg(test)]
    pub fn new(
        host: Arc<dyn BridgeHost>,
        audit: Arc<dyn AuditLog>,
        mode: Arc<dyn ExecModeProvider>,
    ) -> Self {
        Self::new_with_host(
            |_| host,
            audit,
            mode,
            zssh_core::ai::bridge::DEFAULT_APPROVAL_WAIT,
        )
    }

    /// 同 [`Self::new`]，宿主要用这个对话的执行位置回报出口表建（`NativeBridgeHost::with`）。
    #[cfg(test)]
    pub fn new_with_host(
        make_host: impl FnOnce(Arc<ReportRoutes>) -> Arc<dyn BridgeHost>,
        audit: Arc<dyn AuditLog>,
        mode: Arc<dyn ExecModeProvider>,
        approval_wait: Duration,
    ) -> Self {
        let reports = Arc::new(ReportRoutes::default());
        let host = make_host(Arc::clone(&reports));
        let (prompts, approvals) = std::sync::mpsc::channel();
        let approver = Arc::new(NativeApprover::new(prompts));
        let bridge = Arc::new(
            Bridge::new(
                host,
                audit,
                Arc::clone(&approver) as Arc<dyn Approver>,
                mode,
            )
            .with_approval_wait(approval_wait),
        );
        let scope = bridge.create_scope();
        let (report_tx, target_reports) = std::sync::mpsc::channel();
        reports.attach(scope.scope_id(), report_tx);
        Self {
            bridge,
            approver,
            reports,
            scope,
            approvals,
            target_reports,
        }
    }

    /// 挂到共用的桥上：开一个新 scope，这个 scope 的确认请求只送到这个对话的队列。
    fn attach(
        bridge: &Arc<Bridge>,
        approver: &Arc<NativeApprover>,
        reports: &Arc<ReportRoutes>,
    ) -> Self {
        let (prompts, approvals) = std::sync::mpsc::channel();
        let (report_tx, target_reports) = std::sync::mpsc::channel();
        let scope = bridge.create_scope();
        approver.attach_scope(scope.scope_id(), prompts);
        reports.attach(scope.scope_id(), report_tx);
        Self {
            bridge: Arc::clone(bridge),
            approver: Arc::clone(approver),
            reports: Arc::clone(reports),
            scope,
            approvals,
            target_reports,
        }
    }

    /// 取下一份执行位置回报（不阻塞）。
    pub fn next_report(&self) -> Option<ExecTargetReport> {
        self.target_reports.try_recv().ok()
    }

    pub fn bridge(&self) -> &Arc<Bridge> {
        &self.bridge
    }

    pub fn scope(&self) -> &ScopeHandle {
        &self.scope
    }

    /// 「停止」：作废这个对话此刻之前开始的所有工具请求。
    ///
    /// 和 Tauri 版 `ai_agent_abort` 走同一条路径（`Bridge::stop_scope`）：等审批的按拒绝
    /// 结束、已经发出命令的后台通道关掉、还没开始写的文件不再写。另外把窗口这边已经排队、
    /// 还没取走的确认全部丢掉：停止之前发起的请求不能在停止之后重新冒出一张可点的卡片。
    /// 返回丢掉了几条排队的确认。
    pub fn stop(&self) -> usize {
        self.bridge.stop_scope(self.scope.scope_id());
        // 只结束这个对话的确认：共用桥上别的标签的对话照常等它们的用户。
        self.approver.deny_scope(self.scope.scope_id());
        let mut dropped = 0;
        while self.approvals.try_recv().is_ok() {
            dropped += 1;
        }
        dropped
    }

    /// 用户发了新消息：解除 [`ToolChannel::stop`] 设下的「已停止」态，之后这个对话的工具调用恢复正常。
    /// 外部智能体在用户点停止之后、收到中断之前发来的调用会被一律拒绝，直到这里被调用。
    pub fn resume(&self) {
        self.bridge.resume_scope(self.scope.scope_id());
    }

    /// 取下一条还有效的确认。已经被摘掉的（停止、超时、销毁）直接跳过，不给界面。
    pub fn next_prompt(&self) -> Option<ApprovalPrompt> {
        while let Ok(prompt) = self.approvals.try_recv() {
            if self.approver.is_live(prompt.id) {
                return Some(prompt);
            }
            tracing::debug!(id = prompt.id, "跳过已经失效的确认请求");
        }
        None
    }

    /// 界面回答了一条确认请求；请求已经失效时返回 `false`。
    pub fn resolve_approval(&self, id: u64, allowed: bool) -> bool {
        self.approver.resolve(id, allowed)
    }

    /// 这条确认还在等人回答吗。
    pub fn approval_is_live(&self, id: u64) -> bool {
        self.approver.is_live(id)
    }

    /// 让 scope 里的主机目标和界面选中的一致：多出来的摘掉，选中的加上（已有的只更新）。
    ///
    /// 只管 `host-<id>` 目标；摘掉目标会让只对旧目标批准过的在途请求失效（桥的 R-9 F11）。
    /// 只是"允许 AI 用这些主机"，不等于已经连上：第一次执行时由 [`NativeBridgeHost`] 自动连接。
    pub fn sync_hosts(&self, wanted: &[HostTarget]) {
        let wanted_ids: HashSet<String> = wanted.iter().map(HostTarget::scope_id).collect();
        for target in self.scope.targets() {
            let ours = parse_host_target_id(&target.id).is_some()
                || target.id.starts_with(crate::ai_terminals::TERMINAL_PREFIX);
            if ours && !wanted_ids.contains(&target.id) {
                self.scope.remove_target(&target.id);
            }
        }
        for host in wanted {
            // 同一个 ID 再加是更新（焦点、「当前」随标签变化时不算「移出再加回」，在途请求不受影响）。
            // 状态：这台主机 / 这个终端现在有打开的终端就是 connected，否则只是被 @ 进来（pinned）。
            let connected = match host.terminal.as_deref() {
                Some(terminal) => terminals().get(terminal).is_some(),
                None => terminals().has_host(host.id),
            };
            let status = if connected { "connected" } else { "pinned" };
            self.scope.add_target(
                host.scope_id(),
                host.name.clone(),
                host.username.clone(),
                status,
                host.current,
                host.pinned,
            );
        }
    }
}

impl ToolChannel {
    /// 对话结束：作废在途请求，摘掉这个 scope 在共用桥上的出口，销毁 scope（能力密钥立即失效）。
    pub fn close(&self) {
        self.stop();
        self.approver.detach_scope(self.scope.scope_id());
        self.reports.detach(self.scope.scope_id());
        self.scope.destroy();
    }
}

impl Drop for ToolChannel {
    fn drop(&mut self) {
        // 没走 shutdown 就丢了句柄：至少别把出口和 scope 留在共用的桥上。
        self.approver.detach_scope(self.scope.scope_id());
        self.reports.detach(self.scope.scope_id());
        self.scope.destroy();
    }
}

// ---------------------------------------------------------------------------
// 对话
// ---------------------------------------------------------------------------

/// 一个真实的智能体对话（opencode / codex / Claude Code 三种共用）：驱动句柄、工具桥一侧、事件出口。
///
/// Claude Code 的进程是懒启动的（`ClaudeConversation::create` 只探测 `--help`），第一条消息由
/// 调用方发；opencode / codex 的对话开在共用进程里（进程第一次用到时由 [`AgentHub`] 拉起）。
pub struct AgentSession {
    pub kind: AgentKind,
    /// 驱动句柄。
    pub conversation: Arc<dyn AgentConversation>,
    /// 工具桥一侧：scope、审批队列、停止。
    pub tools: ToolChannel,
    /// 驱动事件（正文 / 工具 / 回合结束）。
    pub events: mpsc::UnboundedReceiver<AgentEvent>,
    /// 要求续接历史会话时：是不是真的接上了智能体自己的会话。false = 作为新会话继续
    /// （界面提示"之前的上下文没有带给 AI"）。Claude Code 要到第一次发消息才知道，这里先按
    /// true，续不上时驱动另发 `ResumeFailed` 事件。
    pub resumed: bool,
    /// 内置助手专有的两样（外部智能体是 `None`）。
    pub builtin: Option<BuiltinLink>,
}

/// 内置助手的会话多出来的东西：旁路信号（用量 / 提示 / 失败，由界面翻成统一的事件）。
pub struct BuiltinLink {
    pub signals: mpsc::UnboundedReceiver<BuiltinSignal>,
}

/// 启动内置助手对话要的东西。
pub struct BuiltinStart {
    pub endpoint: AiEndpoint,
    pub api_key: ApiKey,
    /// HTTP 层：产品里按"软件网络"代理建；测试里是假接口。
    pub transport: Arc<dyn Transport>,
    pub catalog: Arc<Catalog>,
    /// 续接：上次 `resume_id()` 给的值。
    pub resume: Option<String>,
    pub tuning: BuiltinTuning,
}

impl AgentSession {
    /// 界面回答了一条确认请求；请求已经超时 / 不存在时返回 `false`。
    pub fn resolve_approval(&self, id: u64, allowed: bool) -> bool {
        self.tools.resolve_approval(id, allowed)
    }

    /// 让 AI 能用的主机和界面选中的一致。
    pub fn sync_hosts(&self, wanted: &[HostTarget]) {
        self.tools.sync_hosts(wanted);
    }

    /// 这个对话的能力密钥（存历史时要从正文里抹掉）。
    pub fn capability(&self) -> &str {
        self.tools.scope().capability()
    }

    /// 取下一条还有效的确认请求。
    pub fn next_prompt(&self) -> Option<ApprovalPrompt> {
        self.tools.next_prompt()
    }

    /// 取下一份执行位置回报。
    pub fn next_report(&self) -> Option<ExecTargetReport> {
        self.tools.next_report()
    }

    /// 「停止」时对终端里的 AI 命令发 Ctrl+C 要用的句柄：桥 + scope id。拿出来再 `await`，不占着会话。
    /// 只有这个对话**确实**有命令在某个终端里跑才会发（用户自己的程序不会被误打断）。
    pub fn interrupt_handle(&self) -> (Arc<Bridge>, String) {
        (
            Arc::clone(self.tools.bridge()),
            self.tools.scope().scope_id().to_owned(),
        )
    }

    /// 停止当前回合的工具请求（桥一侧）；驱动一侧另由 `conversation.stop()` 通知。
    pub fn stop_tools(&self) -> usize {
        self.tools.stop()
    }

    /// 用户发新消息之前调用：解除停止设下的「已停止」态（见 [`ToolChannel::resume`]）。
    pub fn resume_tools(&self) {
        self.tools.resume();
    }

    /// 结束对话：先作废在途工具请求，再停驱动、销毁作用范围。共用进程留给别的对话继续用
    /// （空闲后由 core 自己关）。
    pub async fn shutdown(&mut self) {
        self.tools.stop();
        self.conversation.shutdown().await;
        self.tools.close();
    }
}

/// 发第一条消息：启动期间用户点了停止 / 新对话（`cancel` 置位）就不发，
/// 返回 `Ok(false)`。会话随后会被窗口关掉，模型根本没收到这条消息。
pub async fn send_unless_cancelled(
    conversation: &dyn AgentConversation,
    cancel: &AtomicBool,
    text: &str,
    model: Option<&str>,
) -> Result<bool, String> {
    if cancel.load(Ordering::SeqCst) {
        return Ok(false);
    }
    conversation
        .send(text, model)
        .await
        .map_err(|err| err.to_string())?;
    Ok(true)
}

/// `zssh-mcp`（Windows 上是 `zssh-mcp.exe`）应该在的位置：和主程序放在同一个目录。
fn mcp_binary_location() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|err| format!("{}{err}", t(AgentHostMsg::CurrentExeFailedPrefix)))?;
    let dir = exe
        .parent()
        .ok_or_else(|| t(AgentHostMsg::NoExeParentDir).to_string())?;
    Ok(dir.join(format!("zssh-mcp{}", std::env::consts::EXE_SUFFIX)))
}

/// 驱动的启动参数。
///
/// 单独拿出来是为了能测「续接 id 有没有原样交给驱动」：真的启动会探测 `claude --help`
/// 并起工具桥，测试里不能跑。`extra_env` 产品里恒为空（用用户自己的配置，和 Tauri 版一致）；
/// 只有自动化测试会借它给 claude 指一个独立的 `CLAUDE_CONFIG_DIR`。
fn start_options(
    install: AgentInstall,
    data_dir: &Path,
    mcp: McpServerSpec,
    capability: String,
    events: mpsc::UnboundedSender<AgentEvent>,
    resume_session_id: Option<String>,
    extra_env: Vec<(String, String)>,
) -> StartOptions {
    StartOptions {
        install,
        workspace: data_dir.join("agent-workspace"),
        mcp,
        capability,
        events,
        extra_env,
        resume_session_id,
    }
}

// ---------------------------------------------------------------------------
// 共用的桥和进程
// ---------------------------------------------------------------------------

/// 启动一个对话（或共用进程）时可以覆盖的东西。**产品里全是默认值**（用用户自己的智能体配置，
/// 和 Tauri 版一致）；自动化测试借它把用户真实的 `~/.claude`、`~/.codex`、opencode 配置挡在外面。
/// 共用进程只在第一次用到时启动，覆盖项也只在那一次起作用。
#[derive(Default, Clone)]
pub struct StartOverrides {
    /// claude 的额外环境变量（`CLAUDE_CONFIG_DIR` 等）。
    pub extra_env: Vec<(String, String)>,
    /// opencode 注入的"用户模型配置"。`None` = 读用户自己的 opencode 配置里的模型相关键
    /// （产品行为）；`Some` = 用给定的，不去读用户的配置。
    pub opencode_user_config: Option<serde_json::Value>,
    /// codex 的配置目录（`CODEX_HOME`）。`None` = 用户自己的。
    pub codex_home: Option<PathBuf>,
}

/// 一个共用的智能体进程（opencode / codex 各一个槽位；Claude Code 一个对话一个进程）。
#[derive(Clone)]
enum SharedRuntime {
    OpenCode(Arc<AgentRuntime>),
    Codex(Arc<CodexRuntime>),
}

impl SharedRuntime {
    fn is_closed(&self) -> bool {
        match self {
            SharedRuntime::OpenCode(runtime) => runtime.is_closed(),
            SharedRuntime::Codex(runtime) => runtime.is_closed(),
        }
    }

    async fn shutdown(&self) {
        match self {
            SharedRuntime::OpenCode(runtime) => runtime.shutdown().await,
            SharedRuntime::Codex(runtime) => runtime.shutdown().await,
        }
    }

    async fn models(&self) -> Result<AgentModelCatalog, String> {
        match self {
            SharedRuntime::OpenCode(runtime) => runtime.models().await,
            SharedRuntime::Codex(runtime) => runtime.models().await,
        }
        .map_err(|err| err.to_string())
    }
}

/// 全进程共用的智能体宿主：**一个**工具桥（一个监听端口、一个连接令牌）、一个按 scope 分路的审批，
/// 加 opencode / codex 各一个懒启动的共用进程。每个对话（每个窗口一个）在桥上有自己的 scope
/// 和能力密钥，确认请求只送到自己的窗口。
///
/// 对应 Tauri 版的 `ToolBridge` + `AgentRegistry`：opencode / codex 的 MCP 配置在进程启动时
/// 就写死了桥的端口和令牌，所以桥必须比任何一个对话都活得久。
pub struct AgentHub {
    bridge: Arc<Bridge>,
    approver: Arc<NativeApprover>,
    reports: Arc<ReportRoutes>,
    port: u16,
    data_dir: PathBuf,
    mcp_binary: PathBuf,
    runtimes: Mutex<HashMap<AgentKind, SharedRuntime>>,
    /// 启动共用进程时串行：面板一打开就预取模型目录，可能和用户发第一条消息同时到，
    /// 不串行的话两边都看到"没有进程"，会各起一个。
    start_lock: tokio::sync::Mutex<()>,
    serve: tokio::task::JoinHandle<()>,
}

impl Drop for AgentHub {
    fn drop(&mut self) {
        self.serve.abort();
    }
}

impl AgentHub {
    /// 用给定的宿主、审计和模式来源建桥并开始监听（测试里用桩；产品走 [`Self::production`]）。
    #[cfg(test)]
    pub async fn with_parts(
        host: Arc<dyn BridgeHost>,
        audit: Arc<dyn AuditLog>,
        mode: Arc<dyn ExecModeProvider>,
        data_dir: PathBuf,
        mcp_binary: PathBuf,
    ) -> Result<Arc<Self>, String> {
        Self::with_reports(
            host,
            audit,
            mode,
            data_dir,
            mcp_binary,
            Arc::new(ReportRoutes::default()),
        )
        .await
    }

    /// 同 [`Self::with_parts`]，宿主和中心共用同一张执行位置回报出口表（宿主 `report_target` 往里送，
    /// 各对话的 [`ToolChannel`] 在里面登记自己的出口）。
    pub async fn with_reports(
        host: Arc<dyn BridgeHost>,
        audit: Arc<dyn AuditLog>,
        mode: Arc<dyn ExecModeProvider>,
        data_dir: PathBuf,
        mcp_binary: PathBuf,
        reports: Arc<ReportRoutes>,
    ) -> Result<Arc<Self>, String> {
        // 先同步绑定拿端口（和 Tauri 版一致），异步运行时只负责 accept。
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
            .map_err(|err| format!("{}{err}", t(AgentHostMsg::ToolBridgeBindFailedPrefix)))?;
        listener.set_nonblocking(true).map_err(|err| {
            format!(
                "{}{err}",
                t(AgentHostMsg::ToolBridgeNonblockingFailedPrefix)
            )
        })?;
        let port = listener
            .local_addr()
            .map_err(|err| format!("{}{err}", t(AgentHostMsg::ToolBridgePortFailedPrefix)))?
            .port();
        let approver = Arc::new(NativeApprover::routed());
        let bridge = Arc::new(Bridge::new(
            host,
            audit,
            Arc::clone(&approver) as Arc<dyn Approver>,
            mode,
        ));
        let serve_bridge = Arc::clone(&bridge);
        let serve = tokio::spawn(async move {
            match tokio::net::TcpListener::from_std(listener) {
                Ok(listener) => serve_bridge.serve(listener).await,
                Err(err) => tracing::warn!(error = %err, "工具桥监听失败"),
            }
        });
        Ok(Arc::new(Self {
            bridge,
            approver,
            reports,
            port,
            data_dir,
            mcp_binary,
            runtimes: Mutex::new(HashMap::new()),
            start_lock: tokio::sync::Mutex::new(()),
            serve,
        }))
    }

    /// 产品里的宿主：执行走主机库的 SSH 连接，模式和审计读同一个数据目录里的库。
    async fn production(data_dir: PathBuf, mcp_binary: PathBuf) -> Result<Arc<Self>, String> {
        let reports = Arc::new(ReportRoutes::default());
        Self::with_reports(
            Arc::new(NativeBridgeHost::with(
                Arc::clone(&reports),
                data_dir.clone(),
            )),
            Arc::new(NativeAudit::new(&data_dir)),
            Arc::new(NativeMode::new(&data_dir)),
            data_dir,
            mcp_binary,
            reports,
        )
        .await
    }

    /// 外部智能体要靠 `zssh-mcp.exe` 回头调工具桥，它不在原位就别启动（内置助手进程内调桥，不需要它）。
    fn require_mcp_binary(&self) -> Result<(), String> {
        if fake::world().is_some() || self.mcp_binary.is_file() {
            return Ok(());
        }
        Err(format!(
            "{}{}{}",
            t(AgentHostMsg::McpBinaryMissingPrefix),
            self.mcp_binary.display(),
            t(AgentHostMsg::McpBinaryMissingSuffix)
        ))
    }

    fn mcp_spec(&self) -> McpServerSpec {
        McpServerSpec::zssh(&self.mcp_binary, self.port, self.bridge.connection_token())
    }

    fn runtimes(&self) -> std::sync::MutexGuard<'_, HashMap<AgentKind, SharedRuntime>> {
        self.runtimes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 某种智能体还活着的共用进程。
    fn live_runtime(&self, kind: AgentKind) -> Option<SharedRuntime> {
        self.runtimes()
            .get(&kind)
            .filter(|runtime| !runtime.is_closed())
            .cloned()
    }

    /// 取用某种智能体的现成共用进程；没有或已关停就重新启动一个。
    async fn runtime_for(
        &self,
        kind: AgentKind,
        install: AgentInstall,
        overrides: &StartOverrides,
    ) -> Result<SharedRuntime, String> {
        let _starting = self.start_lock.lock().await;
        if let Some(runtime) = self.live_runtime(kind) {
            return Ok(runtime);
        }
        self.require_mcp_binary()?;
        let workspace = self.data_dir.join("agent-workspace");
        let spec = self.mcp_spec();
        let runtime = match kind {
            // claude / codebuddy 不用共用进程（一个对话一个进程），走不到这儿。
            AgentKind::Claude | AgentKind::Codebuddy => {
                return Err("这种智能体不使用共用进程".to_string()); // i18n-allow: 不可达的内部控制流断言（开发者信息），不进界面
            }
            AgentKind::Builtin => return Err("ZeePilot 不使用共用进程".to_string()), // i18n-allow: 不可达的内部控制流断言（开发者信息），不进界面
            AgentKind::OpenCode => {
                // 只带上用户配置里的模型相关键（自建服务商照常可用），其余一律不带。
                let user_config = overrides
                    .opencode_user_config
                    .clone()
                    .unwrap_or_else(read_user_model_config);
                let options = OpenCodeStartOptions {
                    install,
                    config_home: default_config_home(&workspace),
                    workspace,
                    config_content: build_config_content(&spec, &[], &user_config),
                    startup_timeout: OPENCODE_STARTUP_TIMEOUT,
                };
                SharedRuntime::OpenCode(
                    AgentRuntime::start(options, idle_timeout(OPENCODE_IDLE_TIMEOUT))
                        .await
                        .map_err(|err| err.to_string())?,
                )
            }
            AgentKind::Codex => {
                let probes = codex::startup::command_probes_with_home(
                    &install.path,
                    overrides.codex_home.clone(),
                );
                let options = codex::StartOptions {
                    install,
                    workspace,
                    mcp: spec,
                    probes,
                    codex_home: overrides.codex_home.clone(),
                    startup_timeout: CODEX_STARTUP_TIMEOUT,
                };
                SharedRuntime::Codex(
                    CodexRuntime::start(options, idle_timeout(CODEX_IDLE_TIMEOUT))
                        .await
                        .map_err(|err| err.to_string())?,
                )
            }
        };
        self.runtimes().insert(kind, runtime.clone());
        Ok(runtime)
    }

    /// 启动一个对话：Claude Code 建一个（懒启动的）进程对话；opencode / codex 复用或拉起共用进程，
    /// 在里面开一个会话（有历史标识就先试着接上）。**不发消息**，第一条消息由调用方发。
    ///
    /// 建好之后：`events` 里是驱动的流式事件；`next_prompt()` 取等确认的命令；`sync_hosts` 把选中的
    /// 主机放进 scope，模型才看得到这些主机。失败时 scope 和出口已经收回，不留残余。
    pub async fn start_session(
        self: &Arc<Self>,
        kind: AgentKind,
        install: AgentInstall,
        resume: Option<String>,
        overrides: StartOverrides,
    ) -> Result<AgentSession, String> {
        if kind == AgentKind::Builtin {
            return Err("ZeePilot 要用 start_builtin 启动".to_string()); // i18n-allow: 不可达的内部控制流断言（开发者信息），不进界面
        }
        let tools = ToolChannel::attach(&self.bridge, &self.approver, &self.reports);
        let (events_tx, events) = mpsc::unbounded_channel();
        // 假世界（调试版自动化）：不拉任何真实进程。
        if let Some(world) = fake::world() {
            let bridge = fake::FakeBridge {
                port: self.port,
                token: self.bridge.connection_token().to_owned(),
                capability: tools.scope().capability().to_owned(),
            };
            let (conversation, resumed) =
                world.start(kind, events_tx, resume.as_deref(), Some(bridge))?;
            return Ok(AgentSession {
                kind,
                conversation,
                tools,
                events,
                resumed,
                builtin: None,
            });
        }
        let capability = tools.scope().capability().to_string();
        let (conversation, resumed): (Arc<dyn AgentConversation>, bool) = match kind {
            // claude / codebuddy：一个对话一个进程（懒启动：第一次发消息才拉；空闲 10 分钟关，
            // 下次发消息 `--resume` 恢复上下文）。CodeBuddy 的命令行协议和 Claude Code
            // 同构（-p、stream-json、--allowedTools、--mcp-config 全部同名同义），共用适配器。
            AgentKind::Builtin => return Err("ZeePilot 要用 start_builtin 启动".to_string()), // i18n-allow: 不可达的内部控制流断言（开发者信息），不进界面
            AgentKind::Claude | AgentKind::Codebuddy => {
                self.require_mcp_binary()?;
                let options = start_options(
                    install,
                    &self.data_dir,
                    self.mcp_spec(),
                    capability,
                    events_tx,
                    resume.clone(),
                    overrides.extra_env.clone(),
                );
                let conversation = claude::ClaudeConversation::create(options, CLAUDE_IDLE_TIMEOUT)
                    .await
                    .map_err(|err| err.to_string())?;
                (conversation as Arc<dyn AgentConversation>, resume.is_some())
            }
            // opencode / codex：共用进程里的一个对话。进程意外没了之后，下一次发消息由
            // `ReopeningConversation` 用同一把密钥和会话标识重开（见 `reopen`）。
            AgentKind::OpenCode | AgentKind::Codex => {
                let reopen_events = events_tx.clone();
                let (conversation, resumed) = self
                    .open_shared_conversation(
                        kind,
                        install.clone(),
                        &overrides,
                        reopen::ReopenRequest {
                            capability: capability.clone(),
                            resume: resume.clone(),
                        },
                        tools.scope(),
                        events_tx,
                    )
                    .await?;
                let reopen = self.reopener(
                    kind,
                    install,
                    overrides,
                    tools.scope().clone(),
                    reopen_events.clone(),
                );
                (
                    ReopeningConversation::wrap(conversation, capability, reopen_events, reopen),
                    resumed,
                )
            }
        };
        Ok(AgentSession {
            kind,
            conversation,
            tools,
            events,
            resumed,
            builtin: None,
        })
    }

    /// 在 opencode / codex 的共用进程里开（或续接）一个对话：没有活着的进程就先拉一个。
    /// 第一次开对话和进程没了之后的重开走同一条路，密钥、续接标识都由调用方给。
    async fn open_shared_conversation(
        &self,
        kind: AgentKind,
        install: AgentInstall,
        overrides: &StartOverrides,
        request: reopen::ReopenRequest,
        scope: &ScopeHandle,
        events: mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<(Arc<dyn AgentConversation>, bool), String> {
        let reopen::ReopenRequest { capability, resume } = request;
        match kind {
            AgentKind::OpenCode => {
                let SharedRuntime::OpenCode(runtime) =
                    self.runtime_for(kind, install, overrides).await?
                else {
                    return Err("opencode 的共用进程类型不对".to_string()); // i18n-allow: 不可达的内部控制流断言（开发者信息），不进界面
                };
                let (conversation, resumed) = runtime
                    .open_conversation_resuming(capability, events, resume.as_deref())
                    .await
                    .map_err(|err| err.to_string())?;
                // 重开时也要绑上：opencode 每个对话的工具授权是 scope 派生的独立 session 密钥。
                conversation
                    .bind_tool_scope(scope.clone())
                    .map_err(|err| err.to_string())?;
                Ok((conversation as Arc<dyn AgentConversation>, resumed))
            }
            AgentKind::Codex => {
                let SharedRuntime::Codex(runtime) =
                    self.runtime_for(kind, install, overrides).await?
                else {
                    return Err("codex 的共用进程类型不对".to_string()); // i18n-allow: 不可达的内部控制流断言（开发者信息），不进界面
                };
                let (conversation, resumed) = runtime
                    .open_conversation_resuming(capability, events, resume.as_deref())
                    .await
                    .map_err(|err| err.to_string())?;
                Ok((conversation as Arc<dyn AgentConversation>, resumed))
            }
            AgentKind::Claude | AgentKind::Codebuddy | AgentKind::Builtin => {
                Err("这种智能体不使用共用进程".to_string()) // i18n-allow: 不可达的内部控制流断言（开发者信息），不进界面
            }
        }
    }

    /// 重开对话用的函数：向 hub 要一个活着的共用进程（`runtime_for` 会跳过已关闭的、必要时重新拉起），
    /// 在里面以请求里的密钥和续接标识开对话。scope 和事件通道都是第一次开对话时那一份。
    fn reopener(
        self: &Arc<Self>,
        kind: AgentKind,
        install: AgentInstall,
        overrides: StartOverrides,
        scope: ScopeHandle,
        events: mpsc::UnboundedSender<AgentEvent>,
    ) -> Reopen {
        let hub = Arc::clone(self);
        Arc::new(move |request| {
            let hub = Arc::clone(&hub);
            let install = install.clone();
            let overrides = overrides.clone();
            let scope = scope.clone();
            let events = events.clone();
            Box::pin(async move {
                hub.open_shared_conversation(kind, install, &overrides, request, &scope, events)
                    .await
            })
        })
    }

    /// 启动一个内置助手对话：不拉进程，对话循环在本进程里，工具调用直接走共用的工具桥
    /// （安全确认、执行模式、只读、审计都是桥的）。**不发消息**，第一条消息由调用方发。
    ///
    /// `events` 是和外部智能体同一套的 [`AgentEvent`]；用量 / 提示 / 失败走 `builtin.signals`。
    pub async fn start_builtin(
        self: &Arc<Self>,
        start: BuiltinStart,
    ) -> Result<AgentSession, String> {
        let BuiltinStart {
            endpoint,
            api_key,
            transport,
            catalog,
            resume,
            tuning,
        } = start;
        let tools = ToolChannel::attach(&self.bridge, &self.approver, &self.reports);
        let (events_tx, events) = mpsc::unbounded_channel();
        let (signals_tx, signals) = mpsc::unbounded_channel();
        let resumed = resume.is_some();
        let conversation = Arc::new(BuiltinConversation::new(BuiltinParams {
            endpoint,
            api_key,
            transport,
            catalog,
            bridge: Arc::clone(&self.bridge),
            scope: tools.scope().clone(),
            events: events_tx,
            signals: Some(signals_tx),
            data_dir: Some(self.data_dir.clone()),
            resume_id: resume,
            tuning,
        }));
        Ok(AgentSession {
            kind: AgentKind::Builtin,
            conversation: Arc::clone(&conversation) as Arc<dyn AgentConversation>,
            tools,
            events,
            resumed,
            builtin: Some(BuiltinLink { signals }),
        })
    }

    /// 不开对话直接查某个智能体的模型目录（面板一打开 / 切换智能体时显示默认模型用）。
    ///
    /// opencode / codex 要用共用进程：没在跑就照常启动一个，之后没人用会按空闲超时自己关掉，
    /// 不常驻。Claude Code 不拉对话进程，只跑一次 `--help` 探测参数。
    pub async fn catalog(
        &self,
        kind: AgentKind,
        install: AgentInstall,
        overrides: &StartOverrides,
    ) -> Result<AgentModelCatalog, String> {
        if kind == AgentKind::Claude {
            return Ok(claude::catalog_for_install(&install.path).await);
        }
        self.runtime_for(kind, install, overrides)
            .await?
            .models()
            .await
    }

    /// 关掉所有共用进程（应用退出时）。对话各自的收尾由持有它们的窗口做。
    pub async fn shutdown_all(&self) {
        let runtimes: Vec<SharedRuntime> = self.runtimes().drain().map(|(_, rt)| rt).collect();
        for runtime in runtimes {
            runtime.shutdown().await;
        }
    }
}

static HUB: tokio::sync::OnceCell<Arc<AgentHub>> = tokio::sync::OnceCell::const_new();

/// 全进程那一个宿主，第一次用到时建。
async fn global_hub() -> Result<Arc<AgentHub>, String> {
    HUB.get_or_try_init(|| async {
        // 假世界里没有真实进程要连桥，`zssh-mcp.exe` 在不在无所谓。
        // 外部智能体启动时才检查 `zssh-mcp.exe` 在不在（见 `require_mcp_binary`）：
        // 只用内置助手的人不需要它。
        let mcp = mcp_binary_location()?;
        AgentHub::production(crate::settings::data_dir(), mcp).await
    })
    .await
    .cloned()
}

/// 启动一个对话（产品入口）。
pub async fn start_agent(
    kind: AgentKind,
    install: AgentInstall,
    resume: Option<String>,
) -> Result<AgentSession, String> {
    global_hub()
        .await?
        .start_session(kind, install, resume, StartOverrides::default())
        .await
}

/// 启动一个内置助手对话（产品入口）：读接口配置和 Key，按"软件网络"的代理建传输层，
/// 加载模型资料库。`endpoint_id` 是设置里的接口。
pub async fn start_builtin_agent(
    endpoint_id: i64,
    resume: Option<String>,
) -> Result<AgentSession, String> {
    let (endpoint, api_key) = load_endpoint(endpoint_id)?;
    let transport = crate::agent_setup::builtin::make_transport()?;
    let catalog = crate::agent_setup::builtin::catalog();
    global_hub()
        .await?
        .start_builtin(BuiltinStart {
            endpoint,
            api_key,
            transport,
            catalog,
            resume,
            tuning: BuiltinTuning::default(),
        })
        .await
}

/// 读接口配置和它的 Key（Key 只在这里短暂出现，交给对话后由 `ApiKey` 的 `Debug` 挡住）。
pub(crate) fn load_endpoint(endpoint_id: i64) -> Result<(AiEndpoint, ApiKey), String> {
    let db = crate::host_db::open_database().map_err(|err| {
        format!(
            "{}{}",
            t(AgentHostMsg::OpenSettingsDbFailedPrefix),
            crate::error_text::error_text(&err)
        )
    })?;
    load_endpoint_from(&db, endpoint_id)
}

/// 同 [`load_endpoint`]，用已经打开的库。
pub(crate) fn load_endpoint_from(
    db: &zssh_core::store::Database,
    endpoint_id: i64,
) -> Result<(AiEndpoint, ApiKey), String> {
    let endpoint = db
        .ai_endpoint_get(endpoint_id)
        .map_err(|_| t(AgentHostMsg::EndpointMissing).to_string())?;
    let key = db.ai_endpoint_api_key(endpoint_id).map_err(|err| {
        format!(
            "{}{}",
            t(AgentHostMsg::ReadApiKeyFailedPrefix),
            crate::error_text::error_text(&err)
        )
    })?;
    let key = match key {
        Some(key) => key,
        // 明确设成"不需要 Key"的接口（Ollama 之类）：Key 为空，请求里就不带 Authorization 头。
        None if endpoint.key_not_needed() => String::new(),
        None => {
            return Err(format!(
                "{}{}{}",
                t(AgentHostMsg::MissingApiKeyPrefix),
                endpoint.name,
                t(AgentHostMsg::MissingApiKeySuffix)
            ));
        }
    };
    Ok((endpoint, ApiKey::new(key)))
}

/// 取某个安装的模型目录（产品入口）。
pub async fn fetch_catalog(
    kind: AgentKind,
    install: AgentInstall,
) -> Result<AgentModelCatalog, String> {
    if let Some(world) = fake::world() {
        return world.catalog(kind);
    }
    // Claude Code 不需要桥，没必要为了取目录建桥。
    if kind == AgentKind::Claude {
        return Ok(claude::catalog_for_install(&install.path).await);
    }
    global_hub()
        .await?
        .catalog(kind, install, &StartOverrides::default())
        .await
}

/// codex 共用进程还活着时直接问它登没登录（最权威）；没有活着的返回 `None`，交给别的办法判断。
pub async fn live_codex_logged_in() -> Option<Result<bool, String>> {
    let hub = HUB.get()?;
    let SharedRuntime::Codex(runtime) = hub.live_runtime(AgentKind::Codex)? else {
        return None;
    };
    Some(
        runtime
            .account_logged_in()
            .await
            .map_err(|err| err.to_string()),
    )
}

/// 应用退出：把共用进程收掉（子进程另外绑在 ZeeTerm 的作业上，崩溃也不会残留）。
pub fn shutdown_global() {
    let Some(hub) = HUB.get() else {
        return;
    };
    let Ok(runtime) = crate::ssh_backend::runtime() else {
        return;
    };
    runtime.block_on(async {
        let _ = tokio::time::timeout(Duration::from_secs(5), hub.shutdown_all()).await;
    });
}

/// 空闲关闭时间。产品里是 core 的默认值（10 分钟）；调试版可以用 `ZSSH_AGENT_IDLE_SECS` 调小。
fn idle_timeout(fallback: Duration) -> Duration {
    crate::dev_hooks::var("ZSSH_AGENT_IDLE_SECS")
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .filter(|duration| !duration.is_zero())
        .unwrap_or(fallback)
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use zssh_core::ai::safety::DangerLevel;

    fn sample_request() -> ApprovalRequest {
        ApprovalRequest {
            scope: "0123456789abcdef".to_string(),
            exec_id: "3f2a1b-1".to_string(),
            target_id: "host-1".to_string(),
            session_id: None,
            host_label: "web-1".to_string(),
            command: "uptime".to_string(),
            danger: DangerLevel::Safe,
            reasons: Vec::new(),
            file: None,
        }
    }

    fn ask(approver: &Arc<NativeApprover>) -> std::thread::JoinHandle<ApprovalDecision> {
        let approver = Arc::clone(approver);
        std::thread::spawn(move || futures::executor::block_on(approver.approve(sample_request())))
    }

    #[test]
    fn telnet_and_serial_hosts_are_refused_as_ai_targets_but_ssh_hosts_are_not() {
        use zssh_core::model::{AuthMethod, HostDraft, Protocol, ProtocolSettings, ProxyMode};

        let _dir_guard = tempfile::Builder::new()
            .prefix("zssh-agent-host-refusal-")
            .tempdir()
            .expect("临时目录");
        let dir = _dir_guard.path().to_path_buf();
        let db = Database::open(&dir.join("zssh.db")).unwrap();
        let draft = |name: &str| HostDraft {
            protocol: None,
            id: None,
            group_id: None,
            name: name.to_owned(),
            host: "example.internal".to_owned(),
            port: 22,
            username: "root".to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: None,
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: ProxyMode::None,
            tags: Vec::new(),
            sort: 0,
            legacy_algorithms: false,
        };
        let ssh = db.host_save(&draft("web-1")).unwrap();
        let (telnet, _) = db
            .host_save_with_protocol(&draft("switch-1"), &ProtocolSettings::new(Protocol::Telnet))
            .unwrap();
        let mut serial_settings = ProtocolSettings::new(Protocol::Serial);
        serial_settings.serial.as_mut().unwrap().device = "COM3".to_owned();
        let (serial, _) = db
            .host_save_with_protocol(&draft("console-1"), &serial_settings)
            .unwrap();
        drop(db);

        let host = NativeBridgeHost::with(Arc::new(ReportRoutes::default()), dir.clone());
        let refusal =
            |id: i64| futures::executor::block_on(host.target_refusal(&host_target_id(id)));
        assert_eq!(refusal(ssh.id), None);
        assert!(refusal(telnet.id).unwrap().contains("Telnet"));
        assert!(refusal(serial.id).unwrap().contains("串口"));
        // 不在主机库里的目标（快速连接的会话编号）放行。
        assert_eq!(
            futures::executor::block_on(host.target_refusal("sess-1")),
            None
        );
    }

    #[test]
    fn host_target_parsing_accepts_only_host_ids() {
        assert_eq!(parse_host_target("host-12").ok(), Some(12));
        for bad in ["ses_x", "host-", "host-abc", ""] {
            assert!(parse_host_target(bad).is_err(), "{bad} 不该被当成主机目标");
        }
    }

    fn sample_install() -> AgentInstall {
        AgentInstall {
            path: "claude.exe".to_string(),
            version: "2.0.0".to_string(),
            source: zssh_core::ai::agent::discovery::DiscoverySource::Path,
            needs_shell: false,
            auto: true,
            too_old: false,
            skipped_older: None,
        }
    }

    #[test]
    fn start_options_forward_the_resume_session_id() {
        let session_id = "11111111-2222-3333-4444-555555555555";
        let (events, _events_rx) = mpsc::unbounded_channel();
        let options = start_options(
            sample_install(),
            Path::new("data"),
            McpServerSpec::zssh(Path::new("zssh-mcp.exe"), 1234, "token"),
            "capability".to_string(),
            events,
            Some(session_id.to_string()),
            Vec::new(),
        );
        assert_eq!(
            options.resume_session_id.as_deref(),
            Some(session_id),
            "续接 id 要原样交给驱动"
        );
        assert_eq!(
            options.workspace,
            Path::new("data").join("agent-workspace"),
            "工作目录仍然在数据目录下面"
        );

        // 没有历史会话时保持 None，不能凭空续接。
        let (events, _events_rx) = mpsc::unbounded_channel();
        let options = start_options(
            sample_install(),
            Path::new("data"),
            McpServerSpec::zssh(Path::new("zssh-mcp.exe"), 1234, "token"),
            "capability".to_string(),
            events,
            None,
            Vec::new(),
        );
        assert!(options.resume_session_id.is_none());
    }

    #[test]
    fn exec_rejects_non_host_target_without_connecting() {
        let host = NativeBridgeHost::new();
        let result =
            futures::executor::block_on(host.exec("ses_x", "uptime", Duration::from_secs(5), 1024));
        let err = result.expect_err("非主机目标要报错");
        assert!(err.contains("host-<id>"), "错误要说清只支持主机目标：{err}");
        assert!(err.contains("ses_x"), "错误要带上原始目标：{err}");
    }

    #[test]
    fn approver_resolves_allow_and_deny() {
        let (prompts, prompts_rx) = std::sync::mpsc::channel::<ApprovalPrompt>();
        let approver = Arc::new(NativeApprover::new(prompts));

        let asker = ask(&approver);
        let prompt = prompts_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("窗口要收到审批请求");
        assert_eq!(prompt.request.command, "uptime");
        assert!(approver.resolve(prompt.id, true));
        assert_eq!(
            asker.join().expect("审批线程"),
            ApprovalDecision::Allowed,
            "点允许之后审批要通过"
        );
        // 同一个 id 不能答第二次；没登记过的 id 也不认。
        assert!(!approver.resolve(prompt.id, false));
        assert!(!approver.resolve(9_999, true));

        let asker = ask(&approver);
        let prompt = prompts_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("窗口要收到审批请求");
        assert!(approver.resolve(prompt.id, false));
        assert_eq!(
            asker.join().expect("审批线程"),
            ApprovalDecision::Denied,
            "点拒绝之后审批要拒绝"
        );
    }

    #[test]
    fn approver_denies_when_window_is_gone() {
        let (prompts, prompts_rx) = std::sync::mpsc::channel::<ApprovalPrompt>();
        // 窗口已经关了：请求送不出去，必须马上按拒绝结束，不能挂住。
        drop(prompts_rx);
        let approver = NativeApprover::new(prompts);
        let decision = futures::executor::block_on(approver.approve(sample_request()));
        assert_eq!(decision, ApprovalDecision::Denied);
    }

    #[test]
    fn mode_reads_settings_each_time() {
        let db = Database::open_in_memory().expect("内存库");
        assert_eq!(NativeMode::from_db(&db), ExecMode::Manual, "默认是手动");

        db.set_ai_exec_mode("manual").expect("设置 manual");
        assert_eq!(NativeMode::from_db(&db), ExecMode::Manual);

        db.set_ai_exec_mode("readonly").expect("设置 readonly");
        assert_eq!(NativeMode::from_db(&db), ExecMode::ReadOnly);

        db.set_ai_exec_mode("auto").expect("设置 auto");
        assert_eq!(NativeMode::from_db(&db), ExecMode::Auto);

        // 库里的值不认识（旧版本 / 手改）：按手动处理。
        db.setting_set(zssh_core::store::settings::AI_EXEC_MODE_KEY, "42")
            .expect("写个坏值");
        assert_eq!(NativeMode::from_db(&db), ExecMode::Manual);
    }

    #[test]
    fn mode_falls_back_to_manual_when_db_is_unavailable() {
        let _dir_guard = tempfile::Builder::new()
            .prefix("zssh-agent-host-mode-")
            .tempdir()
            .expect("临时目录");
        let dir = _dir_guard.path().to_path_buf();
        // 拿一个普通文件当"数据目录"：它下面拼出来的 zssh.db 一定打不开。
        let blocker = dir.join("not-a-dir");
        std::fs::write(&blocker, b"x").expect("写占位文件");
        let mode = NativeMode::new(&blocker);
        assert_eq!(mode.mode(), ExecMode::Manual);
    }

    // -----------------------------------------------------------------------
    // 工具桥一侧：停止、目标增减、启动期取消
    // -----------------------------------------------------------------------

    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    /// 记下"真的执行了几次"的宿主：停止之后这个数必须还是 0。
    #[derive(Default)]
    struct CountingHost {
        execs: AtomicUsize,
    }

    fn captured() -> ExecCapture {
        ExecCapture {
            stdout: "ok\n".into(),
            stderr: String::new(),
            stdout_truncated_bytes: 0,
            stderr_truncated_bytes: 0,
            exit_code: Some(0),
            timed_out: false,
            cancelled: false,
            duration_ms: 1,
        }
    }

    impl BridgeHost for CountingHost {
        fn exec<'a>(
            &'a self,
            _target_id: &'a str,
            _command: &'a str,
            _wait: Duration,
            _max_stream_bytes: usize,
        ) -> BoxFuture<'a, Result<ExecCapture, String>> {
            self.execs.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(captured()) })
        }

        fn exec_cancellable<'a>(
            &'a self,
            _target_id: &'a str,
            _command: &'a str,
            _wait: Duration,
            _max_stream_bytes: usize,
            _gate: &'a ExecGate,
        ) -> BoxFuture<'a, Result<ExecCapture, String>> {
            self.execs.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(captured()) })
        }

        fn read_terminal<'a>(
            &'a self,
            _session_id: &'a str,
            _max_lines: usize,
        ) -> BoxFuture<'a, Result<Vec<String>, String>> {
            Box::pin(async { Err("不支持".to_string()) })
        }

        fn remote_fs<'a>(
            &'a self,
            _target_id: &'a str,
        ) -> BoxFuture<'a, Result<Arc<dyn RemoteFs>, String>> {
            Box::pin(async { Err("不支持".to_string()) })
        }
    }

    struct NoAudit;
    impl AuditLog for NoAudit {
        fn record(&self, _entry: AuditEntry) {}
    }

    struct FixedMode(ExecMode);
    impl ExecModeProvider for FixedMode {
        fn mode(&self) -> ExecMode {
            self.0
        }
    }

    fn web_host(id: i64) -> HostTarget {
        HostTarget::plain(id, format!("web-{id}"), "root")
    }

    fn manual_channel(host: Arc<CountingHost>) -> ToolChannel {
        ToolChannel::new(
            host,
            Arc::new(NoAudit),
            Arc::new(FixedMode(ExecMode::Manual)),
        )
    }

    /// 等到有一条还有效的确认被取出来。
    async fn wait_for_prompt(tools: &ToolChannel) -> ApprovalPrompt {
        for _ in 0..250 {
            if let Some(prompt) = tools.next_prompt() {
                return prompt;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("5 秒内没有等到确认请求");
    }

    /// 连上桥、发一条 `run_command`，返回读响应的一端（写端留着，免得桥以为连接断了）。
    async fn send_run_command(
        tools: &ToolChannel,
        command: &str,
    ) -> (
        tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>,
        tokio::net::tcp::OwnedWriteHalf,
    ) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("绑定端口");
        let port = listener.local_addr().expect("端口").port();
        tokio::spawn(Arc::clone(tools.bridge()).serve(listener));
        let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("连上桥");
        let (reader, mut writer) = stream.into_split();
        let hello = serde_json::json!({ "token": tools.bridge().connection_token() });
        let request = serde_json::json!({
            "id": 1,
            "method": "run_command",
            "params": { "command": command, "target_id": "host-1" },
            "capability": tools.scope().capability(),
        });
        let payload = format!("{hello}\n{request}\n");
        writer.write_all(payload.as_bytes()).await.expect("写请求");
        writer.flush().await.expect("flush");
        (tokio::io::BufReader::new(reader), writer)
    }

    #[tokio::test]
    async fn stop_cancels_a_pending_request_through_the_bridge_and_nothing_runs() {
        // 审查 H1：旧窗口只改界面状态，bridge 的 stop_scope 从没被调用过。
        let host = Arc::new(CountingHost::default());
        let tools = manual_channel(Arc::clone(&host));
        tools.sync_hosts(&[web_host(1)]);
        let (mut reader, _writer) = send_run_command(&tools, "uptime").await;

        let prompt = wait_for_prompt(&tools).await;
        assert_eq!(prompt.request.command, "uptime");
        assert!(tools.approval_is_live(prompt.id));

        tools.stop();

        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .expect("停止后工具请求要马上结束，不能等审批超时")
            .expect("读响应");
        assert!(
            line.contains("停止") || line.contains("拒绝"),
            "要告诉模型这次被拒绝/停止了：{line}"
        );
        assert_eq!(
            host.execs.load(Ordering::SeqCst),
            0,
            "停止之后不能再执行命令"
        );
        assert!(
            !tools.approval_is_live(prompt.id),
            "挂着的确认要被摘掉，不能再点"
        );
        assert!(
            !tools.resolve_approval(prompt.id, true),
            "停止之后再点允许也不会放行"
        );
    }

    #[tokio::test]
    async fn prompts_queued_before_a_stop_never_come_back_as_cards() {
        let host = Arc::new(CountingHost::default());
        let tools = manual_channel(host);
        // 两条确认已经进了窗口的队列，但界面还没取走。
        let mut waiters = Vec::new();
        for _ in 0..2 {
            let approver = Arc::clone(&tools.approver);
            // 请求带着真实桥发的 scope id：停止只结束这个对话自己的。
            let mut request = sample_request();
            request.scope = tools.scope().scope_id().to_owned();
            waiters.push(tokio::spawn(async move { approver.approve(request).await }));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;

        let dropped = tools.stop();
        assert_eq!(dropped, 2, "排队的两条要被丢掉");
        assert!(tools.next_prompt().is_none(), "停止之后不能再冒出卡片");
        for waiter in waiters {
            let decision = tokio::time::timeout(Duration::from_secs(5), waiter)
                .await
                .expect("等的一端要马上结束")
                .expect("任务不 panic");
            assert_eq!(decision, ApprovalDecision::Denied);
        }
    }

    #[tokio::test]
    async fn next_prompt_skips_requests_that_are_no_longer_live() {
        let host = Arc::new(CountingHost::default());
        let tools = manual_channel(host);
        let approver = Arc::clone(&tools.approver);
        let first = tokio::spawn(async move { approver.approve(sample_request()).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        // 模拟超时 / scope 销毁：等的一端被摘掉，队列里的提示还在。
        tools.approver.deny_all();
        first.await.expect("任务不 panic");
        assert!(tools.next_prompt().is_none(), "已失效的请求不给界面");

        let approver = Arc::clone(&tools.approver);
        let second = tokio::spawn(async move { approver.approve(sample_request()).await });
        let prompt = wait_for_prompt(&tools).await;
        assert!(tools.resolve_approval(prompt.id, true));
        assert_eq!(
            second.await.expect("任务不 panic"),
            ApprovalDecision::Allowed
        );
    }

    #[test]
    fn sync_hosts_adds_and_removes_only_host_targets() {
        let tools = manual_channel(Arc::new(CountingHost::default()));
        // 一个不是 host-<id> 的目标（比如以后绑定的终端会话）不归主机选择管。
        tools
            .scope()
            .add_target("ses_x", "term", "root", "connected", true, false);

        let ids = |tools: &ToolChannel| {
            let mut ids: Vec<String> = tools.scope().targets().into_iter().map(|t| t.id).collect();
            ids.sort();
            ids
        };

        tools.sync_hosts(&[]);
        assert_eq!(ids(&tools), vec!["ses_x"], "默认不带任何主机");

        tools.sync_hosts(&[web_host(1), web_host(2)]);
        assert_eq!(ids(&tools), vec!["host-1", "host-2", "ses_x"]);

        tools.sync_hosts(&[web_host(2)]);
        assert_eq!(ids(&tools), vec!["host-2", "ses_x"], "取消选择要真的摘掉");

        tools.sync_hosts(&[]);
        assert_eq!(ids(&tools), vec!["ses_x"]);
    }

    #[test]
    fn a_quick_connect_terminal_can_be_the_conversation_target() {
        // V2-U15：快速连接没有主机记录，对话锚定在终端本身（`term-<n>`）。
        let tools = manual_channel(Arc::new(CountingHost::default()));
        let ids = |tools: &ToolChannel| {
            let mut ids: Vec<String> = tools.scope().targets().into_iter().map(|t| t.id).collect();
            ids.sort();
            ids
        };
        let quick = HostTarget {
            id: 0,
            name: "ops@quick".to_owned(),
            username: String::new(),
            current: true,
            pinned: false,
            refusal: None,
            terminal: Some("term-7".to_owned()),
        };
        assert_eq!(quick.scope_id(), "term-7");
        assert_eq!(web_host(3).scope_id(), "host-3");
        tools.sync_hosts(&[quick.clone(), web_host(2)]);
        assert_eq!(ids(&tools), vec!["host-2", "term-7"]);
        let summary = tools
            .scope()
            .targets()
            .into_iter()
            .find(|target| target.id == "term-7")
            .expect("终端目标在 scope 里");
        assert!(summary.current && summary.host_label == "ops@quick");
        // 终端关了（不再被选）：摘掉；主机目标不受影响。
        tools.sync_hosts(&[web_host(2)]);
        assert_eq!(ids(&tools), vec!["host-2"]);
    }

    #[tokio::test]
    async fn removing_a_host_target_invalidates_its_pending_approval() {
        let host = Arc::new(CountingHost::default());
        let tools = manual_channel(Arc::clone(&host));
        tools.sync_hosts(&[web_host(1)]);
        let (mut reader, _writer) = send_run_command(&tools, "uptime").await;
        let _prompt = wait_for_prompt(&tools).await;

        tools.sync_hosts(&[]);

        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .expect("目标被取消选择后，等审批的请求要结束")
            .expect("读响应");
        assert!(line.contains("移出") || line.contains("取消"), "{line}");
        assert_eq!(host.execs.load(Ordering::SeqCst), 0);
    }

    /// 只记发送次数的驱动。
    #[derive(Default)]
    struct CountingConversation {
        sent: AtomicUsize,
        shutdowns: AtomicUsize,
    }

    type AgentResult<T> = Result<T, zssh_core::ai::agent::AgentError>;

    impl AgentConversation for CountingConversation {
        fn session_id(&self) -> &str {
            "test-session"
        }

        fn models<'a>(
            &'a self,
        ) -> BoxFuture<'a, AgentResult<zssh_core::ai::agent::AgentModelCatalog>> {
            Box::pin(async { Ok(zssh_core::ai::agent::AgentModelCatalog::default()) })
        }

        fn send<'a>(
            &'a self,
            _text: &'a str,
            _model: Option<&'a str>,
        ) -> BoxFuture<'a, AgentResult<()>> {
            self.sent.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }

        fn stop<'a>(&'a self) -> BoxFuture<'a, AgentResult<()>> {
            Box::pin(async { Ok(()) })
        }

        fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }
    }

    #[tokio::test]
    async fn first_message_is_not_sent_after_a_stop_during_startup() {
        // 审查 M4：启动中点停止，旧实现照样把第一条消息发给模型。
        let conversation = CountingConversation::default();
        let cancel = AtomicBool::new(false);
        assert_eq!(
            send_unless_cancelled(&conversation, &cancel, "hi", None).await,
            Ok(true)
        );
        assert_eq!(conversation.sent.load(Ordering::SeqCst), 1);

        cancel.store(true, Ordering::SeqCst);
        assert_eq!(
            send_unless_cancelled(&conversation, &cancel, "hi", None).await,
            Ok(false)
        );
        assert_eq!(
            conversation.sent.load(Ordering::SeqCst),
            1,
            "取消之后不能再发"
        );
    }

    // ---- 共用桥：一个桥、多个对话，确认请求按 scope 分路 ----

    /// 数据目录用随机唯一的临时目录：内置智能体的对话记录会写到 `<数据目录>/ai-history`，
    /// 直接用系统临时目录会把记录留在那里。返回的 `TempDir` 排在前面，用例里先放掉桥、再删目录。
    async fn test_hub(host: Arc<CountingHost>) -> (tempfile::TempDir, Arc<AgentHub>) {
        let data_dir = tempfile::Builder::new()
            .prefix("zssh-agent-hub-")
            .tempdir()
            .expect("临时数据目录");
        let hub = AgentHub::with_parts(
            host,
            Arc::new(NoAudit),
            Arc::new(FixedMode(ExecMode::Manual)),
            data_dir.path().to_path_buf(),
            PathBuf::from("zssh-mcp.exe"),
        )
        .await
        .expect("建共用桥");
        (data_dir, hub)
    }

    /// 连上共用桥、用 `capability` 发一条 `run_command`。
    async fn ask_through(
        hub: &AgentHub,
        tools: &ToolChannel,
        command: &str,
    ) -> (
        tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>,
        tokio::net::tcp::OwnedWriteHalf,
    ) {
        let stream = tokio::net::TcpStream::connect(("127.0.0.1", hub.port))
            .await
            .expect("连上桥");
        let (reader, mut writer) = stream.into_split();
        let hello = serde_json::json!({ "token": hub.bridge.connection_token() });
        let request = serde_json::json!({
            "id": 1,
            "method": "run_command",
            "params": { "command": command, "target_id": "host-1" },
            "capability": tools.scope().capability(),
        });
        let payload = format!("{hello}\n{request}\n");
        writer.write_all(payload.as_bytes()).await.expect("写请求");
        writer.flush().await.expect("flush");
        (tokio::io::BufReader::new(reader), writer)
    }

    #[tokio::test]
    async fn one_bridge_routes_each_prompt_to_the_conversation_that_asked() {
        let host = Arc::new(CountingHost::default());
        let (_data_dir, hub) = test_hub(Arc::clone(&host)).await;
        let a = ToolChannel::attach(&hub.bridge, &hub.approver, &hub.reports);
        let b = ToolChannel::attach(&hub.bridge, &hub.approver, &hub.reports);
        a.sync_hosts(&[web_host(1)]);
        b.sync_hosts(&[web_host(1)]);
        assert_ne!(a.scope().scope_id(), b.scope().scope_id());

        let (mut reader_a, _writer_a) = ask_through(&hub, &a, "uptime").await;
        let prompt = wait_for_prompt(&a).await;
        assert_eq!(prompt.request.scope, a.scope().scope_id());
        assert!(b.next_prompt().is_none(), "别的对话不该收到这条确认");

        // B 点了停止：只结束 B 自己的，A 还在等它的用户。
        b.stop();
        assert!(a.approval_is_live(prompt.id), "B 的停止不能动 A 的确认");

        // A 允许：命令在共用的桥上跑起来。
        assert!(a.resolve_approval(prompt.id, true));
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader_a.read_line(&mut line))
            .await
            .expect("等响应")
            .expect("读响应");
        assert!(line.contains("ok"), "{line}");
        assert_eq!(host.execs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stopping_one_conversation_leaves_the_others_prompt_waiting() {
        let host = Arc::new(CountingHost::default());
        let (_data_dir, hub) = test_hub(Arc::clone(&host)).await;
        let a = ToolChannel::attach(&hub.bridge, &hub.approver, &hub.reports);
        let b = ToolChannel::attach(&hub.bridge, &hub.approver, &hub.reports);
        a.sync_hosts(&[web_host(1)]);
        b.sync_hosts(&[web_host(1)]);
        let (_ra, _wa) = ask_through(&hub, &a, "uptime").await;
        let (mut reader_b, _wb) = ask_through(&hub, &b, "df -h").await;
        let prompt_a = wait_for_prompt(&a).await;
        let prompt_b = wait_for_prompt(&b).await;

        a.stop();
        assert!(!a.approval_is_live(prompt_a.id), "A 停止后它的确认失效");
        assert!(b.approval_is_live(prompt_b.id), "B 的确认不受影响");
        assert!(b.resolve_approval(prompt_b.id, false));
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader_b.read_line(&mut line))
            .await
            .expect("等响应")
            .expect("读响应");
        assert!(line.contains("拒绝") || line.contains("deny"), "{line}");
        assert_eq!(host.execs.load(Ordering::SeqCst), 0, "两条都没有执行");
    }

    #[tokio::test]
    async fn a_closed_conversation_has_no_route_and_asks_are_denied() {
        let (_data_dir, hub) = test_hub(Arc::new(CountingHost::default())).await;
        let a = ToolChannel::attach(&hub.bridge, &hub.approver, &hub.reports);
        let scope_id = a.scope().scope_id().to_owned();
        a.close();
        let mut request = sample_request();
        request.scope = scope_id;
        let decision = hub.approver.approve(request).await;
        assert_eq!(
            decision,
            ApprovalDecision::Denied,
            "出口摘掉后再来的请求直接拒绝"
        );
        // 从来没登记过的 scope 同样拒绝（共用桥没有默认出口）。
        assert_eq!(
            hub.approver.approve(sample_request()).await,
            ApprovalDecision::Denied
        );
    }

    #[test]
    fn deny_scope_only_ends_the_requests_of_that_scope() {
        let approver = Arc::new(NativeApprover::routed());
        let (tx_a, rx_a) = std::sync::mpsc::channel::<ApprovalPrompt>();
        let (tx_b, rx_b) = std::sync::mpsc::channel::<ApprovalPrompt>();
        approver.attach_scope("scope-a", tx_a);
        approver.attach_scope("scope-b", tx_b);
        let ask_in = |scope: &str| {
            let approver = Arc::clone(&approver);
            let mut request = sample_request();
            request.scope = scope.to_owned();
            std::thread::spawn(move || futures::executor::block_on(approver.approve(request)))
        };
        let asker_a = ask_in("scope-a");
        let asker_b = ask_in("scope-b");
        let prompt_a = rx_a.recv_timeout(Duration::from_secs(5)).expect("A 收到");
        let prompt_b = rx_b.recv_timeout(Duration::from_secs(5)).expect("B 收到");
        assert_eq!(prompt_a.request.scope, "scope-a");
        assert_eq!(prompt_b.request.scope, "scope-b");

        assert_eq!(approver.deny_scope("scope-a"), 1);
        assert_eq!(asker_a.join().expect("A 线程"), ApprovalDecision::Denied);
        assert!(approver.is_live(prompt_b.id), "B 还在等");
        assert!(approver.resolve(prompt_b.id, true));
        assert_eq!(asker_b.join().expect("B 线程"), ApprovalDecision::Allowed);
    }

    #[tokio::test]
    async fn shutdown_ends_tool_requests_then_the_driver_and_the_route() {
        // 审查 M3：窗口关闭要走显式 shutdown（先作废在途工具请求，再关驱动、摘掉分路），
        // 不能只靠 Drop。
        let conversation = Arc::new(CountingConversation::default());
        let host = Arc::new(CountingHost::default());
        let (_data_dir, hub) = test_hub(Arc::clone(&host)).await;
        let tools = ToolChannel::attach(&hub.bridge, &hub.approver, &hub.reports);
        tools.sync_hosts(&[web_host(1)]);
        let scope_id = tools.scope().scope_id().to_owned();
        let (mut reader, _writer) = ask_through(&hub, &tools, "uptime").await;
        let _prompt = wait_for_prompt(&tools).await;

        let (_events_tx, events) = mpsc::unbounded_channel();
        let mut session = AgentSession {
            kind: AgentKind::Codex,
            conversation: Arc::clone(&conversation) as Arc<dyn AgentConversation>,
            tools,
            events,
            resumed: false,
            builtin: None,
        };
        session.shutdown().await;

        assert_eq!(conversation.shutdowns.load(Ordering::SeqCst), 1);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .expect("关会话时挂着的工具请求要立刻结束")
            .expect("读响应");
        assert!(line.contains("停止") || line.contains("拒绝"), "{line}");
        assert_eq!(host.execs.load(Ordering::SeqCst), 0);
        assert!(
            hub.approver.route_of(&scope_id).is_none(),
            "关掉之后桥上不再有这个对话的出口"
        );
    }

    // ---- 内置助手（假接口，不联网、不拉任何进程）----

    fn fake_endpoint(kind: zssh_core::ai::agent::builtin::config::EndpointKind) -> AiEndpoint {
        AiEndpoint {
            id: 1,
            uid: "uid-e2e".into(),
            name: "测试接口".into(),
            kind,
            base_url: "https://api.example.com/v1".into(),
            key_identity_id: None,
            has_key: true,
            default_model: Some("gpt-5.5".into()),
            models: Vec::new(),
            overrides: Default::default(),
            openai_options: Default::default(),
            preset_id: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn fast_tuning() -> BuiltinTuning {
        BuiltinTuning {
            retry_base_delay: Duration::from_millis(10),
            ..BuiltinTuning::default()
        }
    }

    /// 收事件直到这一回合结束，返回（事件，旁路信号）。
    async fn collect_turn(session: &mut AgentSession) -> (Vec<AgentEvent>, Vec<BuiltinSignal>) {
        let mut events = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let event = tokio::time::timeout_at(deadline, session.events.recv())
                .await
                .expect("10 秒内回合要结束")
                .expect("事件通道没关");
            let done = matches!(event, AgentEvent::TurnEnd);
            events.push(event);
            if done {
                break;
            }
        }
        let mut signals = Vec::new();
        if let Some(link) = session.builtin.as_mut() {
            while let Ok(signal) = link.signals.try_recv() {
                signals.push(signal);
            }
        }
        (events, signals)
    }

    #[tokio::test]
    async fn a_builtin_session_calls_a_tool_through_the_shared_bridge_and_reports_usage() {
        use crate::agent_setup::builtin_fake::FakeTransport;
        use zssh_core::ai::agent::builtin::config::EndpointKind;
        let host = Arc::new(CountingHost::default());
        let _dir_guard = tempfile::Builder::new()
            .prefix("zssh-builtin-e2e-")
            .tempdir()
            .expect("临时目录");
        let dir = _dir_guard.path().to_path_buf();
        let hub = AgentHub::with_parts(
            host,
            Arc::new(NoAudit),
            Arc::new(FixedMode(ExecMode::Manual)),
            dir.clone(),
            PathBuf::from("zssh-mcp.exe"),
        )
        .await
        .expect("建共用桥");
        let fake = Arc::new(FakeTransport::default());
        let mut session = hub
            .start_builtin(BuiltinStart {
                endpoint: fake_endpoint(EndpointKind::OpenAiChat),
                api_key: ApiKey::new("sk-test-e2e"),
                transport: Arc::clone(&fake) as Arc<dyn Transport>,
                catalog: Arc::new(Catalog::builtin()),
                resume: None,
                tuning: fast_tuning(),
            })
            .await
            .expect("启动 ZeePilot");
        assert_eq!(session.kind, AgentKind::Builtin);
        assert!(!session.resumed);
        assert!(session.builtin.is_some());
        session.sync_hosts(&[web_host(1)]);

        // 第一条：模型先调 list_targets（只读、不需要确认），工具结果回填后给结论。
        session
            .conversation
            .send("tool:list", Some("gpt-5.5"))
            .await
            .expect("发送");
        let (events, signals) = collect_turn(&mut session).await;
        let started = events.iter().find_map(|e| match e {
            AgentEvent::ToolStart { tool, .. } => Some(tool.clone()),
            _ => None,
        });
        assert_eq!(started.as_deref(), Some("list_targets"));
        let ended = events.iter().find_map(|e| match e {
            AgentEvent::ToolEnd { output, error, .. } => Some((output.clone(), *error)),
            _ => None,
        });
        let (output, error) = ended.expect("有工具结果");
        assert!(!error, "{output}");
        assert!(
            output.contains("web-1"),
            "工具桥把这台主机交给了模型：{output}"
        );
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            text.contains("看过了"),
            "工具结果回填后模型给了结论：{text}"
        );
        // 一个回合里发了两次请求，每次都有用量；窗口是模型的（gpt-5.5 在资料库里）。
        let usages: Vec<_> = signals
            .iter()
            .filter_map(|s| match s {
                BuiltinSignal::Usage(usage) => Some(usage.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(usages.len(), 2, "{signals:?}");
        assert!(usages[0].window >= 128_000 && !usages[0].window_estimated);
        assert_eq!(usages[0].model, "gpt-5.5");
        assert_eq!(fake.post_count(), 2);

        // 对话记录存下来了，resume_id 可以续接；关掉后收回密钥。
        let resume = session.conversation.resume_id().expect("有可续接的记录");
        session.shutdown().await;

        // 新对话：同一个接口再开一个，互不影响；续接旧的能带回历史。
        let mut again = hub
            .start_builtin(BuiltinStart {
                endpoint: fake_endpoint(EndpointKind::OpenAiChat),
                api_key: ApiKey::new("sk-test-e2e"),
                transport: Arc::clone(&fake) as Arc<dyn Transport>,
                catalog: Arc::new(Catalog::builtin()),
                resume: Some(resume.clone()),
                tuning: fast_tuning(),
            })
            .await
            .expect("续接");
        assert!(again.resumed);
        assert_eq!(
            again.conversation.resume_id().as_deref(),
            Some(resume.as_str())
        );
        again.shutdown().await;
    }

    #[tokio::test]
    async fn the_anthropic_flavour_and_a_rejected_key_both_surface_properly() {
        use crate::agent_setup::builtin_fake::FakeTransport;
        use zssh_core::ai::agent::builtin::config::EndpointKind;
        let host = Arc::new(CountingHost::default());
        let (_data_dir, hub) = test_hub(host).await;
        let fake = Arc::new(FakeTransport::default());

        let mut anthropic = hub
            .start_builtin(BuiltinStart {
                endpoint: fake_endpoint(EndpointKind::Anthropic),
                api_key: ApiKey::new("sk-test-claude"),
                transport: Arc::clone(&fake) as Arc<dyn Transport>,
                catalog: Arc::new(Catalog::builtin()),
                resume: None,
                tuning: fast_tuning(),
            })
            .await
            .expect("启动");
        anthropic
            .conversation
            .send("你好", Some("claude-sonnet-5-5"))
            .await
            .expect("发送");
        let (events, _signals) = collect_turn(&mut anthropic).await;
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "收到：你好");
        anthropic.shutdown().await;

        // Key 被拒（401）：不重试，回合以错误结束，旁路给结构化的失败详情。
        let mut bad = hub
            .start_builtin(BuiltinStart {
                endpoint: fake_endpoint(EndpointKind::OpenAiChat),
                api_key: ApiKey::new("sk-test-bad-key"),
                transport: Arc::clone(&fake) as Arc<dyn Transport>,
                catalog: Arc::new(Catalog::builtin()),
                resume: None,
                tuning: fast_tuning(),
            })
            .await
            .expect("启动");
        bad.conversation
            .send("hi", Some("gpt-5.5"))
            .await
            .expect("发送");
        let (events, signals) = collect_turn(&mut bad).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Error { message } if message.contains("API Key"))),
            "{events:?}"
        );
        assert!(
            signals
                .iter()
                .any(|s| matches!(s, BuiltinSignal::Failure(f) if f.status == Some(401))),
            "{signals:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::Retrying { .. }))
        );
        bad.shutdown().await;
    }

    /// 记下每个请求的头，其余转给假接口。
    struct HeaderRecorder {
        inner: crate::agent_setup::builtin_fake::FakeTransport,
        seen: std::sync::Mutex<Vec<Vec<(String, String)>>>,
    }

    impl Transport for HeaderRecorder {
        fn post<'a>(
            &'a self,
            request: zssh_core::ai::agent::builtin::provider::HttpRequest,
        ) -> zssh_core::ssh::BoxFuture<
            'a,
            Result<
                zssh_core::ai::agent::builtin::provider::HttpResponse,
                zssh_core::ai::agent::builtin::provider::TransportError,
            >,
        > {
            self.seen.lock().expect("锁").push(request.headers.clone());
            self.inner.post(request)
        }
    }

    #[tokio::test]
    async fn an_endpoint_marked_no_key_chats_without_an_authorization_header() {
        use zssh_core::ai::agent::builtin::config::{
            AiEndpointDraft, EndpointKind, OpenAiOptionsConfig,
        };
        let db = Database::open_in_memory().expect("内存库");
        let saved = db
            .ai_endpoint_save(&AiEndpointDraft {
                id: None,
                name: "本机 Ollama".into(),
                kind: EndpointKind::OpenAiChat,
                base_url: "http://localhost:11434/v1".into(),
                api_key: None,
                default_model: Some("gpt-5.5".into()),
                models: Vec::new(),
                overrides: Default::default(),
                openai_options: OpenAiOptionsConfig {
                    no_key: true,
                    ..OpenAiOptionsConfig::default()
                },
                preset_id: Some("ollama".into()),
            })
            .expect("保存");
        // 没写「不需要 Key」的同类接口读不出 Key，提示去补。
        let bare = db
            .ai_endpoint_save(&AiEndpointDraft {
                id: None,
                name: "没填".into(),
                kind: EndpointKind::OpenAiChat,
                base_url: "http://localhost:11434/v1".into(),
                api_key: None,
                default_model: None,
                models: Vec::new(),
                overrides: Default::default(),
                openai_options: OpenAiOptionsConfig::default(),
                preset_id: None,
            })
            .expect("保存");
        let err = load_endpoint_from(&db, bare.id).unwrap_err();
        assert!(err.contains("还没有 API Key"), "{err}");

        let (endpoint, key) = load_endpoint_from(&db, saved.id).expect("不需要 Key 的接口可以读");
        assert!(key.is_empty() && endpoint.is_ready());
        let (_data_dir, hub) = test_hub(Arc::new(CountingHost::default())).await;
        let recorder = Arc::new(HeaderRecorder {
            inner: crate::agent_setup::builtin_fake::FakeTransport::default(),
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let mut session = hub
            .start_builtin(BuiltinStart {
                endpoint,
                api_key: key,
                transport: Arc::clone(&recorder) as Arc<dyn Transport>,
                catalog: Arc::new(Catalog::builtin()),
                resume: None,
                tuning: fast_tuning(),
            })
            .await
            .expect("启动");
        session
            .conversation
            .send("你好", Some("gpt-5.5"))
            .await
            .expect("发送");
        let (events, _signals) = collect_turn(&mut session).await;
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.contains("收到"), "没有 Key 也聊得起来：{text}");
        {
            let seen = recorder.seen.lock().expect("锁");
            assert!(!seen.is_empty());
            assert!(
                seen.iter()
                    .flatten()
                    .all(|(name, _)| !name.eq_ignore_ascii_case("authorization")),
                "不需要 Key 的接口不能发 Authorization 头：{seen:?}"
            );
        }
        session.shutdown().await;
    }

    /// 端到端（本机假 Tokenway，不联网）：连接 → 存保险库 → 对话 → 余额显示 → 需充值 → 授权失效 → 重新连接 → 断开。
    /// 界面上这些提示卡 / 按钮的状态测试在 `agent_chat::builtin_ui`，窗口版见截图场景。
    #[tokio::test]
    async fn the_whole_tokenway_flow_works_without_a_window_connect_chat_balance_topup_revoke_disconnect()
     {
        use crate::agent_setup::tokenway::{
            BalanceBook, BalanceProblem, ConnectDeps, finish_connect, run_connect,
        };
        use std::time::Instant;
        use tokio::sync::Notify;
        use zssh_core::ai::agent::builtin::provider::ReqwestTransport;
        use zssh_core::ai::agent::builtin::tokenway::api::{
            ChatFailureAction, classify_chat_failure, fetch_status,
        };
        use zssh_core::ai::agent::builtin::tokenway::endpoint::disconnect;
        use zssh_core::ai::agent::builtin::tokenway::fake::{FakeBrowser, FakeTokenway};
        use zssh_core::ai::agent::builtin::tokenway::{ApiErrorKind, UrlPolicy};

        let fake = FakeTokenway::start().await.expect("假 Tokenway");
        let preset = fake.preset();
        let policy = UrlPolicy::with_http_loopback_for_tests();
        let transport = Arc::new(ReqwestTransport::build(None).expect("传输层"));
        let db = Database::open_in_memory().expect("内存库");
        let connect = || async {
            run_connect(
                &ConnectDeps {
                    preset: &preset,
                    policy: &policy,
                    transport: transport.as_ref(),
                    opener: &FakeBrowser::new(),
                    device: "测试 设备",
                    timeout: Duration::from_secs(20),
                },
                &Notify::new(),
                &|_| {},
            )
            .await
            .expect("授权")
        };

        // 1 连接：授权页自动点"授权" → 换令牌 → 存进保险库 → 默认接口，内置助手可用。
        let connected = connect().await;
        let endpoint = finish_connect(&db, &preset, &connected, &policy).expect("保存");
        assert!(crate::agent_setup::builtin::usable_in(&db));
        assert_eq!(
            db.ai_endpoint_api_key(endpoint.id).unwrap(),
            fake.last_key()
        );
        let (endpoint, key) = load_endpoint_from(&db, endpoint.id).expect("读接口和 Key");
        assert_eq!(endpoint.preset_id.as_deref(), Some("tokenway"));

        // 2 对话：用保险库里的 Key，经真实的 HTTP。
        let (_data_dir, hub) = test_hub(Arc::new(CountingHost::default())).await;
        let mut session = hub
            .start_builtin(BuiltinStart {
                endpoint: endpoint.clone(),
                api_key: key.clone(),
                transport: Arc::clone(&transport) as Arc<dyn Transport>,
                catalog: Arc::new(Catalog::builtin()),
                resume: None,
                tuning: fast_tuning(),
            })
            .await
            .expect("启动");
        session
            .conversation
            .send("你好", Some("fake-chat-pro"))
            .await
            .expect("发送");
        let (events, signals) = collect_turn(&mut session).await;
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.contains("收到：你好"), "{text}");
        assert!(
            !signals
                .iter()
                .any(|s| matches!(s, BuiltinSignal::Failure(_))),
            "{signals:?}"
        );

        // 3 余额显示：打开面板取一次，最多每 60 秒一次。
        let mut book = BalanceBook::default();
        let t0 = Instant::now();
        let status_url = preset.status_url.clone().unwrap();
        assert!(book.poll(endpoint.id, t0));
        book.finish(
            endpoint.id,
            fetch_status(transport.as_ref(), &policy, &status_url, &key).await,
        );
        assert_eq!(book.view(endpoint.id).unwrap().label, "余额 ¥12.34");
        book.mark_all_dirty();
        assert!(!book.poll(endpoint.id, t0 + Duration::from_secs(20)));

        // 4 需充值：状态变"需充值"；对话回 402，旁路信号带错误码，界面据此给"去充值"。
        fake.set_needs_topup(true);
        assert!(book.poll(endpoint.id, t0 + Duration::from_secs(61)));
        book.finish(
            endpoint.id,
            fetch_status(transport.as_ref(), &policy, &status_url, &key).await,
        );
        let view = book.view(endpoint.id).unwrap();
        assert_eq!((view.label.as_str(), view.needs_topup), ("需充值", true));
        session
            .conversation
            .send("再来一句", Some("fake-chat-pro"))
            .await
            .expect("发送");
        let (_events, signals) = collect_turn(&mut session).await;
        let failure = signals
            .iter()
            .find_map(|s| match s {
                BuiltinSignal::Failure(f) => Some(f.clone()),
                _ => None,
            })
            .expect("402 的失败信号");
        assert_eq!(failure.code.as_deref(), Some("zeeterm_requires_topup"));
        assert_eq!(
            classify_chat_failure(&failure),
            Some(ChatFailureAction::RequiresTopup)
        );
        fake.set_needs_topup(false);

        // 5 授权失效（用户在网站改密码）：对话 401、余额 401 → "授权已失效"，本地配置还在。
        fake.revoke_all();
        session
            .conversation
            .send("还在吗", Some("fake-chat-pro"))
            .await
            .expect("发送");
        let (_events, signals) = collect_turn(&mut session).await;
        let failure = signals
            .iter()
            .find_map(|s| match s {
                BuiltinSignal::Failure(f) => Some(f.clone()),
                _ => None,
            })
            .expect("401 的失败信号");
        assert_eq!(
            classify_chat_failure(&failure),
            Some(ChatFailureAction::AuthorizationInvalid)
        );
        book.mark_all_dirty();
        assert!(book.poll(endpoint.id, t0 + Duration::from_secs(130)));
        let result = fetch_status(transport.as_ref(), &policy, &status_url, &key).await;
        assert_eq!(
            result.as_ref().unwrap_err().kind,
            ApiErrorKind::InvalidToken
        );
        book.finish(endpoint.id, result);
        assert_eq!(
            book.view(endpoint.id).unwrap().problem,
            Some(BalanceProblem::AuthorizationInvalid)
        );
        assert_eq!(db.ai_endpoint_list().unwrap().len(), 1, "不自动删本地配置");
        session.shutdown().await;

        // 6 重新连接：原地换 Key，名字和设置保留，又能聊了。
        let connected = connect().await;
        let again = finish_connect(&db, &preset, &connected, &policy).expect("保存");
        assert_eq!(again.id, endpoint.id);
        assert_eq!(db.ai_endpoint_list().unwrap().len(), 1);
        let (endpoint, key) = load_endpoint_from(&db, again.id).expect("读");
        let mut session = hub
            .start_builtin(BuiltinStart {
                endpoint,
                api_key: key.clone(),
                transport: Arc::clone(&transport) as Arc<dyn Transport>,
                catalog: Arc::new(Catalog::builtin()),
                resume: None,
                tuning: fast_tuning(),
            })
            .await
            .expect("启动");
        session
            .conversation
            .send("回来了", Some("fake-chat-pro"))
            .await
            .expect("发送");
        let (events, _signals) = collect_turn(&mut session).await;
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::TextDelta { text } if text.contains("收到：回来了")
        )));
        session.shutdown().await;

        // 7 断开：服务端停用、保险库里的 Key 和接口配置都删了。
        let report = disconnect(
            &db,
            transport.as_ref(),
            preset.revoke_url.as_deref(),
            &policy,
            again.id,
        )
        .await
        .expect("断开");
        assert!(report.revoked.is_ok());
        assert_eq!(
            fake.key_is_revoked(key.expose()),
            Some(true),
            "服务端已停用这枚令牌"
        );
        assert!(db.ai_endpoint_list().unwrap().is_empty());
        assert!(
            db.identity_list().unwrap().is_empty(),
            "保险库里的 Key 也删了"
        );

        // 整个流程只请求过约定的路径：没有任何使用统计。
        let known = [
            "GET /connect/zeeterm",
            "POST /api/connect/token",
            "GET /api/connect/status",
            "POST /api/connect/revoke",
            "GET /v1/models",
            "POST /v1/chat/completions",
        ];
        for request in fake.requests() {
            assert!(known.contains(&request.as_str()), "多余的请求：{request}");
        }
    }
    #[tokio::test]
    async fn a_topup_error_code_reaches_the_ui_as_a_structured_signal() {
        use crate::agent_setup::builtin_fake::FakeTransport;
        use zssh_core::ai::agent::builtin::config::EndpointKind;
        let (_data_dir, hub) = test_hub(Arc::new(CountingHost::default())).await;
        let mut session = hub
            .start_builtin(BuiltinStart {
                endpoint: fake_endpoint(EndpointKind::OpenAiChat),
                api_key: ApiKey::new("sk-test-topup"),
                transport: Arc::new(FakeTransport::default()),
                catalog: Arc::new(Catalog::builtin()),
                resume: None,
                tuning: fast_tuning(),
            })
            .await
            .expect("启动");
        session
            .conversation
            .send("hi", Some("gpt-5.5"))
            .await
            .expect("发送");
        let (_events, signals) = collect_turn(&mut session).await;
        let code = signals.iter().find_map(|s| match s {
            BuiltinSignal::Failure(f) => f.code.clone(),
            _ => None,
        });
        assert_eq!(code.as_deref(), Some("zeeterm_requires_topup"));
        session.shutdown().await;
    }

    #[tokio::test]
    async fn the_whole_builtin_flow_works_without_a_window_add_test_choose_chat_tool_usage_new() {
        use crate::agent_setup::builtin::{self, picker_catalog};
        use crate::agent_setup::builtin_fake::FakeTransport;
        use crate::main_window::ai_endpoint_dialog::{FormValues, build_draft, describe_probe};
        use zssh_core::ai::agent::builtin::config::{EndpointKind, OpenAiOptionsConfig};
        use zssh_core::ai::agent::builtin::probe::test_endpoint;

        // 1 添加接口：表单值 → 校验过的草稿 → 存库（Key 进保险库）。
        let db = Database::open_in_memory().expect("内存库");
        assert_eq!(
            crate::agent_setup::default_agent(Some(&db)),
            AgentKind::Builtin,
            "没选过时默认 ZeePilot，接口还没配也一样"
        );
        let draft = build_draft(&FormValues {
            id: None,
            preset_id: None,
            kind: EndpointKind::OpenAiChat,
            name: "我的网关",
            base_url: "https://api.example.com/v1/",
            api_key: "sk-test-flow0001",
            has_stored_key: false,
            default_model: "gpt-5.5",
            extra_models: "my-chat-model",
            options: OpenAiOptionsConfig::default(),
            overrides: Default::default(),
        })
        .expect("表单合法");
        // 2 测试：用还没保存的表单值，对假接口拉模型清单并发极短请求。
        let fake = Arc::new(FakeTransport::default());
        let config = draft.provider_config(ApiKey::new("sk-test-flow0001"));
        let probe = test_endpoint(fake.as_ref(), &config, "gpt-5.5").await;
        assert!(probe.ok(), "{:?}", probe.reply);
        let caps = zssh_core::ai::agent::builtin::catalog::resolve(
            "gpt-5.5",
            None,
            probe.remote_caps().get("gpt-5.5").cloned(),
            &Catalog::builtin(),
        );
        let described = describe_probe(&probe, "gpt-5.5", &caps);
        assert!(described.ok && described.lines[0].starts_with("测试通过"));
        assert_eq!(
            probe.model_ids(),
            ["gpt-5.5", "deepseek-v4-flash", "my-chat-model"]
        );
        let saved = db.ai_endpoint_save(&draft).expect("保存");
        assert!(saved.has_key);
        assert_eq!(
            db.ai_endpoint_api_key(saved.id).expect("读").as_deref(),
            Some("sk-test-flow0001")
        );

        // 3 配好之后默认智能体还是内置助手；面板的模型下拉有接口的模型和思考强度变体。
        assert_eq!(
            crate::agent_setup::default_agent(Some(&db)),
            AgentKind::Builtin
        );
        let remote = builtin::RemoteModels::from_list(probe.models.clone().unwrap_or_default());
        let picker = picker_catalog(&saved, Some(&remote), &Catalog::builtin(), Some("gpt-5.5"));
        let ids: Vec<&str> = picker.models.iter().map(|m| m.id.as_str()).collect();
        assert!(
            ids.contains(&"gpt-5.5/high") && ids.contains(&"deepseek-v4-flash"),
            "{ids:?}"
        );

        // 4 发消息：模型调工具（假主机），结果回填，用量上报；5 新对话互不影响。
        let _dir_guard = tempfile::Builder::new()
            .prefix("zssh-builtin-flow-")
            .tempdir()
            .expect("临时目录");
        let dir = _dir_guard.path().to_path_buf();
        let hub = AgentHub::with_parts(
            Arc::new(CountingHost::default()),
            Arc::new(NoAudit),
            Arc::new(FixedMode(ExecMode::Manual)),
            dir.clone(),
            PathBuf::from("zssh-mcp.exe"),
        )
        .await
        .expect("建共用桥");
        let start = |resume: Option<String>| BuiltinStart {
            endpoint: saved.clone(),
            api_key: ApiKey::new("sk-test-flow0001"),
            transport: Arc::clone(&fake) as Arc<dyn Transport>,
            catalog: Arc::new(Catalog::builtin()),
            resume,
            tuning: fast_tuning(),
        };
        let mut first = hub.start_builtin(start(None)).await.expect("启动");
        first.sync_hosts(&[web_host(1)]);
        first
            .conversation
            .send("tool see the servers", Some("gpt-5.5/high"))
            .await
            .expect("发送");
        let (events, signals) = collect_turn(&mut first).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolEnd { error: false, .. }))
        );
        let usage = signals.iter().find_map(|s| match s {
            BuiltinSignal::Usage(u) => Some(u.clone()),
            _ => None,
        });
        let usage = crate::agent_setup::builtin::usage_from_turn(&usage.expect("有用量"));
        assert!(
            usage.window.is_some_and(|w| w >= 128_000),
            "分母是模型窗口：{usage:?}"
        );
        let old_id = first.conversation.resume_id().expect("有记录");
        first.shutdown().await;
        let mut second = hub.start_builtin(start(None)).await.expect("新对话");
        assert_ne!(
            second.conversation.resume_id(),
            Some(old_id),
            "新对话是另一个会话"
        );
        assert_eq!(second.conversation.resume_id(), None, "还没有消息");
        second.shutdown().await;
    }

    // ---- 真实智能体（显式 ZSSH_AGENT_E2E=1 才拉起；每种都用独立配置，不碰用户真实的配置）----

    fn e2e_data_dir(test: &str) -> Option<PathBuf> {
        if std::env::var("ZSSH_AGENT_E2E").ok().as_deref() != Some("1") {
            println!("跳过 {test}：设置 ZSSH_AGENT_E2E=1 才跑（会拉起真实智能体并调用模型）");
            return None;
        }
        let Some(dir) = std::env::var_os("ZSSH_DATA_DIR").map(PathBuf::from) else {
            println!("跳过 {test}：需要 ZSSH_DATA_DIR 指向独立数据目录");
            return None;
        };
        Some(dir)
    }

    /// 找一个能用的 `zssh-mcp.exe`（工具桥那一头；纯对话不会调用它，但智能体启动时要能拉起）。
    fn find_zssh_mcp() -> Option<PathBuf> {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let name = format!("zssh-mcp{}", std::env::consts::EXE_SUFFIX);
        [
            manifest.join("../target/debug").join(&name),
            manifest.join("../../target/debug").join(&name),
            manifest.join("../target/release").join(&name),
        ]
        .into_iter()
        .find(|path| path.is_file())
    }

    /// 发一句纯对话，读到回合结束；返回回复文字。`Err("条件缺失…")` 表示环境不满足（跳过，不算失败）。
    async fn chat_once(session: &mut AgentSession, model: Option<&str>) -> Result<String, String> {
        let cancel = AtomicBool::new(false);
        let sent = send_unless_cancelled(
            session.conversation.as_ref(),
            &cancel,
            "Reply with the single word OK. Do not use any tools.",
            model,
        )
        .await;
        match sent {
            Ok(true) => {}
            Ok(false) => return Err("不该被取消".to_owned()),
            Err(err) => return Err(format!("条件缺失：发送失败：{err}")),
        }
        let mut text = String::new();
        let mut ended = false;
        let mut error = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(150);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(5), session.events.recv()).await {
                Ok(Some(AgentEvent::TextDelta { text: delta })) => text.push_str(&delta),
                Ok(Some(AgentEvent::TurnEnd)) => {
                    ended = true;
                    break;
                }
                Ok(Some(AgentEvent::Error { message })) => error = Some(message),
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => {}
            }
        }
        match (ended, error) {
            (true, None) => Ok(text),
            (_, Some(message)) => Err(format!("条件缺失：登录态不可用或模型报错：{message}")),
            (false, None) => Err("回合没有在 150 秒内结束".to_owned()),
        }
    }

    fn report(name: &str, outcome: Result<String, String>) {
        match outcome {
            Ok(text) => {
                println!("{name}：收到回复 {} 个字符", text.chars().count());
                assert!(!text.trim().is_empty(), "回合结束了却没有任何正文");
            }
            Err(message) if message.starts_with("条件缺失") => println!("{message}"),
            Err(message) => panic!("{message}"),
        }
    }

    /// 真实 Claude Code 的端到端：用开发者专门为测试登录过的配置目录
    /// （`ZSSH_AGENT_E2E_CLAUDE_CONFIG_DIR`，事先 `CLAUDE_CONFIG_DIR=<该目录> claude` 登录一次），
    /// 直接把它当 `CLAUDE_CONFIG_DIR` 用——**不复制、不改、不删**。不能复制用户真实的 `~/.claude`
    /// 里的登录文件：刷新令牌是轮换的，副本一刷新原件就失效，用户终端里的 claude 会掉登录。
    /// 没设置 / 目录不存在 / 没装 claude / 登录不可用都打印"条件缺失"后跳过，不算失败。
    #[test]
    fn real_claude_round_trip_with_an_isolated_config_dir() {
        use zssh_core::ai::agent::discovery::{self, CLAUDE};

        let Some(data_dir) = e2e_data_dir("real_claude_round_trip") else {
            return;
        };
        let Some(config_dir) = std::env::var_os("ZSSH_AGENT_E2E_CLAUDE_CONFIG_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
        else {
            println!(
                "条件缺失：没设置 ZSSH_AGENT_E2E_CLAUDE_CONFIG_DIR（要一个事先单独登录过的专用目录），\
                 跳过真实对话"
            );
            return;
        };
        if !config_dir.is_dir() {
            println!("条件缺失：ZSSH_AGENT_E2E_CLAUDE_CONFIG_DIR 指向的目录不存在，跳过真实对话");
            return;
        }

        let runtime = crate::ssh_backend::runtime().expect("运行时");
        let outcome = runtime.block_on(async {
            let install = match discovery::discover(&CLAUDE, None, &data_dir).await {
                Ok(Some(install)) => install,
                _ => return Err("条件缺失：本机没有可用的 claude".to_owned()),
            };
            // 不测工具调用：mcp 路径指向一个不存在的文件，claude 会忽略这个 MCP 服务。
            let hub = AgentHub::with_parts(
                Arc::new(CountingHost::default()),
                Arc::new(NoAudit),
                Arc::new(FixedMode(ExecMode::Manual)),
                data_dir.clone(),
                data_dir.join("no-such-zssh-mcp.exe"),
            )
            .await?;
            let overrides = StartOverrides {
                extra_env: vec![(
                    "CLAUDE_CONFIG_DIR".to_owned(),
                    config_dir.to_string_lossy().into_owned(),
                )],
                ..StartOverrides::default()
            };
            let mut session = hub
                .start_session(AgentKind::Claude, install, None, overrides)
                .await?;
            let result = chat_once(&mut session, None).await;
            session.shutdown().await;
            result
        });
        report("real_claude_round_trip", outcome);
    }

    /// 真实 opencode 的端到端：不读用户的 opencode 配置（注入空的模型配置），并把它的数据 /
    /// 状态 / 缓存目录指到数据目录下的独立文件夹，不碰用户真实的 opencode 数据。没有登录也能用的
    /// 免费模型才跑得通：回合报错就记"条件缺失"。
    #[test]
    fn real_opencode_round_trip_with_isolated_data() {
        use zssh_core::ai::agent::discovery::{self, OPENCODE};

        let Some(data_dir) = e2e_data_dir("real_opencode_round_trip") else {
            return;
        };
        let isolated = data_dir.join("e2e-opencode-xdg");
        let _ = std::fs::remove_dir_all(&isolated);
        for sub in ["data", "state", "cache"] {
            std::fs::create_dir_all(isolated.join(sub)).expect("建独立目录");
        }
        // SAFETY: 只有这个用例设这三个变量，且在拉起 opencode 之前同步完成（子进程继承）。
        unsafe {
            std::env::set_var("XDG_DATA_HOME", isolated.join("data"));
            std::env::set_var("XDG_STATE_HOME", isolated.join("state"));
            std::env::set_var("XDG_CACHE_HOME", isolated.join("cache"));
        }
        let runtime = crate::ssh_backend::runtime().expect("运行时");
        let outcome = runtime.block_on(async {
            let install = match discovery::discover(&OPENCODE, None, &data_dir).await {
                Ok(Some(install)) if !install.too_old => install,
                _ => return Err("条件缺失：本机没有可用的 opencode".to_owned()),
            };
            let Some(mcp) = find_zssh_mcp() else {
                return Err(
                    "条件缺失：找不到 zssh-mcp.exe（先 cargo build -p zssh-mcp）".to_owned(),
                );
            };
            let hub = AgentHub::with_parts(
                Arc::new(CountingHost::default()),
                Arc::new(NoAudit),
                Arc::new(FixedMode(ExecMode::Manual)),
                data_dir.clone(),
                mcp,
            )
            .await?;
            let overrides = StartOverrides {
                opencode_user_config: Some(serde_json::json!({})),
                ..StartOverrides::default()
            };
            let mut session = match hub
                .start_session(AgentKind::OpenCode, install, None, overrides)
                .await
            {
                Ok(session) => session,
                Err(err) => return Err(format!("条件缺失：opencode 没起来：{err}")),
            };
            // 模型目录走同一个共用进程（没有登录 / 没有配置时可能只有免费模型）。
            let catalog = session.conversation.models().await;
            println!(
                "real_opencode_round_trip：目录里 {} 个模型",
                catalog.as_ref().map(|c| c.models.len()).unwrap_or(0)
            );
            let free = catalog.ok().and_then(|catalog| {
                catalog
                    .models
                    .iter()
                    .find(|model| model.id.starts_with("opencode/") && model.id.contains("free"))
                    .map(|model| model.id.clone())
            });
            let result = chat_once(&mut session, free.as_deref()).await;
            session.shutdown().await;
            hub.shutdown_all().await;
            result
        });
        let _ = std::fs::remove_dir_all(&isolated);
        report("real_opencode_round_trip", outcome);
    }

    /// 真实 codex 的端到端：独立的 `CODEX_HOME`，只按字节复制 `auth.json` 进去，用完删。
    /// 没装 codex / 没有 `auth.json`（比如用的是自建服务商配置，复制一个文件无法复现）就记"条件缺失"。
    #[test]
    fn real_codex_round_trip_with_an_isolated_codex_home() {
        use zssh_core::ai::agent::discovery::{self, CODEX};

        let Some(data_dir) = e2e_data_dir("real_codex_round_trip") else {
            return;
        };
        let runtime = crate::ssh_backend::runtime().expect("运行时");
        let home = data_dir.join("e2e-codex-home");
        let outcome = runtime.block_on(async {
            let install = match discovery::discover(&CODEX, None, &data_dir).await {
                Ok(Some(install)) if !install.too_old => install,
                _ => return Err("条件缺失：本机没有可用的 codex".to_owned()),
            };
            let Some(real_home) = std::env::home_dir() else {
                return Err("条件缺失：找不到家目录，找不到登录文件".to_owned());
            };
            let source = std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| real_home.join(".codex"))
                .join("auth.json");
            let Ok(bytes) = std::fs::read(&source) else {
                return Err("条件缺失：没有可复制的 auth.json，跳过真实 codex 对话".to_owned());
            };
            let _ = std::fs::remove_dir_all(&home);
            std::fs::create_dir_all(&home)
                .map_err(|err| format!("建独立 CODEX_HOME 失败：{err}"))?;
            std::fs::write(home.join("auth.json"), &bytes)
                .map_err(|err| format!("复制登录文件失败：{err}"))?;
            let Some(mcp) = find_zssh_mcp() else {
                return Err(
                    "条件缺失：找不到 zssh-mcp.exe（先 cargo build -p zssh-mcp）".to_owned(),
                );
            };
            let hub = AgentHub::with_parts(
                Arc::new(CountingHost::default()),
                Arc::new(NoAudit),
                Arc::new(FixedMode(ExecMode::Manual)),
                data_dir.clone(),
                mcp,
            )
            .await?;
            let overrides = StartOverrides {
                codex_home: Some(home.clone()),
                ..StartOverrides::default()
            };
            let mut session = match hub
                .start_session(AgentKind::Codex, install, None, overrides)
                .await
            {
                Ok(session) => session,
                Err(err) => return Err(format!("条件缺失：codex 没起来：{err}")),
            };
            let result = chat_once(&mut session, None).await;
            session.shutdown().await;
            hub.shutdown_all().await;
            result
        });
        // 无论成败都删掉复制进去的登录文件。
        let _ = std::fs::remove_dir_all(&home);
        report("real_codex_round_trip", outcome);
    }

    #[test]
    fn every_host_message_has_both_languages() {
        use crate::i18n::Msg as _;
        for key in [
            AgentHostMsg::OpenHostDbFailedPrefix,
            AgentHostMsg::HostTargetOnlyPrefix,
            AgentHostMsg::NoTerminal,
            AgentHostMsg::TerminalBufferLockUnavailable,
            AgentHostMsg::OpenSftpChannelFailedPrefix,
            AgentHostMsg::NoTerminalUseBackground,
            AgentHostMsg::CannotRememberQuickConnect,
            AgentHostMsg::SessionNotConnectedPrefix,
            AgentHostMsg::CurrentExeFailedPrefix,
            AgentHostMsg::NoExeParentDir,
            AgentHostMsg::ToolBridgeBindFailedPrefix,
            AgentHostMsg::ToolBridgeNonblockingFailedPrefix,
            AgentHostMsg::ToolBridgePortFailedPrefix,
            AgentHostMsg::McpBinaryMissingPrefix,
            AgentHostMsg::McpBinaryMissingSuffix,
            AgentHostMsg::OpenSettingsDbFailedPrefix,
            AgentHostMsg::EndpointMissing,
            AgentHostMsg::ReadApiKeyFailedPrefix,
            AgentHostMsg::MissingApiKeyPrefix,
            AgentHostMsg::MissingApiKeySuffix,
        ] {
            assert!(!key.zh().is_empty(), "{key:?} 缺中文");
            assert!(!key.en().is_empty(), "{key:?} 缺英文");
        }
    }
}
