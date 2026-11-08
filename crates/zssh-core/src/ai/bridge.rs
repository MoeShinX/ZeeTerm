//! AI 工具桥服务端（卡片 AI-1）。
//!
//! 职责：令牌握手、scope/会话归属校验、危险判断、审批（手动/自动）、
//! 在宿主连接上执行、写审计。协议见 [`crate::ai::protocol`]。
//!
//! 审批和会话执行通过 trait 交给调用方（`src-tauri` 或测试），core 不依赖 Tauri。

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Notify, Semaphore};

mod file_tools;
pub mod host;
pub mod terminal_write;

pub use host::{CapabilityHost, ExecHost, FsHost, MemoryHost, ReportHost};

// 随机令牌只有一处实现（和三个智能体驱动共用，见 `ai::agent::lifecycle`）。
use crate::ai::agent::lifecycle::random_hex;
use crate::ai::files::{FileDiff, RemoteFs};
use crate::ai::protocol::{
    clamp_max_lines, clamp_timeout_secs, method_carries_exec_id, ErrorBody, ExecLocation,
    ReadTerminalParams, RememberHostParams, RequestEnvelope, ResponseEnvelope, RunCommandParams,
    RunCommandResponse, TargetSummary, TokenLine, CAPABILITY_BYTES, EXEC_ID_FIELD, METHOD_LIST_DIR,
    METHOD_LIST_TARGETS, METHOD_READ_FILE, METHOD_READ_TERMINAL, METHOD_REMEMBER_HOST,
    METHOD_RUN_COMMAND, METHOD_STAT, METHOD_WRITE_FILE,
};
use crate::ai::reason::ApprovalReason;
use crate::ai::safety::{assess, DangerLevel};
use crate::ssh::{BoxFuture, ExecCapture, EXEC_MAX_STREAM_BYTES};
use crate::store::audit::NewAiAudit;
use crate::store::db::Database;

/// 默认审批等待：5 分钟，超时按拒绝处理。
pub const DEFAULT_APPROVAL_WAIT: Duration = Duration::from_secs(5 * 60);
/// 单行消息上限（1 MiB），防止恶意超长行吃内存。
const MAX_LINE_BYTES: usize = 1024 * 1024;
/// 一条连接上响应写回队列的长度（唯一写者任务从这里取，响应不会互相穿插）。
const WRITE_QUEUE_DEPTH: usize = 64;
/// 握手超时：连上后 10 秒内不发令牌就断开。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// 握手行上限：令牌行只有几十个字节，没通过认证的连接不许让我们攒大块内存（R-9 F15）。
const HANDSHAKE_MAX_BYTES: usize = 4 * 1024;
/// 同时处在握手阶段（还没交出连接层令牌）的连接数。认证过的连接不受它限制：claude 每个对话
/// 一个 zssh-mcp，对话多了连接就多，不能因此拒掉正常的连接。
const MAX_PENDING_HANDSHAKES: usize = 16;
/// 一条连接上同时在处理的请求数。到上限就先不读下一条（背压），不会无限开任务。
const MAX_IN_FLIGHT_PER_CONNECTION: usize = 32;
/// 整个工具桥同时在处理的请求总数（所有连接合计，含连接断开后还在收尾的）。每条连接各有
/// 上限还不够：断开重连就能拿到新的额度，旧请求却还在跑（R-9 F15）。正常使用远到不了这个数。
const MAX_IN_FLIGHT_TOTAL: usize = 128;
/// 目标不存在或不属于当前作用范围。
pub(crate) const SCOPE_TARGET_ERROR: &str = "目标不存在或不属于本次对话的作用范围";

/// 执行模式。设置项 `ai_exec_mode`：`"auto"` 全放行，`"readonly"` 只读命令自动放行、
/// 其余要确认，其他值（默认）每条都确认。旧版本不认识 `"readonly"`，会按手动处理（往安全的方向退）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecMode {
    Manual,
    /// 只读命令（[`crate::ai::readonly::is_read_only`]）自动放行；写文件、其他命令要确认。
    ReadOnly,
    Auto,
}

impl ExecMode {
    /// 从设置项原始值解析；没设置或值不认识都按手动。
    pub fn from_setting(raw: Option<&str>) -> ExecMode {
        match raw {
            Some("auto") => ExecMode::Auto,
            Some("readonly") => ExecMode::ReadOnly,
            _ => ExecMode::Manual,
        }
    }
}

/// 指令的最终处置（写进审计的 decision 列）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecDecision {
    /// 自动模式直接放行的指令（含危险命令；危险等级只进审计和工具结果）。
    Auto,
    /// 人工确认通过。
    Allowed,
    /// 人工拒绝（含 scope 销毁导致的拒绝）。
    Denied,
    /// 审批等待超时。
    Timeout,
}

impl ExecDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            ExecDecision::Auto => "auto",
            ExecDecision::Allowed => "allowed",
            ExecDecision::Denied => "denied",
            ExecDecision::Timeout => "timeout",
        }
    }
}

/// 人工确认的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Allowed,
    Denied,
    Timeout,
}

/// 送给确认界面看的审批请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub scope: String,
    /// 这次工具调用的执行编号（和 `ExecTargetReport.exec_id`、工具结果里的标记是同一个）。
    pub exec_id: String,
    /// 实际执行的目标（逻辑目标 ID）和执行用的会话（还没有连上的会话为 `None`）：
    /// 审批卡片据此显示执行位置，和执行层的解析结果一致（AI-SCOPE-2 第 2 节）。
    pub target_id: String,
    pub session_id: Option<String>,
    pub host_label: String,
    /// `run_command` 是命令全文；`write_file` 是 `write_file <路径>`（卡片上用 `file` 里的细节）。
    pub command: String,
    pub danger: DangerLevel,
    pub reasons: Vec<ApprovalReason>,
    /// `write_file` 的细节（路径、差异）；`run_command` 为 `None`。
    pub file: Option<FileApproval>,
}

/// 确认卡片里 `write_file` 要给用户审的内容：写哪个文件、和现有文件的差异。
/// 差异只用于这张确认卡片，审计不会记录它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileApproval {
    pub path: String,
    pub is_new: bool,
    pub new_bytes: usize,
    pub diff: FileDiff,
}

/// 执行层解析出的实际执行位置（AI-SCOPE-2 第 2 节）：每次工具调用解析完目标、真正开始之前
/// 交给宿主，宿主转给界面。界面只认这份回报，不再自己猜"这次会落在哪台主机 / 哪个终端"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecTargetReport {
    /// 对话的工具桥 scope id（界面据此认出是不是自己这个对话的调用）。
    pub scope: String,
    /// 这次工具调用的执行编号（AI-SCOPE-3）：桥每接到一个工具请求生成一个，随回报、审批事件
    /// 发出，也写进返回给智能体的工具结果（末尾一行标记）。界面拿工具结果里的编号校正卡片
    /// 和回报的配对，不再只靠「工具 + 命令 + 先后」的启发式。只是关联用，不是凭据。
    pub exec_id: String,
    /// 工具名（不带前缀）：`run_command`、`read_terminal`、`list_dir`、`read_file`、
    /// `write_file`、`stat`。
    pub tool: String,
    /// 界面把这份回报对到工具卡片上用的键：`run_command` 是命令原文，文件工具是调用时给的
    /// 路径原文，`read_terminal` 为空。
    pub key: String,
    /// 实际执行的逻辑目标 ID（`host-<id>`，快速连接是会话 ID）。
    pub target_id: String,
    /// 执行用的会话；这台主机现在没有可用的会话（会自动连接）时为 `None`。
    pub session_id: Option<String>,
    pub host_label: String,
}

/// 一次工具调用解析出来的目标：逻辑目标、请求票据、执行用的会话、交给宿主的 ID。
pub(crate) struct ResolvedTarget {
    pub(crate) summary: TargetSummary,
    /// 在解析会话**之前**记下的请求票据（停止、目标移出 / 移出再加回都会使它失效）。
    pub(crate) ticket: RequestTicket,
    pub(crate) session_id: Option<String>,
    /// 交给宿主执行方法用的 ID：有会话就是会话 ID，否则是逻辑目标 ID（宿主自己连）。
    pub(crate) exec_id: String,
}

/// 审批通道：`src-tauri` 走事件，测试走桩。工具桥负责超时和 scope 销毁的取消。
pub trait Approver: Send + Sync {
    fn approve<'a>(&'a self, request: ApprovalRequest) -> BoxFuture<'a, ApprovalDecision>;
}

/// 终端执行的请求（交给宿主在用户终端里跑，AI-2c）。
#[derive(Debug, Clone)]
pub struct TerminalExecRequest {
    /// 目标（会话）ID。
    pub target_id: String,
    /// 这个对话的工具桥 scope id。
    ///
    /// 宿主用它在终端执行登记表里记一条"哪个对话在哪个终端跑着哪条命令"
    /// （AI-2d 1.4）：停止按钮只在这个对话确实有命令在终端里跑时才发 Ctrl+C。
    pub scope_id: String,
    /// 用户要看到的命令原文（AI-2d：包装脚本不显示，显示这一份合成回显）。
    pub command: String,
    /// 已经包装好、带结束标记的整段文本（调用方负责组装）。
    pub payload: String,
    /// 结束标记（用来在终端输出里找退出码）。
    pub marker: String,
    /// 超时（超时后命令不中断，只是把当前状态返回给 AI）。
    pub wait: Duration,
    /// 宿主往终端写命令之前要再 [`ExecGate::check`] 一次：抢占终端之后还要等会话就绪，
    /// 这期间请求可能已经失效。失效时不写、释放终端占位、返回错误。
    pub gate: ExecGate,
}

/// 终端执行的结果。
#[derive(Debug, Clone)]
pub struct TerminalExecCapture {
    /// 命令输出（已去 ANSI、截取到结束标记之前）。
    pub output: String,
    /// 退出码；还在跑（超时）时为 `None`。
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    /// 超时：命令仍在终端里运行。
    pub timed_out: bool,
}

/// 执行位置的决定：能就地跑就终端，否则后台 + 原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecPlacement {
    Terminal,
    Background { reason: String },
}

/// 宿主契约：执行、文件、主机记忆、上报四种能力的并集。
///
/// 各能力另有独立的 trait（[`ExecHost`] / [`FsHost`] / [`MemoryHost`] / [`ReportHost`]，
/// 见 [`host`] 模块）：每个能力只声明它必须回答的问题，只依赖一种能力的代码可以把参数
/// 收窄成那个 trait。任何 `BridgeHost` 都通过 blanket impl 自动具备四种能力，所以现有
/// 宿主（native 侧的 `NativeBridgeHost`、测试替身）不用改。
///
/// 为什么这里不写成 `trait BridgeHost: ExecHost + FsHost + MemoryHost + ReportHost`：
/// 宿主是直接 `impl BridgeHost for ...` 一次写完所有方法的，组合关系会让这种写法同时
/// 需要四个 blanket impl，Rust 的 coherence 检查不允许两者共存（会要求每个宿主拆开
/// 实现）。保留这个组合名是刻意的：它是不改宿主就能加能力的唯一办法。
pub trait BridgeHost: Send + Sync {
    /// 一个逻辑目标这次该用哪个会话执行（AI-SCOPE-2 第 1 节）。
    ///
    /// `bound` 是界面绑给这个目标的会话（这台主机焦点所在 / 最近获得焦点的终端）：它还在
    /// （连着或正在连）就优先用它；没有或已经关了，再按现有规则找（目标本身就是会话 ID 就用
    /// 它；`host-<id>` 找这台主机已经连上的会话）。都没有返回 `None`——执行时由宿主自动连接。
    ///
    /// 默认实现信任界面的绑定，目标本身不是 `host-<id>` 时把它当会话 ID（测试替身用）。
    fn execution_session<'a>(
        &'a self,
        target_id: &'a str,
        bound: Option<&'a str>,
    ) -> BoxFuture<'a, Option<String>> {
        let session = bound.map(str::to_owned).or_else(|| {
            (!target_id.starts_with(crate::ai::protocol::HOST_TARGET_PREFIX))
                .then(|| target_id.to_owned())
        });
        Box::pin(async move { session })
    }

    /// 这个目标不能当 AI 的执行目标时，返回给模型看的原因（Telnet / 串口主机没有 SSH 的 exec
    /// 通道和终端 shell 约定，AI 不能往上面跑命令、读写文件）。默认都能。
    ///
    /// 宿主应该用主机库里的协议判断（`ProtocolSettings::ai_exec_refusal`）；快速连接这类
    /// 不在主机库里的目标一定是 SSH，返回 `None`。
    fn target_refusal<'a>(&'a self, target_id: &'a str) -> BoxFuture<'a, Option<String>> {
        let _ = target_id;
        Box::pin(async { None })
    }

    /// 执行层解析出目标之后、真正开始之前回报给界面（AI-SCOPE-2 第 2 节）。默认空操作。
    fn report_target(&self, report: ExecTargetReport) {
        let _ = report;
    }

    fn exec<'a>(
        &'a self,
        session_id: &'a str,
        command: &'a str,
        wait: Duration,
        max_stream_bytes: usize,
    ) -> BoxFuture<'a, Result<ExecCapture, String>>;

    /// 同 [`BridgeHost::exec`]，但受 `gate` 约束（R-9 F11、F12）：
    ///
    /// - 命令发出去之前（比如自动连接完成之后、执行通道开好之后）要 [`ExecGate::check`]，失效
    ///   就不发——ExecGate 实现了 [`crate::ssh::connection::ExecGuard`]，直接交给
    ///   [`crate::ssh::Connection::exec_capture_guarded`]，它在发 exec 的边界上核对（审计 C04）；
    /// - 发出去之后 [`ExecGate::cancel_flag`] 置位（用户点了停止）要尽快结束（关闭执行通道），
    ///   返回的 `cancelled = true`。
    ///
    /// 默认实现只在开始前检查一次，之后等命令自己结束（测试替身用）。
    fn exec_cancellable<'a>(
        &'a self,
        session_id: &'a str,
        command: &'a str,
        wait: Duration,
        max_stream_bytes: usize,
        gate: &'a ExecGate,
    ) -> BoxFuture<'a, Result<ExecCapture, String>> {
        Box::pin(async move {
            gate.check()?;
            self.exec(session_id, command, wait, max_stream_bytes).await
        })
    }

    fn read_terminal<'a>(
        &'a self,
        session_id: &'a str,
        max_lines: usize,
    ) -> BoxFuture<'a, Result<Vec<String>, String>>;

    /// 这个目标现在能不能在用户的终端里执行（终端空闲且是可识别的 POSIX shell）。
    ///
    /// 返回 `Terminal` 就地执行，返回 `Background { reason }` 走后台通道（原因写进
    /// 工具卡片和审计）。默认实现保守地一律走后台（宿主没实现就地执行时）。
    ///
    /// 返回 `Terminal` 表示宿主**已经抢占**了这个终端：实现方要在
    /// [`BridgeHost::run_in_terminal`] 结束时释放（拿到结束标记 / 超时 / 会话断开），
    /// 抢占失败的并发命令由这里直接降级到后台，不会两条命令叠在同一个终端里。
    fn exec_placement<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, ExecPlacement> {
        let _ = session_id;
        Box::pin(async {
            ExecPlacement::Background {
                reason: "宿主没提供终端执行能力".to_string(),
            }
        })
    }

    /// 在用户的终端里执行（AI-2c）。只有在 [`BridgeHost::exec_placement`] 返回
    /// `Terminal` 时才会被调用；实现方负责在结束时释放 [`BridgeHost::exec_placement`]
    /// 抢占的终端（成功、超时、出错都要放）。
    fn run_in_terminal<'a>(
        &'a self,
        request: TerminalExecRequest,
    ) -> BoxFuture<'a, Result<TerminalExecCapture, String>> {
        let _ = request;
        Box::pin(async { Err("宿主没提供终端执行能力".to_string()) })
    }

    /// [`BridgeHost::exec_placement`] 抢占了终端、工具桥随后又决定不执行（请求在这期间失效）时，
    /// 把占位还回去。默认空操作（没实现就地执行的宿主也没有占位）。
    fn release_terminal<'a>(&'a self, target_id: &'a str) -> BoxFuture<'a, ()> {
        let _ = target_id;
        Box::pin(async {})
    }

    /// 文件工具（M3-4）：这个目标服务器上的文件系统（开一条 SFTP 通道）。
    /// 宿主没实现就报错（文件工具不可用，不影响别的工具）。
    fn remote_fs<'a>(
        &'a self,
        target_id: &'a str,
    ) -> BoxFuture<'a, Result<Arc<dyn RemoteFs>, String>> {
        let _ = target_id;
        Box::pin(async { Err("宿主没提供文件工具能力".to_string()) })
    }

    /// 让 AI 停止这个对话正在终端里跑的 AI 命令：向对应终端发 Ctrl+C。
    ///
    /// 只有这个 scope **确实**有一条 AI 命令在终端里跑才会发；没有就什么都不做
    /// （用户自己可能正在那个终端里跑东西，乱发 Ctrl+C 会打断它）。默认空操作。
    fn interrupt_terminal<'a>(&'a self, scope_id: &'a str) -> BoxFuture<'a, ()> {
        let _ = scope_id;
        Box::pin(async {})
    }

    /// 主机长期记忆（[`crate::ai::host_memory`]）：读这个目标对应主机的笔记。
    /// 快速连接这类不在主机库里的目标没有记忆，返回 `Ok(None)`。默认没有记忆。
    fn read_memory<'a>(
        &'a self,
        target_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, String>> {
        let _ = target_id;
        Box::pin(async { Ok(None) })
    }

    /// 存这个目标对应主机的笔记（已经清理过；空串 = 清空）。
    fn write_memory<'a>(
        &'a self,
        target_id: &'a str,
        notes: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        let _ = (target_id, notes);
        Box::pin(async { Err("这个目标不在主机库里，不能保存记忆".to_string()) })
    }

    /// `write_file` 每个文件保留几份备份（设置项 `ai_write_backup_keep`）。
    /// 默认 5 份；宿主可以按当前设置实现。
    fn write_backup_keep(&self) -> usize {
        crate::store::settings::AI_WRITE_BACKUP_KEEP_DEFAULT
    }
}

/// 执行模式来源（每次命令都重新读设置）。
pub trait ExecModeProvider: Send + Sync {
    fn mode(&self) -> ExecMode;

    /// 界面要求“不论哪种模式，AI 在服务器上做的事都要先问人”（应用锁屏时为真）。
    /// 为真时：执行命令、写文件、读目录 / 读文件 / 看终端等所有碰服务器的请求都变成
    /// “要确认”，确认卡在锁屏下看不到，解锁后用户才看得到并决定；锁着的时候等超时按拒绝
    /// 处理。每次请求都重新问一遍，解锁后立即恢复原来的模式行为。默认不强制。
    fn force_confirm(&self) -> bool {
        false
    }
}

/// 锁屏时确认卡上写的原因（中文原文；结构化的写法是 [`ApprovalReason::Locked`]）。
pub use super::reason::LOCKED_REASON;

/// 锁屏期间等审批超时的拒绝原因。
const LOCKED_TIMEOUT_MESSAGE: &str = "锁屏期间超时，已按拒绝处理";

/// 一条待写审计。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    pub at: i64,
    pub scope: String,
    pub host_label: String,
    pub command: String,
    pub danger: DangerLevel,
    pub decision: ExecDecision,
    pub exit_code: Option<i64>,
    pub duration_ms: Option<i64>,
    /// 执行位置（AI-2c）：终端 / 后台。
    pub location: Option<ExecLocation>,
    /// 降级到后台的原因（AI-2c）。
    pub fallback_reason: Option<String>,
    /// 工具名（M3-4）：`run_command` / `list_dir` / `read_file` / `write_file` / `stat`。
    pub tool: String,
    /// 文件工具操作的路径（M3-4）；`run_command` 为 `None`。
    pub path: Option<String>,
    /// 文件工具涉及的字节数：读 = 返回的字节数，写 = 写入的字节数。**只记数量，不记内容。**
    pub bytes: Option<i64>,
    /// `write_file` 覆盖时在服务器上留的备份路径。
    pub backup_path: Option<String>,
}

/// 审计落盘。写失败只记日志，不影响命令执行。
pub trait AuditLog: Send + Sync {
    fn record(&self, entry: AuditEntry);
}

impl AuditLog for Database {
    fn record(&self, entry: AuditEntry) {
        let result = self.ai_audit_insert(NewAiAudit {
            at: entry.at,
            scope: &entry.scope,
            host_label: &entry.host_label,
            command: &entry.command,
            danger: entry.danger.as_str(),
            decision: entry.decision.as_str(),
            exit_code: entry.exit_code,
            duration_ms: entry.duration_ms,
            location: entry.location.map(ExecLocation::as_str),
            fallback_reason: entry.fallback_reason.as_deref(),
            tool: &entry.tool,
            path: entry.path.as_deref(),
            bytes: entry.bytes,
            backup_path: entry.backup_path.as_deref(),
        });
        if let Err(err) = result {
            tracing::warn!(error = %err, "AI 审计写入失败");
        }
    }
}

/// [`read_bounded_line`] 的结果。
#[derive(Debug, PartialEq, Eq)]
enum BoundedLine {
    /// 一整行（不含换行符；最后一行没有换行也算）。
    Line(String),
    /// 对端关了，没有更多数据。
    Eof,
    /// 超过上限还没看到换行：调用方应断开连接。
    TooLong,
}

/// 读一行，最多读 `max` 个字节：超出时不再继续往内存里攒。
async fn read_bounded_line<R>(reader: &mut R, max: usize) -> std::io::Result<BoundedLine>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncReadExt as _;

    let mut buf = Vec::new();
    // 多读一个字节：正好 `max` 字节的内容加上换行符也要能读完整。
    let limit = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    let read = (&mut *reader)
        .take(limit)
        .read_until(b'\n', &mut buf)
        .await?;
    if read == 0 {
        return Ok(BoundedLine::Eof);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    } else if buf.len() > max {
        return Ok(BoundedLine::TooLong);
    }
    match String::from_utf8(buf) {
        Ok(line) => Ok(BoundedLine::Line(line)),
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "请求不是 UTF-8",
        )),
    }
}

pub(crate) fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 恒定时间比较两个令牌（R-8 第 11 节）。
///
/// 用 `==` 比较字符串时，遇到第一个不同的字节就返回，攻击者能按响应时间逐字节猜出
/// 令牌。这里逐字节异或累加，长度不同也把两边都走完再判断，不提前返回。
fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut diff = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        let x = left.get(index).copied().unwrap_or(0);
        let y = right.get(index).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
    }
    diff == 0
}

/// 一个对话的作用范围：独立能力密钥 + 目标列表。
///
/// scope id 是 16 位十六进制（审计和审批事件里用它），能力密钥是
/// [`CAPABILITY_BYTES`] 字节（32 位十六进制），工具调用必须带上它。
pub struct Scope {
    pub(crate) id: String,
    capability: String,
    targets: Arc<Mutex<BTreeMap<String, TargetSummary>>>,
    destroyed: Arc<AtomicBool>,
    /// 单个智能体 session 的授权可以独立撤销，目标列表和整个对话的生命周期仍共用。
    revoked: AtomicBool,
    /// 停止按钮按了几次。请求开始时记下它，之后变了就说明用户叫停了这个回合（R-9 F12）。
    stop_epoch: Arc<AtomicU64>,
    /// 每个目标被移出过几次。请求开始时记下它，审批之后变了（哪怕又被加回来）就不能再按
    /// 旧的批准去执行（R-9 F11）。
    removals: Arc<Mutex<HashMap<String, u64>>>,
    /// 销毁、停止、移出目标都通知它：等审批 / 等执行的请求据此提前结束。
    notify: Arc<Notify>,
}

/// 一条请求开始时的作用范围状态，用来判断它后来有没有失效（停止、目标移出、作用范围销毁）。
#[derive(Debug, Clone)]
pub(crate) struct RequestTicket {
    target_id: String,
    stop_epoch: u64,
    removals: u64,
}

/// 一条已经放行的请求交给宿主的"闸"：宿主在**真正产生副作用之前**（往终端写命令、发 SSH exec、
/// 替换文件）调 [`ExecGate::check`] 再确认一次请求还有效（R-9 F11、F12）。
///
/// 放行之后到真正执行之间可能还要等很久（第一次在终端执行要先探测远端 shell、未连上的主机要
/// 先自动连接）：这期间用户点了停止或把目标移出对话，旧请求就不能再执行。
#[derive(Clone)]
pub struct ExecGate {
    scope: Arc<Scope>,
    ticket: RequestTicket,
    /// 用户点了停止时由工具桥置位：已经发出去的后台命令看到它就关闭执行通道。
    cancel: Arc<AtomicBool>,
}

impl ExecGate {
    pub(crate) fn new(scope: &Arc<Scope>, ticket: &RequestTicket) -> ExecGate {
        ExecGate {
            scope: Arc::clone(scope),
            ticket: ticket.clone(),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 请求还能执行吗；不能时是给模型的原因。
    pub fn check(&self) -> Result<(), String> {
        match self.scope.ticket_refusal(&self.ticket) {
            Some(reason) => Err(reason.to_string()),
            None if self.cancel.load(Ordering::SeqCst) => {
                Err("用户点了停止，这次操作已取消".to_string())
            }
            None => Ok(()),
        }
    }

    /// 停止标志：用户点了停止时由 `run_background` 置位（测试替身和只认原子标志的调用方用）。
    pub fn cancel_flag(&self) -> &AtomicBool {
        &self.cancel
    }
}

/// 后台执行：发 exec 的边界在 ssh 层（[`crate::ssh::Connection::exec_capture_guarded`]），
/// 它拿这个核对完整的有效性——只传停止标志的话，开通道期间目标被移出就拦不住（审计 C04）。
impl crate::ssh::connection::ExecGuard for ExecGate {
    fn check(&self) -> Result<(), String> {
        ExecGate::check(self)
    }

    fn invalidated(&self) -> BoxFuture<'_, String> {
        Box::pin(async move { self.scope.wait_invalidated(&self.ticket).await.to_owned() })
    }

    fn stop_requested(&self) -> bool {
        // 不只看取消标志：标志由 run_background 在收到停止通知之后才置位，中间有一拍。
        self.cancel.load(Ordering::SeqCst) || self.scope.stop_refusal(&self.ticket).is_some()
    }
}

impl std::fmt::Debug for ExecGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecGate")
            .field("scope", &self.scope.id)
            .field("ticket", &self.ticket)
            .field("cancelled", &self.cancel.load(Ordering::SeqCst))
            .finish()
    }
}

impl Scope {
    fn add_target(&self, summary: TargetSummary) {
        if let Ok(mut targets) = self.targets.lock() {
            targets.insert(summary.id.clone(), summary);
        }
    }

    fn remove_target(&self, target_id: &str) {
        if let Ok(mut targets) = self.targets.lock() {
            targets.remove(target_id);
        }
        if let Ok(mut removals) = self.removals.lock() {
            *removals.entry(target_id.to_owned()).or_insert(0) += 1;
        }
        self.notify.notify_waiters();
    }

    fn removal_count(&self, target_id: &str) -> u64 {
        self.removals
            .lock()
            .map(|removals| removals.get(target_id).copied().unwrap_or(0))
            .unwrap_or(u64::MAX)
    }

    /// 停止按钮：这一刻之前开始的请求全部作废（等审批的按拒绝结束，后台命令关通道）。
    fn stop(&self) {
        self.stop_epoch.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// 记下一条请求开始时的状态。
    pub(crate) fn ticket(&self, target_id: &str) -> RequestTicket {
        RequestTicket {
            target_id: target_id.to_owned(),
            stop_epoch: self.stop_epoch.load(Ordering::SeqCst),
            removals: self.removal_count(target_id),
        }
    }

    /// 请求还能不能继续；不能时返回给模型的原因。
    pub(crate) fn ticket_refusal(&self, ticket: &RequestTicket) -> Option<&'static str> {
        if let Some(reason) = self.stop_refusal(ticket) {
            return Some(reason);
        }
        let still_there = self
            .targets
            .lock()
            .map(|targets| targets.contains_key(&ticket.target_id))
            .unwrap_or(false);
        if !still_there || self.removal_count(&ticket.target_id) != ticket.removals {
            return Some("这台服务器已经被移出这个对话，这次操作已取消");
        }
        None
    }

    /// 只看停止和销毁（不看目标移出）。
    fn stop_refusal(&self, ticket: &RequestTicket) -> Option<&'static str> {
        if self.is_destroyed() {
            return Some("用户拒绝（作用范围已销毁）");
        }
        if self.stop_epoch.load(Ordering::SeqCst) != ticket.stop_epoch {
            return Some("用户点了停止，这次操作已取消");
        }
        None
    }

    /// 等到这条请求失效（停止、目标移出、作用范围销毁）为止，返回原因。可安全被 `select!` 取消。
    /// 等审批时用：卡片对应的目标已经不在对话里，就不该再等人点。
    pub(crate) async fn wait_invalidated(&self, ticket: &RequestTicket) -> &'static str {
        self.wait_for(|| self.ticket_refusal(ticket)).await
    }

    /// 等到用户点停止（或作用范围销毁）为止，返回原因。可安全被 `select!` 取消。
    ///
    /// 执行中的命令只因停止而中断：目标被移出对话（比如切换了终端标签）不打断已经在跑的命令，
    /// 半路关掉执行通道可能比让它跑完更糟（升级到一半的包管理器之类）。
    pub(crate) async fn wait_stopped(&self, ticket: &RequestTicket) -> &'static str {
        self.wait_for(|| self.stop_refusal(ticket)).await
    }

    async fn wait_for(&self, check: impl Fn() -> Option<&'static str>) -> &'static str {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(reason) = check() {
                return reason;
            }
            notified.await;
        }
    }

    fn target_list(&self) -> Vec<TargetSummary> {
        self.targets
            .lock()
            .map(|targets| {
                let mut list: Vec<TargetSummary> = targets.values().cloned().collect();
                // 当前焦点那台排第一。
                list.sort_by_key(|t| !t.current);
                list
            })
            .unwrap_or_default()
    }

    pub(crate) fn summary(&self, target_id: &str) -> Option<TargetSummary> {
        self.targets
            .lock()
            .ok()
            .and_then(|targets| targets.get(target_id).cloned())
    }

    fn is_destroyed(&self) -> bool {
        self.destroyed.load(Ordering::SeqCst) || self.revoked.load(Ordering::SeqCst)
    }

    fn destroy(&self) {
        self.destroyed.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

/// 调用方（`src-tauri` 或测试）持有的作用范围句柄。
///
/// 克隆出来的是同一个 scope（`Arc` 内部共享），所以面板可以把它放进驱动任务、
/// 同时在命令里按会话增减成员。
#[derive(Clone)]
pub struct ScopeHandle {
    registry: Arc<Registry>,
    scope: Arc<Scope>,
}

/// 一个智能体 session 使用的独立密钥。撤销后旧模型发起的新请求和已取得 scope 的在途请求
/// 都失效；新 session 使用新密钥，仍看到同一套主机和 scope id。
pub struct ScopeCapability {
    registry: Arc<Registry>,
    scope: Arc<Scope>,
}

impl ScopeCapability {
    pub fn value(&self) -> &str {
        &self.scope.capability
    }

    pub fn revoke(&self) {
        if !self.scope.revoked.swap(true, Ordering::SeqCst) {
            self.scope.stop();
            self.registry.remove(&self.scope.capability);
        }
    }
}

impl Drop for ScopeCapability {
    fn drop(&mut self) {
        self.revoke();
    }
}

impl ScopeHandle {
    pub fn agent_capability(&self) -> Arc<ScopeCapability> {
        let scope = Arc::new(Scope {
            id: self.scope.id.clone(),
            capability: random_hex(CAPABILITY_BYTES),
            targets: Arc::clone(&self.scope.targets),
            destroyed: Arc::clone(&self.scope.destroyed),
            revoked: AtomicBool::new(false),
            stop_epoch: Arc::clone(&self.scope.stop_epoch),
            removals: Arc::clone(&self.scope.removals),
            notify: Arc::clone(&self.scope.notify),
        });
        self.registry.register(Arc::clone(&scope));
        Arc::new(ScopeCapability {
            registry: Arc::clone(&self.registry),
            scope,
        })
    }

    pub fn scope_id(&self) -> &str {
        &self.scope.id
    }

    /// 这个对话的能力密钥（交给模型的系统提示里带它）。
    pub fn capability(&self) -> &str {
        &self.scope.capability
    }

    /// 把一个目标加进这个作用范围（已经连上的会话，或用户 @ 进来的主机）。
    ///
    /// `current` 标记当前焦点终端那台；`pinned` 标记用户 @ 进来的主机。
    pub fn add_target(
        &self,
        id: impl Into<String>,
        host_label: impl Into<String>,
        username: impl Into<String>,
        status: impl Into<String>,
        current: bool,
        pinned: bool,
    ) {
        self.add_target_summary(TargetSummary {
            id: id.into(),
            host_label: host_label.into(),
            username: username.into(),
            status: status.into(),
            current,
            pinned,
            session_id: None,
        });
    }

    /// 同 [`ScopeHandle::add_target`]，但能带上界面绑定的会话（`session_id`）。同一个 ID 再
    /// 加会更新（绑定随焦点变化时不算"移出再加回"，在途请求不受影响）。
    pub fn add_target_summary(&self, summary: TargetSummary) {
        self.scope.add_target(summary);
    }

    /// 目标断开 / 被移除后从作用范围里摘掉。
    pub fn remove_target(&self, target_id: &str) {
        self.scope.remove_target(target_id);
    }

    /// 现在目标列表里有哪些（诊断 / 测试用）。
    pub fn targets(&self) -> Vec<TargetSummary> {
        self.scope.target_list()
    }

    /// 销毁作用范围：能力密钥立即失效，等待中的审批按拒绝结束。
    pub fn destroy(&self) {
        self.scope.destroy();
        self.registry.remove(&self.scope.capability);
    }
}

/// 对话作用范围表：按能力密钥索引。
#[derive(Default)]
struct Registry {
    scopes: Mutex<HashMap<String, Arc<Scope>>>,
}

impl Registry {
    fn register(&self, scope: Arc<Scope>) {
        if let Ok(mut scopes) = self.scopes.lock() {
            scopes.insert(scope.capability.clone(), scope);
        }
    }

    fn lookup(&self, capability: &str) -> Option<Arc<Scope>> {
        self.scopes
            .lock()
            .ok()
            .and_then(|scopes| scopes.get(capability).cloned())
    }

    fn find_by_id(&self, scope_id: &str) -> Option<Arc<Scope>> {
        self.scopes
            .lock()
            .ok()
            .and_then(|scopes| scopes.values().find(|scope| scope.id == scope_id).cloned())
    }

    fn remove(&self, capability: &str) {
        if let Ok(mut scopes) = self.scopes.lock() {
            scopes.remove(capability);
        }
    }
}

/// 工具桥服务端。
pub struct Bridge {
    registry: Arc<Registry>,
    /// 连接层令牌：只证明"是 Z-SSH 启动的那个 opencode 进程连过来的"。
    /// 所有对话共用同一个进程，所以只有一个令牌。
    connection_token: String,
    host: Arc<dyn BridgeHost>,
    pub(crate) audit: Arc<dyn AuditLog>,
    approver: Arc<dyn Approver>,
    mode: Arc<dyn ExecModeProvider>,
    approval_wait: Duration,
    /// 全局在途请求名额（[`MAX_IN_FLIGHT_TOTAL`]）。名额跟着请求任务走，连接断开也不提前归还。
    in_flight: Arc<Semaphore>,
    /// 执行编号 = 本进程随机前缀 + 自增序号：同一个桥内唯一，重启之后也不会和旧对话里
    /// 残留的编号撞上。
    exec_prefix: String,
    exec_counter: AtomicU64,
}

/// [`Bridge::call_tool`] 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    /// 给模型看的文本（成功时是排版好的结果，失败时是桥给的原因；都带执行编号标记）。
    pub text: String,
    pub is_error: bool,
    /// 这次调用的执行编号（带目标的工具才有）。
    pub exec_id: Option<String>,
}

/// 审批等待的结果（比 [`ApprovalDecision`] 多区分"超时"和"scope 销毁"）。
enum WaitOutcome {
    Decision(ApprovalDecision),
    ApprovalTimeout,
    /// 等审批期间请求失效了（作用范围销毁 / 停止 / 目标移出），带原因。
    Invalidated(&'static str),
}

impl Bridge {
    /// 建一个工具桥。返回时生成连接层令牌（所有对话共用一个开放进程）。
    pub fn new(
        host: Arc<dyn BridgeHost>,
        audit: Arc<dyn AuditLog>,
        approver: Arc<dyn Approver>,
        mode: Arc<dyn ExecModeProvider>,
    ) -> Bridge {
        Bridge {
            registry: Arc::new(Registry::default()),
            connection_token: random_hex(32),
            host,
            audit,
            approver,
            mode,
            approval_wait: DEFAULT_APPROVAL_WAIT,
            in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT_TOTAL)),
            exec_prefix: random_hex(3),
            exec_counter: AtomicU64::new(1),
        }
    }

    /// 生成下一个执行编号（`3f2a1b-7`）。
    fn next_exec_id(&self) -> String {
        let n = self.exec_counter.fetch_add(1, Ordering::Relaxed);
        format!("{}-{n}", self.exec_prefix)
    }

    /// 连接层令牌（注入 `zssh-mcp.exe` 的环境变量）。
    pub fn connection_token(&self) -> &str {
        &self.connection_token
    }

    /// 测试用：缩短审批等待。
    pub fn with_approval_wait(mut self, wait: Duration) -> Bridge {
        self.approval_wait = wait;
        self
    }

    /// 新建一个对话作用范围（能力密钥 + 空会话表）。调用方把能力密钥写进该对话的
    /// 系统提示，模型调工具时带上它。
    pub fn create_scope(&self) -> ScopeHandle {
        let scope = Arc::new(Scope {
            id: random_hex(8),
            capability: random_hex(CAPABILITY_BYTES),
            targets: Arc::new(Mutex::new(BTreeMap::new())),
            destroyed: Arc::new(AtomicBool::new(false)),
            revoked: AtomicBool::new(false),
            stop_epoch: Arc::new(AtomicU64::new(0)),
            removals: Arc::new(Mutex::new(HashMap::new())),
            notify: Arc::new(Notify::new()),
        });
        self.registry.register(Arc::clone(&scope));
        ScopeHandle {
            registry: Arc::clone(&self.registry),
            scope,
        }
    }

    /// 接受连接，直到 listener 出错。
    pub async fn serve(self: Arc<Self>, listener: TcpListener) {
        let handshakes = Arc::new(Semaphore::new(MAX_PENDING_HANDSHAKES));
        loop {
            let Ok((stream, _addr)) = listener.accept().await else {
                return;
            };
            // 没认证的连接到上限：直接关掉新连接（本机别的进程反复连、不发令牌也耗不光资源）。
            let Ok(permit) = Arc::clone(&handshakes).try_acquire_owned() else {
                tracing::warn!("工具桥等待握手的连接太多，拒绝新连接");
                drop(stream);
                continue;
            };
            let bridge = Arc::clone(&self);
            tokio::spawn(async move {
                bridge.handle_connection(stream, permit).await;
            });
        }
    }

    /// 一条连接：先连接层令牌握手，然后一条连接上可以有多个请求同时在飞。
    ///
    /// 请求按各自的能力密钥找到对话作用范围（一个进程服务多个对话）。每条请求单独
    /// `spawn` 处理：一条卡在审批上（最长 5 分钟）不会堵住同一条连接上的其它请求
    /// （R-8 第 10 节）。响应按请求里的 `id` 对应，由唯一的写者任务串行写回。
    async fn handle_connection(
        self: &Arc<Self>,
        stream: TcpStream,
        handshake_permit: tokio::sync::OwnedSemaphorePermit,
    ) {
        let (read_half, write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);

        // 第一条消息必须是连接层令牌；10 秒不发就断开。
        let line = match tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            read_bounded_line(&mut reader, HANDSHAKE_MAX_BYTES),
        )
        .await
        {
            Ok(Ok(BoundedLine::Line(line))) => line,
            _ => return,
        };
        match serde_json::from_str::<TokenLine>(line.trim()) {
            Ok(token_line) if constant_time_eq(&token_line.token, &self.connection_token) => {}
            // 令牌不对：整条连接直接断开。
            _ => return,
        }
        // 认证通过：不再占握手名额。
        drop(handshake_permit);

        // 响应写回全部走一个写者任务，写之间不会互相穿插。
        let (write_tx, mut write_rx) = mpsc::channel::<String>(WRITE_QUEUE_DEPTH);
        tokio::spawn(async move {
            let mut write_half = write_half;
            while let Some(payload) = write_rx.recv().await {
                if write_half.write_all(payload.as_bytes()).await.is_err() {
                    return;
                }
            }
        });

        let in_flight = Arc::new(Semaphore::new(MAX_IN_FLIGHT_PER_CONNECTION));
        loop {
            // 先读到上限就停：超长行在分配之前就截住，而不是整行读进来再比较长度（R-9 F15）。
            let line = match read_bounded_line(&mut reader, MAX_LINE_BYTES).await {
                Ok(BoundedLine::Line(line)) => line,
                Ok(BoundedLine::Eof | BoundedLine::TooLong) | Err(_) => return,
            };
            // 在途请求到上限：等有请求处理完再读下一条。
            let Ok(permit) = Arc::clone(&in_flight).acquire_owned().await else {
                return;
            };
            let request = match serde_json::from_str::<RequestEnvelope>(line.trim()) {
                Ok(request) => request,
                Err(_) => {
                    // 解析不出 id：仍然回一条（id 0），让客户端知道这条请求没被接受。
                    let response = ResponseEnvelope {
                        id: 0,
                        result: None,
                        error: Some(ErrorBody {
                            message: "无法解析请求".into(),
                            exec_id: None,
                        }),
                    };
                    let Ok(mut payload) = serde_json::to_string(&response) else {
                        return;
                    };
                    payload.push('\n');
                    if write_tx.send(payload).await.is_err() {
                        return;
                    }
                    continue;
                }
            };
            // 全局名额满了：直接回"忙"，不排队——排队会把这条连接上的读取一起卡住。
            let Ok(global_permit) = Arc::clone(&self.in_flight).try_acquire_owned() else {
                let response = ResponseEnvelope {
                    id: request.id,
                    result: None,
                    error: Some(ErrorBody {
                        message: "Z-SSH 工具桥正在处理的请求太多，请稍后再试".into(),
                        exec_id: None,
                    }),
                };
                let Ok(mut payload) = serde_json::to_string(&response) else {
                    return;
                };
                payload.push('\n');
                if write_tx.send(payload).await.is_err() {
                    return;
                }
                continue;
            };
            let bridge = Arc::clone(self);
            let write_tx = write_tx.clone();
            // 连接断开后已经开始的请求照常跑完，不在中途丢掉：终端占位、临时文件这些收尾都在
            // 请求自己的代码路径里，半路取消会把终端一直标成"忙"。它们各自有审批 / 执行超时，
            // 并且一直占着全局名额，直到真正结束。
            tokio::spawn(async move {
                let _permit = permit;
                let _global_permit = global_permit;
                // 和内置助手的进程内调用走同一个入口（handle_request），安全确认、执行模式、
                // 只读、目标校验都只有这一份。
                let response = bridge.handle_request(request).await;
                let Ok(mut payload) = serde_json::to_string(&response) else {
                    return;
                };
                payload.push('\n');
                // 接收端已经没了说明连接断了；这条响应不用再写。
                let _ = write_tx.send(payload).await;
            });
        }
    }

    /// 处理一条请求，得到响应封装。**外部智能体（经 `zssh-mcp` 的 IPC 连接）和内置助手
    /// （[`Bridge::call_tool`]，进程内）都从这里进**：执行编号、能力密钥校验、目标校验、审批 /
    /// 执行模式 / 只读、审计全在 [`Bridge::dispatch_call`] 里，只有这一份，谁也绕不过去。
    pub(crate) async fn handle_request(&self, request: RequestEnvelope) -> ResponseEnvelope {
        let id = request.id;
        // 需要解析目标的工具，每个请求一个执行编号：成功时放进结果对象，失败时放进错误体，
        // 调用方再把它写成结果文本末尾的一行标记（AI-SCOPE-3）。
        let exec_id = method_carries_exec_id(&request.method).then(|| self.next_exec_id());
        match self.dispatch_call(&request, exec_id.as_deref()).await {
            Ok(mut result) => {
                if let (Some(exec_id), Some(object)) = (&exec_id, result.as_object_mut()) {
                    object.insert(EXEC_ID_FIELD.into(), serde_json::json!(exec_id));
                }
                ResponseEnvelope {
                    id,
                    result: Some(result),
                    error: None,
                }
            }
            Err(message) => ResponseEnvelope {
                id,
                result: None,
                error: Some(ErrorBody { message, exec_id }),
            },
        }
    }

    /// 内置助手的进程内工具调用：不起 `zssh-mcp`、不走本机 TCP，直接进 [`Bridge::handle_request`]，
    /// 和外部智能体经 IPC 来的请求是同一条处理路径（同样的能力密钥校验、目标校验、安全确认、
    /// 执行模式、只读判断、终端 / 后台执行、审计）。
    ///
    /// 返回的文本和 `zssh-mcp` 给外部智能体的完全一致（同一份排版代码，结果末尾带执行编号标记）；
    /// 失败（被拒绝、审批超时、参数不对……）时 `is_error` 为真，文本是桥给的原因。
    ///
    /// 调用方想让请求在被取消时也走完收尾（终端占位、临时文件）：把这个调用 `tokio::spawn`
    /// 出去再等结果，别直接丢掉 future——和 IPC 那边"连接断了请求照常跑完"一样。
    pub async fn call_tool(
        &self,
        capability: &str,
        method: &str,
        params: serde_json::Value,
    ) -> ToolOutcome {
        // 和 IPC 一样占全局名额；满了直接回"忙"，不排队。
        let Ok(_global_permit) = Arc::clone(&self.in_flight).try_acquire_owned() else {
            return ToolOutcome {
                text: "Z-SSH 工具桥正在处理的请求太多，请稍后再试".into(),
                is_error: true,
                exec_id: None,
            };
        };
        let response = self
            .handle_request(RequestEnvelope {
                id: 0,
                method: method.to_owned(),
                capability: capability.to_owned(),
                params,
            })
            .await;
        match response.error {
            // 失败也带编号：被拒绝 / 超时 / 作用范围销毁的调用，界面一样要对回正确的卡片。
            Some(error) => ToolOutcome {
                text: match error.exec_id.as_deref() {
                    Some(exec_id) if !exec_id.is_empty() => {
                        crate::ai::protocol::with_exec_marker(&error.message, exec_id)
                    }
                    _ => error.message.clone(),
                },
                is_error: true,
                exec_id: error.exec_id,
            },
            None => {
                let result = response.result.unwrap_or(serde_json::Value::Null);
                ToolOutcome {
                    text: crate::ai::tool_text::format_tool_result(method, &result),
                    is_error: false,
                    exec_id: result
                        .get(EXEC_ID_FIELD)
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                }
            }
        }
    }

    /// 按请求里的能力密钥找到对话作用范围。
    fn scope_for(&self, request: &RequestEnvelope) -> Result<Arc<Scope>, String> {
        if request.capability.is_empty() {
            return Err("缺少对话能力密钥".into());
        }
        let scope = self
            .registry
            .lookup(&request.capability)
            .ok_or_else(|| "对话能力密钥无效或已过期".to_string())?;
        if scope.is_destroyed() {
            return Err("作用范围已销毁".into());
        }
        Ok(scope)
    }

    /// 处理一条请求（自己生成执行编号；测试和不关心响应封装的调用方用）。
    #[cfg(test)]
    async fn dispatch(&self, request: &RequestEnvelope) -> Result<serde_json::Value, String> {
        let exec_id = method_carries_exec_id(&request.method).then(|| self.next_exec_id());
        self.dispatch_call(request, exec_id.as_deref()).await
    }

    /// 处理一条请求。`exec_id` 是这次调用的执行编号（带目标的工具都有）。
    async fn dispatch_call(
        &self,
        request: &RequestEnvelope,
        exec_id: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        let scope = self.scope_for(request)?;
        let call_id = exec_id.unwrap_or_default();
        match request.method.as_str() {
            METHOD_LIST_TARGETS => self.list_targets(&scope).await,
            METHOD_REMEMBER_HOST => {
                let params: RememberHostParams = serde_json::from_value(request.params.clone())
                    .map_err(|err| format!("参数不正确：{err}"))?;
                let target_id = resolve_target(&scope, params.target_id.as_deref())?;
                let notes = crate::ai::host_memory::clean_memory(&params.notes)?;
                self.host.write_memory(&target_id, &notes).await?;
                Ok(serde_json::json!({
                    "saved_chars": notes.chars().count(),
                    "max_chars": crate::ai::host_memory::MAX_MEMORY_CHARS,
                }))
            }
            METHOD_READ_TERMINAL => {
                let params: ReadTerminalParams = serde_json::from_value(request.params.clone())
                    .map_err(|err| format!("参数不正确：{err}"))?;
                let resolved = self
                    .resolve_exec_target(
                        &scope,
                        params.target_id.as_deref(),
                        "read_terminal",
                        "",
                        call_id,
                    )
                    .await?;
                let max_lines = clamp_max_lines(params.max_lines);
                // 锁屏时连看终端也要先确认（平时不问）。
                if let Err((_, message)) = self
                    .confirm_query(
                        &scope,
                        &resolved.ticket,
                        &resolved.summary,
                        resolved.session_id.as_deref(),
                        "read_terminal",
                        "",
                        call_id,
                        Vec::new(),
                    )
                    .await
                {
                    return Err(format!("{message}，没有读取"));
                }
                let lines = self
                    .host
                    .read_terminal(&resolved.exec_id, max_lines)
                    .await?;
                // 读的过程中（可能在等自动连接）停止了 / 目标被移出了：不把内容交回去。
                if let Some(reason) = scope.ticket_refusal(&resolved.ticket) {
                    return Err(reason.to_string());
                }
                Ok(serde_json::json!({ "lines": lines, "max_lines": max_lines }))
            }
            METHOD_RUN_COMMAND => {
                self.run_command(&scope, request.params.clone(), call_id)
                    .await
            }
            METHOD_LIST_DIR | METHOD_READ_FILE | METHOD_WRITE_FILE | METHOD_STAT => {
                self.file_tool(
                    &scope,
                    request.method.as_str(),
                    request.params.clone(),
                    call_id,
                )
                .await
            }
            other => Err(format!("未知方法：{other}")),
        }
    }

    /// 目标列表，带上每台主机以前记下的笔记。全部放行模式下不带：没有人工确认兜底，
    /// 笔记里被埋的指令会被直接执行。读不到笔记不影响列表。
    async fn list_targets(&self, scope: &Arc<Scope>) -> Result<serde_json::Value, String> {
        let with_memory = self.mode.mode() != ExecMode::Auto;
        let mut targets = Vec::new();
        let mut unavailable = Vec::new();
        for target in scope.target_list() {
            // 不能执行命令的目标（Telnet / 串口）不列成可操作目标，单独说明原因，
            // 免得模型反复尝试。
            if let Some(reason) = self.host.target_refusal(&target.id).await {
                unavailable.push(serde_json::json!({
                    "id": target.id,
                    "host_label": target.host_label,
                    "reason": reason,
                }));
                continue;
            }
            let mut entry = serde_json::to_value(&target).map_err(|err| err.to_string())?;
            if with_memory {
                if let Ok(Some(memory)) = self.host.read_memory(&target.id).await {
                    if !memory.trim().is_empty() {
                        entry["memory"] =
                            serde_json::json!(crate::ai::host_memory::memory_for_agent(&memory));
                    }
                }
            }
            targets.push(entry);
        }
        let mut result = serde_json::json!({ "targets": targets });
        if !unavailable.is_empty() {
            result["unavailable"] = serde_json::json!(unavailable);
        }
        if !with_memory {
            result["memory_note"] =
                serde_json::json!("Host notes are not shown in full-access mode.");
        }
        Ok(result)
    }

    async fn run_command(
        &self,
        scope: &Arc<Scope>,
        params: serde_json::Value,
        call_id: &str,
    ) -> Result<serde_json::Value, String> {
        let params: RunCommandParams =
            serde_json::from_value(params).map_err(|err| format!("参数不正确：{err}"))?;
        // 命令是当键盘输入打进终端的：控制字符会变成按键（Ctrl+U 删掉包装脚本的前缀、
        // 回车提前提交……），单引号挡不住。审批和两条执行通道之前就拒绝（R-8 第 1 节）。
        crate::ai::command_text::reject_forbidden_control_chars(&params.command)?;
        let ResolvedTarget {
            summary,
            ticket,
            session_id,
            exec_id,
            ..
        } = self
            .resolve_exec_target(
                scope,
                params.target_id.as_deref(),
                "run_command",
                &params.command,
                call_id,
            )
            .await?;
        // 票据、审计、闸都按逻辑目标记；交给宿主执行的是 exec_id（有绑定会话就是它）。
        let target_id = summary.id.clone();
        let verdict = assess(&params.command);

        // 手动模式每条命令都要批；自动模式全部直接执行，危险判断只用于标记和审计。
        let (decision, refusal) = self
            .decide(
                scope,
                &ticket,
                ApprovalRequest {
                    scope: scope.id.clone(),
                    exec_id: call_id.to_owned(),
                    target_id: target_id.clone(),
                    session_id: session_id.clone(),
                    host_label: summary.host_label.clone(),
                    command: params.command.clone(),
                    danger: verdict.level,
                    reasons: verdict
                        .reasons
                        .iter()
                        .zip(&verdict.rules)
                        .map(|(text, rule)| ApprovalReason::command(rule, text.clone()))
                        .collect(),
                    file: None,
                },
            )
            .await;

        if let Some(message) = refusal {
            self.record_audit(
                scope,
                &summary,
                &params.command,
                &verdict,
                decision,
                None,
                None,
                None,
                None,
            );
            return Err(message);
        }

        let wait = clamp_timeout_secs(params.timeout_sec);
        // 放行之后到真正执行之间还可能等很久（探测 shell、自动连接）：执行前靠它再核对一次。
        let gate = ExecGate::new(scope, &ticket);

        // 决定执行位置：终端空闲就就地跑，否则降级到后台（AI-2c）。
        // 命令里有 Tab 时不走终端：行编辑器会把它当补全键，实际执行的命令会变（R-7 第 0 节）。
        let (location, fallback_reason) = if crate::ai::command_text::contains_tab(&params.command)
        {
            (
                ExecLocation::Background,
                Some(crate::ai::command_text::TAB_FALLBACK_REASON.to_string()),
            )
        } else {
            match self.host.exec_placement(&exec_id).await {
                ExecPlacement::Terminal => (ExecLocation::Terminal, None),
                ExecPlacement::Background { reason } => (ExecLocation::Background, Some(reason)),
            }
        };

        if location == ExecLocation::Terminal {
            // exec_placement 可能刚探测完远端 shell（第一次要等一会儿）：这期间点了停止 / 移出了
            // 目标，就不再写命令，把抢到的终端还回去（R-9 F11、F12）。
            if let Err(reason) = gate.check() {
                self.host.release_terminal(&exec_id).await;
                // 审计按"没有执行"记：批准过，但在执行前作废了。
                self.record_audit(
                    scope,
                    &summary,
                    &params.command,
                    &verdict,
                    ExecDecision::Denied,
                    None,
                    None,
                    None,
                    None,
                );
                return Err(reason);
            }
            let marker = crate::ai::pty::new_marker();
            let payload = format!(
                "{}{}",
                crate::ai::pty::build_clear_prefix(),
                crate::ai::pty::build_wrapped_command(&params.command, &marker),
            );
            let request = TerminalExecRequest {
                target_id: exec_id.clone(),
                scope_id: scope.id.clone(),
                command: params.command.clone(),
                payload,
                marker,
                wait,
                gate: gate.clone(),
            };
            match self.host.run_in_terminal(request).await {
                Ok(capture) => {
                    let exit_code = capture.exit_code.map(i64::from);
                    self.record_audit(
                        scope,
                        &summary,
                        &params.command,
                        &verdict,
                        decision,
                        exit_code,
                        Some(capture.duration_ms as i64),
                        Some(ExecLocation::Terminal),
                        None,
                    );
                    let response = RunCommandResponse {
                        stdout: capture.output,
                        stderr: String::new(),
                        stdout_truncated_bytes: 0,
                        stderr_truncated_bytes: 0,
                        exit_code: capture.exit_code.map(|c| c.max(0) as u32),
                        duration_ms: capture.duration_ms,
                        danger: verdict.level,
                        location: ExecLocation::Terminal,
                        fallback_reason: None,
                        timed_out: capture.timed_out,
                    };
                    return serde_json::to_value(response).map_err(|err| err.to_string());
                }
                Err(message) => {
                    // 宿主在写命令之前发现请求失效了：命令没有执行，也不能换个通道再执行。
                    if let Err(reason) = gate.check() {
                        self.record_audit(
                            scope,
                            &summary,
                            &params.command,
                            &verdict,
                            ExecDecision::Denied,
                            None,
                            None,
                            None,
                            None,
                        );
                        return Err(reason);
                    }
                    // 写到一半会话关了：命令可能已经进了终端，不能换个通道再执行一遍。
                    if message == terminal_write::WRITE_INTERRUPTED {
                        self.record_audit(
                            scope,
                            &summary,
                            &params.command,
                            &verdict,
                            decision,
                            None,
                            None,
                            Some(ExecLocation::Terminal),
                            None,
                        );
                        return Err(message);
                    }
                    // 终端执行失败：退回后台通道，并记下原因。
                    tracing::warn!(%message, "终端执行失败，降级到后台通道");
                    return self
                        .run_background(
                            scope,
                            &gate,
                            &summary,
                            &exec_id,
                            &params.command,
                            &verdict,
                            decision,
                            wait,
                            Some(format!("终端执行失败：{message}")),
                        )
                        .await;
                }
            }
        }

        self.run_background(
            scope,
            &gate,
            &summary,
            &exec_id,
            &params.command,
            &verdict,
            decision,
            wait,
            fallback_reason,
        )
        .await
    }

    /// 按执行模式决定一次操作能不能做：自动模式直接放行；手动模式弹确认，等人答、超时或
    /// 请求失效（作用范围销毁、用户点了停止、目标被移出对话）。返回 `(处置, 拒绝原因)`，
    /// 有拒绝原因就不能执行。
    ///
    /// 放行之前再核对一次 `ticket`：批准卡片可能是在目标被移出 / 停止之后才点的，
    /// 不能按旧的批准去执行（R-9 F11、F12）。
    pub(crate) async fn decide(
        &self,
        scope: &Arc<Scope>,
        ticket: &RequestTicket,
        request: ApprovalRequest,
    ) -> (ExecDecision, Option<String>) {
        self.decide_with(scope, ticket, request, false).await
    }

    /// 同 [`Bridge::decide`]；`force_confirm` 为真时不论哪种模式都要人确认（只读文件工具
    /// 读到敏感文件时用，那条路径本来不经过批准）。
    pub(crate) async fn decide_with(
        &self,
        scope: &Arc<Scope>,
        ticket: &RequestTicket,
        mut request: ApprovalRequest,
        force_confirm: bool,
    ) -> (ExecDecision, Option<String>) {
        let mode = self.mode.mode();
        // 锁屏：任何模式（含全部放行）都要确认，原因写在卡片上。
        let locked = self.mode.force_confirm();
        if locked {
            request.reasons.push(ApprovalReason::Locked);
        }
        let auto = !force_confirm
            && !locked
            && match mode {
                ExecMode::Auto => true,
                ExecMode::ReadOnly => {
                    let read_only = request.file.is_none()
                        && request.danger == DangerLevel::Safe
                        && crate::ai::readonly::is_read_only(&request.command);
                    // 只读命令也可能把私钥、云凭据、环境变量读出来交给模型服务商：命中敏感来源的
                    // 降级为要确认，并在卡片上写明原因。命令本身不算只读时同样写明（和逐条确认模式
                    // 的卡片一致：那边命中就写，不看命令是不是只读）。
                    match crate::ai::sensitive::sensitive_path_in_command(&request.command) {
                        Some(hit) => {
                            request.reasons.push(hit.reason());
                            false
                        }
                        None => read_only,
                    }
                }
                ExecMode::Manual => {
                    // 逐条确认本来就问；命中敏感路径时在卡片上写明，用户批之前看得到。
                    if let Some(hit) =
                        crate::ai::sensitive::sensitive_path_in_command(&request.command)
                    {
                        request.reasons.push(hit.reason());
                    }
                    false
                }
            };
        let (decision, refusal) = match auto {
            true => (ExecDecision::Auto, None),
            false => {
                let approve = self.approver.approve(request);
                tokio::pin!(approve);
                let outcome = tokio::select! {
                    decision = &mut approve => WaitOutcome::Decision(decision),
                    _ = tokio::time::sleep(self.approval_wait) => WaitOutcome::ApprovalTimeout,
                    reason = scope.wait_invalidated(ticket) => WaitOutcome::Invalidated(reason),
                };
                match outcome {
                    WaitOutcome::Decision(ApprovalDecision::Allowed) => {
                        (ExecDecision::Allowed, None)
                    }
                    WaitOutcome::Decision(ApprovalDecision::Denied) => {
                        (ExecDecision::Denied, Some("用户拒绝".to_string()))
                    }
                    WaitOutcome::Decision(ApprovalDecision::Timeout)
                    | WaitOutcome::ApprovalTimeout => (
                        ExecDecision::Timeout,
                        Some(if self.mode.force_confirm() {
                            LOCKED_TIMEOUT_MESSAGE.to_string()
                        } else {
                            "审批超时，已按拒绝处理".to_string()
                        }),
                    ),
                    WaitOutcome::Invalidated(reason) => {
                        (ExecDecision::Denied, Some(reason.to_string()))
                    }
                }
            }
        };
        if refusal.is_none() {
            if let Some(reason) = scope.ticket_refusal(ticket) {
                return (ExecDecision::Denied, Some(reason.to_string()));
            }
        }
        (decision, refusal)
    }

    /// 只读类请求（读目录、读文件、看终端）的前置确认。平时这些不问；两种情况要问：
    /// 应用锁屏（[`ExecModeProvider::force_confirm`]），或者调用方给了原因（读敏感文件）。
    /// 返回处置：不用问是 `Auto`，用户批准是 `Allowed`；被拒 / 超时 / 请求失效返回
    /// （处置，原因），调用方负责记审计和报错。
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn confirm_query(
        &self,
        scope: &Arc<Scope>,
        ticket: &RequestTicket,
        summary: &TargetSummary,
        session_id: Option<&str>,
        tool: &str,
        key: &str,
        call_id: &str,
        reasons: Vec<ApprovalReason>,
    ) -> Result<ExecDecision, (ExecDecision, String)> {
        if reasons.is_empty() && !self.mode.force_confirm() {
            return Ok(ExecDecision::Auto);
        }
        let command = if key.is_empty() {
            tool.to_owned()
        } else {
            format!("{tool} {key}")
        };
        let (decision, refusal) = self
            .decide_with(
                scope,
                ticket,
                ApprovalRequest {
                    scope: scope.id.clone(),
                    exec_id: call_id.to_owned(),
                    target_id: summary.id.clone(),
                    session_id: session_id.map(str::to_owned),
                    host_label: summary.host_label.clone(),
                    command,
                    danger: DangerLevel::Safe,
                    reasons,
                    file: None,
                },
                true,
            )
            .await;
        match refusal {
            Some(message) => Err((decision, message)),
            None => Ok(decision),
        }
    }

    /// 后台 exec 通道执行（AI-1 的老路径）。
    #[allow(clippy::too_many_arguments)]
    async fn run_background(
        &self,
        scope: &Arc<Scope>,
        gate: &ExecGate,
        summary: &TargetSummary,
        target_id: &str,
        command: &str,
        verdict: &crate::ai::safety::SafetyVerdict,
        decision: ExecDecision,
        wait: Duration,
        fallback_reason: Option<String>,
    ) -> Result<serde_json::Value, String> {
        // 执行期间用户点了停止：置位取消标志——还没发出去的命令不再发，已经发出去的关掉执行通道，
        // 不再等它跑完（R-9 F12）。宿主在发命令之前还会用 gate 核对目标有没有被移出。
        let exec =
            self.host
                .exec_cancellable(target_id, command, wait, EXEC_MAX_STREAM_BYTES, gate);
        tokio::pin!(exec);
        let mut stopped = None;
        let result = tokio::select! {
            result = &mut exec => result,
            reason = scope.wait_stopped(&gate.ticket) => {
                stopped = Some(reason);
                gate.cancel.store(true, Ordering::SeqCst);
                exec.await
            }
        };
        match result {
            Ok(capture) => {
                let exit_code = if capture.timed_out {
                    None
                } else {
                    capture.exit_code.map(i64::from)
                };
                self.record_audit(
                    scope,
                    summary,
                    command,
                    verdict,
                    decision,
                    exit_code,
                    Some(capture.duration_ms as i64),
                    Some(ExecLocation::Background),
                    fallback_reason.clone(),
                );
                if let Some(reason) = stopped {
                    // 已经发生的修改不会回滚：只是不再等它、也不把结果当成正常完成交回去。
                    return Err(if capture.cancelled {
                        format!("{reason}（后台命令的执行通道已关闭，命令可能已经执行了一部分）")
                    } else {
                        format!("{reason}（命令在取消前已经执行完）")
                    });
                }
                if capture.timed_out {
                    return Err("已超时".into());
                }
                let response = RunCommandResponse {
                    stdout: capture.stdout,
                    stderr: capture.stderr,
                    stdout_truncated_bytes: capture.stdout_truncated_bytes,
                    stderr_truncated_bytes: capture.stderr_truncated_bytes,
                    exit_code: capture.exit_code,
                    duration_ms: capture.duration_ms,
                    danger: verdict.level,
                    location: ExecLocation::Background,
                    fallback_reason,
                    timed_out: false,
                };
                serde_json::to_value(response).map_err(|err| err.to_string())
            }
            Err(message) => {
                self.record_audit(
                    scope,
                    summary,
                    command,
                    verdict,
                    // 宿主在发命令之前发现请求失效了：没有执行，按拒绝记。
                    if gate.check().is_err() {
                        ExecDecision::Denied
                    } else {
                        decision
                    },
                    None,
                    None,
                    Some(ExecLocation::Background),
                    fallback_reason,
                );
                Err(message)
            }
        }
    }

    /// 让 AI 停止这个对话正在终端里跑的 AI 命令（向那个终端发 Ctrl+C）。
    ///
    /// 停止按钮（`ai_agent_abort`）的入口：宿主通过终端执行登记表
    /// （[`crate::ai::runs::TerminalRuns`]）确认这个对话真的有一条命令在跑，
    /// 没有就什么都不发。
    /// 停止按钮：作废这个对话在这一刻之前开始的所有工具请求——等审批的按拒绝结束，
    /// 后台命令关闭执行通道，还没开始写的文件不再写（R-9 F12）。终端里的命令另由
    /// [`Bridge::interrupt_terminal`] 发 Ctrl+C。已经发生的修改不会回滚。
    pub fn stop_scope(&self, scope_id: &str) {
        if let Some(scope) = self.registry.find_by_id(scope_id) {
            scope.stop();
        }
    }

    pub async fn interrupt_terminal(&self, scope_id: &str) {
        self.host.interrupt_terminal(scope_id).await;
    }

    #[allow(clippy::too_many_arguments)]
    fn record_audit(
        &self,
        scope: &Arc<Scope>,
        summary: &TargetSummary,
        command: &str,
        verdict: &crate::ai::safety::SafetyVerdict,
        decision: ExecDecision,
        exit_code: Option<i64>,
        duration_ms: Option<i64>,
        location: Option<ExecLocation>,
        fallback_reason: Option<String>,
    ) {
        self.audit.record(AuditEntry {
            at: now_millis(),
            scope: scope.id.clone(),
            host_label: summary.host_label.clone(),
            command: command.to_string(),
            danger: verdict.level,
            decision,
            exit_code,
            duration_ms,
            location,
            fallback_reason,
            tool: "run_command".to_string(),
            path: None,
            bytes: None,
            backup_path: None,
        });
    }
}

impl Bridge {
    /// 解析一次工具调用的执行位置：目标（不填用"当前"）、执行用的会话（界面绑定的优先）。
    ///
    /// 解析完立刻回报给宿主（AI-SCOPE-2 第 2 节）——在审批、自动连接这些可能等很久的步骤
    /// 之前，界面马上就能在卡片上显示"这次会落在哪台主机"。
    pub(crate) async fn resolve_exec_target(
        &self,
        scope: &Arc<Scope>,
        target_id: Option<&str>,
        tool: &str,
        key: &str,
        call_id: &str,
    ) -> Result<ResolvedTarget, String> {
        let target_id = resolve_target(scope, target_id)?;
        let summary = scope
            .summary(&target_id)
            .ok_or_else(|| SCOPE_TARGET_ERROR.to_string())?;
        // 非 SSH 主机（Telnet / 串口）：在找会话、自动连接、审批之前就拒绝，说清楚原因。
        if let Some(reason) = self.host.target_refusal(&target_id).await {
            return Err(reason);
        }
        // 票据必须在任何 await 之前记下：找会话 / 等连接期间用户点了停止，或者把目标移出又
        // 加回来，这条请求都要作废；先解析再记票据会把这些取消状态漏掉。
        let ticket = scope.ticket(&target_id);
        let session_id = self
            .host
            .execution_session(&target_id, summary.session_id.as_deref())
            .await;
        // 解析期间已经失效：不回报、不执行。
        if let Some(reason) = scope.ticket_refusal(&ticket) {
            return Err(reason.to_string());
        }
        self.host.report_target(ExecTargetReport {
            scope: scope.id.clone(),
            exec_id: call_id.to_owned(),
            tool: tool.to_owned(),
            key: key.to_owned(),
            target_id: target_id.clone(),
            session_id: session_id.clone(),
            host_label: summary.host_label.clone(),
        });
        let exec_id = session_id.clone().unwrap_or(target_id);
        Ok(ResolvedTarget {
            summary,
            ticket,
            session_id,
            exec_id,
        })
    }
}

/// 解析目标 ID：不填时用"当前焦点"那台；填了必须是目标列表里的。
pub(crate) fn resolve_target(
    scope: &Arc<Scope>,
    target_id: Option<&str>,
) -> Result<String, String> {
    let list = scope.target_list();
    match target_id {
        Some(id) if !id.is_empty() => {
            if list.iter().any(|t| t.id == id) {
                Ok(id.to_string())
            } else {
                Err(SCOPE_TARGET_ERROR.to_string())
            }
        }
        _ => list
            .iter()
            .find(|t| t.current)
            .or_else(|| list.first())
            .map(|t| t.id.clone())
            .ok_or_else(|| "本次对话没有可操作的目标（请在输入框里 @ 一台主机）".to_string()),
    }
}

#[cfg(test)]
mod tests;
