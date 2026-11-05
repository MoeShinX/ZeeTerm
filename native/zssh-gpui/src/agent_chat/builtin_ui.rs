//! AI 面板里内置助手（V2-A4）的那一部分：选哪个接口、模型下拉的目录、上下文指示器的分母、
//! 旁路信号（用量 / 提示 / 失败）翻成对话里的东西、重试时撤回已经流出的半段正文、不支持工具的提示。
//!
//! 业务（能力识别、请求参数映射、对话循环）都是 core 的，这里只拼装和显示；
//! 接口清单、目录构造等不依赖界面的部分在 `agent_setup::builtin`（有状态测试）。

use std::sync::Arc;

use zssh_core::ai::agent::AgentEvent;
use zssh_core::ai::agent::builtin::catalog::{Catalog, ModelCaps};
use zssh_core::ai::agent::builtin::config::AiEndpoint;
use zssh_core::ai::agent::builtin::{BuiltinSignal, FailureInfo};
use zssh_core::ai::agent::usage::ContextUsage;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::*;
use zssh_core::ai::agent::builtin::tokenway::endpoint::is_authorized_token;
use zssh_core::ai::agent::builtin::tokenway::{ChatFailureAction, classify_chat_failure};

use crate::app_theme::Tokens;

use super::ChatMsg;
use super::{
    AgentChatDemo, AgentSession, CardKind, ChatMessage, ChatState, HostEvent, ModelChoice,
    start_builtin_agent,
};
use crate::agent_setup::AgentKind;
use crate::agent_setup::builtin::{self, RemoteModels};
use crate::i18n::{Msg, t};
use crate::ssh_backend::runtime;

/// 内置助手自己的按钮 / 提示文案。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChatBuiltinMsg {
    Reconnect,
    TopUp,
    StartFailedPrefix,
    NoToolsHint,
}

impl Msg for ChatBuiltinMsg {
    fn zh(&self) -> &'static str {
        let key = *self;
        match key {
            Self::Reconnect => "重新连接",
            Self::TopUp => "充值",
            Self::StartFailedPrefix => "启动 ZeePilot 失败：",
            Self::NoToolsHint => "该模型不支持工具调用，只能聊天",
        }
    }

    fn en(&self) -> &'static str {
        let key = *self;
        match key {
            Self::Reconnect => "Reconnect",
            Self::TopUp => "Top Up",
            Self::StartFailedPrefix => "Failed to start ZeePilot: ",
            Self::NoToolsHint => "This model does not support tool calls; it can only chat",
        }
    }
}

/// 面板里内置助手的状态。
#[derive(Default)]
pub(super) struct BuiltinPanel {
    /// 当前选中的接口（设置里的接口 id）；只有智能体是内置助手时才有意义。
    pub(super) endpoint: Option<i64>,
    ctx: Option<Ctx>,
}

/// 选中接口的配置、接口自报的模型清单和模型资料库——模型能力都从这三样合成。
struct Ctx {
    endpoint: AiEndpoint,
    remote: Option<Arc<RemoteModels>>,
    catalog: Arc<Catalog>,
}

impl Ctx {
    /// 接口里已知的模型名（拆"模型/强度"串时整串命中就不拆）。
    fn known_models(&self) -> Vec<String> {
        let mut known: Vec<String> = self.endpoint.models.clone();
        known.extend(self.endpoint.default_model.clone());
        if let Some(remote) = &self.remote {
            known.extend(remote.order.iter().cloned());
        }
        known
    }

    fn caps(&self, model: &str) -> ModelCaps {
        builtin::caps_for(&self.endpoint, self.remote.as_deref(), &self.catalog, model)
    }
}

impl AgentChatDemo {
    pub(super) fn is_builtin(&self) -> bool {
        self.agent == AgentKind::Builtin
    }

    /// 智能体下拉里内置助手的几项：（接口 id，显示名，是不是当前选中）。
    pub(super) fn builtin_menu_items(&self) -> Vec<(i64, String, bool)> {
        let info = builtin::info();
        info.selectable()
            .into_iter()
            .map(|endpoint| {
                (
                    endpoint.id,
                    info.label_for(endpoint.id),
                    self.is_builtin() && self.builtin.endpoint == Some(endpoint.id),
                )
            })
            .collect()
    }

    /// 按钮上现在写的智能体名（内置助手带接口名）。
    pub(super) fn agent_label_now(&self) -> String {
        if self.is_builtin()
            && let Some(id) = self.builtin.endpoint
        {
            return builtin::info().label_for(id);
        }
        self.agent.label().to_owned()
    }

    /// 选定内置助手的默认接口（切到内置助手、启动时用）。
    pub(super) fn pick_default_builtin_endpoint(&mut self) {
        self.builtin.endpoint = builtin::info()
            .default_endpoint()
            .map(|endpoint| endpoint.id);
        self.builtin.ctx = None;
    }

    /// 切到内置助手的某个接口（下拉里点一项）。已经开始的对话换了接口 = 新对话（每个对话固定一个）。
    pub(super) fn switch_builtin_endpoint(&mut self, id: i64, cx: &mut Context<Self>) {
        if self.is_builtin() && self.builtin.endpoint == Some(id) {
            return;
        }
        let info = builtin::info();
        if info.get(id).is_none_or(|endpoint| !endpoint.ready) {
            return;
        }
        let started = self.session.is_some()
            || self.starting
            || self.pending_resume.is_some()
            || !self.chat.messages.is_empty();
        if started {
            self.new_conversation(cx);
        }
        self.agent = AgentKind::Builtin;
        self.builtin.endpoint = Some(id);
        self.builtin.ctx = None;
        self.applied_catalog = None;
        self.models = vec![ModelChoice::default()];
        self.model_index = 0;
        if let Some(embed) = self.embed.as_mut() {
            embed.clear_catalog();
        }
        if self.history == super::HistoryPanel::List {
            self.refresh_history();
        } else {
            self.close_history();
        }
        let label = info.label_for(id);
        self.link_notice = Some(super::Notice {
            text: if started {
                format!(
                    "{}{label}{}",
                    t(ChatMsg::SwitchedToPrefix),
                    t(ChatMsg::SwitchedToNewSuffix)
                )
            } else {
                format!("{}{label}", t(ChatMsg::SwitchedToPrefix))
            },
            error: false,
        });
        self.footer_error = None;
        cx.notify();
    }

    /// 选中的接口是浏览器授权的 Tokenway 令牌吗（是就返回接口 id）：只有它有余额接口和"重新连接"。
    pub fn tokenway_endpoint(&self) -> Option<i64> {
        if !self.is_builtin() {
            return None;
        }
        let id = self.builtin.endpoint?;
        let info = builtin::info();
        let endpoint = info.get(id)?;
        is_authorized_token(endpoint.preset_id.as_deref()).then_some(id)
    }

    /// 标题栏上上下文指示器旁边的余额（V2-A5）：「余额 ¥12.34」；没有可用的充值余额时是「需充值」+「充值」按钮；
    /// 授权被服务端停用时是「授权已失效」+「重新连接」按钮。还没取到余额（或不是 Tokenway）就不显示。
    pub fn balance_chip(&self, tokens: &Tokens) -> Option<AnyElement> {
        let id = self.tokenway_endpoint()?;
        let view = crate::agent_setup::tokenway::balance_view(id)?;
        let invalid = view.problem
            == Some(crate::agent_setup::tokenway::BalanceProblem::AuthorizationInvalid);
        let color = if invalid || view.needs_topup {
            tokens.danger
        } else {
            tokens.text
        };
        let detail = SharedString::from(view.detail.clone());
        let label = div()
            .id("ai-balance")
            .flex_none()
            .text_xs()
            .text_color(color)
            .child(SharedString::from(view.label.clone()))
            .tooltip(move |window, cx| Tooltip::new(detail.clone()).build(window, cx));
        let mut chip = h_flex()
            .flex_none()
            .items_center()
            .gap_1()
            .ml_2()
            .child(label);
        if invalid {
            chip = chip.child(
                Button::new("ai-balance-reconnect")
                    .label(t(ChatBuiltinMsg::Reconnect))
                    .ghost()
                    .compact()
                    .on_click(|_, _, _| crate::agent_setup::tokenway::request_reconnect()),
            );
        } else if view.needs_topup {
            chip = chip.child(
                Button::new("ai-balance-topup")
                    .label(t(ChatBuiltinMsg::TopUp))
                    .ghost()
                    .compact()
                    .on_click(|_, _, _| {
                        let _ = crate::agent_setup::tokenway::open_topup();
                    }),
            );
        }
        Some(chip.into_any_element())
    }

    /// 每个节拍：选中的接口还在不在、配置改了没有、接口自报的模型清单到了没有、模型下拉套用了没有。
    pub(super) fn sync_builtin_state(&mut self) -> bool {
        let mut changed = false;
        let info = builtin::info();
        // 401 时给不给「重新连接」要看这个对话用的是不是浏览器授权的 Tokenway 令牌。
        self.chat.tokenway_token = self.tokenway_endpoint().is_some();
        // 选中的接口被删了 / 没 Key 了：换回默认接口。
        let valid = self
            .builtin
            .endpoint
            .and_then(|id| info.get(id))
            .filter(|endpoint| endpoint.ready)
            .map(|endpoint| endpoint.id);
        if valid != self.builtin.endpoint || valid.is_none() {
            let fallback = valid.or_else(|| info.default_endpoint().map(|e| e.id));
            if fallback != self.builtin.endpoint {
                self.builtin.endpoint = fallback;
                self.builtin.ctx = None;
                self.applied_catalog = None;
                changed = true;
            }
        }
        let Some(id) = self.builtin.endpoint else {
            return changed;
        };
        let Some(entry) = info.get(id) else {
            return changed;
        };
        // 接口配置变了（updated_at 变）或者还没读：重读。
        let stale = self
            .builtin
            .ctx
            .as_ref()
            .is_none_or(|ctx| ctx.endpoint.updated_at != entry.updated_at);
        if stale {
            let Ok(db) = crate::host_db::open_database() else {
                return changed;
            };
            let Ok(endpoint) = db.ai_endpoint_get(id) else {
                return changed;
            };
            self.builtin.ctx = Some(Ctx {
                endpoint,
                remote: None,
                catalog: builtin::catalog(),
            });
            self.applied_catalog = None;
        }
        // 接口自报的模型清单：每个版本的接口只拉一次，拿到了再补进下拉。
        let key = builtin::remote_key(entry);
        let sender = self.host_tx.clone();
        let Some(ctx) = self.builtin.ctx.as_mut() else {
            return changed;
        };
        if ctx.remote.is_none() {
            if let Some(remote) = builtin::remote_get(&key) {
                ctx.remote = Some(remote);
                self.applied_catalog = None;
            } else if builtin::remote_claim(&key) {
                Self::spawn_remote_models(sender, id, key.clone());
            }
        }
        let apply_key = format!(
            "builtin|{key}|{}|{:?}",
            ctx.remote.is_some(),
            builtin::catalog().timestamp()
        );
        if self.applied_catalog.as_deref() == Some(apply_key.as_str()) {
            return changed;
        }
        let picker = builtin::picker_catalog(
            &ctx.endpoint,
            ctx.remote.as_deref(),
            &ctx.catalog,
            info.initial_model(id).as_deref(),
        );
        self.apply_catalog(&picker);
        self.applied_catalog = Some(apply_key);
        true
    }

    /// 后台拉 `GET /models`（接口不提供 / 连不上就是 `None`，下拉里只有手填的和默认模型）。
    fn spawn_remote_models(
        sender: std::sync::mpsc::Sender<HostEvent>,
        endpoint_id: i64,
        key: String,
    ) {
        let Ok(runtime) = runtime() else {
            builtin::remote_finish(key, None);
            return;
        };
        runtime.spawn(async move {
            let result = async {
                let (endpoint, api_key) = super::load_endpoint(endpoint_id)?;
                let transport = builtin::make_transport()?;
                let config = endpoint.provider_config(api_key);
                Ok::<_, String>(
                    zssh_core::ai::agent::builtin::probe::fetch_models(
                        transport.as_ref(),
                        &config,
                        zssh_core::ai::agent::builtin::probe::MODELS_TIMEOUT,
                    )
                    .await,
                )
            }
            .await;
            let remote = result.ok().flatten().map(RemoteModels::from_list);
            let _ = sender.send(HostEvent::BuiltinModels {
                key,
                result: remote,
            });
        });
    }

    /// 现在选中的模型名（不带强度）。
    pub(super) fn builtin_model_now(&self) -> Option<String> {
        let ctx = self.builtin.ctx.as_ref()?;
        let selected = self
            .embed
            .as_ref()
            .and_then(super::embed::Embed::model_for_send);
        match selected {
            Some(selection) => Some(builtin::model_of_selection(&selection, &ctx.known_models())),
            None => self
                .builtin
                .endpoint
                .and_then(|id| builtin::info().initial_model(id)),
        }
    }

    /// 现在选中的模型和它的能力。
    pub(super) fn builtin_caps_now(&self) -> Option<(String, ModelCaps)> {
        let model = self.builtin_model_now()?;
        let caps = self.builtin.ctx.as_ref()?.caps(&model);
        Some((model, caps))
    }

    /// 标题栏指示器用的用量：有真实的最近一次就用它；内置助手发第一条之前用模型的窗口预显示"0%"。
    pub(super) fn display_usage(&self) -> Option<ContextUsage> {
        if let Some(usage) = &self.chat.usage {
            return Some(usage.clone());
        }
        if !self.is_builtin() {
            return None;
        }
        let (model, caps) = self.builtin_caps_now()?;
        Some(builtin::preview_usage(&caps, &model))
    }

    // ---- 驱动脚本用的读口（调试版自动化；读的和界面显示的是同一份）----

    /// 智能体按钮上现在写的名字。
    #[cfg(debug_assertions)]
    pub fn dev_agent_label(&self) -> String {
        self.agent_label_now()
    }

    /// 智能体下拉里内置助手那几项的文字（带 `*` 前缀的是当前选中的）。
    #[cfg(debug_assertions)]
    pub fn dev_builtin_menu(&self) -> Vec<String> {
        self.builtin_menu_items()
            .into_iter()
            .map(|(_, label, selected)| if selected { format!("*{label}") } else { label })
            .collect()
    }

    /// 输入框上方的"不支持工具调用"提示显示着吗。
    #[cfg(debug_assertions)]
    pub fn dev_no_tools_hint(&self) -> bool {
        self.no_tools_hint().is_some()
    }

    /// 指示器现在用的窗口（token）和是不是估计的。
    #[cfg(debug_assertions)]
    pub fn dev_usage_window(&self) -> Option<(u64, bool)> {
        let usage = self.display_usage()?;
        Some((usage.window?, usage.window_estimated))
    }

    /// 标题栏余额现在写的字（没有就是 `None`）。
    #[cfg(debug_assertions)]
    pub fn dev_balance_label(&self) -> Option<String> {
        let id = self.tokenway_endpoint()?;
        crate::agent_setup::tokenway::balance_view(id).map(|view| view.label)
    }

    /// 对话里的提示卡种类（`topup` / `reconnect`），按出现顺序。
    #[cfg(debug_assertions)]
    pub fn dev_cards(&self) -> Vec<&'static str> {
        self.chat
            .messages
            .iter()
            .filter_map(|message| match message {
                ChatMessage::Assistant(message) => Some(message),
                _ => None,
            })
            .flat_map(|message| message.cards.iter())
            .map(|card| match card {
                CardKind::TopUp => "topup",
                CardKind::Reconnect => "reconnect",
            })
            .collect()
    }

    /// 点提示卡上的按钮（和界面按钮同一个动作）。
    #[cfg(debug_assertions)]
    pub fn dev_click_card(&mut self, which: &str, cx: &mut Context<Self>) -> Result<(), String> {
        match which {
            "topup" => crate::agent_setup::tokenway::open_topup(),
            "reconnect" => {
                crate::agent_setup::tokenway::request_reconnect();
                Ok(())
            }
            _ => Err("tw_card:topup|reconnect".to_owned()),
        }?;
        cx.notify();
        Ok(())
    }

    /// 输入框上方的淡色提示：当前模型不支持工具调用。
    pub(super) fn no_tools_hint(&self) -> Option<&'static str> {
        if !self.is_builtin() {
            return None;
        }
        let (_, caps) = self.builtin_caps_now()?;
        builtin::lacks_tools(&caps).then_some(t(ChatBuiltinMsg::NoToolsHint))
    }

    /// 第一次发送：后台建内置助手对话（读 Key、建传输层、加载资料库），然后发第一条消息。
    pub(super) fn start_builtin_and_send(
        &mut self,
        targets: Vec<crate::agent_host::HostTarget>,
        text: String,
        model: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let resume = self.pending_resume.take();
        self.resume_requested = resume.is_some();
        self.unsent_text = Some(text.clone());
        let Some(endpoint_id) = self.builtin.endpoint else {
            self.start_failed(super::builtin::MISSING_TEXT.to_owned(), cx);
            return;
        };
        let sender = self.host_tx.clone();
        let runtime = match runtime() {
            Ok(runtime) => runtime,
            Err(err) => {
                self.start_failed(format!("{}{err}", t(ChatBuiltinMsg::StartFailedPrefix)), cx);
                return;
            }
        };
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.start_cancel = Some(Arc::clone(&cancel));
        self.starting = true;
        runtime.handle().spawn(async move {
            let session = match start_builtin_agent(endpoint_id, resume).await {
                Ok(session) => session,
                Err(err) => {
                    let _ = sender.send(HostEvent::StartFailed(err));
                    return;
                }
            };
            session.sync_hosts(&targets);
            let sent = super::send_unless_cancelled(
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
            if let Err(rejected) = sender.send(event)
                && let HostEvent::Started { mut session, .. } = rejected.0
            {
                session.shutdown().await;
            }
        });
    }

    /// 取旁路信号：用量 → 统一的用量事件；提示 → 对话里的系统提示行；失败 → 记下详情
    /// （服务端要求充值时换成"需要充值"的文字）。先于普通事件处理：提示总在回合结束之前发出，
    /// 这样不会在 `TurnEnd` 之后冒出一条孤零零的提示消息。
    pub(super) fn drain_builtin_signals(&mut self) -> bool {
        let Some(link) = self
            .session
            .as_mut()
            .and_then(|session| session.builtin.as_mut())
        else {
            return false;
        };
        let mut signals = Vec::new();
        while let Ok(signal) = link.signals.try_recv() {
            signals.push(signal);
        }
        let changed = !signals.is_empty();
        for signal in signals {
            self.chat.apply_builtin_signal(signal);
        }
        changed
    }
}

impl ChatState {
    /// 内置助手的旁路信号。
    pub(super) fn apply_builtin_signal(&mut self, signal: BuiltinSignal) {
        match signal {
            BuiltinSignal::Usage(turn) => {
                self.apply_agent_event(AgentEvent::Usage {
                    usage: builtin::usage_from_turn(&turn),
                });
            }
            BuiltinSignal::Notice(notice) => {
                // 压缩 / 历史修复是中性提示，不支持工具 / 输出被截断算警示；都显示成助手消息里的提示行。
                self.push_notice(notice.text, builtin::notice_is_warning(notice.kind));
            }
            BuiltinSignal::Failure(failure) => self.note_failure(failure),
        }
    }

    /// 回合因为接口错误失败了。Tokenway 的两种失败换成带按钮的提示卡：服务端要求充值
    /// （`zeeterm_requires_topup`）→「去充值」；浏览器授权的令牌被停用（401）→「重新连接」。
    /// 通用的错误提示行（带着错误码原文）不再显示；错误事件还没到就先记着。
    fn note_failure(&mut self, failure: FailureInfo) {
        let card = match classify_chat_failure(&failure) {
            Some(ChatFailureAction::RequiresTopup) => Some(CardKind::TopUp),
            Some(ChatFailureAction::AuthorizationInvalid) if self.tokenway_token => {
                Some(CardKind::Reconnect)
            }
            _ => None,
        };
        self.last_failure = Some(failure);
        let Some(card) = card else {
            return;
        };
        // 通用错误行已经显示出来了（错误事件先到）：换成卡片。
        // 只看最后一条助手消息（当前这一回合的），别动更早的回合留下的错误行。
        let replaced = match self.messages.last_mut() {
            Some(ChatMessage::Assistant(message)) => {
                match message.notices.iter().rposition(|notice| {
                    notice.error
                        && (notice.text.contains(builtin::REQUIRES_TOPUP_CODE)
                            || (card == CardKind::Reconnect && notice.text.contains("HTTP 401")))
                }) {
                    Some(found) => {
                        message.notices.remove(found);
                        message.cards.push(card);
                        true
                    }
                    None => false,
                }
            }
            _ => false,
        };
        if !replaced {
            self.card_pending = Some(card);
        }
    }

    /// 内置助手的普通事件：在通用处理之前多做三件事——错误事件在"需要充值"时换文字；重试时撤回
    /// 这一次请求已经流出的半段正文；工具结果回来 = 下一次请求从这里开始。
    pub(super) fn apply_builtin_event(&mut self, event: AgentEvent) {
        match &event {
            AgentEvent::Error { .. } if self.card_pending.is_some() => {
                if let Some(card) = self.card_pending.take() {
                    self.push_card(card);
                }
                return;
            }
            AgentEvent::Retrying { .. } => self.rewind_current_request(),
            _ => {}
        }
        let tool_ended = matches!(event, AgentEvent::ToolEnd { .. });
        self.apply_agent_event(event);
        if tool_ended {
            self.mark_request_start();
        }
    }

    fn push_card(&mut self, card: CardKind) {
        if let Some(message) = self.ensure_assistant() {
            message.cards.push(card);
        }
    }

    /// 下一次请求从当前助手消息的这个位置开始流出文字（记下正文和思考的长度）。
    fn mark_request_start(&mut self) {
        self.request_mark = self.active_assistant_mut().map_or((0, 0), |message| {
            (message.text.len(), message.reasoning.len())
        });
    }

    /// 请求中途断开、正在重试：重试会把这次回复重新生成一遍，所以把这一次请求已经流出的正文和思考
    /// 清掉再接收，避免前半段出现两遍。更早的请求（工具调用之前）说的话不动。
    fn rewind_current_request(&mut self) {
        let (text_len, reasoning_len) = self.request_mark;
        if let Some(message) = self.active_assistant_mut() {
            message.text.truncate(text_len.min(message.text.len()));
            message
                .reasoning
                .truncate(reasoning_len.min(message.reasoning.len()));
        }
    }
}

impl AgentSession {
    /// 这个会话是内置助手的吗。
    pub(super) fn is_builtin(&self) -> bool {
        self.builtin.is_some()
    }
}

#[cfg(test)]
mod tests {
    // 不写 `use super::*`：`gpui_kit::*` 里有个同名的 `test` 宏，会把 `#[test]` 抢走。
    use super::super::{AssistantMessage, CardKind, ChatMessage, ChatState, Notice};
    use crate::agent_setup::builtin;
    use zssh_core::ai::agent::AgentEvent;
    use zssh_core::ai::agent::builtin::provider::ErrorKind;
    use zssh_core::ai::agent::builtin::{
        BuiltinNotice, BuiltinSignal, FailureInfo, NoticeKind, TurnUsage,
    };

    fn running() -> ChatState {
        let mut chat = ChatState::default();
        assert!(chat.start_real_turn("看看磁盘"));
        chat
    }

    fn assistant(chat: &ChatState) -> &AssistantMessage {
        match chat.messages.last() {
            Some(ChatMessage::Assistant(message)) => message,
            other => panic!("最后一条不是助手消息：{other:?}"),
        }
    }

    fn text(delta: &str) -> AgentEvent {
        AgentEvent::TextDelta {
            text: delta.to_owned(),
        }
    }

    fn retrying() -> AgentEvent {
        AgentEvent::Retrying {
            attempt: 1,
            reason: "连接中途断了".to_owned(),
            next_in_secs: Some(1),
        }
    }

    fn texts(notices: &[Notice]) -> Vec<(&str, bool)> {
        notices.iter().map(|n| (n.text.as_str(), n.error)).collect()
    }

    #[test]
    fn a_usage_signal_drives_the_meter_with_the_models_own_window() {
        let mut chat = running();
        chat.apply_builtin_signal(BuiltinSignal::Usage(TurnUsage {
            model: "gpt-5.5".into(),
            used: 40_000,
            output: 300,
            input: 39_000,
            cache_read: 700,
            cache_write: 0,
            window: 400_000,
            window_estimated: false,
        }));
        let usage = chat.usage.as_ref().expect("指示器有数据了");
        assert_eq!(usage.used, 40_000);
        assert_eq!(usage.window, Some(400_000), "分母是模型的窗口");
        assert!(!usage.window_estimated);
        assert_eq!(usage.percent().map(|p| p.round()), Some(10.0));
        // 只改指示器，不碰回合状态。
        assert!(chat.real_running);
        // 估计的窗口要标出来。
        chat.apply_builtin_signal(BuiltinSignal::Usage(TurnUsage {
            model: "my-chat-model".into(),
            used: 1_000,
            output: 10,
            input: 990,
            cache_read: 0,
            cache_write: 0,
            window: 32_000,
            window_estimated: true,
        }));
        assert!(chat.usage.as_ref().is_some_and(|u| u.window_estimated));
    }

    #[test]
    fn notices_show_up_as_system_lines_in_the_conversation() {
        let mut chat = running();
        for (kind, line) in [
            (
                NoticeKind::NoToolSupport,
                "该模型不支持工具调用，只能聊天，不能操作服务器。",
            ),
            (NoticeKind::Compacted, "已压缩较早的对话。"),
            (
                NoticeKind::OutputTruncated,
                "回复被模型的输出长度上限截断了，可以让它继续。",
            ),
            (
                NoticeKind::HistoryRepaired,
                "上次对话有工具调用没拿到结果，已按失败处理。",
            ),
        ] {
            chat.apply_builtin_signal(BuiltinSignal::Notice(BuiltinNotice {
                kind,
                text: line.to_owned(),
            }));
        }
        let shown = texts(&assistant(&chat).notices);
        assert_eq!(shown.len(), 4);
        assert_eq!(
            shown[0],
            ("该模型不支持工具调用，只能聊天，不能操作服务器。", true)
        );
        assert_eq!(shown[1], ("已压缩较早的对话。", false), "压缩是中性提示");
        assert!(shown[2].1, "输出被截断带警示");
        assert!(!shown[3].1);
    }

    #[test]
    fn a_topup_failure_replaces_the_generic_error_in_either_arrival_order() {
        let generic = "错误：账户余额不足或欠费了（HTTP 402）。（错误码：zeeterm_requires_topup）";
        let failure = || {
            BuiltinSignal::Failure(FailureInfo {
                kind: ErrorKind::Quota,
                code: Some("zeeterm_requires_topup".into()),
                status: Some(402),
            })
        };
        // 错误事件先到：已经显示出来的通用错误换成"需要充值"。
        let mut chat = running();
        chat.apply_builtin_event(AgentEvent::Error {
            message: "账户余额不足或欠费了（HTTP 402）。（错误码：zeeterm_requires_topup）".into(),
        });
        assert!(
            texts(&assistant(&chat).notices)[0]
                .0
                .contains("zeeterm_requires_topup")
        );
        chat.apply_builtin_signal(failure());
        let shown = texts(&assistant(&chat).notices);
        assert!(shown.is_empty(), "通用错误行换成了提示卡：{shown:?}");
        assert_eq!(assistant(&chat).cards, [CardKind::TopUp]);
        assert!(
            builtin::REQUIRES_TOPUP_TEXT.contains("需要充值后使用")
                && builtin::REQUIRES_TOPUP_TEXT.contains("赠送额度可在网页和其他客户端试用")
        );
        assert!(
            !builtin::REQUIRES_TOPUP_TEXT.contains("zeeterm_requires_topup"),
            "不把内部错误码摆给用户"
        );

        // 失败信号先到：记下，错误事件一到就直接显示"需要充值"。
        let mut chat = running();
        chat.apply_builtin_signal(failure());
        chat.apply_builtin_event(AgentEvent::Error {
            message: generic.to_owned(),
        });
        let shown = texts(&assistant(&chat).notices);
        assert!(shown.is_empty(), "{shown:?}");
        assert_eq!(assistant(&chat).cards, [CardKind::TopUp]);
        assert_eq!(
            chat.last_failure.as_ref().and_then(|f| f.code.as_deref()),
            Some("zeeterm_requires_topup")
        );
    }

    #[test]
    fn other_failures_keep_the_normal_error_line_and_leave_the_next_error_alone() {
        let mut chat = running();
        chat.apply_builtin_signal(BuiltinSignal::Failure(FailureInfo {
            kind: ErrorKind::Auth,
            code: Some("invalid_api_key".into()),
            status: Some(401),
        }));
        chat.apply_builtin_event(AgentEvent::Error {
            message: "API Key 不对或没有权限（HTTP 401）".into(),
        });
        let shown = texts(&assistant(&chat).notices);
        assert_eq!(shown, [("错误：API Key 不对或没有权限（HTTP 401）", true)]);
        assert!(chat.card_pending.is_none());
        assert!(
            assistant(&chat).cards.is_empty(),
            "普通 Key 的 401 照旧显示错误行，没有重新连接卡"
        );
    }

    fn auth_failure() -> BuiltinSignal {
        BuiltinSignal::Failure(FailureInfo {
            kind: ErrorKind::Auth,
            code: Some("invalid_token".into()),
            status: Some(401),
        })
    }

    #[test]
    fn a_revoked_tokenway_token_gets_a_reconnect_card_but_keeps_local_config_untouched() {
        // 失败信号先到。
        let mut chat = running();
        chat.tokenway_token = true;
        chat.apply_builtin_signal(auth_failure());
        chat.apply_builtin_event(AgentEvent::Error {
            message: "API Key 不对或没有权限（HTTP 401）".into(),
        });
        assert!(texts(&assistant(&chat).notices).is_empty());
        assert_eq!(assistant(&chat).cards, [CardKind::Reconnect]);
        assert!(builtin::AUTHORIZATION_INVALID_TEXT.contains("重新连接"));
        assert!(builtin::AUTHORIZATION_INVALID_TEXT.contains("可能在网站上改过密码或停用了授权"));

        // 错误事件先到：已经显示的通用错误行换成卡片，而且只换当前回合的。
        let mut chat = running();
        chat.tokenway_token = true;
        chat.apply_builtin_event(AgentEvent::Error {
            message: "API Key 不对或没有权限（HTTP 401）".into(),
        });
        assert_eq!(texts(&assistant(&chat).notices).len(), 1);
        chat.apply_builtin_signal(auth_failure());
        assert!(texts(&assistant(&chat).notices).is_empty());
        assert_eq!(assistant(&chat).cards, [CardKind::Reconnect]);
    }

    #[test]
    fn an_ordinary_key_that_fails_with_401_is_not_offered_reconnect() {
        let mut chat = running();
        chat.tokenway_token = false;
        chat.apply_builtin_signal(auth_failure());
        assert!(chat.card_pending.is_none());
        chat.apply_builtin_event(AgentEvent::Error {
            message: "API Key 不对或没有权限（HTTP 401）".into(),
        });
        assert_eq!(texts(&assistant(&chat).notices).len(), 1);
        assert!(assistant(&chat).cards.is_empty());
    }

    #[test]
    fn rate_limits_and_other_failures_stay_ordinary_error_lines() {
        let mut chat = running();
        chat.tokenway_token = true;
        chat.apply_builtin_signal(BuiltinSignal::Failure(FailureInfo {
            kind: ErrorKind::RateLimited,
            code: Some("rate_limited".into()),
            status: Some(429),
        }));
        chat.apply_builtin_event(AgentEvent::Error {
            message: "请求太频繁（HTTP 429）".into(),
        });
        assert_eq!(texts(&assistant(&chat).notices).len(), 1);
        assert!(assistant(&chat).cards.is_empty());
    }
    #[test]
    fn a_retry_throws_away_the_half_reply_of_that_request_only() {
        let mut chat = running();
        // 第一次请求：先说了一句，然后调工具；工具回来后下一次请求开始。
        chat.apply_builtin_event(text("我先看一下目录。"));
        chat.apply_builtin_event(AgentEvent::ToolStart {
            call_id: "c1".into(),
            tool: "list_dir".into(),
            input: serde_json::json!({"path": "/"}),
        });
        chat.apply_builtin_event(AgentEvent::ToolEnd {
            call_id: "c1".into(),
            tool: "list_dir".into(),
            output: "a b c".into(),
            error: false,
        });
        // 第二次请求说到一半断了，重试：这半段要撤回，第一句保留。
        chat.apply_builtin_event(text("目录里有"));
        chat.apply_builtin_event(AgentEvent::ReasoningDelta {
            text: "想一想".into(),
        });
        chat.apply_builtin_event(retrying());
        assert_eq!(assistant(&chat).text, "我先看一下目录。");
        assert_eq!(assistant(&chat).reasoning, "");
        // 重试后重新生成的整段接上，不重复。
        chat.apply_builtin_event(text("目录里有 a、b、c。"));
        assert_eq!(assistant(&chat).text, "我先看一下目录。目录里有 a、b、c。");
        // 重试提示仍然显示（「思考中」旁边）。
        chat.apply_builtin_event(retrying());
        assert!(chat.retry.is_some());
        assert_eq!(assistant(&chat).text, "我先看一下目录。");
    }

    #[test]
    fn a_retry_in_the_first_request_clears_everything_streamed_so_far() {
        let mut chat = running();
        chat.apply_builtin_event(text("我先看一下，"));
        chat.apply_builtin_event(retrying());
        assert_eq!(assistant(&chat).text, "");
        chat.apply_builtin_event(text("我先看一下，这次完整说完了。"));
        assert_eq!(assistant(&chat).text, "我先看一下，这次完整说完了。");
        // 新的回合从头算：上一回合的标记不能带过来。
        chat.apply_builtin_event(AgentEvent::TurnEnd);
        assert!(chat.start_real_turn("再来"));
        chat.apply_builtin_event(text("好。"));
        chat.apply_builtin_event(retrying());
        assert_eq!(assistant(&chat).text, "");
    }

    #[test]
    fn external_agents_keep_their_old_retry_behaviour() {
        // 三个外部智能体的重试提示不撤回正文（它们的重试不重放已流出的文字）：走原来的通用处理。
        let mut chat = running();
        chat.apply_agent_event(text("已经说了一半"));
        chat.apply_agent_event(retrying());
        assert_eq!(assistant(&chat).text, "已经说了一半");
    }

    #[test]
    fn a_new_conversation_forgets_the_builtin_bookkeeping() {
        let mut chat = running();
        chat.apply_builtin_signal(BuiltinSignal::Failure(FailureInfo {
            kind: ErrorKind::Quota,
            code: Some("zeeterm_requires_topup".into()),
            status: Some(402),
        }));
        assert!(chat.card_pending.is_some());
        chat.new_conversation();
        assert!(chat.card_pending.is_none());
        assert!(chat.last_failure.is_none());
        assert_eq!(chat.request_mark, (0, 0));
    }
}
