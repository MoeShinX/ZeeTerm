//! AI 面板里工具调用的纯逻辑（V2-G10b），和 0.2.14 前端一一对应，不依赖 GPUI，方便单测：
//!
//! - `aiHelpers.ts`：工具名（去前缀、友好名）、从输入 / 结果文本里取命令、路径、目标、退出码、备份路径、
//!   属主变化、执行位置；
//! - `execTargets.ts`：把执行层回报（`ExecTargetReport`）配到工具卡片上；
//! - `toolGroups.ts`：执行完的调用折叠成一行，等审批的、正在执行的不折叠；
//! - `ThinkingIndicator.tsx`：「· · · 12秒 · 思考中」什么时候显示、重试提示怎么写。

use serde_json::Value;
use zssh_core::ai::bridge::ExecTargetReport;

// ---------------------------------------------------------------------------
// 工具名与输入
// ---------------------------------------------------------------------------

/// 去掉智能体加的前缀，得到 `zssh-mcp` 里的工具名（`mcp__zssh__write_file` → `write_file`）。
pub fn bare_tool_name(tool: &str) -> String {
    let lower = tool.to_lowercase();
    let rest = lower.strip_prefix("mcp__").unwrap_or(&lower);
    let Some(after) = rest.strip_prefix("zssh") else {
        return lower;
    };
    // `[_-]{1,2}`：至少一个、至多两个分隔符。
    let separators = after.chars().take_while(|c| matches!(c, '_' | '-')).count();
    if separators == 0 {
        return lower;
    }
    after[separators.min(2)..].to_owned()
}

/// 工具调用的友好名称（卡片上写「执行命令」，而不是 `zssh_run_command` 这种内部 id）。
///
/// 0.2.14 的 `friendlyToolName` 只认 `zssh_` 前缀，Claude Code 的 `mcp__zssh__run_command` 会原样显示；
/// 这里先按 [`bare_tool_name`] 去掉所有智能体前缀，三种智能体显示一致。
pub fn friendly_tool_name(tool: &str) -> String {
    match bare_tool_name(tool).as_str() {
        "list_sessions" | "list_targets" => "列出服务器".to_owned(),
        "run_command" => "执行命令".to_owned(),
        "read_terminal" => "读取终端".to_owned(),
        "list_dir" => "列出目录".to_owned(),
        "read_file" => "读取文件".to_owned(),
        "write_file" => "写入文件".to_owned(),
        "stat" => "查看文件信息".to_owned(),
        "remember_host" => "记住主机信息".to_owned(),
        _ if tool.is_empty() => "工具调用".to_owned(),
        _ => tool.to_owned(),
    }
}

/// 这个工具是不是作用在某个目标服务器上（`list_targets` 之类列表工具不是）。
pub fn tool_has_target(tool: &str) -> bool {
    matches!(
        bare_tool_name(tool).as_str(),
        "run_command" | "read_terminal" | "list_dir" | "read_file" | "write_file" | "stat"
    )
}

/// 是不是文件工具。
pub fn is_file_tool(tool: &str) -> bool {
    matches!(
        bare_tool_name(tool).as_str(),
        "list_dir" | "read_file" | "write_file" | "stat"
    )
}

fn string_field(input: &Value, key: &str) -> Option<String> {
    input.get(key)?.as_str().map(str::to_owned)
}

/// 工具输入里的命令全文。
pub fn command_of(input: &Value) -> Option<String> {
    string_field(input, "command")
}

/// 文件工具输入里的路径。
pub fn path_of(input: &Value) -> Option<String> {
    string_field(input, "path")
}

/// 工具输入里写明的 `target_id`（空串当没写）。
pub fn target_id_of(input: &Value) -> Option<String> {
    string_field(input, "target_id").filter(|id| !id.is_empty())
}

/// 这次调用对回报用的键（和后端 `ExecTargetReport.key` 同一套）：命令原文 / 路径原文 / 空串
/// （`read_terminal`）；输入里取不到为 `None`。
pub fn call_key(tool: &str, input: &Value) -> Option<String> {
    match bare_tool_name(tool).as_str() {
        "run_command" => command_of(input),
        "read_terminal" => Some(String::new()),
        "list_dir" | "read_file" | "write_file" | "stat" => path_of(input),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 结果文本
// ---------------------------------------------------------------------------

/// 在 `text` 里不分大小写地找 `needle`（`needle` 必须是小写 ASCII），返回紧跟其后的下标。
fn find_after_ci(text: &str, needle: &str) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    lower.find(needle).map(|at| at + needle.len())
}

/// 从工具输出里抠出退出码（zssh-mcp 结果文本里是 `exit_code: N`）。
pub fn exit_code_of(output: &str) -> Option<i64> {
    let at = find_after_ci(output, "exit_code:")?;
    let rest = output[at..].trim_start();
    let negative = rest.starts_with('-');
    let digits: String = rest
        .chars()
        .skip(usize::from(negative))
        .take_while(char::is_ascii_digit)
        .collect();
    let value: i64 = digits.parse().ok()?;
    Some(if negative { -value } else { value })
}

/// 把智能体包的 JSON 拆成可读文本（`[{"type":"text","text":"…"}]` 之类）；拆不出来原样返回。
pub fn readable_output(output: &str) -> String {
    let mut text = output.to_owned();
    for _ in 0..3 {
        let trimmed = text.trim();
        if !(trimmed.starts_with('[') || trimmed.starts_with('{') || trimmed.starts_with('"')) {
            break;
        }
        let Ok(parsed) = serde_json::from_str::<Value>(trimmed) else {
            break;
        };
        match parsed {
            Value::String(inner) => text = inner,
            Value::Array(items) => {
                let pieces: Option<Vec<String>> = items
                    .iter()
                    .map(|item| match item {
                        Value::String(piece) => Some(piece.clone()),
                        other => other.get("text")?.as_str().map(str::to_owned),
                    })
                    .collect();
                match pieces {
                    Some(pieces) => text = pieces.concat(),
                    None => break,
                }
            }
            Value::Object(object) => match object.get("text").and_then(Value::as_str) {
                Some(inner) => text = inner.to_owned(),
                None => break,
            },
            _ => break,
        }
    }
    text
}

/// 从 `write_file` 的结果文本里取备份路径（`the previous version was saved on the server at <路径>`）。
pub fn backup_path_of(output: &str) -> Option<String> {
    let text = readable_output(output);
    let at = find_after_ci(&text, "saved on the server at")?;
    let rest = text[at..].trim_start();
    let path: String = rest
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != '"' && *c != '\\')
        .collect();
    (!path.is_empty()).then_some(path)
}

/// 从 `write_file` 的结果文本里取「属主没能改回去」的详情：（原来的, 现在的）。
pub fn owner_change_of(output: &str) -> Option<(String, String)> {
    let text = readable_output(output);
    let at = find_after_ci(
        &text,
        "owner: could not restore the file's owner: it changed from ",
    )?;
    let line = text[at..].split('\n').next().unwrap_or("");
    let (from, to) = line.split_once(" to ")?;
    let to = to.split('(').next().unwrap_or(to);
    Some((from.trim().to_owned(), to.trim().to_owned()))
}

/// 命令实际在哪儿执行的（工具结果文本里的 `location: terminal|background`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    Terminal,
    Background,
}

pub fn location_of(output: &str) -> Option<Location> {
    let at = find_after_ci(output, "location:")?;
    let rest = output[at..].trim_start().to_ascii_lowercase();
    if rest.starts_with("terminal") {
        Some(Location::Terminal)
    } else if rest.starts_with("background") {
        Some(Location::Background)
    } else {
        None
    }
}

/// 降级到后台的原因（`ran in the background because: <原因>`）。
pub fn fallback_reason_of(output: &str) -> Option<String> {
    let at = find_after_ci(output, "ran in the background because:")?;
    let reason = output[at..].split('\n').next().unwrap_or("").trim();
    (!reason.is_empty()).then(|| reason.to_owned())
}

/// 把工具结果末尾的执行编号标记（`[zssh exec <编号>]`）拆出来：返回去掉标记后的文本和编号。
/// 只认末尾那一行——文件内容、命令输出里恰好出现同样的文字不会被当成标记。
pub fn split_exec_marker(output: &str) -> (String, Option<String>) {
    const PREFIX: &str = "[zssh exec ";
    let trimmed = output.trim_end();
    if !trimmed.ends_with(']') {
        return (output.to_owned(), None);
    }
    let Some(start) = trimmed.rfind(PREFIX) else {
        return (output.to_owned(), None);
    };
    let id = &trimmed[start + PREFIX.len()..trimmed.len() - 1];
    let valid = !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if !valid {
        return (output.to_owned(), None);
    }
    // 标记前面紧挨着的那个换行一起去掉。
    let before = &output[..start];
    let before = before
        .strip_suffix("\r\n")
        .or_else(|| before.strip_suffix('\n'))
        .unwrap_or(before);
    (before.to_owned(), Some(id.to_owned()))
}

// ---------------------------------------------------------------------------
// 执行层回报配到工具卡片
// ---------------------------------------------------------------------------

/// 卡片上显示的执行位置：回报（或审批请求）里的那份解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecInfo {
    pub exec_id: String,
    pub target_id: String,
    /// 执行用的终端；这台主机现在没有可用的终端（走后台通道）时为 `None`。
    pub session_id: Option<String>,
    pub label: String,
}

impl From<&ExecTargetReport> for ExecInfo {
    fn from(report: &ExecTargetReport) -> Self {
        Self {
            exec_id: report.exec_id.clone(),
            target_id: report.target_id.clone(),
            session_id: report.session_id.clone(),
            label: report.host_label.clone(),
        }
    }
}

/// 一次带目标的工具调用（对到回报用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracked {
    pub call_id: String,
    /// 不带前缀的工具名。
    pub tool: String,
    /// 对回报用的键；输入里取不到为 `None`（当作通配）。
    pub key: Option<String>,
    /// 调用输入里写明的 `target_id`；省略（落在默认目标上）为 `None`。回报里的 `target_id` 必须和它
    /// 一致才算同一次调用：两台主机同时跑同一条命令时，靠它把各自的回报分清。
    pub target_id: Option<String>,
    /// 工具已经返回了（回报可能晚到）。
    pub ended: bool,
    /// 已经认领过一份回报。
    pub claimed: bool,
    /// 认领的那份回报的执行编号。
    pub exec_id: Option<String>,
    /// 工具结果末尾标记里的执行编号：有了它就以它为准，不再靠启发式。
    pub result_exec_id: Option<String>,
}

/// 登记一次工具调用；不带目标的工具（`list_targets` 之类）返回 `None`。
pub fn track_call(call_id: &str, tool: &str, input: &Value) -> Option<Tracked> {
    if !tool_has_target(tool) {
        return None;
    }
    Some(Tracked {
        call_id: call_id.to_owned(),
        tool: bare_tool_name(tool),
        key: call_key(tool, input),
        target_id: target_id_of(input),
        ended: false,
        claimed: false,
        exec_id: None,
        result_exec_id: None,
    })
}

/// 回报属于哪一次调用（下标）：同一个工具、键相同（输入里取不到键的当作通配）、还没认领过的，并且输入里
/// 写明了 `target_id` 的必须和回报的目标一致（没写的是通配）。候选里写明目标的优先于没写的，还在跑的优先于
/// 已经返回的（晚到的回报不抢新的调用），再按出现先后。
///
/// 智能体的工具事件里拿不到能和桥侧请求一一对应的 ID，所以只能这样配；剩下的歧义只出现在「命令相同、
/// 都省略 target_id」时：它们落在同一个默认目标上，互换回报不影响显示。
pub fn find_call_for_report(calls: &[Tracked], report: &ExecTargetReport) -> Option<usize> {
    let pool: Vec<usize> = calls
        .iter()
        .enumerate()
        .filter(|(_, call)| {
            !call.claimed
                && call.result_exec_id.is_none()
                && call.tool == report.tool
                && call
                    .target_id
                    .as_deref()
                    .is_none_or(|target| target == report.target_id)
        })
        .map(|(index, _)| index)
        .collect();
    let pick = |list: Vec<usize>| -> Option<usize> {
        let named: Vec<usize> = list
            .iter()
            .copied()
            .filter(|index| calls[*index].target_id.is_some())
            .collect();
        let best = if named.is_empty() { list } else { named };
        best.iter()
            .copied()
            .find(|index| !calls[*index].ended)
            .or_else(|| best.first().copied())
    };
    let exact: Vec<usize> = pool
        .iter()
        .copied()
        .filter(|index| calls[*index].key.as_deref() == Some(report.key.as_str()))
        .collect();
    if !exact.is_empty() {
        return pick(exact);
    }
    pick(
        pool.into_iter()
            .filter(|index| calls[*index].key.is_none())
            .collect(),
    )
}

/// 一次配对变化：这张卡片现在对应 `report`；`None` 表示取消原来的对应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecChange {
    pub call_id: String,
    pub report: Option<ExecTargetReport>,
}

/// 把 `report` 配给 `calls[index]`（按编号精确配）。回报原来被另一张卡片临时占着时两张对调：另一张拿到本
/// 卡片原来占的那份（临时配对错开的典型情形是两条同样的命令互换）；本卡片原来没有回报，或另一张卡片的
/// 结果已经指明了别的编号，就让另一张先空着，等它自己的回报到了再认。返回要更新的卡片。
pub fn assign_report(
    calls: &mut [Tracked],
    reports: &std::collections::HashMap<String, ExecTargetReport>,
    index: usize,
    report: &ExecTargetReport,
) -> Vec<ExecChange> {
    if calls[index].exec_id.as_deref() == Some(report.exec_id.as_str()) {
        return Vec::new();
    }
    let previous = calls[index]
        .exec_id
        .as_ref()
        .and_then(|id| reports.get(id))
        .cloned();
    let holder = calls
        .iter()
        .position(|call| call.exec_id.as_deref() == Some(report.exec_id.as_str()));
    calls[index].exec_id = Some(report.exec_id.clone());
    calls[index].claimed = true;
    let mut changes = vec![ExecChange {
        call_id: calls[index].call_id.clone(),
        report: Some(report.clone()),
    }];
    if let Some(holder) = holder.filter(|holder| *holder != index) {
        match previous {
            Some(previous) if calls[holder].result_exec_id.is_none() => {
                calls[holder].exec_id = Some(previous.exec_id.clone());
                changes.push(ExecChange {
                    call_id: calls[holder].call_id.clone(),
                    report: Some(previous),
                });
            }
            _ => {
                calls[holder].exec_id = None;
                calls[holder].claimed = false;
                changes.push(ExecChange {
                    call_id: calls[holder].call_id.clone(),
                    report: None,
                });
            }
        }
    }
    changes
}

/// 工具结果回来了，带着执行编号：记在这次调用上，并按编号校正配对。回报还没到就只记下，回报到了
/// （[`report_arrived`]）再配。
pub fn apply_result_exec_id(
    calls: &mut [Tracked],
    reports: &std::collections::HashMap<String, ExecTargetReport>,
    call_id: &str,
    exec_id: &str,
) -> Vec<ExecChange> {
    let Some(index) = calls.iter().position(|call| call.call_id == call_id) else {
        return Vec::new();
    };
    calls[index].result_exec_id = Some(exec_id.to_owned());
    match reports.get(exec_id).cloned() {
        Some(report) => assign_report(calls, reports, index, &report),
        None => Vec::new(),
    }
}

/// 回报到了：先看有没有哪张卡片的结果已经指明要它（按编号精确配），没有再按启发式临时配。
/// 谁也认领不了返回 `None`（调用方先存着，卡片出现时再认）。
pub fn report_arrived(
    calls: &mut [Tracked],
    reports: &std::collections::HashMap<String, ExecTargetReport>,
    report: &ExecTargetReport,
) -> Option<Vec<ExecChange>> {
    let exact = calls
        .iter()
        .position(|call| call.result_exec_id.as_deref() == Some(report.exec_id.as_str()));
    let index = exact.or_else(|| find_call_for_report(calls, report))?;
    Some(assign_report(calls, reports, index, report))
}

// ---------------------------------------------------------------------------
// 工具调用分组
// ---------------------------------------------------------------------------

/// 一次调用在分组里的处境。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// 等用户批准：必须让用户看到要批准的内容，绝不折叠。
    AwaitingApproval,
    /// 正在执行：只显示一行进行中。
    Running,
    /// 已经结束（完成 / 失败 / 被拒绝 / 被停止）。
    Settled,
}

/// 分组用到的一次调用的摘要。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallView {
    pub phase: Phase,
    /// 失败、被拒绝、被停止，或命令退出码不是 0。
    pub failed: bool,
    /// 确认卡片本身（命令结果在另一张工具卡片上）：折叠时不计数，免得一条命令算两次。
    pub is_approval: bool,
}

/// 面板里一行的展示形式（下标指向传入的调用列表）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolRow {
    /// 完整卡片：等待审批的调用。
    Card(usize),
    /// 正在执行：一行进行中。
    Progress(usize),
    /// 执行完的一组调用，折叠成一行。
    Group(Vec<usize>),
}

/// 把一条消息里的工具调用变成展示行：同一段连续的调用里，执行完的并成一组；遇到等待审批 / 正在执行的
/// 就断开（它们前后各自成组，等它们执行完再并进来）。
pub fn group_calls(calls: &[CallView]) -> Vec<ToolRow> {
    let mut rows = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    for (index, call) in calls.iter().enumerate() {
        match call.phase {
            Phase::AwaitingApproval => {
                flush(&mut rows, &mut open);
                rows.push(ToolRow::Card(index));
            }
            Phase::Running => {
                flush(&mut rows, &mut open);
                rows.push(ToolRow::Progress(index));
            }
            Phase::Settled => open.push(index),
        }
    }
    flush(&mut rows, &mut open);
    rows
}

fn flush(rows: &mut Vec<ToolRow>, open: &mut Vec<usize>) {
    if !open.is_empty() {
        rows.push(ToolRow::Group(std::mem::take(open)));
    }
}

/// 一组调用的摘要，如「执行了 2 条命令、读了 1 个文件」。`tools` 是各调用的工具名。
pub fn summarize_calls(tools: &[&str]) -> String {
    let (mut commands, mut reads, mut writes, mut looks, mut others) = (0, 0, 0, 0, 0);
    for tool in tools {
        match bare_tool_name(tool).as_str() {
            "run_command" => commands += 1,
            "read_file" => reads += 1,
            "write_file" => writes += 1,
            "list_dir" | "stat" => looks += 1,
            _ => others += 1,
        }
    }
    let mut parts = Vec::new();
    if commands > 0 {
        parts.push(format!("执行了 {commands} 条命令"));
    }
    if reads > 0 {
        parts.push(format!("读了 {reads} 个文件"));
    }
    if writes > 0 {
        parts.push(format!("写了 {writes} 个文件"));
    }
    if looks > 0 {
        parts.push(format!("查看了 {looks} 处目录或文件信息"));
    }
    if others > 0 {
        parts.push(format!("{others} 次其他查询"));
    }
    parts.join("、")
}

/// 正在执行的一行文字：「正在执行：<命令前 60 个字符>…」。
pub fn progress_text(tool: &str, input: &Value) -> String {
    let Some(detail) = command_of(input).or_else(|| path_of(input)) else {
        return format!("正在执行：{}", friendly_tool_name(tool));
    };
    let flat = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = flat.chars().collect();
    if chars.len() > 60 {
        format!("正在执行：{}…", chars[..60].iter().collect::<String>())
    } else {
        format!("正在执行：{flat}")
    }
}

// ---------------------------------------------------------------------------
// 思考指示
// ---------------------------------------------------------------------------

/// 对话里最后一项是什么（决定要不要显示「思考中」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastItem {
    /// 还没有任何消息。
    None,
    /// 用户的问题。
    User,
    /// 助手的文字（正在出字，或者出完了）。
    Text,
    /// 工具卡片，附：这一轮里有没有还在执行的卡片。
    Tool { any_running: bool },
}

/// 什么时候显示：回合在跑，且最后一项是用户的问题，或者最后一张命令卡片已经结束（AI 在想下一步）。
/// 文字正在出、命令正在跑、等用户批准时不显示——那些本身就有动静。并行的几条命令里最后一张先跑完、前面的
/// 还在跑：也算命令在跑，不显示。
pub fn show_thinking(running: bool, last: LastItem, approvals_pending: bool) -> bool {
    if !running || approvals_pending {
        return false;
    }
    match last {
        LastItem::None | LastItem::User => true,
        LastItem::Text => false,
        LastItem::Tool { any_running } => !any_running,
    }
}

/// 已经等了多久：`12秒` / `1分05秒`。
pub fn elapsed_text(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}秒")
    } else {
        format!("{}分{}秒", secs / 60, secs % 60)
    }
}

/// 后端同一回合最多放行的重试次数（超过就停掉这一轮，三个智能体共用）。
pub const MAX_RETRIES: u32 = 3;

/// 智能体正在自动重试时的提示内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryNotice {
    pub attempt: u32,
    pub reason: String,
    /// 距离下次重试大约几秒；不知道是 `None`。
    pub next_in_secs: Option<u64>,
}

fn next_text(secs: Option<u64>) -> String {
    match secs {
        None | Some(0) => String::new(),
        Some(secs) if secs < 60 => format!("，约 {secs} 秒后重试"),
        Some(secs) if secs < 3600 => format!("，约 {} 分钟后重试", secs.div_ceil(60)),
        Some(secs) => format!("，约 {} 小时后重试", secs.div_ceil(3600)),
    }
}

/// 「遇到问题，正在重试（第 N 次，最多 3 次）：原因，约 X 秒后重试」。
pub fn retry_text(notice: &RetryNotice) -> String {
    let attempt = notice.attempt.clamp(1, MAX_RETRIES);
    let reason = notice.reason.trim();
    let reason = if reason.is_empty() {
        String::new()
    } else {
        format!("：{reason}")
    };
    format!(
        "遇到问题，正在重试（第 {attempt} 次，最多 {MAX_RETRIES} 次）{reason}{}",
        next_text(notice.next_in_secs)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn report(exec_id: &str, tool: &str, key: &str, target: &str) -> ExecTargetReport {
        ExecTargetReport {
            scope: "s".into(),
            exec_id: exec_id.into(),
            tool: tool.into(),
            key: key.into(),
            target_id: target.into(),
            session_id: Some("term-1".into()),
            host_label: "web-1".into(),
        }
    }

    fn call(id: &str, tool: &str, command: &str, target: Option<&str>) -> Tracked {
        let mut input = json!({ "command": command });
        if let Some(target) = target {
            input["target_id"] = json!(target);
        }
        track_call(id, tool, &input).expect("带目标的工具")
    }

    #[test]
    fn tool_names_lose_the_agent_prefix() {
        for (raw, bare) in [
            ("mcp__zssh__run_command", "run_command"),
            ("zssh_write_file", "write_file"),
            ("zssh-read_file", "read_file"),
            ("run_command", "run_command"),
            ("Read", "read"),
        ] {
            assert_eq!(bare_tool_name(raw), bare, "{raw}");
        }
        assert_eq!(friendly_tool_name("zssh_run_command"), "执行命令");
        assert_eq!(friendly_tool_name("mcp__zssh__read_file"), "读取文件");
        assert_eq!(friendly_tool_name("zssh-stat"), "查看文件信息");
        assert_eq!(friendly_tool_name("custom_tool"), "custom_tool");
        assert_eq!(friendly_tool_name(""), "工具调用");
        assert!(tool_has_target("mcp__zssh__read_terminal"));
        assert!(!tool_has_target("mcp__zssh__list_targets"));
        assert!(is_file_tool("zssh_list_dir") && !is_file_tool("zssh_run_command"));
    }

    #[test]
    fn results_expose_exit_code_location_backup_and_owner() {
        assert_eq!(exit_code_of("stdout:\nx\nexit_code: 127\n"), Some(127));
        assert_eq!(exit_code_of("Exit_Code:  -1"), Some(-1));
        assert_eq!(exit_code_of("nothing"), None);
        let run = "location: background\nran in the background because: 终端里开着全屏程序（vim / top 之类），已改用后台通道\n";
        assert_eq!(location_of(run), Some(Location::Background));
        assert_eq!(
            fallback_reason_of(run).as_deref(),
            Some("终端里开着全屏程序（vim / top 之类），已改用后台通道")
        );
        assert_eq!(
            location_of("location: terminal\n"),
            Some(Location::Terminal)
        );
        assert_eq!(location_of("no place"), None);
        let write = "wrote 12 bytes\nbackup: the previous version was saved on the server at /srv/a.conf.zssh-bak-1\nowner: could not restore the file's owner: it changed from uid 0 / gid 0 to uid 1000 / gid 1000 (not permitted)\n";
        assert_eq!(
            backup_path_of(write).as_deref(),
            Some("/srv/a.conf.zssh-bak-1")
        );
        assert_eq!(
            owner_change_of(write),
            Some(("uid 0 / gid 0".to_owned(), "uid 1000 / gid 1000".to_owned()))
        );
        assert_eq!(backup_path_of("backup: none (new file)"), None);
    }

    #[test]
    fn json_wrapped_results_are_unwrapped_before_parsing() {
        let wrapped =
            r#"[{"type":"text","text":"saved on the server at /srv/x.zssh-bak-2\nexit_code: 0"}]"#;
        assert_eq!(
            backup_path_of(wrapped).as_deref(),
            Some("/srv/x.zssh-bak-2"),
            "路径到引号 / 反斜杠为止"
        );
        assert_eq!(readable_output("plain text"), "plain text");
        assert_eq!(readable_output(r#""inner""#), "inner");
        assert_eq!(
            readable_output("[1, 2]"),
            "[1, 2]",
            "不认识的 JSON 原样返回"
        );
    }

    #[test]
    fn exec_marker_is_split_from_the_end_only() {
        assert_eq!(
            split_exec_marker("hello\n[zssh exec 3f2a1b-7]"),
            ("hello".to_owned(), Some("3f2a1b-7".to_owned()))
        );
        assert_eq!(
            split_exec_marker("hello\r\n[zssh exec ab-1]\n"),
            ("hello".to_owned(), Some("ab-1".to_owned()))
        );
        // 文件内容里恰好有同样的文字、但不在末尾：不当成标记。
        let inside = "see [zssh exec ab-1] then more text";
        assert_eq!(split_exec_marker(inside), (inside.to_owned(), None));
        let bad = "x\n[zssh exec bad id!]";
        assert_eq!(split_exec_marker(bad), (bad.to_owned(), None));
    }

    #[test]
    fn a_report_goes_to_the_matching_command_and_prefers_the_running_one() {
        let calls = vec![
            call("a", "run_command", "uptime", None),
            call("b", "run_command", "uptime", None),
        ];
        let mut calls = calls;
        calls[0].ended = true;
        let r = report("e1", "run_command", "uptime", "host-1");
        assert_eq!(
            find_call_for_report(&calls, &r),
            Some(1),
            "晚到的回报不抢新的调用"
        );
        let other_tool = report("e2", "read_file", "uptime", "host-1");
        assert_eq!(find_call_for_report(&calls, &other_tool), None);
    }

    #[test]
    fn calls_naming_a_target_only_take_that_targets_report() {
        let calls = vec![
            call("web", "run_command", "uptime", Some("host-1")),
            call("db", "run_command", "uptime", Some("host-2")),
        ];
        let for_db = report("e1", "run_command", "uptime", "host-2");
        assert_eq!(find_call_for_report(&calls, &for_db), Some(1));
        let for_web = report("e2", "run_command", "uptime", "host-1");
        assert_eq!(find_call_for_report(&calls, &for_web), Some(0));
    }

    #[test]
    fn the_result_marker_swaps_two_identical_commands_back() {
        let mut calls = vec![
            call("a", "run_command", "uptime", None),
            call("b", "run_command", "uptime", None),
        ];
        let mut reports: HashMap<String, ExecTargetReport> = HashMap::new();
        let r1 = report("e1", "run_command", "uptime", "host-1");
        let r2 = report("e2", "run_command", "uptime", "host-2");
        // 先到的回报按先后临时配：e1 → a，e2 → b。
        for r in [&r1, &r2] {
            let changes = report_arrived(&mut calls, &reports, r).expect("能认领");
            assert_eq!(changes.len(), 1);
            reports.insert(r.exec_id.clone(), r.clone());
        }
        assert_eq!(calls[0].exec_id.as_deref(), Some("e1"));
        assert_eq!(calls[1].exec_id.as_deref(), Some("e2"));
        // b 的结果说「我是 e1」：两张卡片对调。
        let changes = apply_result_exec_id(&mut calls, &reports, "b", "e1");
        assert_eq!(calls[1].exec_id.as_deref(), Some("e1"));
        assert_eq!(calls[0].exec_id.as_deref(), Some("e2"));
        assert_eq!(changes.len(), 2, "两张卡片都要更新");
        // a 的结果也指明了编号：不再抢别人的。
        apply_result_exec_id(&mut calls, &reports, "a", "e2");
        assert_eq!(calls[0].exec_id.as_deref(), Some("e2"));
    }

    #[test]
    fn a_report_that_arrives_before_its_card_is_left_for_later() {
        let mut calls: Vec<Tracked> = Vec::new();
        let reports = HashMap::new();
        let r = report("e1", "run_command", "uptime", "host-1");
        assert!(report_arrived(&mut calls, &reports, &r).is_none());
    }

    #[test]
    fn finished_calls_fold_but_waiting_and_running_ones_never_do() {
        let settled = |failed| CallView {
            phase: Phase::Settled,
            failed,
            is_approval: false,
        };
        let views = [
            settled(false),
            settled(true),
            CallView {
                phase: Phase::AwaitingApproval,
                failed: false,
                is_approval: true,
            },
            settled(false),
            CallView {
                phase: Phase::Running,
                failed: false,
                is_approval: false,
            },
            settled(false),
        ];
        assert_eq!(
            group_calls(&views),
            vec![
                ToolRow::Group(vec![0, 1]),
                ToolRow::Card(2),
                ToolRow::Group(vec![3]),
                ToolRow::Progress(4),
                ToolRow::Group(vec![5]),
            ]
        );
        assert!(group_calls(&[]).is_empty());
    }

    #[test]
    fn group_summaries_count_each_kind() {
        assert_eq!(
            summarize_calls(&[
                "mcp__zssh__run_command",
                "mcp__zssh__run_command",
                "mcp__zssh__read_file",
                "mcp__zssh__write_file",
                "mcp__zssh__list_dir",
                "mcp__zssh__stat",
                "mcp__zssh__list_targets",
            ]),
            "执行了 2 条命令、读了 1 个文件、写了 1 个文件、查看了 2 处目录或文件信息、1 次其他查询"
        );
        assert_eq!(summarize_calls(&[]), "");
    }

    #[test]
    fn progress_text_flattens_and_truncates_by_characters() {
        let short = progress_text("zssh_run_command", &json!({"command": "ls   -la\n/tmp"}));
        assert_eq!(short, "正在执行：ls -la /tmp");
        let long = "字".repeat(80);
        let text = progress_text("zssh_run_command", &json!({ "command": long }));
        assert_eq!(text.chars().count(), "正在执行：".chars().count() + 61);
        assert!(text.ends_with('…'));
        assert_eq!(
            progress_text("zssh_read_terminal", &json!({})),
            "正在执行：读取终端"
        );
        assert_eq!(
            progress_text("zssh_read_file", &json!({"path": "/etc/hosts"})),
            "正在执行：/etc/hosts"
        );
    }

    #[test]
    fn thinking_shows_only_when_nothing_else_is_moving() {
        assert!(show_thinking(true, LastItem::None, false));
        assert!(show_thinking(true, LastItem::User, false));
        assert!(!show_thinking(true, LastItem::Text, false), "字在出");
        assert!(!show_thinking(false, LastItem::User, false), "回合没在跑");
        assert!(!show_thinking(true, LastItem::User, true), "等批准");
        assert!(show_thinking(
            true,
            LastItem::Tool { any_running: false },
            false
        ));
        assert!(!show_thinking(
            true,
            LastItem::Tool { any_running: true },
            false
        ));
        assert_eq!(elapsed_text(0), "0秒");
        assert_eq!(elapsed_text(12_900), "12秒");
        assert_eq!(elapsed_text(65_000), "1分5秒");
    }

    #[test]
    fn retry_text_clamps_the_attempt_and_formats_the_wait() {
        let notice = |attempt, reason: &str, next| RetryNotice {
            attempt,
            reason: reason.to_owned(),
            next_in_secs: next,
        };
        assert_eq!(
            retry_text(&notice(1, "额度用完", Some(30))),
            "遇到问题，正在重试（第 1 次，最多 3 次）：额度用完，约 30 秒后重试"
        );
        assert_eq!(
            retry_text(&notice(9, "  ", Some(90))),
            "遇到问题，正在重试（第 3 次，最多 3 次），约 2 分钟后重试"
        );
        assert_eq!(
            retry_text(&notice(0, "x", None)),
            "遇到问题，正在重试（第 1 次，最多 3 次）：x"
        );
        assert!(retry_text(&notice(1, "x", Some(7200))).ends_with("约 2 小时后重试"));
    }
}
