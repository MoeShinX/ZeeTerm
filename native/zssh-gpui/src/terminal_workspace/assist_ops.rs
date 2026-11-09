//! 窗格的输入跟踪接线：把 shell 标记、按键、粘贴、广播 / 锁屏等限制、屏幕读数喂给核心库里的
//! [`InputTracker`]（`zssh_core::terminal_assist`）。
//!
//! 这一层**只旁路观察，不改变任何按键行为**：按键该发什么还是发什么，Tab、Enter、方向键原样交给 shell；
//! 这里只是在发送之前把"这是什么输入"告诉状态机、发送之后把结果告诉它。
//! 状态机在拿不准时会失同步，失同步后绝不会往远端补发任何东西。
//!
//! 代次失效复用 `highlight_ops` 的做法：窗格编号 + 连接代次 + 目标身份变了，整个状态机作废
//! （见 [`PaneAssist::sync_context`]）。

use std::sync::{Arc, Mutex};
use std::time::Instant;

use zssh_core::terminal_assist::history::{self, HistoryStore};
use zssh_core::terminal_assist::integration::{HANDSHAKE_TIMEOUT, ShellKind};
use zssh_core::terminal_assist::{
    AcceptRefusal, HandoffKey, InputCell, InputTracker, IntentOrigin, KeyClass, MarkAnchor,
    MarkKind, Phase, Provenance, RequestIdentity, RestrictionKind, ScreenSource, ScreenView,
    TrackerContext, TrackerEvent, WriteOutcome, WriteSource,
};
use zssh_session::{CoreFilter, SessionPump};
use zssh_terminal::{LineCellKind, ShellMarkKind};

mod suggest;

pub(crate) use suggest::SuggestView;

/// 一次写入是怎么来的。输入跟踪据此决定"这是用户在输入行上打的字"还是"别的东西"。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteOrigin {
    /// 键盘上敲出来的可打印字符（字节就是它的 UTF-8 文本）。
    TypedText,
    /// 输入法的最终提交（字节就是它的 UTF-8 文本）。
    ImeText,
    /// 非字符按键：Enter、退格，以及一切"交给 shell 并失同步"的键。
    Key(KeyClass),
    /// 不是逐键输入：粘贴、快捷命令、撰写栏、凭据回答……
    Foreign(WriteSource),
    /// 不经输入跟踪：鼠标上报、调试钩子、演示窗格的自写输出。
    Untracked,
}

/// 把一次按键分成"字符"还是"交给 shell 的键"。`bytes` 是 `encode_key` 编出来的、真正要发的字节。
pub(crate) fn classify_key(
    key: &str,
    key_char: Option<&str>,
    ctrl: bool,
    alt: bool,
    bytes: &[u8],
) -> WriteOrigin {
    let handoff = |kind| WriteOrigin::Key(KeyClass::Handoff(kind));
    match key {
        "enter" | "\n" | "\r" => return WriteOrigin::Key(KeyClass::Enter),
        "backspace" => return WriteOrigin::Key(KeyClass::Backspace),
        "delete" => return WriteOrigin::Key(KeyClass::Delete),
        "tab" => return handoff(HandoffKey::Tab),
        "up" | "down" | "left" | "right" | "home" | "end" | "pageup" | "pagedown" => {
            return handoff(HandoffKey::Navigation);
        }
        "escape" => return handoff(HandoffKey::Escape),
        _ => {}
    }
    if ctrl {
        return if matches!(key, "r" | "R" | "s" | "S") {
            handoff(HandoffKey::HistorySearch)
        } else {
            handoff(HandoffKey::Control)
        };
    }
    if alt {
        // Alt+字符发的是 ESC + 字符（readline 的 Meta 前缀）。
        return handoff(HandoffKey::Control);
    }
    match key_char {
        // 发出去的字节必须就是这个字符本身，否则不是"手动字符"。
        Some(text)
            if !text.is_empty()
                && text.as_bytes() == bytes
                && !text.chars().any(char::is_control) =>
        {
            WriteOrigin::TypedText
        }
        _ => handoff(HandoffKey::Other),
    }
}

/// 屏幕来源适配：状态机只看轻量读数，需要时才取一行单元格。
struct PumpScreen<'a>(&'a SessionPump<CoreFilter>);

impl ScreenSource for PumpScreen<'_> {
    fn view(&self) -> ScreenView {
        let probe = self.0.screen_probe();
        ScreenView {
            rows: probe.rows,
            cols: probe.cols,
            cursor_row: probe.cursor_row,
            cursor_col: probe.cursor_col,
            alt_screen: probe.alt_screen,
            display_offset: probe.display_offset,
            content_revision: probe.content_revision,
        }
    }

    fn line(&self, row: usize) -> Option<Vec<InputCell>> {
        let cells = self.0.line_cells(row)?;
        Some(
            cells
                .into_iter()
                .map(|cell| InputCell {
                    ch: cell.ch,
                    extra: cell.zerowidth,
                    kind: match cell.kind {
                        LineCellKind::Normal => zssh_core::terminal_assist::CellKind::Normal,
                        LineCellKind::Wide => zssh_core::terminal_assist::CellKind::Wide,
                        LineCellKind::WideSpacer => {
                            zssh_core::terminal_assist::CellKind::WideSpacer
                        }
                        LineCellKind::LeadingSpacer => {
                            zssh_core::terminal_assist::CellKind::LeadingSpacer
                        }
                    },
                })
                .collect(),
        )
    }
}

fn mark_kind(kind: ShellMarkKind) -> MarkKind {
    match kind {
        ShellMarkKind::PromptStart => MarkKind::PromptStart,
        ShellMarkKind::PromptEnd => MarkKind::PromptEnd,
        ShellMarkKind::CommandStart => MarkKind::CommandStart,
        ShellMarkKind::CommandEnd { exit_code } => MarkKind::CommandEnd { exit_code },
        ShellMarkKind::CommandLine(line) => MarkKind::CommandLine(line),
        ShellMarkKind::Cwd { host, path } => MarkKind::Cwd { host, path },
    }
}

/// 临时 shell 集成在这个窗格里的状态。默认关闭，只对当前会话生效；连接断开 / 重连就回到 `Off`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IntegrationState {
    Off,
    /// 脚本已经发出去，等第一个输入起点标记（最多 [`HANDSHAKE_TIMEOUT`]）。
    Handshake {
        kind: ShellKind,
        since: Instant,
    },
    /// 收到了就绪标记。
    Active {
        kind: ShellKind,
    },
    /// 握手超时：说明失败，停止本地建议；用户确认空闲后可以重试。
    Failed {
        kind: ShellKind,
    },
}

/// 每个窗格一份：输入跟踪 + 它依赖的少量窗格级状态。
pub(crate) struct PaneAssist {
    tracker: InputTracker,
    /// 设置里的 `terminal_assist.enabled`。关着时不喂、不观察，状态机停在 Unknown。
    enabled: bool,
    /// 已经处理过的 AI 注入次数（泵里的计数追上它就说明又有 AI 命令写进了终端）。
    ai_arms_seen: u64,
    integration: IntegrationState,
    /// 进程共用的内存命令历史（按目标分域）。
    history: Arc<Mutex<HistoryStore>>,
    /// 候选的检索、展示和选择模式（见 `suggest`）。
    suggest: suggest::SuggestState,
    /// 设置里的 `terminal_assist.history_enabled`。关着时不采集。
    history_enabled: bool,
    /// 测试里看一眼取走过哪些事件（产品代码不读）。
    #[cfg(test)]
    drained: Vec<TrackerEvent>,
}

/// 进程共用的那份历史。测试里每个测试线程一份，互不污染（测试各自的窗格目标名常常相同）。
pub(crate) fn shared_history() -> Arc<Mutex<HistoryStore>> {
    #[cfg(not(test))]
    {
        history::shared()
    }
    #[cfg(test)]
    {
        thread_local! {
            static STORE: Arc<Mutex<HistoryStore>> = Arc::new(Mutex::new(HistoryStore::new()));
        }
        STORE.with(Arc::clone)
    }
}

impl PaneAssist {
    pub(crate) fn new(ctx: TrackerContext) -> Self {
        Self {
            tracker: InputTracker::new(ctx),
            enabled: true,
            ai_arms_seen: 0,
            integration: IntegrationState::Off,
            history: shared_history(),
            suggest: suggest::SuggestState::default(),
            history_enabled: true,
            #[cfg(test)]
            drained: Vec::new(),
        }
    }

    /// 窗格从状态机取走累积的事件（状态机里的队列有上限，不取就会被挤掉）。
    /// 只有 `CommandStarted` 带命令时才进历史：状态机已经替我们判过"可靠输入、单行、行尾、来源干净、
    /// 非前导空格、同一个 Prompt 的待提交快照"。
    fn drain_events(&mut self) {
        let events = self.tracker.take_events();
        let password_phase = self.tracker.restrictions().password_prompt;
        let target = self.tracker.context().target.clone();
        for event in events {
            if let TrackerEvent::CommandStarted {
                command: Some(command),
            } = &event
                && self.history_enabled
            {
                let provenance = if password_phase {
                    Provenance::PasswordPhase
                } else {
                    Provenance::Typed
                };
                history::lock(&self.history).record(&target, command, provenance);
            }
            #[cfg(test)]
            self.drained.push(event);
        }
    }

    #[cfg(test)]
    pub(crate) fn drained_events(&self) -> &[TrackerEvent] {
        &self.drained
    }

    #[cfg(test)]
    pub(crate) fn history(&self) -> &Arc<Mutex<HistoryStore>> {
        &self.history
    }

    /// 设置里的历史采集开关。关掉时停止采集，并清空本模块的全部内存历史。
    pub(crate) fn set_history_enabled(&mut self, enabled: bool) {
        if self.history_enabled && !enabled {
            history::lock(&self.history).clear_all();
        }
        self.history_enabled = enabled;
    }

    pub(crate) fn integration(&self) -> IntegrationState {
        self.integration
    }

    /// 开始临时集成的握手：集成世代换一代（旧的标记作废），之后收到第一个输入起点标记就算就绪。
    pub(crate) fn begin_integration(&mut self, kind: ShellKind, now: Instant) {
        self.tracker.on_integration_reset(now);
        self.integration = IntegrationState::Handshake { kind, since: now };
    }

    /// 停用（或放弃）临时集成：世代再换一代，之后的标记不再被当成这次集成的就绪证据。
    pub(crate) fn end_integration(&mut self, now: Instant) {
        if self.integration != IntegrationState::Off {
            self.integration = IntegrationState::Off;
            self.tracker.on_integration_reset(now);
        }
    }

    /// 现在有没有资格给出候选：开着、见过可靠的输入起点标记、临时集成没有失败。
    pub(crate) fn suggestions_available(&self) -> bool {
        self.enabled
            && self.tracker.saw_prompt_marks()
            && !matches!(self.integration, IntegrationState::Failed { .. })
    }

    /// 握手进度：收到就绪标记 -> Active，超时 -> Failed。
    fn advance_handshake(&mut self, now: Instant) {
        if let IntegrationState::Handshake { kind, since } = self.integration {
            if self.tracker.saw_prompt_marks() {
                self.integration = IntegrationState::Active { kind };
            } else if now.saturating_duration_since(since) >= HANDSHAKE_TIMEOUT {
                self.integration = IntegrationState::Failed { kind };
            }
        }
    }

    pub(crate) fn tracker(&self) -> &InputTracker {
        &self.tracker
    }

    /// 测试里直接摆弄状态机（取候选请求身份、取事件、开关限制）。
    #[cfg(test)]
    pub(crate) fn tracker_mut(&mut self) -> &mut InputTracker {
        &mut self.tracker
    }

    pub(crate) fn phase(&self) -> Phase {
        self.tracker.phase()
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool, now: Instant) {
        if self.enabled != enabled {
            self.enabled = enabled;
            // 开关切换等于换了一个世代：关掉时立刻取消一切，打开时从 Unknown 重新来。
            self.tracker.on_integration_reset(now);
            // 关掉自动建议：浮层和选择模式立即收起，在途检索作废。
            self.close_suggestions();
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// 窗格所属的连接 / 目标变了就整个重置。没变就什么也不做（不分配）。
    pub(crate) fn sync_context(
        &mut self,
        pane_id: u64,
        generation: u64,
        target: &str,
        now: Instant,
    ) {
        let current = self.tracker.context();
        if current.pane_id == pane_id
            && current.connection_generation == generation
            && current.target == target
        {
            return;
        }
        let ctx = TrackerContext {
            pane_id,
            connection_generation: generation,
            target: target.to_owned(),
            broadcast_revision: current.broadcast_revision,
        };
        self.tracker.set_context(ctx, now);
    }

    /// 标签的广播开关翻转过几次：变了只让在途的候选请求过期。
    pub(crate) fn set_broadcast_revision(&mut self, revision: u64, now: Instant) {
        if self.tracker.context().broadcast_revision != revision {
            let mut ctx = self.tracker.context().clone();
            ctx.broadcast_revision = revision;
            self.tracker.set_context(ctx, now);
        }
    }

    /// 写入时发现已锁屏：马上暂停，不等下一帧的限制同步。
    pub(crate) fn note_locked(&mut self, now: Instant) {
        self.tracker
            .set_restriction(RestrictionKind::Locked, true, now);
    }

    /// 断线 / 重连 / 连上：旧会话的状态全部作废，临时集成随会话结束。
    pub(crate) fn on_connection_reset(&mut self, now: Instant) {
        self.integration = IntegrationState::Off;
        self.tracker.on_connection_reset(now);
        self.close_suggestions();
    }

    pub(crate) fn on_geometry_changed(&mut self, now: Instant) {
        self.tracker.on_geometry_changed(now);
    }

    /// 外部限制（锁屏 / 广播 / 模态 / 鼠标上报）。
    pub(crate) fn set_restrictions(
        &mut self,
        locked: bool,
        broadcast: bool,
        modal: bool,
        mouse_report: bool,
        now: Instant,
    ) {
        for (kind, on) in [
            (RestrictionKind::Locked, locked),
            (RestrictionKind::Broadcast, broadcast),
            (RestrictionKind::Modal, modal),
            (RestrictionKind::MouseReport, mouse_report),
        ] {
            self.tracker.set_restriction(kind, on, now);
        }
    }

    pub(crate) fn on_ime_preedit(&mut self, active: bool, now: Instant) {
        if active {
            // 输入法开始预编辑：退出选择模式、收起列表，这次输入交回原输入链。
            self.close_suggestions();
        }
        if self.enabled {
            self.tracker.on_ime_preedit(active, now);
        }
    }

    /// 发送之前：登记这次写入。返回要在发送之后报告结果的本地序号。
    pub(crate) fn before_write(
        &mut self,
        origin: WriteOrigin,
        bytes: &[u8],
        now: Instant,
    ) -> Option<u64> {
        // 选择模式里任何别的写入（普通输入、输入法提交、粘贴……）都让它退出；这次写入照常往下走一次。
        self.exit_select_mode();
        if !self.enabled {
            return None;
        }
        match origin {
            WriteOrigin::TypedText | WriteOrigin::ImeText => {
                let intent = if origin == WriteOrigin::ImeText {
                    IntentOrigin::ImeCommit
                } else {
                    IntentOrigin::Manual
                };
                match std::str::from_utf8(bytes) {
                    Ok(text) => self.tracker.on_text(text, intent, now),
                    Err(_) => {
                        self.tracker
                            .on_key(KeyClass::Handoff(HandoffKey::Other), now);
                        None
                    }
                }
            }
            WriteOrigin::Key(class) => {
                self.tracker.on_key(class, now);
                None
            }
            WriteOrigin::Foreign(source) => {
                self.tracker.on_foreign_write(source, now);
                None
            }
            WriteOrigin::Untracked => None,
        }
    }

    /// 发候选请求之前取请求身份：结果回来和接受时都要用 [`InputTracker::is_current`] 重新核对。
    pub(crate) fn request_identity(&mut self) -> RequestIdentity {
        self.tracker.identity()
    }

    /// 接受候选前的登记：校验全部通过才返回本地序号，状态进入 AwaitingEcho。
    pub(crate) fn begin_accept(
        &mut self,
        suffix: &str,
        id: &RequestIdentity,
        now: Instant,
    ) -> Result<u64, AcceptRefusal> {
        if !self.enabled {
            return Err(AcceptRefusal::NotInInput);
        }
        self.tracker.begin_accept(suffix, id, now)
    }

    /// 发送之后：报告结果。
    pub(crate) fn after_write(
        &mut self,
        origin: WriteOrigin,
        seq: Option<u64>,
        outcome: WriteOutcome,
        now: Instant,
    ) {
        if !self.enabled {
            return;
        }
        match seq {
            Some(seq) => self.tracker.on_write_outcome(seq, outcome, now),
            None => {
                if matches!(origin, WriteOrigin::Key(_)) {
                    self.tracker.on_unsent_write(outcome, now);
                }
            }
        }
    }

    /// 每次处理完输出（以及定时）：先处理新到的 shell 标记，再观察屏幕。
    pub(crate) fn after_output(&mut self, pump: &mut SessionPump<CoreFilter>, now: Instant) {
        let events = pump.take_shell_events();
        if !self.enabled {
            return;
        }
        let arms = pump.ai_arm_count();
        if arms != self.ai_arms_seen {
            self.ai_arms_seen = arms;
            self.tracker.on_foreign_write(WriteSource::AiInjection, now);
        }
        let screen = PumpScreen(pump);
        for event in events {
            let anchor = MarkAnchor {
                row: event.anchor.row,
                col: event.anchor.col,
                alt_screen: event.anchor.alt_screen,
            };
            self.tracker
                .on_mark(mark_kind(event.mark.kind), anchor, &screen, now);
        }
        self.tracker.observe(&screen, now);
        self.advance_handshake(now);
        self.drain_events();
    }
}

// ---- 临时 Bash / Zsh 集成：窗格和工作区的入口 ----

/// 启用临时集成被拒绝的原因（界面按原因出词）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IntegrationError {
    /// 窗格不是已连接的会话。
    NoSession,
    /// 设置里关掉了命令建议。
    Disabled,
    Locked,
    /// 上一次启用还在握手。
    Handshaking,
    /// 正在执行命令：脚本不能发进一个运行中的程序。
    Running,
    /// 输入行里已经有没执行的文字：脚本会接在它后面一起被执行。
    LineNotEmpty,
    /// 脚本没能排进发送队列。
    WriteFailed(WriteOutcome),
}

/// 停用的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopOutcome {
    /// 窗格停在空的输入行上，已把卸载命令发了过去。
    Sent,
    /// 没有可靠的空闲提示符，没有往远端发任何东西；用户要手动运行卸载命令。
    Manual,
}

/// 命令建议入口弹窗要显示的东西。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AssistView {
    pub(crate) live: bool,
    pub(crate) enabled: bool,
    pub(crate) phase: Phase,
    /// 这个会话里见过输入起点标记。
    pub(crate) marks: bool,
    pub(crate) integration: IntegrationState,
    pub(crate) suggestions: bool,
}

impl super::Pane {
    #[cfg(test)]
    pub(super) fn assist_view_for_test(&self) -> AssistView {
        self.assist_view()
    }

    fn assist_view(&self) -> AssistView {
        AssistView {
            live: self.state == super::PaneState::Live && self.is_session(),
            enabled: self.assist.enabled(),
            phase: self.assist.phase(),
            marks: self.assist.tracker().saw_prompt_marks(),
            integration: self.assist.integration(),
            suggestions: self.assist.suggestions_available(),
        }
    }

    /// 接受一个候选：只把 `suffix`（后缀）发给 shell，不发回车，也不提前把它当成已确认。候选列表的"填入"点击
    /// 和选择模式里的 Tab / Enter 都只走这一个出口（不要用 `write_as` 去发后缀）。
    ///
    /// 顺序和 [`Self::write_as`] 一样（先登记再发送再报告结果）。接受前最后核对一遍锁屏和窗格状态，
    /// 再让状态机核对请求身份、输入起点、光标位置等全部条件（[`InputTracker::begin_accept`]）；
    /// 任何一项不满足就一个字节都不发。已经排进发送队列的字节无法撤回，所以锁屏时只是不再发新的。
    /// `Ok` 里是发送结果：被拒绝 / 传输失败都会让状态机失同步，**不会重试、不会补发退格或补偿文本**。
    pub(super) fn accept_suggestion(
        &mut self,
        suffix: &str,
        id: &zssh_core::terminal_assist::RequestIdentity,
    ) -> Result<WriteOutcome, AcceptRefusal> {
        if self.state != super::PaneState::Live {
            return Ok(WriteOutcome::Rejected(zssh_session::RejectReason::NotLive));
        }
        if crate::app_lock::is_locked() {
            self.assist.note_locked(Instant::now());
            return Ok(WriteOutcome::Rejected(zssh_session::RejectReason::Locked));
        }
        let now = Instant::now();
        let seq = self.assist.begin_accept(suffix, id, now)?;
        let outcome = self.backend.send_write(suffix.as_bytes().to_vec());
        self.assist
            .after_write(WriteOrigin::Untracked, Some(seq), outcome, now);
        self.poll_events();
        Ok(outcome)
    }

    /// 往这个窗格发临时集成脚本。调用方（界面）已经让用户确认过"窗格停在空闲提示符"。
    /// 这里再核对自己知道的事：运行中的程序、输入行里已经有字，都不发。
    pub(super) fn start_integration(&mut self, kind: ShellKind) -> Result<(), IntegrationError> {
        if self.state != super::PaneState::Live || !self.is_session() {
            return Err(IntegrationError::NoSession);
        }
        if !self.assist.enabled() {
            return Err(IntegrationError::Disabled);
        }
        if crate::app_lock::is_locked() {
            return Err(IntegrationError::Locked);
        }
        if matches!(
            self.assist.integration(),
            IntegrationState::Handshake { .. }
        ) {
            return Err(IntegrationError::Handshaking);
        }
        let state = self.assist.tracker().state();
        match state.phase {
            Phase::Running => return Err(IntegrationError::Running),
            Phase::Input | Phase::AwaitingEcho if !state.predicted_text.is_empty() => {
                return Err(IntegrationError::LineNotEmpty);
            }
            _ => {}
        }
        let now = Instant::now();
        // 先换集成世代再发：发送过程中回来的标记才算这次握手的。
        self.assist.begin_integration(kind, now);
        let line = format!(
            "{}\r",
            zssh_core::terminal_assist::integration::inject_line(kind)
        );
        let outcome = self.write_as(
            line.as_bytes(),
            WriteOrigin::Foreign(WriteSource::IntegrationScript),
        );
        if !outcome.is_queued() {
            self.assist.end_integration(now);
            return Err(IntegrationError::WriteFailed(outcome));
        }
        Ok(())
    }

    /// 停用临时集成。只有输入行是空的、确认停在输入行上才往远端发卸载命令；
    /// 否则不发（不在未知状态下往 shell 里写东西），由用户手动运行卸载命令。
    pub(super) fn stop_integration(&mut self) -> StopOutcome {
        let state = self.assist.tracker().state();
        let idle = self.state == super::PaneState::Live
            && state.phase == Phase::Input
            && state.predicted_text.is_empty();
        self.assist.end_integration(Instant::now());
        if !idle {
            return StopOutcome::Manual;
        }
        let line = format!(
            "{}\r",
            zssh_core::terminal_assist::integration::cleanup_line()
        );
        let outcome = self.write_as(
            line.as_bytes(),
            WriteOrigin::Foreign(WriteSource::IntegrationScript),
        );
        if outcome.is_queued() {
            StopOutcome::Sent
        } else {
            StopOutcome::Manual
        }
    }
}

impl super::TerminalWorkspace {
    /// 聚焦窗格的命令建议状态；没有标签时是 `None`。
    pub(crate) fn assist_view(&self) -> Option<AssistView> {
        self.pane().map(super::Pane::assist_view)
    }

    pub(crate) fn start_integration(
        &mut self,
        kind: ShellKind,
        cx: &mut gpui_kit::Context<Self>,
    ) -> Result<(), IntegrationError> {
        let result = match self.pane_mut() {
            Some(pane) => pane.start_integration(kind),
            None => Err(IntegrationError::NoSession),
        };
        cx.notify();
        result
    }

    pub(crate) fn stop_integration(&mut self, cx: &mut gpui_kit::Context<Self>) -> StopOutcome {
        let outcome = match self.pane_mut() {
            Some(pane) => pane.stop_integration(),
            None => StopOutcome::Manual,
        };
        cx.notify();
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(key: &str, key_char: Option<&str>, ctrl: bool, alt: bool) -> WriteOrigin {
        let bytes = key_char.unwrap_or("").as_bytes().to_vec();
        classify_key(key, key_char, ctrl, alt, &bytes)
    }

    #[test]
    fn printable_characters_are_typed_text_and_everything_the_shell_edits_is_handed_over() {
        assert_eq!(
            classify("a", Some("a"), false, false),
            WriteOrigin::TypedText
        );
        assert_eq!(
            classify("a", Some("A"), false, false),
            WriteOrigin::TypedText
        );
        assert_eq!(
            classify("space", Some(" "), false, false),
            WriteOrigin::TypedText
        );
        assert_eq!(
            classify("1", Some("!"), false, false),
            WriteOrigin::TypedText
        );
        assert_eq!(
            classify("enter", None, false, false),
            WriteOrigin::Key(KeyClass::Enter)
        );
        assert_eq!(
            classify("\n", Some("\n"), false, false),
            WriteOrigin::Key(KeyClass::Enter),
            "输入法单独提交的换行是 Enter，不是字符"
        );
        assert_eq!(
            classify("backspace", None, false, false),
            WriteOrigin::Key(KeyClass::Backspace)
        );
        assert_eq!(
            classify("delete", None, false, false),
            WriteOrigin::Key(KeyClass::Delete)
        );
        for key in [
            "tab", "up", "down", "left", "right", "home", "end", "pageup", "pagedown",
        ] {
            let expected = if key == "tab" {
                HandoffKey::Tab
            } else {
                HandoffKey::Navigation
            };
            assert_eq!(
                classify(key, None, false, false),
                WriteOrigin::Key(KeyClass::Handoff(expected)),
                "{key}"
            );
        }
        assert_eq!(
            classify("r", Some("r"), true, false),
            WriteOrigin::Key(KeyClass::Handoff(HandoffKey::HistorySearch))
        );
        assert_eq!(
            classify("c", Some("c"), true, false),
            WriteOrigin::Key(KeyClass::Handoff(HandoffKey::Control))
        );
        assert_eq!(
            classify("b", Some("b"), false, true),
            WriteOrigin::Key(KeyClass::Handoff(HandoffKey::Control)),
            "Alt+字符是 ESC 前缀"
        );
        assert_eq!(
            classify("escape", None, false, false),
            WriteOrigin::Key(KeyClass::Handoff(HandoffKey::Escape))
        );
        assert_eq!(
            classify("f5", None, false, false),
            WriteOrigin::Key(KeyClass::Handoff(HandoffKey::Other))
        );
    }

    #[test]
    fn a_character_whose_bytes_differ_from_its_text_is_not_manual_text() {
        assert_eq!(
            classify_key("a", Some("a"), false, false, b"\x1ba"),
            WriteOrigin::Key(KeyClass::Handoff(HandoffKey::Other))
        );
        assert_eq!(
            classify_key("x", Some("\u{7}"), false, false, b"\x07"),
            WriteOrigin::Key(KeyClass::Handoff(HandoffKey::Other))
        );
    }
}
