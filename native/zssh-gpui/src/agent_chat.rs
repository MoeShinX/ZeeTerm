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
//!
//! 模块划分（纯结构拆分，行为不变）：`text` 界面文案、`demo` 离线脚本、`model`
//! 数据模型、`state` 状态机、`approval` 执行模式与确认、`history` 历史存取、
//! `session` 会话生命周期、`actions` 用户操作、`view` / `history_view` /
//! `window_view` 渲染；嵌在主窗口面板里的那部分在 `embed` 等既有子模块。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use ::gpui::{
    App, ClickEvent, ClipboardItem, EventEmitter, FocusHandle, ImageSource, MouseButton, Resource,
    ScrollHandle, Subscription, Task, Window, WindowBounds, WindowOptions, anchored, deferred,
};
use gpui_kit::base::SelectableText;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _};
use gpui_kit::component::text::{SelectionFormat, TextView};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::{Value, json};
use zssh_core::ai::agent::builtin::FailureInfo;
use zssh_core::ai::agent::usage::ContextUsage;
use zssh_core::ai::agent::{AgentEvent, AgentModelCatalog};
use zssh_core::ai::bridge::{ApprovalRequest, ExecMode, ExecTargetReport, FileApproval};
use zssh_core::ai::files::make_diff;
use zssh_core::ai::history::{
    HistoryAnchor, HistoryDraft, HistoryMessage, HistoryRecord, HistoryStore, HistorySummary,
    is_secret_key, now_ms,
};
use zssh_core::ai::safety::DangerLevel;
use zssh_core::store::Database;

use crate::agent_host::{
    AgentSession, HostTarget, load_endpoint, send_unless_cancelled, start_agent,
    start_builtin_agent,
};
use crate::agent_setup::builtin::{self, RemoteModels};
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
use crate::app_theme::Tokens;
use crate::host_activity;
use crate::i18n::{Msg, t};
use crate::install_state;
use crate::menu_kit;
use crate::ssh_backend::runtime;

/// 下一个对话的编号。
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

/// 脚本推进间隔；和 AI 智能体窗口、终端窗口的轮询节奏一致。
const TICK_INTERVAL: Duration = Duration::from_millis(100);
/// 点了消息上的「复制」之后，对勾和「已复制」留多久。
const COPIED_FLASH: Duration = Duration::from_millis(1500);

/// 调试版里让窗口从离线演示开始的钩子（走 `dev_hooks`，发行版读不到）。
const DEMO_ENV: &str = "ZSSH_AGENT_DEMO";

// 拆到子模块里的名字在这里留一份绑定：子模块用 `use super::*` 取用，路径不变。
use approval::{
    AllowAllGate, ModeSwitch, approval_input, load_exec_mode, mode_is_persisted, mode_label,
    open_settings_db, save_exec_mode,
};
use demo::{
    DENIED_ERROR, DemoInput, MARKDOWN_SAMPLE, STOPPED_ERROR, TurnEvent, TurnScript,
    demo_toggle_available, start_in_real_mode,
};
use history::{
    continue_conversation, delete_history_record, history_host, history_id_for_turn, history_meta,
    history_title, history_tool_status, load_history_record, load_history_rows, resume_session_id,
};
use model::{
    ApprovalDetail, ApprovalState, AssistantMessage, BodyMode, CardKind, ChatMessage, HistoryPanel,
    ModelChoice, Notice, PREVIEW_LIMIT, PendingApproval, RealApproval, ToolCard, ToolInput,
    ToolStatus, assistant_copy_text, body_markdown, body_mode, card_detail_lines, card_status_text,
    clipped_body, content_preview, copy_group, copy_menu_choice, history_tool_card,
    host_names_label, model_choices, model_label, next_model_index, notice_copy_text,
    push_paragraph, redact_tool_input, selected_targets, toggle_host_selection,
    tool_history_status, tool_output_text,
};
use session::{HostEvent, load_hosts};
use state::ChatState;
use text::ChatMsg;
use view::{code_block_copy_text, link_click_notice, table_tsv};
use window_view::allow_all_dialog;

mod actions;
mod agent_ui;
mod approval;
mod builtin_ui;
mod composer;
mod demo;
mod embed;
mod history;
mod history_view;
mod model;
mod queue;
mod queue_view;
mod reason;
mod session;
mod state;
mod text;
mod usage_view;
mod view;
mod window_view;

#[cfg(test)]
mod history_tests;
#[cfg(test)]
mod tests;

pub use approval::AgentChatEvent;
pub(crate) use approval::{allow_all_message, allow_all_title};
pub(crate) use view::link_click_opens;

pub use embed::EmbedContext;

/// 「目标主机」里的一台主机；只留执行需要的字段。
type HostChoice = HostTarget;

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
    /// 刚点过「复制」的那条消息的键和时刻：键对上的按钮在 [`COPIED_FLASH`] 内显示对勾和「已复制」。
    copied: Option<(String, Instant)>,
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
    /// 内置助手（V2-A4）：选中的接口、能力合成要的配置（见 `builtin_ui`）。
    builtin: builtin_ui::BuiltinPanel,
    /// 独立窗口里「全部放行」的确认状态（嵌入面板的确认由主窗口弹，见 [`AgentChatEvent`]）。
    allow_all_gate: AllowAllGate,
    _input_events: Subscription,
    _tick: Task<()>,
}

impl EventEmitter<AgentChatEvent> for AgentChatDemo {}

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
                .placeholder(t(ChatMsg::InputPlaceholder))
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
            copied: None,
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
            builtin: builtin_ui::BuiltinPanel::default(),
            allow_all_gate: AllowAllGate::default(),
            _input_events,
            _tick: Task::ready(()),
        };
        if this.is_builtin() {
            this.pick_default_builtin_endpoint();
        }
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
                    // 「已复制」的对勾到点就收回；只重画，不能借 `changed` 把对话滚到底部。
                    if this.expire_copied() {
                        cx.notify();
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

    /// 打开窗口时把焦点放到输入框，方便直接输入。
    pub fn input_focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }
}

impl Render for AgentChatDemo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 嵌在主窗口右侧面板里时画紧凑版（标题由面板外壳画），独立窗口画完整版。
        if self.embed.is_some() {
            return self.render_embedded(window, cx);
        }
        let asking = self.allow_all_gate.asking();
        let window_view = self.render_window(cx).into_any_element();
        let dialog = asking.then(|| allow_all_dialog(&cx.entity(), window, cx));
        div()
            .size_full()
            .relative()
            .child(window_view)
            .children(dialog)
            .into_any_element()
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
        ..crate::window_frame::app_options()
    };
    let _ = cx.open_window(options, |window, cx| {
        let view = cx.new(|cx| AgentChatDemo::new(window, cx));
        window.set_window_title(t(ChatMsg::WindowTitle));
        window.focus(&view.read(cx).input_focus_handle(cx), cx);
        view
    });
}
