//! 输入状态机：方案第 7.1 / 7.2 节的实现。
//!
//! 每个窗格一个 [`InputTracker`]。它只接收事实（shell 标记、本地写入及其结果、屏幕读数、限制条件的变化），
//! 自己维护 [`InputState`]；**从不产生要发给 shell 的字节**。失同步（[`Phase::Desynced`]）之后
//! 只会等屏幕能唯一确认，或者等下一个 Prompt，不会补发退格或补偿文本。
//!
//! 状态迁移表的每一行在下面都有对应的方法和同名意图的测试（见 `tests` 模块里的 `row_*`）。
//!
//! 几个实现上的约定（方案没有逐字规定，这里固定下来）：
//! - 跟踪的是"输入起点到光标之间的文字"（光标前文本）。输入总是追加在它后面，所以回显确认只需要比
//!   前缀；光标后面的东西可能是行中编辑留下的尾巴，也可能是 zsh 的右侧提示，不当作输入，只记是否空白；
//! - 失同步后想回到 Input，需要屏幕"安静下来"：见 [`SETTLE`] 和 [`HANDOFF_QUIET_WAIT`]，避免在远端
//!   还没来得及响应 Tab 之类的按键时就把旧内容当成现在的输入行；
//! - 用户按了 Enter、命令开始事件（`133;C`）还没到的这段时间，屏幕上光标会换行，这是预期的，不算失同步。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::cells::{read_input_line, InputCell, InputLine, ReadError};
use super::echo::{EchoModel, EchoVerdict};
use super::text_index::{ByteIdx, TermCol};
use super::write::{Disposition, IntentOrigin, RejectReason, WriteIntent, WriteOutcome};

pub use super::cells::MAX_TRACKED_BYTES;

/// 初始回显等待上限：登记写入后这么久还没被屏幕确认，就失同步。不会被后续输出延长。
pub const ECHO_DEADLINE: Duration = Duration::from_secs(5);
/// 失同步之后屏幕上出现过变化时，要安静这么久才尝试重新读输入行。
pub const SETTLE: Duration = Duration::from_millis(150);
/// 失同步之后屏幕一直没变化（例如 Tab 没有补全结果）时，等这么久再尝试。
pub const HANDOFF_QUIET_WAIT: Duration = Duration::from_millis(800);
/// 同一次失同步最多尝试重新读几次（屏幕没有新变化时）。
const MAX_RECOVER_ATTEMPTS: u32 = 3;
/// 同时在途的写入意图上限。
pub const MAX_PENDING_INTENTS: usize = 64;
/// 窗格没取走的事件条数上限。
const MAX_PENDING_EVENTS: usize = 512;

/// 输入状态的阶段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// 不知道是不是在输入行上。
    Unknown,
    /// 确认停在 shell 输入行上，本地认为的输入和屏幕一致，没有未确认的写入。
    Input,
    /// 本地有写入还没被屏幕确认。
    AwaitingEcho,
    /// 命令正在执行。
    Running,
    /// 本地和屏幕可能不一致，不能据此填入任何东西。
    Desynced,
    /// 被限制条件暂停（锁屏、广播、备用屏幕、鼠标上报、密码交互、滚回历史、模态框）。
    Suspended,
}

/// 把按键交给 shell 之后输入行内容不再由本地掌握的原因类别。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandoffKey {
    /// 普通 Tab（含 Shift+Tab）：shell 自己补全。
    Tab,
    /// 方向键、Home / End / PageUp / PageDown 及其带修饰键的形式：移动光标或翻历史。
    Navigation,
    /// Ctrl+R / Ctrl+S：历史检索。
    HistorySearch,
    /// Esc：readline 的 Meta 前缀 / vi 模式切换。
    Escape,
    /// 其他 Ctrl 组合（^C ^U ^W ^A ^E ^L ^D ……）。
    Control,
    /// 功能键、Insert 以及任何认不出的编辑动作。
    Other,
}

/// 非字符按键的分类。字符输入走 [`InputTracker::on_text`]。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyClass {
    Enter,
    Backspace,
    Delete,
    Handoff(HandoffKey),
}

/// 不是用户逐键输入的写入来源。协议回复（DSR / DA 等）走专门的回复通道，根本不经过输入跟踪，
/// 所以没有对应的来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteSource {
    Paste,
    /// 快捷命令。
    QuickCommand,
    /// 撰写栏发送。
    Compose,
    /// AI 往终端里写的命令。
    AiInjection,
    /// 密码 / 口令等凭据回答。
    CredentialAnswer,
    /// 临时 shell 集成的脚本或清理命令。
    IntegrationScript,
}

/// 失同步的原因（诊断用，也让测试能断言走的是哪条路径）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesyncReason {
    Handoff(HandoffKey),
    Edit,
    EchoMismatch,
    EchoTimeout,
    WriteRejected(RejectReason),
    TransportFailed,
    UnexpectedChange,
    AnchorMoved,
    Unreadable(ReadError),
    TooLong,
    TooManyPending,
    EnterWhileUncertain,
    EnterWithoutStart,
}

/// 输入起点（Prompt 之后用户输入的第一个格子）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputAnchor {
    pub row: usize,
    pub col: TermCol,
}

/// 输入行内容的可信来源：全程是本地逐键 / 输入法 / 已确认补全产生的，还是屏幕上原有的
/// （例如 shell 历史回调、行内编辑之后重新读出来的）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputOrigin {
    Clean,
    /// 来源不能确定：不采集历史。
    Mixed,
}

/// 用户按下 Enter 时保存的待提交快照。只有收到同一个 Prompt 的命令开始事件才算真的提交。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmittedCommand {
    pub text: String,
    pub prompt_epoch: u64,
    pub input_revision: u64,
}

/// 限制条件。任何一项为真，状态机就是 [`Phase::Suspended`]。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Restrictions {
    pub locked: bool,
    pub broadcast: bool,
    pub password_prompt: bool,
    pub modal: bool,
    /// 远端程序接管了鼠标上报（全屏 TUI）。
    pub mouse_report: bool,
    /// 由屏幕读数得出。
    pub alt_screen: bool,
    /// 由屏幕读数得出：正在看滚回历史。
    pub scrollback: bool,
}

impl Restrictions {
    pub fn any(&self) -> bool {
        self.locked
            || self.broadcast
            || self.password_prompt
            || self.modal
            || self.mouse_report
            || self.alt_screen
            || self.scrollback
    }
}

/// 可由外部设置的限制。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestrictionKind {
    Locked,
    Broadcast,
    PasswordPrompt,
    Modal,
    MouseReport,
}

/// 状态机所属的上下文。`pane_id` / `connection_generation` / `target` 任何一个变了，
/// 都是另一条连接或另一个目标：状态整个作废。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackerContext {
    pub pane_id: u64,
    pub connection_generation: u64,
    /// 实际协议 / 地址 / 端口 / 用户，不是 host_id。
    pub target: String,
    pub broadcast_revision: u64,
}

/// 候选请求带着的"我是在什么状态下发出的"。结果返回和接受时用 [`InputTracker::is_current`] 核对，
/// 任何一项变了，结果就作废。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestIdentity {
    pub pane_id: u64,
    pub connection_generation: u64,
    pub target_identity: String,
    pub integration_epoch: u64,
    pub prompt_epoch: u64,
    pub input_revision: u64,
    pub request_id: u64,
    pub caret: ByteIdx,
    pub broadcast_revision: u64,
    /// 用到路径补全（P5）时才有。
    pub cwd_revision: Option<u64>,
}

/// shell 标记（和终端模型里的 `ShellMarkKind` 同构；核心库不依赖终端 crate，由窗格层转换）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkKind {
    PromptStart,
    PromptEnd,
    CommandStart,
    CommandEnd { exit_code: Option<i32> },
    CommandLine(String),
    Cwd { host: Option<String>, path: String },
}

/// 标记发生那一刻的光标位置。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkAnchor {
    pub row: usize,
    pub col: usize,
    pub alt_screen: bool,
}

/// 屏幕读数（不含单元格）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenView {
    pub rows: usize,
    pub cols: usize,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub alt_screen: bool,
    /// 滚回历史的行数；0 是在最底部。
    pub display_offset: usize,
    /// 文字内容的修订号（滚动不算）。
    pub content_revision: u64,
}

/// 屏幕来源。状态机只在需要时才取单元格，而且只取一行。
pub trait ScreenSource {
    fn view(&self) -> ScreenView;
    /// 屏幕第 `row` 行（屏幕坐标）的全部单元格，保留尾部空格。
    fn line(&self, row: usize) -> Option<Vec<InputCell>>;
}

/// 状态机产生的、窗格层可能关心的事件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrackerEvent {
    PhaseChanged {
        from: Phase,
        to: Phase,
    },
    /// 命令开始执行。`command` 只在满足历史采集条件（单行、行尾、来源干净、非前导空格、
    /// 同一个 Prompt 的待提交快照）时有值。
    CommandStarted {
        command: Option<String>,
    },
    CommandFinished {
        exit_code: Option<i32>,
    },
    CwdChanged {
        host: Option<String>,
        path: String,
    },
}

/// 接受候选被拒绝的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcceptRefusal {
    NotInInput,
    Restricted,
    Composing,
    Multiline,
    /// 光标不在行尾（光标后面还有字）。
    CursorNotAtEnd,
    /// 有写入还没被屏幕确认（含已排队但没回显的）。
    WriteInFlight,
    /// 已经按了 Enter、等命令开始。
    SubmitPending,
    /// 请求身份和现在的状态对不上。
    StaleRequest,
    /// 后缀为空、含控制字符或换行。
    BadSuffix,
    TooLong,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResetCause {
    Connection,
    Integration,
}

#[derive(Clone, Copy, Debug)]
struct ResyncWait {
    since: Instant,
    /// 失同步之后屏幕上有过变化。
    changed: bool,
    attempts: u32,
}

/// 状态机对外可读的状态（方案 7.1 的 `InputState`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputState {
    pub phase: Phase,
    /// 屏幕已经确认的光标前文本。
    pub confirmed_text: String,
    /// 已确认 + 待确认（本地写入了、还没回显）。
    pub predicted_text: String,
    pub cursor_byte: ByteIdx,
    /// 输入内容变化、写入、失同步、限制变化……任何会让候选过期的事情都会让它加一。
    pub input_revision: u64,
    pub prompt_epoch: u64,
    pub input_anchor: Option<InputAnchor>,
    /// 输入法正在预编辑。
    pub composing: bool,
    /// 输入跨了物理行（或软换行）：不再自动填入。
    pub multiline: bool,
    pub origin: InputOrigin,
    /// 按 Enter 时保存的待提交快照（满足采集条件才有）。
    pub submitted_command: Option<SubmittedCommand>,
    /// 接受键按住期间的重复事件要继续消费，直到松开（见 [`InputTracker::arm_accept_latch`]）。
    pub accept_key_latch: Option<String>,
    /// 光标之后到行尾是否全是空白。
    pub tail_blank: bool,
    /// 最近一次失同步的原因。
    pub desync_reason: Option<DesyncReason>,
}

impl InputState {
    fn new() -> Self {
        Self {
            phase: Phase::Unknown,
            confirmed_text: String::new(),
            predicted_text: String::new(),
            cursor_byte: ByteIdx(0),
            input_revision: 0,
            prompt_epoch: 0,
            input_anchor: None,
            composing: false,
            multiline: false,
            origin: InputOrigin::Clean,
            submitted_command: None,
            accept_key_latch: None,
            tail_blank: true,
            desync_reason: None,
        }
    }
}

/// 一个窗格的输入跟踪。
#[derive(Debug)]
pub struct InputTracker {
    ctx: TrackerContext,
    state: InputState,
    restrictions: Restrictions,
    echo: EchoModel,
    intents: VecDeque<WriteIntent>,
    next_seq: u64,
    next_request: u64,
    latest_request: u64,
    integration_epoch: u64,
    /// 输入起点是否仍然可信（收到 `133;B` 置真；命令开始 / 新 Prompt / 外部写入 / 重置置假）。
    anchor_valid: bool,
    /// 这个会话是否出现过可靠的输入起点标记（用于界面状态说明）。
    saw_prompt_marks: bool,
    enter_sent_at: Option<Instant>,
    /// 633;E 报告的、当前 Prompt 的命令行（用来交叉核对待提交快照）。
    reported_line: Option<String>,
    last_content_revision: Option<u64>,
    last_change: Option<Instant>,
    resync: Option<ResyncWait>,
    recheck: bool,
    cwd_revision: u64,
    cwd: Option<(Option<String>, String)>,
    events: Vec<TrackerEvent>,
}

impl InputTracker {
    pub fn new(ctx: TrackerContext) -> Self {
        Self {
            ctx,
            state: InputState::new(),
            restrictions: Restrictions::default(),
            echo: EchoModel::new(""),
            intents: VecDeque::new(),
            next_seq: 0,
            next_request: 0,
            latest_request: 0,
            integration_epoch: 0,
            anchor_valid: false,
            saw_prompt_marks: false,
            enter_sent_at: None,
            reported_line: None,
            last_content_revision: None,
            last_change: None,
            resync: None,
            recheck: false,
            cwd_revision: 0,
            cwd: None,
            events: Vec::new(),
        }
    }

    // ---- 读取 ----

    pub fn state(&self) -> &InputState {
        &self.state
    }

    pub fn phase(&self) -> Phase {
        self.state.phase
    }

    pub fn context(&self) -> &TrackerContext {
        &self.ctx
    }

    pub fn restrictions(&self) -> Restrictions {
        self.restrictions
    }

    pub fn integration_epoch(&self) -> u64 {
        self.integration_epoch
    }

    /// 这个会话里是否见过可靠的输入起点标记（`133;B` / `633;B`）。
    pub fn saw_prompt_marks(&self) -> bool {
        self.saw_prompt_marks
    }

    /// 最近一次 OSC 7 报告的目录。
    pub fn cwd(&self) -> Option<(Option<&str>, &str)> {
        self.cwd
            .as_ref()
            .map(|(host, path)| (host.as_deref(), path.as_str()))
    }

    pub fn cwd_revision(&self) -> u64 {
        self.cwd_revision
    }

    /// 还在途的写入意图（调试 / 测试用）。
    pub fn pending_intents(&self) -> impl Iterator<Item = &WriteIntent> {
        self.intents.iter()
    }

    /// 取走累积的事件。
    pub fn take_events(&mut self) -> Vec<TrackerEvent> {
        std::mem::take(&mut self.events)
    }

    /// 现在能不能接受候选（不检查请求身份）。
    pub fn can_accept(&self) -> bool {
        self.accept_refusal().is_none()
    }

    fn accept_refusal(&self) -> Option<AcceptRefusal> {
        if self.restrictions.any() {
            return Some(AcceptRefusal::Restricted);
        }
        if self.state.phase != Phase::Input {
            return Some(AcceptRefusal::NotInInput);
        }
        if self.state.composing {
            return Some(AcceptRefusal::Composing);
        }
        if self.state.multiline {
            return Some(AcceptRefusal::Multiline);
        }
        if self.enter_sent_at.is_some() {
            return Some(AcceptRefusal::SubmitPending);
        }
        if !self.intents.is_empty() || self.echo.has_pending() {
            return Some(AcceptRefusal::WriteInFlight);
        }
        if !self.state.tail_blank {
            return Some(AcceptRefusal::CursorNotAtEnd);
        }
        None
    }

    // ---- 请求身份 ----

    /// 发一个候选请求之前取身份。每次调用都是新的请求编号，旧编号随即作废。
    pub fn identity(&mut self) -> RequestIdentity {
        self.next_request += 1;
        self.latest_request = self.next_request;
        self.current_identity(self.next_request, None)
    }

    /// 用到路径补全时的身份（多带目录修订号）。
    pub fn identity_with_cwd(&mut self) -> RequestIdentity {
        self.next_request += 1;
        self.latest_request = self.next_request;
        self.current_identity(self.next_request, Some(self.cwd_revision))
    }

    fn current_identity(&self, request_id: u64, cwd_revision: Option<u64>) -> RequestIdentity {
        RequestIdentity {
            pane_id: self.ctx.pane_id,
            connection_generation: self.ctx.connection_generation,
            target_identity: self.ctx.target.clone(),
            integration_epoch: self.integration_epoch,
            prompt_epoch: self.state.prompt_epoch,
            input_revision: self.state.input_revision,
            request_id,
            caret: self.state.cursor_byte,
            broadcast_revision: self.ctx.broadcast_revision,
            cwd_revision,
        }
    }

    /// 请求身份是不是和现在的状态完全一致，而且是最新发出的那个请求。
    pub fn is_current(&self, id: &RequestIdentity) -> bool {
        id.request_id == self.latest_request
            && *id
                == self.current_identity(id.request_id, id.cwd_revision.map(|_| self.cwd_revision))
    }

    // ---- 上下文 / 重置 ----

    /// 窗格所属的连接 / 目标变了就整个重置；只有广播修订号变了只是让旧请求过期。
    pub fn set_context(&mut self, ctx: TrackerContext, now: Instant) {
        let moved = ctx.pane_id != self.ctx.pane_id
            || ctx.connection_generation != self.ctx.connection_generation
            || ctx.target != self.ctx.target;
        self.ctx = ctx;
        if moved {
            self.reset(ResetCause::Connection, now);
        }
    }

    /// 断线 / 重连 / 切目标：回到 Unknown，丢弃旧请求、写入确认和候选。
    pub fn on_connection_reset(&mut self, now: Instant) {
        self.reset(ResetCause::Connection, now);
    }

    /// 临时集成启用 / 停用 / 失败：集成的世代换了，旧集成产生的标记不再可信。
    pub fn on_integration_reset(&mut self, now: Instant) {
        self.reset(ResetCause::Integration, now);
    }

    fn reset(&mut self, cause: ResetCause, _now: Instant) {
        self.intents.clear();
        self.echo.reset("");
        self.state.confirmed_text.clear();
        self.state.predicted_text.clear();
        self.state.cursor_byte = ByteIdx(0);
        self.state.input_anchor = None;
        self.state.composing = false;
        self.state.multiline = false;
        self.state.origin = InputOrigin::Clean;
        self.state.submitted_command = None;
        self.state.accept_key_latch = None;
        self.state.tail_blank = true;
        self.state.desync_reason = None;
        self.state.prompt_epoch += 1;
        self.state.input_revision += 1;
        self.anchor_valid = false;
        self.enter_sent_at = None;
        self.reported_line = None;
        self.resync = None;
        self.recheck = false;
        self.last_content_revision = None;
        self.last_change = None;
        self.saw_prompt_marks = false;
        if cause == ResetCause::Integration {
            self.integration_epoch += 1;
        }
        // 连接换了，限制条件里"这条连接的屏幕"带来的两项（备用屏幕 / 滚回）也要重新读。
        self.restrictions.alt_screen = false;
        self.restrictions.scrollback = false;
        let to = if self.restrictions.any() {
            Phase::Suspended
        } else {
            Phase::Unknown
        };
        self.set_phase(to);
    }

    // ---- 限制条件 ----

    /// 外部限制（锁屏 / 广播 / 密码交互 / 模态框 / 鼠标上报）开关。
    pub fn set_restriction(&mut self, kind: RestrictionKind, on: bool, now: Instant) {
        let slot = match kind {
            RestrictionKind::Locked => &mut self.restrictions.locked,
            RestrictionKind::Broadcast => &mut self.restrictions.broadcast,
            RestrictionKind::PasswordPrompt => &mut self.restrictions.password_prompt,
            RestrictionKind::Modal => &mut self.restrictions.modal,
            RestrictionKind::MouseReport => &mut self.restrictions.mouse_report,
        };
        if *slot == on {
            return;
        }
        *slot = on;
        self.apply_restrictions(now);
    }

    fn apply_view_restrictions(&mut self, view: &ScreenView, now: Instant) {
        let alt = view.alt_screen;
        let scrollback = view.display_offset != 0;
        if self.restrictions.alt_screen != alt || self.restrictions.scrollback != scrollback {
            self.restrictions.alt_screen = alt;
            self.restrictions.scrollback = scrollback;
            self.apply_restrictions(now);
        }
    }

    /// 限制集合变了：有限制就暂停（取消请求、在途写入和待采集命令），全部解除回到 Unknown。
    fn apply_restrictions(&mut self, now: Instant) {
        if self.restrictions.any() {
            if self.state.phase != Phase::Suspended {
                self.intents.clear();
                self.echo.reset("");
                self.state.confirmed_text.clear();
                self.state.predicted_text.clear();
                self.state.cursor_byte = ByteIdx(0);
                self.state.submitted_command = None;
                self.state.accept_key_latch = None;
                self.enter_sent_at = None;
                self.resync = None;
                self.state.input_revision += 1;
                self.set_phase(Phase::Suspended);
            }
        } else if self.state.phase == Phase::Suspended {
            // 解除后不恢复旧的 AwaitingEcho / Input：先 Unknown，经新 Prompt 或严格屏幕协调再回来。
            self.state.input_revision += 1;
            self.last_change = Some(now);
            self.set_phase(Phase::Unknown);
        }
    }

    // ---- shell 标记 ----

    /// 收到一条 shell 标记。`src` 是标记处理完之后的屏幕。
    pub fn on_mark(
        &mut self,
        kind: MarkKind,
        anchor: MarkAnchor,
        src: &dyn ScreenSource,
        now: Instant,
    ) {
        self.apply_view_restrictions(&src.view(), now);
        match kind {
            MarkKind::PromptStart => {
                self.drop_prompt_state();
                self.state.input_revision += 1;
                if self.state.phase != Phase::Suspended {
                    self.set_phase(Phase::Unknown);
                }
            }
            MarkKind::PromptEnd => {
                self.saw_prompt_marks = true;
                self.drop_prompt_state();
                self.state.prompt_epoch += 1;
                self.state.input_revision += 1;
                if anchor.alt_screen {
                    // 备用屏幕上的提示符不是 shell 输入行。
                    if self.state.phase != Phase::Suspended {
                        self.set_phase(Phase::Unknown);
                    }
                    return;
                }
                self.state.input_anchor = Some(InputAnchor {
                    row: anchor.row,
                    col: TermCol(anchor.col),
                });
                self.anchor_valid = true;
                if self.state.phase == Phase::Suspended {
                    return;
                }
                self.set_phase(Phase::Unknown);
                self.try_recover(src, now, true);
            }
            MarkKind::CommandStart => {
                let committed = self.state.submitted_command.take().filter(|sub| {
                    sub.prompt_epoch == self.state.prompt_epoch
                        && self.state.phase == Phase::Input
                        && self
                            .reported_line
                            .as_deref()
                            .is_none_or(|reported| reported == sub.text)
                });
                self.push_event(TrackerEvent::CommandStarted {
                    command: committed.map(|sub| sub.text),
                });
                self.drop_prompt_state();
                self.state.input_revision += 1;
                if self.state.phase != Phase::Suspended {
                    self.set_phase(Phase::Running);
                }
            }
            MarkKind::CommandEnd { exit_code } => {
                self.push_event(TrackerEvent::CommandFinished { exit_code });
                self.drop_prompt_state();
                self.state.input_revision += 1;
                // 命令结束了，但还没有新 Prompt：不能假定可以输入。
                if self.state.phase != Phase::Suspended {
                    self.set_phase(Phase::Unknown);
                }
            }
            MarkKind::CommandLine(line) => {
                if line.len() <= MAX_TRACKED_BYTES {
                    self.reported_line = Some(line);
                }
            }
            MarkKind::Cwd { host, path } => {
                self.cwd_revision += 1;
                self.cwd = Some((host.clone(), path.clone()));
                self.push_event(TrackerEvent::CwdChanged { host, path });
            }
        }
    }

    /// 新 Prompt / 命令开始 / 命令结束都意味着上一个输入行的东西作废。
    fn drop_prompt_state(&mut self) {
        self.intents.clear();
        self.echo.reset("");
        self.state.confirmed_text.clear();
        self.state.predicted_text.clear();
        self.state.cursor_byte = ByteIdx(0);
        self.state.input_anchor = None;
        self.state.multiline = false;
        self.state.origin = InputOrigin::Clean;
        self.state.submitted_command = None;
        self.state.desync_reason = None;
        self.state.tail_blank = true;
        self.anchor_valid = false;
        self.enter_sent_at = None;
        self.reported_line = None;
        self.resync = None;
    }

    // ---- 本地写入 ----

    /// 输入法开始 / 结束预编辑。预编辑文字既不记录也不检索也不填入；结束后按最新屏幕重新确认。
    pub fn on_ime_preedit(&mut self, active: bool, _now: Instant) {
        if self.state.composing != active {
            self.state.composing = active;
            self.state.input_revision += 1;
            if !active {
                self.recheck = true;
            }
        }
    }

    /// 手动输入的字符或输入法的最终提交。**必须在真正发送之前调用**：回环回显可能在发送的同一个调用里
    /// 就到达，先登记了才认得出来。返回本地序号（之后用 [`Self::on_write_outcome`] 报告结果）；
    /// 没有登记（不在输入行上）返回 `None`，调用方照常发送即可。
    pub fn on_text(&mut self, text: &str, origin: IntentOrigin, now: Instant) -> Option<u64> {
        if text.is_empty() {
            return None;
        }
        if text.chars().any(char::is_control) {
            // 带控制字符的不是"手动字符"：当成交给 shell 的编辑动作。
            self.on_key(KeyClass::Handoff(HandoffKey::Control), now);
            return None;
        }
        // 提交发生了，预编辑就结束了。
        self.state.composing = false;
        match self.state.phase {
            Phase::Input | Phase::AwaitingEcho if !self.restrictions.any() => {}
            Phase::Desynced => {
                self.note_remote_activity(now);
                return None;
            }
            _ => return None,
        }
        // 在 Enter 之后、命令开始之前继续输入：快照作废，之后的回显就是正常输入了。
        self.state.submitted_command = None;
        self.enter_sent_at = None;
        if self.state.predicted_text.len() + text.len() > MAX_TRACKED_BYTES {
            self.desync(DesyncReason::TooLong, now);
            return None;
        }
        if self.intents.len() >= MAX_PENDING_INTENTS {
            self.desync(DesyncReason::TooManyPending, now);
            return None;
        }
        Some(self.register(text, origin, now))
    }

    fn register(&mut self, suffix: &str, origin: IntentOrigin, now: Instant) -> u64 {
        let expected_base = self.echo.predicted();
        let end_offset = self.echo.register(suffix);
        self.next_seq += 1;
        let seq = self.next_seq;
        self.state.input_revision += 1;
        let identity = self.current_identity(self.latest_request, None);
        self.intents.push_back(WriteIntent {
            local_seq: seq,
            origin,
            identity,
            expected_base,
            append_suffix: suffix.to_owned(),
            deadline: now + ECHO_DEADLINE,
            disposition: Disposition::Pending,
            end_offset,
        });
        self.refresh_fields();
        self.set_phase(Phase::AwaitingEcho);
        seq
    }

    /// 报告 [`Self::on_text`] / [`Self::begin_accept`] 登记的那次写入的发送结果。
    /// 被拒绝或传输失败都会失同步，**不会补发任何东西**。
    pub fn on_write_outcome(&mut self, seq: u64, outcome: WriteOutcome, now: Instant) {
        let Some(intent) = self.intents.iter_mut().find(|i| i.local_seq == seq) else {
            return;
        };
        match outcome {
            WriteOutcome::Queued => {
                intent.disposition = Disposition::Queued;
                self.finish_if_done();
            }
            WriteOutcome::Rejected(reason) => {
                intent.disposition = Disposition::Rejected(reason);
                self.desync(DesyncReason::WriteRejected(reason), now);
            }
            WriteOutcome::TransportFailed => {
                intent.disposition = Disposition::TransportFailed;
                self.desync(DesyncReason::TransportFailed, now);
            }
        }
    }

    /// Tab / 方向键 / Ctrl+R / 退格 / Enter 等非字符按键。同样要在发送之前调用；
    /// 按键本身照原样发给 shell，这里只更新判断。
    pub fn on_key(&mut self, key: KeyClass, now: Instant) {
        match key {
            KeyClass::Enter => self.on_enter(now),
            KeyClass::Backspace | KeyClass::Delete => {
                self.take_over(DesyncReason::Edit, now);
            }
            KeyClass::Handoff(kind) => {
                self.take_over(DesyncReason::Handoff(kind), now);
            }
        }
    }

    /// 输入行内容从此由 shell 掌握：Input / AwaitingEcho 失同步；已经失同步的记一次"远端会有响应"。
    fn take_over(&mut self, reason: DesyncReason, now: Instant) {
        match self.state.phase {
            Phase::Input | Phase::AwaitingEcho => self.desync(reason, now),
            Phase::Desynced => self.note_remote_activity(now),
            _ => {}
        }
    }

    fn on_enter(&mut self, now: Instant) {
        match self.state.phase {
            Phase::Input if !self.restrictions.any() => {
                self.state.submitted_command = self.capturable_snapshot();
                self.enter_sent_at = Some(now);
                self.state.input_revision += 1;
            }
            // 不确定的行：不采集，也不暗中补发任何执行键。
            Phase::AwaitingEcho => self.desync(DesyncReason::EnterWhileUncertain, now),
            Phase::Desynced => {
                self.state.submitted_command = None;
                self.note_remote_activity(now);
            }
            _ => {}
        }
    }

    /// 满足历史采集条件时的待提交快照。
    fn capturable_snapshot(&self) -> Option<SubmittedCommand> {
        let text = &self.state.confirmed_text;
        let ok = self.state.origin == InputOrigin::Clean
            && self.state.tail_blank
            && !self.state.multiline
            && !self.state.composing
            && self.intents.is_empty()
            && !self.echo.has_pending()
            && !text.trim().is_empty()
            // 本地一律不采集前导空格命令（shell 的"不进历史"习惯）。
            && !text.starts_with(char::is_whitespace)
            && !text.chars().any(char::is_control);
        ok.then(|| SubmittedCommand {
            text: text.clone(),
            prompt_epoch: self.state.prompt_epoch,
            input_revision: self.state.input_revision,
        })
    }

    /// 粘贴 / 快捷命令 / AI / 凭据回答 / 临时集成脚本：不作为逐键用户命令，关闭建议，
    /// 之后必须重新取得可靠的输入边界（新 Prompt）。正在执行命令时不改变状态。
    pub fn on_foreign_write(&mut self, _source: WriteSource, _now: Instant) {
        match self.state.phase {
            Phase::Input | Phase::AwaitingEcho | Phase::Desynced | Phase::Unknown => {
                self.drop_prompt_state();
                self.state.input_revision += 1;
                self.set_phase(Phase::Unknown);
            }
            Phase::Running | Phase::Suspended => {}
        }
    }

    /// 一次**没有登记意图**的本地写入（Enter、Tab、退格……）没有成功排队：它没有发出去（或者不知道发到了
    /// 哪），所以此前按这次按键推演的判断（待提交快照、"shell 接管了输入行"）都不能信。
    pub fn on_unsent_write(&mut self, outcome: WriteOutcome, now: Instant) {
        let reason = match outcome {
            WriteOutcome::Queued => return,
            WriteOutcome::Rejected(reason) => DesyncReason::WriteRejected(reason),
            WriteOutcome::TransportFailed => DesyncReason::TransportFailed,
        };
        if matches!(self.state.phase, Phase::Input | Phase::AwaitingEcho) {
            self.desync(reason, now);
        }
    }

    /// 屏幕几何变了（窗口尺寸、本地清屏）：输入起点的行列号不再对应原来的格子，
    /// 必须等新 Prompt 重新确立。正在执行的命令不受影响。
    pub fn on_geometry_changed(&mut self, _now: Instant) {
        if matches!(self.state.phase, Phase::Running | Phase::Suspended) {
            self.state.input_anchor = None;
            self.anchor_valid = false;
            return;
        }
        self.drop_prompt_state();
        self.state.input_revision += 1;
        self.set_phase(Phase::Unknown);
    }

    /// 登记一次"接受候选"：只发送 `suffix`（后缀），不发回车。校验全部通过才登记并进入 AwaitingEcho；
    /// 之后的发送和结果报告与 [`Self::on_text`] 相同。
    pub fn begin_accept(
        &mut self,
        suffix: &str,
        id: &RequestIdentity,
        now: Instant,
    ) -> Result<u64, AcceptRefusal> {
        if let Some(refusal) = self.accept_refusal() {
            return Err(refusal);
        }
        if !self.is_current(id) {
            return Err(AcceptRefusal::StaleRequest);
        }
        if suffix.is_empty() || suffix.chars().any(char::is_control) {
            return Err(AcceptRefusal::BadSuffix);
        }
        if self.state.predicted_text.len() + suffix.len() > MAX_TRACKED_BYTES {
            return Err(AcceptRefusal::TooLong);
        }
        Ok(self.register(suffix, IntentOrigin::Accept, now))
    }

    // ---- 接受键的重复事件 ----

    /// 键盘接受了候选：登记这个按键，按住后的重复事件继续消费，直到松开。
    pub fn arm_accept_latch(&mut self, key: &str) {
        self.state.accept_key_latch = Some(key.to_owned());
    }

    /// 这个按键是不是应当被接受键的 latch 消费掉（按住的重复事件）。
    pub fn latch_consumes(&self, key: &str) -> bool {
        self.state.accept_key_latch.as_deref() == Some(key)
    }

    /// 按键松开：只清对应的 latch。
    pub fn release_accept_latch(&mut self, key: &str) {
        if self.latch_consumes(key) {
            self.state.accept_key_latch = None;
        }
    }

    /// 失焦：不管松开事件有没有收到，latch 都清掉。
    pub fn on_focus_lost(&mut self) {
        self.state.accept_key_latch = None;
    }

    // ---- 屏幕 ----

    /// 每次处理完输出（以及定时）调用：确认回显、检测超时、失同步后的重新协调。
    /// 只有需要时才会向 `src` 要那一行的单元格。
    pub fn observe(&mut self, src: &dyn ScreenSource, now: Instant) {
        let view = src.view();
        self.apply_view_restrictions(&view, now);
        let changed = self.last_content_revision != Some(view.content_revision);
        self.last_content_revision = Some(view.content_revision);
        if changed {
            self.last_change = Some(now);
            if let Some(wait) = &mut self.resync {
                wait.changed = true;
                wait.attempts = 0;
            }
        }
        let recheck = std::mem::take(&mut self.recheck);
        match self.state.phase {
            Phase::Suspended | Phase::Running => {}
            Phase::Unknown => {
                if self.anchor_valid && self.quiet_for(now, SETTLE) {
                    self.try_recover(src, now, false);
                }
            }
            Phase::Input => {
                if let Some(sent) = self.enter_sent_at {
                    // 已按 Enter：光标换行是预期的。命令开始事件迟迟不来就失同步。
                    if now.saturating_duration_since(sent) >= ECHO_DEADLINE {
                        self.desync(DesyncReason::EnterWithoutStart, now);
                    }
                } else if changed || recheck {
                    self.verify_input(src, &view, now);
                }
            }
            Phase::AwaitingEcho => {
                if self.intents.iter().any(|intent| {
                    intent.end_offset > self.echo.consumed() && intent.deadline <= now
                }) {
                    self.desync(DesyncReason::EchoTimeout, now);
                } else if changed || recheck {
                    self.confirm_echo(src, &view, now);
                }
            }
            Phase::Desynced => {
                if self.resync_due(now) {
                    self.try_recover(src, now, false);
                }
            }
        }
    }

    fn quiet_for(&self, now: Instant, span: Duration) -> bool {
        self.last_change
            .is_none_or(|at| now.saturating_duration_since(at) >= span)
    }

    fn resync_due(&self, now: Instant) -> bool {
        let Some(wait) = &self.resync else {
            return false;
        };
        if wait.attempts >= MAX_RECOVER_ATTEMPTS && !wait.changed {
            return false;
        }
        if wait.changed {
            self.quiet_for(now, SETTLE)
        } else {
            now.saturating_duration_since(wait.since) >= HANDOFF_QUIET_WAIT
        }
    }

    /// 失同步期间又有写入 / 按键发给了 shell，远端还会有响应：重新计时。
    fn note_remote_activity(&mut self, now: Instant) {
        self.resync = Some(ResyncWait {
            since: now,
            changed: false,
            attempts: 0,
        });
    }

    fn read_line(
        &mut self,
        src: &dyn ScreenSource,
        view: &ScreenView,
    ) -> Result<InputLine, DesyncReason> {
        let Some(anchor) = self.state.input_anchor else {
            return Err(DesyncReason::AnchorMoved);
        };
        if view.cursor_row != anchor.row {
            // 光标不在输入起点那一行：输入换行了，或者屏幕滚了。
            self.state.multiline = true;
            return Err(DesyncReason::AnchorMoved);
        }
        let Some(cells) = src.line(anchor.row) else {
            return Err(DesyncReason::Unreadable(ReadError::OutOfRange));
        };
        read_input_line(&cells, anchor.col, TermCol(view.cursor_col)).map_err(|err| {
            if matches!(err, ReadError::Wrapped | ReadError::PendingWrap) {
                self.state.multiline = true;
            }
            DesyncReason::Unreadable(err)
        })
    }

    /// Input 阶段：屏幕变了，输入行还得是我们确认过的那样。
    fn verify_input(&mut self, src: &dyn ScreenSource, view: &ScreenView, now: Instant) {
        match self.read_line(src, view) {
            Ok(line) if line.before_cursor == self.state.confirmed_text => {
                self.state.tail_blank = line.tail_blank;
            }
            Ok(_) => self.desync(DesyncReason::UnexpectedChange, now),
            Err(reason) => self.desync(reason, now),
        }
    }

    /// AwaitingEcho 阶段：拿屏幕上的光标前文本去对期望前缀。
    fn confirm_echo(&mut self, src: &dyn ScreenSource, view: &ScreenView, now: Instant) {
        let line = match self.read_line(src, view) {
            Ok(line) => line,
            Err(reason) => {
                self.desync(reason, now);
                return;
            }
        };
        match self.echo.observe(&line.before_cursor) {
            EchoVerdict::Mismatch => self.desync(DesyncReason::EchoMismatch, now),
            EchoVerdict::NoProgress => {
                self.state.tail_blank = line.tail_blank;
            }
            EchoVerdict::Advanced | EchoVerdict::Complete => {
                self.state.tail_blank = line.tail_blank;
                self.state.input_revision += 1;
                self.refresh_fields();
                self.finish_if_done();
            }
        }
    }

    /// 全部确认且没有在途写入才回到 Input。已确认的意图丢掉，还没拿到发送结果的留着。
    fn finish_if_done(&mut self) {
        let consumed = self.echo.consumed();
        self.intents.retain(|intent| {
            intent.end_offset > consumed || intent.disposition == Disposition::Pending
        });
        if self.state.phase == Phase::AwaitingEcho
            && !self.echo.has_pending()
            && self.intents.is_empty()
        {
            self.refresh_fields();
            self.set_phase(Phase::Input);
        }
    }

    /// 从屏幕重新读出输入行并回到 Input。`at_prompt` 为真表示刚收到 `133;B`（输入起点刚确立）。
    fn try_recover(&mut self, src: &dyn ScreenSource, now: Instant, at_prompt: bool) {
        if self.restrictions.any()
            || self.state.composing
            || !self.anchor_valid
            || self.enter_sent_at.is_some()
        {
            return;
        }
        let view = src.view();
        match self.read_line(src, &view) {
            Ok(line) => {
                self.intents.clear();
                self.echo.reset(&line.before_cursor);
                self.state.origin = if line.before_cursor.is_empty() && at_prompt {
                    InputOrigin::Clean
                } else {
                    InputOrigin::Mixed
                };
                self.state.tail_blank = line.tail_blank;
                self.state.multiline = false;
                self.state.desync_reason = None;
                self.resync = None;
                self.state.input_revision += 1;
                self.refresh_fields();
                self.set_phase(Phase::Input);
            }
            Err(reason) => {
                if self.state.phase == Phase::Desynced {
                    self.state.desync_reason = Some(reason);
                    if let Some(wait) = &mut self.resync {
                        wait.since = now;
                        wait.changed = false;
                        wait.attempts += 1;
                    }
                }
            }
        }
    }

    // ---- 内部 ----

    fn desync(&mut self, reason: DesyncReason, now: Instant) {
        if self.state.phase == Phase::Suspended {
            return;
        }
        self.intents.clear();
        self.echo.reset(&self.state.confirmed_text.clone());
        self.state.submitted_command = None;
        self.state.accept_key_latch = None;
        self.enter_sent_at = None;
        self.state.desync_reason = Some(reason);
        self.state.input_revision += 1;
        // 因屏幕上已经出现的变化而失同步的，只需等安静；因按键 / 写入失败的，要等远端响应。
        let observed = matches!(
            reason,
            DesyncReason::EchoMismatch | DesyncReason::UnexpectedChange
        );
        self.resync = Some(ResyncWait {
            since: now,
            changed: observed,
            attempts: 0,
        });
        self.refresh_fields();
        self.set_phase(Phase::Desynced);
    }

    fn refresh_fields(&mut self) {
        self.state.confirmed_text.clear();
        self.state.confirmed_text.push_str(self.echo.confirmed());
        self.state.predicted_text = self.echo.predicted();
        self.state.cursor_byte = ByteIdx(self.state.confirmed_text.len());
    }

    fn set_phase(&mut self, to: Phase) {
        let from = self.state.phase;
        if from != to {
            self.state.phase = to;
            self.push_event(TrackerEvent::PhaseChanged { from, to });
        }
    }

    /// 事件队列有上限：窗格不取就丢最旧的，不能无限增长。
    fn push_event(&mut self, event: TrackerEvent) {
        if self.events.len() >= MAX_PENDING_EVENTS {
            self.events.remove(0);
        }
        self.events.push(event);
    }
}

#[cfg(test)]
mod tests;
