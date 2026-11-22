//! 用户操作：新对话、排队消息、历史面板、执行模式、确认回答和复制。
//!
//! 都是「点一下按钮」的入口，界面状态怎么变由这里决定，渲染在 `view`。

use super::*;

impl AgentChatDemo {
    /// 当前模式下生效的执行模式。
    pub(super) fn current_mode(&self) -> ExecMode {
        if self.real {
            self.exec.mode
        } else {
            self.demo_mode
        }
    }

    /// 每个节拍：别的窗口 / 设置页改了执行模式，这个面板的显示跟上（只在库版本翻动时才读库）。
    /// 离线演示的模式只在内存里，不对齐。
    pub(super) fn sync_exec_mode(&mut self) -> bool {
        if !self.real {
            return false;
        }
        let Some(db) = self.db.as_ref() else {
            return false;
        };
        self.exec
            .poll(db.data_version().ok(), || load_exec_mode(db))
    }

    /// 输入框里的字加进待发送队列（回合进行中按 Enter）。
    pub(super) fn enqueue_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().to_string();
        if self.chat.enqueue(&text).is_none() {
            return;
        }
        self.input.update(cx, |state, cx| state.clean(window, cx));
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// 队列删掉一条。
    pub(super) fn queue_remove(&mut self, id: u64, cx: &mut Context<Self>) {
        if self
            .queue_edit
            .as_ref()
            .is_some_and(|(edit_id, _)| *edit_id == id)
        {
            self.queue_edit = None;
        }
        if self.chat.queue.remove(id) {
            cx.notify();
        }
    }

    /// 开始编辑一条：弹出编辑框，内容是这条现在的文字。
    pub(super) fn queue_begin_edit(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(text) = self.chat.queue.text_of(id).map(str::to_owned) else {
            return;
        };
        if !self.chat.queue.begin_edit(id) {
            return;
        }
        let state = cx.new(|cx| TextareaState::new(window, cx).auto_grow(1, 4));
        state.update(cx, |state, cx| state.set_value(text, window, cx));
        self.queue_edit = Some((id, state));
        cx.notify();
    }

    /// 保存编辑（空白 = 删除这条）。
    pub(super) fn queue_save_edit(&mut self, cx: &mut Context<Self>) {
        if let Some((id, state)) = self.queue_edit.take() {
            let text = state.read(cx).value().to_string();
            self.chat.queue.edit(id, &text);
            cx.notify();
        }
    }

    /// 不保存地放弃编辑。
    pub(super) fn queue_cancel_edit(&mut self, cx: &mut Context<Self>) {
        self.queue_edit = None;
        self.chat.queue.cancel_edit();
        cx.notify();
    }

    /// 「打断并发送」：走现有停止链结束当前回合，旧回合收尾后立刻发队列。
    /// 会话还在后台起的时候不提供（没有回合可打断，停止只会让会话起来就被关掉）。
    pub(super) fn interrupt_and_send(&mut self, cx: &mut Context<Self>) {
        if !self.real || self.session.is_none() || !self.chat.queue_can_interrupt() {
            return;
        }
        self.stop_turn(true, cx);
        cx.notify();
    }

    /// 用户点了停止后，队列留着没发：「现在发送」。
    pub(super) fn queue_send_now(&mut self, cx: &mut Context<Self>) {
        self.chat.queue.release();
        // 正在编辑的先按已保存的内容算（放弃没存的改动），免得点了发送却什么也没发。
        if self.queue_edit.take().is_some() {
            self.chat.queue.cancel_edit();
        }
        cx.notify();
    }

    /// 回合进行中输入框的占位文字换成「Enter 加入待发送队列」，结束后换回来。返回有没有改。
    pub(super) fn sync_input_placeholder(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let queueing = self.real && self.chat.is_running();
        if queueing == self.placeholder_queueing {
            return false;
        }
        self.placeholder_queueing = queueing;
        let text = if queueing {
            t(ChatMsg::InputPlaceholderRunning)
        } else {
            t(ChatMsg::InputPlaceholder)
        };
        self.input
            .update(cx, |state, cx| state.set_placeholder(text, window, cx));
        true
    }

    /// 节拍里调用：队列该发就发。发不出去（智能体不可用）放回队首并扣住。
    pub(super) fn flush_queue(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.real {
            return false;
        }
        let Some(text) = self.chat.take_queue_to_send(Instant::now()) else {
            return false;
        };
        if self.history != HistoryPanel::Closed {
            self.close_history();
        }
        if let Some(reason) = self.blocked_reason().or_else(|| self.anchor_gate()) {
            self.chat.queue.put_back(&text);
            self.footer_error = Some(reason);
            return true;
        }
        self.send_real_with(text, false, window, cx);
        true
    }

    /// 「新对话」：关掉真实会话并清空消息；演示脚本一起取消。
    pub(super) fn new_conversation(&mut self, cx: &mut Context<Self>) {
        self.end_current_conversation(cx);
        // 新对话不接旧会话：点过「继续」但还没发送的续接 id 一起丢掉。
        self.pending_resume = None;
        self.resume_notice = None;
        // 新对话另起一条历史：下一次发送重新分配 id，不覆盖上一个对话的文件。
        self.history_id = None;
        self.history_save = None;
        self.link_notice = None;
        self.shown_messages = MESSAGES_PER_PAGE;
        self.close_history();
        self.queue_edit = None;
        self.chat.new_conversation();
        // 新对话回到未锁定：目标栏重新跟着当前聚焦窗格，`@` 的主机清空。
        self.reset_anchor();
        self.sync_ai_turn_count();
        self.sync_host_activity();
        self.footer_error = None;
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// 「Markdown 示例」：追加一条静态助手消息，只走本地渲染，不碰脚本和模型。
    pub(super) fn append_markdown_sample(&mut self, cx: &mut Context<Self>) {
        self.chat
            .messages
            .push(ChatMessage::Assistant(AssistantMessage {
                text: MARKDOWN_SAMPLE.to_owned(),
                ..AssistantMessage::default()
            }));
        // 不碰 `active`：正在跑的回合继续写它自己的那条消息。
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// 「历史」：开 / 关面板；打开时刷新列表（本地扫描，不碰网络）。
    pub(super) fn toggle_history(&mut self, cx: &mut Context<Self>) {
        if self.history == HistoryPanel::Closed {
            self.history = HistoryPanel::List;
            self.history_armed = None;
            self.refresh_history();
        } else {
            self.close_history();
        }
        cx.notify();
    }

    /// 关面板（「历史」按钮 / 关闭 / 继续之后都走这里）。
    pub(super) fn close_history(&mut self) {
        self.history = HistoryPanel::Closed;
        self.history_armed = None;
    }

    /// 重新读当前智能体的历史列表；读失败时保留 `HistoryError` 的文字给面板。
    pub(super) fn refresh_history(&mut self) {
        match load_history_rows(self.agent) {
            Ok(rows) => {
                self.history_rows = rows;
                self.history_error = None;
            }
            Err(err) => {
                self.history_rows.clear();
                self.history_error = Some(format!("{}{err}", t(ChatMsg::ReadHistoryFailedPrefix)));
            }
        }
    }

    /// 「查看」：读整条历史，只读显示在面板里。
    pub(super) fn open_history(&mut self, id: &str, cx: &mut Context<Self>) {
        self.history_armed = None;
        match load_history_record(self.agent, id) {
            Ok(record) => {
                self.history = HistoryPanel::Detail(Box::new(record));
                self.history_error = None;
                self.history_shown = MESSAGES_PER_PAGE;
                self.history_full.clear();
            }
            Err(err) => {
                self.history = HistoryPanel::List;
                self.history_error = Some(format!("{}{err}", t(ChatMsg::OpenHistoryFailedPrefix)));
            }
        }
        cx.notify();
    }

    /// 「删除」：第一下只武装确认，第二下才真删；同一时间只允许一行。
    pub(super) fn delete_history(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.history_armed.as_deref() != Some(id) {
            self.history_armed = Some(id.to_owned());
            cx.notify();
            return;
        }
        match delete_history_record(self.agent, id) {
            Ok(()) => {
                self.history_armed = None;
                self.history_error = None;
                self.refresh_history();
                if matches!(&self.history, HistoryPanel::Detail(record) if record.id.as_str() == id)
                {
                    self.history = HistoryPanel::List;
                }
            }
            Err(err) => {
                self.history_armed = None;
                self.history_error =
                    Some(format!("{}{err}", t(ChatMsg::DeleteHistoryFailedPrefix)));
            }
        }
        cx.notify();
    }

    /// 「取消」：撤掉删除确认。
    pub(super) fn disarm_history_delete(&mut self, cx: &mut Context<Self>) {
        if self.history_armed.take().is_some() {
            cx.notify();
        }
    }

    /// 「继续」：先结束当前对话，把那条历史的消息放回对话区、历史 id 换成它的，
    /// 记住智能体会话 id，下一次懒启动时接上（Claude Code 用 `--resume`，别的用各自的续接）。
    ///
    /// 旧版不清当前对话也不换 id，续接后的消息会连同屏幕上原来的对话一起写进**原来那条**
    /// 历史文件，把两个对话写串（审查 H4）。
    pub(super) fn continue_history(&mut self, id: &str, cx: &mut Context<Self>) {
        let record = match load_history_record(self.agent, id) {
            Ok(record) => record,
            Err(err) => {
                self.history = HistoryPanel::List;
                self.history_error = Some(format!("{}{err}", t(ChatMsg::OpenHistoryFailedPrefix)));
                cx.notify();
                return;
            }
        };
        // 先确认能续接（有智能体会话标识），再动当前对话。
        if let Err(err) = resume_session_id(&record) {
            self.footer_error = Some(err);
            cx.notify();
            return;
        }
        // 当前对话还在跑：先停掉并留一份（用旧的历史 id），再换成要继续的那条。
        if self.chat.is_running() {
            self.stop(cx);
        }
        self.end_current_conversation(cx);
        if let Err(err) = continue_conversation(
            &mut self.chat,
            &mut self.history_id,
            &mut self.pending_resume,
            &record,
        ) {
            self.footer_error = Some(err);
            cx.notify();
            return;
        }
        self.history_save = None;
        self.link_notice = None;
        self.shown_messages = MESSAGES_PER_PAGE;
        // 回到它当初锚定的主机（旧历史没有锚定就保持未锁定）。
        self.restore_anchor(record.anchor.as_ref());
        self.sync_ai_turn_count();
        self.sync_host_activity();
        self.real = true;
        self.close_history();
        self.footer_error = None;
        self.resume_notice = Some(
            match (self.embed.is_some(), record.anchor.is_some()) {
                (false, _) => t(ChatMsg::ResumeSelected),
                (true, true) => t(ChatMsg::ResumeAnchored),
                (true, false) => t(ChatMsg::ResumeUnanchored),
            }
            .to_owned(),
        );
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// 「真实 Claude」/「离线演示」：在两种模式之间切换。只有调试版有这个入口。
    pub(super) fn toggle_real(&mut self, cx: &mut Context<Self>) {
        if !demo_toggle_available() {
            return;
        }
        if self.real {
            // 回离线：先把真实会话关掉，别在后台留进程。
            self.end_current_conversation(cx);
            // 续接是真实模式的事：回离线之后不能再拿旧 id 起进程。
            self.pending_resume = None;
            self.resume_notice = None;
            self.chat.stop();
            self.sync_ai_turn_count();
            self.sync_host_activity();
            self.real = false;
        } else {
            self.real = true;
            self.footer_error = None;
        }
        cx.notify();
    }

    /// 点一台主机：加进目标或从目标里去掉；已有会话就马上同步给桥。
    pub(super) fn toggle_host(&mut self, id: i64, cx: &mut Context<Self>) {
        toggle_host_selection(&mut self.selected_hosts, id);
        let targets = self.selected_targets();
        if let Some(session) = self.session.as_ref() {
            session.sync_hosts(&targets);
        }
        self.sync_host_activity();
        cx.notify();
    }

    pub(super) fn cycle_model(&mut self, cx: &mut Context<Self>) {
        self.model_index = next_model_index(self.models.len(), self.model_index);
        cx.notify();
    }

    /// 当前选中的模型 id；「默认」是 `None`。
    pub(super) fn selected_model(&self) -> Option<String> {
        // 主窗口里的面板：模型和强度是两个选择，发的是它们拼出来的平铺 id。
        if let Some(embed) = &self.embed {
            return embed.model_for_send();
        }
        self.models
            .get(self.model_index)
            .and_then(|model| model.id.clone())
    }

    /// 「执行模式」下拉里选了一项。
    ///
    /// 真实模式写回设置库（工具桥每次执行前读它）；离线演示只改内存里的 `demo_mode`，
    /// 不碰设置库。切到「全部放行」要先确认：嵌入面板请主窗口弹框，独立窗口自己弹。
    pub(super) fn request_mode(&mut self, mode: ExecMode, cx: &mut Context<Self>) {
        match self.allow_all_gate.request(self.current_mode(), mode) {
            ModeSwitch::Apply => self.set_mode_explicit(mode, cx),
            ModeSwitch::Ask if self.embed.is_some() => {
                self.allow_all_gate.cancel();
                cx.emit(AgentChatEvent::ConfirmAllowAll);
            }
            ModeSwitch::Ask => cx.notify(),
        }
    }

    pub(super) fn confirm_allow_all(&mut self, cx: &mut Context<Self>) {
        if let Some(mode) = self.allow_all_gate.confirm() {
            self.set_mode_explicit(mode, cx);
        }
        cx.notify();
    }

    pub(super) fn cancel_allow_all(&mut self, cx: &mut Context<Self>) {
        self.allow_all_gate.cancel();
        cx.notify();
    }

    pub(super) fn write_mode(&self, mode: ExecMode) -> Result<(), String> {
        match &self.db {
            Some(db) => save_exec_mode(db, mode),
            None => Err(format!(
                "{}{}{}",
                t(ChatMsg::SettingsUnavailablePrefix),
                self.storage_error
                    .as_deref()
                    .unwrap_or(t(ChatMsg::SettingsOpenFailed)),
                t(ChatMsg::SettingsUnavailableModeSuffix)
            )),
        }
    }

    /// 确认/拒绝一个待确认工具；真实确认回填给宿主，演示确认推进脚本。
    pub(super) fn answer_approval(&mut self, call_id: &str, allow: bool, cx: &mut Context<Self>) {
        let changed = if let Some(id) = self.chat.real_approval(call_id) {
            let live = self
                .session
                .as_ref()
                .is_some_and(|session| session.resolve_approval(id, allow));
            self.chat.resolve_real_approval(call_id, allow, live)
        } else {
            self.chat.resolve_approval(call_id, allow)
        };
        if changed {
            self.scroll.scroll_to_bottom();
            cx.notify();
        }
    }

    /// 点了 Markdown 链接。所有链接 handler（渲染、原文、历史）都走这里：校验后只复制网址。
    pub(super) fn open_link(&mut self, raw: &str, cx: &mut Context<Self>) {
        let (notice, copy) = link_click_notice(raw);
        if let Some(url) = copy {
            crate::chat_copy::write(url, cx);
        }
        self.link_notice = Some(notice);
        cx.notify();
    }

    /// 复制一段文字（命令全文等）。
    pub(super) fn copy_text(&mut self, text: String, cx: &mut Context<Self>) {
        crate::chat_copy::write(text, cx);
    }

    /// 消息上的「复制」按钮 / 右键菜单的「复制」：写剪贴板，并让键对上的按钮闪一下对勾。
    pub(super) fn copy_message(&mut self, key: &str, text: String, cx: &mut Context<Self>) {
        crate::chat_copy::write(text, cx);
        self.copied = Some((key.to_owned(), Instant::now()));
        cx.notify();
    }

    /// 对勾到点了就收回；返回有没有变化。
    pub(super) fn expire_copied(&mut self) -> bool {
        if self
            .copied
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= COPIED_FLASH)
        {
            self.copied = None;
            return true;
        }
        false
    }

    pub(super) fn is_copied(&self, key: &str) -> bool {
        self.copied.as_ref().is_some_and(|(k, _)| k == key)
    }
}
