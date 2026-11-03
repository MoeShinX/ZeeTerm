//! 终端工作区：标签、窗格、分屏、广播、选区/粘贴/鼠标上报、高亮扫描，以及这些功能的渲染。
//!
//! 它是一个可复用的 entity（`TerminalWorkspace`）：主窗口 `MainWindow` 和调试用的
//! `TerminalDemo` 窗口各持有一个，两边用的是同一份代码。V2-F1 修过的流控、Ctrl+C、
//! 断线提示、异步连接等逻辑都在 [`Pane`] 和下面的输入处理里，抽取时原样搬过来，
//! 没有改动语义。
//!
//! 工作区自己不画标题栏、标签条、主机树，也不知道主机库：谁要用它，就在外面套自己的
//! 外壳，通过 `open_host_tab` / `new_empty_tab` / `duplicate_tab` 等方法开标签。
//! 网格在窗口里的位置由 `canvas` 量出来（上一帧的结果），不再写死窗口坐标。

use ::gpui::{
    Anchor, Bounds, ClipboardItem, DismissEvent, FocusHandle, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, ScrollDelta, ScrollEvent, Subscription,
    Task, Window, anchored, canvas, deferred,
};
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::menu::PopupMenu;
use gpui_kit::component::slider::{SliderEvent, SliderState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;
use zssh_core::ssh::flow::FlowGate;
use zssh_core::ssh::latency::{self, LatencyMonitor, LatencySnapshot, LatencyViewer};
use zssh_session::{
    CoreFilter, KeyInput, LoopbackBackend, MouseAction, MouseButton as TerminalMouseButton,
    MouseInput, SessionBackend, SessionCommand, SessionDriver, SessionLog, SessionPump, encode_key,
    encode_mouse,
};
use zssh_terminal::{MouseMode, NamedColor, Size, Snapshot, TerminalColor};

use crate::highlight::{EngineHandle, HighlightRule, MAX_RULES as MAX_HIGHLIGHT_RULES, Span};
use crate::settings::{
    CursorShape, DEFAULT_FONT_SIZE, MAX_FONT_SIZE, MAX_LINE_HEIGHT, MAX_SCROLLBACK, MIN_FONT_SIZE,
    MIN_LINE_HEIGHT, MIN_SCROLLBACK, UiSettings,
};
use crate::sftp_demo::{SftpDrag, terminal_insert_text};
use crate::split_tree::{self, Axis, Layout, Node, PaneId};
use crate::tab_set::TabSet;

/// 每个窗格四周的内边距（0.2.14 的 `p-2`）：文字和光标不贴边，焦点线也不压在字上。
const PANE_PAD: f32 = 8.0;

/// 还没量到区域、也没有窗口信息时，新标签用的初始尺寸。
const DEFAULT_SIZE: Size = Size {
    rows: 30,
    cols: 120,
};

/// 窗格的会话状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PaneState {
    /// 正常：离线演示窗格，或已连上的 SSH 会话。
    Live,
    /// 后台正在连接/开 shell，界面只显示"正在连接…"。
    Connecting,
    /// 连接失败或已断开：输入不再发出，等用户重连。
    Disconnected,
    /// 空白标签（「+」新建、还没选主机）：没有会话，输入全部丢弃，也不显示任何提示。
    Empty,
}

/// 连接中/已断开的窗格用的写端：丢掉一切输入。
/// 不能退化成回环后端，否则键入的内容（包括密码）会在屏幕上明文回显。
struct NullBackend;

impl SessionBackend for NullBackend {
    fn send(&mut self, _command: SessionCommand) {}
}

/// 去掉控制字符：错误和断线原因里可能带远端可控的文字，不能让它在终端里夹带转义序列。
fn plain(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_control()).collect()
}

const RECONNECT_HINT: &str = "按 Ctrl+Shift+R 或点「重连」重新连接。";

/// 用户主动断开时窗格里显示的文字。
const USER_DISCONNECT_NOTICE: &str = "\r\n\x1b[33m已断开连接（手动）\x1b[0m\r\n按 Ctrl+Shift+R 或点标签右键「重新连接」重新连接。\r\n";

/// 连接失败时窗格里显示的文字。
fn failure_notice(error: &str) -> String {
    format!(
        "\r\n\x1b[31m{}\x1b[0m\r\n{RECONNECT_HINT}\r\n",
        plain(error)
    )
}

/// 会话结束时窗格里显示的文字：有原因写原因，否则写退出码。
fn disconnect_notice(exit: &zssh_session::SessionExit) -> String {
    let detail = match (&exit.reason, exit.code) {
        (Some(reason), _) => format!("：{}", plain(reason)),
        (None, Some(code)) => format!("（退出码 {code}）"),
        (None, None) => "（远端关闭了连接）".to_owned(),
    };
    format!("\r\n\x1b[33m连接已断开{detail}\x1b[0m\r\n{RECONNECT_HINT}\r\n")
}

/// 快速连接（`user@host:port` + 密码，不进主机库）的目标。密码只留在内存里，给重连和复制标签用
/// （0.2.14 的标签来源 `quick` 也带着密码）；手写 `Debug`，免得它进日志。
pub(crate) struct QuickSpec {
    pub target: zssh_core::model::QuickTarget,
    pub password: String,
    /// 标签上显示的名字（就是用户输入的那串）。
    pub label: String,
}

impl std::fmt::Debug for QuickSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuickSpec")
            .field("label", &self.label)
            .field("password", &"<已隐藏>")
            .finish()
    }
}

pub(crate) struct Pane {
    /// 分屏树的叶子只记它；重连换掉整个窗格时要沿用旧编号。
    pub(crate) id: PaneId,
    driver: SessionDriver<CoreFilter>,
    backend: Box<dyn SessionBackend>,
    pub(crate) snapshot: Snapshot,
    pub(crate) fed: usize,
    /// 从主机库打开的会话记住主机和已认证连接，复制标签时复用。
    pub(crate) host_id: Option<i64>,
    /// 快速连接的窗格（没有主机记录，靠这个记着连哪儿）。
    pub(crate) quick: Option<Arc<QuickSpec>>,
    pub(crate) factory: Option<Arc<crate::ssh_backend::SshSessionFactory>>,
    pub(crate) state: PaneState,
    /// 后台连接任务；窗格被关掉时随它一起取消。
    pending: Option<crate::ssh_backend::PendingOpen>,
    /// 向远端申请 PTY 时用的尺寸；连接期间窗口变了，连上后要补发一次。
    open_size: Size,
    history: usize,
    generation: u64,
    /// 这个窗格的 shell 是第几个在服务器上打开的（全进程递增）：同一条连接上有几个终端时，
    /// 跟随终端目录靠它和服务器上按打开顺序排的 shell 对号。
    shell_seq: u64,
    /// 这个窗格的 shell 在 AI 终端登记表里的编号（V2-G10b）；没连上 / 离线回环窗格没有。
    terminal_id: Option<String>,
    /// 右上角延迟徽标的数据来源；串口、经跳板机的 Telnet、没连上的窗格没有。
    latency: Option<PaneLatency>,
}

/// 窗格对延迟监视器的登记：`viewer` 报告这个窗格现在看不看得见（都看不见时后台测量暂停），
/// `label` 是悬停提示里写的主机名。
pub(crate) struct PaneLatency {
    viewer: LatencyViewer,
    label: String,
}

/// 下一个窗格编号（全进程唯一：标签可以在窗口之间搬）。
fn next_pane_id() -> PaneId {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// 下一个 shell 的序号。
fn next_shell_seq() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl Pane {
    fn new(size: Size, history: usize, generation: u64) -> Self {
        let pump = SessionPump::new(
            size,
            history,
            Arc::new(FlowGate::new()),
            CoreFilter::new(),
            generation,
        );
        let (backend, events) = LoopbackBackend::new(generation);
        let mut pane = Self::with_parts(pump, Box::new(backend), events, None, None);
        pane.history = history;
        pane
    }

    fn with_parts(
        pump: SessionPump<CoreFilter>,
        backend: Box<dyn SessionBackend>,
        events: Receiver<zssh_session::SessionEvent>,
        host_id: Option<i64>,
        factory: Option<Arc<crate::ssh_backend::SshSessionFactory>>,
    ) -> Self {
        let snapshot = pump.snapshot();
        let generation = pump.generation();
        Self {
            id: next_pane_id(),
            open_size: snapshot.size,
            driver: SessionDriver::new(pump, events),
            backend,
            snapshot,
            fed: 0,
            host_id,
            quick: None,
            factory,
            state: PaneState::Live,
            pending: None,
            history: crate::settings::DEFAULT_SCROLLBACK,
            generation,
            shell_seq: next_shell_seq(),
            terminal_id: None,
            latency: None,
        }
    }

    /// 没有活会话的占位窗格：输入全部丢弃，事件通道是空的（发送端已丢）。
    /// 连接中和失败/断开两种状态共用它。
    fn placeholder(
        size: Size,
        history: usize,
        generation: u64,
        host_id: Option<i64>,
        state: PaneState,
    ) -> Self {
        let pump = SessionPump::new(
            size,
            history,
            Arc::new(FlowGate::new()),
            CoreFilter::new(),
            generation,
        );
        let (_, events) = std::sync::mpsc::channel();
        let mut pane = Self::with_parts(pump, Box::new(NullBackend), events, host_id, None);
        pane.state = state;
        pane.history = history;
        pane.generation = generation;
        pane
    }

    /// 后台开始连接：窗格立刻可见，显示"正在连接…"，结果在 `poll_events` 里接上。
    fn connecting(
        size: Size,
        history: usize,
        generation: u64,
        host_id: Option<i64>,
        pending: crate::ssh_backend::PendingOpen,
    ) -> Self {
        let mut pane = Self::placeholder(size, history, generation, host_id, PaneState::Connecting);
        pane.pending = Some(pending);
        pane.pump_mut().feed_local("正在连接…\r\n".as_bytes());
        pane.refresh();
        pane
    }

    /// 开新窗格：有已认证连接就在同一条连接上再开 shell，否则按主机库重新连接；
    /// 都没有就是离线回环窗格。连接都在后台做，这里不阻塞界面线程。
    fn start_ssh(
        factory: Option<Arc<crate::ssh_backend::SshSessionFactory>>,
        host_id: Option<i64>,
        size: Size,
        history: usize,
        generation: u64,
    ) -> Self {
        if let Some(factory) = factory {
            let pending = crate::ssh_backend::begin_open(factory, size, generation);
            return Self::connecting(size, history, generation, host_id, pending);
        }
        let Some(host_id) = host_id else {
            return Self::new(size, history, generation);
        };
        let pending = crate::ssh_backend::begin_connect(host_id, size, generation);
        Self::connecting(size, history, generation, Some(host_id), pending)
    }

    /// 快速连接的窗格：有已认证连接就在同一条连接上再开 shell，否则按地址 + 密码连；都在后台做。
    fn start_quick(
        spec: Arc<QuickSpec>,
        factory: Option<Arc<crate::ssh_backend::SshSessionFactory>>,
        size: Size,
        history: usize,
        generation: u64,
    ) -> Self {
        let pending = match factory {
            Some(factory) => crate::ssh_backend::begin_open(factory, size, generation),
            None => crate::ssh_backend::begin_connect_quick(
                &spec.target,
                spec.password.clone(),
                size,
                generation,
            ),
        };
        let mut pane = Self::connecting(size, history, generation, None, pending);
        pane.quick = Some(spec);
        pane
    }

    /// 会话日志文件名里用的名字：主机库里的主机用它的名字，快速连接用它的标签。
    fn log_label(&self) -> String {
        if let Some(spec) = &self.quick {
            return spec.label.clone();
        }
        self.host_id
            .and_then(|id| {
                zssh_core::store::Database::open(&crate::settings::data_dir().join("zssh.db"))
                    .ok()
                    .and_then(|db| db.host_get(id).ok())
            })
            .map(|host| host.name)
            .unwrap_or_else(|| "session".to_owned())
    }

    /// 这个窗格是不是一个会话（主机库里的主机，或者快速连接）。
    fn is_session(&self) -> bool {
        self.host_id.is_some() || self.quick.is_some()
    }

    /// 连上了：换成真实会话的泵、写端和事件流，保留当前的尺寸和正在记录的日志。
    fn attach(
        &mut self,
        backend: Box<dyn SessionBackend>,
        events: Receiver<zssh_session::SessionEvent>,
        gate: Arc<FlowGate>,
        factory: Option<Arc<crate::ssh_backend::SshSessionFactory>>,
    ) {
        let size = self.snapshot.size;
        let log = self.pump_mut().take_log();
        let mut pump =
            SessionPump::new(size, self.history, gate, CoreFilter::new(), self.generation);
        if let Some(log) = log {
            pump.set_log(log);
        }
        // 设置里开了会话日志：这个会话一个文件（每次连上 / 重连各一个）。
        if self.is_session()
            && let Some(plain) = crate::app_prefs::open_session_log(&self.log_label())
        {
            pump.set_plain_log(plain);
        }
        self.driver = SessionDriver::new(pump, events);
        // 告诉 AI 终端登记表：这个 shell 属于哪台主机（快速连接没有主机记录，登记表里就没有主机）。
        self.terminal_id = backend.terminal_id();
        if let Some(id) = &self.terminal_id {
            crate::ai_terminals::terminals().describe(id, self.host_id);
        }
        self.backend = backend;
        self.factory = factory;
        self.state = PaneState::Live;
        self.shell_seq = next_shell_seq();
        if size != self.open_size {
            self.backend.send(SessionCommand::Resize(size));
            self.open_size = size;
        }
        self.snapshot = self.driver.pump().snapshot();
    }

    /// 连接失败或会话断开：停止接收输入，在窗格里写明原因。
    fn disconnect(&mut self, notice: &str) {
        self.terminal_id = None;
        // 断开后徽标没有意义；丢掉登记，共用这条连接的其他窗格不受影响。
        self.latency = None;
        self.state = PaneState::Disconnected;
        self.pending = None;
        self.pump_mut().feed_local(notice.as_bytes());
    }

    /// 这个窗格有没有可以主动断开的会话：连着或正在连的主机 / 快速连接。
    fn can_disconnect(&self) -> bool {
        self.is_session() && matches!(self.state, PaneState::Live | PaneState::Connecting)
    }

    /// 用户主动断开（标签右键「断开连接」）：只结束这个窗格自己的 shell 通道，窗格留着显示已断开，
    /// 之后能用「重新连接」恢复。同一条 SSH 连接上别的标签 / 窗格 / SFTP 另外持有连接的引用，
    /// 这里只放掉自己那一份（`factory` 置空、关通道），连接是不是跟着关交给引用计数：最后一个
    /// 使用者走了才会真的断，不会误伤别人。
    fn disconnect_by_user(&mut self) {
        if !self.can_disconnect() {
            return;
        }
        // 先收尾：日志写完、通道发 Close、还在连的任务取消（`pending` 一并丢）。
        self.close();
        // 旧写端丢掉：之后的输入不会再写进（可能已经半关的）通道。
        self.backend = Box::new(NullBackend);
        self.factory = None;
        self.disconnect(USER_DISCONNECT_NOTICE);
        self.refresh();
    }

    /// 接上延迟监视器（`None` 就是这个窗格不显示延迟）。
    fn set_latency(&mut self, monitor: Option<Arc<LatencyMonitor>>) {
        self.latency = monitor.map(|monitor| PaneLatency {
            viewer: monitor.viewer(),
            label: self.latency_label(),
        });
    }

    /// 悬停提示里写的主机名：快速连接用输入的那串，主机库里的主机用它的名字。
    fn latency_label(&self) -> String {
        if self.is_session() {
            self.log_label()
        } else {
            String::new()
        }
    }

    /// 这个窗格现在看得见吗（外壳每个节拍告诉它）；变了才动监视器。
    fn set_latency_visible(&mut self, visible: bool) {
        if let Some(latency) = &mut self.latency {
            latency.viewer.set_visible(visible);
        }
    }

    /// 徽标要画的东西：监视器的快照和悬停提示用的主机名。已经断开 / 还没有第一次结果就不画。
    fn latency_snapshot(&self) -> Option<(LatencySnapshot, &str)> {
        let latency = self.latency.as_ref()?;
        let snapshot = latency.viewer.monitor().snapshot();
        (!snapshot.stopped && !snapshot.stats.is_empty())
            .then_some((snapshot, latency.label.as_str()))
    }

    /// 调试钩子：摆一个固定的延迟值（给截图用）。窗格没有监视器就装一个不测量的。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_set_latency(&mut self, sample: Option<latency::Sample>) {
        let Some(sample) = sample else {
            self.latency = None;
            return;
        };
        if self.latency.is_none() {
            self.set_latency(Some(LatencyMonitor::new(latency::ProbeKind::Ssh)));
            if let Some(latency) = &mut self.latency
                && latency.label.is_empty()
            {
                latency.label = "web-1".to_owned();
            }
        }
        if let Some(latency) = &self.latency {
            latency.viewer.monitor().record(sample);
        }
    }

    /// 回滚上限改了：新建会话（重连后的泵）用新值，正在用的终端立刻生效（调小会丢掉最旧的行）。
    fn set_history(&mut self, history: usize) {
        self.history = history;
        self.pump_mut().set_history(history);
        self.refresh();
    }

    fn pump(&self) -> &SessionPump<CoreFilter> {
        self.driver.pump()
    }

    fn pump_mut(&mut self) -> &mut SessionPump<CoreFilter> {
        self.driver.pump_mut()
    }

    /// 后台连接任务有结果了就接上；返回是否有变化。
    fn poll_pending(&mut self) -> bool {
        let Some(result) = self.pending.as_ref().and_then(|pending| pending.try_take()) else {
            return false;
        };
        self.pending = None;
        match result {
            Ok(opened) => {
                // SSH 窗格没有自带的监视器：用已认证连接上那一份（同一条连接上的窗格共用）。
                let monitor = opened.latency.or_else(|| {
                    opened
                        .factory
                        .as_ref()
                        .map(|factory| Arc::clone(factory.connection().latency()))
                });
                self.attach(opened.backend, opened.events, opened.gate, opened.factory);
                self.set_latency(monitor);
            }
            Err(err) => self.disconnect(&failure_notice(&err)),
        }
        true
    }

    /// 事件到了就刷新快照；返回是否有变化。
    fn poll_events(&mut self) -> bool {
        let mut changed = self.poll_pending();
        if self.driver.poll() {
            changed = true;
        }
        // 远端退出、连接断了、写失败：提示一次。离线回环窗格关闭时的 Exit 不提示。
        if self.state == PaneState::Live
            && self.is_session()
            && let Some(exit) = self.driver.exit().cloned()
        {
            self.disconnect(&disconnect_notice(&exit));
            changed = true;
        }
        if changed {
            self.snapshot = self.driver.pump().snapshot();
        }
        changed
    }

    fn refresh(&mut self) {
        self.snapshot = self.driver.pump().snapshot();
    }

    /// 写命令走会话后端；同进程回环后端会立刻回送事件。
    /// 还在连接或已断开时丢弃输入，不能回显也不能攒着。
    fn write(&mut self, bytes: &[u8]) {
        if self.state != PaneState::Live {
            return;
        }
        self.backend.send(SessionCommand::Write(bytes.to_vec()));
        self.poll_events();
    }

    /// 发送 Break 信号（串口会话的按钮 / 快捷键用，后续界面卡接上；其他协议的会话忽略）。
    #[allow(dead_code)]
    pub(crate) fn send_break(&mut self) {
        if self.state == PaneState::Live {
            self.backend.send_break();
        }
    }

    /// 本地重排后把新尺寸也发给远端（回环后端忽略）。
    fn resize(&mut self, size: Size) {
        self.pump_mut().resize(size);
        if self.state == PaneState::Live {
            self.backend.send(SessionCommand::Resize(size));
            self.open_size = size;
        }
        self.refresh();
    }

    /// 关闭窗格：先刷日志，再通知后端；还在连接的任务随 `pending` 一起取消。
    fn close(&mut self) {
        if let Some(log) = self.pump_mut().take_log() {
            log.finish();
        }
        self.pending = None;
        self.backend.send(SessionCommand::Close);
    }
}

fn terminal_button(button: MouseButton) -> Option<TerminalMouseButton> {
    match button {
        MouseButton::Left => Some(TerminalMouseButton::Left),
        MouseButton::Middle => Some(TerminalMouseButton::Middle),
        MouseButton::Right => Some(TerminalMouseButton::Right),
        _ => None,
    }
}

/// 标签上状态点的含义（0.2.14 的 `tabDot`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TabStatus {
    /// 空白标签。
    Empty,
    Connecting,
    Connected,
    Disconnected,
}

/// 一个标签里各窗格的状态合成一个点：正在连 > 已连上 > 全断开 > 空标签。
pub(crate) fn tab_status(states: &[PaneState]) -> TabStatus {
    if states.contains(&PaneState::Connecting) {
        TabStatus::Connecting
    } else if states.contains(&PaneState::Live) {
        TabStatus::Connected
    } else if states.contains(&PaneState::Disconnected) {
        TabStatus::Disconnected
    } else {
        TabStatus::Empty
    }
}

/// 外壳（标题栏标签条）画一个标签需要的全部信息。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TabInfo {
    pub id: u64,
    pub title: String,
    pub status: TabStatus,
    /// 聚焦窗格连的主机；空白标签和离线演示窗格没有。
    pub host_id: Option<i64>,
    /// 聚焦窗格是快速连接时的 `user@host:port`（右键「复制地址」用）。
    pub quick_target: Option<String>,
    /// 聚焦窗格是快速连接时归并用的键 `用户@地址:端口`（地址不分大小写）：输入写法不同（带不带端口）
    /// 但连的是同一处，就算同一台主机（标签序号按它编号）。
    pub quick_key: Option<String>,
    /// 至少有一个窗格已经断开（右键「重新连接」才可用）。
    pub can_reconnect: bool,
    /// 至少有一个窗格还连着或正在连（右键「断开连接」才可用）。
    pub can_disconnect: bool,
    /// AI 此刻正在这个标签的某个终端里执行命令（标签上画一个活动点，0.2.14 的 `aiBadges`）。
    pub ai_active: bool,
}

/// 标签 id 全进程唯一：标签可以在窗口之间搬，id 不能撞。
fn next_tab_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub(crate) struct Tab {
    id: u64,
    title: String,
    /// 窗格，永远按视觉顺序排（和 `tree` 的叶子顺序一致：左到右、上到下），所以下标就是
    /// 屏幕上的次序；`active_pane` 是聚焦窗格的下标。
    panes: Vec<Pane>,
    active_pane: usize,
    /// 分屏树（0.2.14 的 `splitTree`）：叶子是窗格编号，分割点记方向和比例。
    tree: Node,
    /// 输入广播：键盘和粘贴同时发给这个标签的所有分屏窗格（0.2.14 也只作用于当前标签）。
    broadcast: bool,
}

impl Tab {
    fn new(title: String, pane: Pane) -> Self {
        Self {
            id: next_tab_id(),
            title,
            tree: Node::Pane(pane.id),
            panes: vec![pane],
            active_pane: 0,
            broadcast: false,
        }
    }

    /// 整个标签换成一个新窗格（空白标签里选了主机）：分屏树一并重置。
    fn reset_to(&mut self, pane: Pane) {
        self.tree = Node::Pane(pane.id);
        self.panes = vec![pane];
        self.active_pane = 0;
    }

    /// 把聚焦的窗格按 `axis` 一分为二：新窗格紧跟在它后面（右 / 下），并拿到焦点（0.2.14 的
    /// `splitAt`）。返回新窗格的下标。
    fn split_active(&mut self, axis: Axis, new_pane: Pane) -> usize {
        let at = self.active_pane.min(self.panes.len() - 1);
        let target = self.panes[at].id;
        let new_id = new_pane.id;
        if !self
            .tree
            .split_pane(target, new_id, split_tree::next_split_id(), axis)
        {
            // 树和窗格表不一致（不该发生）：宁可不拆，也不让两边对不上。
            return at;
        }
        self.panes.insert(at + 1, new_pane);
        self.active_pane = at + 1;
        self.debug_check_order();
        at + 1
    }

    /// 窗格表必须和分屏树的叶子同序（下标 = 屏幕上的次序，鼠标命中、广播、验证钩子都靠它）。
    fn debug_check_order(&self) {
        debug_assert_eq!(
            self.tree.pane_ids(),
            self.panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
            "窗格表和分屏树不同序"
        );
    }

    /// 关掉第 `index` 个窗格，兄弟节点顶掉它的位置；焦点按 0.2.14 的规则挪（关的不是焦点
    /// 窗格就不动，是的话落到相邻窗格）。只剩一个窗格时不动，返回 `None`（调用方该关整个标签）。
    fn close_pane_at(&mut self, index: usize) -> Option<Pane> {
        if self.panes.len() <= 1 || index >= self.panes.len() {
            return None;
        }
        let victim = self.panes[index].id;
        let focused = self.panes[self.active_pane.min(self.panes.len() - 1)].id;
        let tree = std::mem::replace(&mut self.tree, Node::Pane(victim));
        let (tree, focus) = split_tree::close_pane_with_focus(tree, victim, focused);
        let Some(tree) = tree else {
            // 多于一个窗格时关一个不会清空树；防御性地还原。
            self.tree = Node::Pane(self.panes[0].id);
            return None;
        };
        self.tree = tree;
        let pane = self.panes.remove(index);
        self.active_pane = focus
            .and_then(|id| self.panes.iter().position(|pane| pane.id == id))
            .unwrap_or(0);
        self.debug_check_order();
        Some(pane)
    }

    fn active(&self) -> &Pane {
        &self.panes[self.active_pane.min(self.panes.len() - 1)]
    }

    fn active_mut(&mut self) -> &mut Pane {
        let index = self.active_pane.min(self.panes.len() - 1);
        &mut self.panes[index]
    }

    fn is_split(&self) -> bool {
        self.panes.len() > 1
    }

    /// 键盘输入和粘贴要发给哪些窗格：开了广播是这个标签的所有窗格，否则只有聚焦的那个。
    /// 广播只作用于当前标签，不会越过标签去碰别的主机。
    fn input_targets(&self) -> Vec<usize> {
        if self.broadcast {
            (0..self.panes.len()).collect()
        } else {
            vec![self.active_pane.min(self.panes.len() - 1)]
        }
    }

    fn status(&self) -> TabStatus {
        let states: Vec<PaneState> = self.panes.iter().map(|pane| pane.state).collect();
        tab_status(&states)
    }

    /// 断开标签里所有连着的会话窗格；返回断开了几个。
    fn disconnect_sessions(&mut self) -> usize {
        let mut count = 0;
        for pane in &mut self.panes {
            if pane.can_disconnect() {
                pane.disconnect_by_user();
                count += 1;
            }
        }
        count
    }

    /// 还没连任何主机的空白标签。
    fn is_blank(&self) -> bool {
        self.panes.iter().all(|pane| pane.state == PaneState::Empty)
    }

    fn info(&self) -> TabInfo {
        TabInfo {
            id: self.id,
            title: self.title.clone(),
            status: self.status(),
            host_id: self.active().host_id,
            quick_target: self.active().quick.as_ref().map(|spec| spec.label.clone()),
            quick_key: self.active().quick.as_ref().map(|spec| {
                format!(
                    "{}@{}:{}",
                    spec.target.username,
                    spec.target.host.to_lowercase(),
                    spec.target.port
                )
            }),
            can_reconnect: self
                .panes
                .iter()
                .any(|pane| pane.state == PaneState::Disconnected && pane.is_session()),
            can_disconnect: self.panes.iter().any(Pane::can_disconnect),
            ai_active: self.panes.iter().any(|pane| {
                pane.terminal_id
                    .as_deref()
                    .is_some_and(|id| crate::ai_terminals::terminals().is_ai_active(id))
            }),
        }
    }
}

/// 高亮状态：规则、修订号、最近扫描结果都在这里（面板的输入框等界面状态归外壳）。
struct HighlightUi {
    rules: Vec<HighlightRule>,
    revision: u64,
    next_id: u64,
    /// 每行的命中区间，覆盖当前聚焦窗格。
    spans: Vec<Vec<Span>>,
    /// 上次请求的 (标签 id, 窗格, 修订, 原始字节, 滚动偏移, 行数)。
    key: Option<(u64, usize, u64, usize, usize, usize)>,
    status: Option<String>,
    engine: EngineHandle,
}

impl HighlightUi {
    fn bump(&mut self) {
        self.revision += 1;
        self.key = None;
    }
}

/// 一个窗格在窗口里的位置（窗口坐标，逻辑像素）：鼠标命中和换算格子都靠它。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct PaneGeom {
    /// 窗格整块（含四周内边距）：点在这里面就是聚焦这个窗格。
    rect: split_tree::Rect,
    /// 文字区的左上角。
    left: f32,
    top: f32,
    cols: usize,
    rows: usize,
}

pub struct TerminalWorkspace {
    tabs: TabSet<Tab>,
    focus: FocusHandle,
    /// (anchor, head)，可见区坐标。
    selection: Option<((usize, usize), (usize, usize))>,
    selecting: bool,
    /// 左键按下的格子；指针离开这一格才产生选区（单击不留空选区）。
    drag_anchor: Option<(usize, usize)>,
    cell_width: f32,
    cell_height: f32,
    pending_paste: Option<String>,
    hover_link: Option<Arc<str>>,
    /// 持久化的界面设置（字号、行高、光标、主题）。
    settings: UiSettings,
    /// 一行状态文字（日志路径、「正在重新连接…」、不能执行的原因）。
    status_text: String,
    /// 高亮规则与最近一次扫描结果。
    highlight: HighlightUi,
    /// 定时取后台结果的轻量任务。
    _highlight_tick: Task<()>,
    /// 正在拖的分隔条（分割点编号）。
    dragging_split: Option<u64>,
    /// 活动标签上一帧的窗格位置（和 `Tab::panes` 同序）。
    pane_geom: Vec<PaneGeom>,
    /// 活动标签上一帧的布局（相对终端区域）：拖分隔条时按它换算比例。
    layout: Layout,
    /// 终端区域在窗口里的左上角（上一帧量到的）。
    area_origin: (f32, f32),
    /// 设置里的字体栈解析出来的实际字体名：栈没变就不重算。
    font_cache: Option<(String, SharedString)>,
    /// 设置页「自定义字体」的输入框（工作区拥有，设置页只是画它）。
    font_input: Entity<InputState>,
    /// 设置页选了「自定义…」：即使栈的第一项是常见字体也显示输入框。
    font_custom: bool,
    /// 设置页回滚行数的滑块；松手才应用（拖的过程中只是预览，免得途经小值时丢掉历史）。
    scrollback_slider: Entity<SliderState>,
    _settings_subs: Vec<Subscription>,
    /// 弹着的终端右键菜单（V2-U25）；关掉（点外面、Esc、点了某项）时清空。
    context_menu: Option<ContextMenu>,
    /// 远端已接管本次左/中/右键按下，松开时要补一条上报。
    mouse_pressed: bool,
    /// 上一次上报的格子；移动事件只在跨格时发。
    last_mouse_cell: Option<(usize, usize)>,
    /// 窗格区域在窗口里的位置：canvas 在绘制时写入，下一帧排版读取。
    area: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// 上一次排版用的区域；和 `area` 不一样说明窗口或外壳变了，要重排。
    layout_area: Option<Bounds<Pixels>>,
    /// 新开的标签用的初始尺寸（最近一次排版算出来的整宽窗格）。
    default_size: Size,
    /// 输入法状态机里本窗口的名字（关掉「自动切英文」时要把输入法还回去）。
    ime_key: String,
    /// 光标闪烁的相位时钟。
    cursor_blink: BlinkClock,
    /// 终端内容现在有没有摆在窗口里（主窗口在主机页 / 设置页 / 文件标签时没有）。外壳每个节拍告诉
    /// 它；独立终端窗口恒为真。延迟测量只对看得见的窗格做。
    surface_shown: bool,
    /// 上一次看到的各窗格延迟版本之和：变了就要重画徽标。
    latency_stamp: u64,
    _ime_focus: Subscription,
    _ime_blur: Subscription,
}

/// 弹着的右键菜单：菜单实体、弹出的位置（窗口坐标）、弹出时的条目（调试断言读它）。
struct ContextMenu {
    menu: Entity<PopupMenu>,
    position: ::gpui::Point<Pixels>,
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    entries: Vec<crate::terminal_menu::Entry>,
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    link: Option<Arc<str>>,
    /// 订阅随菜单一起放着：丢掉菜单，订阅也就没了。
    _dismiss: Subscription,
}

/// 窗口现在是不是真的摆在用户面前：可见且没有最小化（收进托盘的窗口不可见）。
/// 非 Windows 没有这个概念（ZeeTerm 只面向 Windows），一律当作可见。
fn window_shown(window: &Window) -> bool {
    #[cfg(windows)]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindowVisible};
        if let Ok(handle) = HasWindowHandle::window_handle(window)
            && let RawWindowHandle::Win32(win32) = handle.as_raw()
        {
            let hwnd = HWND(win32.hwnd.get() as _);
            // SAFETY: 只读查询，句柄失效时返回假。
            return unsafe { IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool() };
        }
    }
    let _ = window;
    true
}

/// 哪些窗格该被登记成"看得见"：只有活动标签的窗格，而且设置开着、窗口和页面都摆在用户面前。
pub(crate) fn latency_visible_flags(
    pane_count: usize,
    tab_is_active: bool,
    setting_on: bool,
    surface_shown: bool,
    window_shown: bool,
) -> Vec<bool> {
    vec![tab_is_active && setting_on && surface_shown && window_shown; pane_count]
}

impl TerminalWorkspace {
    /// 空的工作区：一个标签也没有。外壳自己决定什么时候开第一个。
    pub fn new(window: &mut Window, cx: &mut Context<Self>, ime_key: String) -> Self {
        let focus = cx.focus_handle();
        let settings = UiSettings::load();
        let rules = settings
            .highlight_rules
            .clone()
            .unwrap_or_else(crate::highlight::default_rules);
        let next_id = rules.iter().map(|rule| rule.id).max().unwrap_or(0) + 1;
        let highlight = HighlightUi {
            rules,
            revision: 1,
            next_id,
            spans: Vec::new(),
            key: None,
            status: None,
            engine: EngineHandle::start(),
        };
        // 「终端里自动切到英文输入」关着时不碰输入法（离开时 `apply(false)` 对没进过英文区的窗口是空操作）。
        let _ime_focus = cx.on_focus(&focus, window, {
            let key = ime_key.clone();
            move |this, _window, _cx| {
                if this.settings.ime_english {
                    crate::ime::apply(&key, true);
                }
            }
        });
        let ime_blur_key = ime_key.clone();
        let _ime_blur = cx.on_blur(&focus, window, move |_this, _window, _cx| {
            crate::ime::apply(&ime_blur_key, false);
        });
        // 设置页的两个控件：自定义字体的输入框、回滚行数的滑块。状态归工作区，这样设置页每次
        // 重绘拿到的是同一个实体。
        let font_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("JetBrains Mono, Cascadia Mono, Consolas, monospace")
        });
        font_input.update(cx, |state, cx| {
            state.set_value(settings.font_family.clone(), window, cx);
        });
        let scrollback_slider = cx.new(|_| {
            // 先设上限再设下限：组件每设一个就按当前的另一端夹一次值，反过来会先撞上默认的上限 100。
            SliderState::new()
                .max(MAX_SCROLLBACK as f32)
                .min(MIN_SCROLLBACK as f32)
                .step(crate::settings::SCROLLBACK_STEP as f32)
                .default_value(settings.scrollback as f32)
        });
        let mut settings_subs = Vec::new();
        settings_subs.push(cx.subscribe_in(
            &font_input,
            window,
            |this, input, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Change) {
                    let text = input.read(cx).value().to_string();
                    this.set_font_family(&text, cx);
                }
            },
        ));
        settings_subs.push(cx.subscribe_in(
            &scrollback_slider,
            window,
            |this, _slider, event: &SliderEvent, _window, cx| match event {
                // 拖的过程中只让数字跟着变；松手才真的应用（调小会丢掉最旧的行）。
                SliderEvent::Change(_) => cx.notify(),
                SliderEvent::Release(value) => {
                    this.set_scrollback(value.end().round() as usize, cx)
                }
            },
        ));
        let mut this = Self {
            tabs: TabSet::new(),
            focus,
            selection: None,
            selecting: false,
            drag_anchor: None,
            cell_width: 8.0,
            cell_height: 18.0,
            pending_paste: None,
            hover_link: None,
            settings,
            status_text: String::new(),
            dragging_split: None,
            pane_geom: Vec::new(),
            layout: Layout::default(),
            area_origin: (0.0, 0.0),
            font_cache: None,
            font_input,
            font_custom: false,
            scrollback_slider,
            _settings_subs: settings_subs,
            context_menu: None,
            mouse_pressed: false,
            last_mouse_cell: None,
            highlight,
            _highlight_tick: Task::ready(()),
            area: Rc::new(Cell::new(None)),
            layout_area: None,
            default_size: DEFAULT_SIZE,
            ime_key,
            cursor_blink: BlinkClock::new(),
            surface_shown: true,
            latency_stamp: 0,
            _ime_focus,
            _ime_blur,
        };
        // 后台扫描完成后主动重绘；没有新结果时只醒来一次。区域变了（窗口缩放、外壳的
        // 侧栏/状态栏出现消失）也在这里发现并重排。
        this._highlight_tick = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(60))
                    .await;
                let alive = this.update_in(cx, |this, window, cx| {
                    let sessions = this.poll_sessions();
                    let relayout = this.area.get() != this.layout_area;
                    // 光标闪烁只在终端有焦点时重画（没焦点光标画成静止，不必每半秒重画）。
                    let blink = this.settings.cursor_blink
                        && this.focus.is_focused(window)
                        && this.cursor_blink.changed();
                    let latency = this.sync_latency(window);
                    if this.poll_highlight() || sessions || relayout || blink || latency {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        // 跟随系统主题：每 2 秒对齐一次，只有「自动」模式会真正切换。
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(2)).await;
                let alive = this.update_in(cx, |this, _window, cx| this.sync_system_theme(cx));
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
        this
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// 外壳告诉工作区：终端内容现在有没有摆在窗口里（影响延迟测量的暂停 / 恢复）。
    pub(crate) fn set_surface_shown(&mut self, shown: bool) {
        self.surface_shown = shown;
    }

    /// 每个节拍：把各窗格"看不看得见"登记给延迟监视器（变了才有动作），并看延迟有没有新数据。
    /// 返回是否要重画。
    fn sync_latency(&mut self, window: &Window) -> bool {
        let shown = window_shown(window);
        let active = self.tabs.active_index();
        let setting = self.settings.show_latency;
        let surface = self.surface_shown;
        let mut stamp = 0u64;
        for (index, tab) in self.tabs.items_mut().iter_mut().enumerate() {
            let flags = latency_visible_flags(
                tab.panes.len(),
                Some(index) == active,
                setting,
                surface,
                shown,
            );
            for (pane, visible) in tab.panes.iter_mut().zip(flags) {
                pane.set_latency_visible(visible);
                if visible && let Some(latency) = &pane.latency {
                    stamp = stamp.wrapping_add(latency.viewer.monitor().version() + 1);
                }
            }
        }
        let changed = stamp != self.latency_stamp;
        self.latency_stamp = stamp;
        changed
    }

    // ---- 只读查询（外壳用） ----

    pub(crate) fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub(crate) fn active_index(&self) -> Option<usize> {
        self.tabs.active_index()
    }

    pub(crate) fn tab_infos(&self) -> Vec<TabInfo> {
        self.tabs.items().iter().map(Tab::info).collect()
    }

    /// 活动标签是不是还没选主机的空白标签。
    pub(crate) fn active_tab_is_blank(&self) -> bool {
        self.tabs.active().is_some_and(Tab::is_blank)
    }

    /// 活动标签聚焦窗格连的主机（状态栏、右键「复制地址」用）。
    pub(crate) fn active_host_id(&self) -> Option<i64> {
        self.tabs.active().and_then(|tab| tab.active().host_id)
    }

    pub(crate) fn tab_host_id(&self, index: usize) -> Option<i64> {
        self.tabs.get(index).and_then(|tab| tab.active().host_id)
    }

    /// 这个标签聚焦窗格的主机和已认证连接（右键「打开 SFTP」复用它，不再握手）。
    pub(crate) fn tab_origin(
        &self,
        index: usize,
    ) -> Option<(i64, Option<Arc<crate::ssh_backend::SshSessionFactory>>)> {
        let pane = self.tabs.get(index)?.active();
        Some((pane.host_id?, pane.factory.clone()))
    }

    /// 每个标签聚焦窗格的主机 id（AI 面板按标签各自一份，目标主机跟着各自的标签走）。
    pub(crate) fn tab_hosts(&self) -> Vec<(u64, Option<i64>)> {
        self.tabs
            .items()
            .iter()
            .map(|tab| (tab.id, tab.active().host_id))
            .collect()
    }

    /// 这个窗口里所有标签、所有分屏窗格的主机 id（含没聚焦的、已经断开的：标签还在就还能重连）。
    /// AI 对话锚定的主机「还有没有标签」按它算；只看各标签的聚焦窗格会把分屏里的主机漏掉。
    pub(crate) fn pane_host_ids(&self) -> std::collections::HashSet<i64> {
        self.tabs
            .items()
            .iter()
            .flat_map(|tab| tab.panes.iter().filter_map(|pane| pane.host_id))
            .collect()
    }

    /// 活动标签聚焦窗格是快速连接时：（它的终端编号，连上了才有；显示名）。AI 对话锚定快速连接时认终端本身。
    pub(crate) fn active_quick(&self) -> Option<(Option<String>, String)> {
        let pane = self.pane()?;
        let spec = pane.quick.as_ref()?;
        Some((self.active_terminal_id(), spec.label.clone()))
    }

    /// 这个标签聚焦窗格是快速连接时的 `user@host:port`。
    pub(crate) fn tab_quick_target(&self, index: usize) -> Option<String> {
        self.tabs
            .get(index)
            .and_then(|tab| tab.active().quick.as_ref().map(|spec| spec.label.clone()))
    }

    /// 活动窗格的主机和已认证连接（开 SFTP / 新窗口时复用）。
    pub(crate) fn active_origin(
        &self,
    ) -> Option<(i64, Option<Arc<crate::ssh_backend::SshSessionFactory>>)> {
        let pane = self.pane()?;
        Some((pane.host_id?, pane.factory.clone()))
    }

    pub(crate) fn settings(&self) -> &UiSettings {
        &self.settings
    }

    pub(crate) fn status_text(&self) -> &str {
        &self.status_text
    }

    pub(crate) fn set_status_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.status_text = text.into();
        cx.notify();
    }

    pub(crate) fn broadcast_on(&self) -> bool {
        self.tabs.active().is_some_and(|tab| tab.broadcast)
    }

    pub(crate) fn is_split(&self) -> bool {
        self.tabs.active().is_some_and(Tab::is_split)
    }

    pub(crate) fn highlight_rules(&self) -> &[HighlightRule] {
        &self.highlight.rules
    }

    pub(crate) fn highlight_status(&self) -> Option<&str> {
        self.highlight.status.as_deref()
    }

    pub(crate) fn highlight_set_status(&mut self, text: Option<String>) {
        self.highlight.status = text;
    }

    /// 活动标签的屏幕文字：截图前的隐私检查用。
    pub(crate) fn visible_text(&self) -> Vec<String> {
        let Some(tab) = self.tabs.active() else {
            return Vec::new();
        };
        tab.panes
            .iter()
            .flat_map(|pane| pane.snapshot.rows.iter().map(|row| row.text.clone()))
            .collect()
    }

    /// 活动窗格是否已经连上（调试钩子等它连上再往里打字）。
    pub(crate) fn active_pane_is_live(&self) -> bool {
        self.pane()
            .is_some_and(|pane| pane.state == PaneState::Live && pane.is_session())
    }

    // ---- 内部访问 ----

    fn pane(&self) -> Option<&Pane> {
        self.tabs.active().map(Tab::active)
    }

    fn pane_mut(&mut self) -> Option<&mut Pane> {
        self.tabs.active_mut().map(Tab::active_mut)
    }

    /// 换标签 / 改标签结构以后，旧的选区、悬停链接、待确认粘贴都不能留。
    fn reset_interaction(&mut self) {
        self.selection = None;
        self.selecting = false;
        self.drag_anchor = None;
        self.hover_link = None;
        self.pending_paste = None;
        self.mouse_pressed = false;
        self.last_mouse_cell = None;
        self.dragging_split = None;
        self.highlight.key = None;
        self.context_menu = None;
    }

    fn refresh(&mut self) {
        if let Some(pane) = self.pane_mut() {
            pane.refresh();
        }
    }

    /// 每帧取一下所有窗格的会话事件；后台标签也继续消费。
    fn poll_sessions(&mut self) -> bool {
        let mut changed = false;
        for tab in self.tabs.items_mut() {
            for pane in &mut tab.panes {
                changed |= pane.poll_events();
            }
        }
        changed
    }

    /// 关闭所有会话（窗口关闭前调用），避免 SSH 通道挂着。
    pub fn close_all(&mut self) {
        for tab in self.tabs.items_mut() {
            for pane in &mut tab.panes {
                pane.close();
            }
        }
    }

    fn close_tab_panes(mut tab: Tab) {
        for pane in &mut tab.panes {
            pane.close();
        }
    }

    // ---- 标签操作 ----

    pub(crate) fn select_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.tabs.select(index) {
            self.reset_interaction();
            self.refresh();
            cx.notify();
        }
    }

    /// Ctrl+Tab / Ctrl+Shift+Tab。
    pub(crate) fn cycle_tab(&mut self, step: isize, cx: &mut Context<Self>) -> bool {
        if !self.tabs.cycle(step) {
            return false;
        }
        self.reset_interaction();
        self.refresh();
        cx.notify();
        true
    }

    /// 在新标签里连一台主机（后台连接，立刻可见）。返回新标签的下标。
    pub(crate) fn open_host_tab(
        &mut self,
        host_id: i64,
        title: String,
        cx: &mut Context<Self>,
    ) -> usize {
        self.open_host_tab_with_factory(host_id, title, None, cx)
    }

    /// 同上；给了已认证连接就在这条连接上再开一个 shell，不再握手。
    pub(crate) fn open_host_tab_with_factory(
        &mut self,
        host_id: i64,
        title: String,
        factory: Option<Arc<crate::ssh_backend::SshSessionFactory>>,
        cx: &mut Context<Self>,
    ) -> usize {
        let history = self.settings.scrollback;
        let pane = Pane::start_ssh(factory, Some(host_id), self.default_size, history, 1);
        let index = self.tabs.push(Tab::new(title, pane));
        self.reset_interaction();
        cx.notify();
        index
    }

    /// 快速连接：在新标签里连 `user@host:port`（不保存主机）。返回新标签的下标。
    pub(crate) fn open_quick_tab(
        &mut self,
        target: zssh_core::model::QuickTarget,
        password: String,
        label: String,
        cx: &mut Context<Self>,
    ) -> usize {
        let spec = Arc::new(QuickSpec {
            target,
            password,
            label: label.clone(),
        });
        let history = self.settings.scrollback;
        let pane = Pane::start_quick(spec, None, self.default_size, history, 1);
        let index = self.tabs.push(Tab::new(label, pane));
        self.reset_interaction();
        cx.notify();
        index
    }

    /// 调试窗口用：丢掉现有的所有标签，只留一个正在连这台主机的标签。
    pub(crate) fn replace_with_host_tab(
        &mut self,
        host_id: i64,
        factory: Option<Arc<crate::ssh_backend::SshSessionFactory>>,
        size: Size,
        cx: &mut Context<Self>,
    ) {
        let history = self.settings.scrollback;
        let pane = Pane::start_ssh(factory, Some(host_id), size, history, 1);
        self.replace_with_tab(Tab::new(format!("终端 #{host_id}"), pane));
        cx.notify();
    }

    /// 调试窗口顶部那行：窗格尺寸、历史、滚动偏移、流控账本。
    pub(crate) fn stats_line(&self) -> String {
        let Some(pane) = self.pane() else {
            return String::new();
        };
        let snapshot = &pane.snapshot;
        let ledger = pane.pump().ledger();
        format!(
            "{} 行 x {} 列  历史 {}  滚动 {}  已喂 {}  raw {}  acked {}",
            snapshot.size.rows,
            snapshot.size.cols,
            snapshot.history_size,
            pane.pump().display_offset(),
            pane.fed,
            ledger.raw,
            ledger.acked
        )
    }

    /// 「+」：一个空白标签，等用户在里面选主机。
    pub(crate) fn new_empty_tab(&mut self, cx: &mut Context<Self>) -> usize {
        let history = self.settings.scrollback;
        let pane = Pane::placeholder(self.default_size, history, 1, None, PaneState::Empty);
        let index = self.tabs.push(Tab::new("新标签".to_owned(), pane));
        self.reset_interaction();
        cx.notify();
        index
    }

    /// 在活动的空白标签里连一台主机（0.2.14：空白标签选主机就在原地连，不再另开）。
    pub(crate) fn connect_active_empty(
        &mut self,
        host_id: i64,
        title: String,
        cx: &mut Context<Self>,
    ) -> bool {
        let size = self.default_size;
        let history = self.settings.scrollback;
        let Some(tab) = self.tabs.active_mut() else {
            return false;
        };
        if !tab.is_blank() {
            return false;
        }
        tab.title = title;
        // 空白标签里即使被分过屏，选了主机也是整个标签换成一个窗格。
        tab.reset_to(Pane::start_ssh(None, Some(host_id), size, history, 1));
        self.reset_interaction();
        cx.notify();
        true
    }

    /// 复制标签：紧挨在原标签右边再开一个连同一台主机的标签。活的会话复用它的已认证
    /// 连接（只多开一个 shell）；断线的会话连接已经死了，要重新连。空白标签没有可复制的。
    pub(crate) fn duplicate_tab(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        let Some(tab) = self.tabs.get(index) else {
            return false;
        };
        let source = tab.active();
        if source.host_id.is_none() && source.quick.is_none() {
            return false;
        }
        let factory = if source.state == PaneState::Live {
            source.factory.clone()
        } else {
            None
        };
        let size = source.snapshot.size;
        let title = tab.title.clone();
        let history = self.settings.scrollback;
        let pane = match (&source.quick, source.host_id) {
            (Some(spec), _) => Pane::start_quick(Arc::clone(spec), factory, size, history, 1),
            (None, host_id) => Pane::start_ssh(factory, host_id, size, history, 1),
        };
        self.tabs.insert_after(index, Tab::new(title, pane));
        self.reset_interaction();
        cx.notify();
        true
    }

    /// 关掉一个标签（可以关到一个不剩）。
    pub(crate) fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        let Some(tab) = self.tabs.remove(index) else {
            return false;
        };
        Self::close_tab_panes(tab);
        self.reset_interaction();
        self.refresh();
        cx.notify();
        true
    }

    pub(crate) fn close_other_tabs(&mut self, keep: usize, cx: &mut Context<Self>) -> usize {
        let removed = self.tabs.remove_others(keep);
        self.finish_bulk_close(removed, cx)
    }

    pub(crate) fn close_tabs_left(&mut self, index: usize, cx: &mut Context<Self>) -> usize {
        let removed = self.tabs.remove_left(index);
        self.finish_bulk_close(removed, cx)
    }

    pub(crate) fn close_tabs_right(&mut self, index: usize, cx: &mut Context<Self>) -> usize {
        let removed = self.tabs.remove_right(index);
        self.finish_bulk_close(removed, cx)
    }

    pub(crate) fn close_all_tabs(&mut self, cx: &mut Context<Self>) -> usize {
        let removed = self.tabs.remove_all();
        self.finish_bulk_close(removed, cx)
    }

    fn finish_bulk_close(&mut self, removed: Vec<Tab>, cx: &mut Context<Self>) -> usize {
        let count = removed.len();
        for tab in removed {
            Self::close_tab_panes(tab);
        }
        if count > 0 {
            self.reset_interaction();
            self.refresh();
            cx.notify();
        }
        count
    }

    /// Ctrl+Shift+W：分屏时关聚焦的窗格，否则关整个标签。返回有没有动。
    pub(crate) fn close_focused_pane_or_tab(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(index) = self.tabs.active_index() else {
            return false;
        };
        let Some(tab) = self.tabs.get_mut(index) else {
            return false;
        };
        if !tab.is_split() {
            return self.close_tab(index, cx);
        }
        let at = tab.active_pane.min(tab.panes.len() - 1);
        self.close_pane_index(at, cx)
    }

    /// 关掉活动标签里的第 `index` 个窗格：兄弟节点顶掉它的位置，焦点按 0.2.14 的规则挪。
    /// 只剩一个窗格时不动（要关整个标签用 `close_tab`）。
    pub(crate) fn close_pane_index(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        let Some(tab) = self.tabs.active_mut() else {
            return false;
        };
        let Some(mut pane) = tab.close_pane_at(index) else {
            return false;
        };
        pane.close();
        self.reset_interaction();
        self.refresh();
        cx.notify();
        true
    }

    /// 按标签编号取出一个标签（拖出窗口 / 合并到别的窗口用）：同一个 `Tab` 交出去，会话不断、不重连。
    /// 和 `take_active_tab` 不同，最后一个标签也能取（主窗口没有终端标签时显示主机页）。
    pub(crate) fn take_tab_by_id(
        &mut self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) -> Option<(usize, Tab)> {
        let index = self.tabs.items().iter().position(|tab| tab.id == tab_id)?;
        let tab = self.tabs.remove(index)?;
        self.reset_interaction();
        self.refresh();
        cx.notify();
        Some((index, tab))
    }

    /// 聚焦活动标签的第 `index` 个窗格（验证钩子用；鼠标点窗格走 `focus_pane`）。
    pub(crate) fn focus_pane_index(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        let Some(tab) = self.tabs.active() else {
            return false;
        };
        if index >= tab.panes.len() {
            return false;
        }
        self.focus_pane(index);
        cx.notify();
        true
    }

    /// 取出活动标签（搬到别的窗口用）；最后一个标签不取，返回 `None`。
    pub(crate) fn take_active_tab(&mut self, cx: &mut Context<Self>) -> Option<(usize, Tab)> {
        if self.tabs.len() <= 1 {
            return None;
        }
        let index = self.tabs.active_index()?;
        let tab = self.tabs.remove(index)?;
        self.reset_interaction();
        self.refresh();
        cx.notify();
        Some((index, tab))
    }

    /// 放回被取走的标签（新窗口没建起来时），回到原来的位置并设为活动。
    pub(crate) fn restore_tab(&mut self, index: usize, tab: Tab, cx: &mut Context<Self>) {
        let mut rest = self.tabs.remove_all();
        let at = index.min(rest.len());
        rest.insert(at, tab);
        for item in rest {
            self.tabs.push(item);
        }
        self.tabs.select(at);
        self.reset_interaction();
        self.refresh();
        cx.notify();
    }

    /// 接管另一个窗口合并过来的标签：同一个 `Tab` 直接接过来，会话不断、不重连。
    pub(crate) fn adopt_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tabs.push(tab);
        self.reset_interaction();
        self.refresh();
        cx.notify();
    }

    /// 新窗口接管一个已经存在的标签：先结束原有窗格，日志和会话不能留在新窗口里。
    pub(crate) fn replace_with_tab(&mut self, tab: Tab) {
        for old in self.tabs.remove_all() {
            Self::close_tab_panes(old);
        }
        self.tabs.push(tab);
        self.reset_interaction();
        self.refresh();
    }

    /// 离线演示标签（不连任何主机，键入内容本地回显）：只有调试窗口用。
    pub(crate) fn push_offline_tab(&mut self, title: &str, size: Size) -> usize {
        let pane = Pane::new(size, self.settings.scrollback, 1);
        let index = self.tabs.push(Tab::new(title.to_owned(), pane));
        self.reset_interaction();
        index
    }

    /// 调试驱动用：开一个离线演示标签（本地回显，不连任何主机），用来截光标样式这类外观。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_offline_tab(&mut self, cx: &mut Context<Self>) {
        let size = self.default_size;
        self.push_offline_tab("离线演示", size);
        cx.notify();
    }

    /// 调试驱动用：直接设选区（可见区坐标）。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_select(
        &mut self,
        anchor: (usize, usize),
        head: (usize, usize),
        cx: &mut Context<Self>,
    ) {
        self.selection = Some((anchor, head));
        cx.notify();
    }

    /// 调试驱动用：现在有没有非空的选区文字。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_selected_text(&self) -> String {
        self.selected_text()
    }

    /// 调试驱动用：这段文字在聚焦窗格里第一次出现的位置，返回它中间那一格的中心（窗口逻辑像素）。
    /// 按字符算列，所以只对前面没有宽字符的行准。
    pub(crate) fn dev_text_center(&self, text: &str) -> Option<(f32, f32)> {
        let tab = self.tabs.active()?;
        let geom = self.pane_geom.get(tab.active_pane)?;
        for (row, line) in tab.active().snapshot.rows.iter().enumerate() {
            if let Some(byte) = line.text.find(text) {
                let col = line.text[..byte].chars().count() + text.chars().count() / 2;
                return Some((
                    geom.left + (col as f32 + 0.5) * self.cell_width,
                    geom.top + (row as f32 + 0.5) * self.cell_height,
                ));
            }
        }
        None
    }

    /// 调试驱动用：弹着的右键菜单的条目（没弹是 `None`）。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_menu_entries(&self) -> Option<Vec<crate::terminal_menu::Entry>> {
        self.context_menu.as_ref().map(|open| open.entries.clone())
    }

    /// 调试驱动用：选菜单里的一项（和点它走同一个 `run_menu_item`）；置灰的项选不了。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_menu_pick(
        &mut self,
        label: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(open) = self.context_menu.as_ref() else {
            return Err("菜单没有弹出".to_owned());
        };
        let Some(entry) = open.entries.iter().find(|entry| entry.label == label) else {
            return Err("菜单里没有这一项".to_owned());
        };
        if !entry.enabled {
            return Err("这一项是置灰的".to_owned());
        }
        let (item, link) = (entry.item, open.link.clone());
        self.context_menu = None;
        self.run_menu_item(item, link.as_deref(), window, cx);
        Ok(())
    }

    /// 调试驱动用：关掉右键菜单（不选任何项），焦点回终端。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_menu_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.context_menu = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// 调试驱动用：「右键粘贴」设成指定值（走设置页开关同一个方法）。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_right_click_paste(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.settings.right_click_paste != on {
            self.toggle_right_click_paste(cx);
        }
    }

    /// 调试驱动用：键盘焦点现在在终端上吗。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_terminal_focused(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
    }

    /// 调试驱动用：给活动标签的窗格摆延迟值（`pane` 为空就是所有窗格；`sample` 为空是清掉）。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_latency_set(
        &mut self,
        sample: Option<latency::Sample>,
        pane: Option<usize>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let tab = self.tabs.active_mut().ok_or("没有标签")?;
        if pane.is_some_and(|at| at >= tab.panes.len()) {
            return Err("没有这个窗格".to_owned());
        }
        for (at, target) in tab.panes.iter_mut().enumerate() {
            if pane.is_none_or(|wanted| wanted == at) {
                target.dev_set_latency(sample);
            }
        }
        cx.notify();
        Ok(())
    }

    /// 调试驱动用：第 `tab` 个标签第 `pane` 个窗格的（延迟监视器登记的"看得见"数, 已有样本数）；
    /// 没有监视器是 `None`。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_latency_state(&self, tab: usize, pane: usize) -> Option<(usize, usize)> {
        let latency = self.tabs.get(tab)?.panes.get(pane)?.latency.as_ref()?;
        let monitor = latency.viewer.monitor();
        Some((monitor.visible_viewers(), monitor.snapshot().stats.len()))
    }

    /// 活动窗格的 shell 在它那条连接上排第几个、这条连接上现在有几个 shell（跟随终端目录用，见
    /// `zssh_core::ssh::cwd`）。没连上、不是 SSH 会话都是 `None`。
    pub(crate) fn active_shell_position(
        &self,
    ) -> Option<(Arc<zssh_core::ssh::Connection>, usize, usize)> {
        let tab = self.tabs.active()?;
        let pane = tab
            .panes
            .get(tab.active_pane.min(tab.panes.len().checked_sub(1)?))?;
        if pane.state != PaneState::Live {
            return None;
        }
        let connection = pane.factory.as_ref()?.connection();
        let mut seqs: Vec<u64> = self
            .tabs
            .items()
            .iter()
            .flat_map(|tab| tab.panes.iter())
            .filter(|other| other.state == PaneState::Live)
            .filter(|other| {
                other
                    .factory
                    .as_ref()
                    .is_some_and(|factory| Arc::ptr_eq(&factory.connection(), &connection))
            })
            .map(|other| other.shell_seq)
            .collect();
        seqs.sort_unstable();
        let ordinal = seqs.iter().position(|seq| *seq == pane.shell_seq)?;
        Some((connection, ordinal, seqs.len()))
    }

    /// 往活动窗格写一段内容（离线演示用；已连接的窗格会把它当键入发给远端）。
    pub(crate) fn write_active(&mut self, bytes: &[u8]) {
        if let Some(pane) = self.pane_mut() {
            pane.write(bytes);
            pane.refresh();
        }
    }

    /// 快捷命令栏和撰写栏往终端发内容：`all` 是这个标签的所有分屏窗格，否则只发聚焦的窗格。
    /// 和键盘输入同一条写路径（`Pane::write`），没连上的窗格丢弃输入；返回真正写出去几个。
    /// 标签开着输入广播时 `all` 由调用方算好（0.2.14 的 `sendCommandToTab`）。
    pub(crate) fn send_to_tab(&mut self, data: &str, all: bool, cx: &mut Context<Self>) -> usize {
        self.selection = None;
        let Some(tab) = self.tabs.active_mut() else {
            return 0;
        };
        let targets: Vec<usize> = if all {
            (0..tab.panes.len()).collect()
        } else {
            vec![tab.active_pane.min(tab.panes.len() - 1)]
        };
        let mut sent = 0;
        for index in targets {
            let pane = &mut tab.panes[index];
            if pane.state != PaneState::Live {
                continue;
            }
            if pane.pump().display_offset() != 0 {
                pane.pump_mut().scroll_to_bottom();
            }
            pane.write(data.as_bytes());
            sent += 1;
        }
        cx.notify();
        sent
    }

    /// 活动标签有几个窗格（分屏数）。
    pub(crate) fn pane_count(&self) -> usize {
        self.tabs.active().map_or(0, |tab| tab.panes.len())
    }

    /// 活动标签的编号（AI 对话按标签各自一份）。
    pub(crate) fn active_tab_id(&self) -> Option<u64> {
        self.tabs.active().map(|tab| tab.id)
    }

    /// 聚焦窗格的 AI 终端编号（V2-G10b）：AI 把「这台主机的焦点终端」认成它。没连上 / 离线窗格没有。
    pub(crate) fn active_terminal_id(&self) -> Option<String> {
        self.pane()
            .filter(|pane| pane.state == PaneState::Live)
            .and_then(|pane| pane.terminal_id.clone())
    }

    /// 切到这个终端所在的标签（并聚焦那个窗格）；找不到返回 `false`。
    pub(crate) fn select_terminal(&mut self, terminal_id: &str, cx: &mut Context<Self>) -> bool {
        let found = self
            .tabs
            .items()
            .iter()
            .enumerate()
            .find_map(|(index, tab)| {
                tab.panes
                    .iter()
                    .position(|pane| pane.terminal_id.as_deref() == Some(terminal_id))
                    .map(|pane| (index, pane))
            });
        let Some((index, pane)) = found else {
            return false;
        };
        self.select_tab(index, cx);
        self.focus_pane(pane);
        true
    }

    /// 切到这台主机的一个标签：优先活动标签，其次第一个。「查看终端」在目标还没连上时用它。
    pub(crate) fn select_host_tab(&mut self, host_id: i64, cx: &mut Context<Self>) -> bool {
        let index = self
            .tabs
            .active_index()
            .filter(|index| {
                self.tabs.items()[*index]
                    .panes
                    .iter()
                    .any(|p| p.host_id == Some(host_id))
            })
            .or_else(|| {
                self.tabs
                    .items()
                    .iter()
                    .position(|tab| tab.panes.iter().any(|p| p.host_id == Some(host_id)))
            });
        match index {
            Some(index) => {
                self.select_tab(index, cx);
                true
            }
            None => false,
        }
    }

    /// 活动标签每个窗格的屏幕文字（核对「发到了哪几个窗格」用）。
    pub(crate) fn pane_texts(&self) -> Vec<String> {
        self.tabs
            .active()
            .map(|tab| {
                tab.panes
                    .iter()
                    .map(|pane| {
                        pane.snapshot
                            .rows
                            .iter()
                            .map(|row| row.text.as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 聚焦窗格的下标（活动标签）。
    pub(crate) fn focused_pane_index(&self) -> Option<usize> {
        self.tabs
            .active()
            .map(|tab| tab.active_pane.min(tab.panes.len() - 1))
    }

    /// 所有标签所有窗格现在存着多少行回滚。
    pub(crate) fn history_sizes(&self) -> Vec<usize> {
        self.tabs
            .items()
            .iter()
            .flat_map(|tab| tab.panes.iter().map(|pane| pane.pump().history_size()))
            .collect()
    }

    /// 终端实际用的字体名（验证钩子核对「栈里没装的字体会回退」）。
    pub(crate) fn resolved_font(&mut self, cx: &App) -> String {
        self.mono_family(cx).to_string()
    }

    /// 所有标签所有窗格现在的回滚上限（验证钩子核对「已开的窗格也用了新设置」）。
    pub(crate) fn history_limits(&self) -> Vec<usize> {
        self.tabs
            .items()
            .iter()
            .flat_map(|tab| tab.panes.iter().map(|pane| pane.pump().history_limit()))
            .collect()
    }

    /// 活动标签所有分割点的 (编号, 方向, 比例)。
    pub(crate) fn split_ratios(&self) -> Vec<(u64, Axis, f32)> {
        self.tabs
            .active()
            .map(|tab| tab.tree.splits())
            .unwrap_or_default()
    }

    /// 活动标签上一帧的窗格布局，一个窗格一行：下标、是否聚焦、矩形、行列数；再加分隔条。
    /// 只写数字，不写屏幕内容（验证钩子写进日志用）。
    pub(crate) fn pane_report(&self) -> Vec<String> {
        let Some(tab) = self.tabs.active() else {
            return Vec::new();
        };
        let mut lines = Vec::new();
        for (index, pane) in tab.panes.iter().enumerate() {
            let rect = self.layout.rect_of(pane.id).unwrap_or_default();
            lines.push(format!(
                "pane {index}{} rect=({:.0},{:.0},{:.0},{:.0}) grid={}x{}",
                if index == tab.active_pane {
                    " focus"
                } else {
                    ""
                },
                rect.x,
                rect.y,
                rect.w,
                rect.h,
                pane.snapshot.size.cols,
                pane.snapshot.size.rows,
            ));
        }
        for divider in &self.layout.dividers {
            let rect = divider.rect;
            lines.push(format!(
                "divider {:?} rect=({:.0},{:.0},{:.0},{:.0}) origin=({:.0},{:.0})",
                divider.axis,
                rect.x,
                rect.y,
                rect.w,
                rect.h,
                self.area_origin.0,
                self.area_origin.1
            ));
        }
        lines
    }

    /// 终端区域（窗口坐标）的左上角和大小：验证钩子按它算要点的坐标。
    pub(crate) fn area_rect(&self) -> Option<(f32, f32, f32, f32)> {
        self.area.get().map(|bounds| {
            (
                f32::from(bounds.origin.x),
                f32::from(bounds.origin.y),
                f32::from(bounds.size.width),
                f32::from(bounds.size.height),
            )
        })
    }

    /// 核对活动标签的布局铺满终端区域、没有重叠，且每个窗格的终端尺寸和它的矩形一致
    /// （窗口里真的重排过，不是只改了数字）。
    pub(crate) fn check_layout(&self) -> Result<(), String> {
        let Some(bounds) = self.area.get() else {
            return Err("还没有量到终端区域".to_owned());
        };
        let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let tab = self.tabs.active().ok_or("没有标签")?;
        // 上一帧画的布局要和树现在的比例对得上：拖完分隔条、关完窗格之后，下一帧才会重排。
        if split_tree::compute_layout(&tab.tree, w, h) != self.layout {
            return Err("窗口还没有按最新的分屏树重排".to_owned());
        }
        split_tree::check_tiling(&self.layout, w, h)?;
        for (index, (pane, geom)) in tab.panes.iter().zip(&self.pane_geom).enumerate() {
            let want = Size {
                rows: geom.rows,
                cols: geom.cols,
            };
            if pane.snapshot.size != want {
                return Err(format!(
                    "窗格 {index} 的终端是 {}x{}，矩形算出来是 {}x{}",
                    pane.snapshot.size.cols, pane.snapshot.size.rows, want.cols, want.rows
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn toggle_broadcast(&mut self, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.active_mut() {
            tab.broadcast = !tab.broadcast;
            cx.notify();
        }
    }

    /// Ctrl+Shift+D（左右，`Axis::X`）/ Ctrl+Shift+E（上下，`Axis::Y`）：把聚焦的窗格一分为二，
    /// 新窗格在右 / 下并拿到焦点。可以接着拆，没有个数上限（同 0.2.14 的 `splitAt`）。
    /// 新窗格复用聚焦窗格的已认证连接再开一个 shell；断线的连接已经死了，重新连。
    pub(crate) fn split_active(&mut self, axis: Axis, cx: &mut Context<Self>) -> bool {
        let history = self.settings.scrollback;
        let Some(tab) = self.tabs.active_mut() else {
            return false;
        };
        let source = tab.active();
        let size = source.snapshot.size;
        let host_id = source.host_id;
        let quick = source.quick.clone();
        let blank = source.state == PaneState::Empty;
        let factory = if source.state == PaneState::Live {
            source.factory.clone()
        } else {
            None
        };
        // 新窗格先按一半的尺寸申请 PTY，下一帧排版会按实际矩形再对一次。
        let half = match axis {
            Axis::X => Size {
                rows: size.rows,
                cols: (size.cols / 2).max(20),
            },
            Axis::Y => Size {
                rows: (size.rows / 2).max(2),
                cols: size.cols,
            },
        };
        let mut pane = if blank {
            Pane::placeholder(half, history, 1, None, PaneState::Empty)
        } else if let Some(spec) = quick.clone() {
            Pane::start_quick(spec, factory, half, history, 1)
        } else {
            Pane::start_ssh(factory, host_id, half, history, 1)
        };
        if host_id.is_none() && quick.is_none() && !blank {
            for i in 0..5 {
                pane.write(format!("split pane line {i}\r\n").as_bytes());
            }
            pane.fed = 5;
        }
        tab.split_active(axis, pane);
        self.reset_interaction();
        cx.notify();
        true
    }

    /// 调试窗口的分屏按钮：没分屏就左右拆，已经分屏就收掉最后一个窗格。
    pub(crate) fn toggle_split(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.active() else {
            return;
        };
        if tab.is_split() {
            let last = tab.panes.len() - 1;
            self.close_pane_index(last, cx);
        } else {
            self.split_active(Axis::X, cx);
        }
    }

    /// 重连一个标签里所有已断开的窗格（标签右键「重新连接」）；返回重连了几个。
    pub(crate) fn reconnect_tab(&mut self, index: usize, cx: &mut Context<Self>) -> usize {
        let Some(tab) = self.tabs.get(index) else {
            return 0;
        };
        let targets: Vec<usize> = tab
            .panes
            .iter()
            .enumerate()
            .filter(|(_, pane)| pane.state == PaneState::Disconnected && pane.is_session())
            .map(|(at, _)| at)
            .collect();
        self.status_text = "正在重新连接…".to_owned();
        for &at in &targets {
            self.reconnect_pane(index, at);
        }
        if !targets.is_empty() {
            self.reset_interaction();
            self.refresh();
            cx.notify();
        }
        targets.len()
    }

    /// 断开一个标签里所有还连着的窗格（标签右键「断开连接」，分屏的每个窗格都断）；标签本身留着，
    /// 状态变成已断开，可以「重新连接」。返回断开了几个窗格。
    /// 给这个标签聚焦的窗格发 Break（串口）。窗格已经断开返回 `false`。
    pub(crate) fn send_break_tab(&mut self, index: usize) -> bool {
        let Some(tab) = self.tabs.get_mut(index) else {
            return false;
        };
        let pane = tab.active_mut();
        let live = pane.state == PaneState::Live;
        pane.send_break();
        live
    }

    /// 活动标签聚焦的窗格发 Break。
    pub(crate) fn send_break_active(&mut self) -> bool {
        let Some(tab) = self.tabs.active_mut() else {
            return false;
        };
        let pane = tab.active_mut();
        let live = pane.state == PaneState::Live;
        pane.send_break();
        live
    }

    pub(crate) fn disconnect_tab(&mut self, index: usize, cx: &mut Context<Self>) -> usize {
        let Some(tab) = self.tabs.get_mut(index) else {
            return 0;
        };
        let count = tab.disconnect_sessions();
        if count > 0 {
            self.status_text = "已断开连接".to_owned();
            self.reset_interaction();
            self.refresh();
            cx.notify();
        }
        count
    }

    /// 重建一个窗格的连接：先收掉旧窗格（日志和通道都要结束，否则新旧两条会同时跑），
    /// 之前在记录就继续记到新文件。只在用户操作时调用，不做自动重连。
    fn reconnect_pane(&mut self, tab_index: usize, pane_index: usize) {
        let history = self.settings.scrollback;
        let Some(pane) = self
            .tabs
            .get_mut(tab_index)
            .and_then(|tab| tab.panes.get_mut(pane_index))
        else {
            return;
        };
        if !pane.is_session() {
            return;
        }
        let (host_id, quick) = (pane.host_id, pane.quick.clone());
        let size = pane.snapshot.size;
        let logging = pane.pump().has_log();
        // 分屏树的叶子认窗格编号：换掉整个窗格时沿用旧编号，位置和比例都不变。
        let id = pane.id;
        pane.close();
        *pane = match quick {
            Some(spec) => Pane::start_quick(spec, None, size, history, 1),
            None => Pane::start_ssh(None, host_id, size, history, 1),
        };
        pane.id = id;
        pane.refresh();
        if logging {
            self.status_text = start_log(pane);
        }
    }

    /// Ctrl+Shift+R：重连当前窗格；离线窗格只提示。
    pub(crate) fn reconnect(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.active_index() else {
            return;
        };
        let (pane_index, is_session) = match self.tabs.get(index) {
            Some(tab) => (tab.active_pane, tab.active().is_session()),
            None => return,
        };
        if !is_session {
            self.status_text = "离线标签没有可重连的会话".to_owned();
            cx.notify();
            return;
        }
        // 先写提示：之前在记录日志时，新窗格续记会把状态改成日志路径，那个更有用。
        self.status_text = "正在重新连接…".to_owned();
        self.reconnect_pane(index, pane_index);
        self.reset_interaction();
        self.refresh();
        cx.notify();
    }

    // ---- 高亮 ----

    /// 取后台扫描结果；返回是否有新结果。
    fn poll_highlight(&mut self) -> bool {
        let Some(outcome) = self.highlight.engine.poll() else {
            return false;
        };
        if !self.settings.highlight_enabled {
            return false;
        }
        let mut spans: Vec<Vec<Span>> = Vec::new();
        for span in &outcome.spans {
            if spans.len() <= span.row {
                spans.resize(span.row + 1, Vec::new());
            }
            spans[span.row].push(*span);
        }
        self.highlight.spans = spans;
        let notice = outcome.notices.first().cloned();
        let error = outcome
            .statuses
            .iter()
            .find_map(|status| status.error.clone())
            .map(|error| format!("规则错误：{error}"));
        self.highlight.status = notice.or(error);
        true
    }

    /// 发一次扫描；只有可见行或规则变了才发。
    fn request_highlight_scan(&mut self, rows: usize) {
        if !self.settings.highlight_enabled {
            self.highlight.spans.clear();
            return;
        }
        if self.highlight.rules.is_empty() {
            return;
        }
        let Some(tab) = self.tabs.active() else {
            self.highlight.spans.clear();
            return;
        };
        let revision = self.highlight.revision;
        let tab_id = tab.id;
        let active_pane = tab.active_pane;
        if self.highlight.key.map(|key| (key.0, key.1)) != Some((tab_id, active_pane)) {
            self.highlight.spans.clear();
        }
        let pane = tab.active();
        let key = (
            tab_id,
            active_pane,
            revision,
            pane.pump().ledger().raw,
            pane.pump().display_offset(),
            rows,
        );
        if self.highlight.key == Some(key) {
            return;
        }
        let lines = pane
            .snapshot
            .rows
            .iter()
            .map(crate::highlight::line_text)
            .collect();
        let HighlightUi { engine, rules, .. } = &mut self.highlight;
        engine.request(revision, rules, lines);
        self.highlight.key = Some(key);
    }

    /// 直接加一条规则；高亮面板的输入框和验证钩子共用。
    pub(crate) fn add_highlight_rule(
        &mut self,
        pattern: String,
        case_sensitive: bool,
        cx: &mut Context<Self>,
    ) {
        let id = self.highlight.next_id;
        self.highlight.next_id += 1;
        self.highlight
            .rules
            .push(HighlightRule::new(id, pattern, case_sensitive));
        self.highlight.bump();
        self.persist_highlight_rules();
        cx.notify();
    }

    fn persist_highlight_rules(&mut self) {
        self.settings.highlight_rules = Some(self.highlight.rules.clone());
        self.settings.save();
    }

    pub(crate) fn toggle_highlight_rule(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(rule) = self.highlight.rules.iter_mut().find(|rule| rule.id == id) {
            rule.enabled = !rule.enabled;
            self.highlight.bump();
            self.persist_highlight_rules();
            cx.notify();
        }
    }

    /// 改一条规则（设置页的规则编辑器共用）：改完规则集的修订号加一，后台线程重新编译，并存盘。
    fn edit_highlight_rule(
        &mut self,
        id: u64,
        edit: impl FnOnce(&mut HighlightRule),
        cx: &mut Context<Self>,
    ) {
        if let Some(rule) = self.highlight.rules.iter_mut().find(|rule| rule.id == id) {
            edit(rule);
            self.highlight.bump();
            self.persist_highlight_rules();
            cx.notify();
        }
    }

    pub(crate) fn set_highlight_pattern(
        &mut self,
        id: u64,
        pattern: String,
        cx: &mut Context<Self>,
    ) {
        self.edit_highlight_rule(id, |rule| rule.pattern = pattern, cx);
    }

    pub(crate) fn set_highlight_color(&mut self, id: u64, color: u32, cx: &mut Context<Self>) {
        self.edit_highlight_rule(id, |rule| rule.set_color(color), cx);
    }

    pub(crate) fn set_highlight_regex(&mut self, id: u64, regex: bool, cx: &mut Context<Self>) {
        self.edit_highlight_rule(id, |rule| rule.regex = regex, cx);
    }

    pub(crate) fn set_highlight_case(
        &mut self,
        id: u64,
        case_sensitive: bool,
        cx: &mut Context<Self>,
    ) {
        self.edit_highlight_rule(id, |rule| rule.case_sensitive = case_sensitive, cx);
    }

    pub(crate) fn set_highlight_background(&mut self, id: u64, on: bool, cx: &mut Context<Self>) {
        self.edit_highlight_rule(id, |rule| rule.set_background(on), cx);
    }

    /// 添加一条空规则（颜色按现有条数轮换）；返回新规则的编号，设置页据此建它的输入框。
    /// 规则数到上限（和同步校验一致）不再加。空模式的规则不生效，也不报错，等用户填。
    pub(crate) fn add_blank_highlight_rule(&mut self, cx: &mut Context<Self>) -> Option<u64> {
        if self.highlight.rules.len() >= MAX_HIGHLIGHT_RULES {
            return None;
        }
        let id = self.highlight.next_id;
        self.highlight.next_id += 1;
        let mut rule = HighlightRule::new(id, String::new(), false);
        rule.foreground = crate::highlight::HIGHLIGHT_COLORS
            [self.highlight.rules.len() % crate::highlight::HIGHLIGHT_COLORS.len()]
        .0;
        self.highlight.rules.push(rule);
        self.highlight.bump();
        self.persist_highlight_rules();
        cx.notify();
        Some(id)
    }

    /// 补上还没有的常用规则（按模式对比，已有的不重复加）；返回补了几条。
    pub(crate) fn add_sample_highlight_rules(&mut self, cx: &mut Context<Self>) -> usize {
        let missing = crate::highlight::missing_samples(&self.highlight.rules);
        let room = MAX_HIGHLIGHT_RULES.saturating_sub(self.highlight.rules.len());
        let mut added = 0;
        for mut rule in missing.into_iter().take(room) {
            rule.id = self.highlight.next_id;
            self.highlight.next_id += 1;
            self.highlight.rules.push(rule);
            added += 1;
        }
        if added > 0 {
            self.highlight.bump();
            self.persist_highlight_rules();
            cx.notify();
        }
        added
    }

    pub(crate) fn remove_highlight_rule(&mut self, id: u64, cx: &mut Context<Self>) {
        self.highlight.rules.retain(|rule| rule.id != id);
        self.highlight.bump();
        self.persist_highlight_rules();
        cx.notify();
    }

    pub(crate) fn reset_highlight_rules(&mut self, cx: &mut Context<Self>) {
        self.highlight.rules = crate::highlight::default_rules();
        self.highlight.next_id = 8;
        self.highlight.bump();
        self.persist_highlight_rules();
        cx.notify();
    }

    // ---- 主题与设置（外壳的设置页和调试窗口共用） ----

    pub(crate) fn toggle_theme(&mut self, cx: &mut Context<Self>) {
        let dark = !crate::app_theme::is_dark(cx);
        self.set_theme(dark, cx);
    }

    pub(crate) fn set_theme(&mut self, dark: bool, cx: &mut Context<Self>) {
        self.settings.follow_system = false;
        self.settings.dark = dark;
        crate::app_theme::set_mode(dark, cx);
        self.settings.save();
        cx.notify();
    }

    pub(crate) fn set_theme_auto(&mut self, cx: &mut Context<Self>) {
        self.settings.follow_system = true;
        let dark = crate::settings::resolve_dark(
            true,
            self.settings.dark,
            zssh_platform_windows::theme::apps_use_light_theme(),
        );
        self.settings.dark = dark;
        crate::app_theme::set_mode(dark, cx);
        self.settings.save();
        cx.notify();
    }

    /// 「自动」模式下周期性对齐系统主题；系统值没变时什么都不做。
    fn sync_system_theme(&mut self, cx: &mut Context<Self>) {
        if !self.settings.follow_system {
            return;
        }
        let Some(dark) = zssh_platform_windows::theme::apps_use_light_theme().map(|light| !light)
        else {
            return;
        };
        if dark == crate::app_theme::is_dark(cx) {
            return;
        }
        self.settings.dark = dark;
        crate::app_theme::set_mode(dark, cx);
        self.settings.save();
        cx.notify();
    }

    /// 把设置里的皮肤 / 终端配色写进全局主题（界面和终端一起变）。
    fn apply_look(&self, cx: &mut Context<Self>) {
        crate::app_theme::set_look(self.settings.look(), cx);
    }

    /// 换皮肤：`dark` 选的是深色那一栏还是浅色那一栏；明暗对不上的 id 不收。
    /// 当前明暗正在用这一栏时界面立刻跟着变，另一栏记下来，切明暗时生效（同 0.2.14）。
    pub(crate) fn set_skin(&mut self, dark: bool, id: &str, cx: &mut Context<Self>) {
        if crate::app_theme::skin(id).is_none_or(|skin| skin.dark != dark) {
            return;
        }
        let slot = if dark {
            &mut self.settings.skin_dark
        } else {
            &mut self.settings.skin_light
        };
        if slot == id {
            return;
        }
        *slot = id.to_owned();
        self.apply_look(cx);
        self.settings.save();
        cx.notify();
    }

    /// 终端配色：`skin`（跟随皮肤）或某个皮肤的 id；别的值不收。
    pub(crate) fn set_terminal_theme(&mut self, id: &str, cx: &mut Context<Self>) {
        if crate::app_theme::normalize_terminal_theme(id) != id
            || self.settings.terminal_theme == id
        {
            return;
        }
        self.settings.terminal_theme = id.to_owned();
        self.apply_look(cx);
        self.settings.save();
        cx.notify();
    }

    /// 设置里的字体栈（输入框每次变化都会走这里）；空的 / 太长的退回默认。
    pub(crate) fn set_font_family(&mut self, text: &str, cx: &mut Context<Self>) {
        let stack = crate::settings::sanitize_font_family(text);
        if stack == self.settings.font_family {
            return;
        }
        self.settings.font_family = stack;
        self.font_cache = None;
        self.settings.save();
        cx.notify();
    }

    /// 下拉里选了一个常见字体：写成「它 + 后备」，并把输入框里的字同步过去。
    pub(crate) fn pick_font(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.font_custom = false;
        let stack = crate::fonts::stack_for_choice(name);
        self.font_input
            .update(cx, |state, cx| state.set_value(stack.clone(), window, cx));
        self.set_font_family(&stack, cx);
        cx.notify();
    }

    /// 下拉里选了「自定义…」：显示输入框，字体栈先不动。
    pub(crate) fn pick_custom_font(&mut self, cx: &mut Context<Self>) {
        self.font_custom = true;
        cx.notify();
    }

    pub(crate) fn font_custom_shown(&self) -> bool {
        self.font_custom || !crate::fonts::is_common(&self.settings.font_family)
    }

    pub(crate) fn font_input(&self) -> &Entity<InputState> {
        &self.font_input
    }

    pub(crate) fn scrollback_slider(&self) -> &Entity<SliderState> {
        &self.scrollback_slider
    }

    /// 终端实际用的字体名：设置里的字体栈里第一个本机装了的，一个都没有就用主题自带的等宽字体。
    fn mono_family(&mut self, cx: &App) -> SharedString {
        if let Some((stack, family)) = &self.font_cache
            && *stack == self.settings.font_family
        {
            return family.clone();
        }
        let installed = crate::fonts::installed_fonts(cx);
        let theme_mono = cx.theme().mono_font_family.to_string();
        let family: SharedString =
            crate::fonts::resolve_family(&self.settings.font_family, installed, &theme_mono).into();
        self.font_cache = Some((self.settings.font_family.clone(), family.clone()));
        family
    }

    /// 回滚行数（1000～100000）：所有标签里已经开着的窗格立刻按新上限（调小会丢掉最旧的行，
    /// 调大只是以后能攒更多，同 xterm 的 `scrollback` 选项），之后新开的窗格也用它。
    pub(crate) fn set_scrollback(&mut self, lines: usize, cx: &mut Context<Self>) {
        let lines = lines.clamp(MIN_SCROLLBACK, MAX_SCROLLBACK);
        if lines == self.settings.scrollback {
            return;
        }
        self.settings.scrollback = lines;
        for tab in self.tabs.items_mut() {
            for pane in &mut tab.panes {
                pane.set_history(lines);
            }
        }
        self.settings.save();
        cx.notify();
    }

    pub(crate) fn adjust_font(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.settings.font_size =
            (self.settings.font_size + delta).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
        self.settings.save();
        cx.notify();
    }

    pub(crate) fn adjust_line_height(&mut self, delta: f32, cx: &mut Context<Self>) {
        let next = ((self.settings.line_height + delta) * 100.0).round() / 100.0;
        self.settings.line_height = next.clamp(MIN_LINE_HEIGHT, MAX_LINE_HEIGHT);
        self.settings.save();
        cx.notify();
    }

    pub(crate) fn set_cursor_shape(&mut self, shape: CursorShape, cx: &mut Context<Self>) {
        self.settings.cursor = shape;
        self.settings.save();
        cx.notify();
    }

    pub(crate) fn toggle_cursor_blink(&mut self, cx: &mut Context<Self>) {
        self.settings.cursor_blink = !self.settings.cursor_blink;
        // 刚改完光标要看得见：闪烁相位回到「亮」。
        self.cursor_blink = BlinkClock::new();
        self.settings.save();
        cx.notify();
    }

    /// 「终端里自动切到英文输入」：关掉时焦点进终端不再动输入法；已经在英文区的话马上还回去。
    /// 「在终端右上角显示延迟」：关着时窗格也不再登记成看得见，后台测量随之暂停。
    pub(crate) fn toggle_show_latency(&mut self, cx: &mut Context<Self>) {
        self.settings.show_latency = !self.settings.show_latency;
        self.settings.save();
        cx.notify();
    }

    pub(crate) fn toggle_ime_english(&mut self, cx: &mut Context<Self>) {
        self.settings.ime_english = !self.settings.ime_english;
        if !self.settings.ime_english {
            crate::ime::apply(&self.ime_key, false);
        }
        self.settings.save();
        cx.notify();
    }

    pub(crate) fn toggle_copy_on_select(&mut self, cx: &mut Context<Self>) {
        self.settings.copy_on_select = !self.settings.copy_on_select;
        self.settings.save();
        cx.notify();
    }

    pub(crate) fn toggle_right_click_paste(&mut self, cx: &mut Context<Self>) {
        self.settings.right_click_paste = !self.settings.right_click_paste;
        self.settings.save();
        cx.notify();
    }

    pub(crate) fn toggle_confirm_multiline(&mut self, cx: &mut Context<Self>) {
        self.settings.confirm_multiline = !self.settings.confirm_multiline;
        self.settings.save();
        cx.notify();
    }

    pub(crate) fn toggle_highlight_enabled(&mut self, cx: &mut Context<Self>) {
        self.settings.highlight_enabled = !self.settings.highlight_enabled;
        if !self.settings.highlight_enabled {
            self.highlight.spans.clear();
            self.highlight.key = None;
        } else if self.highlight.rules.is_empty() {
            // 打开时一条规则都没有（用户全删了）：补上常用规则，同 0.2.14，免得开了开关什么也不标。
            self.add_sample_highlight_rules(cx);
        }
        self.settings.save();
        cx.notify();
    }

    pub(crate) fn cycle_bracketed_paste(&mut self, cx: &mut Context<Self>) {
        self.settings.bracketed_paste = self.settings.bracketed_paste.next();
        self.settings.save();
        cx.notify();
    }

    pub(crate) fn toggle_start_hidden(&mut self, cx: &mut Context<Self>) {
        self.settings.start_hidden = !self.settings.start_hidden;
        self.settings.save();
        cx.notify();
    }

    pub(crate) fn reset_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings = UiSettings::default();
        self.settings.highlight_rules = Some(self.highlight.rules.clone());
        let lines = self.settings.scrollback;
        for tab in self.tabs.items_mut() {
            for pane in &mut tab.panes {
                pane.set_history(lines);
            }
        }
        self.font_cache = None;
        self.font_custom = false;
        let stack = self.settings.font_family.clone();
        self.font_input
            .update(cx, |state, cx| state.set_value(stack, window, cx));
        self.scrollback_slider
            .update(cx, |state, cx| state.set_value(lines as f32, window, cx));
        self.apply_look(cx);
        crate::app_theme::set_mode(self.settings.dark, cx);
        self.settings.save();
        cx.notify();
    }

    // ---- 键盘 ----

    /// 按键编码后直接发给活动窗格的会话（离线演示窗格会本地回显）。
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // 按键从子元素冒泡上来：别的输入框有焦点时终端不能抢键。
        if !self.focus.is_focused(window) {
            return;
        }
        if self.tabs.is_empty() {
            return;
        }
        let keystroke = &event.keystroke;
        let ctrl = keystroke.modifiers.control;
        let shift = keystroke.modifiers.shift;
        let key = keystroke.key.as_str();
        // 打字时光标先亮着，停下来才闪（和常见终端一致）。
        self.cursor_blink.reset();

        // Ctrl+Shift+R 重连当前 SSH 窗格；离线标签只提示。
        if ctrl && shift && key.eq_ignore_ascii_case("r") {
            self.reconnect(cx);
            return;
        }

        // Ctrl+= / Ctrl+- / Ctrl+0 缩放终端字号。
        if ctrl {
            match key {
                "=" | "+" => {
                    self.settings.font_size = (self.settings.font_size + 1.0).min(MAX_FONT_SIZE);
                    self.settings.save();
                    cx.notify();
                    return;
                }
                "-" => {
                    self.settings.font_size = (self.settings.font_size - 1.0).max(MIN_FONT_SIZE);
                    self.settings.save();
                    cx.notify();
                    return;
                }
                "0" => {
                    self.settings.font_size = DEFAULT_FONT_SIZE;
                    self.settings.save();
                    cx.notify();
                    return;
                }
                _ => {}
            }
        }

        // Shift+PageUp/PageDown 翻页，Shift+Home/End 到顶/到底。
        if shift {
            let rows = self
                .pane()
                .map(|pane| pane.snapshot.size.rows as i32)
                .unwrap_or(0);
            if let Some(pane) = self.pane_mut() {
                match key {
                    "pageup" => {
                        pane.pump_mut().scroll(rows);
                        pane.refresh();
                        cx.notify();
                        return;
                    }
                    "pagedown" => {
                        pane.pump_mut().scroll(-rows);
                        pane.refresh();
                        cx.notify();
                        return;
                    }
                    "home" => {
                        pane.pump_mut().scroll_to_top();
                        pane.refresh();
                        cx.notify();
                        return;
                    }
                    "end" => {
                        pane.pump_mut().scroll_to_bottom();
                        pane.refresh();
                        cx.notify();
                        return;
                    }
                    _ => {}
                }
            }
        }

        // 普通输入先回到当前行，避免在历史里输入。
        if let Some(pane) = self.pane_mut()
            && pane.pump().display_offset() != 0
        {
            pane.pump_mut().scroll_to_bottom();
            pane.refresh();
        }

        // 剪贴板快捷键与现有前端保持一致：
        // - Ctrl+V / Ctrl+Shift+V 粘贴；
        // - Ctrl+C 有选区才复制，没有选区继续发 ^C；
        // - Ctrl+Shift+C 没有选区时吞掉，不能把 ^C 发给远端。
        if ctrl && key.eq_ignore_ascii_case("v") {
            self.request_paste(cx);
            return;
        }
        if ctrl && key.eq_ignore_ascii_case("c") {
            let selected = self.selected_text();
            match crate::selection::copy_key_action(&selected, shift) {
                crate::selection::CopyKey::Copy => {
                    self.copy_selection(cx);
                    // 和 0.2.14 一致：复制后清掉选区，下一次 Ctrl+C 才能中断命令。
                    self.selection = None;
                    cx.notify();
                    return;
                }
                crate::selection::CopyKey::Swallow => return,
                crate::selection::CopyKey::Interrupt => {}
            }
        }
        if shift && key.eq_ignore_ascii_case("insert") {
            self.request_paste(cx);
            return;
        }

        let Some(app_cursor) = self.pane().map(|pane| pane.pump().app_cursor()) else {
            return;
        };
        let input = KeyInput {
            key: &keystroke.key,
            key_char: keystroke.key_char.as_deref(),
            ctrl: keystroke.modifiers.control,
            alt: keystroke.modifiers.alt,
            shift: keystroke.modifiers.shift,
            app_cursor,
        };
        if let Some(bytes) = encode_key(input) {
            self.send_bytes(&bytes);
            cx.notify();
        }
    }

    // ---- 鼠标 ----

    /// 点在哪个窗格里（含四周的内边距）：点一下就聚焦它。
    fn pane_at(&self, position: ::gpui::Point<::gpui::Pixels>) -> Option<usize> {
        let (x, y) = (f32::from(position.x), f32::from(position.y));
        self.pane_geom
            .iter()
            .position(|geom| geom.rect.contains(x, y))
    }

    /// 命中哪个窗格的哪一格；每个窗格按自己文字区的左上角换算。
    fn cell_at(&self, position: ::gpui::Point<::gpui::Pixels>) -> Option<(usize, usize, usize)> {
        let (x, y) = (f32::from(position.x), f32::from(position.y));
        let tab = self.tabs.active()?;
        for (index, geom) in self.pane_geom.iter().enumerate() {
            let (local_x, local_y) = (x - geom.left, y - geom.top);
            if local_x < 0.0 || local_y < 0.0 {
                continue;
            }
            let col = (local_x / self.cell_width.max(1.0)).floor() as usize;
            let row = (local_y / self.cell_height.max(1.0)).floor() as usize;
            let Some(pane) = tab.panes.get(index) else {
                continue;
            };
            if col >= geom.cols || row >= pane.snapshot.size.rows {
                continue;
            }
            return Some((index, row, col));
        }
        None
    }

    fn link_at(&self, position: ::gpui::Point<::gpui::Pixels>) -> Option<Arc<str>> {
        let (pane_ix, row, col) = self.cell_at(position)?;
        let row_snapshot = self
            .tabs
            .active()?
            .panes
            .get(pane_ix)?
            .snapshot
            .rows
            .get(row)?;
        if let Some(link) = &row_snapshot.cells.get(col)?.hyperlink {
            return Some(link.clone());
        }
        crate::links::find_plain_links(row_snapshot)
            .into_iter()
            .find(|link| col >= link.start_col && col <= link.end_col)
            .map(|link| Arc::from(link.url))
    }

    /// 按下的位置落在分隔条上（命中区 8px，比可见线宽）：单击开始拖，双击恢复平分
    /// （0.2.14 的 `onDoubleClick`）。返回有没有被分隔条接走。
    fn start_split_drag(
        &mut self,
        position: ::gpui::Point<::gpui::Pixels>,
        click_count: usize,
    ) -> bool {
        let Some(tab) = self.tabs.active() else {
            return false;
        };
        if !tab.is_split() {
            return false;
        }
        let rel_x = f32::from(position.x) - self.area_origin.0;
        let rel_y = f32::from(position.y) - self.area_origin.1;
        let Some(divider) = split_tree::divider_at(&self.layout, rel_x, rel_y).copied() else {
            return false;
        };
        if click_count >= 2 {
            if let Some(tab) = self.tabs.active_mut() {
                tab.tree.set_ratio(divider.split_id, 0.5);
            }
            return true;
        }
        self.dragging_split = Some(divider.split_id);
        true
    }

    /// 拖动中：指针位置换算成这条分隔条的比例（两边留出最小窗格，再夹到 0.1～0.9）。
    fn update_split(&mut self, position: ::gpui::Point<::gpui::Pixels>) {
        let Some(split_id) = self.dragging_split else {
            return;
        };
        let Some(divider) = self
            .layout
            .dividers
            .iter()
            .find(|divider| divider.split_id == split_id)
            .copied()
        else {
            return;
        };
        let pointer = match divider.axis {
            Axis::X => f32::from(position.x) - self.area_origin.0,
            Axis::Y => f32::from(position.y) - self.area_origin.1,
        };
        let ratio = split_tree::ratio_from_pointer(&divider, pointer);
        if let Some(tab) = self.tabs.active_mut() {
            tab.tree.set_ratio(split_id, ratio);
        }
    }

    /// 远端接管鼠标时返回它的上报模式；按住 Shift 强制走本地选择（xterm 惯例）。
    fn mouse_report_mode(&self, shift: bool) -> MouseMode {
        let remote = self
            .pane()
            .map(|pane| pane.pump().mouse_mode())
            .unwrap_or_default();
        crate::terminal_menu::effective_mouse_mode(remote, shift)
    }

    /// 上报坐标按聚焦窗格的网格换算；超出范围钳到边缘，避免窗口边距丢掉事件。
    fn mouse_cell_at(&self, position: ::gpui::Point<::gpui::Pixels>) -> (usize, usize) {
        let Some(tab) = self.tabs.active() else {
            return (0, 0);
        };
        let geom = self
            .pane_geom
            .get(tab.active_pane)
            .copied()
            .unwrap_or_default();
        let cols = geom.cols.max(1);
        let rows = tab.active().snapshot.size.rows.max(1);
        let col = ((f32::from(position.x) - geom.left) / self.cell_width.max(1.0)).floor();
        let row = ((f32::from(position.y) - geom.top) / self.cell_height.max(1.0)).floor();
        (
            (row.max(0.0) as usize).min(rows - 1),
            (col.max(0.0) as usize).min(cols - 1),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn report_mouse(
        &mut self,
        action: MouseAction,
        button: TerminalMouseButton,
        position: ::gpui::Point<::gpui::Pixels>,
        shift: bool,
        alt: bool,
        ctrl: bool,
        sgr: bool,
    ) {
        let (row, col) = self.mouse_cell_at(position);
        let bytes = encode_mouse(MouseInput {
            action,
            button,
            col,
            row,
            shift,
            alt,
            ctrl,
            sgr,
        });
        if let Some(pane) = self.pane_mut() {
            pane.write(&bytes);
        }
    }

    fn focus_pane(&mut self, pane: usize) {
        if let Some(tab) = self.tabs.active_mut()
            && pane != tab.active_pane
        {
            tab.active_pane = pane;
            self.selection = None;
            self.hover_link = None;
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        // 拖动分屏分隔条：只改比例，不做选择。
        if self.start_split_drag(event.position, event.click_count) {
            cx.notify();
            return;
        }
        // 点在哪个窗格就聚焦哪个（含四周的内边距）；先聚焦，下面的模式判断、选择、上报都按它走。
        if let Some(index) = self.pane_at(event.position) {
            self.focus_pane(index);
        }

        let mode = self.mouse_report_mode(event.modifiers.shift);
        if mode.reports_buttons() {
            if let Some(button) = terminal_button(event.button) {
                self.report_mouse(
                    MouseAction::Press,
                    button,
                    event.position,
                    event.modifiers.shift,
                    event.modifiers.alt,
                    event.modifiers.control,
                    mode.sgr,
                );
                self.mouse_pressed = true;
                self.last_mouse_cell = Some(self.mouse_cell_at(event.position));
                cx.notify();
            }
            return;
        }

        if event.button == MouseButton::Right {
            // 上面的上报分支已经把远端接管的情形（没按 Shift）处理掉了，这里只剩粘贴和弹菜单。
            let link = self.link_at(event.position);
            match crate::terminal_menu::right_click_route(
                mode,
                self.settings.right_click_paste,
                link.is_some(),
            ) {
                crate::terminal_menu::RightClick::Paste => self.request_paste(cx),
                crate::terminal_menu::RightClick::Menu => {
                    self.open_context_menu(event.position, link, window, cx)
                }
                crate::terminal_menu::RightClick::Remote => {}
            }
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }

        // 双击链接：交给系统默认浏览器；只放行 http/https。
        if event.click_count >= 2
            && let Some(link) = self.link_at(event.position)
        {
            self.open_link(&link);
            return;
        }
        // 三击选行、双击选词（链接优先）。
        if let Some((_, row, col)) = self.cell_at(event.position) {
            let Some(snapshot_rows) = self.pane().map(|pane| &pane.snapshot.rows) else {
                return;
            };
            let span = match snapshot_rows.get(row) {
                Some(line) if event.click_count >= 3 => Some(crate::selection::line_span(line)),
                Some(line) if event.click_count == 2 => crate::selection::word_span(line, col),
                _ => None,
            };
            if let Some((start, end)) = span {
                self.selection = Some(((row, start), (row, end)));
                self.selecting = false;
                self.copy_on_select(cx);
                cx.notify();
                return;
            }
            // 单击只记锚点，拖动离开这一格才成为选区。
            self.selection = None;
            self.drag_anchor = Some((row, col));
            self.selecting = true;
            cx.notify();
        }
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dragging_split.is_some() {
            if event.pressed_button != Some(MouseButton::Left) {
                self.dragging_split = None;
                cx.notify();
                return;
            }
            self.update_split(event.position);
            cx.notify();
            return;
        }
        if self.selecting {
            // 丢失 mouse up 时不要继续扩选。
            if event.pressed_button != Some(MouseButton::Left) {
                self.selecting = false;
                return;
            }
            // 选区只在聚焦窗格里：指针拖到别的窗格或窗格外时，按聚焦窗格的边缘钳住。
            if let Some(anchor) = self.drag_anchor {
                let (row, col) = self.mouse_cell_at(event.position);
                self.selection = crate::selection::drag_selection(anchor, (row, col));
                self.copy_on_select(cx);
                cx.notify();
            }
            return;
        }

        // 1002 拖动、1003 悬停：只在跨格时发，避免每个像素一条事件。
        let mode = self.mouse_report_mode(event.modifiers.shift);
        if mode.reports_motion() {
            let cell = self.mouse_cell_at(event.position);
            let button = match event.pressed_button {
                Some(button) => terminal_button(button),
                None if mode.motion => Some(TerminalMouseButton::None),
                None => None,
            };
            if let Some(button) = button {
                if self.last_mouse_cell != Some(cell) {
                    self.last_mouse_cell = Some(cell);
                    self.report_mouse(
                        MouseAction::Motion,
                        button,
                        event.position,
                        event.modifiers.shift,
                        event.modifiers.alt,
                        event.modifiers.control,
                        mode.sgr,
                    );
                    cx.notify();
                }
                return;
            }
        }

        let hover = self.link_at(event.position);
        if self.hover_link != hover {
            self.hover_link = hover;
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.mouse_pressed {
            if let Some(button) = terminal_button(event.button) {
                let mode = self.mouse_report_mode(false);
                self.report_mouse(
                    MouseAction::Release,
                    button,
                    event.position,
                    event.modifiers.shift,
                    event.modifiers.alt,
                    event.modifiers.control,
                    mode.sgr,
                );
                cx.notify();
            }
            self.mouse_pressed = false;
            self.last_mouse_cell = None;
            return;
        }
        self.selecting = false;
        if self.dragging_split.take().is_some() {
            // 松手：下一帧窗格按最终尺寸重排并上报（拖的过程中没有重排）。
            cx.notify();
        }
    }

    fn on_wheel(&mut self, event: &ScrollEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let row_height = self.settings.font_size * self.settings.line_height;
        let lines = match event.delta {
            ScrollDelta::Lines(point) => point.y,
            ScrollDelta::Pixels(point) => f32::from(point.y) / row_height.max(1.0),
        };
        let delta = lines.round() as i32;

        let mode = self.mouse_report_mode(event.modifiers.shift);
        if mode.reports_buttons() && delta != 0 {
            let action = if delta > 0 {
                MouseAction::WheelUp
            } else {
                MouseAction::WheelDown
            };
            // 一次滚轮事件可能跨多行；最多补 5 条，防止触控板刷屏。
            for _ in 0..delta.unsigned_abs().min(5) {
                self.report_mouse(
                    action,
                    TerminalMouseButton::None,
                    event.position,
                    event.modifiers.shift,
                    event.modifiers.alt,
                    event.modifiers.control,
                    mode.sgr,
                );
            }
            cx.notify();
            return;
        }

        // 滚的是指针下面的窗格（分屏时不一定是聚焦的那个）；不在任何窗格里就滚聚焦的。
        if delta != 0 {
            let under = self.pane_at(event.position);
            if let Some(tab) = self.tabs.active_mut() {
                let index = under.unwrap_or(tab.active_pane).min(tab.panes.len() - 1);
                let pane = &mut tab.panes[index];
                pane.pump_mut().scroll(delta);
                pane.refresh();
                cx.notify();
            }
        }
    }

    // ---- 复制、粘贴、日志 ----

    /// 当前选区的文字；没有选区返回空串。
    fn selected_text(&self) -> String {
        let Some((anchor, head)) = self.selection else {
            return String::new();
        };
        self.pane()
            .map(|pane| pane.pump().selection_text(anchor, head))
            .unwrap_or_default()
    }

    pub(crate) fn copy_selection(&mut self, cx: &mut Context<Self>) {
        let text = self.selected_text();
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// 用系统默认浏览器打开链接（先过 http/https 校验）。失败只记日志，和双击链接一贯的做法一样。
    fn open_link(&self, link: &str) {
        let opened = crate::terminal_menu::open_link(link, |url| {
            zssh_core::external_url::open_external_url(url)
        });
        if let Err(err) = opened {
            tracing::warn!(%err, "打开链接失败");
        }
    }

    /// 在 `position`（窗口坐标）弹出终端右键菜单。`link` 是鼠标下的链接。点在已有选区之外也不清选区，
    /// 这样「先选中再右键复制」能用。
    fn open_context_menu(
        &mut self,
        position: ::gpui::Point<Pixels>,
        link: Option<Arc<str>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = crate::terminal_menu::MenuState {
            link: link.as_deref().map(str::to_owned),
            has_selection: !self.selected_text().is_empty(),
            clipboard_has_text: cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .is_some_and(|text| !text.is_empty()),
        };
        let entries = crate::terminal_menu::menu_entries(&state);
        let tokens = crate::app_theme::Tokens::current(cx);
        let entity = cx.entity();
        let dispatch: crate::terminal_menu::Dispatch = {
            let link = link.clone();
            Rc::new(move |item, window, cx| {
                entity.update(cx, |this, cx| {
                    this.run_menu_item(item, link.as_deref(), window, cx)
                });
            })
        };
        let shown = entries.clone();
        // `action_context` 让菜单关闭后把焦点还给终端（点了某项、点外面、Esc 都一样）。
        let focus = self.focus.clone();
        let menu = PopupMenu::build(window, cx, move |menu, _window, _cx| {
            crate::terminal_menu::build_menu(menu.action_context(focus), &shown, &tokens, dispatch)
        });
        let dismiss = cx.subscribe_in(
            &menu,
            window,
            |this, menu, _event: &DismissEvent, _window, cx| {
                // 在别处再右键一次时，旧菜单的「点外面关闭」可能晚于新菜单的创建才送到：只清自己那一个。
                if this
                    .context_menu
                    .as_ref()
                    .is_some_and(|open| open.menu.entity_id() == menu.entity_id())
                {
                    this.context_menu = None;
                }
                cx.notify();
            },
        );
        // 焦点要等这次按下分发完再给菜单：终端根元素跟踪了焦点，按下冒泡到它时会把焦点抢回去，
        // 这里同步抢的话菜单拿不到 Esc / 方向键（组件库自己的右键菜单也是下一拍才聚焦）。
        let menu_focus = menu.clone();
        window.defer(cx, move |window, cx| {
            menu_focus.focus_handle(cx).focus(window, cx);
        });
        self.context_menu = Some(ContextMenu {
            menu,
            position,
            entries,
            link,
            _dismiss: dismiss,
        });
        cx.notify();
    }

    /// 菜单里选了一项（和对应的快捷键走同一批方法）。
    fn run_menu_item(
        &mut self,
        item: crate::terminal_menu::MenuItem,
        link: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::terminal_menu::MenuItem;
        match item {
            MenuItem::OpenLink => {
                if let Some(link) = link {
                    self.open_link(link);
                }
            }
            MenuItem::CopyLink => {
                if let Some(link) = link {
                    cx.write_to_clipboard(ClipboardItem::new_string(link.to_owned()));
                }
            }
            MenuItem::Copy => {
                // 和 Ctrl+Shift+C 一样：复制后清掉选区。
                self.copy_selection(cx);
                self.selection = None;
            }
            MenuItem::Paste => self.request_paste(cx),
            MenuItem::SelectAll => self.select_all(cx),
            MenuItem::SplitRight => {
                self.split_active(Axis::X, cx);
            }
            MenuItem::SplitDown => {
                self.split_active(Axis::Y, cx);
            }
            MenuItem::Separator => {}
        }
        window.focus(&self.focus, cx);
        cx.notify();
    }

    /// 全选：聚焦窗格当前屏幕上的所有格子（不含回滚缓冲）。「选中即复制」开着时同样会写剪贴板。
    fn select_all(&mut self, cx: &mut Context<Self>) {
        let Some(size) = self.pane().map(|pane| pane.snapshot.size) else {
            return;
        };
        self.selection = crate::terminal_menu::select_all_range(size.rows, size.cols);
        self.selecting = false;
        self.drag_anchor = None;
        self.copy_on_select(cx);
    }

    /// 选中即复制：开关打开时，选区一变化就写剪贴板。
    fn copy_on_select(&mut self, cx: &mut Context<Self>) {
        if !self.settings.copy_on_select {
            return;
        }
        self.copy_selection(cx);
    }

    pub(crate) fn toggle_log(&mut self, cx: &mut Context<Self>) {
        let Some(pane) = self.pane_mut() else {
            return;
        };
        self.status_text = match pane.pump_mut().take_log() {
            Some(log) => {
                let written = log.finish();
                format!("日志已停止：{written} 字节")
            }
            None => start_log(pane),
        };
        cx.notify();
    }

    fn request_paste(&mut self, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let Some(text) = item.text() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        // 多行粘贴先确认：远端没开 bracketed paste 时，多行会被 shell 直接执行。
        if text.contains(['\r', '\n']) && self.settings.confirm_multiline {
            self.pending_paste = Some(text);
            cx.notify();
        } else {
            // 直接粘贴时清掉之前的确认条，避免旧内容还挂在那里。
            self.pending_paste = None;
            self.send_paste(&text, cx);
        }
    }

    /// 输入的去向：开了广播就是当前标签的所有分屏窗格，否则只有聚焦的窗格。
    /// 各窗格各自回底并刷新快照。
    fn send_bytes(&mut self, bytes: &[u8]) {
        // 键盘输入发出去之后选区就过时了，留着会让下一次 Ctrl+C 走复制。
        self.selection = None;
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };
        for index in tab.input_targets() {
            let pane = &mut tab.panes[index];
            if pane.pump().display_offset() != 0 {
                pane.pump_mut().scroll_to_bottom();
            }
            pane.write(bytes);
        }
    }

    /// 从文件面板的远端栏拖来的文件落在第 `index` 个窗格上：把远端路径插到输入行（逐个单引号引用，不执行、
    /// 不加回车，走和粘贴同一条编码，所以开了 bracketed paste 的 shell 里也是"粘贴"而不是逐键输入）。
    /// 窗格没连上、路径里有终端控制字符都不插。
    fn drop_sftp_paths(
        &mut self,
        index: usize,
        drag: &SftpDrag,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(paths) = drag.remote_paths(cx) else {
            return;
        };
        let text = match terminal_insert_text(&paths) {
            Ok(text) => text,
            Err(reason) => {
                // 0.2.14 弹的是错误提示：这里是窗口右上角的通知。
                window.push_notification(reason.to_owned(), cx);
                return;
            }
        };
        let mode = self.settings.bracketed_paste;
        let Some(tab) = self.tabs.active_mut() else {
            return;
        };
        let Some(pane) = tab.panes.get_mut(index) else {
            return;
        };
        if pane.state != PaneState::Live {
            return;
        }
        if pane.pump().display_offset() != 0 {
            pane.pump_mut().scroll_to_bottom();
        }
        let wrapped = mode.wraps(pane.pump().bracketed_paste());
        pane.write(&zssh_session::encode_paste(&text, wrapped));
        tab.active_pane = index;
        self.selection = None;
        cx.notify();
    }

    fn send_paste(&mut self, text: &str, cx: &mut Context<Self>) {
        self.selection = None;
        let mode = self.settings.bracketed_paste;
        if let Some(tab) = self.tabs.active_mut() {
            for index in tab.input_targets() {
                let pane = &mut tab.panes[index];
                let wrapped = mode.wraps(pane.pump().bracketed_paste());
                pane.write(&zssh_session::encode_paste(text, wrapped));
            }
        }
        cx.notify();
    }

    fn confirm_paste(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.pending_paste.take() {
            self.send_paste(&text, cx);
        }
        self.pending_paste = None;
        cx.notify();
    }

    /// 调试钩子用：往活动窗格的会话里"打字"（和键盘输入同一条路径）。
    pub(crate) fn type_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.send_bytes(text.as_bytes());
        cx.notify();
    }

    // ---- 离线演示用的小工具（调试窗口） ----

    /// 截图/手动验证用：让终端进入 1002 + 1006 鼠标上报模式。
    pub fn seed_mouse_reporting(&mut self, cx: &mut Context<Self>) {
        self.write_active(b"\x1b[?1002h\x1b[?1006h");
        cx.notify();
    }

    /// 所有标签的所有窗格各喂一批：后台标签同样继续消费，切回时不重建。
    pub(crate) fn feed_batch(&mut self, lines: usize) {
        for tab in self.tabs.items_mut() {
            for pane in &mut tab.panes {
                for i in 0..lines {
                    let n = pane.fed + i;
                    let line = format!(
                        "\x1b[32mok\x1b[0m  line {n:04}  \u{4e2d}\u{6587}\u{8f93}\u{51fa} \x1b[31merror\x1b[0m \x1b[33mwarn\x1b[0m \x1b[34mhttps://example.com/{n}\x1b[0m  \x1b[1mbold\x1b[0m\r\n"
                    );
                    pane.write(line.as_bytes());
                }
                pane.fed += lines;
            }
        }
    }

    pub(crate) fn refresh_all(&mut self) {
        for tab in self.tabs.items_mut() {
            for pane in &mut tab.panes {
                pane.refresh();
            }
        }
    }

    pub(crate) fn feed_more(&mut self) {
        self.feed_batch(20);
        self.refresh_all();
    }
}

/// 开始记录一个窗格的会话，返回要显示的状态文字。
fn start_log(pane: &mut Pane) -> String {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let path =
        std::env::temp_dir().join(format!("zssh-session-{}-{stamp}.log", std::process::id()));
    match SessionLog::create(&path, 16 * 1024 * 1024) {
        Ok(log) => {
            pane.pump_mut().set_log(log);
            format!("记录到 {}", path.display())
        }
        Err(err) => format!("无法开始记录：{err}"),
    }
}

/// 光标闪烁的相位：每次按键 / 改设置后从「亮」重新计时，之后每半个周期翻一次
/// （530 ms，xterm 的节奏）。相位由时间算出来，不需要定时器翻转状态；窗口的 60 ms 节拍
/// 只负责在相位变了的时候通知重画。
pub(crate) struct BlinkClock {
    since: std::time::Instant,
    /// 上一次通知重画时的相位。
    drawn: bool,
}

const BLINK_HALF_PERIOD: Duration = Duration::from_millis(530);

impl BlinkClock {
    pub(crate) fn new() -> Self {
        Self {
            since: std::time::Instant::now(),
            drawn: true,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.since = std::time::Instant::now();
    }

    fn phase_at(elapsed: Duration) -> bool {
        (elapsed.as_millis() / BLINK_HALF_PERIOD.as_millis()).is_multiple_of(2)
    }

    /// 现在光标是亮的吗。
    pub(crate) fn visible(&self) -> bool {
        Self::phase_at(self.since.elapsed())
    }

    /// 相位和上次画的不一样了（该重画）；顺手记下。
    fn changed(&mut self) -> bool {
        let now = self.visible();
        let changed = now != self.drawn;
        self.drawn = now;
        changed
    }
}

impl Render for TerminalWorkspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = self.mono_family(cx);
        let mono_size = ::gpui::px(self.settings.font_size);
        // 终端自己的色板：配色可以固定成和界面皮肤不同的一套（默认界面 Dracula、终端 Tokyo Night），
        // 所以底色、前景、选区、光标都取终端色板，不取界面主题。
        let term = crate::app_theme::terminal_palette(cx);
        let foreground: Hsla = ::gpui::rgb(term.foreground).into();
        let background: Hsla = ::gpui::rgb(term.background).into();
        let selection_color: Hsla = ::gpui::rgb(term.selection).into();
        let cursor_color: Hsla = ::gpui::rgb(term.cursor).into();
        let ansi = term.ansi;
        let entity = cx.entity();

        // 按区域大小和等宽字符宽度算行列；尺寸变化时重排终端。
        let font_id = window
            .text_system()
            .resolve_font(&::gpui::font(mono.clone()));
        let cell_width = window
            .text_system()
            .advance(font_id, mono_size, 'M')
            .map(|size| f32::from(size.width))
            .unwrap_or(f32::from(mono_size) * 0.6);
        let cell_height = f32::from(mono_size) * self.settings.line_height;
        self.cell_width = cell_width;
        self.cell_height = cell_height;

        // 区域用上一帧量到的；第一帧还没有，先按整个窗口估，下一帧就会纠正。
        let measured = self.area.get();
        let (area_x, area_y, area_w, area_h) = match measured {
            Some(bounds) => (
                f32::from(bounds.origin.x),
                f32::from(bounds.origin.y),
                f32::from(bounds.size.width),
                f32::from(bounds.size.height),
            ),
            None => {
                let viewport = window.viewport_size();
                (
                    0.0,
                    0.0,
                    f32::from(viewport.width),
                    f32::from(viewport.height),
                )
            }
        };
        self.layout_area = measured;
        self.area_origin = (area_x, area_y);

        let tokens = crate::app_theme::Tokens::current(cx);
        let Some(active) = self.tabs.active() else {
            return v_flex()
                .size_full()
                .track_focus(&self.focus)
                .bg(background)
                .into_any_element();
        };

        // 新开的标签用整块区域的尺寸（扣掉四周内边距）。
        let usable_w = (area_w - PANE_PAD * 2.0).max(0.0);
        let usable_h = (area_h - PANE_PAD * 2.0).max(0.0);
        self.default_size = Size {
            rows: ((usable_h / cell_height).floor() as usize).max(2),
            cols: ((usable_w / cell_width).floor() as usize).max(20),
        };

        // 分屏树 -> 每个窗格的矩形和分隔条；窗格表和树同序，下标一一对应。
        let layout = split_tree::compute_layout(&active.tree, area_w, area_h);
        let mut geoms: Vec<PaneGeom> = Vec::with_capacity(active.panes.len());
        for pane in &active.panes {
            let rect = layout.rect_of(pane.id).unwrap_or(split_tree::Rect {
                x: 0.0,
                y: 0.0,
                w: area_w,
                h: area_h,
            });
            let inner_w = (rect.w - PANE_PAD * 2.0).max(0.0);
            let inner_h = (rect.h - PANE_PAD * 2.0).max(0.0);
            geoms.push(PaneGeom {
                rect: split_tree::Rect {
                    x: area_x + rect.x,
                    y: area_y + rect.y,
                    ..rect
                },
                left: area_x + rect.x + PANE_PAD,
                top: area_y + rect.y + PANE_PAD,
                cols: ((inner_w / cell_width).floor() as usize).max(20),
                rows: ((inner_h / cell_height).floor() as usize).max(2),
            });
        }

        // 活动标签的每个窗格按自己的矩形重排；隐藏标签保持原尺寸，切回时再算。
        // 拖分隔条的过程中不重排（0.2.14 UI-1：只在松手后重排并上报，免得每帧给远端发窗口变化）。
        if self.dragging_split.is_none()
            && let Some(tab) = self.tabs.active_mut()
        {
            for (pane, geom) in tab.panes.iter_mut().zip(geoms.iter()) {
                let size = Size {
                    rows: geom.rows,
                    cols: geom.cols,
                };
                if pane.snapshot.size != size {
                    pane.resize(size);
                }
            }
        }
        self.pane_geom = geoms;
        self.layout = layout.clone();

        let Some(active) = self.tabs.active() else {
            return v_flex()
                .size_full()
                .track_focus(&self.focus)
                .bg(background)
                .into_any_element();
        };
        let highlight_rows = active.active().snapshot.size.rows;
        self.request_highlight_scan(highlight_rows);

        let Some(active) = self.tabs.active() else {
            return v_flex()
                .size_full()
                .track_focus(&self.focus)
                .bg(background)
                .into_any_element();
        };
        let active_pane = active.active_pane;
        let show_latency = self.settings.show_latency;
        let ui_skin = crate::app_theme::active_skin(cx);
        let split = active.is_split();
        let selection = self.selection;
        let broadcast = active.broadcast;
        let dragging = self.dragging_split;

        let pending_bar = self.pending_paste.clone().map(|text| {
            let lines = text.lines().count().max(1);
            let confirm = entity.clone();
            let cancel = entity.clone();
            h_flex()
                .h(::gpui::px(40.0))
                .px_2()
                .gap_2()
                .items_center()
                .child(div().text_sm().child(format!(
                    "\u{7c98}\u{8d34} {lines} \u{884c}\u{5185}\u{5bb9}\u{ff1f}"
                )))
                .child(Button::new("paste-ok").label("\u{7c98}\u{8d34}").on_click(
                    move |_, _, cx| {
                        confirm.update(cx, |this, cx| this.confirm_paste(cx));
                    },
                ))
                .child(
                    Button::new("paste-cancel")
                        .label("\u{53d6}\u{6d88}")
                        .on_click(move |_, _, cx| {
                            cancel.update(cx, |this, cx| {
                                this.pending_paste = None;
                                cx.notify();
                            });
                        }),
                )
                .into_any_element()
        });

        let area_cell = Rc::clone(&self.area);
        let mut children: Vec<AnyElement> = Vec::new();
        for (index, pane) in active.panes.iter().enumerate() {
            let rect = layout.rect_of(pane.id).unwrap_or(split_tree::Rect {
                x: 0.0,
                y: 0.0,
                w: area_w,
                h: area_h,
            });
            let focused = index == active_pane;
            let pane_selection = if focused { selection } else { None };
            let pane_highlights: &[Vec<Span>] = if focused { &self.highlight.spans } else { &[] };
            let accepts_drop = pane.state == PaneState::Live;
            let drop_border = tokens.accent;
            children.push(
                div()
                    .id(("pane", index))
                    .absolute()
                    .left(::gpui::px(rect.x))
                    .top(::gpui::px(rect.y))
                    .w(::gpui::px(rect.w))
                    .h(::gpui::px(rect.h))
                    .overflow_hidden()
                    .bg(background)
                    // 远端栏拖来的文件：只有连着的窗格接，本地栏的条目不接（同 0.2.14）。
                    .drag_over::<SftpDrag>(move |style, drag, _, _| {
                        if accepts_drop && drag.is_remote() {
                            style.border_1().border_color(drop_border)
                        } else {
                            style
                        }
                    })
                    .on_drop::<SftpDrag>(cx.listener(move |this, drag: &SftpDrag, window, cx| {
                        this.drop_sftp_paths(index, drag, window, cx);
                    }))
                    .child(
                        v_flex()
                            .size_full()
                            .p(::gpui::px(PANE_PAD))
                            .child(pane_grid(
                                pane,
                                pane_selection,
                                pane_highlights,
                                focused,
                                &mono,
                                self.settings.font_size,
                                self.settings.line_height,
                                self.settings.cursor,
                                !self.settings.cursor_blink || self.cursor_blink.visible(),
                                foreground,
                                background,
                                selection_color,
                                cursor_color,
                                &ansi,
                            )),
                    )
                    // 分屏时聚焦窗格顶上一道细线，告诉用户键盘输入去哪（单个窗格不画；
                    // 广播时每个窗格都收输入，线就没有意义了）。同 0.2.14 的 `pane-focus-line`。
                    .when(split && focused && !broadcast, |d| {
                        d.child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .right_0()
                                .h(::gpui::px(2.0))
                                .bg(tokens.accent.alpha(0.7)),
                        )
                    })
                    // 右上角延迟徽标：设置开着、窗格连着、已经测到第一个值才画。
                    .when(show_latency && pane.state == PaneState::Live, |d| {
                        d.children(latency_badge(
                            index,
                            pane,
                            &mono,
                            ui_skin,
                            term.background,
                            self.settings.font_size,
                        ))
                    })
                    .into_any_element(),
            );
        }
        // 分隔条：可见线 4px，命中区每边多 2px（透明，叠在窗格边缘上）；拖的时候线变成强调色。
        let grow = (split_tree::DIVIDER_HIT - split_tree::DIVIDER_SIZE) / 2.0;
        for (index, divider) in layout.dividers.iter().enumerate() {
            let line = divider.rect;
            let horizontal = divider.axis == Axis::X;
            let (x, y, w, h) = if horizontal {
                (line.x - grow, line.y, line.w + grow * 2.0, line.h)
            } else {
                (line.x, line.y - grow, line.w, line.h + grow * 2.0)
            };
            let color = if dragging == Some(divider.split_id) {
                tokens.accent
            } else {
                cx.theme().border
            };
            children.push(
                div()
                    .id(("split-divider", index))
                    .absolute()
                    .left(::gpui::px(x))
                    .top(::gpui::px(y))
                    .w(::gpui::px(w))
                    .h(::gpui::px(h))
                    .when(horizontal, |d| d.cursor_col_resize())
                    .when(!horizontal, |d| d.cursor_row_resize())
                    .child(
                        div()
                            .absolute()
                            .left(::gpui::px(if horizontal { grow } else { 0.0 }))
                            .top(::gpui::px(if horizontal { 0.0 } else { grow }))
                            .w(::gpui::px(line.w))
                            .h(::gpui::px(line.h))
                            .bg(color),
                    )
                    .into_any_element(),
            );
        }

        // 右键菜单：画在最上层（deferred），位置是弹出时的窗口坐标；菜单上滚轮不要滚到后面的终端。
        let context_menu = self.context_menu.as_ref().map(|open| {
            deferred(
                anchored()
                    .position(open.position)
                    .anchor(Anchor::TopLeft)
                    .snap_to_window_with_margin(::gpui::px(8.0))
                    .child(
                        div()
                            .on_scroll(|_, _, cx| cx.stop_propagation())
                            .child(open.menu.clone()),
                    ),
            )
            .with_priority(gpui_kit::base::POPUP_PRIORITY)
        });

        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll(cx.listener(Self::on_wheel))
            .bg(background)
            .when(self.hover_link.is_some(), |d| d.cursor_pointer())
            .children(pending_bar)
            .children(context_menu)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(
                        canvas(
                            move |bounds, _, _| area_cell.set(Some(bounds)),
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(
                        div()
                            .id("pane-area")
                            .relative()
                            .size_full()
                            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
                            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
                            .on_mouse_move(cx.listener(Self::on_mouse_move))
                            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
                            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
                            .children(children),
                    )
                    // 广播开着时在窗格右上角常驻提示：同一段输入会进到这个标签的每个窗格。
                    .when(broadcast, |d| {
                        d.child(
                            div()
                                .absolute()
                                .top_1()
                                .right_3()
                                .px_2()
                                .py_0p5()
                                .rounded_sm()
                                .bg(tokens.accent)
                                .text_color(tokens.on_accent)
                                .text_xs()
                                .child("\u{5e7f}\u{64ad}\u{4e2d}"),
                        )
                    }),
            )
            .into_any_element()
    }
}

/// 窗格右上角的延迟徽标：小圆角、半透明底（悬停变实）、数值按色档着色；悬停提示写明测的是什么。
/// 窗格没有监视器、还没有第一个值、或者已断开时是 `None`。
fn latency_badge(
    index: usize,
    pane: &Pane,
    mono: &::gpui::SharedString,
    skin: &crate::app_theme::Skin,
    terminal_bg: u32,
    font_size: f32,
) -> Option<AnyElement> {
    let (snapshot, host) = pane.latency_snapshot()?;
    let last = snapshot.stats.last()?;
    let badge = crate::app_theme::latency_badge(latency::level(last), skin, terminal_bg);
    let fill: Hsla = ::gpui::rgb(badge.fill).into();
    let fg: Hsla = ::gpui::rgb(badge.fg).into();
    let text = latency::format_sample(last);
    let tip: ::gpui::SharedString = latency::tooltip(host, snapshot.kind, &snapshot.stats).into();
    Some(
        div()
            .id(("latency-badge", index))
            .absolute()
            .top(::gpui::px(6.0))
            .right(::gpui::px(12.0))
            .px(::gpui::px(6.0))
            .py(::gpui::px(1.0))
            .rounded(::gpui::px(5.0))
            .bg(fill.alpha(0.8))
            .hover(move |style| style.bg(fill))
            .text_color(fg)
            .font_family(mono.clone())
            .text_size(::gpui::px((font_size - 2.0).max(10.0)))
            .child(text)
            // 点在徽标上不要开始选区 / 抢走终端焦点以外的东西。
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .tooltip(move |window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
            })
            .into_any_element(),
    )
}

/// 一个窗格的可见行；不同窗格各自独立选区，只有聚焦窗格画光标。
#[allow(clippy::too_many_arguments)]
fn pane_grid(
    pane: &Pane,
    selection: Option<((usize, usize), (usize, usize))>,
    highlights: &[Vec<Span>],
    focused: bool,
    mono: &::gpui::SharedString,
    font_size: f32,
    line_height: f32,
    cursor_shape: CursorShape,
    cursor_lit: bool,
    foreground: Hsla,
    background: Hsla,
    selection_color: Hsla,
    cursor_color: Hsla,
    ansi: &[u32; 16],
) -> impl IntoElement {
    let mono_size = ::gpui::px(font_size);
    let cursor = if focused && cursor_lit {
        pane.pump().cursor_visible()
    } else {
        None
    };
    v_flex()
        .flex_1()
        .overflow_hidden()
        .children(pane.snapshot.rows.iter().enumerate().map(|(row_ix, row)| {
            let plain_links = crate::links::find_plain_links(row);
            let row_spans = highlights.get(row_ix).map(Vec::as_slice).unwrap_or(&[]);
            h_flex().h(::gpui::px(font_size * line_height)).children(
                runs(
                    row,
                    foreground,
                    background,
                    selection,
                    row_ix,
                    cursor,
                    &plain_links,
                    row_spans,
                    ansi,
                )
                .into_iter()
                .map(|run| {
                    let block = run.cursor && cursor_shape == CursorShape::Block;
                    let underline = run.cursor && cursor_shape == CursorShape::Underline;
                    let bar = run.cursor && cursor_shape == CursorShape::Bar;
                    let bg = if block {
                        Some(cursor_color)
                    } else if run.selected {
                        Some(selection_color)
                    } else if run.bg != background {
                        Some(run.bg)
                    } else {
                        None
                    };
                    div()
                        .font_family(mono.clone())
                        .text_size(mono_size)
                        .text_color(if block { background } else { run.fg })
                        .when_some(bg, |d, bg| d.bg(bg))
                        .when(run.underline || underline, |d| d.underline())
                        .when(run.strikethrough, |d| d.line_through())
                        .when(run.bold, |d| d.font_weight(FontWeight::BOLD))
                        .when(run.italic, |d| d.italic())
                        .child(run.text)
                        // 竖线光标：压在格子左边的 2 px 细条，不占宽度，后面的字不会被挤开。
                        .when(bar, |d| {
                            d.relative().child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .bottom_0()
                                    .left_0()
                                    .w(::gpui::px(2.0))
                                    .bg(cursor_color),
                            )
                        })
                }),
            )
        }))
}

fn is_selected(
    selection: Option<((usize, usize), (usize, usize))>,
    row: usize,
    col: usize,
) -> bool {
    let Some((anchor, head)) = selection else {
        return false;
    };
    let (start, end) = if anchor <= head {
        (anchor, head)
    } else {
        (head, anchor)
    };
    (row > start.0 || (row == start.0 && col >= start.1))
        && (row < end.0 || (row == end.0 && col <= end.1))
}

/// 把一行按相同属性（含选中状态）切成连续的 run；宽字符占位格跳过。
struct Run {
    text: String,
    fg: Hsla,
    bg: Hsla,
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
    selected: bool,
    cursor: bool,
}

#[allow(clippy::too_many_arguments)]
fn runs(
    row: &zssh_terminal::RowSnapshot,
    foreground: Hsla,
    background: Hsla,
    selection: Option<((usize, usize), (usize, usize))>,
    row_ix: usize,
    cursor: Option<(usize, usize)>,
    plain_links: &[crate::links::LinkSpan],
    spans: &[Span],
    ansi: &[u32; 16],
) -> Vec<Run> {
    let mut out: Vec<Run> = Vec::new();
    for (col, cell) in row.cells.iter().enumerate() {
        if cell.ch == '\0' || cell.spacer {
            continue;
        }
        let (mut fg, mut bg) = (
            color_to_hsla(cell.fg, foreground, background, ansi),
            color_to_hsla(cell.bg, foreground, background, ansi),
        );
        // 反显：前景/背景互换。
        if cell.inverse {
            std::mem::swap(&mut fg, &mut bg);
        }
        // 高亮规则叠加在 ANSI 颜色之上；后加的规则优先。
        let mut span_fg = None;
        let mut span_bg = None;
        for span in spans {
            if col >= span.start && col <= span.end {
                span_fg = Some(span.fg);
                span_bg = span.bg;
            }
        }
        if let Some(color) = span_fg {
            fg = ::gpui::rgb(color).into();
        }
        if let Some(color) = span_bg {
            bg = ::gpui::rgb(color).into();
        }
        let selected = is_selected(selection, row_ix, col);
        let is_cursor = cursor == Some((row_ix, col));
        let underline = cell.underline
            || cell.hyperlink.is_some()
            || plain_links
                .iter()
                .any(|link| col >= link.start_col && col <= link.end_col);
        let mut text = if cell.hidden {
            " ".to_owned()
        } else {
            let mut text = String::from(cell.ch);
            if let Some(zerowidth) = &cell.zerowidth {
                text.extend(zerowidth.iter());
            }
            text
        };
        match out.last_mut() {
            Some(run)
                if run.fg == fg
                    && run.bg == bg
                    && run.bold == cell.bold
                    && run.italic == cell.italic
                    && run.underline == underline
                    && run.strikethrough == cell.strikethrough
                    && run.selected == selected
                    && run.cursor == is_cursor =>
            {
                run.text.push_str(&text);
            }
            _ => out.push(Run {
                text: std::mem::take(&mut text),
                fg,
                bg,
                bold: cell.bold,
                italic: cell.italic,
                underline,
                strikethrough: cell.strikethrough,
                selected,
                cursor: is_cursor,
            }),
        }
    }
    // 行尾空白只在没有背景色、没被选中/光标时裁掉。
    while let Some(run) = out.last() {
        if run.text.trim().is_empty() && !run.cursor && !run.selected && run.bg == background {
            out.pop();
        } else {
            break;
        }
    }
    out
}

fn color_to_hsla(
    cell_color: TerminalColor,
    foreground: Hsla,
    background: Hsla,
    ansi: &[u32; 16],
) -> Hsla {
    match cell_color {
        TerminalColor::Named(named) => named_color(named, foreground, background, ansi),
        TerminalColor::Indexed(index) => indexed_color(index, ansi),
        TerminalColor::Spec(rgb) => {
            ::gpui::rgb(((rgb.r as u32) << 16) | ((rgb.g as u32) << 8) | rgb.b as u32).into()
        }
    }
}

/// 16 色按当前主题的终端色板取（`app_theme` 里的 `ansi`）；「暗」色没有单独的色值，和
/// 普通色一样；前景/背景用主题的前景/背景。
fn named_color(color: NamedColor, foreground: Hsla, background: Hsla, ansi: &[u32; 16]) -> Hsla {
    let index = match color {
        NamedColor::Black | NamedColor::DimBlack => 0,
        NamedColor::Red | NamedColor::DimRed => 1,
        NamedColor::Green | NamedColor::DimGreen => 2,
        NamedColor::Yellow | NamedColor::DimYellow => 3,
        NamedColor::Blue | NamedColor::DimBlue => 4,
        NamedColor::Magenta | NamedColor::DimMagenta => 5,
        NamedColor::Cyan | NamedColor::DimCyan => 6,
        NamedColor::White | NamedColor::DimWhite => 7,
        NamedColor::BrightBlack => 8,
        NamedColor::BrightRed => 9,
        NamedColor::BrightGreen => 10,
        NamedColor::BrightYellow => 11,
        NamedColor::BrightBlue => 12,
        NamedColor::BrightMagenta => 13,
        NamedColor::BrightCyan => 14,
        NamedColor::BrightWhite => 15,
        NamedColor::Background => return background,
        NamedColor::Foreground | NamedColor::BrightForeground => return foreground,
        _ => return foreground,
    };
    ::gpui::rgb(ansi[index]).into()
}

fn indexed_color(index: u8, ansi: &[u32; 16]) -> Hsla {
    let rgb = match index {
        0..=15 => ansi[index as usize],
        16..=231 => {
            let i = index - 16;
            let r = (i / 36) % 6;
            let g = (i / 6) % 6;
            let b = i % 6;
            let scale = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            ((scale(r) as u32) << 16) | ((scale(g) as u32) << 8) | scale(b) as u32
        }
        _ => {
            let level = 8 + (index - 232) * 10;
            ((level as u32) << 16) | ((level as u32) << 8) | level as u32
        }
    };
    ::gpui::rgb(rgb).into()
}

#[cfg(test)]
mod pane_tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{
        CoreFilter, FlowGate, Pane, PaneState, SessionBackend, SessionCommand, SessionLog,
        SessionPump, Size, disconnect_notice, failure_notice,
    };
    use std::sync::{Arc, Mutex};
    use zssh_session::{SessionEvent, SessionExit};

    const SIZE: Size = Size { rows: 12, cols: 60 };

    fn screen(pane: &Pane) -> String {
        pane.snapshot
            .rows
            .iter()
            .map(|row| row.text.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 记录收到的命令，并把写入当成"远端没有回显"丢掉。
    struct Recorder(Arc<Mutex<Vec<SessionCommand>>>);

    impl SessionBackend for Recorder {
        fn send(&mut self, command: SessionCommand) {
            self.0.lock().unwrap().push(command);
        }
    }

    fn remote_pane() -> (
        Pane,
        std::sync::mpsc::Sender<SessionEvent>,
        Arc<Mutex<Vec<SessionCommand>>>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let pump = SessionPump::new(SIZE, 100, Arc::new(FlowGate::new()), CoreFilter::new(), 1);
        let pane = Pane::with_parts(
            pump,
            Box::new(Recorder(Arc::clone(&sent))),
            rx,
            Some(7),
            None,
        );
        (pane, tx, sent)
    }

    #[test]
    fn connection_failure_shows_the_reason_and_never_echoes_input() {
        let pending = crate::ssh_backend::ready(Err("连接失败：timed out".to_owned()));
        let mut pane = Pane::connecting(SIZE, 100, 1, Some(7), pending);
        assert_eq!(pane.state, PaneState::Connecting);
        assert!(screen(&pane).contains("正在连接"));

        assert!(pane.poll_events());
        assert_eq!(pane.state, PaneState::Disconnected);
        let text = screen(&pane);
        assert!(text.contains("连接失败：timed out"), "{text}");
        assert!(text.contains("Ctrl+Shift+R"), "{text}");
        // 还能重连：主机 id 没丢。
        assert_eq!(pane.host_id, Some(7));

        // 失败后键入的内容（比如密码）不能回显到屏幕上。
        pane.write(b"hunter2\r");
        assert!(!screen(&pane).contains("hunter2"));
    }

    #[test]
    fn typing_while_connecting_is_dropped() {
        let pending = crate::ssh_backend::ready(Err("x".to_owned()));
        let mut pane = Pane::connecting(SIZE, 100, 1, Some(7), pending);
        pane.write(b"early keys");
        assert!(!screen(&pane).contains("early keys"));
    }

    #[test]
    fn remote_exit_marks_the_pane_disconnected_once() {
        let (mut pane, tx, sent) = remote_pane();
        tx.send(SessionEvent::Data {
            generation: 1,
            bytes: b"bye\r\n".to_vec(),
        })
        .unwrap();
        tx.send(SessionEvent::Exit {
            generation: 1,
            code: Some(3),
        })
        .unwrap();
        assert!(pane.poll_events());
        assert_eq!(pane.state, PaneState::Disconnected);
        let text = screen(&pane);
        assert!(text.contains("bye"), "{text}");
        assert!(text.contains("连接已断开（退出码 3）"), "{text}");
        assert!(text.contains("Ctrl+Shift+R"), "{text}");

        // 之后的输入不再发给后端，也不再重复提示。
        pane.write(b"ls\r");
        assert!(sent.lock().unwrap().is_empty());
        assert!(!pane.poll_events());
        assert_eq!(screen(&pane).matches("连接已断开").count(), 1);
    }

    #[test]
    fn session_error_reason_is_shown() {
        let (mut pane, tx, _) = remote_pane();
        tx.send(SessionEvent::Error {
            generation: 1,
            message: "写入远端失败：broken pipe".to_owned(),
        })
        .unwrap();
        assert!(pane.poll_events());
        let text = screen(&pane);
        assert!(
            text.contains("连接已断开：写入远端失败：broken pipe"),
            "{text}"
        );
    }

    #[test]
    fn offline_pane_close_does_not_claim_a_disconnect() {
        let mut pane = Pane::new(SIZE, 100, 1);
        pane.write(b"echo\r\n");
        assert!(screen(&pane).contains("echo"));
        pane.close();
        pane.poll_events();
        assert_eq!(pane.state, PaneState::Live);
        assert!(!screen(&pane).contains("连接已断开"));
    }

    #[test]
    fn notices_cannot_smuggle_escape_sequences() {
        let text = disconnect_notice(&SessionExit {
            code: None,
            reason: Some("bad\x1b]0;title\x07\r\nreason".to_owned()),
        });
        assert!(!text.contains("\x1b]0;"));
        assert!(!text.contains('\x07'));
        let text = failure_notice("e\x1b[2Jrror");
        assert!(!text.contains("\x1b[2J"));
    }

    #[test]
    fn attach_keeps_the_log_and_syncs_a_resized_window() {
        let pending = crate::ssh_backend::ready(Err("never read".to_owned()));
        let mut pane = Pane::connecting(SIZE, 100, 1, Some(7), pending);
        // 连接期间窗口变小了，也在记录日志。
        pane.resize(Size { rows: 8, cols: 40 });
        let path =
            std::env::temp_dir().join(format!("zssh-pane-attach-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        pane.pump_mut()
            .set_log(SessionLog::create(&path, 1024).unwrap());

        let (tx, rx) = std::sync::mpsc::channel();
        let sent = Arc::new(Mutex::new(Vec::new()));
        pane.attach(
            Box::new(Recorder(Arc::clone(&sent))),
            rx,
            Arc::new(FlowGate::new()),
            None,
        );
        assert_eq!(pane.state, PaneState::Live);
        assert!(pane.pump().has_log(), "重连/连上后日志要继续记");
        assert_eq!(
            *sent.lock().unwrap(),
            vec![SessionCommand::Resize(Size { rows: 8, cols: 40 })]
        );

        tx.send(SessionEvent::Data {
            generation: 1,
            bytes: b"hello\r\n".to_vec(),
        })
        .unwrap();
        assert!(pane.poll_events());
        assert!(screen(&pane).contains("hello"));
        assert_eq!(pane.snapshot.size, Size { rows: 8, cols: 40 });
        pane.close();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\r\n");
        let _ = std::fs::remove_file(&path);
    }
}

/// 标签模型：状态点怎么合成、标签信息、空白标签的判断（不起窗口，只用离线 / 占位窗格）。
#[cfg(test)]
mod tab_tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{
        Axis, LatencyMonitor, Pane, PaneState, QuickSpec, Size, Tab, TabStatus, latency,
        latency_visible_flags, split_tree, tab_status,
    };
    use std::sync::Arc;
    use std::time::Duration;

    const SIZE: Size = Size { rows: 10, cols: 40 };

    /// 按下标顺序列出窗格编号（= 屏幕上的次序）。
    fn order(tab: &Tab) -> Vec<u64> {
        tab.panes.iter().map(|pane| pane.id).collect()
    }

    fn pane_in(state: PaneState, host: Option<i64>) -> Pane {
        Pane::placeholder(SIZE, 100, 1, host, state)
    }

    fn monitor_with(rtt_ms: u64) -> Arc<LatencyMonitor> {
        let monitor = LatencyMonitor::new(latency::ProbeKind::Ssh);
        monitor.record(latency::Sample::Rtt(Duration::from_millis(rtt_ms)));
        monitor
    }

    #[test]
    fn a_pane_registers_whether_it_is_visible_and_forgets_it_on_disconnect() {
        let mut pane = pane_in(PaneState::Live, None);
        let monitor = LatencyMonitor::new(latency::ProbeKind::Ssh);
        pane.set_latency(Some(Arc::clone(&monitor)));
        assert_eq!(
            monitor.visible_viewers(),
            0,
            "接上时默认看不见，等外壳的节拍"
        );
        assert!(pane.latency_snapshot().is_none(), "还没有第一个值不画徽标");

        monitor.record(latency::Sample::Rtt(Duration::from_millis(42)));
        pane.set_latency_visible(true);
        pane.set_latency_visible(true);
        assert_eq!(monitor.visible_viewers(), 1);
        let (snapshot, _) = pane.latency_snapshot().expect("有值就画");
        assert_eq!(
            snapshot.stats.last(),
            Some(latency::Sample::Rtt(Duration::from_millis(42)))
        );

        pane.set_latency_visible(false);
        assert_eq!(monitor.visible_viewers(), 0, "切到后台标签 / 最小化：暂停");
        pane.set_latency_visible(true);
        pane.disconnect("断开");
        assert!(pane.latency.is_none() && pane.latency_snapshot().is_none());
        assert_eq!(monitor.visible_viewers(), 0, "断开后不再占着监视器");
    }

    #[test]
    fn panes_on_one_connection_share_a_value_and_count_separately() {
        let monitor = monitor_with(8);
        let mut a = pane_in(PaneState::Live, None);
        let mut b = pane_in(PaneState::Live, None);
        a.set_latency(Some(Arc::clone(&monitor)));
        b.set_latency(Some(Arc::clone(&monitor)));
        a.set_latency_visible(true);
        b.set_latency_visible(true);
        assert_eq!(monitor.visible_viewers(), 2);
        assert_eq!(
            a.latency_snapshot().map(|(s, _)| s),
            b.latency_snapshot().map(|(s, _)| s),
            "同一条连接上的窗格显示同一个值"
        );
        a.set_latency_visible(false);
        assert_eq!(monitor.visible_viewers(), 1, "还有一个窗格看得见，测量继续");
    }

    #[test]
    fn a_pane_without_a_monitor_shows_nothing_and_ignores_visibility() {
        // 串口、经跳板机的 Telnet、离线窗格：没有监视器。
        let mut pane = pane_in(PaneState::Live, None);
        pane.set_latency(None);
        pane.set_latency_visible(true);
        assert!(pane.latency_snapshot().is_none());
    }

    #[test]
    fn only_the_active_tab_of_a_shown_window_with_the_setting_on_counts_as_visible() {
        assert_eq!(
            latency_visible_flags(2, true, true, true, true),
            [true, true]
        );
        assert_eq!(
            latency_visible_flags(2, false, true, true, true),
            [false, false]
        );
        assert_eq!(latency_visible_flags(1, true, false, true, true), [false]);
        assert_eq!(latency_visible_flags(1, true, true, false, true), [false]);
        assert_eq!(latency_visible_flags(1, true, true, true, false), [false]);
    }

    #[test]
    fn the_tab_dot_prefers_connecting_then_connected_then_disconnected() {
        use PaneState::{Connecting, Disconnected, Empty, Live};
        assert_eq!(tab_status(&[Live, Connecting]), TabStatus::Connecting);
        assert_eq!(tab_status(&[Disconnected, Live]), TabStatus::Connected);
        assert_eq!(
            tab_status(&[Disconnected, Disconnected]),
            TabStatus::Disconnected
        );
        assert_eq!(tab_status(&[Empty]), TabStatus::Empty);
        assert_eq!(tab_status(&[]), TabStatus::Empty);
        // 分屏里一个断了、另一个还连着：整个标签看着还是「已连接」，重连要靠右键菜单。
        assert_eq!(tab_status(&[Live, Disconnected]), TabStatus::Connected);
    }

    #[test]
    fn a_blank_tab_has_nothing_to_duplicate_or_reconnect() {
        let tab = Tab::new("新标签".to_owned(), pane_in(PaneState::Empty, None));
        assert!(tab.is_blank());
        let info = tab.info();
        assert_eq!(info.status, TabStatus::Empty);
        assert_eq!(info.host_id, None);
        assert!(!info.can_reconnect);
    }

    fn quick_pane(state: PaneState) -> Pane {
        let mut pane = pane_in(state, None);
        pane.quick = Some(Arc::new(QuickSpec {
            target: zssh_core::model::QuickTarget {
                username: "ops".to_owned(),
                host: "192.0.2.5".to_owned(),
                port: 22,
            },
            password: "hunter2".to_owned(),
            label: "ops@192.0.2.5".to_owned(),
        }));
        pane
    }

    #[test]
    fn a_quick_connect_pane_is_a_session_even_without_a_saved_host() {
        let down = Tab::new(
            "ops@192.0.2.5".to_owned(),
            quick_pane(PaneState::Disconnected),
        );
        let info = down.info();
        assert_eq!(info.host_id, None, "快速连接没有主机记录");
        assert_eq!(info.quick_target.as_deref(), Some("ops@192.0.2.5"));
        // 归并用的键带上端口（输入时没写端口也一样），地址不分大小写。
        assert_eq!(info.quick_key.as_deref(), Some("ops@192.0.2.5:22"));
        assert!(info.can_reconnect, "断线后能重连（地址和密码都记着）");
        assert!(
            !Tab::new("x".to_owned(), pane_in(PaneState::Disconnected, None))
                .info()
                .can_reconnect,
            "离线演示窗格没有会话，不能重连"
        );
    }

    #[test]
    fn the_password_of_a_quick_connect_never_reaches_a_debug_print() {
        let pane = quick_pane(PaneState::Live);
        let text = format!("{:?}", pane.quick.as_ref().expect("有快速连接信息"));
        assert!(text.contains("ops@192.0.2.5"));
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn only_a_disconnected_host_pane_can_be_reconnected() {
        let down = Tab::new("a".to_owned(), pane_in(PaneState::Disconnected, Some(7)));
        let info = down.info();
        assert_eq!(info.status, TabStatus::Disconnected);
        assert_eq!(info.host_id, Some(7));
        assert!(info.can_reconnect);

        // 离线演示窗格（没有主机）断不断都谈不上重连。
        let offline = Tab::new("b".to_owned(), pane_in(PaneState::Disconnected, None));
        assert!(!offline.info().can_reconnect);

        let live = Tab::new("c".to_owned(), pane_in(PaneState::Live, Some(7)));
        assert!(!live.info().can_reconnect);
        assert!(!live.is_blank());
    }

    #[test]
    fn disconnect_is_offered_only_while_a_host_session_is_up_or_coming_up() {
        for (state, host, want) in [
            (PaneState::Live, Some(7), true),
            (PaneState::Connecting, Some(7), true),
            (PaneState::Disconnected, Some(7), false),
            (PaneState::Empty, None, false),
            // 离线演示窗格不是会话，没有东西可断。
            (PaneState::Live, None, false),
        ] {
            let tab = Tab::new("t".to_owned(), pane_in(state, host));
            assert_eq!(tab.info().can_disconnect, want, "{state:?} {host:?}");
        }
        let mut quick = Tab::new("q".to_owned(), quick_pane(PaneState::Live));
        assert!(quick.info().can_disconnect, "快速连接也能断");
        assert_eq!(quick.disconnect_sessions(), 1);
        assert!(
            quick.info().can_reconnect,
            "断开之后地址和密码还记着，能重连"
        );
    }

    #[test]
    fn disconnecting_a_split_tab_cuts_every_session_pane_and_keeps_the_tab() {
        let mut tab = Tab::new("a".to_owned(), pane_in(PaneState::Live, Some(1)));
        tab.split_active(Axis::X, pane_in(PaneState::Live, Some(1)));
        tab.split_active(Axis::Y, pane_in(PaneState::Connecting, Some(1)));
        assert_eq!(tab.info().status, TabStatus::Connecting);
        assert_eq!(tab.disconnect_sessions(), 3, "分屏的每个窗格都断");
        assert_eq!(tab.panes.len(), 3, "标签和窗格都还在，只是断了");
        assert!(
            tab.panes
                .iter()
                .all(|pane| pane.state == PaneState::Disconnected && pane.factory.is_none())
        );
        let info = tab.info();
        assert_eq!(info.status, TabStatus::Disconnected, "标签显示已断开");
        assert!(info.can_reconnect, "能用「重新连接」恢复");
        assert!(!info.can_disconnect);
        assert_eq!(tab.disconnect_sessions(), 0, "再断一次什么也不做");
    }

    #[test]
    fn input_goes_to_the_focused_pane_unless_the_tab_broadcasts() {
        let mut tab = Tab::new("a".to_owned(), pane_in(PaneState::Live, Some(1)));
        tab.split_active(Axis::X, pane_in(PaneState::Live, Some(1)));
        tab.split_active(Axis::Y, pane_in(PaneState::Live, Some(1)));
        tab.active_pane = 1;
        assert_eq!(tab.input_targets(), vec![1]);
        tab.broadcast = true;
        assert_eq!(
            tab.input_targets(),
            vec![0, 1, 2],
            "广播：当前标签的所有分屏窗格，而且只有它们"
        );
        // 聚焦下标越界（窗格刚被关掉）时钳到最后一个，不会 panic。
        tab.broadcast = false;
        tab.active_pane = 9;
        assert_eq!(tab.input_targets(), vec![2]);
    }

    #[test]
    fn a_tab_starts_as_a_single_pane_tree() {
        let tab = Tab::new("a".to_owned(), pane_in(PaneState::Live, Some(1)));
        assert!(!tab.is_split());
        assert_eq!(tab.tree.pane_ids(), order(&tab));
    }

    #[test]
    fn splitting_puts_the_new_pane_after_the_focused_one_and_focuses_it() {
        let mut tab = Tab::new("a".to_owned(), pane_in(PaneState::Live, Some(1)));
        let first = tab.panes[0].id;
        assert_eq!(
            tab.split_active(Axis::X, pane_in(PaneState::Live, Some(1))),
            1
        );
        assert_eq!(tab.active_pane, 1, "新窗格拿到焦点");
        // 聚焦回第一个，再上下拆：新窗格紧跟在它后面，不是排到最后。
        tab.active_pane = 0;
        assert_eq!(
            tab.split_active(Axis::Y, pane_in(PaneState::Live, Some(1))),
            1
        );
        assert_eq!(tab.panes.len(), 3);
        assert_eq!(tab.panes[0].id, first);
        assert_eq!(tab.tree.pane_ids(), order(&tab), "窗格表和树同序");
        let axes: Vec<Axis> = tab.tree.splits().iter().map(|split| split.1).collect();
        assert_eq!(axes, vec![Axis::X, Axis::Y], "先左右拆，再把左半上下拆");
    }

    #[test]
    fn a_four_pane_grid_can_be_built_and_each_pane_has_its_own_rect() {
        let mut tab = Tab::new("a".to_owned(), pane_in(PaneState::Live, Some(1)));
        tab.split_active(Axis::X, pane_in(PaneState::Live, Some(1))); // A | B
        tab.split_active(Axis::Y, pane_in(PaneState::Live, Some(1))); // B 上下：B / C
        tab.active_pane = 0;
        tab.split_active(Axis::Y, pane_in(PaneState::Live, Some(1))); // A 上下：A / D
        assert_eq!(tab.panes.len(), 4);
        assert_eq!(tab.tree.pane_ids(), order(&tab));
        let layout = split_tree::compute_layout(&tab.tree, 1004.0, 604.0);
        assert_eq!(split_tree::check_tiling(&layout, 1004.0, 604.0), Ok(()));
        let rects: Vec<_> = tab
            .panes
            .iter()
            .map(|pane| layout.rect_of(pane.id).expect("每个窗格都有矩形"))
            .collect();
        // 下标 0、1 在左半（上、下），2、3 在右半（上、下）。
        assert!(rects[0].x == rects[1].x && rects[1].y > rects[0].y);
        assert!(rects[2].x == rects[3].x && rects[3].y > rects[2].y);
        assert!(rects[2].x > rects[0].x);
    }

    #[test]
    fn closing_a_pane_merges_its_sibling_and_moves_focus_like_0_2_14() {
        let mut tab = Tab::new("a".to_owned(), pane_in(PaneState::Live, Some(1)));
        tab.split_active(Axis::X, pane_in(PaneState::Live, Some(1)));
        tab.split_active(Axis::Y, pane_in(PaneState::Live, Some(1)));
        // A | (B / C)，焦点在 C（下标 2）。
        let before = order(&tab);
        // 关的不是焦点（B）：焦点还在 C，只是下标变了。
        let closed = tab.close_pane_at(1).expect("关得掉");
        assert_eq!(closed.id, before[1]);
        assert_eq!(order(&tab), vec![before[0], before[2]]);
        assert_eq!(tab.panes[tab.active_pane].id, before[2]);
        assert_eq!(tab.tree.pane_ids(), order(&tab));
        // 剩下 A | C：关焦点窗格 C，焦点落到相邻的 A。
        let closed = tab.close_pane_at(1).expect("关得掉");
        assert_eq!(closed.id, before[2]);
        assert_eq!(tab.panes.len(), 1);
        assert_eq!(tab.active_pane, 0);
        assert!(!tab.is_split());
        // 只剩一个窗格：关不掉（调用方该关整个标签）。
        assert!(tab.close_pane_at(0).is_none());
    }

    #[test]
    fn closing_the_middle_pane_of_four_keeps_the_other_three_in_place() {
        let mut tab = Tab::new("a".to_owned(), pane_in(PaneState::Live, Some(1)));
        tab.split_active(Axis::X, pane_in(PaneState::Live, Some(1))); // A | B
        tab.active_pane = 0;
        tab.split_active(Axis::Y, pane_in(PaneState::Live, Some(1))); // A / C | B
        tab.active_pane = 2;
        tab.split_active(Axis::Y, pane_in(PaneState::Live, Some(1))); // A / C | B / D
        let ids = order(&tab); // [A, C, B, D]
        assert_eq!(ids.len(), 4);
        tab.active_pane = 1; // 焦点在 C
        let closed = tab.close_pane_at(1).expect("关得掉"); // 关掉 C
        assert_eq!(closed.id, ids[1]);
        assert_eq!(order(&tab), vec![ids[0], ids[2], ids[3]]);
        assert_eq!(tab.panes[tab.active_pane].id, ids[0], "焦点落到兄弟 A");
        // A 现在占满左半，B / D 仍是右半的上下分。
        let layout = split_tree::compute_layout(&tab.tree, 1004.0, 604.0);
        assert_eq!(split_tree::check_tiling(&layout, 1004.0, 604.0), Ok(()));
        let a = layout.rect_of(ids[0]).expect("A");
        let b = layout.rect_of(ids[2]).expect("B");
        let d = layout.rect_of(ids[3]).expect("D");
        assert_eq!(a.h, 604.0);
        assert!(b.x > a.x + a.w && d.x == b.x && d.y > b.y);
    }

    #[test]
    fn a_blank_tab_that_was_split_becomes_one_pane_when_a_host_is_chosen() {
        let mut tab = Tab::new("新标签".to_owned(), pane_in(PaneState::Empty, None));
        tab.split_active(Axis::X, pane_in(PaneState::Empty, None));
        assert!(tab.is_blank() && tab.is_split());
        let replacement = pane_in(PaneState::Connecting, Some(3));
        let id = replacement.id;
        tab.reset_to(replacement);
        assert_eq!(order(&tab), vec![id]);
        assert_eq!(tab.tree.pane_ids(), vec![id]);
        assert!(!tab.is_split());
        assert_eq!(tab.active_pane, 0);
    }

    #[test]
    fn broadcast_reaches_every_pane_of_a_split_tab() {
        let mut tab = Tab::new("a".to_owned(), pane_in(PaneState::Live, Some(1)));
        for axis in [Axis::X, Axis::Y, Axis::X] {
            tab.split_active(axis, pane_in(PaneState::Live, Some(1)));
        }
        tab.broadcast = true;
        assert_eq!(tab.input_targets(), vec![0, 1, 2, 3]);
        // 关掉中间一个之后广播的对象跟着变，不会指向已经不存在的窗格。
        tab.close_pane_at(1);
        assert_eq!(tab.input_targets(), vec![0, 1, 2]);
    }

    #[test]
    fn a_pane_uses_the_scrollback_it_was_opened_with_and_follows_later_changes() {
        let mut pane = Pane::new(SIZE, 50, 1);
        assert_eq!(pane.pump().history_limit(), 50);
        assert_eq!(pane.history, 50);
        for i in 0..120 {
            pane.write(format!("line {i}\r\n").as_bytes());
        }
        assert!(pane.pump().history_size() <= 50);
        assert!(pane.pump().history_size() > 30, "历史应该攒起来了");
        // 调小：已有的历史立刻被裁。
        pane.set_history(10);
        assert_eq!(pane.pump().history_limit(), 10);
        assert!(pane.pump().history_size() <= 10);
        // 调大：之后能攒更多，重连新建的泵也用新值。
        pane.set_history(80);
        for i in 0..200 {
            pane.write(format!("more {i}\r\n").as_bytes());
        }
        let size = pane.pump().history_size();
        assert!(size > 10 && size <= 80, "history={size}");
        assert_eq!(pane.history, 80);
    }

    #[test]
    fn tab_ids_are_unique_across_tabs() {
        let a = Tab::new("a".to_owned(), pane_in(PaneState::Empty, None));
        let b = Tab::new("b".to_owned(), pane_in(PaneState::Empty, None));
        assert_ne!(a.id, b.id, "标签能在窗口之间搬，id 不能撞");
    }
}

/// 联网验证：真实会话里的大输出、Ctrl+C、less、断线和重连，走和界面相同的
/// `Pane` 路径（后台连接 → 60 ms 轮询 → 账本/流控）。需要 `ZSSH_LIVE_FULL=1` 和
/// `ZSSH_TEST_HOST/PORT/USER` + `ZSSH_TEST_KEY_PATH`；缺了就打印原因跳过。
#[cfg(test)]
mod live_tests {
    use super::{Pane, PaneState, Size};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use zssh_core::model::ProxyKind;
    use zssh_core::ssh::{
        AuthConfig, BoxFuture, ChainHop, Connection, HostKeyDecision, HostKeyInfo, Interactor,
        KeySource, KnownHosts, PromptAnswers, PromptRequest, ProxyConfig,
    };

    const LIVE: Size = Size {
        rows: 30,
        cols: 120,
    };

    /// 测试里没有人点对话框：主机指纹仅本次信任，其余提问一律取消。
    struct TrustOnce;

    impl Interactor for TrustOnce {
        fn confirm_host_key(&self, _info: HostKeyInfo) -> BoxFuture<'_, HostKeyDecision> {
            Box::pin(async { HostKeyDecision::TrustOnce })
        }

        fn prompt(&self, _request: PromptRequest) -> BoxFuture<'_, Option<PromptAnswers>> {
            Box::pin(async { None })
        }
    }

    fn env(name: &str) -> Option<String> {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    }

    fn proxy_from_env() -> Option<ProxyConfig> {
        let raw = env("ZSSH_TEST_SOCKS5")?;
        let (host, port) = raw.rsplit_once(':')?;
        Some(ProxyConfig {
            kind: ProxyKind::Socks5,
            host: host.to_owned(),
            port: port.parse().ok()?,
            username: None,
            password: None,
        })
    }

    fn screen(pane: &Pane) -> String {
        pane.snapshot
            .rows
            .iter()
            .map(|row| row.text.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn has_line(pane: &Pane, wanted: &str) -> bool {
        pane.snapshot
            .rows
            .iter()
            .any(|row| row.text.trim() == wanted)
    }

    /// 像界面一样每 60 ms 轮询一次，直到条件成立或超时。
    fn wait_until(pane: &mut Pane, timeout: Duration, mut done: impl FnMut(&Pane) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            pane.poll_events();
            if done(pane) {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
    }

    fn type_line(pane: &mut Pane, line: &str) {
        pane.write(format!("{line}\r").as_bytes());
    }

    #[test]
    fn real_session_flood_interrupt_less_disconnect_and_reopen() {
        if env("ZSSH_LIVE_FULL").as_deref() != Some("1") {
            println!("跳过：没有设置 ZSSH_LIVE_FULL=1");
            return;
        }
        let (Some(host), Some(user), Some(key)) = (
            env("ZSSH_TEST_HOST"),
            env("ZSSH_TEST_USER"),
            env("ZSSH_TEST_KEY_PATH"),
        ) else {
            println!("跳过：缺少 ZSSH_TEST_HOST / ZSSH_TEST_USER / ZSSH_TEST_KEY_PATH");
            return;
        };
        let port = env("ZSSH_TEST_PORT")
            .and_then(|raw| raw.parse::<u16>().ok())
            .unwrap_or(22);
        let known_hosts = std::env::temp_dir().join(format!("zssh-f1-kh-{}", std::process::id()));
        let hop = ChainHop {
            label: "测试服务器".to_owned(),
            host,
            port,
            username: user,
            proxy: proxy_from_env(),
            auth: AuthConfig::key(KeySource::File {
                path: key.into(),
                passphrase: env("ZSSH_TEST_KEY_PASSPHRASE"),
            }),
            known_hosts: KnownHosts::new(&known_hosts),
            legacy_algorithms: false,
        };
        let runtime = crate::ssh_backend::runtime().expect("runtime");
        let connection = runtime
            .block_on(Connection::connect_chain(vec![hop], Arc::new(TrustOnce)))
            .expect("连接测试服务器");
        let factory = crate::ssh_backend::SshSessionFactory::from_connection(connection);

        // 1. 打开真实会话：先是"正在连接"，轮询后接上。
        let mut pane = Pane::start_ssh(Some(Arc::clone(&factory)), Some(1), LIVE, 10_000, 1);
        assert_eq!(pane.state, PaneState::Connecting);
        assert!(
            wait_until(&mut pane, Duration::from_secs(20), |p| p.state
                == PaneState::Live),
            "会话应在 20 秒内接上：{}",
            screen(&pane)
        );
        // 全程只在自己的 /tmp/zssh-f1 下操作。
        type_line(
            &mut pane,
            "mkdir -p /tmp/zssh-f1 && cd /tmp/zssh-f1 && echo READY_$((40+2))",
        );
        assert!(
            wait_until(&mut pane, Duration::from_secs(20), |p| has_line(
                p, "READY_42"
            )),
            "shell 应能交互：{}",
            screen(&pane)
        );

        // 2. 大输出（B-1）：约 14 MB，累计远超 1 MiB 高水位，之后仍能交互。
        let started = Instant::now();
        type_line(&mut pane, "seq 1 2000000");
        assert!(
            wait_until(&mut pane, Duration::from_secs(180), |p| has_line(
                p, "2000000"
            )),
            "大输出应全部显示完；账本 {:?}",
            pane.pump().ledger()
        );
        let flood = started.elapsed();
        wait_until(&mut pane, Duration::from_secs(5), |p| {
            p.pump().gate().unacked() == 0
        });
        let ledger = pane.pump().ledger();
        println!(
            "大输出：{} 字节，用时 {:?}，未确认 {}",
            ledger.raw,
            flood,
            pane.pump().gate().unacked()
        );
        assert!(ledger.raw >= 14_000_000, "输出量不对：{ledger:?}");
        assert_eq!(pane.pump().gate().unacked(), 0, "流控账必须回到 0");
        type_line(&mut pane, "echo AFTER_$((1+2))");
        assert!(
            wait_until(&mut pane, Duration::from_secs(10), |p| has_line(
                p, "AFTER_3"
            )),
            "大输出之后还要能交互：{}",
            screen(&pane)
        );

        // 3. Ctrl+C 中断 sleep 100：^C 字节真的送到远端。
        let started = Instant::now();
        type_line(&mut pane, "sleep 100");
        std::thread::sleep(Duration::from_millis(800));
        pane.write(b"\x03");
        type_line(&mut pane, "echo INT_$((20+2))");
        assert!(
            wait_until(&mut pane, Duration::from_secs(10), |p| has_line(
                p, "INT_22"
            )),
            "Ctrl+C 应当中断 sleep：{}",
            screen(&pane)
        );
        println!("Ctrl+C 中断 sleep 100：{:?}", started.elapsed());
        assert!(started.elapsed() < Duration::from_secs(20));

        // 4. less 进入再退出。
        type_line(&mut pane, "seq 1 200 > f.txt; less f.txt");
        assert!(
            // less 只画第一屏，底部提示行带文件名；看不到 199。
            wait_until(&mut pane, Duration::from_secs(10), |p| has_line(p, "10")
                && screen(p).contains("f.txt")),
            "less 应当显示文件内容：{}",
            screen(&pane)
        );
        std::thread::sleep(Duration::from_millis(500));
        pane.write(b"q");
        type_line(&mut pane, "echo LESS_$((5+5))");
        assert!(
            wait_until(&mut pane, Duration::from_secs(10), |p| has_line(
                p, "LESS_10"
            )),
            "q 之后应回到 shell：{}",
            screen(&pane)
        );

        // 5. 清理自己的目录，然后杀掉自己的 shell：界面应提示已断开。
        type_line(
            &mut pane,
            "cd /tmp && rm -rf /tmp/zssh-f1 && echo CLEAN_$((7+1))",
        );
        assert!(wait_until(
            &mut pane,
            Duration::from_secs(10),
            |p| has_line(p, "CLEAN_8")
        ));
        type_line(&mut pane, "kill -9 $$");
        assert!(
            wait_until(&mut pane, Duration::from_secs(20), |p| p.state
                == PaneState::Disconnected),
            "shell 被杀后窗格应显示已断开：{}",
            screen(&pane)
        );
        let text = screen(&pane);
        assert!(text.contains("连接已断开"), "{text}");
        assert!(text.contains("Ctrl+Shift+R"), "{text}");
        // 断开后的输入不回显、不发送。
        pane.write(b"must-not-appear\r");
        assert!(!screen(&pane).contains("must-not-appear"));
        println!(
            "断线提示：{}",
            text.lines()
                .filter(|line| line.contains("连接已断开"))
                .collect::<Vec<_>>()
                .join(" | ")
        );

        // 6. 重连：同一条连接上重新开 shell，窗格恢复可用。
        let mut again = Pane::start_ssh(Some(factory), Some(1), LIVE, 10_000, 1);
        assert!(wait_until(&mut again, Duration::from_secs(20), |p| p.state
            == PaneState::Live));
        type_line(&mut again, "echo RECONNECT_$((3+4))");
        assert!(
            wait_until(&mut again, Duration::from_secs(10), |p| has_line(
                p,
                "RECONNECT_7"
            )),
            "重连后应可交互：{}",
            screen(&again)
        );
        again.close();
        let _ = std::fs::remove_file(&known_hosts);
    }

    /// 连上进程内的迷你 SSH 服务器（本机回环，不碰任何外部主机）。
    fn local_factory() -> Arc<crate::ssh_backend::SshSessionFactory> {
        let runtime = crate::ssh_backend::runtime().expect("runtime");
        let addr = runtime.block_on(crate::test_ssh_server::spawn());
        let known_hosts = std::env::temp_dir().join(format!(
            "zssh-f1-local-kh-{}-{}",
            std::process::id(),
            addr.port()
        ));
        let hop = ChainHop {
            label: "本机测试服务器".to_owned(),
            host: addr.ip().to_string(),
            port: addr.port(),
            username: "tester".to_owned(),
            proxy: None,
            auth: AuthConfig::password(Some("pw".to_owned())),
            known_hosts: KnownHosts::new(&known_hosts),
            legacy_algorithms: false,
        };
        let connection = runtime
            .block_on(Connection::connect_chain(vec![hop], Arc::new(TrustOnce)))
            .expect("连接本机测试服务器");
        let _ = std::fs::remove_file(&known_hosts);
        crate::ssh_backend::SshSessionFactory::from_connection(connection)
    }

    fn open_live(factory: &Arc<crate::ssh_backend::SshSessionFactory>) -> Pane {
        let mut pane = Pane::start_ssh(Some(Arc::clone(factory)), Some(1), LIVE, 10_000, 1);
        assert_eq!(pane.state, PaneState::Connecting);
        assert!(
            wait_until(&mut pane, Duration::from_secs(20), |p| has_line(
                p,
                "MINI_READY"
            )),
            "会话应接上：{}",
            screen(&pane)
        );
        assert_eq!(pane.state, PaneState::Live);
        pane
    }

    /// 真连进程内迷你 SSH 服务器：开 shell 就起这条连接的延迟测量，窗格从连接上取到同一份监视器；
    /// 看不见时不测，看得见时测到真实往返；同一条连接上的第二个窗格拿到的是同一个监视器。
    #[test]
    fn a_local_ssh_pane_measures_a_real_round_trip_only_while_visible() {
        let factory = local_factory();
        let mut first = open_live(&factory);
        let mut second = open_live(&factory);
        let monitor = Arc::clone(factory.connection().latency());
        assert!(monitor.is_started(), "开 shell 时起了测量任务");
        for pane in [&first, &second] {
            let own = pane.latency.as_ref().expect("SSH 窗格有延迟监视器");
            assert!(
                Arc::ptr_eq(own.viewer.monitor(), &monitor),
                "共用同一个监视器"
            );
        }

        // 都没登记成看得见：等一阵也不测。
        std::thread::sleep(Duration::from_millis(800));
        assert!(monitor.snapshot().stats.is_empty());

        first.set_latency_visible(true);
        assert!(
            wait_until(&mut first, Duration::from_secs(10), |p| p
                .latency_snapshot()
                .is_some()),
            "看得见之后应当很快测到第一个值"
        );
        let (snapshot, _) = first.latency_snapshot().expect("有值");
        assert!(matches!(
            snapshot.stats.last(),
            Some(zssh_core::ssh::latency::Sample::Rtt(rtt)) if rtt > Duration::ZERO
        ));
        assert_eq!(
            second.latency_snapshot().map(|(s, _)| s),
            Some(snapshot),
            "同一条连接上的另一个窗格读到同一个值"
        );

        first.close();
        second.close();
    }

    /// B-1 的端到端验证：真实 russh 传输 + 核心流控，输出 6 MiB（高水位的 6 倍）
    /// 仍然全部显示，账本回到 0，之后还能交互；Ctrl+C 字节到达远端；
    /// 远端退出和整条连接被断开都会让窗格显示"已断开"。
    #[test]
    fn local_server_flood_ctrl_c_exit_and_dropped_connection() {
        let factory = local_factory();

        let mut pane = open_live(&factory);
        type_line(&mut pane, "hello");
        assert!(wait_until(
            &mut pane,
            Duration::from_secs(10),
            |p| has_line(p, "ECHO:hello")
        ));

        let started = Instant::now();
        type_line(&mut pane, "flood");
        assert!(
            wait_until(&mut pane, Duration::from_secs(60), |p| has_line(
                p,
                "FLOOD_DONE"
            )),
            "大输出应全部显示完；账本 {:?}，未确认 {}",
            pane.pump().ledger(),
            pane.pump().gate().unacked()
        );
        println!(
            "本机服务器大输出：{} 字节，用时 {:?}",
            pane.pump().ledger().raw,
            started.elapsed()
        );
        assert!(pane.pump().ledger().raw >= crate::test_ssh_server::FLOOD_BYTES);
        wait_until(&mut pane, Duration::from_secs(5), |p| {
            p.pump().gate().unacked() == 0
        });
        assert_eq!(pane.pump().gate().unacked(), 0, "流控账必须回到 0");
        type_line(&mut pane, "after");
        assert!(wait_until(
            &mut pane,
            Duration::from_secs(10),
            |p| has_line(p, "ECHO:after")
        ));

        pane.write(b"\x03");
        assert!(wait_until(
            &mut pane,
            Duration::from_secs(10),
            |p| has_line(p, "GOT_CTRL_C")
        ));

        // 远端退出：显示退出码，之后的输入不再发出。
        type_line(&mut pane, "bye");
        assert!(wait_until(&mut pane, Duration::from_secs(10), |p| p.state
            == PaneState::Disconnected));
        assert!(
            screen(&pane).contains("连接已断开（退出码 3）"),
            "{}",
            screen(&pane)
        );
        pane.write(b"ignored\r");
        assert!(!screen(&pane).contains("ECHO:ignored"));

        // 同一条连接上重新开一个会话（重连）。
        let mut again = open_live(&factory);
        type_line(&mut again, "again");
        assert!(wait_until(&mut again, Duration::from_secs(10), |p| {
            has_line(p, "ECHO:again")
        }));

        // 整条连接被断开：窗格提示断开；再往这条死连接上开 shell 要明确失败，不能卡住。
        type_line(&mut again, "drop");
        assert!(
            wait_until(&mut again, Duration::from_secs(20), |p| p.state
                == PaneState::Disconnected),
            "{}",
            screen(&again)
        );
        assert!(screen(&again).contains("连接已断开"), "{}", screen(&again));

        let mut dead = Pane::start_ssh(Some(factory), Some(1), LIVE, 10_000, 1);
        assert!(
            wait_until(&mut dead, Duration::from_secs(30), |p| p.state
                == PaneState::Disconnected),
            "死连接上开 shell 应当明确失败：{}",
            screen(&dead)
        );
        assert!(screen(&dead).contains("Ctrl+Shift+R"), "{}", screen(&dead));
        assert_eq!(dead.host_id, Some(1), "失败后仍可重连");
    }
    /// V2-U1 第 3 条：标签右键「断开连接」只放掉这个窗格自己的那份连接引用。同一条 SSH 连接上
    /// 另一个窗格（别的标签 / 分屏 / SFTP）照常能用；最后一个使用者走了，连接才真的释放。
    #[test]
    fn disconnecting_one_pane_leaves_the_shared_connection_to_the_others() {
        let factory = local_factory();
        let weak_connection = Arc::downgrade(&factory.connection());
        let mut a = open_live(&factory);
        let mut b = open_live(&factory);
        let before = Arc::strong_count(&factory);
        assert!(before >= 3, "测试自己 + 两个窗格各持一份：{before}");

        a.disconnect_by_user();
        assert_eq!(a.state, PaneState::Disconnected);
        assert!(a.factory.is_none(), "放掉自己那份引用");
        assert_eq!(
            Arc::strong_count(&factory),
            before - 1,
            "只少了这个窗格的一份"
        );
        assert!(screen(&a).contains("已断开连接（手动）"), "{}", screen(&a));
        assert!(screen(&a).contains("Ctrl+Shift+R"), "{}", screen(&a));
        // 断开之后输入不再发出，也不回显。
        a.write(b"must-not-appear\r");
        assert!(!screen(&a).contains("ECHO:must-not-appear"));

        // 另一个窗格不受影响：同一条连接照常收发。
        assert_eq!(b.state, PaneState::Live);
        assert!(!factory.connection().is_closed(), "连接还有别人在用");
        type_line(&mut b, "still-here");
        assert!(
            wait_until(&mut b, Duration::from_secs(10), |p| has_line(
                p,
                "ECHO:still-here"
            )),
            "{}",
            screen(&b)
        );
        // 断开的这个可以在同一条连接上重新连。
        let mut again = open_live(&factory);
        type_line(&mut again, "back");
        assert!(wait_until(&mut again, Duration::from_secs(10), |p| {
            has_line(p, "ECHO:back")
        }));

        // 全部断开、测试自己也放手：没人用了，连接才真的释放。
        b.disconnect_by_user();
        again.disconnect_by_user();
        drop(factory);
        let released = (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(100));
            weak_connection.upgrade().is_none()
        });
        assert!(released, "最后一个使用者走了之后连接应当释放");
    }

    /// 还没连上（正在连接）的窗格也能断开：后台的连接任务随之取消，窗格显示已断开。
    #[test]
    fn a_connecting_pane_can_be_disconnected_too() {
        let factory = local_factory();
        let mut pane = Pane::start_ssh(Some(Arc::clone(&factory)), Some(1), LIVE, 10_000, 1);
        assert_eq!(pane.state, PaneState::Connecting);
        assert!(pane.can_disconnect());
        pane.disconnect_by_user();
        assert_eq!(pane.state, PaneState::Disconnected);
        assert!(pane.pending.is_none(), "后台任务一并取消");
        // 之后不会有迟到的结果把它又变回已连接。
        std::thread::sleep(Duration::from_millis(500));
        pane.poll_events();
        assert_eq!(pane.state, PaneState::Disconnected);
    }
}

#[cfg(test)]
mod blink_tests {
    use super::{BLINK_HALF_PERIOD, BlinkClock};
    use std::time::Duration;

    #[test]
    fn the_cursor_is_lit_for_half_a_period_then_dark_and_repeats() {
        let half = BLINK_HALF_PERIOD;
        assert!(BlinkClock::phase_at(Duration::ZERO), "刚打字：亮着");
        assert!(BlinkClock::phase_at(half - Duration::from_millis(1)));
        assert!(!BlinkClock::phase_at(half), "半个周期后灭");
        assert!(!BlinkClock::phase_at(half * 2 - Duration::from_millis(1)));
        assert!(BlinkClock::phase_at(half * 2), "一个周期后又亮");
    }

    #[test]
    fn a_fresh_clock_starts_lit_and_reports_no_flip() {
        let mut clock = BlinkClock::new();
        assert!(clock.visible());
        assert!(!clock.changed(), "相位没变就不用重画");
    }
}

#[cfg(test)]
mod protocol_session_tests {
    use super::{Pane, PaneState, Size};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use zssh_core::ssh::flow::FlowGate;
    use zssh_session::{CoreFilter, SessionBackend, SessionCommand, SessionPump};

    /// 记录 Break 次数的写端。
    struct BreakCounter(Arc<AtomicUsize>);

    impl SessionBackend for BreakCounter {
        fn send(&mut self, _command: SessionCommand) {}

        fn send_break(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn pane_with(counter: Arc<AtomicUsize>) -> Pane {
        let pump = SessionPump::new(
            Size { rows: 5, cols: 20 },
            100,
            Arc::new(FlowGate::new()),
            CoreFilter::new(),
            1,
        );
        let (_, events) = std::sync::mpsc::channel();
        Pane::with_parts(pump, Box::new(BreakCounter(counter)), events, Some(1), None)
    }

    /// 窗格的 Break 命令只在会话活着时送到写端；断开之后不再发。
    #[test]
    fn send_break_reaches_the_backend_only_while_the_session_is_live() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut pane = pane_with(Arc::clone(&counter));
        pane.send_break();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        pane.state = PaneState::Disconnected;
        pane.send_break();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    /// 终端窗格接上一个没有 SSH 工厂的会话（Telnet）：横幅显示在屏幕上，键入送到服务器，
    /// 对端关闭之后窗格显示已断开。走的就是 SSH 会话同一条 `attach` 路径。
    #[test]
    fn a_telnet_session_attaches_to_a_pane_without_an_ssh_factory() {
        use std::time::{Duration, Instant};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let runtime = crate::ssh_backend::runtime().expect("runtime");
        let (opened, mut server_rx) = runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            tokio::spawn(async move {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let _ = socket.write_all(b"PANE_OK\r\n").await;
                let mut buffer = [0u8; 256];
                loop {
                    match socket.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            let chunk = buffer[..read].to_vec();
                            if chunk.windows(4).any(|w| w == b"quit") {
                                let _ = tx.send(chunk);
                                let _ = socket.shutdown().await;
                                break;
                            }
                            let _ = tx.send(chunk);
                        }
                    }
                }
            });
            let socket = tokio::net::TcpStream::connect(addr).await.unwrap();
            let opened = crate::protocol_backend::start_telnet(
                Box::new(socket),
                Box::new(()),
                zssh_core::model::TelnetOptions::default(),
                String::new(),
                None,
                Size { rows: 5, cols: 40 },
                1,
            );
            (opened, rx)
        });
        let pending = crate::ssh_backend::ready(Ok(opened));
        let mut pane = Pane::connecting(Size { rows: 5, cols: 40 }, 100, 1, Some(1), pending);
        assert_eq!(pane.state, PaneState::Connecting);

        let screen = |pane: &Pane| -> String {
            pane.snapshot
                .rows
                .iter()
                .map(|row| row.text.clone())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !screen(&pane).contains("PANE_OK") {
            pane.poll_events();
            assert!(Instant::now() < deadline, "横幅没有出现：{}", screen(&pane));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(pane.state, PaneState::Live);
        assert!(pane.factory.is_none());
        assert!(screen(&pane).contains("Telnet 不加密"));

        pane.write(b"quit\r");
        let seen = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut all = Vec::new();
                while let Some(chunk) = server_rx.recv().await {
                    all.extend_from_slice(&chunk);
                    if all.windows(4).any(|w| w == b"quit") {
                        break;
                    }
                }
                all
            })
            .await
        });
        assert!(seen.is_ok(), "服务器没有收到键入");

        let deadline = Instant::now() + Duration::from_secs(10);
        while pane.state != PaneState::Disconnected {
            pane.poll_events();
            assert!(Instant::now() < deadline, "对端关闭后窗格应当显示已断开");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
