// Adapted from Netcatty components/AIChatSidePanel.tsx (via ui/src/components/ai/AIChatPanel.tsx)
//! AI 对话嵌在主窗口右侧面板里时多出来的东西（0.2.14 `AIChatPanel.tsx` 的面板形态）：
//!
//! - 目标主机：对话在第一条消息发出时锚定到当时聚焦窗格的主机，之后不跟标签 / 分屏焦点走；没发第一条
//!   之前显示当前聚焦窗格的主机（预览）。再加用户 `@` 的主机，显示成可以 × 掉的标签（规则在
//!   `main_window::anchor`）；锚定的主机和当前标签不一致、或者已经没有打开的终端时，目标栏下面有一行提示；
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
use gpui_kit::component::menu::DropdownMenu as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::ai::agent::AgentModelCatalog;
use zssh_core::ai::agent::usage::UsageLevel;
use zssh_core::ai::bridge::ExecMode;

use super::composer::{self, ActionKind, ComposerItem, EffortStyle, ModeTone};
use super::usage_view::{self, MeterView};
use super::{
    AgentChatDemo, HistoryPanel, HostChoice, NO_HOST_HINT, mode_is_persisted, mode_label,
    redact_paths,
};
// 下面这一组只被 dev 断言方法（usage_text、dev_* 等，见 impl 里的 cfg）用。
#[cfg(debug_assertions)]
use super::{
    ApprovalDetail, ApprovalState, ChatMessage, ToolCard, ToolStatus, card_detail_lines,
    card_status_text,
};
use crate::agent_host::HostTarget;
#[cfg(debug_assertions)]
use crate::agent_tools::{self, ToolRow};
use crate::app_theme::Tokens;
use crate::icons::Ico;
use crate::main_window::anchor::{
    self, Banner, Binding, Focus, Key, LockOutcome, Open, Pin, Target,
};
use crate::main_window::bar_height;
use crate::main_window::bar_settings;
use crate::main_window::model_picker::{self, ModelPicker};
use crate::main_window::scope_targets;
use crate::menu_kit;

/// 主窗口每个节拍告诉面板的上下文：当前标签聚焦窗格、还开着终端的主机和终端、主机库里的主机。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EmbedContext {
    pub focus: Option<Focus>,
    pub open_hosts: HashSet<i64>,
    pub open_terminals: HashSet<String>,
    pub hosts: Vec<HostChoice>,
}

pub(super) struct Embed {
    ctx: EmbedContext,
    /// 对话锁定了没有：没发第一条消息之前目标栏是当前窗格的预览，发出第一条就锚定。
    binding: Binding,
    /// 用户 `@` 进来的主机（和对话一起，新对话清空）。
    pins: Vec<Pin>,
    /// 这次发送没能锚定时（Telnet / 串口 / 快速连接没连上）要告诉用户的话；发送流程清掉旧提示之后再显示。
    pending_notice: Option<String>,
    /// 标题栏的「上下文」下拉开着没有（明细卡片）。
    usage_open: bool,
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
            binding: Binding::Unlocked,
            pins: Vec::new(),
            pending_notice: None,
            usage_open: false,
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

    fn open(&self) -> Open<'_> {
        Open {
            hosts: &self.ctx.open_hosts,
            terminals: &self.ctx.open_terminals,
        }
    }

    /// 目标栏现在的标签（绑定的 / 预览的 + @ 的）。
    fn targets(&self) -> Vec<Target> {
        anchor::build(
            &self.binding,
            self.ctx.focus.as_ref(),
            &self.pins,
            &self.open(),
        )
    }

    /// 能交给桥的主机：没失效、库里还有、不是 Telnet / 串口。
    fn bridge_hosts(&self) -> Vec<i64> {
        self.targets()
            .iter()
            .filter(|target| !target.dangling)
            .filter_map(Target::host_id)
            .filter(|id| {
                self.ctx
                    .hosts
                    .iter()
                    .any(|host| host.id == *id && host.refusal.is_none())
            })
            .collect()
    }

    /// 锚定在快速连接终端上时，交给桥的那个终端目标（没失效才给）。
    fn bridge_terminal(&self) -> Option<HostTarget> {
        self.targets()
            .into_iter()
            .find_map(|target| match target.key {
                Key::Terminal(id) if !target.dangling && !id.is_empty() => Some(HostTarget {
                    id: 0,
                    name: target.name,
                    username: String::new(),
                    current: true,
                    pinned: false,
                    refusal: None,
                    terminal: Some(id),
                }),
                _ => None,
            })
    }

    /// 有没有「当前」目标（绑定的或预览的）：没有的话工具卡片上要标出主机名。
    pub(super) fn has_current_target(&self) -> bool {
        self.binding.anchor().is_some()
            || self.ctx.focus.as_ref().is_some_and(|f| f.key().is_some())
    }

    /// 这台主机不能当执行目标的原因（Telnet / 串口）。
    fn refusal_of(&self, host_id: i64) -> Option<String> {
        self.ctx
            .hosts
            .iter()
            .find(|host| host.id == host_id)
            .and_then(|host| host.refusal.clone())
    }

    /// 绑定的目标（没锁定时是预览）的身份：`@` 它时不再另加一份固定目标。
    fn bound_key(&self) -> Option<Key> {
        self.binding
            .anchor()
            .map(|anchor| anchor.key.clone())
            .or_else(|| self.ctx.focus.as_ref().and_then(Focus::key))
    }

    /// 新对话：回到未锁定，`@` 的主机一并清掉（它们是上一个对话的范围）。
    pub(super) fn reset_scope(&mut self) {
        self.binding = Binding::Unlocked;
        self.pins.clear();
        self.pending_notice = None;
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

    fn selected(&self) -> Option<(usize, Option<String>)> {
        model_picker::split_selection(&self.selection, &self.picker.groups)
    }

    #[cfg(test)]
    pub(super) fn selection(&self) -> &str {
        &self.selection
    }
}

/// 目标栏里一个标签的文字：绑定的写「已绑定」，没锁定的预览写「当前」，失效写「未连接」。
fn chip_label(target: &Target, refused: bool) -> String {
    let name = &target.name;
    if target.dangling {
        return format!("{name}（未连接）");
    }
    match (target.current, target.locked, refused) {
        (true, _, true) => format!("{name}（当前，不可用）"),
        (true, true, false) => format!("{name}（已绑定）"),
        (true, false, false) => format!("{name}（当前）"),
        (false, _, true) => format!("{name}（不可用）"),
        (false, _, false) => name.clone(),
    }
}

impl AgentChatDemo {
    /// 面板形态的对话：紧凑布局，目标主机跟着标签走。
    pub fn new_embedded(window: &mut Window, cx: &mut Context<Self>, input_height: f32) -> Self {
        let mut this = Self::new(window, cx);
        this.embed = Some(Embed::new(input_height));
        this
    }

    /// 主窗口每个节拍调一次：标签 / 焦点 / 主机库变了才有动作。对话锁定之后焦点变了只影响「当前标签」提示，
    /// 不改目标；没锁定时预览跟着焦点走。
    pub fn set_context(&mut self, ctx: EmbedContext, cx: &mut Context<Self>) {
        let Some(embed) = self.embed.as_mut() else {
            return;
        };
        if embed.ctx == ctx {
            return;
        }
        self.hosts = ctx.hosts.clone();
        embed.ctx = ctx;
        self.recompute_targets();
        cx.notify();
    }

    /// 每个目标的（当前, 固定）标记：同步给桥，没写 `target_id` 的工具调用落在「当前」那台上。
    pub(super) fn target_flags(&self) -> std::collections::HashMap<i64, (bool, bool)> {
        self.embed_targets()
            .into_iter()
            .filter_map(|target| Some((target.host_id()?, (target.current, target.pinned))))
            .collect()
    }

    /// 目标列表变了：`selected_hosts` 跟着变，已有会话马上同步给桥，「AI 使用中」标记也跟着。
    pub(super) fn recompute_targets(&mut self) {
        let Some(embed) = self.embed.as_ref() else {
            return;
        };
        // Telnet / 串口主机在目标栏里显示（置灰写原因），失效（没有打开的终端）的绑定目标也不给桥：
        // 都不能当执行目标。
        let ids = embed.bridge_hosts();
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
    pub(super) fn embed_targets(&self) -> Vec<Target> {
        let Some(embed) = self.embed.as_ref() else {
            return Vec::new();
        };
        embed
            .targets()
            .into_iter()
            // 库里已经删掉的主机：`@` 进来的不再显示；绑定的那台仍显示（灰掉，对话失效）。
            .filter(|target| {
                target.current
                    || target
                        .host_id()
                        .is_none_or(|id| embed.ctx.hosts.iter().any(|host| host.id == id))
            })
            .collect()
    }

    /// 快速连接终端当目标时交给桥的那一项。
    pub(super) fn terminal_target(&self) -> Option<HostTarget> {
        self.embed.as_ref().and_then(Embed::bridge_terminal)
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
        let (pins, already) =
            anchor::pin_host(&embed.pins, host_id, &name, embed.bound_key().as_ref());
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
            embed.pins = anchor::unpin(&embed.pins, host_id);
        }
        self.recompute_targets();
        cx.notify();
    }

    // ---- 上下文用量（V2-U15） ----

    /// 面板标题栏右侧的「上下文 ▬ 12% ∨」：主窗口画标题栏时放进去，点一下开 / 关明细卡片。
    /// 没有用量数据时写「上下文 —」，不能点开。
    pub fn usage_meter(&self, entity: Entity<Self>, tokens: &Tokens) -> AnyElement {
        let view = usage_view::meter(self.display_usage().as_ref());
        let open = self.embed.as_ref().is_some_and(|embed| embed.usage_open);
        let (muted, text, hover) = (tokens.text_muted, tokens.text, tokens.hover);
        let fill = |level: UsageLevel| match level {
            UsageLevel::Normal => tokens.accent,
            UsageLevel::Warning => tokens.dot_connecting,
            UsageLevel::Danger => tokens.danger,
        };
        let caret = || Icon::new(IconName::ChevronDown).size_3();
        let base = h_flex()
            .id("ai-usage-meter")
            .flex_none()
            .items_center()
            .gap_1p5()
            .px_1p5()
            .py_0p5()
            .rounded_md()
            .text_xs()
            .text_color(muted)
            .when(open, |d| d.bg(hover))
            .child("上下文");
        let clickable = view != MeterView::Empty;
        let body = match view {
            MeterView::Empty => base.child("—"),
            MeterView::Tokens { text: tokens_text } => base
                .child(
                    div()
                        .text_color(text)
                        .child(SharedString::from(tokens_text)),
                )
                .child(caret()),
            MeterView::Bar {
                fraction,
                percent_text,
                level,
            } => base
                .child(
                    div()
                        .relative()
                        .flex_none()
                        .w(px(44.0))
                        .h(px(4.0))
                        .rounded_full()
                        .bg(muted.opacity(0.3))
                        .child(
                            div()
                                .absolute()
                                .left_0()
                                .top_0()
                                .h_full()
                                .w(::gpui::relative(fraction.max(0.03)))
                                .rounded_full()
                                .bg(fill(level)),
                        ),
                )
                .child(
                    div()
                        .text_color(text)
                        .child(SharedString::from(percent_text)),
                )
                .child(caret()),
        };
        body.when(clickable, |d| {
            d.cursor_pointer()
                .hover(move |style| style.bg(hover).text_color(text))
                .tooltip(|window, cx| {
                    Tooltip::new("最近一轮的上下文用量（已用 / 模型上下文窗口）").build(window, cx)
                })
                .on_click(move |_, _window, cx| {
                    entity.update(cx, |this, cx| {
                        if let Some(embed) = this.embed.as_mut() {
                            embed.usage_open = !embed.usage_open;
                        }
                        cx.notify();
                    });
                })
        })
        .into_any_element()
    }

    /// 标题栏指示器上现在写的字（脚本断言用）：`上下文 —` / `上下文 12.3k` / `上下文 3%`。
    #[cfg(debug_assertions)]
    pub fn usage_text(&self) -> String {
        match usage_view::meter(self.display_usage().as_ref()) {
            MeterView::Empty => "上下文 —".to_owned(),
            MeterView::Tokens { text } => format!("上下文 {text}"),
            MeterView::Bar { percent_text, .. } => format!("上下文 {percent_text}"),
        }
    }

    /// 指示器的颜色档（脚本断言用）：`normal` / `warning` / `danger`，没有数据是 `none`。
    #[cfg(debug_assertions)]
    pub fn usage_level_name(&self) -> &'static str {
        match self.display_usage().as_ref() {
            None => "none",
            Some(usage) => match usage.level() {
                UsageLevel::Normal => "normal",
                UsageLevel::Warning => "warning",
                UsageLevel::Danger => "danger",
            },
        }
    }

    /// 开 / 关明细卡片（脚本用，和点指示器同一个方法）。
    pub fn set_usage_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if let Some(embed) = self.embed.as_mut() {
            embed.usage_open = open;
        }
        cx.notify();
    }

    // ---- 锚定（V2-U15） ----

    /// 发送之前：没锁定就按当前聚焦窗格锁定；锁定的目标已经没有打开的终端就拦住这次发送（返回原因，
    /// 不自动改绑）。当前窗格不能锚定（Telnet / 串口 / 快速连接没连上）时保持未锁定，原因在
    /// [`Self::show_anchor_notice`] 里提示。
    pub(super) fn anchor_gate(&mut self) -> Option<String> {
        let embed = self.embed.as_mut()?;
        match anchor::lock_on_send(&embed.binding, embed.ctx.focus.as_ref()) {
            LockOutcome::Already | LockOutcome::NoFocus => {}
            LockOutcome::Locked(anchor) => embed.binding = Binding::Locked(anchor),
            LockOutcome::Skipped(why) => embed.pending_notice = Some(why.message()),
        }
        let reason = match &embed.binding {
            Binding::Locked(anchor) if !anchor::is_alive(anchor, &embed.open()) => Some(format!(
                "「{}」没有打开的终端，先重新连接或改用当前主机",
                anchor.name
            )),
            _ => None,
        };
        self.recompute_targets();
        reason
    }

    /// 发送流程清掉旧提示之后，把「没能锚定」的原因放到底部提示里。
    pub(super) fn show_anchor_notice(&mut self) {
        if let Some(text) = self.embed.as_mut().and_then(|e| e.pending_notice.take()) {
            self.link_notice = Some(super::Notice { text, error: false });
        }
    }

    /// 「新对话」：回到未锁定（目标栏重新跟着当前窗格），`@` 的主机清空。
    pub(super) fn reset_anchor(&mut self) {
        if let Some(embed) = self.embed.as_mut() {
            embed.reset_scope();
        }
        self.recompute_targets();
    }

    /// 「继续」历史会话：回到它当初锚定的主机；旧历史没有锚定就保持未锁定。
    pub(super) fn restore_anchor(&mut self, saved: Option<&zssh_core::ai::history::HistoryAnchor>) {
        let Some(embed) = self.embed.as_mut() else {
            return;
        };
        embed.reset_scope();
        if let Some(saved) = saved {
            let hosts = &embed.ctx.hosts;
            embed.binding =
                Binding::Locked(anchor::from_history(saved.host_id, &saved.name, |id| {
                    hosts
                        .iter()
                        .find(|host| host.id == id)
                        .map(|host| host.name.clone())
                }));
        }
        self.recompute_targets();
    }

    /// 存历史用的锚定（快速连接只存占位名）。
    pub(super) fn history_anchor(&self) -> Option<zssh_core::ai::history::HistoryAnchor> {
        let anchor = self.embed.as_ref()?.binding.anchor()?;
        let (host_id, name) = anchor::to_history(anchor);
        Some(zssh_core::ai::history::HistoryAnchor { host_id, name })
    }

    /// 「改用 <当前>」（对应 Netcatty 的 /rebind）：之后的命令跑在当前聚焦窗格的主机上，对话里插一条系统提示。
    pub fn rebind_to_current(&mut self, cx: &mut Context<Self>) {
        if self.chat.is_running() {
            self.link_notice = Some(super::Notice {
                text: "AI 正在回答，等这一轮结束再改用当前主机".to_owned(),
                error: false,
            });
            cx.notify();
            return;
        }
        let Some(embed) = self.embed.as_mut() else {
            return;
        };
        match anchor::rebind(embed.ctx.focus.as_ref()) {
            None => return,
            Some(Err(why)) => {
                self.link_notice = Some(super::Notice {
                    text: why.message(),
                    error: true,
                });
            }
            Some(Ok(anchor)) => {
                let name = anchor.name.clone();
                embed.binding = Binding::Locked(anchor);
                // 对话里留一条记录：之后的命令跑在哪台主机上，翻回去看得明白。
                self.chat.push_system_line(format!("已改绑到 {name}"));
                self.link_notice = None;
                self.recompute_targets();
                // 历史里的锚定跟着改（对话已经存过的话）。
                self.save_history();
            }
        }
        cx.notify();
    }

    /// 「重新连接 <X>」：打开绑定主机的标签（主窗口下一个节拍处理）。快速连接的终端回不来，没有这个按钮。
    pub fn reopen_anchor_host(&mut self, cx: &mut Context<Self>) {
        let host = self
            .embed
            .as_ref()
            .and_then(|embed| embed.binding.anchor())
            .and_then(|anchor| match anchor.key {
                Key::Host(id) => Some(id),
                Key::Terminal(_) => None,
            });
        if let Some(host) = host {
            crate::agent_setup::request_open_host(host);
        }
        cx.notify();
    }

    /// 目标栏下面那一行提示的内容（脚本断言用）。
    #[cfg(debug_assertions)]
    pub fn banner_text(&self) -> Option<String> {
        let embed = self.embed.as_ref()?;
        anchor::banner(&embed.binding, embed.ctx.focus.as_ref(), &embed.open()).map(|banner| {
            match banner {
                Banner::Mismatch {
                    anchor_name,
                    current_name,
                    ..
                } => format!("对话绑定在 {anchor_name}；当前标签是 {current_name}"),
                Banner::Dangling { anchor_name, .. } => format!("{anchor_name} 没有打开的终端"),
            }
        })
    }

    /// 绑定的主机名（没锁定是 `None`）。
    #[cfg(debug_assertions)]
    pub fn anchor_name(&self) -> Option<String> {
        self.embed
            .as_ref()
            .and_then(|embed| embed.binding.anchor())
            .map(|anchor| anchor.name.clone())
    }

    /// 目标栏每个标签的文字（脚本断言用）。
    #[cfg(debug_assertions)]
    pub fn target_chip_texts(&self) -> Vec<String> {
        let Some(embed) = self.embed.as_ref() else {
            return Vec::new();
        };
        self.embed_targets()
            .iter()
            .map(|target| {
                let refused = target
                    .host_id()
                    .and_then(|id| embed.refusal_of(id))
                    .is_some();
                chip_label(target, refused)
            })
            .collect()
    }

    /// 现在对话的目标主机 id（按目标栏的顺序）。
    #[cfg(debug_assertions)]
    pub fn target_ids(&self) -> Vec<i64> {
        self.selected_hosts.clone()
    }

    #[cfg(debug_assertions)]
    pub fn is_turn_running(&self) -> bool {
        self.chat.is_running()
    }

    /// 输入框里的内容（驱动脚本用）。
    #[cfg(debug_assertions)]
    pub fn input_text(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    pub fn set_input_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = text.to_owned();
        self.input
            .update(cx, |state, cx| state.set_value(text, window, cx));
    }

    #[cfg(debug_assertions)]
    pub fn send_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.send(window, cx);
    }

    /// 发送按钮整个在面板里（上一帧量到的位置）。还没量到返回 `None`。
    #[cfg(debug_assertions)]
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
    #[cfg(debug_assertions)]
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

    #[cfg(debug_assertions)]
    pub fn message_count(&self) -> usize {
        self.chat.messages.len()
    }

    /// 最后一条助手消息的正文（驱动脚本核对真实回复用）。
    #[cfg(debug_assertions)]
    pub fn last_assistant_text(&self) -> Option<String> {
        self.chat
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                super::ChatMessage::Assistant(message) => Some(message.text.clone()),
                super::ChatMessage::User(_) | ChatMessage::System(_) => None,
            })
    }

    #[cfg(debug_assertions)]
    pub fn footer_error_text(&self) -> Option<String> {
        self.footer_error.clone()
    }

    // ---- 驱动脚本用的读写口（调试版自动化；走和按钮同一批方法）----

    /// 当前智能体的 id（`opencode` / `codex` / `claude` / `builtin`）。
    #[cfg(debug_assertions)]
    pub fn agent_id(&self) -> &'static str {
        self.agent.id()
    }

    /// 当前是内置助手吗（脚本放行"发送"时用：内置助手配了假接口就不联网）。
    #[cfg(debug_assertions)]
    pub fn agent_is_builtin(&self) -> bool {
        self.is_builtin()
    }

    /// 换智能体（和下拉里点一项同一个方法）；这一项不可选（没装 / 没登录 / 版本过低）时说明原因。
    #[cfg(debug_assertions)]
    pub fn switch_agent_by_id(&mut self, id: &str, cx: &mut Context<Self>) -> Result<(), String> {
        // `builtin:<接口名>`：内置助手的某个接口（脚本用，和下拉里点那一项同一个方法）。
        if let Some(name) = id.strip_prefix("builtin:") {
            let info = crate::agent_setup::builtin::info();
            let endpoint = info
                .selectable()
                .into_iter()
                .find(|endpoint| endpoint.name == name)
                .ok_or("没有这个 ZeePilot 接口，或它没配 Key")?;
            self.switch_builtin_endpoint(endpoint.id, cx);
            return Ok(());
        }
        let kind = crate::agent_setup::AgentKind::parse(id).ok_or("不认识的智能体")?;
        if let Some(note) = self.status.get(kind).availability().selector_note() {
            return Err(format!("{} 不可选：{note}", kind.label()));
        }
        self.switch_agent(kind, cx);
        Ok(())
    }

    /// 当前智能体的可用状态名。
    #[cfg(debug_assertions)]
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
    #[cfg(debug_assertions)]
    pub fn is_blocked(&self) -> bool {
        self.real && self.availability().blocks_input()
    }

    /// 底部提示、各种通知和对话里的提示合在一起的文字（脚本断言用，不显示）。
    #[cfg(debug_assertions)]
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
    #[cfg(debug_assertions)]
    pub fn model_labels(&self) -> Vec<String> {
        self.embed
            .as_ref()
            .map(|e| e.picker.groups.iter().map(|g| g.label.clone()).collect())
            .unwrap_or_default()
    }

    /// 当前模型框显示的文字（没有目录是「默认模型」）。
    #[cfg(debug_assertions)]
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
    #[cfg(debug_assertions)]
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
    #[cfg(debug_assertions)]
    pub fn effort_label_now(&self) -> String {
        let Some(embed) = self.embed.as_ref() else {
            return String::new();
        };
        model_picker::effort_label(embed.selected().and_then(|(_, e)| e).as_deref())
    }

    /// 发消息时会带的模型（`None` = 智能体自己的默认）。
    #[cfg(debug_assertions)]
    pub fn model_for_send_now(&self) -> Option<String> {
        self.embed.as_ref().and_then(Embed::model_for_send)
    }

    #[cfg(debug_assertions)]
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

    #[cfg(debug_assertions)]
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
    #[cfg(debug_assertions)]
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
    #[cfg(debug_assertions)]
    pub fn history_count(&self) -> usize {
        self.history_rows.len()
    }

    #[cfg(debug_assertions)]
    pub fn toggle_history_panel(&mut self, cx: &mut Context<Self>) {
        self.toggle_history(cx);
    }

    /// 在历史列表里点「继续」第 n 条。
    #[cfg(debug_assertions)]
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

    #[cfg(debug_assertions)]
    pub fn exec_mode(&self) -> ExecMode {
        self.current_mode()
    }

    #[cfg(debug_assertions)]
    pub fn is_real(&self) -> bool {
        self.real
    }

    #[cfg(debug_assertions)]
    pub fn clear_conversation(&mut self, cx: &mut Context<Self>) {
        self.new_conversation(cx);
    }

    // ---- 调试驱动用的读写口（V2-G10b；走和按钮同一批方法）----

    /// 「停止」按钮。
    #[cfg(debug_assertions)]
    pub fn dev_stop(&mut self, cx: &mut Context<Self>) {
        self.stop(cx);
    }

    /// 往当前助手消息里放一条提示（`error` 为真是红色的错误），截图 / 复制验证用，不进历史。
    #[cfg(debug_assertions)]
    pub fn dev_push_notice(&mut self, text: &str, error: bool, cx: &mut Context<Self>) {
        self.chat.push_notice(text, error);
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// 消息区滚到最新（截图前用）。
    #[cfg(debug_assertions)]
    pub fn dev_scroll_bottom(&mut self, cx: &mut Context<Self>) {
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// 等着用户批准的确认卡片个数。
    #[cfg(debug_assertions)]
    pub fn dev_pending_approvals(&self) -> usize {
        self.chat.approvals.len()
    }

    /// 点第一张等批准的确认卡片上的「允许」/「拒绝」。
    #[cfg(debug_assertions)]
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
    #[cfg(debug_assertions)]
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
    #[cfg(debug_assertions)]
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
    #[cfg(debug_assertions)]
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
    #[cfg(debug_assertions)]
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
    #[cfg(debug_assertions)]
    pub fn dev_view_terminal(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let exec = self
            .chat
            .messages
            .iter()
            .filter_map(|message| match message {
                ChatMessage::Assistant(message) => Some(message),
                ChatMessage::User(_) | ChatMessage::System(_) => None,
            })
            .flat_map(|message| message.tools.iter())
            .find_map(|card| card.exec.clone())
            .ok_or_else(|| "没有带执行位置的卡片".to_owned())?;
        self.view_terminal(&exec, cx);
        Ok(())
    }

    /// 「思考中」现在显示着吗 / 重试提示的文字。
    #[cfg(debug_assertions)]
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
        if mode_is_persisted(self.real) {
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
                            menu_kit::choice(label, current_group == Some(index)).on_click(
                                move |_: &ClickEvent, _window, cx| {
                                    entity.update(cx, |this, cx| this.pick_model(index, cx));
                                },
                            ),
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
                            menu_kit::choice(
                                model_picker::effort_label(choice.as_deref()),
                                &current == choice,
                            )
                            .on_click(
                                move |_: &ClickEvent, _window, cx| {
                                    let picked = picked.clone();
                                    entity.update(cx, |this, cx| this.pick_effort(picked, cx));
                                },
                            ),
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
            let caution = tokens.caution;
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
                            menu_kit::choice(
                                format!("{}  ·  {hint}", mode_label(mode)),
                                current == mode,
                            )
                            .on_click(
                                move |_: &ClickEvent, _window, cx| {
                                    entity.update(cx, |this, cx| {
                                        this.request_mode(mode, cx);
                                    });
                                },
                            ),
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
        let refusal_of =
            |host_id: i64| -> Option<String> { embed.and_then(|e| e.refusal_of(host_id)) };
        let refused_reason: Option<String> = targets
            .iter()
            .find_map(|target| target.host_id().and_then(refusal_of));
        let target_chips = targets.iter().enumerate().map(|(index, target)| {
            let entity = entity.clone();
            let host_id = target.host_id();
            let removable = target.pinned && !target.current;
            let refusal = host_id.and_then(refusal_of);
            let dangling = target.dangling;
            let grayed = refusal.is_some() || dangling;
            let label: SharedString = chip_label(target, refusal.is_some()).into();
            let tip: Option<String> = refusal
                .clone()
                .or_else(|| dangling.then(|| format!("{} 没有打开的终端", target.name)));
            h_flex()
                .id(("ai-target", index))
                .items_center()
                .gap_1()
                .px_2()
                .py_0p5()
                .rounded_md()
                .border_1()
                .border_color(tokens.border)
                .when(!grayed, |d| d.bg(tokens.accent_soft))
                .when(grayed, |d| d.text_color(muted))
                .when_some(tip, |d, reason| {
                    d.tooltip(move |window, cx| Tooltip::new(reason.clone()).build(window, cx))
                })
                .text_xs()
                .child(div().truncate().max_w(px(160.0)).child(label))
                .when_some(host_id.filter(|_| removable), |d, host_id| {
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
        // 目标栏下面一行：对话绑定的主机和当前标签不一致（淡色提示）/ 绑定的主机已经没有打开的终端（警示色）。
        let banner =
            embed.and_then(|e| anchor::banner(&e.binding, e.ctx.focus.as_ref(), &e.open()));
        let small_button = |id: &'static str, label: String| {
            div()
                .id(id)
                .px_1p5()
                .py_0p5()
                .rounded_md()
                .border_1()
                .border_color(tokens.border)
                .cursor_pointer()
                .text_color(text)
                .hover(move |style| style.bg(hover))
                .child(SharedString::from(label))
        };
        // 警示色：浅色底上用深一点的琥珀色保证读得清（和「全部放行」同一个）。
        let warn = tokens.caution;
        let banner_row = banner.map(|banner| {
            let new_convo = {
                let entity = entity.clone();
                small_button("ai-banner-new", "新对话".to_owned()).on_click(move |_, _w, cx| {
                    entity.update(cx, |this, cx| this.new_conversation(cx));
                })
            };
            let row = h_flex()
                .id("ai-anchor-banner")
                .gap_1()
                .flex_wrap()
                .items_center()
                .text_xs();
            match banner {
                Banner::Mismatch {
                    anchor_name,
                    current_name,
                    can_rebind,
                } => row
                    .text_color(muted)
                    .child(SharedString::from(format!(
                        "对话绑定在 {anchor_name}；当前标签是 {current_name}"
                    )))
                    .when(can_rebind, |d| {
                        let entity = entity.clone();
                        d.child(
                            small_button("ai-rebind", format!("改用 {current_name}")).on_click(
                                move |_, _w, cx| {
                                    entity.update(cx, |this, cx| this.rebind_to_current(cx));
                                },
                            ),
                        )
                    })
                    .child(new_convo),
                Banner::Dangling {
                    anchor_name,
                    can_reconnect,
                    can_use_current,
                } => row
                    .text_color(warn)
                    .child(SharedString::from(format!(
                        "{anchor_name} 没有打开的终端，对话先停在这里"
                    )))
                    .when(can_reconnect, |d| {
                        let entity = entity.clone();
                        d.child(
                            small_button("ai-reconnect", format!("重新连接 {anchor_name}"))
                                .on_click(move |_, _w, cx| {
                                    entity.update(cx, |this, cx| this.reopen_anchor_host(cx));
                                }),
                        )
                    })
                    .when(can_use_current, |d| {
                        let entity = entity.clone();
                        d.child(
                            small_button("ai-use-current", "改用当前主机".to_owned()).on_click(
                                move |_, _w, cx| {
                                    entity.update(cx, |this, cx| this.rebind_to_current(cx));
                                },
                            ),
                        )
                    })
                    .child(new_convo),
            }
        });
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
                        .text_color(tokens.caution)
                        .child(NO_HOST_HINT),
                )
            })
            .children(refused_reason.map(|reason| {
                div()
                    .text_xs()
                    .text_color(tokens.caution)
                    .child(SharedString::from(reason))
            }))
            .children(banner_row);

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
        // 内置助手的当前模型不支持工具调用：输入框上方一行淡色提示（只能聊天，不能操作服务器）。
        let no_tools_row = self.no_tools_hint().map(|hint| {
            div()
                .id("ai-no-tools-hint")
                .w_full()
                .px_1()
                .pb_1p5()
                .text_xs()
                .text_color(muted)
                .child(hint)
        });
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
            .children(no_tools_row)
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

        // 「上下文」明细卡片：点标题栏的指示器打开，点卡片外面关上。只展示，没有任何操作。
        let shown_usage = self.display_usage();
        let usage_card = embed
            .filter(|e| e.usage_open)
            .and(shown_usage.as_ref())
            .map(|usage| {
                let fallback =
                    (model_label.as_ref() != "默认模型").then(|| model_label.to_string());
                let lines = usage_view::detail_lines(usage, fallback.as_deref());
                let close = entity.clone();
                let mut card = v_flex()
                    .id("ai-usage-card")
                    .absolute()
                    .top(px(6.0))
                    .right(px(8.0))
                    .w(px(272.0))
                    .max_w_full()
                    .p_3()
                    .gap_1p5()
                    .rounded_lg()
                    .border_1()
                    .border_color(tokens.border)
                    .bg(tokens.surface)
                    .shadow_md()
                    .occlude()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("上下文用量"),
                    );
                for (index, (name, value)) in lines.into_iter().enumerate() {
                    // 「已用 / 窗口」是总览，下面几行是本轮的明细：中间留一条小标题。
                    if index == 1 {
                        card = card.child(div().pt_1().text_xs().text_color(muted).child("本轮"));
                    }
                    let stacked = name == "已用 / 窗口" || name == "模型";
                    card = card.child(if stacked {
                        v_flex()
                            .gap_0p5()
                            .text_xs()
                            .child(div().text_color(muted).child(name))
                            .child(div().text_color(text).child(SharedString::from(value)))
                            .into_any_element()
                    } else {
                        h_flex()
                            .justify_between()
                            .gap_2()
                            .text_xs()
                            .child(div().text_color(muted).child(name))
                            .child(div().text_color(text).child(SharedString::from(value)))
                            .into_any_element()
                    });
                }
                div()
                    .id("ai-usage-layer")
                    .absolute()
                    .size_full()
                    .on_mouse_down(MouseButton::Left, move |_, _window, cx| {
                        close.update(cx, |this, cx| this.set_usage_open(false, cx));
                    })
                    .child(card)
            });

        v_flex()
            .id("ai-panel-body")
            .relative()
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
            .children(usage_card)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use std::collections::HashSet;

    use super::{
        AgentModelCatalog, Binding, Embed, EmbedContext, Focus, HostChoice, LockOutcome, Pin,
        chip_label, model_picker,
    };
    use crate::main_window::anchor;
    use zssh_core::ai::agent::AgentModel;

    fn hosts() -> Vec<HostChoice> {
        let mut telnet = HostChoice::plain(5, "switch-1", "admin");
        telnet.refusal = Some("Telnet 主机不能让 AI 执行命令".to_owned());
        vec![
            HostChoice::plain(1, "web-1", "root"),
            HostChoice::plain(2, "db-1", "root"),
            HostChoice::plain(3, "cache-1", "root"),
            telnet,
        ]
    }

    fn on_host(id: i64, name: &str) -> Focus {
        Focus::Host {
            id,
            name: name.to_owned(),
            refusal: None,
        }
    }

    fn embed_with(focus: Option<Focus>, open: &[i64]) -> Embed {
        let mut embed = Embed::new(75.0);
        embed.ctx = EmbedContext {
            focus,
            open_hosts: open.iter().copied().collect(),
            open_terminals: HashSet::new(),
            hosts: hosts(),
        };
        embed
    }

    /// 和 `AgentChatDemo::anchor_gate` 里锁定那一步一样的规则。
    fn lock(embed: &mut Embed) {
        if let LockOutcome::Locked(anchor) =
            anchor::lock_on_send(&embed.binding, embed.ctx.focus.as_ref())
        {
            embed.binding = Binding::Locked(anchor);
        }
    }

    #[test]
    fn the_bridge_follows_the_preview_until_the_first_message_then_stays_on_the_anchor() {
        let mut embed = embed_with(Some(on_host(1, "web-1")), &[1, 2]);
        assert_eq!(embed.bridge_hosts(), vec![1]);
        embed.ctx.focus = Some(on_host(2, "db-1"));
        assert_eq!(embed.bridge_hosts(), vec![2], "没发消息：预览跟着焦点走");
        lock(&mut embed);
        for focus in [on_host(1, "web-1"), on_host(3, "cache-1")] {
            embed.ctx.focus = Some(focus);
            assert_eq!(
                embed.bridge_hosts(),
                vec![2],
                "锁定后切标签 / 切分屏焦点不变"
            );
        }
        embed.ctx.focus = None;
        assert_eq!(embed.bridge_hosts(), vec![2]);
        // @ 一台加进来、再去掉；绑定的那台不受影响。
        embed.pins = vec![Pin {
            host_id: 3,
            name: "cache-1".to_owned(),
        }];
        assert_eq!(embed.bridge_hosts(), vec![2, 3]);
        embed.pins.clear();
        assert_eq!(embed.bridge_hosts(), vec![2]);
        embed.reset_scope();
        assert_eq!(embed.binding, Binding::Unlocked);
    }

    #[test]
    fn a_dangling_anchor_is_withdrawn_from_the_bridge_but_pins_stay() {
        let mut embed = embed_with(Some(on_host(1, "web-1")), &[1, 2]);
        lock(&mut embed);
        embed.pins = vec![Pin {
            host_id: 3,
            name: "cache-1".to_owned(),
        }];
        assert_eq!(embed.bridge_hosts(), vec![1, 3]);
        // web-1 的标签全关了：不再是执行目标；@ 的主机（不需要标签）照旧。
        embed.ctx.open_hosts = [2].into_iter().collect();
        assert_eq!(embed.bridge_hosts(), vec![3]);
        let targets = embed.targets();
        assert!(targets[0].dangling && targets[0].current);
        assert_eq!(chip_label(&targets[0], false), "web-1（未连接）");
        // 重新连上（又有标签了）：自动回来，不用改绑。
        embed.ctx.open_hosts = [1, 2].into_iter().collect();
        assert_eq!(embed.bridge_hosts(), vec![1, 3]);
    }

    #[test]
    fn telnet_and_serial_panes_neither_lock_nor_reach_the_bridge() {
        let telnet = Focus::Host {
            id: 5,
            name: "switch-1".to_owned(),
            refusal: Some("Telnet 主机不能让 AI 执行命令".to_owned()),
        };
        let mut embed = embed_with(Some(telnet), &[5]);
        assert!(embed.bridge_hosts().is_empty(), "目标栏显示、但不给桥");
        lock(&mut embed);
        assert_eq!(embed.binding, Binding::Unlocked, "不锚定，保持未锁定");
        assert_eq!(
            chip_label(&embed.targets()[0], embed.refusal_of(5).is_some()),
            "switch-1（当前，不可用）"
        );
        // 焦点回到 SSH 窗格后，下一次发送再锁定。
        embed.ctx.focus = Some(on_host(1, "web-1"));
        lock(&mut embed);
        assert_eq!(
            embed.binding.anchor().map(|a| a.name.as_str()),
            Some("web-1")
        );
    }

    #[test]
    fn a_quick_connect_terminal_is_the_bridge_target() {
        let mut embed = embed_with(
            Some(Focus::Quick {
                terminal: Some("term-4".to_owned()),
                label: "ops@192.0.2.9".to_owned(),
            }),
            &[],
        );
        embed.ctx.open_terminals = ["term-4".to_owned()].into_iter().collect();
        assert!(embed.bridge_hosts().is_empty(), "不是主机库里的主机");
        let preview = embed.bridge_terminal().expect("预览也给桥（同一个规则）");
        assert_eq!(preview.scope_id(), "term-4");
        lock(&mut embed);
        embed.ctx.focus = Some(on_host(2, "db-1"));
        let bound = embed.bridge_terminal().expect("锁定后切到别的标签不变");
        assert_eq!(bound.scope_id(), "term-4");
        assert!(bound.current);
        assert_eq!(
            chip_label(&embed.targets()[0], false),
            "ops@192.0.2.9（已绑定）"
        );
        // 终端关了：失效，不再给桥。
        embed.ctx.open_terminals.clear();
        assert!(embed.bridge_terminal().is_none());
        assert!(embed.targets()[0].dangling);
    }

    #[test]
    fn restoring_from_history_goes_back_to_the_saved_host() {
        let mut embed = embed_with(Some(on_host(2, "db-1")), &[1, 2]);
        let saved = anchor::from_history(Some(1), "web-1", |_| Some("web-1".to_owned()));
        embed.binding = Binding::Locked(saved);
        assert_eq!(
            embed.bridge_hosts(),
            vec![1],
            "回到当初锚定的主机，不是当前标签"
        );
        let (host_id, name) = anchor::to_history(embed.binding.anchor().expect("锁定"));
        assert_eq!((host_id, name.as_str()), (Some(1), "web-1"));
    }

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
