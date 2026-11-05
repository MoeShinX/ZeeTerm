//! 智能体会话的生命周期和状态刷新：懒启动、发消息、取宿主事件、存历史、停止和收尾。
//!
//! 会话在后台运行；界面线程只在 100ms tick 里取结果，不阻塞等待。

use super::*;

/// 后台任务 → 界面的消息（100ms tick 取走）。
pub(super) enum HostEvent {
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
    /// 内置助手：接口自报的模型清单拉回来了（`None` = 没拿到）。`key` 是 `builtin::remote_key`。
    BuiltinModels {
        key: String,
        result: Option<RemoteModels>,
    },
    /// 已有会话上发消息的结果。
    Sent(Result<(), String>),
    /// 后台存历史的结果（`id` 是那条历史）；失败是 `HistoryError` 的文字。
    HistorySaved {
        id: String,
        result: Result<(), String>,
    },
}

impl AgentChatDemo {
    /// 当前智能体现在能不能用。
    pub(super) fn availability(&self) -> Availability {
        self.status.get(self.agent).availability()
    }

    /// 不能发送的原因（能发返回 `None`）：还在检测、没找到、没登录。版本过低只提示、不拦着。
    pub(super) fn blocked_reason(&self) -> Option<String> {
        agent_setup::status::blocked_reason(self.agent, self.status.get(self.agent))
    }

    /// 每个节拍：刷新三种智能体的状态；当前智能体可用且目录没取过就在后台取（面板一打开就要
    /// 显示默认模型，不用先发消息），取到了就套用到模型下拉。返回是否有可见变化。
    pub(super) fn sync_agent_state(&mut self) -> bool {
        let mut changed = false;
        if agent_setup::version() != self.status.version {
            self.status = agent_setup::snapshot();
            changed = true;
        }
        if !self.real {
            return changed;
        }
        // 内置助手没有安装可找：模型目录来自接口配置（和接口自报的清单），见 `builtin_ui`。
        if self.is_builtin() {
            if self.status.get(AgentKind::Builtin).availability() == Availability::Ready {
                changed |= self.sync_builtin_state();
            }
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
    pub(super) fn apply_catalog(&mut self, catalog: &AgentModelCatalog) {
        if let Some(embed) = self.embed.as_mut() {
            embed.set_catalog(catalog);
        }
        self.models = model_choices(catalog);
        if self.model_index >= self.models.len() {
            self.model_index = 0;
        }
    }

    /// 独立窗口里的切换按钮：按 opencode → codex → Claude Code 轮着换到下一个可用的。
    pub(super) fn cycle_agent(&mut self, cx: &mut Context<Self>) {
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
    pub(super) fn switch_agent(&mut self, next: AgentKind, cx: &mut Context<Self>) {
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
        if next == AgentKind::Builtin {
            self.pick_default_builtin_endpoint();
        }
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
                format!(
                    "{}{}{}",
                    t(ChatMsg::SwitchedToPrefix),
                    next.label(),
                    t(ChatMsg::SwitchedToNewSuffix)
                )
            } else {
                format!("{}{}", t(ChatMsg::SwitchedToPrefix), next.label())
            },
            error: false,
        });
        self.footer_error = None;
        cx.notify();
    }

    /// 选中的目标主机（按主机库顺序），带上「当前」「固定」标记（没写 `target_id` 的调用落在「当前」上）。
    pub(super) fn selected_targets(&self) -> Vec<HostTarget> {
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
    pub(super) fn sync_ai_turn_count(&mut self) {
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
    pub(super) fn sync_host_activity(&mut self) {
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
    pub(super) fn sync_ai_terminals(&self) {
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
    pub(super) fn clear_host_activity(&mut self) {
        for host_id in std::mem::take(&mut self.activity_hosts) {
            host_activity::turn_finished(host_id);
        }
        terminals().set_ai_active(self.owner_id, HashSet::new());
    }

    /// 发送输入框里的内容；内容为空时什么也不做。回合进行中（真实模式）改成放进待发送队列：
    /// 驱动在回合里会回 Busy，所以等这一回合结束后再合并发出（见 `queue`）。
    pub(super) fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    pub(super) fn send_real(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.send_real_with(text, true, window, cx);
    }

    /// `clear_input`：发的是输入框里的字就清掉；发的是排队消息时输入框里可能正写着新的，不能动。
    pub(super) fn send_real_with(
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
                Err(err) => self.fail_turn(format!("{}{err}", t(ChatMsg::SendFailedPrefix)), cx),
            }
            return;
        }
        self.start_and_send(targets, text, model, cx);
    }

    /// 第一次发送：后台起会话（Claude Code 懒启动进程，opencode / codex 复用或拉起共用进程）、
    /// 加目标，然后发第一条消息。安装信息来自进程级的状态（和面板顶部、设置页看到的同一份）。
    pub(super) fn start_and_send(
        &mut self,
        targets: Vec<HostTarget>,
        text: String,
        model: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.is_builtin() {
            self.start_builtin_and_send(targets, text, model, cx);
            return;
        }
        let kind = self.agent;
        // 续接 id 只用在这一次启动上：启动失败也不再留着，免得下次误接。
        let resume_session_id = self.pending_resume.take();
        self.resume_requested = resume_session_id.is_some();
        // 启动失败时要把这条消息放回输入框（0.2.14 同：失败不丢字）。
        self.unsent_text = Some(text.clone());
        let Some(install) = self.status.get(kind).found().cloned() else {
            self.start_failed(
                format!(
                    "{}{}{}",
                    t(ChatMsg::AgentNotFoundPrefix),
                    kind.label(),
                    t(ChatMsg::AgentNotFoundSuffix)
                ),
                cx,
            );
            return;
        };
        let sender = self.host_tx.clone();
        let runtime = match runtime() {
            Ok(runtime) => runtime,
            Err(err) => {
                self.start_failed(
                    format!(
                        "{}{}{}{err}",
                        t(ChatMsg::StartFailedPrefix),
                        kind.label(),
                        t(ChatMsg::StartFailedMiddle)
                    ),
                    cx,
                );
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
    pub(super) fn start_failed(&mut self, message: String, cx: &mut Context<Self>) {
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
    pub(super) fn poll_host_events(&mut self, cx: &mut Context<Self>) -> bool {
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
                            self.fail_turn(format!("{}{err}", t(ChatMsg::SendFailedPrefix)), cx);
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
                                "{}{err}{}{}{}",
                                t(ChatMsg::ReadModelListFailedPrefix),
                                t(ChatMsg::ReadModelListFailedMiddle),
                                t(ChatMsg::DefaultModel),
                                t(ChatMsg::ReadModelListFailedSuffix)
                            ));
                        }
                    }
                },
                HostEvent::BuiltinModels { key, result } => {
                    // 下一个节拍的 `sync_builtin_state` 从进程级缓存里取到它，补进模型下拉。
                    builtin::remote_finish(key, result);
                }
                HostEvent::Sent(Ok(())) => {}
                HostEvent::Sent(Err(err)) => {
                    // 已经点了停止 / 新对话：迟到的发送错误不再往对话里写。
                    if self.chat.is_running() {
                        self.fail_turn(format!("{}{err}", t(ChatMsg::SendFailedPrefix)), cx);
                    }
                }
                HostEvent::HistorySaved { id, result } => {
                    // 新对话之后迟到的结果不再提示：那个对话的历史 id 已经换掉了。
                    if self.history_id.as_deref() == Some(id.as_str()) {
                        match result {
                            Ok(()) => {
                                self.history_save = Some(Notice {
                                    text: t(ChatMsg::SavedToHistory).to_owned(),
                                    error: false,
                                });
                                // 面板正开着（后台写完时用户点开了历史）：列表要跟着变。
                                if self.history == HistoryPanel::List {
                                    self.refresh_history();
                                }
                            }
                            Err(err) => {
                                self.history_save = Some(Notice {
                                    text: format!("{}{err}", t(ChatMsg::SaveHistoryFailedPrefix)),
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
    pub(super) fn drain_session(&mut self) -> bool {
        // 内置助手的用量 / 提示 / 失败信号先处理（它们总在回合结束之前发出）。
        let mut changed = self.drain_builtin_signals();
        let Some(session) = self.session.as_mut() else {
            return changed;
        };
        let builtin_session = session.is_builtin();
        // 只有还在跑的回合收到 TurnEnd 才值得存一次；停止之后迟到的 TurnEnd 不再写。
        let mut turn_ended = false;
        while let Ok(event) = session.events.try_recv() {
            if matches!(&event, AgentEvent::TurnEnd) && self.chat.real_running {
                turn_ended = true;
            }
            if builtin_session {
                self.chat.apply_builtin_event(event);
            } else {
                self.chat.apply_agent_event(event);
            }
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
            // 一轮对话结束：余额变了，下次允许时（最多每 60 秒）重新取。
            if builtin_session {
                agent_setup::tokenway::mark_balance_dirty();
            }
        }
        // TurnEnd 已经把 `real_running` 置 false；这里补一次计数更新。
        self.sync_ai_turn_count();
        self.sync_host_activity();
        changed
    }

    /// 启动 / 发送失败：结束本回合，把原因写进对话和底部。
    pub(super) fn fail_turn(&mut self, message: String, cx: &mut Context<Self>) {
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
    pub(super) fn save_history(&mut self) {
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
                    text: format!("{}{err}", t(ChatMsg::SaveHistoryFailedPrefix)),
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
                Err(err) => Err(format!("{}{err}", t(ChatMsg::BackgroundTaskFailedPrefix))),
            };
            let _ = sender.send(HostEvent::HistorySaved { id, result });
        });
    }

    /// 关掉一个会话：`shutdown` 在运行时上跑，界面不等待。
    pub(super) fn shutdown_session(&mut self, session: AgentSession, cx: &mut Context<Self>) {
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
                self.footer_error = Some(format!(
                    "{}{}{}{err}",
                    t(ChatMsg::CloseSessionFailedPrefix),
                    session_label,
                    t(ChatMsg::CloseSessionFailedMiddle)
                ));
                cx.notify();
            }
        }
    }

    /// 启动还在后台进行时，让后台任务别再发第一条消息。
    pub(super) fn cancel_starting(&mut self) {
        if let Some(cancel) = &self.start_cancel {
            cancel.store(true, Ordering::SeqCst);
        }
    }

    /// 结束当前真实会话：关掉已有的，或者让还在后台起的那个起来之后直接丢掉。
    pub(super) fn end_current_conversation(&mut self, cx: &mut Context<Self>) {
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
    pub(super) fn teardown(&mut self) {
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
    pub(super) fn stop(&mut self, cx: &mut Context<Self>) {
        self.stop_turn(false, cx);
    }

    /// 停止当前回合的完整链路。`interrupt`：「打断并发送」用的——停止之后不扣住队列，
    /// 而是等旧回合的收尾事件到了再发；普通「停止」会扣住队列。返回有没有真的停掉一个回合。
    pub(super) fn stop_turn(&mut self, interrupt: bool, cx: &mut Context<Self>) -> bool {
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
                Err(err) => {
                    self.footer_error = Some(format!("{}{err}", t(ChatMsg::StopFailedPrefix)))
                }
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

    /// 「思考中」要不要重画：显示着的时候，已等秒数变了才画。
    pub(super) fn thinking_tick(&mut self) -> bool {
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
}

/// 读主机库里的执行目标；读不到就留空，原因交给底部。
pub(super) fn load_hosts(db: Option<&Database>) -> (Vec<HostChoice>, Option<String>) {
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
        Err(err) => (
            Vec::new(),
            Some(format!(
                "{}{}",
                t(ChatMsg::ReadHostsFailedPrefix),
                crate::error_text::error_text(&err)
            )),
        ),
    }
}
