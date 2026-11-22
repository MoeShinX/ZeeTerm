//! 对话的数据模型：消息、工具卡片、确认细节和纯逻辑辅助函数。
//!
//! 这里只有数据与纯函数，不碰界面也不碰宿主；状态机在 `state`，渲染在 `view`。

use super::*;

/// 内容预览最多显示多少个字符（转义之后）。
pub(super) const PREVIEW_LIMIT: usize = 80;

/// 递归复制 JSON，把凭据类键的值换成占位；原值仍留给历史存储（由 core 再抹一次）。
/// 键名判断用 core 历史存储的同一份词表（`history::is_secret_key`），不在这里另抄一份。
pub(super) fn redact_tool_input(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let value = if is_secret_key(key) {
                        json!(t(ChatMsg::HiddenSecret))
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

/// 工具卡片的输入：演示脚本用固定文案，真实事件用宿主给的参数。
#[derive(Clone, Debug, PartialEq)]
pub(super) enum ToolInput {
    Demo(DemoInput),
    Real {
        /// 一行摘要。
        summary: String,
        /// 宿主给的原始参数；存历史时原样写进去（截断和脱敏由 core 做）。
        input: Value,
    },
}

impl ToolInput {
    pub(super) fn summary(&self) -> String {
        match self {
            Self::Demo(input) => input.summary(),
            Self::Real { summary, .. } => summary.clone(),
        }
    }

    /// 命令全文（`run_command` 的 `command` 参数）；没有命令的工具返回 `None`。
    pub(super) fn command(&self) -> Option<String> {
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
    pub(super) fn json(&self) -> Value {
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
pub(super) fn content_preview(content: &str) -> String {
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

/// 一条工具调用的确认状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ApprovalState {
    NotRequired,
    Pending,
    Allowed,
    Denied,
}

/// 工具卡片的状态；`AwaitingApproval` 和 `Running` 都不能显示成已完成。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ToolStatus {
    AwaitingApproval,
    Running,
    /// 真实模式的确认卡片：用户已经回答（命令结果在另一张工具卡片上）。
    Answered,
    Done,
    Failed,
    Stopped,
}

impl ToolStatus {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::AwaitingApproval => t(ChatMsg::StatusAwaitingApproval),
            Self::Running => t(ChatMsg::StatusRunning),
            Self::Answered => t(ChatMsg::StatusAnswered),
            Self::Done => t(ChatMsg::StatusDone),
            Self::Failed => t(ChatMsg::StatusFailed),
            Self::Stopped => t(ChatMsg::StatusStopped),
        }
    }
}

/// 卡片状态文字：命令跑完带退出码时写「退出码 N」（0.2.14 同），别的用状态名。
pub(super) fn card_status_text(card: &ToolCard) -> String {
    match (card.status, card.output.as_deref()) {
        (ToolStatus::Done, Some(output)) => agent_tools::exit_code_of(output).map_or_else(
            || card.status.label().to_owned(),
            |code| format!("{}{code}", t(ChatMsg::ExitCodePrefix)),
        ),
        _ => card.status.label().to_owned(),
    }
}

/// 卡片上结果带出来的几行细节（文字, 是不是警示色）：写系统路径、原文件备份到哪儿、属主没能改回去、
/// 命令在终端里还是后台执行的。界面和调试断言共用这一份，不会各写各的。
pub(super) fn card_detail_lines(card: &ToolCard) -> Vec<(String, bool)> {
    let Some(output) = card.output.as_deref() else {
        return Vec::new();
    };
    let tool = agent_tools::bare_tool_name(&card.tool);
    let mut lines = Vec::new();
    if tool == "write_file"
        && let Some(reason) = file_danger_reason(output)
    {
        lines.push((
            format!("{}{reason}", t(ChatMsg::WroteSystemPathPrefix)),
            true,
        ));
    }
    if let Some(backup) = agent_tools::backup_path_of(output) {
        lines.push((format!("{}{backup}", t(ChatMsg::BackupPathPrefix)), false));
    }
    if let Some((from, to)) = agent_tools::owner_change_of(output) {
        lines.push((
            format!(
                "{}{from}{}{to}{}",
                t(ChatMsg::OwnerChangedPrefix),
                t(ChatMsg::OwnerChangedMiddle),
                t(ChatMsg::OwnerChangedSuffix)
            ),
            true,
        ));
    }
    if tool == "run_command" {
        match agent_tools::location_of(output) {
            Some(Location::Terminal) => lines.push((t(ChatMsg::RanInTerminal).to_owned(), false)),
            Some(Location::Background) => {
                let reason = agent_tools::fallback_reason_of(output)
                    .map(|reason| {
                        format!(
                            "{}{reason}{}",
                            t(ChatMsg::ParenLeft),
                            t(ChatMsg::ParenRight)
                        )
                    })
                    .unwrap_or_default();
                lines.push((format!("{}{reason}", t(ChatMsg::RanInBackground)), false));
            }
            None => {}
        }
    }
    lines
}

/// 确认卡片要给用户审的内容：整份桥给的请求（命令全文、危险等级、原因、文件差异）
/// 加两个展开状态。回答之后也留着，回看时能确认当时批准的是什么。
#[derive(Clone, Debug)]
pub(super) struct ApprovalDetail {
    pub(super) request: ApprovalRequest,
    /// 命令框展开（默认限高滚动，超长时给「展开全文」）。
    pub(super) command_expanded: bool,
    /// 文件差异展开（默认只显示前 30 行）。
    pub(super) diff_expanded: bool,
}

impl ApprovalDetail {
    pub(super) fn new(request: ApprovalRequest) -> Self {
        Self {
            request,
            command_expanded: false,
            diff_expanded: false,
        }
    }

    /// 危险命令高亮：和 0.2.14 一样直接看桥给的 `danger` 等级（core 的 `safety::assess`）。
    pub(super) fn dangerous(&self) -> bool {
        self.request.danger == DangerLevel::Dangerous
    }

    /// 要显示的命令文本：`write_file` 的 `command` 只是 `write_file <路径>`，
    /// 细节在文件区，不重复显示。
    pub(super) fn command_text(&self) -> Option<String> {
        self.request
            .file
            .is_none()
            .then(|| visible_command_text(&self.request.command))
    }
}

/// 一张工具卡片。`call_id` 是驱动给的调用编号（演示脚本是 `call-1`，真实模式是
/// 宿主的工具调用 id，确认卡片是 `approval-<请求 id>`）。
#[derive(Clone, Debug)]
pub(super) struct ToolCard {
    pub(super) call_id: String,
    /// 工具名；真实模式直接用驱动上报的名字。
    pub(super) tool: String,
    pub(super) input: ToolInput,
    pub(super) approval: ApprovalState,
    pub(super) status: ToolStatus,
    pub(super) output: Option<String>,
    pub(super) error: Option<String>,
    /// 输出默认展开；点标题行可以收起。
    pub(super) output_expanded: bool,
    /// 输出显示全部（默认只显示前几十行，防止长输出拖慢界面）。
    pub(super) output_full: bool,
    /// 命令框展开（工具卡片上显示的命令，没有确认请求时用）。
    pub(super) command_expanded: bool,
    /// 确认卡片的内容；非确认卡片为 `None`。
    pub(super) detail: Option<ApprovalDetail>,
    /// 执行层回报的实际执行位置（哪台主机、哪个终端）；还没回报 / 不带目标的工具为 `None`。
    pub(super) exec: Option<ExecInfo>,
    /// 回报里的主机名要不要显示在卡片上（对话里有多个目标，或者当时没有「当前」）。
    pub(super) exec_multi: bool,
}

impl ToolCard {
    pub(super) fn new(call_id: String, tool: String, input: ToolInput) -> Self {
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
    pub(super) fn is_approval(&self) -> bool {
        self.call_id.starts_with("approval-")
    }

    /// 命令退出码不是 0（只看已完成的调用）。
    pub(super) fn exit_failed(&self) -> bool {
        self.status == ToolStatus::Done
            && self
                .output
                .as_deref()
                .and_then(agent_tools::exit_code_of)
                .is_some_and(|code| code != 0)
    }

    /// 工具调用分组用的摘要：等批准的 / 正在执行的绝不折叠。
    pub(super) fn view(&self) -> CallView {
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
    pub(super) fn input_json(&self) -> Value {
        self.input.json()
    }

    /// 卡片要不要标"危险"：确认请求的等级、工具输入里的 `danger` 标记、
    /// `write_file` 结果里"写系统路径"的提示，三者任一即可（0.2.14 同样三处）。
    pub(super) fn dangerous(&self) -> bool {
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
pub(super) struct PendingApproval {
    pub(super) call_id: String,
    pub(super) tool: String,
    pub(super) input: ToolInput,
}

/// 真实模式里等用户回答的确认请求；`id` 用来回填 `ClaudeSession::resolve_approval`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RealApproval {
    pub(super) id: u64,
    pub(super) call_id: String,
}

/// 真实模式下的一条提示（错误 / 重试 / 续接失败），显示在助手消息里。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Notice {
    pub(super) text: String,
    pub(super) error: bool,
}

/// 助手消息下面带按钮的提示卡（Tokenway，V2-A5）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CardKind {
    /// 需要充值：按钮「去充值」打开充值页。
    TopUp,
    /// 授权失效：按钮「重新连接」打开连接对话框。
    Reconnect,
}

/// 一条助手消息；文本和思考都往同一条里累积，工具卡片按顺序挂在下面。
#[derive(Clone, Debug, Default)]
pub(super) struct AssistantMessage {
    pub(super) text: String,
    pub(super) reasoning: String,
    pub(super) reasoning_expanded: bool,
    pub(super) tools: Vec<ToolCard>,
    pub(super) notices: Vec<Notice>,
    /// 带按钮的提示卡（需要充值 / 授权失效）。只是界面状态，不进历史。
    pub(super) cards: Vec<CardKind>,
    pub(super) stopped: bool,
    /// 正文显示原文还是渲染：「原文 / 渲染」按钮切换，只影响显示。
    pub(super) show_raw: bool,
    /// 超长正文已展开（默认只渲染前面一段）。
    pub(super) body_expanded: bool,
    /// 展开着的工具调用分组（键 = 这一组第一张卡片的下标，卡片只增不减，下标稳定）。
    pub(super) open_groups: HashSet<usize>,
}

/// 助手正文的两种显示：Markdown 渲染，或包在代码块里的原始 Markdown。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BodyMode {
    Rendered,
    Raw,
}

impl BodyMode {
    /// 切换按钮上的文字：当前是渲染就让用户看原文，反之亦然。
    pub(super) fn toggle_label(self) -> &'static str {
        match self {
            Self::Rendered => t(ChatMsg::BodyRaw),
            Self::Raw => t(ChatMsg::BodyRendered),
        }
    }
}

/// 正文当前该按哪种模式显示；`show_raw` 只影响显示，不动累积和历史。
pub(super) fn body_mode(message: &AssistantMessage) -> BodyMode {
    if message.show_raw {
        BodyMode::Raw
    } else {
        BodyMode::Rendered
    }
}

/// 把原始 Markdown 包进代码围栏。围栏由 [`markdown_fence`] 按正文里最长的反引号串
/// 现算：正文里再多的反引号也关不掉外层，也就不会有内容"逃"出来按 Markdown 渲染；
/// 换行原样保留，代码块的「复制」拿到的就是模型返回的原文。
pub(super) fn raw_markdown_block(text: &str) -> String {
    markdown_fence(text)
}

/// 正文超长时的折叠：渲染 Markdown 每帧都要解析，几十万字符的正文会拖慢整个窗口。
/// 返回要显示的正文和是否被折叠；`expanded` 为真时全显。
pub(super) fn clipped_body(text: &str, expanded: bool) -> (String, bool) {
    if expanded {
        return (text.to_owned(), false);
    }
    let clip = clip_text(text, usize::MAX, BODY_PREVIEW_CHARS);
    (clip.text, clip.clipped)
}

/// 正文 TextView 的 Markdown 源；`None` 表示渲染模式下没有正文（交给「正在生成…」）。
pub(super) fn body_markdown(message: &AssistantMessage, mode: BodyMode) -> Option<String> {
    let (text, _) = clipped_body(&message.text, message.body_expanded);
    match mode {
        BodyMode::Rendered => (!text.is_empty()).then_some(text),
        BodyMode::Raw if text.is_empty() => Some(t(ChatMsg::NoBody).to_owned()),
        BodyMode::Raw => Some(raw_markdown_block(&text)),
    }
}

/// 一条助手消息「复制」要复制的内容：Markdown 原文（不是渲染后的，也不是只显示的前一段）。
/// 没有正文（只有错误 / 重试提示，它们各自带复制按钮）返回空串，整条消息就不画复制按钮。
pub(super) fn assistant_copy_text(message: &AssistantMessage) -> String {
    message.text.clone()
}

/// 一条错误 / 重试提示的完整文字（和界面上显示的一致：用户目录等路径已脱敏）。
pub(super) fn notice_copy_text(notice: &Notice) -> String {
    redact_paths(&notice.text)
}

/// 右键菜单「复制」的名字和内容：此刻有选中文字就复制选中的，没有就复制整块。
pub(super) fn copy_menu_choice(selected: &str, whole: &str) -> (&'static str, String) {
    if selected.trim().is_empty() {
        (t(ChatMsg::Copy), whole.to_owned())
    } else {
        (t(ChatMsg::CopySelected), selected.to_owned())
    }
}

/// 一条消息的「悬停组」名字：复制按钮靠它在鼠标移进这条消息时才显示。
pub(super) fn copy_group(key: &str) -> SharedString {
    format!("chat-copy-{key}").into()
}

/// 对话里的一条消息。
#[derive(Clone, Debug)]
pub(super) enum ChatMessage {
    User(String),
    Assistant(AssistantMessage),
    /// 独立的系统提示行（比如「已改绑到 X」）：居中、淡色、没有「助手 / 原文」行头；不进历史，也不算对话内容。
    System(String),
}

/// 历史面板当前显示什么；关着时消息区照常显示。
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) enum HistoryPanel {
    /// 关着：显示对话。
    #[default]
    Closed,
    /// 当前智能体的历史列表。
    List,
    /// 只读查看一条历史会话。
    Detail(Box<HistoryRecord>),
}

/// 工具卡片 → 历史里的状态（和 [`history_tool_status`] 的显示一一对应）。
///
/// 被拒绝 / 未确认的卡片在界面上都是 `Failed`，靠确认状态区分成 `rejected`。
pub(super) fn tool_history_status(status: ToolStatus, approval: ApprovalState) -> &'static str {
    match status {
        ToolStatus::Done | ToolStatus::Answered => "done",
        ToolStatus::Stopped => "stopped",
        ToolStatus::Failed if approval == ApprovalState::Denied => "rejected",
        ToolStatus::Failed => "error",
        ToolStatus::Running | ToolStatus::AwaitingApproval => "running",
    }
}

/// 工具卡片的结果：有输出写输出，只有错误写错误，两个都有就都留下。
pub(super) fn tool_output_text(card: &ToolCard) -> String {
    match (&card.output, &card.error) {
        (Some(output), Some(error)) => format!("{output}\n{}{error}", t(ChatMsg::ErrorPrefix)),
        (Some(output), None) => output.clone(),
        (None, Some(error)) => error.clone(),
        (None, None) => String::new(),
    }
}

/// 历史里的一条工具消息 → 回看用的卡片：没有按钮，不会显示成"运行中"。
pub(super) fn history_tool_card(tool: &str, input: &Value, output: &str, status: &str) -> ToolCard {
    let shown = redact_tool_input(input);
    let summary = if shown.is_null() {
        String::new()
    } else {
        format!(
            "{}{}",
            t(ChatMsg::ParamsPrefix),
            content_preview(&shown.to_string())
        )
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
            card.error = Some(text.unwrap_or_else(|| t(ChatMsg::StatusFailed).to_owned()));
        }
        "rejected" => {
            card.status = ToolStatus::Failed;
            card.approval = ApprovalState::Denied;
            card.error = Some(text.unwrap_or_else(|| t(ChatMsg::DeniedByUser).to_owned()));
        }
        "stopped" => {
            card.status = ToolStatus::Stopped;
            card.error = Some(text.unwrap_or_else(|| t(ChatMsg::StatusStopped).to_owned()));
        }
        // 存的时候还在跑（异常退出 / 旧文件）：回看时不能一直显示"运行中"。
        _ => {
            card.status = ToolStatus::Stopped;
            card.error = Some(text.unwrap_or_else(|| t(ChatMsg::TurnEnded).to_owned()));
        }
    }
    card
}

/// 两段增量之间换行，避免中文句子粘在一起；仍然只在同一条消息里累积。
pub(super) fn push_paragraph(target: &mut String, delta: &str) {
    if !target.is_empty() {
        target.push('\n');
    }
    target.push_str(delta);
}

/// 点一台主机：选中就取消，没选中就加上。默认一台都不选（和 0.2.14 一致：没有 @ 选过的
/// 主机就没有目标）。返回点完之后这台主机是不是选中。
pub(super) fn toggle_host_selection(selected: &mut Vec<i64>, id: i64) -> bool {
    if let Some(position) = selected.iter().position(|chosen| *chosen == id) {
        selected.remove(position);
        false
    } else {
        selected.push(id);
        true
    }
}

/// 选中的主机（按主机库里的顺序）；主机库里已经没有的 id 直接忽略。
pub(super) fn selected_targets(hosts: &[HostChoice], selected: &[i64]) -> Vec<HostTarget> {
    hosts
        .iter()
        .filter(|host| selected.contains(&host.id))
        .cloned()
        .collect()
}

/// 存进历史的主机显示名：选了几台就写几台，没选就不写。
pub(super) fn host_names_label(targets: &[HostTarget]) -> Option<String> {
    if targets.is_empty() {
        None
    } else {
        Some(
            targets
                .iter()
                .map(|host| host.name.as_str())
                .collect::<Vec<_>>()
                .join(t(ChatMsg::SeqSeparator)),
        )
    }
}

/// 模型循环里的一项；`id` 为 `None` 表示智能体默认模型。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ModelChoice {
    pub(super) id: Option<String>,
    pub(super) label: String,
}

impl Default for ModelChoice {
    fn default() -> Self {
        Self {
            id: None,
            label: t(ChatMsg::DefaultModel).to_owned(),
        }
    }
}

/// 「默认」+ 智能体目录里的模型。
pub(super) fn model_choices(catalog: &AgentModelCatalog) -> Vec<ModelChoice> {
    let mut models = vec![ModelChoice::default()];
    models.extend(catalog.models.iter().map(|model| ModelChoice {
        id: Some(model.id.clone()),
        label: model.name.clone(),
    }));
    models
}

/// 「模型」按钮上的文字。
pub(super) fn model_label(models: &[ModelChoice], index: usize) -> String {
    let name = models
        .get(index)
        .map(|model| model.label.as_str())
        .unwrap_or(t(ChatMsg::DefaultModel));
    format!("{}{name}", t(ChatMsg::ModelPrefix))
}

/// 模型循环的下一个下标；只有「默认」一项时不动。
pub(super) fn next_model_index(count: usize, index: usize) -> usize {
    if count <= 1 { 0 } else { (index + 1) % count }
}
