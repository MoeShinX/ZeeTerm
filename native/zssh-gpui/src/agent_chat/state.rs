//! 对话状态机 [`ChatState`]：消息、工具卡片、演示脚本位置和真实回合状态。
//!
//! 界面只调用这里的方法；这里不碰窗口、不碰宿主进程，只读写自己的状态。

use super::*;

/// 纯状态：消息、工具卡片、演示脚本位置和真实回合状态。界面只调用这里的方法。
#[derive(Debug, Default)]
pub(super) struct ChatState {
    pub(super) messages: Vec<ChatMessage>,
    /// 当前回合的助手消息下标；回合结束后清掉。
    pub(super) active: Option<usize>,
    pub(super) script: Option<TurnScript>,
    pub(super) pending: Option<PendingApproval>,
    /// 真实模式：宿主回合是否还在跑；和演示脚本无关。
    pub(super) real_running: bool,
    /// 真实模式里还没回答的确认。
    pub(super) approvals: Vec<RealApproval>,
    /// 带目标的工具调用，用来把执行层回报配到卡片上（V2-G10b，规则见 `agent_tools`）。
    pub(super) tracked: Vec<Tracked>,
    /// 这个对话收到的执行位置回报（执行编号 → 回报）。
    pub(super) reports: HashMap<String, ExecTargetReport>,
    /// 回报到了、对应的卡片还没出现：先存着，卡片出现时再认领。
    pub(super) unclaimed: Vec<ExecTargetReport>,
    /// 回报到的时候要在卡片上标主机名的那些执行编号。
    pub(super) report_multi: HashSet<String>,
    /// 智能体正在带退避地自动重试（「思考中」旁边的提示）；有新动静就清掉。
    pub(super) retry: Option<RetryNotice>,
    /// 这一轮开始的时间（「思考中」显示已经等了多久）。
    pub(super) turn_started: Option<Instant>,
    /// 回合里最后一次动静是什么（决定要不要显示「思考中」）。
    pub(super) last_item: Option<LastKind>,
    /// 回合进行中用户补充的消息（V2-U1：三种驱动在回合里都回 Busy，所以先排队）。
    pub(super) queue: queue::PendingQueue,
    /// 最近一轮的上下文用量（面板标题栏的指示器，V2-U15）；新对话清掉，切到历史对话时是它存下的那一次。
    pub(super) usage: Option<ContextUsage>,
    /// 内置助手：当前这一次请求从助手消息的哪个位置开始流出文字（正文、思考的字节长度）。
    /// 重试时据此撤回已经流出的半段，见 `builtin_ui`。
    pub(super) request_mark: (usize, usize),
    /// 内置助手：服务端要求充值 / 授权失效，但对应的错误事件还没到（到了就换成带按钮的提示卡）。
    pub(super) card_pending: Option<CardKind>,
    /// 当前对话用的是浏览器授权的 Tokenway 令牌（401 时才给"重新连接"，普通 Key 的 401 照旧显示错误）。
    pub(super) tokenway_token: bool,
    /// 内置助手：最近一次接口失败的详情。
    pub(super) last_failure: Option<FailureInfo>,
}

/// 回合里最后一次动静。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LastKind {
    User,
    Text,
    Tool,
}

impl ChatState {
    pub(super) fn is_running(&self) -> bool {
        self.script.is_some() || self.real_running
    }

    #[cfg(test)]
    pub(super) fn pending(&self) -> Option<&PendingApproval> {
        self.pending.as_ref()
    }

    /// 发一条消息并开始脚本；正在跑或输入为空时不做事。
    pub(super) fn start_turn(&mut self, prompt: &str) -> bool {
        if self.is_running() {
            return false;
        }
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return false;
        }
        self.messages.push(ChatMessage::User(prompt.to_owned()));
        self.messages
            .push(ChatMessage::Assistant(AssistantMessage::default()));
        self.active = Some(self.messages.len() - 1);
        self.script = Some(TurnScript::new());
        self.pending = None;
        self.approvals.clear();
        true
    }

    /// 推进一步脚本并落到消息上；返回是否有变化。
    pub(super) fn advance(&mut self, mode: ExecMode) -> bool {
        let Some(script) = self.script.as_mut() else {
            return false;
        };
        let Some(event) = script.tick(mode) else {
            return false;
        };
        let ended = matches!(event, TurnEvent::TurnEnd);
        self.apply(event);
        if ended {
            self.script = None;
            self.pending = None;
            self.active = None;
        }
        true
    }

    /// 把脚本事件写进当前助手消息。
    pub(super) fn apply(&mut self, event: TurnEvent) {
        match event {
            TurnEvent::ReasoningDelta(delta) => {
                if let Some(message) = self.active_assistant_mut() {
                    push_paragraph(&mut message.reasoning, delta);
                }
            }
            TurnEvent::TextDelta(delta) => {
                if let Some(message) = self.active_assistant_mut() {
                    push_paragraph(&mut message.text, delta);
                }
            }
            TurnEvent::ToolStart {
                call_id,
                tool,
                input,
                needs_approval,
            } => {
                if let Some(message) = self.active_assistant_mut() {
                    let mut card = ToolCard::new(
                        call_id.to_owned(),
                        tool.name().to_owned(),
                        ToolInput::Demo(input),
                    );
                    if needs_approval {
                        card.approval = ApprovalState::Pending;
                        card.status = ToolStatus::AwaitingApproval;
                        card.detail = Some(ApprovalDetail::new(input.approval_request()));
                    }
                    message.tools.push(card);
                }
                if needs_approval {
                    self.pending = Some(PendingApproval {
                        call_id: call_id.to_owned(),
                        tool: tool.name().to_owned(),
                        input: ToolInput::Demo(input),
                    });
                }
            }
            TurnEvent::ToolEnd { call_id, outcome } => {
                if let Some(message) = self.active_assistant_mut()
                    && let Some(card) = message
                        .tools
                        .iter_mut()
                        .find(|card| card.call_id == call_id)
                {
                    match outcome {
                        Ok(output) => {
                            card.output = Some(output.to_owned());
                            card.error = None;
                            card.status = ToolStatus::Done;
                            card.output_expanded = true;
                        }
                        Err(error) => {
                            card.output = None;
                            card.error = Some(error.to_owned());
                            card.status = ToolStatus::Failed;
                        }
                    }
                }
                if self
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.call_id == call_id)
                {
                    self.pending = None;
                }
            }
            TurnEvent::TurnEnd => {
                self.pending = None;
            }
        }
    }

    /// 确认当前等待的工具；`call_id` 对不上说明界面状态过期，直接拒绝。
    pub(super) fn resolve_approval(&mut self, call_id: &str, allow: bool) -> bool {
        let call_id = call_id.to_owned();
        let Some(pending) = self.pending.as_ref() else {
            return false;
        };
        if pending.call_id != call_id {
            return false;
        }
        let Some(script) = self.script.as_mut() else {
            return false;
        };
        if !script.resolve(allow) {
            return false;
        }
        self.pending = None;
        if let Some(message) = self.active_assistant_mut()
            && let Some(card) = message
                .tools
                .iter_mut()
                .find(|card| card.call_id == call_id)
        {
            card.approval = if allow {
                ApprovalState::Allowed
            } else {
                ApprovalState::Denied
            };
            if allow {
                card.status = ToolStatus::Running;
            } else {
                card.status = ToolStatus::Failed;
                card.error = Some(DENIED_ERROR.to_owned());
            }
        }
        true
    }

    /// 真实模式：记下用户消息并开一个回合；宿主事件稍后由 tick 取来。
    pub(super) fn start_real_turn(&mut self, prompt: &str) -> bool {
        if self.is_running() {
            return false;
        }
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return false;
        }
        self.messages.push(ChatMessage::User(prompt.to_owned()));
        self.messages
            .push(ChatMessage::Assistant(AssistantMessage::default()));
        self.active = Some(self.messages.len() - 1);
        self.script = None;
        self.pending = None;
        self.approvals.clear();
        self.real_running = true;
        self.retry = None;
        self.turn_started = Some(Instant::now());
        self.last_item = Some(LastKind::User);
        self.request_mark = (0, 0);
        self.card_pending = None;
        // 新回合开始：之前因为「停止」扣住的队列，从这一回合结束时起重新按「回合结束后发出」算。
        self.queue.release();
        true
    }

    /// 回合进行中用户补充一条消息：放进待发送队列。回合没在跑（或是离线演示脚本）时返回 `None`，
    /// 调用方直接发。
    pub(super) fn enqueue(&mut self, text: &str) -> Option<u64> {
        if !self.real_running {
            return None;
        }
        self.queue.push(text)
    }

    /// 用户点「停止」：停掉回合；队列里还有话就扣住，不自动发，留给用户决定。
    pub(super) fn stop_by_user(&mut self) -> bool {
        let had_turn = self.stop();
        if had_turn && !self.queue.is_empty() {
            self.queue.hold();
        }
        had_turn
    }

    /// 现在能不能「打断并发送」：有真实回合在跑、队列里有话。
    pub(super) fn queue_can_interrupt(&self) -> bool {
        self.real_running && !self.queue.is_empty()
    }

    /// 「打断并发送」的状态部分：停掉回合，放开扣住，并开始等旧回合的收尾事件。
    /// 没有真实回合在跑返回 false（什么也没动）。
    pub(super) fn interrupt_for_queue(&mut self, now: Instant) -> bool {
        if !self.real_running || self.queue.is_empty() {
            return false;
        }
        if !self.stop() {
            return false;
        }
        self.queue.release();
        self.queue.start_wind_down(now);
        true
    }

    /// 队列现在该不该自动发；该发就取出合并后的那一条。
    pub(super) fn take_queue_to_send(&mut self, now: Instant) -> Option<String> {
        if !self.queue.ready_to_flush(self.is_running(), now) {
            return None;
        }
        self.queue.take_merged()
    }

    /// 启动失败（会话还没起来）：撤掉刚追加的用户消息和空的助手消息，回合当作没发生过。
    ///
    /// 只撤"刚好是这一轮追加、什么都没收到"的那一对；已经有内容的助手消息不动。
    pub(super) fn discard_unstarted_turn(&mut self) {
        let blank = matches!(
            self.messages.last(),
            Some(ChatMessage::Assistant(message))
                if message.text.is_empty()
                    && message.reasoning.is_empty()
                    && message.tools.is_empty()
                    && message.notices.is_empty()
                    && message.cards.is_empty()
        );
        if blank {
            self.messages.pop();
            if matches!(self.messages.last(), Some(ChatMessage::User(_))) {
                self.messages.pop();
            }
        }
        self.finish_real_turn();
    }

    /// 把宿主事件写进对话。正文和工具只落在还在跑的回合里；提示总是可见。
    pub(super) fn apply_agent_event(&mut self, event: AgentEvent) {
        // 重试成功（又出字 / 出思考 / 跑工具）或这一轮结束 / 出错：重试提示消失。
        if !matches!(
            event,
            AgentEvent::Retrying { .. } | AgentEvent::Usage { .. }
        ) {
            self.retry = None;
        }
        match event {
            AgentEvent::TextDelta { text } => {
                if self.real_running
                    && let Some(message) = self.active_assistant_mut()
                {
                    // 宿主给的是流式片段，直接拼起来，不能像脚本那样每段换行。
                    message.text.push_str(&text);
                    self.last_item = Some(LastKind::Text);
                }
            }
            AgentEvent::ReasoningDelta { text } => {
                if self.real_running
                    && let Some(message) = self.active_assistant_mut()
                {
                    message.reasoning.push_str(&text);
                    self.last_item = Some(LastKind::Text);
                }
            }
            AgentEvent::ToolStart {
                call_id,
                tool,
                input,
            } => {
                if !self.real_running {
                    return;
                }
                let shown = redact_tool_input(&input);
                let summary = format!(
                    "{}{}",
                    t(ChatMsg::ParamsPrefix),
                    content_preview(&shown.to_string())
                );
                let tracked = agent_tools::track_call(&call_id, &tool, &input);
                if let Some(message) = self.ensure_assistant() {
                    message.tools.push(ToolCard::new(
                        call_id,
                        tool,
                        ToolInput::Real { summary, input },
                    ));
                }
                self.last_item = Some(LastKind::Tool);
                if let Some(tracked) = tracked {
                    self.tracked.push(tracked);
                    self.claim_stashed_reports();
                }
            }
            AgentEvent::ToolEnd {
                call_id,
                tool,
                output,
                error,
            } => {
                if self.card_mut(&call_id).is_none() {
                    // 没收到 ToolStart（重连 / 事件丢失）：补一张卡片，别把结果吞掉。
                    if self.real_running
                        && let Some(message) = self.ensure_assistant()
                    {
                        message.tools.push(ToolCard::new(
                            call_id.clone(),
                            tool,
                            ToolInput::Real {
                                summary: String::new(),
                                input: Value::Null,
                            },
                        ));
                    }
                }
                // 结果末尾带着后端给这次调用的执行编号：拿它按编号精确校正卡片和回报的配对，
                // 卡片上不显示这一行。
                let (output, result_exec_id) = agent_tools::split_exec_marker(&output);
                if let Some(call) = self.tracked.iter_mut().find(|call| call.call_id == call_id) {
                    call.ended = true;
                }
                if let Some(card) = self.card_mut(&call_id) {
                    if error {
                        card.output = None;
                        card.error = Some(output);
                        card.status = ToolStatus::Failed;
                    } else {
                        card.output = Some(output);
                        card.error = None;
                        card.status = ToolStatus::Done;
                        card.output_expanded = true;
                    }
                }
                if let Some(exec_id) = result_exec_id {
                    let changes = agent_tools::apply_result_exec_id(
                        &mut self.tracked,
                        &self.reports,
                        &call_id,
                        &exec_id,
                    );
                    self.apply_exec_changes(changes);
                }
            }
            AgentEvent::TurnEnd => {
                // 旧回合的收尾事件到了：「打断并发送」可以发新回合了（它之前的事件都已经处理完）。
                self.queue.finish_wind_down();
                self.finish_real_turn();
            }
            AgentEvent::Error { message } => {
                self.push_notice(format!("{}{message}", t(ChatMsg::ErrorPrefix)), true);
            }
            AgentEvent::Retrying {
                attempt,
                reason,
                next_in_secs,
            } => {
                // 0.2.14：重试提示放在「思考中」旁边，不往消息里堆一条条提示。
                self.retry = Some(RetryNotice {
                    attempt,
                    reason,
                    next_in_secs,
                });
            }
            // 只改指示器，不碰回合状态。
            AgentEvent::Usage { usage } => self.usage = Some(usage),
            AgentEvent::ResumeFailed => self.push_notice(t(ChatMsg::ResumeFailedNewSession), false),
        }
    }

    /// 执行层回报了一次工具调用的实际执行位置：配到对应的卡片上；卡片还没出现就先存着。
    /// `multi`：卡片上要标出主机名（对话里有多个目标，或者当时没有「当前」）。
    pub(super) fn apply_report(&mut self, report: ExecTargetReport, multi: bool) {
        if multi {
            self.report_multi.insert(report.exec_id.clone());
        }
        let arrived = agent_tools::report_arrived(&mut self.tracked, &self.reports, &report);
        self.reports.insert(report.exec_id.clone(), report.clone());
        match arrived {
            Some(changes) => self.apply_exec_changes(changes),
            None => self.unclaimed.push(report),
        }
    }

    /// 新卡片出现了：先到的回报里有没有它的。
    pub(super) fn claim_stashed_reports(&mut self) {
        let stashed = std::mem::take(&mut self.unclaimed);
        for report in stashed {
            match agent_tools::report_arrived(&mut self.tracked, &self.reports, &report) {
                Some(changes) => self.apply_exec_changes(changes),
                None => self.unclaimed.push(report),
            }
        }
    }

    pub(super) fn apply_exec_changes(&mut self, changes: Vec<agent_tools::ExecChange>) {
        for change in changes {
            let multi = change
                .report
                .as_ref()
                .is_some_and(|report| self.report_multi.contains(&report.exec_id));
            if let Some(card) = self.card_mut(&change.call_id) {
                card.exec = change.report.as_ref().map(ExecInfo::from);
                card.exec_multi = multi;
            }
        }
    }

    /// 这张卡片的调用是不是带目标的、已经结束、执行层却一直没回报（目标没能解析出来）。
    pub(super) fn target_unknown(&self, card: &ToolCard) -> bool {
        card.exec.is_none()
            && !card.is_approval()
            && !matches!(
                card.status,
                ToolStatus::Running | ToolStatus::AwaitingApproval
            )
            && self.tracked.iter().any(|call| call.call_id == card.call_id)
    }

    /// 「思考中」要不要显示（回合在跑，且这会儿既没有文字在出、也没有命令在跑、也没有等批准的）。
    pub(super) fn thinking_visible(&self) -> bool {
        let last = match self.last_item {
            None => LastItem::None,
            Some(LastKind::User) => LastItem::User,
            Some(LastKind::Text) => LastItem::Text,
            Some(LastKind::Tool) => {
                let any_running = match self.messages.last() {
                    Some(ChatMessage::Assistant(message)) => message
                        .tools
                        .iter()
                        .any(|card| card.status == ToolStatus::Running),
                    _ => false,
                };
                LastItem::Tool { any_running }
            }
        };
        agent_tools::show_thinking(self.real_running, last, !self.approvals.is_empty())
    }

    /// 真实模式的确认请求：挂一张「等待确认」卡片，等用户回答。
    pub(super) fn approval_prompt(&mut self, id: u64, request: &ApprovalRequest) -> bool {
        if self.approvals.iter().any(|approval| approval.id == id) {
            return false;
        }
        let call_id = format!("approval-{id}");
        let (tool, input) = approval_input(request);
        if let Some(message) = self.ensure_assistant() {
            let mut card = ToolCard::new(call_id.clone(), tool, input);
            card.approval = ApprovalState::Pending;
            card.status = ToolStatus::AwaitingApproval;
            card.detail = Some(ApprovalDetail::new(request.clone()));
            // 审批请求自带执行位置（和执行层回报是同一份解析结果）：卡片上马上能标出哪台主机、哪个终端。
            if !request.target_id.is_empty() {
                card.exec = Some(ExecInfo {
                    exec_id: request.exec_id.clone(),
                    target_id: request.target_id.clone(),
                    session_id: request.session_id.clone(),
                    label: request.host_label.clone(),
                });
                card.exec_multi = true;
            }
            message.tools.push(card);
        }
        self.approvals.push(RealApproval { id, call_id });
        self.last_item = Some(LastKind::Tool);
        true
    }

    /// 等待期间失效的确认（超时、停止、目标被取消选择）：卡片标成失败，不再给按钮。
    /// `is_live` 问宿主这个请求还有没有人在等；返回是否有卡片变了。
    pub(super) fn expire_dead_approvals(&mut self, is_live: impl Fn(u64) -> bool) -> bool {
        let dead: Vec<RealApproval> = self
            .approvals
            .iter()
            .filter(|approval| !is_live(approval.id))
            .cloned()
            .collect();
        if dead.is_empty() {
            return false;
        }
        self.approvals
            .retain(|approval| dead.iter().all(|gone| gone.id != approval.id));
        for approval in dead {
            if let Some(card) = self.card_mut(&approval.call_id) {
                card.approval = ApprovalState::Denied;
                card.status = ToolStatus::Failed;
                card.error = Some(t(ChatMsg::ApprovalGone).to_owned());
            }
        }
        true
    }

    /// 这条确认是不是真实模式弹出来的；界面据此决定回填到宿主还是演示脚本。
    pub(super) fn real_approval(&self, call_id: &str) -> Option<u64> {
        self.approvals
            .iter()
            .find(|approval| approval.call_id == call_id)
            .map(|approval| approval.id)
    }

    /// 用户回答了一条真实确认；`live` 是宿主是否还认这个请求。
    pub(super) fn resolve_real_approval(&mut self, call_id: &str, allow: bool, live: bool) -> bool {
        let Some(index) = self
            .approvals
            .iter()
            .position(|approval| approval.call_id == call_id)
        else {
            return false;
        };
        self.approvals.remove(index);
        let Some(card) = self.card_mut(call_id) else {
            return true;
        };
        if !live {
            // 请求已经超时 / 被销毁：卡片不能停在「等待确认」，也不能显示成完成。
            card.approval = ApprovalState::Denied;
            card.status = ToolStatus::Failed;
            card.error = Some(t(ChatMsg::ApprovalExpired).to_owned());
        } else if allow {
            card.approval = ApprovalState::Allowed;
            // 确认卡片的职责到此为止；命令本身的结果在驱动的工具卡片上。
            card.status = ToolStatus::Answered;
        } else {
            card.approval = ApprovalState::Denied;
            card.status = ToolStatus::Failed;
            card.error = Some(DENIED_ERROR.to_owned());
        }
        true
    }

    /// 回合结束：停掉运行状态；还没回答的确认标成未确认，不能看起来像完成。
    pub(super) fn finish_real_turn(&mut self) {
        self.real_running = false;
        self.retry = None;
        self.turn_started = None;
        // 这一回合里始终没等到卡片的回报：作废，别让它去认领下一回合里同名的调用。
        self.unclaimed.clear();
        let approvals = std::mem::take(&mut self.approvals);
        if let Some(message) = self.active_assistant_mut() {
            for card in &mut message.tools {
                if approvals
                    .iter()
                    .any(|approval| approval.call_id == card.call_id)
                {
                    card.approval = ApprovalState::Denied;
                    card.status = ToolStatus::Failed;
                    card.error = Some(t(ChatMsg::ApprovalExpired).to_owned());
                } else if matches!(
                    card.status,
                    ToolStatus::Running | ToolStatus::AwaitingApproval
                ) {
                    // 工具没等到结果回合就结束了：不能一直显示「运行中」。
                    card.status = ToolStatus::Stopped;
                    if card.error.is_none() {
                        card.error = Some(t(ChatMsg::TurnEnded).to_owned());
                    }
                }
            }
        }
        self.active = None;
        self.pending = None;
    }

    /// 停止当前回合：脚本立即作废，助手消息标记已停止，待确认按拒绝处理。
    pub(super) fn stop(&mut self) -> bool {
        if self.script.is_none() && !self.real_running {
            return false;
        }
        self.script = None;
        self.real_running = false;
        self.retry = None;
        self.turn_started = None;
        let pending = self.pending.take();
        let approvals = std::mem::take(&mut self.approvals);
        if let Some(message) = self.active_assistant_mut() {
            message.stopped = true;
            let mut denied: Vec<&str> = approvals
                .iter()
                .map(|approval| approval.call_id.as_str())
                .collect();
            if let Some(pending) = &pending {
                denied.push(pending.call_id.as_str());
            }
            for card in &mut message.tools {
                if denied.contains(&card.call_id.as_str()) {
                    card.approval = ApprovalState::Denied;
                    card.status = ToolStatus::Stopped;
                    card.error = Some(STOPPED_ERROR.to_owned());
                } else if matches!(
                    card.status,
                    ToolStatus::Running | ToolStatus::AwaitingApproval
                ) {
                    card.status = ToolStatus::Stopped;
                    if card.error.is_none() {
                        card.error = Some(STOPPED_ERROR.to_owned());
                    }
                }
            }
        }
        self.active = None;
        true
    }

    /// 新对话：清空消息，脚本、真实回合和待确认一起丢掉。
    pub(super) fn new_conversation(&mut self) {
        self.messages.clear();
        self.active = None;
        self.script = None;
        self.pending = None;
        self.real_running = false;
        self.approvals.clear();
        self.tracked.clear();
        self.reports.clear();
        self.unclaimed.clear();
        self.report_multi.clear();
        self.retry = None;
        self.turn_started = None;
        self.last_item = None;
        self.queue.clear();
        self.usage = None;
        self.request_mark = (0, 0);
        self.card_pending = None;
        self.last_failure = None;
    }

    pub(super) fn toggle_reasoning(&mut self, index: usize) -> bool {
        if let Some(ChatMessage::Assistant(message)) = self.messages.get_mut(index) {
            message.reasoning_expanded = !message.reasoning_expanded;
            true
        } else {
            false
        }
    }

    /// 「原文 / 渲染」：只切换正文的显示方式，不动正文、工具卡片和历史。
    pub(super) fn toggle_raw(&mut self, index: usize) -> bool {
        if let Some(ChatMessage::Assistant(message)) = self.messages.get_mut(index) {
            message.show_raw = !message.show_raw;
            true
        } else {
            false
        }
    }

    pub(super) fn toggle_tool_output(&mut self, message_index: usize, tool_index: usize) -> bool {
        if let Some(ChatMessage::Assistant(message)) = self.messages.get_mut(message_index)
            && let Some(card) = message.tools.get_mut(tool_index)
        {
            card.output_expanded = !card.output_expanded;
            true
        } else {
            false
        }
    }

    /// 按下标取一张卡片（界面按钮用）。
    pub(super) fn card_at(
        &mut self,
        message_index: usize,
        tool_index: usize,
    ) -> Option<&mut ToolCard> {
        match self.messages.get_mut(message_index)? {
            ChatMessage::Assistant(message) => message.tools.get_mut(tool_index),
            ChatMessage::User(_) | ChatMessage::System(_) => None,
        }
    }

    /// 确认卡片 / 命令框的「展开全文」。
    pub(super) fn toggle_command_expanded(
        &mut self,
        message_index: usize,
        tool_index: usize,
    ) -> bool {
        let Some(card) = self.card_at(message_index, tool_index) else {
            return false;
        };
        match card.detail.as_mut() {
            Some(detail) => detail.command_expanded = !detail.command_expanded,
            None => card.command_expanded = !card.command_expanded,
        }
        true
    }

    /// 文件差异的「展开 / 收起」。
    pub(super) fn toggle_diff_expanded(&mut self, message_index: usize, tool_index: usize) -> bool {
        match self
            .card_at(message_index, tool_index)
            .and_then(|card| card.detail.as_mut())
        {
            Some(detail) => {
                detail.diff_expanded = !detail.diff_expanded;
                true
            }
            None => false,
        }
    }

    /// 工具输出的「显示全部 / 收起」。
    pub(super) fn toggle_output_full(&mut self, message_index: usize, tool_index: usize) -> bool {
        match self.card_at(message_index, tool_index) {
            Some(card) => {
                card.output_full = !card.output_full;
                true
            }
            None => false,
        }
    }

    /// 一组已经执行完的工具调用的「展开 / 收起」（键 = 这一组第一张卡片的下标）。
    pub(super) fn toggle_group(&mut self, message_index: usize, first: usize) -> bool {
        if let Some(ChatMessage::Assistant(message)) = self.messages.get_mut(message_index) {
            if !message.open_groups.remove(&first) {
                message.open_groups.insert(first);
            }
            true
        } else {
            false
        }
    }

    /// 超长助手正文的「展开全文 / 收起」。
    pub(super) fn toggle_body_expanded(&mut self, index: usize) -> bool {
        if let Some(ChatMessage::Assistant(message)) = self.messages.get_mut(index) {
            message.body_expanded = !message.body_expanded;
            true
        } else {
            false
        }
    }

    /// 把历史会话的消息放回对话区（继续历史会话时用），先清空当前对话。
    ///
    /// 工具消息挂在它前面最近的一条助手消息下面（历史里助手正文和工具是平铺的）。
    /// 回合状态不恢复：这些只是回看用的内容，新一轮从下一次发送开始。
    pub(super) fn load_history(&mut self, messages: &[HistoryMessage]) {
        self.new_conversation();
        for message in messages {
            match message {
                HistoryMessage::User { text } => {
                    self.messages.push(ChatMessage::User(text.clone()));
                }
                HistoryMessage::Assistant { text } => {
                    self.messages.push(ChatMessage::Assistant(AssistantMessage {
                        text: text.clone(),
                        ..AssistantMessage::default()
                    }));
                }
                HistoryMessage::Tool {
                    tool,
                    input,
                    output,
                    status,
                } => {
                    if !matches!(self.messages.last(), Some(ChatMessage::Assistant(_))) {
                        self.messages
                            .push(ChatMessage::Assistant(AssistantMessage::default()));
                    }
                    if let Some(ChatMessage::Assistant(last)) = self.messages.last_mut() {
                        last.tools
                            .push(history_tool_card(tool, input, output, status));
                    }
                }
            }
        }
    }

    pub(super) fn active_assistant_mut(&mut self) -> Option<&mut AssistantMessage> {
        let index = self.active?;
        match self.messages.get_mut(index)? {
            ChatMessage::Assistant(message) => Some(message),
            ChatMessage::User(_) | ChatMessage::System(_) => None,
        }
    }

    /// 取当前回合的助手消息；回合已经收尾时另起一条（真实事件可能晚到）。
    pub(super) fn ensure_assistant(&mut self) -> Option<&mut AssistantMessage> {
        if self.active.is_none() {
            self.messages
                .push(ChatMessage::Assistant(AssistantMessage::default()));
            self.active = Some(self.messages.len() - 1);
        }
        self.active_assistant_mut()
    }

    /// 按 `call_id` 找工具卡片；真实事件可能落在已经收尾的消息上。
    pub(super) fn card_mut(&mut self, call_id: &str) -> Option<&mut ToolCard> {
        self.messages
            .iter_mut()
            .rev()
            .find_map(|message| match message {
                ChatMessage::Assistant(message) => message
                    .tools
                    .iter_mut()
                    .rev()
                    .find(|card| card.call_id == call_id),
                ChatMessage::User(_) | ChatMessage::System(_) => None,
            })
    }

    /// 独立的系统提示行（不挂在助手消息下面，不带「助手 / 原文」行头）。
    pub(super) fn push_system_line(&mut self, text: impl Into<String>) {
        self.messages.push(ChatMessage::System(text.into()));
    }

    pub(super) fn push_notice(&mut self, text: impl Into<String>, error: bool) {
        if let Some(message) = self.ensure_assistant() {
            message.notices.push(Notice {
                text: text.into(),
                error,
            });
        }
    }

    /// 把对话映射成历史草稿：用户消息、助手正文、工具卡片按顺序。
    ///
    /// 只存对话本身：提示（重试 / 错误）和思考是界面状态，不进历史。助手正文为空
    /// （只有工具调用）时不写一条空消息。
    pub(super) fn history_draft(
        &self,
        id: &str,
        host_label: Option<&str>,
        anchor: Option<HistoryAnchor>,
    ) -> HistoryDraft {
        let mut messages = Vec::new();
        for message in &self.messages {
            match message {
                ChatMessage::User(text) => {
                    messages.push(HistoryMessage::User { text: text.clone() })
                }
                ChatMessage::Assistant(message) => {
                    if !message.text.trim().is_empty() {
                        messages.push(HistoryMessage::Assistant {
                            text: message.text.clone(),
                        });
                    }
                    for card in &message.tools {
                        messages.push(HistoryMessage::Tool {
                            tool: card.tool.clone(),
                            input: card.input.json(),
                            output: tool_output_text(card),
                            status: tool_history_status(card.status, card.approval).to_owned(),
                        });
                    }
                }
                // 系统提示行是界面状态，不进历史。
                ChatMessage::System(_) => {}
            }
        }
        HistoryDraft {
            id: id.to_owned(),
            host_label: host_label.map(str::to_owned),
            anchor,
            usage: self.usage.clone(),
            messages,
        }
    }
}
