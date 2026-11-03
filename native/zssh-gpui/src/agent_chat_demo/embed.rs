// Adapted from Netcatty components/AIChatSidePanel.tsx (via ui/src/components/ai/AIChatPanel.tsx)
//! AI 对话嵌在主窗口右侧面板里时多出来的东西（0.2.14 `AIChatPanel.tsx` 的面板形态）：
//!
//! - 目标主机：当前标签的主机 + 用户 `@` 的主机 + 切焦点时自动保留的主机（规则在
//!   `main_window::scope_targets`），显示成可以 × 掉的标签；
//! - 顶部一行：智能体、历史、新对话；执行模式、模型、思考强度三个下拉在输入框卡片底部一行
//!   （版面规则在 `composer`，选择规则在 `main_window::model_picker`），右边是发送 / 停止；
//! - 输入框上沿的拖动条调输入框高度（双击恢复）；消息区不在底部时出现「回到底部」；
//! - 紧凑版布局：标题和关闭由面板外壳画。
//!
//! 对话状态、安全逻辑、历史、消息渲染都还是 [`AgentChatDemo`] 里那一份（独立窗口和面板共用），
//! 这里只加面板形态要用的状态和界面。

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;

use ::gpui::{Bounds, ClickEvent, MouseButton, Pixels, canvas, px};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Textarea;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::ai::agent::AgentModelCatalog;
use zssh_core::ai::bridge::ExecMode;

use super::composer::{self, ActionKind, ComposerItem, EffortStyle, ModeTone};
use super::{
    AgentChatDemo, ApprovalDetail, ApprovalState, ChatMessage, HistoryPanel, HostChoice,
    NO_HOST_HINT, ToolCard, ToolStatus, card_detail_lines, card_status_text, mode_label,
    redact_paths,
};
use crate::agent_tools::{self, ToolRow};
use crate::app_theme::Tokens;
use crate::icons::Ico;
use crate::main_window::bar_height;
use crate::main_window::bar_settings;
use crate::main_window::model_picker::{self, ModelPicker};
use crate::main_window::scope_targets::{self, Pin};

/// 主窗口每个节拍告诉面板的上下文：当前标签的主机、还开着标签的主机、主机库里的主机。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EmbedContext {
    pub current: Option<(i64, String)>,
    pub open_hosts: HashSet<i64>,
    pub hosts: Vec<HostChoice>,
}

pub(super) struct Embed {
    ctx: EmbedContext,
    pins: Vec<Pin>,
    /// 点了「@ 主机」按钮：打开主机列表（输入框里打 `@词` 也会出这个列表）。
    picker_open: bool,
    picker: ModelPicker,
    /// 平铺的模型 id（`模型` 或 `模型/强度`）。
    selection: String,
    wanted_effort: Option<String>,
    input_height: f32,
    input_drag: Option<(f32, f32)>,
    /// 上一帧量到的面板整体和发送按钮的位置（核对按钮没被挤出面板用，见 `dev_send_inside_panel`）。
    panel_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    send_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl Embed {
    pub(super) fn new(input_height: f32) -> Self {
        Self {
            ctx: EmbedContext::default(),
            pins: Vec::new(),
            picker_open: false,
            picker: ModelPicker::default(),
            selection: String::new(),
            wanted_effort: None,
            input_height,
            input_drag: None,
            panel_bounds: Rc::new(Cell::new(None)),
            send_bounds: Rc::new(Cell::new(None)),
        }
    }

    /// 发送时带的模型；还没有目录（第一条消息发出前）就不带，智能体用自己的默认。
    pub(super) fn model_for_send(&self) -> Option<String> {
        model_picker::model_for_send(&self.selection)
    }

    pub(super) fn set_catalog(&mut self, catalog: &AgentModelCatalog) {
        self.picker = model_picker::build(Some(catalog));
        self.selection = model_picker::selection_after_catalog(
            &self.selection,
            &self.picker,
            self.wanted_effort.as_deref(),
        );
    }

    /// 换了智能体：旧智能体的目录和选中的模型都不能带过去（想要的强度留着，新模型支持就沿用）。
    pub(super) fn clear_catalog(&mut self) {
        self.picker = ModelPicker::default();
        self.selection.clear();
    }

    /// 当前聚焦标签的主机。
    pub(super) fn current_host(&self) -> Option<i64> {
        self.ctx.current.as_ref().map(|(id, _)| *id)
    }

    fn selected(&self) -> Option<(usize, Option<String>)> {
        model_picker::split_selection(&self.selection, &self.picker.groups)
    }

    #[cfg(test)]
    pub(super) fn selection(&self) -> &str {
        &self.selection
    }
}

impl AgentChatDemo {
    /// 面板形态的对话：紧凑布局，目标主机跟着标签走。
    pub fn new_embedded(window: &mut Window, cx: &mut Context<Self>, input_height: f32) -> Self {
        let mut this = Self::new(window, cx);
        this.embed = Some(Embed::new(input_height));
        this
    }

    /// 主窗口每个节拍调一次：标签 / 主机库变了才有动作。
    pub fn set_context(&mut self, ctx: EmbedContext, cx: &mut Context<Self>) {
        let conversation_active = !self.chat.messages.is_empty() || self.chat.is_running();
        let Some(embed) = self.embed.as_mut() else {
            return;
        };
        if embed.ctx == ctx {
            return;
        }
        // 对话进行中焦点切到别的主机：原来那台留在对话范围里（在途请求不会因此失效）。
        if conversation_active && embed.ctx.current != ctx.current {
            let previous = embed
                .ctx
                .current
                .as_ref()
                .map(|(id, name)| (*id, name.as_str()));
            embed.pins = scope_targets::retain_previous(
                &embed.pins,
                previous,
                ctx.current.as_ref().map(|(id, _)| *id),
                &ctx.open_hosts,
            );
        }
        embed.pins = scope_targets::prune_retained(&embed.pins, &ctx.open_hosts);
        self.hosts = ctx.hosts.clone();
        embed.ctx = ctx;
        self.recompute_targets();
        cx.notify();
    }

    /// 每个目标的（当前, 固定）标记：同步给桥，没写 `target_id` 的工具调用落在「当前」那台上。
    pub(super) fn target_flags(&self) -> std::collections::HashMap<i64, (bool, bool)> {
        self.embed_targets()
            .into_iter()
            .map(|target| (target.host_id, (target.current, target.pinned)))
            .collect()
    }

    /// 目标列表变了：`selected_hosts` 跟着变，已有会话马上同步给桥，「AI 使用中」标记也跟着。
    fn recompute_targets(&mut self) {
        let Some(embed) = self.embed.as_ref() else {
            return;
        };
        let current = embed
            .ctx
            .current
            .as_ref()
            .map(|(id, name)| (*id, name.as_str()));
        let ids: Vec<i64> = scope_targets::build(current, &embed.pins, &embed.ctx.open_hosts)
            .into_iter()
            .map(|target| target.host_id)
            // Telnet / 串口主机在目标栏里显示（置灰写原因），但不能当执行目标：不给桥。
            .filter(|id| {
                embed
                    .ctx
                    .hosts
                    .iter()
                    .any(|host| host.id == *id && host.refusal.is_none())
            })
            .collect();
        let changed = ids != self.selected_hosts;
        self.selected_hosts = ids;
        // 「当前」「固定」和有没有打开的终端会随标签、焦点变：每次上下文变了都对桥再同步一遍
        // （同一个 ID 再加是更新，在途请求不受影响）。
        if let Some(session) = self.session.as_ref() {
            session.sync_hosts(&self.selected_targets());
        }
        if changed {
            self.sync_host_activity();
        }
    }

    /// 目标栏当前显示的标签。
    fn embed_targets(&self) -> Vec<scope_targets::Target> {
        let Some(embed) = self.embed.as_ref() else {
            return Vec::new();
        };
        let current = embed
            .ctx
            .current
            .as_ref()
            .map(|(id, name)| (*id, name.as_str()));
        scope_targets::build(current, &embed.pins, &embed.ctx.open_hosts)
            .into_iter()
            .filter(|target| embed.ctx.hosts.iter().any(|host| host.id == target.host_id))
            .collect()
    }

    pub fn pin_host(&mut self, host_id: i64, cx: &mut Context<Self>) {
        let Some(embed) = self.embed.as_mut() else {
            return;
        };
        let Some(host) = embed.ctx.hosts.iter().find(|host| host.id == host_id) else {
            return;
        };
        let name = host.name.clone();
        if let Some(reason) = host.refusal.clone() {
            embed.picker_open = false;
            self.link_notice = Some(super::Notice {
                text: format!("「{name}」不能加进目标：{reason}"),
                error: true,
            });
            cx.notify();
            return;
        }
        let (pins, already) = scope_targets::pin_host(
            &embed.pins,
            host_id,
            &name,
            embed.ctx.current.as_ref().map(|(id, _)| *id),
        );
        embed.pins = pins;
        embed.picker_open = false;
        if already {
            self.link_notice = Some(super::Notice {
                text: format!("「{name}」已经在这次对话的目标里了"),
                error: false,
            });
        }
        self.recompute_targets();
        cx.notify();
    }

    pub fn unpin_host(&mut self, host_id: i64, cx: &mut Context<Self>) {
        if let Some(embed) = self.embed.as_mut() {
            embed.pins = scope_targets::unpin(&embed.pins, host_id);
        }
        self.recompute_targets();
        cx.notify();
    }

    /// 现在对话的目标主机 id（按目标栏的顺序）。
    pub fn target_ids(&self) -> Vec<i64> {
        self.selected_hosts.clone()
    }

    pub fn is_turn_running(&self) -> bool {
        self.chat.is_running()
    }

    /// 输入框里的内容（驱动脚本用）。
    pub fn input_text(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    pub fn set_input_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = text.to_owned();
        self.input
            .update(cx, |state, cx| state.set_value(text, window, cx));
    }

    pub fn send_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.send(window, cx);
    }

    /// 发送按钮整个在面板里（上一帧量到的位置）。还没量到返回 `None`。
    pub fn dev_send_inside_panel(&self) -> Option<bool> {
        let embed = self.embed.as_ref()?;
        let panel = embed.panel_bounds.get()?;
        let send = embed.send_bounds.get()?;
        let slack = px(0.5);
        Some(
            send.left() >= panel.left() - slack
                && send.right() <= panel.right() + slack
                && send.top() >= panel.top() - slack
                && send.bottom() <= panel.bottom() + slack,
        )
    }

    /// 面板和发送按钮的宽度（写进日志用）。
    pub fn dev_send_geometry(&self) -> Option<(f32, f32, f32)> {
        let embed = self.embed.as_ref()?;
        let panel = embed.panel_bounds.get()?;
        let send = embed.send_bounds.get()?;
        Some((
            f32::from(panel.size.width),
            f32::from(send.size.width),
            f32::from(panel.right() - send.right()),
        ))
    }

    pub fn message_count(&self) -> usize {
        self.chat.messages.len()
    }

    /// 最后一条助手消息的正文（驱动脚本核对真实回复用）。
    pub fn last_assistant_text(&self) -> Option<String> {
        self.chat
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                super::ChatMessage::Assistant(message) => Some(message.text.clone()),
                super::ChatMessage::User(_) => None,
            })
    }

    pub fn footer_error_text(&self) -> Option<String> {
        self.footer_error.clone()
    }

    // ---- 驱动脚本用的读写口（调试版自动化；走和按钮同一批方法）----

    /// 当前智能体的 id（`opencode` / `codex` / `claude`）。
    pub fn agent_id(&self) -> &'static str {
        self.agent.id()
    }

    /// 换智能体（和下拉里点一项同一个方法）；这一项不可选（没装 / 没登录 / 版本过低）时说明原因。
    pub fn switch_agent_by_id(&mut self, id: &str, cx: &mut Context<Self>) -> Result<(), String> {
        let kind = crate::agent_setup::AgentKind::parse(id).ok_or("不认识的智能体")?;
        if let Some(note) = self.status.get(kind).availability().selector_note() {
            return Err(format!("{} 不可选：{note}", kind.label()));
        }
        self.switch_agent(kind, cx);
        Ok(())
    }

    /// 当前智能体的可用状态名。
    pub fn availability_name(&self) -> &'static str {
        use crate::agent_setup::Availability::*;
        match self.availability() {
            Checking => "checking",
            Ready => "ready",
            Missing => "missing",
            TooOld => "old",
            NeedsLogin => "login",
        }
    }

    /// 发送被挡住了吗（没找到 / 没登录 / 还在检测）。
    pub fn is_blocked(&self) -> bool {
        self.real && self.availability().blocks_input()
    }

    /// 底部提示、各种通知和对话里的提示合在一起的文字（脚本断言用，不显示）。
    pub fn notices_text(&self) -> String {
        let mut out = Vec::new();
        for text in [
            self.footer_error.as_deref(),
            self.link_notice.as_ref().map(|n| n.text.as_str()),
            self.resume_notice.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            out.push(text.to_owned());
        }
        for message in &self.chat.messages {
            if let super::ChatMessage::Assistant(message) = message {
                out.extend(message.notices.iter().map(|n| n.text.clone()));
            }
        }
        out.join(
            "
",
        )
    }

    /// 模型下拉里的各项文字。
    pub fn model_labels(&self) -> Vec<String> {
        self.embed
            .as_ref()
            .map(|e| e.picker.groups.iter().map(|g| g.label.clone()).collect())
            .unwrap_or_default()
    }

    /// 当前模型框显示的文字（没有目录是「默认模型」）。
    pub fn model_label_now(&self) -> String {
        let Some(embed) = self.embed.as_ref() else {
            return String::new();
        };
        embed
            .selected()
            .and_then(|(index, _)| embed.picker.groups.get(index))
            .map_or_else(|| "默认模型".to_owned(), |g| g.label.clone())
    }

    /// 强度下拉里的各项文字（模型不支持强度时是空的）。
    pub fn effort_labels(&self) -> Vec<String> {
        let Some(embed) = self.embed.as_ref() else {
            return Vec::new();
        };
        let Some((index, _)) = embed.selected() else {
            return Vec::new();
        };
        let Some(group) = embed.picker.groups.get(index) else {
            return Vec::new();
        };
        let choices = model_picker::effort_choices(group);
        if choices.len() > 1 {
            choices
                .iter()
                .map(|c| model_picker::effort_label(c.as_deref()))
                .collect()
        } else {
            Vec::new()
        }
    }

    /// 当前强度的文字。
    pub fn effort_label_now(&self) -> String {
        let Some(embed) = self.embed.as_ref() else {
            return String::new();
        };
        model_picker::effort_label(embed.selected().and_then(|(_, e)| e).as_deref())
    }

    /// 发消息时会带的模型（`None` = 智能体自己的默认）。
    pub fn model_for_send_now(&self) -> Option<String> {
        self.embed.as_ref().and_then(Embed::model_for_send)
    }

    pub fn pick_model_by_label(
        &mut self,
        label: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let index = self
            .embed
            .as_ref()
            .and_then(|e| e.picker.groups.iter().position(|g| g.label == label))
            .ok_or("模型下拉里没有这一项")?;
        self.pick_model(index, cx);
        Ok(())
    }

    pub fn pick_effort_by_label(
        &mut self,
        label: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let embed = self.embed.as_ref().ok_or("不是面板形态")?;
        let (index, _) = embed.selected().ok_or("还没有模型目录")?;
        let group = embed.picker.groups.get(index).ok_or("模型下拉没有这一项")?;
        let effort = model_picker::effort_choices(group)
            .into_iter()
            .find(|c| model_picker::effort_label(c.as_deref()) == label)
            .ok_or("这个模型没有这档强度")?;
        self.pick_effort(effort, cx);
        Ok(())
    }

    /// 顶部提示 / 整块提示在不在：`too_old`、`login_unknown`、`unavailable`。
    pub fn shows(&self, what: &str) -> bool {
        let status = self.status.get(self.agent);
        match what {
            "too_old" => status.availability() == crate::agent_setup::Availability::TooOld,
            "login_unknown" => status.login_unknown_hint().is_some(),
            "unavailable" => {
                self.real && self.chat.messages.is_empty() && self.availability().blocks_input()
            }
            _ => false,
        }
    }

    /// 历史面板里列了几条（脚本先 `ai_history:open`）。
    pub fn history_count(&self) -> usize {
        self.history_rows.len()
    }

    pub fn toggle_history_panel(&mut self, cx: &mut Context<Self>) {
        self.toggle_history(cx);
    }

    /// 在历史列表里点「继续」第 n 条。
    pub fn continue_history_nth(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let id = self
            .history_rows
            .get(index)
            .map(|row| row.id.clone())
            .ok_or("历史里没有这一条")?;
        self.continue_history(&id, cx);
        Ok(())
    }

    pub fn exec_mode(&self) -> ExecMode {
        self.current_mode()
    }

    pub fn is_real(&self) -> bool {
        self.real
    }

    pub fn clear_conversation(&mut self, cx: &mut Context<Self>) {
        self.new_conversation(cx);
    }

    // ---- 调试驱动用的读写口（V2-G10b；走和按钮同一批方法）----

    /// 「停止」按钮。
    pub fn dev_stop(&mut self, cx: &mut Context<Self>) {
        self.stop(cx);
    }

    /// 消息区滚到最新（截图前用）。
    pub fn dev_scroll_bottom(&mut self, cx: &mut Context<Self>) {
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// 等着用户批准的确认卡片个数。
    pub fn dev_pending_approvals(&self) -> usize {
        self.chat.approvals.len()
    }

    /// 点第一张等批准的确认卡片上的「允许」/「拒绝」。
    pub fn dev_answer_approval(
        &mut self,
        allow: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let call_id = self
            .chat
            .approvals
            .first()
            .map(|approval| approval.call_id.clone())
            .ok_or_else(|| "没有等批准的确认卡片".to_owned())?;
        self.answer_approval(&call_id, allow, cx);
        Ok(())
    }

    /// 最后一条助手消息里的工具卡片个数 / 正在执行的个数 / 折叠成一行的分组个数。
    pub fn dev_card_counts(&self) -> (usize, usize, usize) {
        let Some(ChatMessage::Assistant(message)) = self.chat.messages.last() else {
            return (0, 0, 0);
        };
        let views: Vec<_> = message.tools.iter().map(ToolCard::view).collect();
        let groups = agent_tools::group_calls(&views)
            .iter()
            .filter(|row| matches!(row, ToolRow::Group(_)))
            .count();
        let running = message
            .tools
            .iter()
            .filter(|card| card.status == ToolStatus::Running)
            .count();
        (message.tools.len(), running, groups)
    }

    /// 展开 / 收起第 `nth` 个折叠分组（从 0 数）。
    pub fn dev_toggle_group(&mut self, nth: usize, cx: &mut Context<Self>) -> Result<(), String> {
        let message_index = self.chat.messages.len().checked_sub(1).ok_or("没有消息")?;
        let Some(ChatMessage::Assistant(message)) = self.chat.messages.last() else {
            return Err("最后一条不是助手消息".to_owned());
        };
        let views: Vec<_> = message.tools.iter().map(ToolCard::view).collect();
        let first = agent_tools::group_calls(&views)
            .into_iter()
            .filter_map(|row| match row {
                ToolRow::Group(ids) => ids.first().copied(),
                _ => None,
            })
            .nth(nth)
            .ok_or_else(|| "没有这个分组".to_owned())?;
        self.chat.toggle_group(message_index, first);
        // 调试断言 / 截图用：展开后看最新的一屏。
        self.scroll.scroll_to_bottom();
        cx.notify();
        Ok(())
    }

    /// 界面上看得见的工具卡片文字（折叠起来的分组只有那一行摘要，展开了才有里面的卡片）。
    /// 断言「某段文字有没有显示出来」用它，不看被折叠藏起来的。
    pub fn dev_visible_cards_text(&self) -> String {
        let show_target = self.selected_hosts.len() > 1;
        let mut out = Vec::new();
        for message in &self.chat.messages {
            let ChatMessage::Assistant(message) = message else {
                continue;
            };
            let views: Vec<_> = message.tools.iter().map(ToolCard::view).collect();
            for row in agent_tools::group_calls(&views) {
                match row {
                    ToolRow::Card(index) => {
                        out.push(self.card_dev_text(&message.tools[index], show_target));
                    }
                    ToolRow::Progress(index) => {
                        let card = &message.tools[index];
                        out.push(agent_tools::progress_text(&card.tool, &card.input_json()));
                    }
                    ToolRow::Group(ids) => {
                        let counted: Vec<&ToolCard> = ids
                            .iter()
                            .map(|index| &message.tools[*index])
                            .filter(|card| !card.is_approval())
                            .collect();
                        let tools: Vec<&str> = counted.iter().map(|c| c.tool.as_str()).collect();
                        let failed = counted.iter().filter(|c| c.view().failed).count();
                        let mut line = agent_tools::summarize_calls(&tools);
                        if failed > 0 {
                            line.push_str(&format!("（其中 {failed} 条失败）"));
                        }
                        out.push(format!("[组] {line}"));
                        if ids
                            .first()
                            .is_some_and(|first| message.open_groups.contains(first))
                        {
                            for index in ids {
                                out.push(self.card_dev_text(&message.tools[index], show_target));
                            }
                        }
                    }
                }
            }
        }
        out.join("\n")
    }

    /// 一张卡片显示出来的文字（标题、主机名、状态、命令 / 路径、错误、细节、输出）。
    fn card_dev_text(&self, card: &ToolCard, show_target: bool) -> String {
        let mut parts = vec![agent_tools::friendly_tool_name(&card.tool)];
        if let Some(exec) = &card.exec
            && (card.exec_multi || show_target)
        {
            parts.push(format!("@{}", exec.label));
        }
        if self.chat.target_unknown(card) {
            parts.push("执行目标未确定".to_owned());
        }
        if card.exec.is_some() && self.embed.is_some() {
            parts.push("[查看终端]".to_owned());
        }
        parts.push(card_status_text(card));
        match card.approval {
            ApprovalState::Pending => parts.push("需要确认".to_owned()),
            ApprovalState::Allowed => parts.push("已允许".to_owned()),
            ApprovalState::Denied => parts.push("已拒绝".to_owned()),
            ApprovalState::NotRequired => {}
        }
        if card.dangerous() {
            parts.push("危险".to_owned());
        }
        let mut text = parts.join(" | ");
        if let Some(command) = card.detail.as_ref().and_then(ApprovalDetail::command_text) {
            text.push_str(&format!("\n命令：{command}"));
        } else if let Some(command) = card.input.command() {
            text.push_str(&format!("\n命令：{command}"));
        } else if let Some(path) = agent_tools::path_of(&card.input_json()) {
            text.push_str(&format!("\n路径：{path}"));
        }
        if let Some(file) = card.detail.as_ref().and_then(|d| d.request.file.as_ref()) {
            text.push_str(&format!(
                "\n文件：{}（{}）",
                file.path,
                if file.is_new {
                    "新建"
                } else {
                    "覆盖前自动备份"
                }
            ));
        }
        if let Some(error) = &card.error {
            text.push_str(&format!("\n错误：{error}"));
        }
        for (line, _) in card_detail_lines(card) {
            text.push_str(&format!("\n{line}"));
        }
        if let Some(output) = &card.output {
            text.push_str(&format!("\n输出：{}", agent_tools::readable_output(output)));
        }
        text
    }

    /// 点第一张带执行位置的卡片上的「查看终端」。
    pub fn dev_view_terminal(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let exec = self
            .chat
            .messages
            .iter()
            .filter_map(|message| match message {
                ChatMessage::Assistant(message) => Some(message),
                ChatMessage::User(_) => None,
            })
            .flat_map(|message| message.tools.iter())
            .find_map(|card| card.exec.clone())
            .ok_or_else(|| "没有带执行位置的卡片".to_owned())?;
        self.view_terminal(&exec, cx);
        Ok(())
    }

    /// 「思考中」现在显示着吗 / 重试提示的文字。
    pub fn dev_thinking(&self) -> (bool, Option<String>) {
        let shown = self.real && self.chat.thinking_visible();
        let retry = self
            .chat
            .retry
            .as_ref()
            .filter(|_| self.chat.real_running)
            .map(agent_tools::retry_text);
        (shown, retry)
    }

    pub fn set_mode_explicit(&mut self, mode: ExecMode, cx: &mut Context<Self>) {
        if self.real {
            match self.write_mode(mode) {
                Ok(()) => self.settings_error = None,
                Err(err) => self.settings_error = Some(err),
            }
            self.mode = mode;
        } else {
            self.demo_mode = mode;
        }
        cx.notify();
    }

    fn pick_model(&mut self, group_index: usize, cx: &mut Context<Self>) {
        let Some(embed) = self.embed.as_mut() else {
            return;
        };
        let Some(group) = embed.picker.groups.get(group_index) else {
            return;
        };
        let effort = model_picker::pick_effort(group, embed.wanted_effort.as_deref());
        embed.selection = model_picker::join_selection(group, effort.as_deref());
        embed.wanted_effort = effort;
        cx.notify();
    }

    fn pick_effort(&mut self, effort: Option<String>, cx: &mut Context<Self>) {
        let Some(embed) = self.embed.as_mut() else {
            return;
        };
        let Some((index, _)) = embed.selected() else {
            return;
        };
        let Some(group) = embed.picker.groups.get(index) else {
            return;
        };
        embed.selection = model_picker::join_selection(group, effort.as_deref());
        embed.wanted_effort = effort;
        cx.notify();
    }

    pub(super) fn at_bottom(&self) -> bool {
        let max = self.scroll.max_offset().y;
        let offset = self.scroll.offset().y;
        f32::from(max) <= 1.0 || (f32::from(offset) + f32::from(max)).abs() < 24.0
    }

    fn commit_input_height(&mut self) {
        let Some(embed) = self.embed.as_mut() else {
            return;
        };
        embed.input_drag = None;
        if let Some(db) = &self.db
            && let Err(err) =
                bar_settings::write_number(db, bar_settings::AI_INPUT_HEIGHT, embed.input_height)
        {
            tracing::warn!(%err, "保存 AI 输入框高度失败");
        }
    }

    pub(super) fn render_embedded(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tokens = Tokens::current(cx);
        let entity = cx.entity();
        let running = self.chat.is_running();
        let blocked = self.real && self.availability().blocks_input();
        let panel_height = f32::from(window.viewport_size().height);
        let agent_button = if self.real {
            Some(self.agent_button(cx))
        } else {
            None
        };
        let banners = if self.real {
            self.agent_banners(cx)
        } else {
            Vec::new()
        };
        let body_area = self.conversation_body(cx);
        let at_bottom = self.at_bottom();
        let hover = tokens.hover;
        let text = tokens.text;
        let muted = tokens.text_muted;

        // ---- 第一行：模型、强度、执行模式、历史、新对话 ----
        let embed = self.embed.as_ref();
        let picker = embed.map(|e| e.picker.clone()).unwrap_or_default();
        let selected = embed.and_then(Embed::selected);
        let model_label: SharedString = selected
            .as_ref()
            .and_then(|(index, _)| picker.groups.get(*index))
            .map(|group| group.label.clone())
            .unwrap_or_else(|| "默认模型".to_owned())
            .into();
        let effort_label: SharedString =
            model_picker::effort_label(selected.as_ref().and_then(|(_, effort)| effort.as_deref()))
                .into();
        let efforts: Vec<Option<String>> = selected
            .as_ref()
            .and_then(|(index, _)| picker.groups.get(*index))
            .map(model_picker::effort_choices)
            .unwrap_or_default();
        let has_catalog = !picker.groups.is_empty();
        let current_effort = selected.as_ref().and_then(|(_, effort)| effort.clone());
        let current_group = selected.as_ref().map(|(index, _)| *index);

        let has_effort = efforts.len() > 1 || (efforts.len() == 1 && current_effort.is_some());
        let mode_text = mode_label(self.current_mode());
        let panel_width = embed
            .and_then(|e| e.panel_bounds.get())
            .map(|bounds| f32::from(bounds.size.width));
        let items =
            composer::composer_items(has_effort, running, panel_width, mode_text, &effort_label);
        let effort_style = items.iter().find_map(|item| match item {
            ComposerItem::Effort(style) => Some(*style),
            _ => None,
        });
        let model_max = composer::model_max_width(
            panel_width,
            mode_text,
            effort_style.map(|style| (style, effort_label.as_ref())),
        );
        let model_button = {
            let entity = entity.clone();
            let groups = picker.groups.clone();
            Button::new("ai-model")
                .ghost()
                .compact()
                .label(model_label.clone())
                .dropdown_caret(has_catalog)
                // 模型名先省略（Button 的标签自带省略号），悬停显示全名。
                .max_w(px(model_max))
                .tooltip(if has_catalog {
                    format!("模型：{model_label}")
                } else if blocked {
                    "这个智能体现在不能用".to_owned()
                } else {
                    "正在读取模型目录".to_owned()
                })
                .disabled(!has_catalog || blocked)
                .dropdown_menu_with_anchor(::gpui::Anchor::BottomLeft, move |menu, _window, _cx| {
                    let mut menu = menu.min_w(px(200.0)).max_h(px(320.0)).scrollable(true);
                    for (index, group) in groups.iter().enumerate() {
                        let entity = entity.clone();
                        let mut label = group.label.clone();
                        if group.unavailable {
                            label.push_str("（不可用）");
                        }
                        menu = menu.item(
                            PopupMenuItem::new(label)
                                .checked(current_group == Some(index))
                                .on_click(move |_: &ClickEvent, _window, cx| {
                                    entity.update(cx, |this, cx| this.pick_model(index, cx));
                                }),
                        );
                    }
                    menu
                })
        };
        let effort_button = effort_style.map(|style| {
            let entity = entity.clone();
            let current = current_effort.clone();
            let button = Button::new("ai-effort").ghost().compact().flex_none();
            let button = match style {
                EffortStyle::Full => button
                    .label(effort_label.clone())
                    .dropdown_caret(true)
                    .tooltip("思考强度"),
                // 窄面板：只剩图标，悬停显示当前值。
                EffortStyle::IconOnly => button
                    .icon(Ico::Brain)
                    .tooltip(format!("思考强度：{effort_label}")),
            };
            button.dropdown_menu_with_anchor(
                ::gpui::Anchor::BottomLeft,
                move |menu, _window, _cx| {
                    let mut menu = menu.min_w(px(140.0));
                    for choice in &efforts {
                        let entity = entity.clone();
                        let picked = choice.clone();
                        menu = menu.item(
                            PopupMenuItem::new(model_picker::effort_label(choice.as_deref()))
                                .checked(&current == choice)
                                .on_click(move |_: &ClickEvent, _window, cx| {
                                    let picked = picked.clone();
                                    entity.update(cx, |this, cx| this.pick_effort(picked, cx));
                                }),
                        );
                    }
                    menu
                },
            )
        });
        let mode_button = {
            let entity = entity.clone();
            let current = self.current_mode();
            let tone = composer::mode_tone(current);
            // 「全部放行」用警示色（浅色底上用深一点的琥珀色保证读得清），其余是普通文字色。
            let caution = if tokens.dark {
                tokens.dot_connecting
            } else {
                ::gpui::rgb(0xB45309).into()
            };
            let icon = match current {
                ExecMode::Manual => Ico::ShieldCheck,
                ExecMode::ReadOnly => Ico::Eye,
                ExecMode::Auto => Ico::ShieldAlert,
            };
            Button::new("ai-mode")
                .ghost()
                .compact()
                .flex_none()
                .icon(icon)
                .label(mode_text)
                .dropdown_caret(true)
                .when(tone == ModeTone::Caution, |b| b.text_color(caution))
                .tooltip("执行模式")
                .dropdown_menu_with_anchor(::gpui::Anchor::BottomLeft, move |menu, _window, _cx| {
                    let mut menu = menu.min_w(px(200.0));
                    for (mode, hint) in [
                        (ExecMode::Manual, "每条命令都要你点确认"),
                        (ExecMode::ReadOnly, "查看类命令直接执行，改动系统的要确认"),
                        (ExecMode::Auto, "所有命令直接执行，不再确认"),
                    ] {
                        let entity = entity.clone();
                        menu = menu.item(
                            PopupMenuItem::new(format!("{}  ·  {hint}", mode_label(mode)))
                                .checked(current == mode)
                                .on_click(move |_: &ClickEvent, _window, cx| {
                                    entity.update(cx, |this, cx| {
                                        this.set_mode_explicit(mode, cx);
                                    });
                                }),
                        );
                    }
                    menu
                })
        };
        let icon_button = |id: &'static str, icon: Icon, tip: &'static str, active: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .size(px(26.0))
                .flex_none()
                .rounded_md()
                .cursor_pointer()
                .text_color(if active { text } else { muted })
                .when(active, |d| d.bg(hover))
                .hover(move |style| style.bg(hover).text_color(text))
                .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
                .child(icon.size_4())
        };
        let history_button = {
            let entity = entity.clone();
            icon_button(
                "ai-history",
                Icon::new(Ico::Clock),
                "历史会话",
                self.history != HistoryPanel::Closed,
            )
            .on_click(move |_, _window, cx| {
                entity.update(cx, |this, cx| this.toggle_history(cx));
            })
        };
        let clear_button = {
            let entity = entity.clone();
            icon_button(
                "ai-clear",
                Icon::new(IconName::Plus),
                "新对话（清空当前对话）",
                false,
            )
            .on_click(move |_, _window, cx| {
                entity.update(cx, |this, cx| this.new_conversation(cx));
            })
        };
        // 一行：智能体、历史、新对话（执行模式、模型、思考强度在输入框底部一行里）。
        let toolbar = h_flex()
            .flex_none()
            .w_full()
            .px_2()
            .py_1p5()
            .gap_0p5()
            // 面板很窄时按钮折到下一行，不被面板边缘截掉（V2-U1）。
            .flex_wrap()
            .items_center()
            .border_b_1()
            .border_color(tokens.border)
            .children(agent_button)
            .child(div().flex_1())
            .child(history_button)
            .child(clear_button);

        // ---- 第二行：目标主机标签 ----
        let targets = self.embed_targets();
        // 不能当执行目标的主机（Telnet / 串口）：目标栏里仍然列出来，置灰并写原因。
        let refusal_of = |host_id: i64| -> Option<String> {
            embed.and_then(|e| {
                e.ctx
                    .hosts
                    .iter()
                    .find(|host| host.id == host_id)
                    .and_then(|host| host.refusal.clone())
            })
        };
        let refused_reason: Option<String> =
            targets.iter().find_map(|target| refusal_of(target.host_id));
        let target_chips = targets.iter().enumerate().map(|(index, target)| {
            let entity = entity.clone();
            let host_id = target.host_id;
            let removable = target.pinned;
            let refusal = refusal_of(host_id);
            let label: SharedString = match (target.current, refusal.is_some()) {
                (true, true) => format!("{}（当前，不可用）", target.name).into(),
                (true, false) => format!("{}（当前）", target.name).into(),
                (false, true) => format!("{}（不可用）", target.name).into(),
                (false, false) => target.name.clone().into(),
            };
            h_flex()
                .id(("ai-target", index))
                .items_center()
                .gap_1()
                .px_2()
                .py_0p5()
                .rounded_md()
                .border_1()
                .border_color(tokens.border)
                .when(refusal.is_none(), |d| d.bg(tokens.accent_soft))
                .when(refusal.is_some(), |d| d.text_color(muted))
                .when_some(refusal.clone(), |d, reason| {
                    d.tooltip(move |window, cx| Tooltip::new(reason.clone()).build(window, cx))
                })
                .text_xs()
                .child(div().truncate().max_w(px(160.0)).child(label))
                .when(removable && !target.current, |d| {
                    d.child(
                        div()
                            .id(("ai-target-x", index))
                            .cursor_pointer()
                            .text_color(muted)
                            .hover(move |style| style.text_color(text))
                            .tooltip(|window, cx| {
                                Tooltip::new("从这次对话的目标里去掉").build(window, cx)
                            })
                            .child(Icon::new(IconName::Close).size_3())
                            .on_click(move |_, _window, cx| {
                                entity.update(cx, |this, cx| this.unpin_host(host_id, cx));
                            }),
                    )
                })
        });
        let add_target = {
            let entity = entity.clone();
            div()
                .id("ai-add-target")
                .px_2()
                .py_0p5()
                .rounded_md()
                .text_xs()
                .cursor_pointer()
                .text_color(muted)
                .hover(move |style| style.bg(hover).text_color(text))
                .tooltip(|window, cx| {
                    Tooltip::new("把主机库里的另一台主机加进目标").build(window, cx)
                })
                .child("@ 主机")
                .on_click(move |_, _window, cx| {
                    entity.update(cx, |this, cx| {
                        if let Some(embed) = this.embed.as_mut() {
                            embed.picker_open = !embed.picker_open;
                        }
                        cx.notify();
                    });
                })
        };
        let target_row = v_flex()
            .flex_none()
            .w_full()
            .px_3()
            .py_1p5()
            .gap_1()
            .border_b_1()
            .border_color(tokens.border)
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .items_center()
                    .child(div().text_xs().text_color(muted).child("目标主机："))
                    .children(target_chips)
                    .child(add_target),
            )
            .when(targets.is_empty(), |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(tokens.dot_connecting)
                        .child(NO_HOST_HINT),
                )
            })
            .children(refused_reason.map(|reason| {
                div()
                    .text_xs()
                    .text_color(tokens.dot_connecting)
                    .child(SharedString::from(reason))
            }));

        // ---- @ 选主机的列表 ----
        let input_text = self.input.read(cx).value().to_string();
        let mention = scope_targets::mention_query(&input_text);
        let picker_open = embed.is_some_and(|e| e.picker_open) || mention.is_some();
        let host_rows: Vec<(i64, String)> = embed
            .map(|e| {
                e.ctx
                    .hosts
                    .iter()
                    .map(|host| (host.id, host.name.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let candidates: Vec<(i64, String)> =
            scope_targets::picker_rows(&host_rows, mention.as_deref().unwrap_or(""), &targets)
                .into_iter()
                .take(8)
                .cloned()
                .collect();
        let mention_list = picker_open.then(|| {
            v_flex()
                .flex_none()
                .w_full()
                .max_h(px(200.0))
                .overflow_y_hidden()
                .px_2()
                .py_1()
                .border_t_1()
                .border_color(tokens.border)
                .bg(tokens.surface)
                .when(candidates.is_empty(), |d| {
                    d.child(
                        div()
                            .px_2()
                            .py_1()
                            .text_xs()
                            .text_color(muted)
                            .child("没有可以加入的主机"),
                    )
                })
                .children(candidates.iter().enumerate().map(|(index, (id, name))| {
                    let entity = entity.clone();
                    let host_id = *id;
                    let strip = mention.is_some();
                    let refusal = refusal_of(host_id);
                    div()
                        .id(("ai-mention", index))
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .text_sm()
                        .cursor_pointer()
                        .when(refusal.is_none(), |d| d.hover(move |style| style.bg(hover)))
                        .when(refusal.is_some(), |d| d.text_color(muted))
                        .when_some(refusal.clone(), |d, reason| {
                            d.tooltip(move |window, cx| {
                                Tooltip::new(reason.clone()).build(window, cx)
                            })
                        })
                        .child(SharedString::from(if refusal.is_some() {
                            format!("{name}（仅 SSH 主机可用）")
                        } else {
                            name.clone()
                        }))
                        .on_click(move |_, window, cx| {
                            entity.update(cx, |this, cx| {
                                if strip {
                                    let current = this.input.read(cx).value().to_string();
                                    let rest = scope_targets::strip_mention(&current);
                                    this.set_input_text(&rest, window, cx);
                                }
                                this.pin_host(host_id, cx);
                            });
                        })
                }))
        });

        // ---- 输入区 ----
        let input_height = embed.map_or(bar_height::AI_INPUT_DEFAULT, |e| e.input_height);
        let handle = {
            let entity = entity.clone();
            div()
                .id("ai-input-resize")
                .absolute()
                .left_0()
                .right_0()
                .top(px(-3.0))
                .h(px(6.0))
                .cursor_row_resize()
                .on_mouse_down(MouseButton::Left, move |event, _window, cx| {
                    cx.stop_propagation();
                    let y = f32::from(event.position.y);
                    let double = event.click_count >= 2;
                    entity.update(cx, |this, cx| {
                        if let Some(embed) = this.embed.as_mut() {
                            if double {
                                embed.input_height = bar_height::AI_INPUT_DEFAULT;
                                embed.input_drag = None;
                                this.commit_input_height();
                            } else {
                                embed.input_drag = Some((y, embed.input_height));
                            }
                        }
                        cx.notify();
                    });
                })
        };
        let (panel_cell, send_cell) = embed.map_or_else(
            || (Rc::new(Cell::new(None)), Rc::new(Cell::new(None))),
            |e| (Rc::clone(&e.panel_bounds), Rc::clone(&e.send_bounds)),
        );
        let send_button = {
            let entity = entity.clone();
            let action = items.iter().find_map(|item| match item {
                ComposerItem::Action(kind) => Some(*kind),
                _ => None,
            });
            let stopping = action == Some(ActionKind::Stop);
            Button::new("ai-send")
                .compact()
                .flex_none()
                .when(stopping, |b| b.outline().icon(Ico::Square))
                .when(!stopping, |b| b.primary().icon(IconName::ArrowUp))
                .tooltip(if stopping {
                    "停止"
                } else {
                    "发送（Enter）"
                })
                .disabled(blocked && !stopping)
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| {
                        if this.chat.is_running() {
                            this.stop(cx);
                        } else {
                            this.send(window, cx);
                        }
                    });
                })
        };
        // 输入框和工具行在同一个圆角边框里（聚焦时整个卡片高亮边框）。
        let focused = self
            .input
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx);
        let composer_card = v_flex()
            .id("ai-composer")
            .w_full()
            .min_w_0()
            .overflow_hidden()
            .rounded_lg()
            .border_1()
            .border_color(if focused {
                tokens.accent
            } else {
                tokens.border
            })
            .bg(tokens.background)
            // 回合进行中输入框也能用：Enter 把话放进待发送队列（V2-U1）。
            .child(
                Textarea::new(&self.input)
                    .w_full()
                    .h(px(input_height))
                    .appearance(false)
                    .bordered(false)
                    .disabled(blocked),
            )
            .child(
                h_flex()
                    .w_full()
                    .px_1p5()
                    .pb_1p5()
                    .gap_1()
                    .items_center()
                    // 左边这组：模型名先省略、强度再收成图标；极窄的面板里仍放不下就折到下一行，
                    // 右边的发送按钮那层不缩，面板再窄按钮也完整留在面板里。
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .flex_wrap()
                            .gap_0p5()
                            .items_center()
                            .child(mode_button)
                            .child(model_button)
                            .children(effort_button),
                    )
                    .child(
                        div()
                            .relative()
                            .flex_none()
                            .child(
                                canvas(
                                    move |bounds, _, _| send_cell.set(Some(bounds)),
                                    |_, _, _, _| {},
                                )
                                .absolute()
                                .size_full(),
                            )
                            .child(send_button),
                    ),
            );
        let input_row = v_flex()
            .id("ai-input-row")
            .relative()
            .flex_none()
            .w_full()
            .min_w_0()
            .px_2()
            .py_2()
            .border_t_1()
            .border_color(tokens.border)
            .on_mouse_move({
                let entity = entity.clone();
                move |event, _window, cx| {
                    let y = f32::from(event.position.y);
                    let pressed = event.pressed_button == Some(MouseButton::Left);
                    entity.update(cx, |this, cx| {
                        let Some(embed) = this.embed.as_mut() else {
                            return;
                        };
                        let Some((start_y, start_h)) = embed.input_drag else {
                            return;
                        };
                        if !pressed {
                            this.commit_input_height();
                        } else {
                            embed.input_height =
                                bar_height::drag_ai_input(start_h, start_y, y, panel_height);
                        }
                        cx.notify();
                    });
                }
            })
            .on_mouse_up(MouseButton::Left, {
                let entity = entity.clone();
                move |_, _window, cx| {
                    entity.update(cx, |this, cx| {
                        if this.embed.as_ref().is_some_and(|e| e.input_drag.is_some()) {
                            this.commit_input_height();
                            cx.notify();
                        }
                    });
                }
            })
            .child(handle)
            .child(composer_card);

        // ---- 底部提示 ----
        let notice = |text: String, error: bool| {
            div()
                .text_xs()
                .text_color(if error {
                    tokens.danger
                } else {
                    tokens.dot_connected
                })
                .child(SharedString::from(redact_paths(&text)))
        };
        let footer = {
            let mut lines: Vec<AnyElement> = Vec::new();
            if let Some(text) = self.resume_notice.clone() {
                lines.push(notice(text, false).into_any_element());
            }
            if let Some(n) = self.link_notice.clone() {
                lines.push(notice(n.text, n.error).into_any_element());
            }
            if let Some(n) = self.history_save.clone() {
                lines.push(notice(n.text, n.error).into_any_element());
            }
            if let Some(text) = self.footer_error.clone() {
                lines.push(notice(text, true).into_any_element());
            }
            if let Some(text) = self.settings_error.clone() {
                lines.push(notice(text, true).into_any_element());
            }
            if let Some(text) = self.storage_error.clone() {
                lines.push(
                    notice(
                        format!("设置库不可用：{text}；执行模式改动不会保存。"),
                        true,
                    )
                    .into_any_element(),
                );
            }
            (!lines.is_empty()).then(|| {
                v_flex()
                    .flex_none()
                    .w_full()
                    .px_3()
                    .pb_1p5()
                    .gap_0p5()
                    .children(lines)
            })
        };

        // 回到底部：用户往上翻了、又有消息时出现。
        let back_to_bottom = (!at_bottom && !self.chat.messages.is_empty()).then(|| {
            let entity = entity.clone();
            div()
                .id("ai-to-bottom")
                .absolute()
                .right(px(16.0))
                .bottom(px(10.0))
                .flex()
                .items_center()
                .justify_center()
                .size(px(28.0))
                .rounded_full()
                .border_1()
                .border_color(tokens.border)
                .bg(tokens.surface)
                .cursor_pointer()
                .tooltip(|window, cx| Tooltip::new("回到底部").build(window, cx))
                .child(Icon::new(IconName::ArrowDown).size_4())
                .on_click(move |_, _window, cx| {
                    entity.update(cx, |this, cx| {
                        this.scroll.scroll_to_bottom();
                        cx.notify();
                    });
                })
        });

        v_flex()
            .id("ai-panel-body")
            .size_full()
            .min_h_0()
            .track_focus(&self.focus)
            .bg(tokens.background)
            .child(
                canvas(
                    move |bounds, _, _| panel_cell.set(Some(bounds)),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(toolbar)
            .children(banners)
            .child(target_row)
            .child(
                // 竖向 flex：消息区自己的 `flex_1` 才生效，内容多了在区域里滚动，不会顶出去盖住输入框。
                v_flex()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(body_area)
                    .children(back_to_bottom),
            )
            .children(mention_list)
            .child(input_row)
            .children(footer)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{AgentModelCatalog, Embed, model_picker};
    use zssh_core::ai::agent::AgentModel;

    #[test]
    fn the_sent_model_follows_the_catalog_and_stays_empty_before_it() {
        let mut embed = Embed::new(75.0);
        assert_eq!(embed.model_for_send(), None, "还没有目录：不带模型");
        embed.set_catalog(&AgentModelCatalog {
            models: vec![
                AgentModel {
                    id: "opus/low".into(),
                    name: "Opus · low".into(),
                    unavailable: false,
                },
                AgentModel {
                    id: "opus/high".into(),
                    name: "Opus · high".into(),
                    unavailable: false,
                },
            ],
            default: None,
            default_label: None,
        });
        assert_eq!(embed.model_for_send(), None, "默认项是占位值，不带模型");
        assert_eq!(embed.selection(), model_picker::DEFAULT_VALUE);
        embed.selection = "opus/high".into();
        assert_eq!(embed.model_for_send().as_deref(), Some("opus/high"));
        // 换了目录，原来选的还在就留着。
        embed.set_catalog(&AgentModelCatalog {
            models: vec![AgentModel {
                id: "opus/high".into(),
                name: "Opus · high".into(),
                unavailable: false,
            }],
            default: None,
            default_label: None,
        });
        assert_eq!(embed.selection(), "opus/high");
    }
}
