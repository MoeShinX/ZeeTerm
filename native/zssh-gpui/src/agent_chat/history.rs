//! 历史会话的存取和显示辅助：历史 id、读写文件、续接和列表文案。
//!
//! 和 Tauri 版共用 `<数据目录>\ai-history`，只扫本地目录，不碰网络。

use super::*;

/// 历史会话的存取：和 Tauri 版共用 `<数据目录>\ai-history`。
pub(super) fn history_store() -> HistoryStore {
    HistoryStore::new(&crate::settings::data_dir())
}

/// 同一个进程里连开几个对话时，同一毫秒分到的序号也不一样。
pub(super) static HISTORY_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 一个对话的历史 id：`ai-<毫秒>-<序号>`，只含字母、数字和 `-`，能直接当文件名。
pub(super) fn new_history_id(now: u64, counter: u64) -> String {
    format!("ai-{now}-{counter}")
}

pub(super) fn allocate_history_id() -> String {
    new_history_id(now_ms(), HISTORY_ID_COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// 这个对话的历史 id：第一次真实发送时分配，之后的回合一直复用；「新对话」把它清掉，
/// 下一次发送再重新分配。
pub(super) fn history_id_for_turn(slot: &mut Option<String>) -> String {
    slot.get_or_insert_with(allocate_history_id).clone()
}

/// 读某个智能体的历史列表（0.2.14：当前智能体的）。只扫本地目录（每种智能体最多 50 个文件），
/// 不碰网络。
pub(super) fn load_history_rows(agent: AgentKind) -> Result<Vec<HistorySummary>, String> {
    history_store()
        .list(agent.id())
        .map_err(|err| crate::error_text::error_text(&err))
}

/// 读一条历史会话（回看 / 续接都要先读整条）。
pub(super) fn load_history_record(agent: AgentKind, id: &str) -> Result<HistoryRecord, String> {
    history_store()
        .load(agent.id(), id)
        .map_err(|err| crate::error_text::error_text(&err))
}

/// 删一条历史会话（不存在也算成功）。
pub(super) fn delete_history_record(agent: AgentKind, id: &str) -> Result<(), String> {
    history_store()
        .delete(agent.id(), id)
        .map_err(|err| crate::error_text::error_text(&err))
}

/// 「继续」要交给驱动的智能体会话 id；没有就只能回看。
pub(super) fn resume_session_id(record: &HistoryRecord) -> Result<String, String> {
    match record.agent_session_id.as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => Ok(id.to_owned()),
        _ => Err(t(ChatMsg::NoSession).to_owned()),
    }
}

/// 「继续」一条历史会话时对话的身份怎么换：当前对话整个换成那条历史的消息，历史 id
/// 换成它的（之后的回合写回它自己的文件，不会写进别的对话），并记下要 `--resume` 的
/// Claude 会话 id。调用方要先结束当前真实会话。
pub(super) fn continue_conversation(
    chat: &mut ChatState,
    history_id: &mut Option<String>,
    pending_resume: &mut Option<String>,
    record: &HistoryRecord,
) -> Result<(), String> {
    let resume = resume_session_id(record)?;
    chat.load_history(&record.messages);
    // 切到这条历史：指示器显示它最后一次的用量（旧历史没有就是「—」）。
    chat.usage = record.usage.clone();
    *history_id = Some(record.id.clone());
    *pending_resume = Some(resume);
    Ok(())
}

/// 历史列表里的更新时间：最近用相对时间，久了写日期（UTC，不引时间库）。
pub(super) fn format_history_time(updated_at: u64, now: u64) -> String {
    let seconds = now.saturating_sub(updated_at) / 1000;
    match seconds {
        0..=59 => t(ChatMsg::JustNow).to_owned(),
        60..=3_599 => format!("{}{}", seconds / 60, t(ChatMsg::MinutesAgoSuffix)),
        3_600..=86_399 => format!("{}{}", seconds / 3_600, t(ChatMsg::HoursAgoSuffix)),
        86_400..=2_591_999 => format!("{}{}", seconds / 86_400, t(ChatMsg::DaysAgoSuffix)),
        _ => format!(
            "{} UTC",
            zssh_core::ai::files::display_timestamp((updated_at / 1000) as i64)
        ),
    }
}

/// 历史里显示的主机名：优先对话锚定的那台（V2-U15），旧历史没有锚定就用当时存的主机显示名。
pub(super) fn history_host(
    anchor: Option<&HistoryAnchor>,
    host_label: Option<&str>,
) -> Option<String> {
    anchor
        .map(|anchor| anchor.name.clone())
        .or_else(|| host_label.map(str::to_owned))
}

/// 一行历史摘要：更新时间 · 主机显示名 · 消息数。
pub(super) fn history_meta(
    updated_at: u64,
    host_label: Option<&str>,
    message_count: usize,
    now: u64,
) -> String {
    let mut parts = vec![format_history_time(updated_at, now)];
    if let Some(host) = host_label.map(str::trim).filter(|host| !host.is_empty()) {
        parts.push(host.to_owned());
    }
    parts.push(format!("{message_count}{}", t(ChatMsg::MessageCountSuffix)));
    parts.join(" · ")
}

/// 列表 / 查看页的标题；历史文件里的标题可能是空的（旧文件 / 手改）。
pub(super) fn history_title(title: &str) -> &str {
    let title = title.trim();
    if title.is_empty() {
        t(ChatMsg::Untitled)
    } else {
        title
    }
}

/// 历史里的工具状态（文件里存的是英文状态）。
pub(super) fn history_tool_status(status: &str) -> String {
    match status {
        "done" => t(ChatMsg::StatusDone).to_owned(),
        "error" => t(ChatMsg::StatusFailed).to_owned(),
        "rejected" => t(ChatMsg::Denied).to_owned(),
        "stopped" => t(ChatMsg::StatusStopped).to_owned(),
        "running" => t(ChatMsg::HistoryRunning).to_owned(),
        "" => t(ChatMsg::HistoryUnknown).to_owned(),
        other => other.to_owned(),
    }
}
