//! 历史会话相关的测试：存档映射、历史 id、续接和列表文案。
//!
//! 复用 `tests` 里的 `started` / `real_started` / `approval_request` 辅助函数。

use super::history::{format_history_time, new_history_id};
use super::tests::approval_request;
use super::{
    ApprovalState, ChatMessage, ChatMsg, ChatState, HistoryAnchor, HistoryMessage, HistoryRecord,
    HistoryStore, ToolStatus, continue_conversation, history_host, history_id_for_turn,
    history_meta, history_title, history_tool_status, redact_tool_input, resume_session_id,
    tool_history_status,
};
use crate::i18n::t;
use serde_json::json;
use zssh_core::ai::agent::AgentEvent;
use zssh_core::ai::agent::discovery::CLAUDE;

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
        usage: None,
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

fn sample_usage(used: u64) -> zssh_core::ai::agent::usage::ContextUsage {
    zssh_core::ai::agent::usage::ContextUsage {
        used,
        window: Some(200_000),
        model: Some("claude-opus-5-5".to_owned()),
        ..Default::default()
    }
}

#[test]
fn usage_events_update_the_meter_without_touching_the_turn() {
    let mut state = ChatState::default();
    assert_eq!(state.usage, None, "新对话还没有用量");
    state.start_real_turn("看一下");
    state.apply_agent_event(AgentEvent::Retrying {
        attempt: 1,
        reason: "服务繁忙".into(),
        next_in_secs: None,
    });
    state.apply_agent_event(AgentEvent::Usage {
        usage: sample_usage(4_511),
    });
    assert_eq!(state.usage.as_ref().map(|u| u.used), Some(4_511));
    assert!(state.retry.is_some(), "用量事件不清掉重试提示");
    assert!(state.is_running(), "也不结束回合");
    state.apply_agent_event(AgentEvent::Usage {
        usage: sample_usage(9_000),
    });
    assert_eq!(
        state.usage.as_ref().map(|u| u.used),
        Some(9_000),
        "以最新一次为准"
    );
    state.new_conversation();
    assert_eq!(state.usage, None, "新对话清掉");
}

#[test]
fn usage_is_saved_with_the_history_and_restored_when_continuing() {
    let mut state = ChatState::default();
    state.start_real_turn("看一下");
    state.apply_agent_event(AgentEvent::TextDelta { text: "好".into() });
    state.apply_agent_event(AgentEvent::Usage {
        usage: sample_usage(4_511),
    });
    state.apply_agent_event(AgentEvent::TurnEnd);
    let draft = state.history_draft("ai-1-0", None, None);
    assert_eq!(draft.usage.as_ref().map(|u| u.used), Some(4_511));

    // 继续这条历史：指示器是它存下的那一次；继续一条没有用量的旧历史：回到「—」。
    let mut record = sample_record(Some("11111111-1111-1111-1111-111111111111"));
    record.usage = draft.usage;
    let mut chat = ChatState::default();
    let (mut history_id, mut pending) = (None, None);
    continue_conversation(&mut chat, &mut history_id, &mut pending, &record).expect("可以续接");
    assert_eq!(chat.usage.as_ref().map(|u| u.used), Some(4_511));
    record.usage = None;
    continue_conversation(&mut chat, &mut history_id, &mut pending, &record).expect("可以续接");
    assert_eq!(chat.usage, None, "旧历史没有用量字段：不沿用上一个对话的");
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
    assert_eq!(t(ChatMsg::Resumable), "可续接");
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

/// 随机唯一的临时目录；返回的 `TempDir` 要拿到用例结束，drop 时整棵删掉。
fn temp_dir(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix(&format!("zssh-agent-chat-history-{name}-"))
        .tempdir()
        .expect("创建临时目录");
    let path = dir.path().to_path_buf();
    (dir, path)
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
    let (_dir_guard, dir) = temp_dir("round-trip");
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
            ChatMessage::Assistant(_) | ChatMessage::System(_) => None,
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
    let (_dir_guard, dir) = temp_dir("continue-no-mix");
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
