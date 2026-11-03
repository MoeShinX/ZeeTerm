//! 「进程管理」对话框（V2-U18）：概览面板「进程」一节的按钮打开，居中的大对话框，列出这台服务器上的进程，
//! 按 CPU / 内存排序、搜索，选中一行可以「结束进程」(SIGTERM) 或「强制结束」(SIGKILL)。
//!
//! 套路同「所有网络连接」（[`super::connections_ui`]）：
//! - 采集在 `zssh_core::ssh::processes`（一次只读 exec，5 秒超时）：点开和点「刷新」各执行一次，**不自动轮询**，
//!   可以打开「每 3 秒自动刷新」（默认关）。走当前标签已有的那条 SSH 连接，不新建连接、不往终端里打字；
//! - 搜索 / 排序都是 core 里的纯函数，这里只画：表格是虚拟列表；
//! - **结束进程是写操作**：只能在这个界面里由用户点，必须先弹确认框（写清 PID、用户、命令行，PID 1 和承载本次
//!   SSH 连接的进程特别警示），Enter 不会替用户确认；真正执行的 `terminate_process` 先重新采一次核对 PID 还是
//!   选中的那个进程，没权限就如实提示、**绝不自动用 sudo**；结束后 1 秒自动刷新一次，显示结果。
//!   这个动作没有接进 AI 工具桥。

use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use ::gpui::{MouseButton, SharedString, UniformListScrollHandle, Window, hsla, px, uniform_list};
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::ssh::processes::view_indices;
use zssh_core::ssh::{
    KillError, KillSignal, KillVerdict, ProcMode, ProcRisk, ProcRow, ProcSort, ProcSortKey,
    ProcessReport, ProcessesError, Protection, elapsed_label, kill_verdict, terminate_process,
};

use super::MainWindow;
use super::bridge::on_ssh_runtime;
use super::modal::Modal;
use super::widgets::{ButtonLook, segmented, text_button};
use crate::app_theme::Tokens;
use crate::icons::Ico;
use crate::typography::{Role, Typo as _};

/// 对话框卡片的宽度。
pub const DIALOG_WIDTH: f32 = 1060.0;
/// 自动刷新的间隔。
const AUTO_REFRESH: Duration = Duration::from_secs(3);
/// 结束进程之后多久刷新一次看结果。
pub const VERIFY_DELAY: Duration = Duration::from_secs(1);
const ROW_HEIGHT: f32 = 26.0;
/// 表格各列的宽度（最后一列「命令」吃剩下的）。
const COL_PID: f32 = 64.0;
const COL_USER: f32 = 96.0;
const COL_CPU: f32 = 64.0;
const COL_MEM: f32 = 64.0;
const COL_RSS: f32 = 88.0;
const COL_STATE: f32 = 60.0;
const COL_TIME: f32 = 96.0;

// ---- 状态 ----

/// 确认框要写的字（纯数据，单测核对文案）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskText {
    pub title: &'static str,
    /// `PID 812 · 用户 app`。
    pub who: String,
    pub command: String,
    /// 这个动作会发生什么。
    pub effect: String,
    /// 额外的警示，按严重程度。
    pub warnings: Vec<(Warn, String)>,
    pub confirm_label: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Warn {
    /// 会让服务器崩溃 / 断开连接：红色。
    Danger,
    /// 可能做不成 / 没有效果：琥珀色。
    Caution,
    /// 补充说明（不是警告）：淡色。
    Note,
}

/// 确认框的文案。`uid` / `login_user` 是当前登录用户（来自采集）。
pub fn ask_text(
    row: &ProcRow,
    signal: KillSignal,
    risk: ProcRisk,
    login_user: Option<&str>,
    uid: Option<u32>,
) -> AskText {
    let effect = match signal {
        KillSignal::Term => {
            "这会结束服务器上的这个进程：向它发送 SIGTERM，请它自行退出（进程可以不理会）。"
        }
        KillSignal::Kill => {
            "这会强制结束服务器上的这个进程：发送 SIGKILL，立即终止，来不及保存数据或清理。"
        }
    };
    let mut warnings = Vec::new();
    match risk {
        ProcRisk::Init => warnings.push((
            Warn::Danger,
            "这是 PID 1（系统的初始化进程），结束它可能让服务器崩溃或重启。".to_owned(),
        )),
        ProcRisk::SshSession => warnings.push((
            Warn::Danger,
            "这是承载你当前 SSH 连接的进程，结束它会断开现在的连接（也可能让 sshd 不再接受新的登录）。"
                .to_owned(),
        )),
        ProcRisk::Normal => {}
    }
    if row.is_zombie() {
        let parent = row
            .ppid
            .map(|p| format!("，需要处理它的父进程（PID {p}）"))
            .unwrap_or_default();
        warnings.push((
            Warn::Caution,
            format!("这是僵尸进程（已经结束，只等父进程回收），再结束它不会有任何效果{parent}。"),
        ));
    }
    let not_root = uid.is_some_and(|uid| uid != 0);
    let other_owner = login_user.is_none_or(|me| me != row.user);
    if not_root && other_owner && !row.user.is_empty() {
        let me = login_user
            .map(|me| format!("你现在是 {me}"))
            .unwrap_or_else(|| "你现在不是 root".to_owned());
        warnings.push((
            Warn::Caution,
            format!(
                "这个进程属于 {}，{me}，可能没有权限结束它（不会自动使用 sudo）。",
                row.user
            ),
        ));
    }
    AskText {
        title: match signal {
            KillSignal::Term => "结束进程",
            KillSignal::Kill => "强制结束进程",
        },
        who: format!("PID {} · 用户 {}", row.pid, row.user),
        command: if row.command.is_empty() {
            "（没有命令行）".to_owned()
        } else {
            row.command.clone()
        },
        effect: effect.to_owned(),
        warnings,
        confirm_label: signal.label(),
    }
}

/// 结束进程的确认框状态。
pub struct KillAsk {
    pub target: ProcRow,
    pub signal: KillSignal,
    pub risk: ProcRisk,
    /// 已经点了确认、正在执行：不能再点、不能关。
    pub busy: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Success,
    Warn,
    Error,
}

/// 结束进程之后表格上方的一行结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KillNotice {
    pub level: NoticeLevel,
    pub text: String,
}

fn short(command: &str) -> String {
    let command = command.trim();
    if command.chars().count() > 60 {
        let cut: String = command.chars().take(60).collect();
        format!("{cut}…")
    } else {
        command.to_owned()
    }
}

/// 信号发出去了：先告诉用户在等结果。
pub fn sent_notice(target: &ProcRow, signal: KillSignal) -> KillNotice {
    KillNotice {
        level: NoticeLevel::Info,
        text: format!(
            "已向 PID {} 发送 SIG{}，正在确认结果…",
            target.pid,
            signal.name()
        ),
    }
}

/// 结束没成功（信号没发出去）。没权限时点出进程属于谁、当前用户是谁，提示需要 root 或 sudo。
pub fn failure_notice(err: &KillError, target: &ProcRow, login_user: Option<&str>) -> KillNotice {
    let level = match err {
        KillError::NoSuchProcess | KillError::Changed => NoticeLevel::Warn,
        _ => NoticeLevel::Error,
    };
    KillNotice {
        level,
        text: err.describe(&target.user, login_user),
    }
}

/// 结束后再采一次的结果：已结束 / 僵尸 / 仍在运行。
pub fn verdict_notice(
    verdict: KillVerdict,
    target: &ProcRow,
    signal: KillSignal,
    now: Option<&ProcRow>,
) -> KillNotice {
    let name = short(&target.command);
    match verdict {
        KillVerdict::Gone => KillNotice {
            level: NoticeLevel::Success,
            text: format!("已结束：PID {}（{name}）。", target.pid),
        },
        KillVerdict::Zombie => KillNotice {
            level: NoticeLevel::Warn,
            text: format!(
                "PID {} 已经结束，现在是僵尸进程，等它的父进程回收{}。",
                target.pid,
                now.and_then(|r| r.ppid)
                    .map(|p| format!("（PID {p}）"))
                    .unwrap_or_default()
            ),
        },
        KillVerdict::StillRunning => KillNotice {
            level: NoticeLevel::Warn,
            text: match signal {
                KillSignal::Term => format!(
                    "已发送 SIGTERM，但 PID {} 仍在运行（它可能忽略了信号，或还在退出）。可以稍后刷新，或使用「强制结束」。",
                    target.pid
                ),
                KillSignal::Kill => format!(
                    "已发送 SIGKILL，但 PID {} 仍在运行（可能卡在内核里等待，状态 {}）。",
                    target.pid,
                    now.map_or("?", |r| r.state.as_str())
                ),
            },
        },
    }
}

/// 结束之后要在 `due` 刷新一次、再给出结果。
pub struct Verify {
    pub target: ProcRow,
    pub signal: KillSignal,
    pub due: Instant,
    /// 已经发出了确认用的那次刷新。
    pub refreshing: bool,
}

/// 对话框的状态；搜索框是主窗口自己的 `proc_query`。
pub struct ProcessesDialog {
    pub host_id: i64,
    pub host_name: String,
    pub report: Option<ProcessReport>,
    /// 最近一次采集失败的原因（已有数据时只在表格上方显示一行，数据还留着）。
    pub error: Option<String>,
    pub loading: bool,
    /// 采集完成的时刻（本地时间 HH:MM:SS）。
    pub fetched: Option<String>,
    pub mode: ProcMode,
    pub sort: ProcSort,
    /// 「每 3 秒自动刷新」，默认关。
    pub auto: bool,
    /// 搜索 / 排序之后要显示的行（下标指向 `report.rows`）。
    pub shown: Vec<usize>,
    /// 现在在等的那次采集的编号：关掉对话框再打开后，迟到的旧结果不能写进新对话框。
    pub generation: u64,
    last_done: Option<Instant>,
    pub scroll: UniformListScrollHandle,
    /// 选中的进程（按 PID 记，刷新后行换了位置也跟着）。
    pub selected: Option<u32>,
    pub ask: Option<KillAsk>,
    pub notice: Option<KillNotice>,
    pub verify: Option<Verify>,
    /// 下个节拍要刷新一次（列表过时了：PID 变了、进程已经不在）。
    pub want_refresh: bool,
    /// 从"所有网络连接"的右键菜单「在进程管理中查看」打开：第一次采到进程后选中它并滚到可见处。
    pub pending_focus: Option<u32>,
}

impl ProcessesDialog {
    pub fn new(host_id: i64, host_name: String, generation: u64, mode: ProcMode) -> Self {
        Self {
            host_id,
            host_name,
            report: None,
            error: None,
            loading: false,
            fetched: None,
            mode,
            sort: ProcSort::default(),
            auto: false,
            shown: Vec::new(),
            generation,
            last_done: None,
            scroll: UniformListScrollHandle::new(),
            selected: None,
            ask: None,
            notice: None,
            verify: None,
            want_refresh: false,
            pending_focus: None,
        }
    }

    pub fn refilter(&mut self, query: &str) {
        self.shown = match &self.report {
            Some(report) => view_indices(&report.rows, self.mode, query, self.sort),
            None => Vec::new(),
        };
        self.scroll.scroll_to_item(0, ::gpui::ScrollStrategy::Top);
    }

    /// 要定位的 PID（见 `pending_focus`）：在表里就选中它并滚到中间，不在（已经退出）就算了。只做一次。
    pub fn apply_pending_focus(&mut self) {
        let Some(pid) = self.pending_focus.take() else {
            return;
        };
        let Some(report) = &self.report else {
            return;
        };
        let Some(at) = self
            .shown
            .iter()
            .position(|index| report.rows.get(*index).is_some_and(|row| row.pid == pid))
        else {
            return;
        };
        self.selected = Some(pid);
        self.scroll
            .scroll_to_item(at, ::gpui::ScrollStrategy::Center);
    }

    /// 选中的进程受不受保护（受保护的不能结束，按钮置灰）。
    pub fn selected_protection(&self) -> Option<Protection> {
        let pid = self.selected?;
        self.report.as_ref()?.protection(pid)
    }

    pub fn selected_row(&self) -> Option<&ProcRow> {
        let pid = self.selected?;
        self.report.as_ref()?.row(pid)
    }

    /// 该不该再采一次：没在采，并且（开着自动刷新且距离上次完成已满 3 秒、或列表过时了、或到了结束后的确认时刻）。
    pub fn refresh_due(&self, now: Instant) -> bool {
        if self.loading {
            return false;
        }
        let auto = self.auto
            && self
                .last_done
                .is_none_or(|done| now.duration_since(done) >= AUTO_REFRESH);
        let verify = self
            .verify
            .as_ref()
            .is_some_and(|v| !v.refreshing && now >= v.due);
        auto || self.want_refresh || verify
    }

    /// 确认框开着且已点了确认：对话框不能关。
    pub fn busy(&self) -> bool {
        self.ask.as_ref().is_some_and(|ask| ask.busy)
    }
}

/// 「HH:MM:SS」本地时间。
fn clock_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
        + crate::sftp_demo::local_utc_offset();
    let in_day = secs.rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02}",
        in_day / 3600,
        in_day % 3600 / 60,
        in_day % 60
    )
}

/// 表格里的百分比：没有就是 `—`。
pub fn percent_label(value: Option<f64>) -> String {
    value.map_or_else(|| "—".to_owned(), |v| format!("{v:.1}%"))
}

/// 常驻内存（输入是 KiB）：`1,984 MB`，小于 10 MB 保留一位小数。
pub fn rss_label(kb: Option<u64>) -> String {
    let Some(kb) = kb else {
        return "—".to_owned();
    };
    let mb = kb as f64 / 1024.0;
    if mb < 10.0 {
        format!("{mb:.1} MB")
    } else {
        let whole = mb.round() as u64;
        let digits = whole.to_string();
        let mut grouped = String::new();
        for (i, ch) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i).is_multiple_of(3) {
                grouped.push(',');
            }
            grouped.push(ch);
        }
        format!("{grouped} MB")
    }
}

/// 表格上方的淡色提示（这台服务器给不出 CPU / 内存时）。
pub fn report_hint(report: &ProcessReport) -> Option<&'static str> {
    if !report.cpu_available() && !report.mem_available() {
        Some("这台服务器的 ps 比较简单，没有 CPU / 内存占用，只能看到进程列表。")
    } else if !report.cpu_available() {
        Some("这台服务器的 ps 不提供 CPU 占用，CPU 列显示为「—」。")
    } else {
        None
    }
}

/// 确认框里的点击处理。
pub(super) type ConfirmHandler = Rc<dyn Fn(&mut Window, &mut ::gpui::App)>;
/// 确认框左边多出来的一个按钮（标签 + 处理）。
pub(super) type AltButton = (&'static str, ConfirmHandler);

/// 确认框（结束进程、断开连接共用）：盖在对话框上、遮住后面的表格；"取消"和确认按钮在右下，Enter 不确认
/// （`confirm_modal` 对这两个对话框都是空操作），执行中两个按钮都置灰。`danger` 决定盾牌图标的颜色。
pub(super) fn confirm_overlay(
    tokens: &Tokens,
    text: AskText,
    danger: bool,
    busy: bool,
    on_cancel: impl Fn(&mut Window, &mut ::gpui::App) + 'static,
    on_ok: impl Fn(&mut Window, &mut ::gpui::App) + 'static,
    alt: Option<AltButton>,
) -> AnyElement {
    let card = v_flex()
        .id("proc-confirm")
        .w(px(560.0))
        .max_w_full()
        .p_5()
        .gap_3()
        .rounded_lg()
        .bg(tokens.background)
        .border_1()
        .border_color(tokens.border)
        .shadow_lg()
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(Icon::new(Ico::ShieldAlert).size_4().text_color(if danger {
                    tokens.danger
                } else {
                    tokens.caution
                }))
                .child(div().typo(Role::GroupTitle, tokens).child(text.title)),
        )
        .child(
            v_flex()
                .gap_1()
                .child(
                    div()
                        .id("proc-confirm-who")
                        .typo(Role::Body, tokens)
                        .font_weight(FontWeight::MEDIUM)
                        .child(SharedString::from(text.who)),
                )
                .child(
                    div()
                        .id("proc-confirm-command")
                        .max_h(px(76.0))
                        .overflow_hidden()
                        .p_2()
                        .rounded_md()
                        .bg(tokens.subtle)
                        .border_1()
                        .border_color(tokens.border)
                        .typo(Role::Caption, tokens)
                        .text_color(tokens.text)
                        .child(SharedString::from(text.command)),
                ),
        )
        .child(
            div()
                .id("proc-confirm-effect")
                .typo(Role::Body, tokens)
                .child(SharedString::from(text.effect)),
        )
        .children(
            text.warnings
                .into_iter()
                .enumerate()
                .map(|(i, (warn, line))| {
                    div()
                        .id(("proc-confirm-warn", i))
                        .typo(Role::Body, tokens)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(match warn {
                            Warn::Danger => tokens.danger,
                            Warn::Caution => tokens.caution,
                            Warn::Note => tokens.text_muted,
                        })
                        .child(SharedString::from(line))
                }),
        )
        .child(
            h_flex()
                .gap_2()
                .justify_end()
                .child(
                    text_button(
                        "proc-confirm-cancel",
                        "取消",
                        ButtonLook::Outline,
                        move |_, window, cx| on_cancel(window, cx),
                    )
                    .disabled(busy),
                )
                .children(alt.map(|(label, on_alt)| {
                    text_button(
                        "proc-confirm-alt",
                        label,
                        ButtonLook::Outline,
                        move |_, window, cx| on_alt(window, cx),
                    )
                    .disabled(busy)
                }))
                .child(
                    text_button(
                        "proc-confirm-ok",
                        if busy {
                            "正在结束…"
                        } else {
                            text.confirm_label
                        },
                        ButtonLook::Danger,
                        move |_, window, cx| on_ok(window, cx),
                    )
                    .disabled(busy),
                ),
        );
    div()
        .id("proc-confirm-scrim")
        .occlude()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_lg()
        .bg(hsla(0.0, 0.0, 0.0, 0.40))
        .child(card)
        .into_any_element()
}

/// 结束 / 断开之后表格上方的一行结果（颜色按级别）。
pub(super) fn notice_line(notice: &KillNotice, tokens: &Tokens) -> Stateful<Div> {
    let color = match notice.level {
        NoticeLevel::Info => tokens.text_muted,
        NoticeLevel::Success => tokens.dot_connected,
        NoticeLevel::Warn => tokens.caution,
        NoticeLevel::Error => tokens.danger,
    };
    div()
        .id("proc-notice")
        .typo(Role::Caption, tokens)
        .font_weight(FontWeight::MEDIUM)
        .text_color(color)
        .child(SharedString::from(notice.text.clone()))
}

impl MainWindow {
    /// 概览面板「进程管理」：开对话框并立刻采一次。
    pub(crate) fn open_processes_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_processes_dialog_at(None, window, cx);
    }

    /// 开进程管理；`focus` 不空时，采到进程后选中这个 PID 并滚到它那一行（"所有网络连接"右键菜单用）。
    pub(crate) fn open_processes_dialog_at(
        &mut self,
        focus: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host_id) = self.status_host else {
            return;
        };
        let host_name = self
            .store
            .host(host_id)
            .map(|host| host.name.clone())
            .unwrap_or_default();
        self.proc_generation += 1;
        let generation = self.proc_generation;
        let query = self.proc_query.clone();
        query.update(cx, |input, cx| input.set_value("", window, cx));
        window.focus(&query.read(cx).focus_handle(cx), cx);
        let mode = self.docks.overview_procs;
        let mut dialog = ProcessesDialog::new(host_id, host_name, generation, mode);
        dialog.pending_focus = focus;
        self.modal = Some(Modal::Processes(Box::new(dialog)));
        self.refresh_processes(window, cx);
        cx.notify();
    }

    /// 采一次（点开、点「刷新」、自动刷新到点、结束之后的确认）。上一次还没回来就不再发。
    pub(crate) fn refresh_processes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::Processes(dialog)) = &mut self.modal else {
            return;
        };
        if dialog.loading {
            return;
        }
        dialog.loading = true;
        dialog.want_refresh = false;
        if let Some(verify) = &mut dialog.verify
            && Instant::now() >= verify.due
        {
            verify.refreshing = true;
        }
        let generation = dialog.generation;
        cx.notify();

        // 调试钩子：喂一份假输出（走同一个解析器），没有真实服务器也能截图。
        if let Some(result) = self.dev_fake.processes_result() {
            self.processes_loaded(generation, result, cx);
            return;
        }

        let Some((connection, _, _)) = self.workspace.read(cx).active_shell_position() else {
            self.processes_loaded(
                generation,
                Err(ProcessesError::Exec(
                    "当前标签没有可用的 SSH 连接（还没连上或已断开）".to_owned(),
                )),
                cx,
            );
            return;
        };
        let rx =
            on_ssh_runtime(async move { zssh_core::ssh::collect_processes(&connection).await });
        match rx {
            Ok(rx) => self.await_result(rx, window, cx, move |this, result, _window, cx| {
                this.processes_loaded(generation, result, cx);
            }),
            Err(reason) => {
                self.processes_loaded(generation, Err(ProcessesError::Exec(reason)), cx);
            }
        }
    }

    fn processes_loaded(
        &mut self,
        generation: u64,
        result: Result<ProcessReport, ProcessesError>,
        cx: &mut Context<Self>,
    ) {
        let query = self.proc_query.read(cx).value().to_string();
        let Some(Modal::Processes(dialog)) = &mut self.modal else {
            return;
        };
        if dialog.generation != generation {
            return;
        }
        dialog.loading = false;
        dialog.last_done = Some(Instant::now());
        let verifying = dialog.verify.as_ref().is_some_and(|v| v.refreshing);
        match result {
            Ok(report) => {
                if verifying && let Some(verify) = dialog.verify.take() {
                    let verdict = kill_verdict(&report, &verify.target);
                    dialog.notice = Some(verdict_notice(
                        verdict,
                        &verify.target,
                        verify.signal,
                        report.row(verify.target.pid),
                    ));
                }
                // 选中的进程已经不在了：取消选中。
                if dialog.selected.is_some_and(|pid| report.row(pid).is_none()) {
                    dialog.selected = None;
                }
                dialog.report = Some(report);
                dialog.error = None;
                dialog.fetched = Some(clock_now());
                dialog.refilter(&query);
                dialog.apply_pending_focus();
            }
            Err(err) => {
                if verifying && let Some(verify) = dialog.verify.take() {
                    dialog.notice = Some(KillNotice {
                        level: NoticeLevel::Warn,
                        text: format!(
                            "已向 PID {} 发出结束请求，但刷新列表失败：{err}",
                            verify.target.pid
                        ),
                    });
                }
                dialog.error = Some(err.to_string());
            }
        }
        cx.notify();
    }

    /// 每个节拍：自动刷新 / 列表过时 / 结束后的确认到点就再采一次。
    pub(super) fn processes_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let due = matches!(&self.modal, Some(Modal::Processes(dialog)) if dialog.refresh_due(Instant::now()));
        if due {
            self.refresh_processes(window, cx);
        }
    }

    /// 搜索框文字变了 / 换了分段或排序：重算要显示的行。
    pub(super) fn processes_refilter(&mut self, cx: &mut Context<Self>) {
        let query = self.proc_query.read(cx).value().to_string();
        if let Some(Modal::Processes(dialog)) = &mut self.modal {
            dialog.refilter(&query);
            cx.notify();
        }
    }

    pub(crate) fn processes_set_mode(&mut self, mode: ProcMode, cx: &mut Context<Self>) {
        if let Some(Modal::Processes(dialog)) = &mut self.modal {
            dialog.mode = mode;
            // 换分段 = 回到这一档的默认排序（点表头选的列不跟着走）。
            dialog.sort = ProcSort::default();
        }
        self.processes_refilter(cx);
    }

    pub(crate) fn processes_sort_by(&mut self, key: ProcSortKey, cx: &mut Context<Self>) {
        if let Some(Modal::Processes(dialog)) = &mut self.modal {
            dialog.sort = dialog.sort.toggled(key);
        }
        self.processes_refilter(cx);
    }

    fn processes_set_auto(&mut self, on: bool, cx: &mut Context<Self>) {
        if let Some(Modal::Processes(dialog)) = &mut self.modal {
            dialog.auto = on;
            // 刚打开开关：从现在起算 3 秒，不立刻再采一次。
            dialog.last_done = Some(Instant::now());
        }
        cx.notify();
    }

    pub(crate) fn processes_select(&mut self, pid: u32, cx: &mut Context<Self>) {
        if let Some(Modal::Processes(dialog)) = &mut self.modal
            && !dialog.busy()
        {
            dialog.selected = Some(pid);
            cx.notify();
        }
    }

    /// 弹确认框（还不会结束任何东西）。
    pub(crate) fn processes_ask_kill(
        &mut self,
        pid: u32,
        signal: KillSignal,
        cx: &mut Context<Self>,
    ) {
        let Some(Modal::Processes(dialog)) = &mut self.modal else {
            return;
        };
        if dialog.busy() {
            return;
        }
        let Some(report) = &dialog.report else {
            return;
        };
        let Some(row) = report.row(pid) else {
            return;
        };
        // 受保护的进程没有解锁途径：连确认框都不弹（界面上按钮 / 菜单已经置灰，这里是兜底）。
        if let Some(protection) = report.protection(pid) {
            dialog.notice = Some(failure_notice(
                &KillError::Protected(protection),
                row,
                report.user.as_deref(),
            ));
            dialog.selected = Some(pid);
            cx.notify();
            return;
        }
        let risk = report.risk_of(pid);
        dialog.selected = Some(pid);
        dialog.ask = Some(KillAsk {
            target: row.clone(),
            signal,
            risk,
            busy: false,
        });
        cx.notify();
    }

    pub(crate) fn processes_cancel_ask(&mut self, cx: &mut Context<Self>) {
        if let Some(Modal::Processes(dialog)) = &mut self.modal
            && !dialog.busy()
        {
            dialog.ask = None;
            cx.notify();
        }
    }

    /// 用户在确认框里点了「结束进程 / 强制结束」：执行。
    pub(crate) fn processes_confirm_kill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let status_host = self.status_host;
        let Some(Modal::Processes(dialog)) = &mut self.modal else {
            return;
        };
        let Some(ask) = &mut dialog.ask else {
            return;
        };
        if ask.busy {
            return;
        }
        let (target, signal) = (ask.target.clone(), ask.signal);
        let generation = dialog.generation;
        // 连接已经不是打开对话框时那台主机（切了标签、重连到别处）：不执行，免得杀到别的机器上。
        if status_host != Some(dialog.host_id) {
            dialog.ask = None;
            dialog.notice = Some(KillNotice {
                level: NoticeLevel::Error,
                text: "当前连接已经不是打开这个窗口时的服务器，已取消，请重新打开进程管理。"
                    .to_owned(),
            });
            cx.notify();
            return;
        }
        ask.busy = true;
        cx.notify();

        // 调试钩子：假的结束结果（没有真实服务器也能截图）。
        if let Some(result) = self.dev_fake.kill_result(target.pid) {
            self.processes_kill_done(generation, target, signal, result, cx);
            return;
        }

        let Some((connection, _, _)) = self.workspace.read(cx).active_shell_position() else {
            self.processes_kill_done(
                generation,
                target,
                signal,
                Err(KillError::Failed(
                    "当前标签没有可用的 SSH 连接（还没连上或已断开）".to_owned(),
                )),
                cx,
            );
            return;
        };
        let for_task = target.clone();
        let rx =
            on_ssh_runtime(async move { terminate_process(&connection, &for_task, signal).await });
        match rx {
            Ok(rx) => self.await_result(rx, window, cx, move |this, result, _window, cx| {
                this.processes_kill_done(generation, target, signal, result, cx);
            }),
            Err(reason) => {
                self.processes_kill_done(
                    generation,
                    target,
                    signal,
                    Err(KillError::Failed(reason)),
                    cx,
                );
            }
        }
    }

    fn processes_kill_done(
        &mut self,
        generation: u64,
        target: ProcRow,
        signal: KillSignal,
        result: Result<(), KillError>,
        cx: &mut Context<Self>,
    ) {
        let Some(Modal::Processes(dialog)) = &mut self.modal else {
            return;
        };
        if dialog.generation != generation {
            return;
        }
        dialog.ask = None;
        let login_user = dialog.report.as_ref().and_then(|r| r.user.clone());
        match result {
            Ok(()) => {
                dialog.notice = Some(sent_notice(&target, signal));
                dialog.verify = Some(Verify {
                    target,
                    signal,
                    due: Instant::now() + VERIFY_DELAY,
                    refreshing: false,
                });
            }
            Err(err) => {
                dialog.notice = Some(failure_notice(&err, &target, login_user.as_deref()));
                // PID 变了 / 进程已经没了：列表过时了，刷新一次。
                if matches!(err, KillError::Changed | KillError::NoSuchProcess) {
                    dialog.want_refresh = true;
                }
            }
        }
        cx.notify();
    }

    // ---- 画 ----

    pub(super) fn processes_body(
        &self,
        dialog: &ProcessesDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        // 对话框高度跟窗口走：卡片外有 24 的外边距、20 的内边距。
        let height = (self.window_height - 48.0 - 40.0).clamp(420.0, 700.0);

        let title = h_flex()
            .gap_2()
            .items_center()
            .child(
                Icon::new(Ico::ListChecks)
                    .size_4()
                    .text_color(tokens.accent),
            )
            .child(div().typo(Role::GroupTitle, tokens).child("进程管理"))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .typo(Role::Caption, tokens)
                    .child(SharedString::from(format!("· {}", dialog.host_name))),
            )
            .child({
                let close = entity.clone();
                div()
                    .id("proc-close")
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(24.0))
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(tokens.text_muted)
                    .hover({
                        let hover = tokens.hover;
                        move |style| style.bg(hover)
                    })
                    .child(Icon::new(IconName::Close).size_4())
                    .on_click(move |_, window, cx| {
                        close.update(cx, |this, cx| this.close_modal(window, cx));
                    })
            });

        let mode_now = dialog.mode;
        let modes = div().w(px(150.0)).flex_none().child(segmented(
            "proc-mode",
            ProcMode::ALL
                .iter()
                .map(|m| (m.label().to_owned(), *m == mode_now, None))
                .collect(),
            {
                let entity = entity.clone();
                move |index, _window, cx| {
                    if let Some(mode) = ProcMode::ALL.get(index).copied() {
                        entity.update(cx, |this, cx| this.processes_set_mode(mode, cx));
                    }
                }
            },
        ));
        let toolbar = h_flex()
            .gap_3()
            .items_center()
            .child(modes)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&self.proc_query).prefix(Icon::new(IconName::Search))),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .flex_none()
                    .child(Switch::new("proc-auto").checked(dialog.auto).on_click({
                        let entity = entity.clone();
                        let on = !dialog.auto;
                        move |_, _window, cx| {
                            entity.update(cx, |this, cx| this.processes_set_auto(on, cx));
                        }
                    }))
                    .child(div().typo(Role::Caption, tokens).child("每 3 秒自动刷新")),
            )
            .child({
                let entity = entity.clone();
                text_button(
                    "proc-refresh",
                    if dialog.loading {
                        "刷新中…"
                    } else {
                        "刷新"
                    },
                    ButtonLook::Outline,
                    move |_, window, cx| {
                        entity.update(cx, |this, cx| this.refresh_processes(window, cx));
                    },
                )
            });

        // 状态行：条数、来源、采集时间。
        let summary = {
            let total = dialog.report.as_ref().map_or(0, |r| r.rows.len());
            let mut text = format!("{} / {} 个进程", dialog.shown.len(), total);
            if let Some(report) = &dialog.report {
                text.push_str(&format!(" · 来源 {}", report.source.label()));
            }
            if let Some(at) = &dialog.fetched {
                text.push_str(&format!(" · 采集于 {at}"));
            }
            text
        };
        let hint = dialog
            .report
            .as_ref()
            .and_then(report_hint)
            .unwrap_or("CPU% 是进程启动以来的平均值（ps 的口径），不是此刻的瞬时占用。");
        let info = v_flex()
            .gap_0p5()
            .child(
                h_flex()
                    .gap_3()
                    .justify_between()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .typo(Role::Caption, tokens)
                            .child(SharedString::from(hint)),
                    )
                    .child(
                        div()
                            .id("proc-summary")
                            .flex_none()
                            .typo(Role::Caption, tokens)
                            .child(SharedString::from(summary)),
                    ),
            )
            .children(dialog.report.as_ref().filter(|r| r.truncated).map(|_| {
                div()
                    .typo(Role::Caption, tokens)
                    .text_color(tokens.caution)
                    .child(format!(
                        "进程太多，只显示前 {} 个。",
                        zssh_core::ssh::PROCESSES_MAX_ROWS
                    ))
            }))
            .children(
                dialog
                    .error
                    .as_ref()
                    .filter(|_| dialog.report.is_some())
                    .map(|error| {
                        div()
                            .typo(Role::Caption, tokens)
                            .text_color(tokens.danger)
                            .child(SharedString::from(format!("刷新失败：{error}")))
                    }),
            )
            .children(
                dialog
                    .notice
                    .as_ref()
                    .map(|notice| notice_line(notice, tokens)),
            );

        // 表头：点列名排序。
        let header = {
            let (sort_key, sort_desc) = dialog.sort.effective(dialog.mode);
            let cell = |id: &'static str,
                        label: &'static str,
                        key: ProcSortKey,
                        width: Option<f32>,
                        right: bool,
                        tip: &'static str| {
                let entity = entity.clone();
                let arrow = (sort_key == key).then_some(if sort_desc {
                    IconName::ArrowDown
                } else {
                    IconName::ArrowUp
                });
                let hover = tokens.hover;
                h_flex()
                    .id(id)
                    .gap_1()
                    .items_center()
                    .h_full()
                    .px_2()
                    .cursor_pointer()
                    .when(right, |d| d.justify_end())
                    .when_some(width, |d, w| d.w(px(w)).flex_none())
                    .when(width.is_none(), |d| d.flex_1().min_w_0())
                    .hover(move |style| style.bg(hover))
                    .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
                    .children(arrow.map(|icon| Icon::new(icon).size_3()))
                    .child(div().child(label))
                    .on_click(move |_, _window, cx| {
                        entity.update(cx, |this, cx| this.processes_sort_by(key, cx));
                    })
            };
            const TIP: &str = "点一下排序，再点反向，第三次恢复默认";
            h_flex()
                .flex_none()
                .h(px(28.0))
                .items_center()
                .border_b_1()
                .border_color(tokens.border)
                .typo(Role::Caption, tokens)
                .font_weight(FontWeight::MEDIUM)
                .text_color(tokens.text_muted)
                .child(cell(
                    "proc-h-pid",
                    "PID",
                    ProcSortKey::Pid,
                    Some(COL_PID),
                    true,
                    TIP,
                ))
                .child(cell(
                    "proc-h-user",
                    "用户",
                    ProcSortKey::User,
                    Some(COL_USER),
                    false,
                    TIP,
                ))
                .child(cell(
                    "proc-h-cpu",
                    "CPU%",
                    ProcSortKey::Cpu,
                    Some(COL_CPU),
                    true,
                    "进程启动以来的平均 CPU 占用（ps 的口径）。点一下排序，再点反向，第三次恢复默认",
                ))
                .child(cell(
                    "proc-h-mem",
                    "内存%",
                    ProcSortKey::Mem,
                    Some(COL_MEM),
                    true,
                    TIP,
                ))
                .child(cell(
                    "proc-h-rss",
                    "RSS",
                    ProcSortKey::Rss,
                    Some(COL_RSS),
                    true,
                    "常驻内存（MB）。点一下排序，再点反向，第三次恢复默认",
                ))
                .child(cell(
                    "proc-h-state",
                    "状态",
                    ProcSortKey::State,
                    Some(COL_STATE),
                    false,
                    TIP,
                ))
                .child(cell(
                    "proc-h-time",
                    "运行时长",
                    ProcSortKey::Elapsed,
                    Some(COL_TIME),
                    true,
                    TIP,
                ))
                .child(cell(
                    "proc-h-cmd",
                    "命令",
                    ProcSortKey::Command,
                    None,
                    false,
                    TIP,
                ))
        };

        // 表体：没数据 / 失败 / 没有符合条件的 / 虚拟列表。
        let placeholder = |text: String, color: ::gpui::Hsla| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p_6()
                .typo(Role::Body, tokens)
                .text_color(color)
                .child(SharedString::from(text))
                .into_any_element()
        };
        let body: AnyElement = match (&dialog.report, &dialog.error) {
            (None, Some(error)) => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_3()
                .p_6()
                .child(
                    div()
                        .typo(Role::Body, tokens)
                        .text_color(tokens.danger)
                        .child(SharedString::from(error.clone())),
                )
                .child({
                    let entity = entity.clone();
                    text_button(
                        "proc-retry",
                        "重试",
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            entity.update(cx, |this, cx| this.refresh_processes(window, cx));
                        },
                    )
                })
                .into_any_element(),
            (None, None) => placeholder("正在读取进程列表…".to_owned(), tokens.text_muted),
            (Some(_), _) if dialog.shown.is_empty() => {
                placeholder("没有符合条件的进程".to_owned(), tokens.text_muted)
            }
            (Some(_), _) => {
                let row_tokens = *tokens;
                uniform_list(
                    "proc-rows",
                    dialog.shown.len(),
                    cx.processor(move |this, range: Range<usize>, _window, cx| {
                        this.process_rows(range, &row_tokens, cx)
                    }),
                )
                .track_scroll(&dialog.scroll)
                .size_full()
                .into_any_element()
            }
        };
        let table = v_flex()
            .flex_1()
            .min_h_0()
            .rounded_md()
            .border_1()
            .border_color(tokens.border)
            .overflow_hidden()
            .child(header)
            .child(div().relative().flex_1().min_h_0().child(body));

        // 底部：选中的进程 + 两个结束按钮（没选中时置灰）。
        let selected = dialog.selected_row();
        let pid = selected.map(|r| r.pid);
        let blocked = dialog.selected_protection();
        let tip = |normal: &'static str| -> SharedString {
            match blocked {
                Some(p) => format!("受保护的系统进程不能结束：{}", p.reason()).into(),
                None => normal.into(),
            }
        };
        let actions = h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .id("proc-selected")
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .typo(Role::Caption, tokens)
                    .child(SharedString::from(match selected {
                        Some(row) => format!(
                            "已选中：PID {} · {} · {}{}",
                            row.pid,
                            row.user,
                            short(&row.command),
                            blocked
                                .map(|p| format!("（受保护：{}，不能结束）", p.reason()))
                                .unwrap_or_default()
                        ),
                        None => "点一行选中进程（也可以右键）".to_owned(),
                    })),
            )
            .child({
                let entity = entity.clone();
                text_button(
                    "proc-kill-term",
                    KillSignal::Term.label(),
                    ButtonLook::Outline,
                    move |_, _window, cx| {
                        if let Some(pid) = pid {
                            entity.update(cx, |this, cx| {
                                this.processes_ask_kill(pid, KillSignal::Term, cx);
                            });
                        }
                    },
                )
                .disabled(pid.is_none() || blocked.is_some())
                .tooltip(tip("向选中的进程发送 SIGTERM，请它退出（会先确认）"))
            })
            .child({
                let entity = entity.clone();
                text_button(
                    "proc-kill-kill",
                    KillSignal::Kill.label(),
                    ButtonLook::Danger,
                    move |_, _window, cx| {
                        if let Some(pid) = pid {
                            entity.update(cx, |this, cx| {
                                this.processes_ask_kill(pid, KillSignal::Kill, cx);
                            });
                        }
                    },
                )
                .disabled(pid.is_none() || blocked.is_some())
                .tooltip(tip("向选中的进程发送 SIGKILL，立即终止（会先确认）"))
            });

        let confirm = dialog
            .ask
            .as_ref()
            .map(|ask| self.kill_confirm(dialog, ask, tokens, cx));

        v_flex()
            .relative()
            .gap_3()
            .h(px(height))
            .child(title)
            .child(toolbar)
            .child(info)
            .child(table)
            .child(actions)
            .children(confirm)
            .into_any_element()
    }

    /// 结束进程的确认框：盖在对话框上，遮住后面的表格。版式在 [`confirm_overlay`]（"所有网络连接"的确认框共用）。
    fn kill_confirm(
        &self,
        dialog: &ProcessesDialog,
        ask: &KillAsk,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let report = dialog.report.as_ref();
        let text = ask_text(
            &ask.target,
            ask.signal,
            ask.risk,
            report.and_then(|r| r.user.as_deref()),
            report.and_then(|r| r.uid),
        );
        let danger_ask = ask.risk != ProcRisk::Normal || ask.signal == KillSignal::Kill;
        let cancel = entity.clone();
        confirm_overlay(
            tokens,
            text,
            danger_ask,
            ask.busy,
            move |_window, cx| {
                cancel.update(cx, |this, cx| this.processes_cancel_ask(cx));
            },
            move |window, cx| {
                entity.update(cx, |this, cx| this.processes_confirm_kill(window, cx));
            },
            None,
        )
    }

    /// 虚拟列表要画的这一段行。
    fn process_rows(
        &self,
        range: Range<usize>,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(Modal::Processes(dialog)) = &self.modal else {
            return Vec::new();
        };
        let Some(report) = &dialog.report else {
            return Vec::new();
        };
        let entity = cx.entity();
        let (hover, selected_bg) = (tokens.hover, tokens.accent_soft);
        range
            .filter_map(|at| {
                let row: &ProcRow = report.rows.get(*dialog.shown.get(at)?)?;
                let pid = row.pid;
                let is_selected = dialog.selected == Some(pid);
                let protection = report.protection(pid);
                let cell = |id: &'static str, text: String, width: Option<f32>, right: bool| {
                    let tip = text.clone();
                    div()
                        .id((id, at))
                        .px_2()
                        .truncate()
                        .when(right, |d| d.text_right())
                        .when_some(width, |d, w| d.w(px(w)).flex_none())
                        .when(width.is_none(), |d| d.flex_1().min_w_0())
                        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                        .child(SharedString::from(text))
                };
                let muted = tokens.text_muted;
                let select = entity.clone();
                let right_select = entity.clone();
                let menu_entity = entity.clone();
                let state_color = if row.is_zombie() {
                    tokens.caution
                } else {
                    tokens.text
                };
                let command = if row.command.is_empty() {
                    "—".to_owned()
                } else {
                    row.command.clone()
                };
                Some(
                    h_flex()
                        .id(("proc-row", at))
                        .w_full()
                        .h(px(ROW_HEIGHT))
                        .items_center()
                        .typo(Role::Caption, tokens)
                        .text_color(tokens.text)
                        .cursor_pointer()
                        .when(is_selected, |d| d.bg(selected_bg))
                        .when(!is_selected, |d| d.hover(move |style| style.bg(hover)))
                        .child(
                            cell("proc-c-pid", pid.to_string(), Some(COL_PID), true)
                                .text_color(muted),
                        )
                        .child(cell("proc-c-user", row.user.clone(), Some(COL_USER), false))
                        .child(cell(
                            "proc-c-cpu",
                            percent_label(row.cpu),
                            Some(COL_CPU),
                            true,
                        ))
                        .child(cell(
                            "proc-c-mem",
                            percent_label(row.mem),
                            Some(COL_MEM),
                            true,
                        ))
                        .child(cell(
                            "proc-c-rss",
                            rss_label(row.rss_kb),
                            Some(COL_RSS),
                            true,
                        ))
                        .child(
                            cell("proc-c-state", row.state.clone(), Some(COL_STATE), false)
                                .text_color(state_color),
                        )
                        .child(
                            cell(
                                "proc-c-time",
                                row.elapsed_secs
                                    .map_or_else(|| "—".to_owned(), elapsed_label),
                                Some(COL_TIME),
                                true,
                            )
                            .text_color(muted),
                        )
                        .child({
                            // 命令列：受保护的进程前面带小锁，悬停说原因。
                            let tip = match protection {
                                Some(p) => format!("{}：{command}", p.reason()),
                                None => command.clone(),
                            };
                            h_flex()
                                .id(("proc-c-cmd", at))
                                .px_2()
                                .gap_1()
                                .items_center()
                                .flex_1()
                                .min_w_0()
                                .tooltip(move |window, cx| {
                                    Tooltip::new(tip.clone()).build(window, cx)
                                })
                                .children(protection.map(|p| {
                                    div()
                                        .id(("proc-lock", at))
                                        .flex_none()
                                        .text_color(tokens.caution)
                                        .tooltip(move |window, cx| {
                                            Tooltip::new(p.reason()).build(window, cx)
                                        })
                                        .child(Icon::new(Ico::Lock).size_3())
                                }))
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .child(SharedString::from(command)),
                                )
                        })
                        .on_click(move |_, _window, cx| {
                            select.update(cx, |this, cx| this.processes_select(pid, cx));
                        })
                        // 右键先选中这一行（菜单项按这一行的 PID 动作，和选中状态无关）。
                        .on_mouse_down(MouseButton::Right, move |_, _window, cx| {
                            right_select.update(cx, |this, cx| this.processes_select(pid, cx));
                        })
                        .context_menu(move |menu, _window, cx| {
                            let tokens = Tokens::current(cx);
                            let (term, force) = (menu_entity.clone(), menu_entity.clone());
                            menu.item(
                                crate::menu_kit::row(&tokens, KillSignal::Term.label(), Ico::X)
                                    .disabled(protection.is_some())
                                    .right(protection.map(|p| p.reason()))
                                    .on_click(move |_, _window, cx| {
                                        term.update(cx, |this, cx| {
                                            this.processes_ask_kill(pid, KillSignal::Term, cx);
                                        });
                                    }),
                            )
                            .item(
                                crate::menu_kit::row(&tokens, KillSignal::Kill.label(), Ico::Zap)
                                    .danger()
                                    .disabled(protection.is_some())
                                    .right(protection.map(|p| p.reason()))
                                    .on_click(move |_, _window, cx| {
                                        force.update(cx, |this, cx| {
                                            this.processes_ask_kill(pid, KillSignal::Kill, cx);
                                        });
                                    }),
                            )
                        })
                        .into_any_element(),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{
        AskText, KillNotice, NoticeLevel, ProcessesDialog, Warn, ask_text, clock_now,
        failure_notice, percent_label, report_hint, rss_label, sent_notice, verdict_notice,
    };
    use std::time::{Duration, Instant};
    use zssh_core::ssh::{
        KillError, KillSignal, KillVerdict, ProcMode, ProcRisk, ProcRow, ProcSort, ProcSortKey,
        parse_processes,
    };

    const SAMPLE: &str = "\
@@ZSSH uid 1000
@@ZSSH user app
@@ZSSH self 4242
@@ZSSH memtotal 16000000
@@ZSSH source procps
PID PPID USER %CPU %MEM RSS STAT ELAPSED COMMAND
812 1 app 55.5 12.4 1984000 Ssl 93784 /usr/bin/app-worker --queue=high
1290 1 www-data 7.0 6.1 976000 S 5000 /usr/sbin/web-server -g daemon on;
1 0 root 0.0 0.3 48000 Ss 172800 /sbin/init
3100 3090 app 0.0 0.1 9000 S 3600 sshd: app@notty
3090 800 root 0.0 0.1 11000 Ss 3600 sshd: app [priv]
800 1 root 0.0 0.1 15000 Ss 172700 sshd: /usr/sbin/sshd -D [listener]
4242 3100 app 0.4 0.0 3000 Ss 0 sh -c exec stuff
5001 812 app 0.0 0.0 0 Z 1200 [worker] <defunct>
";

    fn loaded() -> ProcessesDialog {
        let mut dialog = ProcessesDialog::new(1, "web-1".into(), 1, ProcMode::Cpu);
        dialog.report = Some(parse_processes(SAMPLE).expect("解析"));
        dialog.refilter("");
        dialog
    }

    fn row(dialog: &ProcessesDialog, pid: u32) -> ProcRow {
        dialog
            .report
            .as_ref()
            .and_then(|r| r.row(pid))
            .cloned()
            .expect("有这个进程")
    }

    #[test]
    fn a_fresh_dialog_is_empty_idle_and_not_auto_refreshing() {
        let dialog = ProcessesDialog::new(7, "web-1".into(), 3, ProcMode::Mem);
        assert!(dialog.report.is_none() && dialog.shown.is_empty());
        assert!(!dialog.loading && !dialog.auto && !dialog.busy());
        assert!(dialog.selected.is_none() && dialog.ask.is_none() && dialog.notice.is_none());
        assert_eq!((dialog.generation, dialog.mode), (3, ProcMode::Mem));
        assert!(!dialog.refresh_due(Instant::now()), "默认不自动刷新");
    }

    #[test]
    fn the_segment_chooses_the_default_order_and_header_clicks_override_it() {
        let mut dialog = loaded();
        let pids = |d: &ProcessesDialog| {
            d.shown
                .iter()
                .map(|i| d.report.as_ref().map(|r| r.rows[*i].pid).unwrap_or(0))
                .collect::<Vec<_>>()
        };
        // CPU 档：CPU% 从高到低。
        assert_eq!(pids(&dialog)[..2], [812, 1290]);
        // 内存档：内存% 从高到低（812 的 12.4 仍在前，1290 的 6.1 第二）。
        dialog.mode = ProcMode::Mem;
        dialog.refilter("");
        assert_eq!(pids(&dialog)[..2], [812, 1290]);
        // 点表头按 PID 升序。
        dialog.sort = ProcSort::default().toggled(ProcSortKey::Pid);
        dialog.refilter("");
        assert_eq!(pids(&dialog)[..3], [1, 800, 812]);
        // 搜索：命令 / 用户 / PID。
        dialog.refilter("www");
        assert_eq!(pids(&dialog), [1290]);
        dialog.refilter("sshd");
        assert_eq!(pids(&dialog).len(), 3);
        dialog.refilter("812");
        assert_eq!(pids(&dialog), [812]);
    }

    #[test]
    fn auto_refresh_waits_three_seconds_never_overlaps_and_stale_lists_refresh_at_once() {
        let mut dialog = loaded();
        dialog.auto = true;
        let now = Instant::now();
        assert!(dialog.refresh_due(now), "从没完成过：马上可以采");
        dialog.last_done = Some(now);
        assert!(!dialog.refresh_due(now + Duration::from_secs(2)));
        assert!(dialog.refresh_due(now + Duration::from_secs(3)));
        dialog.loading = true;
        assert!(
            !dialog.refresh_due(now + Duration::from_secs(60)),
            "上一次没回来不再发"
        );
        dialog.loading = false;
        dialog.auto = false;
        assert!(!dialog.refresh_due(now + Duration::from_secs(60)));
        dialog.want_refresh = true;
        assert!(dialog.refresh_due(now), "列表过时：不等自动刷新");
    }

    #[test]
    fn the_check_after_a_kill_is_due_after_one_second_and_only_once() {
        let mut dialog = loaded();
        let now = Instant::now();
        dialog.verify = Some(super::Verify {
            target: row(&dialog, 812),
            signal: KillSignal::Term,
            due: now + super::VERIFY_DELAY,
            refreshing: false,
        });
        assert_eq!(super::VERIFY_DELAY, Duration::from_secs(1));
        assert!(!dialog.refresh_due(now + Duration::from_millis(900)));
        assert!(dialog.refresh_due(now + Duration::from_secs(1)));
        if let Some(verify) = &mut dialog.verify {
            verify.refreshing = true;
        }
        dialog.loading = false;
        assert!(
            !dialog.refresh_due(now + Duration::from_secs(5)),
            "确认用的刷新已经发出"
        );
    }

    #[test]
    fn the_confirmation_names_pid_user_command_and_what_will_happen() {
        let dialog = loaded();
        let report = dialog.report.as_ref().expect("报告");
        let app = row(&dialog, 812);
        let text = ask_text(
            &app,
            KillSignal::Term,
            report.risk_of(812),
            Some("app"),
            Some(1000),
        );
        assert_eq!(text.title, "结束进程");
        assert_eq!(text.who, "PID 812 · 用户 app");
        assert_eq!(text.command, "/usr/bin/app-worker --queue=high");
        assert!(text.effect.contains("这会结束服务器上的这个进程"));
        assert!(text.effect.contains("SIGTERM"));
        assert!(
            text.warnings.is_empty(),
            "自己的普通进程没有额外警示：{:?}",
            text.warnings
        );
        assert_eq!(text.confirm_label, "结束进程");

        let forced = ask_text(
            &app,
            KillSignal::Kill,
            ProcRisk::Normal,
            Some("app"),
            Some(1000),
        );
        assert_eq!(forced.title, "强制结束进程");
        assert!(forced.effect.contains("SIGKILL") && forced.effect.contains("来不及保存数据"));
        assert_eq!(forced.confirm_label, "强制结束");
    }

    #[test]
    fn pid_one_and_the_ssh_session_get_red_warnings() {
        let dialog = loaded();
        let report = dialog.report.as_ref().expect("报告");
        let init = ask_text(
            &row(&dialog, 1),
            KillSignal::Kill,
            report.risk_of(1),
            Some("root"),
            Some(0),
        );
        assert_eq!(init.warnings.len(), 1);
        assert_eq!(init.warnings[0].0, Warn::Danger);
        assert!(init.warnings[0].1.contains("PID 1"));
        for pid in [3100, 3090, 800] {
            let text = ask_text(
                &row(&dialog, pid),
                KillSignal::Term,
                report.risk_of(pid),
                Some("root"),
                Some(0),
            );
            let (warn, line) = text.warnings.first().expect("警示");
            assert_eq!(*warn, Warn::Danger, "{pid}");
            assert!(
                line.contains("当前 SSH 连接") && line.contains("断开"),
                "{pid}: {line}"
            );
        }
    }

    #[test]
    fn someone_elses_process_and_zombies_get_cautions_and_root_does_not() {
        let dialog = loaded();
        let www = row(&dialog, 1290);
        let text = ask_text(
            &www,
            KillSignal::Term,
            ProcRisk::Normal,
            Some("app"),
            Some(1000),
        );
        let (warn, line) = text.warnings.first().expect("警示");
        assert_eq!(*warn, Warn::Caution);
        assert!(line.contains("属于 www-data") && line.contains("你现在是 app"));
        assert!(line.contains("不会自动使用 sudo"));
        // root 登录：不提示权限。
        let as_root = ask_text(
            &www,
            KillSignal::Term,
            ProcRisk::Normal,
            Some("root"),
            Some(0),
        );
        assert!(as_root.warnings.is_empty());
        // 僵尸：说明结束它没有效果、该处理父进程。
        let zombie = row(&dialog, 5001);
        let text = ask_text(
            &zombie,
            KillSignal::Kill,
            ProcRisk::Normal,
            Some("app"),
            Some(1000),
        );
        assert!(
            text.warnings
                .iter()
                .any(|(_, l)| l.contains("僵尸进程") && l.contains("PID 812"))
        );
        // 没有命令行：给占位字。
        let mut bare = row(&dialog, 812);
        bare.command.clear();
        let text: AskText = ask_text(
            &bare,
            KillSignal::Term,
            ProcRisk::Normal,
            Some("app"),
            Some(1000),
        );
        assert_eq!(text.command, "（没有命令行）");
    }

    #[test]
    fn the_result_lines_say_what_happened_and_the_permission_one_names_the_owner() {
        let dialog = loaded();
        let www = row(&dialog, 1290);
        let denied = failure_notice(&KillError::NoPermission, &www, Some("app"));
        assert_eq!(denied.level, NoticeLevel::Error);
        assert_eq!(
            denied.text,
            "没有权限结束这个进程（属于用户 www-data，你现在是 app），需要 root 或 sudo。"
        );
        let gone = failure_notice(&KillError::NoSuchProcess, &www, None);
        assert_eq!(
            (gone.level, gone.text.as_str()),
            (NoticeLevel::Warn, "这个进程已经不存在了。")
        );
        let changed = failure_notice(&KillError::Changed, &www, None);
        assert_eq!(changed.level, NoticeLevel::Warn);
        assert!(changed.text.contains("重新选择"));
        let failed = failure_notice(&KillError::Failed("boom".into()), &www, None);
        assert_eq!(
            failed,
            KillNotice {
                level: NoticeLevel::Error,
                text: "结束失败：boom".into()
            }
        );

        let app = row(&dialog, 812);
        assert_eq!(
            sent_notice(&app, KillSignal::Kill).text,
            "已向 PID 812 发送 SIGKILL，正在确认结果…"
        );
        let ok = verdict_notice(KillVerdict::Gone, &app, KillSignal::Term, None);
        assert_eq!(ok.level, NoticeLevel::Success);
        assert!(ok.text.starts_with("已结束：PID 812（/usr/bin/app-worker"));
        let still = verdict_notice(
            KillVerdict::StillRunning,
            &app,
            KillSignal::Term,
            Some(&app),
        );
        assert_eq!(still.level, NoticeLevel::Warn);
        assert!(still.text.contains("仍在运行") && still.text.contains("强制结束"));
        let still_kill = verdict_notice(
            KillVerdict::StillRunning,
            &app,
            KillSignal::Kill,
            Some(&app),
        );
        assert!(still_kill.text.contains("SIGKILL") && still_kill.text.contains("状态 Ssl"));
        let zombie = row(&dialog, 5001);
        let z = verdict_notice(
            KillVerdict::Zombie,
            &zombie,
            KillSignal::Kill,
            Some(&zombie),
        );
        assert!(z.text.contains("僵尸进程") && z.text.contains("PID 812"));
    }

    #[test]
    fn numbers_are_formatted_for_the_table() {
        assert_eq!(percent_label(Some(55.5)), "55.5%");
        assert_eq!(percent_label(Some(0.0)), "0.0%");
        assert_eq!(percent_label(None), "—");
        assert_eq!(rss_label(None), "—");
        assert_eq!(rss_label(Some(512)), "0.5 MB");
        assert_eq!(rss_label(Some(1_984_000)), "1,938 MB");
        assert_eq!(rss_label(Some(48_000)), "47 MB");
        assert_eq!(rss_label(Some(10 * 1024)), "10 MB");
    }

    #[test]
    fn the_report_hint_only_appears_when_cpu_or_memory_is_missing() {
        let full = parse_processes(SAMPLE).expect("解析");
        assert_eq!(report_hint(&full), None);
        let busybox =
            parse_processes("@@ZSSH source bb\nPID USER VSZ STAT COMMAND\n1 root 100 S init\n")
                .expect("解析");
        assert!(report_hint(&busybox).is_some_and(|h| h.contains("没有 CPU / 内存占用")));
        let no_cpu = parse_processes(
            "@@ZSSH memtotal 1000\n@@ZSSH source bb\nPID PPID USER STAT RSS COMMAND\n1 0 root S 100 init\n",
        )
        .expect("解析");
        assert!(report_hint(&no_cpu).is_some_and(|h| h.contains("不提供 CPU")));
    }

    #[test]
    fn the_confirmation_blocks_closing_only_while_the_kill_is_running() {
        let mut dialog = loaded();
        assert!(!dialog.busy());
        dialog.ask = Some(super::KillAsk {
            target: row(&dialog, 812),
            signal: KillSignal::Term,
            risk: ProcRisk::Normal,
            busy: false,
        });
        assert!(!dialog.busy(), "确认框开着但还没点确认：可以取消");
        if let Some(ask) = &mut dialog.ask {
            ask.busy = true;
        }
        assert!(dialog.busy());
    }

    #[test]
    fn protected_rows_disable_the_kill_buttons_and_refuse_a_confirmation() {
        use zssh_core::ssh::Protection;
        let mut dialog = loaded();
        let expect = [
            (1, Some(Protection::Init)),
            (3100, Some(Protection::SshSession)),
            (3090, Some(Protection::SshSession)),
            (800, Some(Protection::SshSession)),
            (812, None),
            (1290, None),
        ];
        for (pid, want) in expect {
            dialog.selected = Some(pid);
            assert_eq!(dialog.selected_protection(), want, "PID {pid}");
        }
        // 没有选中：没有保护也没有按钮可点。
        dialog.selected = None;
        assert_eq!(dialog.selected_protection(), None);
    }

    #[test]
    fn the_clock_is_hh_mm_ss() {
        let now = clock_now();
        assert_eq!(now.len(), 8);
        assert_eq!(&now[2..3], ":");
        assert_eq!(&now[5..6], ":");
    }
}
