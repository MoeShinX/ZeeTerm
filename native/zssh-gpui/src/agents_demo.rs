//! 设置页里的 AI 部分（0.2.14 `SettingsPage.tsx` 的 `AISection` + `AgentBlock`）：默认智能体、
//! 执行模式、写文件备份份数、清空 AI 历史，以及 opencode / codex / Claude Code 各一块——当前检测到的版本
//! 和来源、登录状态、手动指定路径（先检测、通过了才保存）、重新检测；opencode 另有一键下载。
//!
//! 检测结果、登录状态、codex 的本机工具核对都在 `agent_setup` 的进程级状态里，这里和 AI 面板读同一份，
//! 改了路径 / 下载完 / 点「重新检测」就刷新它，面板下一个节拍就看到新结果。业务判定全是 core 的，
//! 这里只拼装和画。
//!
//! 同一个 [`AgentsDemo`] 有两种摆法：嵌在主窗口设置页的 AI 一节里（`embedded`），以及调试版里单独开的
//! "AI 智能体"窗口（`open_agents_window`，内容完全一样，外面套个滚动容器）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use ::gpui::{App, FocusHandle, Task, Window, WindowBounds, WindowOptions};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::ai::agent::discovery::{self, AgentInstall};
use zssh_core::ai::agent::download::ReleaseInfo;
use zssh_core::ai::history::HistoryStore;
use zssh_core::store::Database;

use crate::agent_setup::download::{self as agent_download, format_bytes, percent};
use crate::agent_setup::status::{
    login_followup, login_text, skipped_older_note, source_label, too_old_note,
};
use crate::agent_setup::{self, AgentKind, InstallState};
use crate::agent_text::redact_paths;
use crate::app_theme::Tokens;
use crate::ssh_backend::runtime;
use crate::typography::{Role, Typo as _};

/// 后台结果轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 清除手动路径时写进设置库的值：JSON 空串（和 `store::settings` 的约定一致）。
const EMPTY_PATH_SETTING: &str = "\"\"";

/// 执行模式的设置值、界面标签和说明（0.2.14 `EXEC_MODE_OPTIONS`）。
const EXEC_MODES: [(&str, &str, &str); 3] = [
    ("manual", "逐条确认", "每条命令都要你点确认。"),
    (
        "readonly",
        "只读自动",
        "查看类命令（ls、cat、systemctl status 等）直接执行，会改动系统的命令和写文件要确认。",
    ),
    (
        "auto",
        "全部放行",
        "所有命令直接执行，不再确认（危险命令也一样）。",
    ),
];

/// 写文件备份份数的范围（0.2.14 的滑块 1～20，设置库默认值见 core）。
const BACKUP_KEEP_MIN: usize = 1;
const BACKUP_KEEP_MAX: usize = 20;

/// 每个智能体块下面的"发现方式 / 安装方法"说明（0.2.14 逐字）。
fn block_description(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::OpenCode => {
            "发现方式：自动按 数据目录 tools → PATH → 常见安装位置 的顺序找；手动只用指定路径（坏了就报错，不回退）。"
        }
        AgentKind::Codex => {
            "codex 不提供一键下载：先 `npm i -g @openai/codex`，或在这里手动指定路径。发现方式同上；模型和登录用 codex 自己的配置。"
        }
        AgentKind::Builtin => "ZeePilot 不需要安装：在上面的\"ZeePilot\"里添加接口即可。",
        AgentKind::Claude => {
            "Claude Code 不提供一键下载：先 `npm i -g @anthropic-ai/claude-code`（或官方安装脚本，装到 ~/.local/bin），也可以在这里手动指定路径。发现方式同上；登录和模型用 Claude Code 自己的配置（不改 CLAUDE_CONFIG_DIR / HOME）。"
        }
    }
}

/// 后台任务 → 界面的单向消息（100ms tick 取走）。
enum Event {
    /// 「检测并保存」的探测结果；`seq` 对不上（中途切回了自动、又点了一次）就作废。
    Probed {
        kind: AgentKind,
        seq: u64,
        value: String,
        result: Result<AgentInstall, String>,
    },
    /// 查到的下载信息（确认框要显示）。
    DownloadInfo(Result<ReleaseInfo, String>),
    Progress {
        received: u64,
        total: u64,
    },
    Downloaded(Result<AgentInstall, String>),
    HistoryCleared(Result<usize, String>),
}

/// 一个智能体块的界面状态。
struct Block {
    path: Entity<InputState>,
    /// 开关拨到「手动」但还没保存路径：先显示输入框，点「检测并保存」通过了才存。
    manual_pending: bool,
    probing: bool,
    /// 每次「检测并保存」和每次拨开关都加一：检测结果回来时序号对不上就作废。
    probe_seq: u64,
    /// 上一次操作的提示（可用 / 失败原因 / 保存失败）。
    note: Option<(String, bool)>,
}

/// 一键下载的界面状态。
enum DownloadUi {
    Idle,
    /// 正在查 Release 信息。
    Fetching,
    /// 等用户确认（来源 + 版本 + 大小）。
    Confirm(ReleaseInfo),
    Running {
        received: u64,
        total: u64,
    },
}

pub struct AgentsDemo {
    /// 嵌在设置页里（没有自己的标题和边距）。
    embedded: bool,
    blocks: [Block; 3],
    /// 当前生效的手动路径（设置库里读到的）。
    saved: [Option<String>; 3],
    status: agent_setup::Snapshot,
    default_agent: AgentKind,
    exec_mode: String,
    backup_keep: usize,
    download: DownloadUi,
    download_error: Option<String>,
    download_note: Option<String>,
    download_cancel: Arc<AtomicBool>,
    /// 「清空 AI 历史」点过一次、等第二次确认。
    clear_confirming: bool,
    clear_note: Option<(String, bool)>,
    /// 设置库；打不开时为 `None`，路径改动只在界面上提示、不保存。
    db: Option<Database>,
    storage_error: Option<String>,
    /// 设置读 / 写失败的说明（不影响路径块）。
    settings_error: Option<String>,
    events: Receiver<Event>,
    sender: Sender<Event>,
    focus: FocusHandle,
    _tick: Task<()>,
}

impl AgentsDemo {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::build(false, window, cx)
    }

    /// 嵌在主窗口设置页里的形态。
    pub fn new_embedded(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::build(true, window, cx)
    }

    fn build(embedded: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        let (sender, events) = std::sync::mpsc::channel();
        // 打开时读一次保存的路径和 AI 设置；库打不开就退回自动发现，并在底部说明。
        let (db, storage_error) = match open_settings_db() {
            Ok(db) => (Some(db), None),
            Err(err) => (None, Some(err)),
        };
        let mut settings_error = None;
        let mut saved: [Option<String>; 3] = std::array::from_fn(|_| None);
        let mut default_agent = AgentKind::Builtin;
        let mut exec_mode = "manual".to_owned();
        let mut backup_keep = 5;
        if let Some(db) = &db {
            let mut problems = Vec::new();
            for kind in AgentKind::ALL {
                match kind.saved_path(db) {
                    Ok(path) => saved[kind.index()] = path,
                    Err(err) => problems.push(format!("读取 {} 的路径失败：{err}", kind.label())),
                }
            }
            // 用户选过的以用户为准；没选过默认就是 ZeePilot（V2-U32），不管配没配好。
            match db.ai_default_agent() {
                Ok(_) => default_agent = agent_setup::default_agent(Some(db)),
                Err(err) => problems.push(format!("读取默认智能体失败：{err}")),
            }
            match db.ai_exec_mode() {
                Ok(value) => exec_mode = value,
                Err(err) => problems.push(format!("读取执行模式失败：{err}")),
            }
            match db.ai_write_backup_keep() {
                Ok(value) => backup_keep = value,
                Err(err) => problems.push(format!("读取备份份数失败：{err}")),
            }
            if !problems.is_empty() {
                settings_error = Some(problems.join("；"));
            }
        }
        let blocks = std::array::from_fn(|index| {
            let kind = AgentKind::ALL[index];
            let initial = saved[index].clone().unwrap_or_default();
            Block {
                path: cx.new(|cx| {
                    InputState::new(window, cx)
                        .default_value(initial)
                        .placeholder(kind.path_placeholder())
                }),
                manual_pending: false,
                probing: false,
                probe_seq: 0,
                note: None,
            }
        });
        let mut this = Self {
            embedded,
            blocks,
            saved,
            status: agent_setup::snapshot(),
            default_agent,
            exec_mode,
            backup_keep,
            download: DownloadUi::Idle,
            download_error: None,
            download_note: None,
            download_cancel: Arc::new(AtomicBool::new(false)),
            clear_confirming: false,
            clear_note: None,
            db,
            storage_error,
            settings_error,
            events,
            sender,
            focus,
            _tick: Task::ready(()),
        };
        // 100ms 取一次后台结果和检测状态；窗口没了 `update_in` 会报错，任务自己退出。
        this._tick = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                let alive = this.update_in(cx, |this, _window, cx| this.poll(cx));
                if alive.is_err() {
                    break;
                }
            }
        });
        this
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    // ---- 状态 ----

    /// 存了手动路径，或者刚拨到「手动」还没保存，都算手动模式；留空是自动（默认）。
    fn is_manual(&self, kind: AgentKind) -> bool {
        self.saved[kind.index()].is_some() || self.blocks[kind.index()].manual_pending
    }

    fn db_unavailable(&self) -> String {
        format!(
            "设置库不可用：{}；改动不会保存。",
            self.storage_error.as_deref().unwrap_or("打开失败")
        )
    }

    fn poll(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        if agent_setup::version() != self.status.version {
            self.status = agent_setup::snapshot();
            // 状态变了顺手重读默认智能体（别的窗口 / 同步可能改了它）。
            self.default_agent = agent_setup::default_agent(self.db.as_ref());
            changed = true;
        }
        while let Ok(event) = self.events.try_recv() {
            changed = true;
            self.apply_event(event);
        }
        if changed {
            cx.notify();
        }
    }

    fn apply_event(&mut self, event: Event) {
        match event {
            Event::Probed {
                kind,
                seq,
                value,
                result,
            } => {
                // 检测期间切回了自动（或又点了一次）：这次结果作废，别把路径又存回去。
                if seq != self.blocks[kind.index()].probe_seq {
                    return;
                }
                self.blocks[kind.index()].probing = false;
                match result {
                    Ok(install) => {
                        let version = install.version.trim().to_owned();
                        match self.save_path(kind, &value) {
                            Ok(()) => {
                                let block = &mut self.blocks[kind.index()];
                                block.manual_pending = false;
                                block.note = Some((format!("可用：{version}"), false));
                                agent_setup::refresh(kind);
                            }
                            // 保存失败：设置没变，输入框里的草稿留着。
                            Err(err) => self.blocks[kind.index()].note = Some((err, true)),
                        }
                    }
                    Err(err) => {
                        self.blocks[kind.index()].note = Some((redact_paths(&err), true));
                    }
                }
            }
            Event::DownloadInfo(result) => match result {
                Ok(info) => self.download = DownloadUi::Confirm(info),
                Err(err) => {
                    self.download = DownloadUi::Idle;
                    self.download_error = Some(redact_paths(&err));
                }
            },
            Event::Progress { received, total } => {
                if let DownloadUi::Running { .. } = self.download {
                    self.download = DownloadUi::Running { received, total };
                }
            }
            Event::Downloaded(result) => {
                self.download = DownloadUi::Idle;
                match result {
                    Ok(install) => {
                        self.download_note =
                            Some(format!("已下载 opencode {}", install.version.trim()));
                        // 下载结果不一定就是发现会选中的那一个：按同样的规则重新检测，显示的才是
                        // AI 面板实际会用的。
                        agent_setup::refresh(AgentKind::OpenCode);
                    }
                    Err(err) => self.download_error = Some(redact_paths(&err)),
                }
            }
            Event::HistoryCleared(result) => {
                self.clear_confirming = false;
                self.clear_note = Some(match result {
                    Ok(removed) => (format!("已清空 AI 历史（{removed} 个会话）"), false),
                    Err(err) => (format!("清空 AI 历史失败：{err}"), true),
                });
            }
        }
    }

    // ---- 设置读写 ----

    fn set_default_agent(&mut self, kind: AgentKind, cx: &mut Context<Self>) {
        match &self.db {
            Some(db) => match db.set_ai_default_agent(kind.id()) {
                Ok(()) => {
                    self.default_agent = kind;
                    self.settings_error = None;
                }
                Err(err) => self.settings_error = Some(format!("保存默认智能体失败：{err}")),
            },
            None => self.settings_error = Some(self.db_unavailable()),
        }
        cx.notify();
    }

    fn set_exec_mode(&mut self, mode: &str, cx: &mut Context<Self>) {
        match &self.db {
            Some(db) => match db.set_ai_exec_mode(mode) {
                Ok(()) => {
                    self.exec_mode = mode.to_owned();
                    self.settings_error = None;
                }
                Err(err) => self.settings_error = Some(format!("保存执行模式失败：{err}")),
            },
            None => self.settings_error = Some(self.db_unavailable()),
        }
        cx.notify();
    }

    fn set_backup_keep(&mut self, keep: usize, cx: &mut Context<Self>) {
        let keep = keep.clamp(BACKUP_KEEP_MIN, BACKUP_KEEP_MAX);
        match &self.db {
            Some(db) => match db.setting_set(
                zssh_core::store::settings::AI_WRITE_BACKUP_KEEP_KEY,
                &keep.to_string(),
            ) {
                Ok(()) => {
                    self.backup_keep = keep;
                    self.settings_error = None;
                }
                Err(err) => self.settings_error = Some(format!("保存备份份数失败：{err}")),
            },
            None => self.settings_error = Some(self.db_unavailable()),
        }
        cx.notify();
    }

    /// 写手动路径；空串清除。库不可用时返回给界面看的说明，不 panic。
    fn save_path(&mut self, kind: AgentKind, input: &str) -> Result<(), String> {
        let Some(db) = &self.db else {
            return Err(self.db_unavailable());
        };
        let saved = save_path_setting(db, kind.path_key(), input)?;
        self.saved[kind.index()] = saved;
        Ok(())
    }

    // ---- 智能体块的操作 ----

    /// 「重新检测」。
    fn recheck(&mut self, kind: AgentKind, cx: &mut Context<Self>) {
        self.blocks[kind.index()].note = None;
        agent_setup::refresh(kind);
        cx.notify();
    }

    /// 自动 / 手动开关。拨到手动：先出输入框，检测通过才保存；切回自动：清掉手动路径。
    fn toggle_manual(
        &mut self,
        kind: AgentKind,
        manual: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let block = &mut self.blocks[kind.index()];
        block.probe_seq += 1;
        block.probing = false;
        block.note = None;
        if manual {
            block.manual_pending = true;
            cx.notify();
            return;
        }
        block.manual_pending = false;
        block
            .path
            .update(cx, |state, cx| state.set_value("", window, cx));
        if self.saved[kind.index()].is_some() {
            match self.save_path(kind, "") {
                Ok(()) => agent_setup::refresh(kind),
                Err(err) => self.blocks[kind.index()].note = Some((err, true)),
            }
        }
        cx.notify();
    }

    /// 「检测并保存」：先探测这个路径，能用才写进设置、再重新检测。
    fn probe_and_save(&mut self, kind: AgentKind, cx: &mut Context<Self>) {
        let value = self.blocks[kind.index()].path.read(cx).value().to_string();
        let value = value.trim().to_owned();
        let block = &mut self.blocks[kind.index()];
        if value.is_empty() {
            block.note = Some(("请填可执行文件的完整路径".to_owned(), true));
            cx.notify();
            return;
        }
        block.probe_seq += 1;
        block.probing = true;
        block.note = None;
        let seq = block.probe_seq;
        let sender = self.sender.clone();
        // 手动路径只有外部智能体有（内置助手不会走到这里）。
        let Some(spec) = kind.spec() else {
            return;
        };
        match runtime() {
            Ok(runtime) => {
                let task_value = value.clone();
                runtime.spawn(async move {
                    let result =
                        discovery::probe_path(spec, std::path::Path::new(&task_value)).await;
                    let _ = sender.send(Event::Probed {
                        kind,
                        seq,
                        value: task_value,
                        result,
                    });
                });
            }
            Err(err) => {
                let block = &mut self.blocks[kind.index()];
                block.probing = false;
                block.note = Some((err, true));
            }
        }
        cx.notify();
    }

    // ---- 一键下载（只有 opencode）----

    /// 「下载 opencode」：先查 Release 信息，弹确认（来源 + 大小），确认后才下。
    fn start_download(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.download, DownloadUi::Idle) {
            return;
        }
        self.download = DownloadUi::Fetching;
        self.download_error = None;
        self.download_note = None;
        let sender = self.sender.clone();
        let data_dir = crate::settings::data_dir();
        match runtime() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    let result = match agent_setup::fake::world() {
                        Some(world) => world.release_info(),
                        None => {
                            let proxy = agent_download::proxy_url(&data_dir);
                            agent_download::fetch_info(proxy.as_deref()).await
                        }
                    };
                    let _ = sender.send(Event::DownloadInfo(result));
                });
            }
            Err(err) => {
                self.download = DownloadUi::Idle;
                self.download_error = Some(err);
            }
        }
        cx.notify();
    }

    /// 确认框里点了取消（没开始下载）。
    fn cancel_download_confirm(&mut self, cx: &mut Context<Self>) {
        if matches!(self.download, DownloadUi::Confirm(_)) {
            self.download = DownloadUi::Idle;
            cx.notify();
        }
    }

    /// 确认框里点了「开始下载」。
    fn confirm_download(&mut self, cx: &mut Context<Self>) {
        let DownloadUi::Confirm(info) = std::mem::replace(&mut self.download, DownloadUi::Idle)
        else {
            return;
        };
        self.download = DownloadUi::Running {
            received: 0,
            total: info.size,
        };
        // 取消标志先清零（上一次取消过）。
        self.download_cancel.store(false, Ordering::SeqCst);
        let cancel = Arc::clone(&self.download_cancel);
        let sender = self.sender.clone();
        let data_dir = crate::settings::data_dir();
        match runtime() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    let progress_sender = sender.clone();
                    let on_progress = move |received: u64, total: u64| {
                        let _ = progress_sender.send(Event::Progress { received, total });
                    };
                    let result = match agent_setup::fake::world() {
                        Some(world) => world.download(on_progress, &cancel).await,
                        None => {
                            let proxy = agent_download::proxy_url(&data_dir);
                            agent_download::download_and_probe(
                                &info,
                                &data_dir,
                                proxy.as_deref(),
                                on_progress,
                                &cancel,
                            )
                            .await
                        }
                    };
                    let _ = sender.send(Event::Downloaded(result));
                });
            }
            Err(err) => {
                self.download = DownloadUi::Idle;
                self.download_error = Some(err);
            }
        }
        cx.notify();
    }

    // ---- 清空 AI 历史 ----

    fn clear_history(&mut self, cx: &mut Context<Self>) {
        if !self.clear_confirming {
            self.clear_confirming = true;
            self.clear_note = None;
            cx.notify();
            return;
        }
        let sender = self.sender.clone();
        let data_dir = crate::settings::data_dir();
        match runtime() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        HistoryStore::new(&data_dir)
                            .clear()
                            .map_err(|err| err.to_string())
                    })
                    .await
                    .unwrap_or_else(|err| Err(format!("后台任务失败：{err}")));
                    let _ = sender.send(Event::HistoryCleared(result));
                });
            }
            Err(err) => self.clear_note = Some((err, true)),
        }
        cx.notify();
    }
}

// ---- 驱动脚本用的读写口（调试版自动化；走和按钮同一批方法）----

impl AgentsDemo {
    /// 这个智能体块上"当前：…"那一行加所有提示合成的文字（断言用）。
    pub fn dev_text(&self, kind: AgentKind) -> String {
        let status = self.status.get(kind);
        let mut out = match &status.install {
            InstallState::Checking => "检测中…".to_owned(),
            InstallState::Missing | InstallState::Invalid(_) => "未找到或手动路径不可用".to_owned(),
            InstallState::Found(install) => format!(
                "{}（{}）",
                install.version.trim(),
                source_label(kind, install.source)
            ),
        };
        if let InstallState::Found(install) = &status.install {
            out.push_str(&format!(
                "
路径 {}",
                redact_paths(&install.path)
            ));
            if install.too_old {
                out.push_str(&format!(
                    "
{}",
                    too_old_note(kind)
                ));
            }
            if let Some(note) = skipped_older_note(install) {
                out.push_str(&format!(
                    "
{note}"
                ));
            }
        }
        if let InstallState::Invalid(reason) = &status.install {
            out.push_str(&format!(
                "
{}",
                redact_paths(reason)
            ));
        }
        if let Some(login) = &status.login {
            out.push_str(&format!(
                "
登录：{}",
                login_text(login.logged_in)
            ));
            if let Some(text) = login_followup(kind, login) {
                out.push_str(&format!(
                    "
{text}"
                ));
            }
        }
        if let Some(guard) = &status.guard {
            out.push_str(&format!(
                "
{guard}"
            ));
        }
        if let Some((note, _)) = &self.blocks[kind.index()].note {
            out.push_str(&format!(
                "
{note}"
            ));
        }
        out
    }

    pub fn dev_is_manual(&self, kind: AgentKind) -> bool {
        self.is_manual(kind)
    }

    pub fn dev_set_manual(
        &mut self,
        kind: AgentKind,
        manual: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_manual(kind, manual, window, cx);
    }

    pub fn dev_type_path(
        &mut self,
        kind: AgentKind,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = text.to_owned();
        self.blocks[kind.index()]
            .path
            .update(cx, |state, cx| state.set_value(text, window, cx));
    }

    pub fn dev_probe_and_save(&mut self, kind: AgentKind, cx: &mut Context<Self>) {
        self.probe_and_save(kind, cx);
    }

    pub fn dev_recheck(&mut self, kind: AgentKind, cx: &mut Context<Self>) {
        self.recheck(kind, cx);
    }

    pub fn dev_saved_path(&self, kind: AgentKind) -> Option<String> {
        self.saved[kind.index()].clone()
    }

    pub fn dev_set_default_agent(&mut self, kind: AgentKind, cx: &mut Context<Self>) {
        self.set_default_agent(kind, cx);
    }

    pub fn dev_set_exec_mode(&mut self, mode: &str, cx: &mut Context<Self>) {
        self.set_exec_mode(mode, cx);
    }

    /// 下载流程走到哪一步：`idle` / `fetching` / `confirm` / `running`。
    pub fn dev_download_state(&self) -> &'static str {
        match self.download {
            DownloadUi::Idle => "idle",
            DownloadUi::Fetching => "fetching",
            DownloadUi::Confirm(_) => "confirm",
            DownloadUi::Running { .. } => "running",
        }
    }

    /// 下载的结果提示（成功 / 失败）合成的文字。
    pub fn dev_download_text(&self) -> String {
        let mut out = String::new();
        if let Some(note) = &self.download_note {
            out.push_str(note);
        }
        if let Some(err) = &self.download_error {
            out.push_str(&format!("下载失败：{err}"));
        }
        out
    }

    pub fn dev_download(&mut self, step: &str, cx: &mut Context<Self>) -> Result<(), String> {
        match step {
            "start" => self.start_download(cx),
            "confirm" => self.confirm_download(cx),
            "cancel" => self.cancel_download_confirm(cx),
            _ => return Err("下载步骤只能是 start / confirm / cancel".to_owned()),
        }
        Ok(())
    }

    pub fn dev_clear_history(&mut self, cx: &mut Context<Self>) {
        self.clear_history(cx);
    }

    pub fn dev_clear_note(&self) -> Option<String> {
        self.clear_note.as_ref().map(|(note, _)| note.clone())
    }

    pub fn dev_default_agent(&self) -> &'static str {
        self.default_agent.id()
    }

    pub fn dev_exec_mode(&self) -> String {
        self.exec_mode.clone()
    }
}

impl Drop for AgentsDemo {
    fn drop(&mut self) {
        // 页面关了还在下的下载就停掉，别在后台把包写完。
        self.download_cancel.store(true, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// 画
// ---------------------------------------------------------------------------

/// 一行设置的标题 + 说明（左边）和控件（右边）。
fn setting_row(
    title: &'static str,
    description: Option<String>,
    control: impl IntoElement,
    tokens: &Tokens,
) -> AnyElement {
    h_flex()
        .w_full()
        .min_h(::gpui::px(28.0))
        .gap_4()
        .items_center()
        .justify_between()
        .child(
            v_flex()
                .gap_0p5()
                .min_w_0()
                .child(div().typo(Role::ItemName, tokens).child(title))
                .when_some(description, |d, text| {
                    d.child(div().typo(Role::Caption, tokens).child(text))
                }),
        )
        .child(h_flex().flex_none().gap_1().items_center().child(control))
        .into_any_element()
}

impl AgentsDemo {
    /// 默认智能体：三个按钮里高亮当前的（0.2.14 是下拉，这里和主题 / 光标行一样用分段按钮）。
    fn default_agent_row(&self, entity: &Entity<Self>, tokens: &Tokens) -> AnyElement {
        let segments = AgentKind::SELECTABLE
            .iter()
            .map(|kind| crate::settings_view::Segment {
                id: format!("ai-default-agent-{}", kind.id()).into(),
                label: kind.label(),
                icon: None,
                selected: self.default_agent == *kind,
            })
            .collect();
        let entity = entity.clone();
        setting_row(
            "默认智能体",
            Some("打开 AI 面板时默认用哪一个（面板顶部还能按对话切换）。".to_owned()),
            crate::settings_view::choice_group(segments, tokens, move |index, _, cx| {
                if let Some(kind) = AgentKind::SELECTABLE.get(index).copied() {
                    entity.update(cx, |this, cx| this.set_default_agent(kind, cx));
                }
            }),
            tokens,
        )
    }

    fn exec_mode_row(&self, entity: &Entity<Self>, tokens: &Tokens) -> AnyElement {
        let current = EXEC_MODES
            .iter()
            .find(|(value, _, _)| *value == self.exec_mode)
            .map_or("manual", |(value, _, _)| *value);
        let segments = EXEC_MODES
            .iter()
            .map(|(value, label, _)| crate::settings_view::Segment {
                id: format!("ai-exec-mode-{value}").into(),
                label,
                icon: None,
                selected: current == *value,
            })
            .collect();
        let entity = entity.clone();
        let description = EXEC_MODES
            .iter()
            .map(|(_, label, hint)| format!("{label}：{hint}"))
            .collect::<Vec<_>>()
            .join(" ");
        setting_row(
            "执行模式",
            Some(description),
            crate::settings_view::choice_group(segments, tokens, move |index, _, cx| {
                if let Some((value, _, _)) = EXEC_MODES.get(index).copied() {
                    entity.update(cx, |this, cx| this.set_exec_mode(value, cx));
                }
            }),
            tokens,
        )
    }

    fn backup_row(&self, entity: &Entity<Self>, tokens: &Tokens) -> AnyElement {
        let (minus, plus) = (entity.clone(), entity.clone());
        let keep = self.backup_keep;
        setting_row(
            "写文件备份份数",
            Some(
                "AI 覆盖服务器文件时，每个文件只保留最近几份 .zssh-bak-* 备份，更早的删掉。"
                    .to_owned(),
            ),
            h_flex()
                .gap_1()
                .items_center()
                .child(
                    Button::new("ai-backup-minus")
                        .label("-")
                        .compact()
                        .on_click(move |_, _, cx| {
                            minus.update(cx, |this, cx| {
                                this.set_backup_keep(this.backup_keep.saturating_sub(1), cx)
                            });
                        }),
                )
                .child(
                    div()
                        .w(::gpui::px(52.0))
                        .text_sm()
                        .text_center()
                        .text_color(tokens.text)
                        .child(format!("{keep} 份")),
                )
                .child(Button::new("ai-backup-plus").label("+").compact().on_click(
                    move |_, _, cx| {
                        plus.update(cx, |this, cx| {
                            this.set_backup_keep(this.backup_keep + 1, cx)
                        });
                    },
                )),
            tokens,
        )
    }

    fn history_row(&self, entity: &Entity<Self>, tokens: &Tokens) -> AnyElement {
        let click = entity.clone();
        let cancel = entity.clone();
        let button = if self.clear_confirming {
            h_flex()
                .gap_1()
                .child(
                    Button::new("ai-history-clear-cancel")
                        .label("取消")
                        .compact()
                        .on_click(move |_, _, cx| {
                            cancel.update(cx, |this, cx| {
                                this.clear_confirming = false;
                                cx.notify();
                            });
                        }),
                )
                .child(
                    Button::new("ai-history-clear")
                        .label("确认清空")
                        .compact()
                        .danger()
                        .on_click(move |_, _, cx| {
                            click.update(cx, |this, cx| this.clear_history(cx));
                        }),
                )
                .into_any_element()
        } else {
            Button::new("ai-history-clear")
                .label("清空 AI 历史")
                .compact()
                .on_click(move |_, _, cx| {
                    click.update(cx, |this, cx| this.clear_history(cx));
                })
                .into_any_element()
        };
        let mut description = String::from(
            "AI 面板的历史会话保存在本机数据目录里（每种智能体最多 50 个），是未加密的明文，\
             包括命令输出的摘要（每条最多 4 KB）——如果让 AI 看过 .env、密钥之类的文件，内容可能在里面。\
             不需要时可以清空，清空后不能恢复。",
        );
        if let Some((note, _)) = &self.clear_note {
            description.push('\n');
            description.push_str(note);
        }
        setting_row("AI 历史", Some(description), button, tokens)
    }

    /// 一个智能体的块：当前版本和来源、登录、按钮行、自动 / 手动开关、各种提示、手动路径输入。
    fn agent_block(&self, kind: AgentKind, entity: &Entity<Self>, tokens: &Tokens) -> AnyElement {
        let status = self.status.get(kind);
        let block = &self.blocks[kind.index()];
        let manual = self.is_manual(kind);
        let muted = tokens.text_muted;
        let warning = tokens.dot_connecting;
        let danger = tokens.danger;
        let found = status.found();

        let detected_line = match &status.install {
            InstallState::Checking => "检测中…".to_owned(),
            InstallState::Missing => {
                if manual {
                    "手动路径不可用".to_owned()
                } else {
                    "未找到".to_owned()
                }
            }
            InstallState::Invalid(_) => "手动路径不可用".to_owned(),
            InstallState::Found(install) => format!(
                "{}（{}）",
                install.version.trim(),
                source_label(kind, install.source)
            ),
        };
        let login_part = match &status.login {
            Some(login) if kind != AgentKind::OpenCode => {
                format!("；登录：{}", login_text(login.logged_in))
            }
            _ => String::new(),
        };

        let small = |text: String, color| div().text_xs().text_color(color).child(text);
        let mut lines: Vec<AnyElement> =
            vec![small(block_description(kind).to_owned(), muted).into_any_element()];
        if let Some(install) = found {
            if install.auto {
                lines.push(
                    small(
                        format!(
                            "已自动发现：{}（{}）",
                            redact_paths(&install.path),
                            install.version.trim()
                        ),
                        muted,
                    )
                    .into_any_element(),
                );
            } else {
                lines.push(
                    small(redact_paths(&install.path), muted)
                        .font_family("Consolas")
                        .into_any_element(),
                );
            }
            if !install.too_old
                && let Some(note) = skipped_older_note(install)
            {
                lines.push(small(note, muted).into_any_element());
            }
            if install.too_old {
                lines.push(small(too_old_note(kind), warning).into_any_element());
            }
        }
        if let InstallState::Invalid(reason) = &status.install {
            lines.push(small(redact_paths(reason), danger).into_any_element());
        }
        if kind != AgentKind::OpenCode
            && let Some(login) = &status.login
            && let Some(text) = login_followup(kind, login)
        {
            lines.push(small(redact_paths(&text), warning).into_any_element());
        }
        if let Some(guard) = &status.guard {
            lines.push(
                small(
                    format!("{}（点「重新检测」重试）", redact_paths(guard)),
                    danger,
                )
                .into_any_element(),
            );
        }
        if let Some((note, error)) = &block.note {
            lines.push(
                small(
                    note.clone(),
                    if *error { danger } else { tokens.dot_connected },
                )
                .into_any_element(),
            );
        }

        // 按钮行：重新检测、（opencode 的）下载、右边自动 / 手动开关。
        let recheck = {
            let entity = entity.clone();
            Button::new(("agent-recheck", kind.index()))
                .label(if block.probing {
                    "检测中…"
                } else {
                    "重新检测"
                })
                .compact()
                .disabled(block.probing)
                .on_click(move |_, _, cx| {
                    entity.update(cx, |this, cx| this.recheck(kind, cx));
                })
        };
        let download_button = (kind == AgentKind::OpenCode).then(|| {
            let entity = entity.clone();
            let busy = !matches!(self.download, DownloadUi::Idle);
            Button::new("agent-download")
                .label(if busy {
                    "下载中…"
                } else {
                    "下载 opencode"
                })
                .compact()
                .primary()
                .disabled(busy)
                .on_click(move |_, _, cx| {
                    entity.update(cx, |this, cx| this.start_download(cx));
                })
        });
        let switch = {
            let entity = entity.clone();
            h_flex()
                .gap_1p5()
                .items_center()
                .text_xs()
                .child(
                    div()
                        .text_color(if manual { muted } else { tokens.text })
                        .child("自动"),
                )
                .child(
                    Switch::new(("agent-manual", kind.index()))
                        .checked(manual)
                        .accessibility_label(format!("{} 手动指定路径", kind.label()))
                        .on_click(move |checked, window, cx| {
                            let manual = *checked;
                            entity.update(cx, |this, cx| {
                                this.toggle_manual(kind, manual, window, cx);
                            });
                        }),
                )
                .child(
                    div()
                        .text_color(if manual { tokens.text } else { muted })
                        .child("手动指定"),
                )
        };

        let mut column = v_flex()
            .id(("agent-block", kind.index()))
            .w_full()
            .gap_1()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(div().typo(Role::ItemName, tokens).child(kind.label()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(format!("当前：{detected_line}{login_part}")),
                    ),
            )
            .children(lines)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .mt_1p5()
                    .child(recheck)
                    .children(download_button)
                    .child(div().flex_1())
                    .child(switch),
            );
        if manual {
            let entity = entity.clone();
            column = column.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .mt_1()
                    .child(Input::new(&block.path).flex_1())
                    .child(
                        Button::new(("agent-probe", kind.index()))
                            .label("检测并保存")
                            .compact()
                            .disabled(block.probing)
                            .on_click(move |_, _, cx| {
                                entity.update(cx, |this, cx| this.probe_and_save(kind, cx));
                            }),
                    ),
            );
        }
        // 一键下载的确认框 / 进度 / 结果（只在 opencode 块下面）。
        if kind == AgentKind::OpenCode {
            column = column.children(self.download_panel(entity, tokens));
        }
        column.into_any_element()
    }

    /// opencode 块下面的下载确认 / 进度 / 失败 / 成功。
    fn download_panel(&self, entity: &Entity<Self>, tokens: &Tokens) -> Vec<AnyElement> {
        let mut out: Vec<AnyElement> = Vec::new();
        match &self.download {
            DownloadUi::Idle => {}
            DownloadUi::Fetching => out.push(
                div()
                    .typo(Role::Caption, tokens)
                    .child("正在查询最新版本…")
                    .into_any_element(),
            ),
            DownloadUi::Confirm(info) => {
                let (cancel, start) = (entity.clone(), entity.clone());
                let source = if info.page_url.is_empty() {
                    info.download_url.clone()
                } else {
                    info.page_url.clone()
                };
                out.push(
                    v_flex()
                        .id("agent-download-confirm")
                        .gap_1()
                        .mt_1()
                        .p_3()
                        .rounded_md()
                        .border_1()
                        .border_color(tokens.border)
                        .bg(tokens.background)
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .child("下载 opencode"),
                        )
                        .child(div().typo(Role::Caption, tokens).child("来源："))
                        .child(
                            div()
                                .typo(Role::Caption, tokens)
                                .font_family("Consolas")
                                .child(source),
                        )
                        .child(
                            div()
                                .text_xs()
                                .child(format!("版本：{}", info.version.trim())),
                        )
                        .child(
                            div()
                                .text_xs()
                                .child(format!("大小：{}", format_bytes(info.size))),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .justify_end()
                                .child(
                                    Button::new("agent-download-cancel")
                                        .label("取消")
                                        .compact()
                                        .on_click(move |_, _, cx| {
                                            cancel.update(cx, |this, cx| {
                                                this.cancel_download_confirm(cx)
                                            });
                                        }),
                                )
                                .child(
                                    Button::new("agent-download-start")
                                        .label("开始下载")
                                        .compact()
                                        .primary()
                                        .on_click(move |_, _, cx| {
                                            start.update(cx, |this, cx| this.confirm_download(cx));
                                        }),
                                ),
                        )
                        .into_any_element(),
                );
            }
            DownloadUi::Running { received, total } => {
                let pct = percent(*received, *total).unwrap_or(0);
                out.push(
                    v_flex()
                        .id("agent-download-progress")
                        .gap_1()
                        .mt_1()
                        .child(
                            div()
                                .typo(Role::Caption, tokens)
                                .child(format!("下载进度 {pct}%")),
                        )
                        .child(
                            div()
                                .h(::gpui::px(6.0))
                                .w_full()
                                .rounded_full()
                                .bg(tokens.subtle)
                                .child(
                                    div()
                                        .h_full()
                                        .rounded_full()
                                        .bg(tokens.accent)
                                        .w(::gpui::relative(pct as f32 / 100.0)),
                                ),
                        )
                        .into_any_element(),
                );
            }
        }
        if let Some(err) = &self.download_error
            && matches!(self.download, DownloadUi::Idle)
        {
            out.push(
                div()
                    .id("agent-download-error")
                    .text_xs()
                    .text_color(tokens.danger)
                    .child(format!("下载失败：{err}"))
                    .into_any_element(),
            );
        }
        if let Some(note) = &self.download_note
            && matches!(self.download, DownloadUi::Idle)
        {
            out.push(
                div()
                    .id("agent-download-note")
                    .text_xs()
                    .text_color(tokens.dot_connected)
                    .child(note.clone())
                    .into_any_element(),
            );
        }
        out
    }

    /// 设置页 AI 一节的全部内容（嵌入形态直接用，独立窗口外面再套滚动容器）。
    fn section(&self, entity: &Entity<Self>, cx: &App) -> AnyElement {
        let tokens = Tokens::current(cx);
        let mut rows: Vec<AnyElement> = vec![
            self.default_agent_row(entity, &tokens),
            self.exec_mode_row(entity, &tokens),
            self.backup_row(entity, &tokens),
            self.history_row(entity, &tokens),
        ];
        for kind in AgentKind::ALL {
            rows.push(self.agent_block(kind, entity, &tokens));
        }
        if let Some(err) = &self.settings_error {
            rows.push(
                div()
                    .text_xs()
                    .text_color(tokens.danger)
                    .child(redact_paths(err))
                    .into_any_element(),
            );
        }
        if let Some(err) = &self.storage_error {
            rows.push(
                div()
                    .text_xs()
                    .text_color(tokens.danger)
                    .child(redact_paths(&format!(
                        "设置库不可用：{err}；路径和设置的改动不会保存。"
                    )))
                    .into_any_element(),
            );
        }
        crate::settings_view::card(
            "ai-settings",
            "智能体与执行模式",
            Some("AI 助手通过外部智能体在服务器上执行命令，遵守下面的执行模式。"),
            &tokens,
            rows,
        )
    }
}

impl Render for AgentsDemo {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let section = self.section(&entity, cx);
        if self.embedded {
            return section;
        }
        let tokens = Tokens::current(cx);
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .bg(tokens.background)
            .child(
                div()
                    .id("agents-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_4()
                    .child(section),
            )
            .into_any_element()
    }
}

/// 打开 AI 智能体窗口（调试入口；产品里这些在设置页的 AI 一节）；同一个进程可以开多个。
pub fn open_agents_window(cx: &mut App) {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(
            size(::gpui::px(860.), ::gpui::px(720.)),
            cx,
        )),
        focus: true,
        ..Default::default()
    };
    let _ = cx.open_window(options, |window, cx| {
        let view = cx.new(|cx| AgentsDemo::new(window, cx));
        window.set_window_title("AI 智能体");
        window.focus(&view.read(cx).focus_handle(), cx);
        view
    });
}

/// 打开 `<数据目录>/zssh.db`（和主机库、Tauri 版同一个库）。
fn open_settings_db() -> Result<Database, String> {
    let dir = crate::settings::data_dir();
    std::fs::create_dir_all(&dir).map_err(|err| format!("创建数据目录失败：{err}"))?;
    Database::open(&dir.join("zssh.db")).map_err(|err| format!("打开设置库失败：{err}"))
}

/// 输入框内容 → 设置值：空白等于清除（`None`），否则是 JSON 编码后的字符串。
fn path_setting_value(input: &str) -> Result<Option<String>, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    serde_json::to_string(trimmed)
        .map(Some)
        .map_err(|err| format!("路径序列化失败：{err}"))
}

/// 把手动路径的输入写进设置库；返回生效的路径（`None` 表示已清除）。
fn save_path_setting(db: &Database, key: &str, input: &str) -> Result<Option<String>, String> {
    match path_setting_value(input)? {
        None => {
            db.setting_set(key, EMPTY_PATH_SETTING)
                .map_err(|err| format!("清除保存的路径失败：{err}"))?;
            Ok(None)
        }
        Some(encoded) => {
            db.setting_set(key, &encoded)
                .map_err(|err| format!("保存路径失败：{err}"))?;
            Ok(Some(input.trim().to_owned()))
        }
    }
}

#[cfg(test)]
mod tests {
    // 不写 `use super::*`：父模块 glob 进来的 `gpui_kit::test` 会盖掉内置的
    // `#[test]` 属性宏，展开时直接撞递归上限。
    use super::{EXEC_MODES, block_description, path_setting_value, save_path_setting};
    use crate::agent_setup::AgentKind;
    use zssh_core::store::Database;
    use zssh_core::store::settings::AI_OPENCODE_PATH_KEY;

    #[test]
    fn empty_path_clears_and_non_empty_is_json_encoded() {
        assert_eq!(path_setting_value(""), Ok(None));
        assert_eq!(path_setting_value("  \t "), Ok(None));

        let encoded = path_setting_value(r"  C:\Program Files\opencode.exe  ")
            .expect("编码路径")
            .expect("非空输入");
        assert!(encoded.starts_with('"') && encoded.ends_with('"'));
        assert_eq!(
            serde_json::from_str::<String>(&encoded).expect("解回路径"),
            r"C:\Program Files\opencode.exe"
        );
    }

    #[test]
    fn saving_a_path_round_trips_through_the_database() {
        let db = Database::open_in_memory().expect("内存库");
        // 空格、中文都得原样存取（可移植性：用户名 / 安装目录可以带空格和中文）。
        let path = r"C:\工具 箱\my agent\opencode.exe";

        let saved =
            save_path_setting(&db, AI_OPENCODE_PATH_KEY, &format!("  {path}  ")).expect("保存");
        assert_eq!(saved.as_deref(), Some(path));
        assert_eq!(db.ai_opencode_path().expect("读回").as_deref(), Some(path));

        let cleared = save_path_setting(&db, AI_OPENCODE_PATH_KEY, "   ").expect("清除");
        assert_eq!(cleared, None);
        assert_eq!(db.ai_opencode_path().expect("读回"), None);
    }

    #[test]
    fn exec_modes_match_the_values_the_bridge_reads() {
        let values: Vec<_> = EXEC_MODES.iter().map(|(value, _, _)| *value).collect();
        assert_eq!(values, ["manual", "readonly", "auto"]);
        let db = Database::open_in_memory().expect("内存库");
        assert_eq!(db.ai_exec_mode().expect("默认"), "manual");
        db.set_ai_exec_mode("readonly").expect("写");
        assert_eq!(db.ai_exec_mode().expect("读回"), "readonly");
    }

    #[test]
    fn every_agent_has_its_install_hint_text() {
        assert!(block_description(AgentKind::OpenCode).contains("数据目录 tools → PATH"));
        assert!(block_description(AgentKind::Codex).contains("npm i -g @openai/codex"));
        assert!(
            block_description(AgentKind::Claude).contains("npm i -g @anthropic-ai/claude-code")
        );
    }
}
