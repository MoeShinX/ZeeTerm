//! SFTP 文件视图：本地、远端两栏文件管理器 + 传输队列。
//!
//! 行为对照 0.2.14 的 `ui/src/components/sftp/`：文件列表（列、点表头排序、单选 / Ctrl / Shift /
//! Ctrl+A 多选、键盘上下 / 回车 / Backspace / F2 / F5 / Delete / Alt+Enter、面包屑和可编辑路径栏、
//! 过滤、隐藏文件开关、后退前进）、右键菜单（文件、文件夹、空白处、多选各一套，带子菜单“新建”）、
//! 对话框（新建 / 重命名、删除确认、修改权限、属性、下载到…、移动到…、解压同名文件、远端改名覆盖确认）、
//! 远端压缩 / 解压、传输队列。
//!
//! 代码分层：
//! - `model` / `menu` / `props` / `queue` / `archive_ui` / `dialogs` / `chmod` / `local_ops` / `move_to`：
//!   没有界面的规则和操作，每条都有单测；
//! - `engine`：同名文件提问者、递归删除、路径和名字规则、自动化钩子的环境变量解析；
//! - `pane` 保存一栏的状态，本文件和 `nav` / `ops` / `transfer` 是 [`SftpDemo`] 的状态与动作，
//!   `view` 只负责把状态画出来。
//!
//! 连接 → `realpath(".")` → 列目录；传输走 `zssh_core` 的 [`TransferEngine`]（另开一条传输通道，
//! 不占用浏览这条）。本地栏默认落在临时目录，不往用户自己的家目录 / 文档里写东西。窗口关掉就取消
//! 没传完的任务、丢掉通道，连接回到池里给终端用。
//!
//! 删除：确认对话框写明名字和“将递归删除其中所有内容”，点“删除”才开始；目录交给核心的递归删除
//! （遇到符号链接只删链接、不跟进去）；递归删除在后台跑，进度条显示已删项数，可以随时取消
//! （停在当前项、已删的不回滚、取消后数一遍剩余项），关窗口时停掉。有未完成的传输或删除时关窗口要确认。
//!
//! 整棵树里面的同名文件由引擎逐个提问，窗口里的同名文件面板回答（覆盖 / 跳过 / 续传 / 重命名 /
//! 全部覆盖 / 全部跳过）；`ZSSH_SFTP_CONFLICT` 给了值时改由自动化直接回答。
//!
//! 「全部覆盖 / 全部跳过」的范围是**一次提交**（一次上传 / 下载 / 拖放）：每次提交用
//! `engine.new_batch()` 开一个批次，任务用 `submit_in_batch` 排进去，批次里的任务结束后记住的
//! 选择随之作废，下一次提交重新问。和 0.2.14 的批次语义一致。

use std::collections::VecDeque;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use ::gpui::{App, FocusHandle, Subscription, Task, Window, WindowBounds, WindowOptions};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::*;
use zssh_core::sftp::archive::{ArchiveCaps, CompressOutcome, ExtractOutcome, ExtractPlan};
use zssh_core::sftp::path::join;
use zssh_core::sftp::size::SizeTotals;
use zssh_core::sftp::transfer::backend::{SftpChannelProvider, SftpEndpoints};
use zssh_core::sftp::transfer::{
    ConflictBatch, ConflictInfo, Direction, ProgressReport, TransferEngine, TransferEvent,
    TransferId, TransferState,
};
use zssh_core::sftp::{DirEntry, SftpChannel};
use zssh_core::store::Database;

use crate::install_state;
use crate::settings::data_dir;
use crate::ssh_backend::{SshSessionFactory, factory_for_host, runtime};

mod archive_ui;
mod chmod;
mod clip;
mod copy;
mod dialogs;
mod drag;
mod dup;
mod engine;
mod fixture;
mod follow;
mod header;
mod local_ops;
mod menu;
mod model;
mod move_flow;
mod move_to;
mod nav;
mod ops;
mod pane;
mod paste;
mod props;
mod queue;
mod script;
mod transfer;
mod view;
mod view_dialogs;
mod view_move;

pub(crate) use model::civil_from_days;
pub(crate) use view::local_utc_offset;

use dialogs::Dialog;
pub use drag::{SftpDrag, terminal_insert_text};
use engine::*;
pub(crate) use follow::FOLLOW_POLL_MS;
use menu::Side;
use model::NavMode;
use pane::Pane;

/// 新 SFTP 窗口的编号（每台主机可以有多个窗口）。
static NEXT_SFTP: AtomicU64 = AtomicU64::new(1);

/// 文件视图的编号：复制的源认视图（同一个视图里粘贴不用登记对端）。
static NEXT_VIEW: AtomicU64 = AtomicU64::new(1);

/// 窗口生命周期。`Ready` 之后还要等后台任务把目录读回来。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Connecting,
    Ready,
    Failed,
}

/// 新视图一开始要落在哪（复制标签时用原视图当前的两个目录）：远端读不出来就退回家目录，
/// 本地不是目录就退回默认的临时目录。
pub(crate) struct StartDirs {
    pub(crate) remote: String,
    pub(crate) local: String,
}

/// 后台任务 → 界面的单向事件（`std::sync::mpsc`，由 100ms tick 取走）。
enum SftpEvent {
    /// 连上了，并且第一条目录已经读回来。
    Ready {
        factory: Arc<SshSessionFactory>,
        channel: Arc<SftpChannel>,
        cwd: String,
        entries: Vec<DirEntry>,
    },
    /// 换目录成功。`seq` 是发起这次读取时的序号，只有最新一次的结果才算数。
    Listed {
        seq: u64,
        path: String,
        entries: Vec<DirEntry>,
        mode: NavMode,
        /// 刷新前想留在状态栏里的一句话（比如"已删除 XXX"），刷新回来一起显示。
        notice: Option<String>,
    },
    /// 连接 / 开通道 / 首条目录失败：窗口停在失败态。
    ConnectFailed(String),
    /// 换目录失败：保留旧列表，只换状态栏。旧请求的失败不理会（见 `Listed`）。
    ListFailed {
        seq: u64,
        path: String,
        mode: NavMode,
        message: String,
    },
    /// 传输的进度或结束事件（后台转发任务送过来）。
    Transfer(TransferEvent),
    /// 引擎撞上同名文件，在等界面回答（`respond` 用这个编号）。
    Conflict { id: u64, info: ConflictInfo },
    /// 递归删除的进度（后台任务限流后送来）。
    DeleteProgress {
        id: u64,
        deleted: u64,
        current: String,
    },
    /// 删除结束（成功、被取消或失败）。
    DeleteDone { id: u64, outcome: DeleteOutcome },
    /// 写操作（新建 / 重命名 / 改权限 / 本地删除）的结果；成功后刷新 `refresh` 那一栏。
    OpDone {
        ok: bool,
        message: String,
        refresh: Option<Side>,
    },
    /// 远端改名撞上同名文件：先不覆盖地试过了，问用户要不要覆盖。
    RenameExists {
        from: String,
        to: String,
        name: String,
    },
    /// 递归改权限的进度。
    ChmodProgress { changed: u64 },
    /// 属性对话框里“计算大小”的进度 / 结果。
    SizeProgress {
        request: u64,
        phase: props::CalcPhase,
        totals: SizeTotals,
        error: Option<String>,
    },
    /// 服务器上的压缩工具探测结果（`None` = 命令没跑成，下次右键再试）。
    ArchiveCaps(Option<ArchiveCaps>),
    /// 压缩的进度：临时压缩包已写多少字节。
    ArchiveProgress { op: u64, bytes: Option<u64> },
    /// 压缩结束（`Ok(None)` = 被取消）。
    CompressDone {
        op: u64,
        result: Result<Option<CompressOutcome>, String>,
    },
    /// 解压前检查结束（`Ok(None)` = 被取消）。
    ExtractPlanned {
        op: u64,
        target: dialogs::ExtractTarget,
        result: Result<Option<ExtractPlan>, String>,
    },
    /// 粘贴（服务器端复制）的进度：已复制几个文件。
    PasteProgress { op: u64, files_done: u64 },
    /// 粘贴的决定和服务器端复制做完了；中转 / 本地复制的任务已经排进引擎（`jobs`）。
    PasteDone {
        op: u64,
        result: Result<(clip::Outcome, Vec<paste::PastedJob>), String>,
    },
    /// 「移动到…」对话框里读回来一个目录的子文件夹（`request` 是发起时的序号）。
    MoveBrowsed {
        request: u64,
        dir: String,
        result: Result<Vec<model::Entry>, String>,
    },
    /// 「移动到…」对话框里新建文件夹的结果。
    MoveFolderMade {
        dir: String,
        path: String,
        result: Result<(), String>,
    },
    /// 「移动到…」里一项移动完了（成功、同名或失败）。
    MoveStep {
        job: u64,
        result: move_to::StepResult,
    },
    /// 解压结束（`Ok(None)` = 被取消）。
    ExtractDone {
        op: u64,
        ask: dialogs::ExtractAsk,
        result: Result<Option<ExtractOutcome>, String>,
    },
}

/// 自动化用的测试钩子：只在 Ready 之后执行一次。
enum TestHook {
    Download(String),
    DownloadDir(String),
    Upload(String),
    UploadDir(String),
    Mkdir(String),
    Rename {
        old: String,
        new: String,
    },
    Delete(String),
    /// `ZSSH_SFTP_CHMOD=<名字>:<八进制权限>`：改当前目录下某个条目的权限。
    Chmod {
        name: String,
        mode: u32,
    },
    /// `ZSSH_SFTP_DROP=<本地路径>`：走拖放上传的同一条处理链。
    Drop(String),
    /// `ZSSH_SFTP_EDIT=<名字>`：在当前目录下打开远端编辑窗口。
    Edit(String),
    /// 环境变量的值写错了：报出来，不猜用户想干什么。
    Invalid(String),
}

/// 钩子排进来的远端写操作。一个结束（`OpDone`）之后才排下一个：
/// 不然"建目录 → 改名 → 删除"会在服务器上抢跑。
enum RemoteOp {
    Mkdir(String),
    Rename { old: String, new: String },
    Delete(String),
    Chmod { name: String, mode: u32 },
}

/// 传输面板的一行。任务号在 `submit` 时就能拿到，所以先建行，进度事件回来再填数据。
struct TransferRow {
    id: TransferId,
    name: String,
    direction: Direction,
    /// 这个任务属于哪次提交的批次（「全部覆盖 / 跳过」的作用范围）。
    batch: Option<u64>,
    source: String,
    target: String,
    state: TransferState,
    total_files: u64,
    done_files: u64,
    total_bytes: u64,
    done_bytes: u64,
    speed_bps: u64,
    eta_secs: Option<u64>,
    current: Option<String>,
    notes: Vec<String>,
    started: Instant,
    /// 第一次进入终态的时间（用来算用时和平均速度）。
    finished: Option<Instant>,
}

impl TransferRow {
    fn new(
        id: TransferId,
        name: String,
        direction: Direction,
        batch: Option<u64>,
        source: String,
        target: String,
    ) -> Self {
        Self {
            id,
            name,
            direction,
            batch,
            source,
            target,
            state: TransferState::Queued,
            total_files: 0,
            done_files: 0,
            total_bytes: 0,
            done_bytes: 0,
            speed_bps: 0,
            eta_secs: None,
            current: None,
            notes: Vec::new(),
            started: Instant::now(),
            finished: None,
        }
    }

    fn apply(&mut self, report: &ProgressReport) {
        self.state = report.state.clone();
        self.total_files = report.total_files;
        self.done_files = report.done_files;
        self.total_bytes = report.total_bytes;
        self.done_bytes = report.done_bytes;
        self.speed_bps = report.speed_bps;
        self.eta_secs = report.eta_secs;
        self.current = report.current.clone();
        self.notes = report.notes.clone();
        if report.state.is_terminal() {
            self.finished.get_or_insert_with(Instant::now);
        } else {
            // 重试把任务改回排队：重新计时。
            self.finished = None;
        }
    }

    fn facts(&self) -> queue::RowFacts<'_> {
        queue::RowFacts {
            state: &self.state,
            total_files: self.total_files,
            done_files: self.done_files,
            total_bytes: self.total_bytes,
            done_bytes: self.done_bytes,
        }
    }

    fn elapsed(&self) -> Option<Duration> {
        self.finished
            .map(|end| end.saturating_duration_since(self.started))
    }
}

/// 服务器上有没有 tar / zip / unzip：第一次右键时探测，之后用缓存。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ArchiveCapsState {
    Idle,
    Loading,
    Ready(ArchiveCaps),
}

/// 服务器上正在跑的压缩 / 检查压缩包 / 解压（同一时间一个）。
struct Busy {
    kind: archive_ui::BusyKind,
    op: u64,
    started: Instant,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    cancelling: bool,
    bytes: Option<u64>,
    /// 粘贴：服务器端复制已经复制完几个文件。
    files: Option<u64>,
    /// 粘贴的是本地文件（没有"服务器上复制"，也没有取消按钮）。
    local: bool,
}

pub struct SftpDemo {
    /// 视图编号（见 `NEXT_VIEW`）。
    view_id: u64,
    /// 主机库 id：编辑器的草稿按它区分主机。
    host_id: i64,
    host_label: String,
    /// 调试夹具视图（`ZSSH_SFTP_FAKE`）：没有连接，远端栏是假数据，见 `fixture`。
    fixture: bool,
    phase: Phase,
    status: String,
    /// 状态栏这句话是不是出错 / 警告（用警示色）。
    status_is_error: bool,
    remote: Pane,
    local: Pane,
    /// 键盘焦点所在的栏（对话框关闭后把焦点还给它）。
    focused_side: Side,
    /// 当前打开的对话框（同一时间一个）。
    dialog: Option<Dialog>,
    /// 这个窗口给安装前检查登记的未结束传输数；每个窗口一条，总数是各窗口之和。
    install_transfers: install_state::Registration,
    transfers: Vec<TransferRow>,
    /// 传输队列展开着吗。
    queue_open: bool,
    /// 正在跑的递归删除（同一时间只有一个）。
    deleting: Option<DeleteJob>,
    next_delete: u64,
    /// 正在递归改权限：已改的项数。
    chmod_running: Option<u64>,
    /// 关窗口时还有未完成的传输 / 删除：正在问 继续传输 / 取消并关闭。
    close_prompt: bool,
    /// 用户已经确认可以关：`on_window_should_close` 不再拦。
    close_confirmed: bool,
    /// 输入法登记用的窗口名：真正关窗口时要注销。
    ime_key: String,
    /// 嵌在主窗口停靠面板里（远端在上、本地在下，标题由面板外壳画）。
    dock: bool,
    /// 停靠面板里本地栏收起了（0.2.14 `layout.sftp_dock_local_hidden`）。
    local_hidden: bool,
    /// 引擎的同名文件提问者：界面回答它，或者自动化钩子直接回答。
    resolver: Option<Arc<UiConflictResolver>>,
    /// `ZSSH_SFTP_CONFLICT`：有值就不弹面板（自动化测试用）。
    conflict_automation: Option<ConflictAutomation>,
    /// 引擎正在等回答的同名文件，按到达顺序显示最前面那个。
    conflicts: VecDeque<ConflictPrompt>,
    /// 正在提交的这一批任务的批次：`in_new_batch` 里设置，出来就清。
    batch_scope: Option<Arc<ConflictBatch>>,
    /// 目录列表请求序号。
    listing: ListingGate,
    /// 冲突面板里「重命名」用的名字。
    conflict_name_input: Entity<InputState>,
    /// 连上之后才有：传输引擎和浏览通道分开，互不拖累。
    engine: Option<Arc<TransferEngine>>,
    /// 引擎用的端点（属性对话框里数远端文件夹大小也用它列目录）。
    endpoints: Option<Arc<SftpEndpoints>>,
    /// 环境变量给的自动传输，Ready 之后跑一次。
    pending_hooks: Vec<TestHook>,
    /// `ZSSH_SFTP_PAUSE_MS=<n>`：Ready 之后 n 毫秒把任务暂停，2 秒后再继续。
    /// 只给手动 / 大文件测试用；没设置就是 `None`，界面行为完全不变。
    pause_hook_ms: Option<u64>,
    /// 暂停钩子的下一个到点时间；`None` = 钩子没在跑（没设置或者已经跑完）。
    pause_hook_deadline: Option<Instant>,
    /// 暂停钩子已经暂停过了，下一次到点要做的动作是"继续"。
    pause_hook_resuming: bool,
    /// 暂停钩子按下去的任务号：2 秒后按这批号继续，不依赖行状态在这期间刷新到 `Paused`。
    pause_hook_ids: Vec<TransferId>,
    /// 钩子排队的远端写操作，按顺序一个个跑。
    pending_ops: VecDeque<RemoteOp>,
    events: Receiver<SftpEvent>,
    sender: Sender<SftpEvent>,
    /// 连上之后一直持有：连接回到池里之后终端还能复用同一条。
    factory: Option<Arc<SshSessionFactory>>,
    channel: Option<Arc<SftpChannel>>,
    /// 压缩 / 解压 / 粘贴正在占用的服务器任务。
    busy: Option<Busy>,
    /// 传输数据面（跨主机复制时登记给对端的引擎）。
    provider: Option<Arc<SftpChannelProvider>>,
    /// 登录 shell 的类型缓存（粘贴时决定能不能用 cp）。
    shell_kind: Arc<std::sync::Mutex<Option<zssh_core::ai::shell_kind::ShellKind>>>,
    /// `cp` 失败后给的"改为经本机中转重试"。
    paste_retry: Option<copy::PasteRetry>,
    /// 跟随终端目录开着吗（停靠面板里的开关，默认关）。
    follow: bool,
    /// 最近一次粘贴的目标和源（失败后重试用）。
    last_paste: Option<copy::LastPaste>,
    /// 比行先到的进度（粘贴任务在后台排进引擎，行在界面线程建）：行建好后补上。
    orphan_reports: std::collections::HashMap<TransferId, ProgressReport>,
    /// 递归改权限的取消开关（关窗口时用）。
    chmod_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    archive_caps: ArchiveCapsState,
    /// 正在进行的「移动到…」（同一时间一个）。
    moving: Option<move_to::MoveJob>,
    next_move: u64,
    /// 「移动到…」对话框里目录读取的序号。
    move_browse_seq: u64,
    next_op: u64,
    /// 属性对话框里“计算大小”的编号（事件带着它回来认领）。
    next_calc: u64,
    focus: FocusHandle,
    /// 对话框（没有输入框的那几种）接收键盘用。
    dialog_focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
    _tick: Task<()>,
}

/// 0.2.14 设置里的“默认显示隐藏文件”（`sftp_show_hidden`）：两栏的初始开关。
fn default_show_hidden() -> bool {
    Database::open(&data_dir().join("zssh.db"))
        .ok()
        .and_then(|db| db.setting_get("sftp_show_hidden").ok().flatten())
        .is_some_and(|value| value == "true")
}

impl SftpDemo {
    fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        host_id: i64,
        ime_key: String,
        factory: Option<Arc<SshSessionFactory>>,
        start: Option<StartDirs>,
    ) -> Self {
        let (sender, events) = std::sync::mpsc::channel();
        let (start_remote, start_local) = match start {
            Some(dirs) => (Some(dirs.remote), Some(dirs.local)),
            None => (None, None),
        };
        let host_label = host_label(host_id);
        let show_hidden = default_show_hidden();

        let remote_filter = cx.new(|cx| InputState::new(window, cx).placeholder("过滤"));
        let local_filter = cx.new(|cx| InputState::new(window, cx).placeholder("过滤"));
        let conflict_name_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("新名字（留空 = 自动起名）"));
        let remote = Pane::new(show_hidden, remote_filter, cx.focus_handle());
        let mut local = Pane::new(show_hidden, local_filter, cx.focus_handle());
        // 本地栏先落地：初始目录必须是临时区，读不出来就空着，绝不回退到家目录。
        // 复制标签带来的目录必须还是个目录才用，否则按默认处理。
        let local_start = start_local
            .filter(|dir| !dir.is_empty() && Path::new(dir).is_dir())
            .unwrap_or_else(|| absolute_local(&initial_local_dir()).display().to_string());
        let mut local_status = String::new();
        match local_ops::list_dir(&local_start) {
            Ok(entries) => {
                local.set_listing(local_start.clone(), entries, false);
                local.history.visit(&local_start);
            }
            Err(err) => {
                local.path = local_start;
                local_status = err;
            }
        }

        let pending_hooks = test_hooks();
        let (conflict_automation, conflict_automation_error) = conflict_automation();
        let focus = cx.focus_handle();

        let fixture_dir = fixture::start_dir().map(|dir| start_remote.clone().unwrap_or(dir));
        let fixture = fixture_dir.is_some();
        let (phase, status) = if fixture {
            // 夹具：不连服务器，下面直接铺上假目录。
            (Phase::Ready, String::new())
        } else {
            match runtime() {
                Ok(runtime) => {
                    let sender = sender.clone();
                    runtime.spawn(async move {
                        let _ = sender.send(load_first(host_id, factory, start_remote).await);
                    });
                    (Phase::Connecting, format!("正在连接 {host_label}"))
                }
                Err(err) => (Phase::Failed, format!("连接失败：{err}")),
            }
        };

        let mut this = Self {
            view_id: NEXT_VIEW.fetch_add(1, Ordering::Relaxed),
            host_id,
            host_label,
            fixture,
            phase,
            status,
            status_is_error: false,
            remote,
            local,
            focused_side: Side::Remote,
            dialog: None,
            install_transfers: install_state::Registration::new(install_state::WorkKind::Transfers),
            transfers: Vec::new(),
            queue_open: true,
            deleting: None,
            next_delete: 1,
            chmod_running: None,
            close_prompt: false,
            close_confirmed: false,
            ime_key,
            dock: false,
            local_hidden: false,
            resolver: None,
            conflict_automation,
            conflicts: VecDeque::new(),
            batch_scope: None,
            listing: ListingGate::default(),
            conflict_name_input,
            engine: None,
            endpoints: None,
            pending_hooks,
            pause_hook_ms: pause_hook_ms(),
            pause_hook_deadline: None,
            pause_hook_resuming: false,
            pause_hook_ids: Vec::new(),
            pending_ops: VecDeque::new(),
            events,
            sender,
            factory: None,
            channel: None,
            busy: None,
            provider: None,
            shell_kind: Arc::new(std::sync::Mutex::new(None)),
            paste_retry: None,
            follow: false,
            last_paste: None,
            orphan_reports: std::collections::HashMap::new(),
            chmod_cancel: None,
            archive_caps: ArchiveCapsState::Idle,
            moving: None,
            next_move: 1,
            move_browse_seq: 0,
            next_op: 1,
            next_calc: 1,
            focus,
            dialog_focus: cx.focus_handle(),
            _subscriptions: Vec::new(),
            _tick: Task::ready(()),
        };
        this._subscriptions = this.build_subscriptions(window, cx);
        if !local_status.is_empty() {
            this.status = format!("{} · 本地：{local_status}", this.status);
        }
        // 写错了就说出来：这次仍按界面处理，免得以为钩子生效了、其实在等人点。
        if let Some(message) = conflict_automation_error {
            this.status = format!("{} · {message}", this.status);
        }
        this._tick = this.spawn_tick(window, cx);
        if let Some(dir) = fixture_dir {
            let rows = fixture::entries(&dir);
            this.set_remote_listing(dir, rows, NavMode::Visit, None);
        }
        this
    }

    /// 和窗口绑着的订阅：输入法（焦点在文件列表这类英文区时切到英文，离开时恢复）、输入框重画、
    /// 过滤框文字变了列表跟着过滤。换窗口（见 [`Self::rebind_window`]）时整套重建。
    fn build_subscriptions(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Subscription> {
        let mut subscriptions = Vec::new();
        for handle in [
            self.focus.clone(),
            self.remote.focus.clone(),
            self.local.focus.clone(),
        ] {
            let key = self.ime_key.clone();
            subscriptions.push(cx.on_focus(&handle, window, move |_this, _window, _cx| {
                crate::ime::apply(&key, true);
            }));
            let key = self.ime_key.clone();
            subscriptions.push(cx.on_blur(&handle, window, move |_this, _window, _cx| {
                crate::ime::apply(&key, false);
            }));
        }
        // 输入框是自己画出来的元素：它的状态（文字、光标、焦点）一变，文件视图要跟着重画。
        for input in [
            &self.remote.filter_input,
            &self.local.filter_input,
            &self.conflict_name_input,
        ] {
            subscriptions.push(cx.observe(input, |_this, _input, cx| cx.notify()));
        }
        // 过滤框里的文字一变，列表跟着过滤。
        for side in [Side::Remote, Side::Local] {
            let input = self.pane(side).filter_input.clone();
            subscriptions.push(cx.subscribe(&input, move |this, input, event, cx| {
                if matches!(event, InputEvent::Change) {
                    let text = input.read(cx).value().to_string();
                    let pane = this.pane_mut(side);
                    if pane.filter.text != text {
                        pane.filter.text = text;
                        pane.rebuild();
                        cx.notify();
                    }
                }
            }));
        }
        subscriptions
    }

    /// 100ms 轮询一次后台事件；窗口没了 `update_in` 会报错，任务自己退出。
    fn spawn_tick(&self, window: &mut Window, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                let alive = this.update_in(cx, |this, window, cx| this.poll_events(window, cx));
                if alive.is_err() {
                    break;
                }
            }
        })
    }

    /// 同一个视图换到另一个窗口里显示（弹成独立文件窗口、合并回主窗口）：连接、引擎、传输队列、
    /// 两栏的目录 / 选择 / 排序、后台任务都原样留着，只把和旧窗口绑着的东西重建——输入框、输入法订阅、
    /// 轮询任务。打开着的对话框和路径编辑框是旧窗口里建的，关掉（它们的状态不影响任何进行中的任务）。
    pub fn rebind_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let n = NEXT_SFTP.fetch_add(1, Ordering::Relaxed);
        // 旧窗口名在英文区名单里的登记要撤掉，否则输入法可能一直停在英文。
        crate::ime::window_destroyed(&self.ime_key);
        self.ime_key = format!("sftp-embedded-{n}");
        for side in [Side::Remote, Side::Local] {
            let text = self.pane(side).filter.text.clone();
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("过滤"));
            input.update(cx, |state, cx| state.set_value(text, window, cx));
            let pane = self.pane_mut(side);
            pane.filter_input = input;
            pane.path_edit = None;
        }
        self.conflict_name_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("新名字（留空 = 自动起名）"));
        self.prefill_conflict_name(window, cx);
        // 换了窗口，旧窗口里的同名询问没了：停在询问上的移动不会再有人回答，放弃它。
        if matches!(self.dialog, Some(Dialog::MoveConflict(_))) {
            self.moving = None;
        }
        self.dialog = None;
        self._subscriptions = self.build_subscriptions(window, cx);
        self._tick = self.spawn_tick(window, cx);
        cx.notify();
    }

    /// 键盘焦点交给远端栏的列表（主窗口把这个视图推到前台时用）。
    pub fn focus_handle(&self) -> FocusHandle {
        self.remote.focus.clone()
    }

    fn pane(&self, side: Side) -> &Pane {
        match side {
            Side::Remote => &self.remote,
            Side::Local => &self.local,
        }
    }

    fn pane_mut(&mut self, side: Side) -> &mut Pane {
        match side {
            Side::Remote => &mut self.remote,
            Side::Local => &mut self.local,
        }
    }

    /// 状态栏上放一句普通的话。
    fn say(&mut self, text: impl Into<String>) {
        self.status = text.into();
        self.status_is_error = false;
    }

    /// 状态栏上放一句出错 / 警告的话（警示色）。
    fn warn(&mut self, text: impl Into<String>) {
        self.status = text.into();
        self.status_is_error = true;
    }

    /// 窗口关闭：没传完的先取消（临时文件会被清掉），再丢掉通道和引擎；
    /// 连接本身留在全局池里。
    fn close(&mut self) {
        // 递归删除跟着窗口停：任务持有通道，不叫停会一直删到底。
        if let Some(job) = &self.deleting {
            job.cancel.cancel();
        }
        // 压缩 / 解压 / 属性里的计算也跟着停。
        if let Some(busy) = &self.busy {
            busy.cancel.store(true, Ordering::SeqCst);
        }
        if let Some(cancel) = &self.chmod_cancel {
            cancel.store(true, Ordering::SeqCst);
        }
        self.cancel_property_calcs();
        self.close_prompt = false;
        self.dialog = None;
        self.moving = None;
        // 输入法登记跟着窗口走；真正关窗口才注销（用户在确认条上选"继续传输"时不会走到这里）。
        crate::ime::window_destroyed(&self.ime_key);
        if let Some(engine) = &self.engine {
            for row in &self.transfers {
                if !row.state.is_terminal() {
                    engine.cancel(row.id);
                }
            }
        }
        // 还在等回答的冲突按跳过收尾：引擎的后台任务不会一直挂在 `decide()` 上。
        if let Some(resolver) = self.resolver.take() {
            resolver.shutdown();
        }
        self.conflicts.clear();
        self.engine = None;
        self.endpoints = None;
        self.provider = None;
        self.paste_retry = None;
        self.channel = None;
        self.factory = None;
        // 窗口关了，暂停钩子也不再需要（它只是自动测试用的）。
        self.pause_hook_deadline = None;
        self.pause_hook_ids.clear();
        self.remote.raw.clear();
        self.remote.rows.clear();
        self.local.raw.clear();
        self.local.rows.clear();
        // 窗口关了，这个窗口的传输不再有未结束的；安装前检查不该一直拦着更新。
        self.install_transfers.set(0);
    }

    /// 还没结束的传输数 + 正在跑的删除 / 压缩 / 解压：关窗口会把它们都取消，要先问。
    fn unfinished_work(&self) -> usize {
        self.transfers
            .iter()
            .filter(|row| !row.state.is_terminal())
            .count()
            + usize::from(self.deleting.is_some())
            // 粘贴不算：关窗口会叫停服务器端复制，但它不像压缩 / 解压那样要先问（同 0.2.14）。
            + usize::from(
                self.busy
                    .as_ref()
                    .is_some_and(|busy| busy.kind != archive_ui::BusyKind::Paste),
            )
    }

    /// 窗口关闭按钮：没有未完成的传输 / 删除就直接关；有就显示确认条，窗口留着。
    fn should_close(&mut self, cx: &mut Context<Self>) -> bool {
        if close_decision(self.close_confirmed, self.unfinished_work()) == CloseDecision::Close {
            self.close();
            return true;
        }
        self.close_prompt = true;
        let text = format!(
            "还有 {} 项传输 / 删除没完成，关闭会取消它们",
            self.unfinished_work()
        );
        self.warn(text);
        cx.notify();
        false
    }

    /// 取后台事件；有变化就重绘。没有事件时只醒来一次。
    fn poll_events(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut changed = false;
        while let Ok(event) = self.events.try_recv() {
            self.apply(event, window, cx);
            changed = true;
        }
        // 暂停钩子挂在同一条 100ms 心跳上：不用再起一个定时任务。
        if self.tick_pause_hook(cx) {
            changed = true;
        }
        // 压缩 / 解压的提示条每秒要跳一下用时。
        if self.busy.is_some() {
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    fn apply(&mut self, event: SftpEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            SftpEvent::Ready {
                factory,
                channel,
                cwd,
                entries,
            } => {
                self.phase = Phase::Ready;
                self.factory = Some(factory);
                self.channel = Some(channel);
                self.set_remote_listing(cwd, entries, NavMode::Visit, None);
                self.start_engine();
                self.arm_pause_hook();
                self.run_hooks(cx);
            }
            SftpEvent::Listed {
                seq,
                path,
                entries,
                mode,
                notice,
            } => {
                // 不是最新一次请求的结果：用户已经去了别处，别把旧目录盖回来。
                if !self.listing.is_latest(seq) {
                    return;
                }
                self.phase = Phase::Ready;
                self.set_remote_listing(path, entries, mode, notice);
            }
            SftpEvent::ConnectFailed(message) => {
                self.phase = Phase::Failed;
                self.warn(format!("连接失败：{message}"));
            }
            // 换目录失败不动旧列表：用户还在原来的目录里。
            SftpEvent::ListFailed {
                seq,
                path,
                mode,
                message,
            } => {
                if self.listing.is_latest(seq) {
                    self.remote.loading = false;
                    // 后退 / 前进去不了的目录从历史里拿掉，免得一直撞墙。
                    if mode == NavMode::History {
                        self.remote.history.forget(&path);
                    }
                    self.warn(message);
                }
            }
            SftpEvent::Transfer(event) => {
                let report = match event {
                    TransferEvent::Progress(report) | TransferEvent::Finished(report) => report,
                };
                // 上传完成：远端列表里要马上看到新文件（下载完成刷新本地栏，见 `apply_report`）。
                let finished = matches!(report.state, TransferState::Done);
                let uploaded = report.direction == Direction::Upload && finished;
                let copied = report.direction == Direction::RemoteCopy && finished;
                if !self.transfers.iter().any(|row| row.id == report.id) {
                    // 行还没建（粘贴的任务是后台排进引擎的）：先记着，行建好时补上。
                    if self.orphan_reports.len() < 256 {
                        self.orphan_reports.insert(report.id, report);
                    }
                    return;
                }
                self.apply_report(report);
                if uploaded || copied {
                    let notice = self.status.clone();
                    self.refresh_with_notice(Side::Remote, notice, cx);
                }
            }
            SftpEvent::Conflict { id, info } => {
                // 自动化钩子有值时 resolver 自己回答，不会送这个事件；走到这里是等人点。
                let first = self.conflicts.is_empty();
                self.conflicts.push_back(ConflictPrompt { id, info });
                if first {
                    self.prefill_conflict_name(window, cx);
                }
                self.warn("等待处理同名文件");
            }
            SftpEvent::DeleteProgress {
                id,
                deleted,
                current,
            } => {
                if let Some(job) = self.deleting.as_mut().filter(|job| job.id == id) {
                    job.deleted = deleted;
                    job.current = current;
                    let verb = if job.cancelling {
                        "正在取消删除"
                    } else {
                        "正在删除"
                    };
                    let text = format!("{verb} {}：已删 {deleted} 项", job.name);
                    self.say(text);
                }
            }
            SftpEvent::DeleteDone { id, outcome } => {
                let Some(job) = self.deleting.take_if(|job| job.id == id) else {
                    return;
                };
                let message = delete_outcome_text(&job.name, &outcome);
                if matches!(outcome, DeleteOutcome::Done(_)) {
                    self.say(message.clone());
                } else {
                    self.warn(message.clone());
                }
                // 不管成功还是中途取消，目录里的东西都变了：刷新，并把结果留在刷新后的状态栏里。
                self.refresh_with_notice(Side::Remote, message, cx);
                // 钩子排队的操作：这一个结束了，才轮到下一个。
                self.run_next_hook_op(cx);
            }
            SftpEvent::OpDone {
                ok,
                message,
                refresh,
            } => {
                if ok {
                    self.say(message.clone());
                } else {
                    self.warn(message.clone());
                }
                self.chmod_running = None;
                if let Some(side) = refresh {
                    // 要刷新的（成功，或者做了一半）：刷新回来时把这句话一起显示，否则 100ms 后
                    // 就被"N 项"顶掉了。
                    self.refresh_with_notice(side, message, cx);
                }
                // 钩子排队的操作：这一个结束了，才轮到下一个。
                self.run_next_hook_op(cx);
            }
            SftpEvent::RenameExists { from, to, name } => {
                self.ask_overwrite_rename(from, to, name, window, cx);
            }
            SftpEvent::ChmodProgress { changed } => {
                if self.chmod_running.is_some() {
                    self.chmod_running = Some(changed);
                    self.say(format!("正在递归改权限…已改 {changed} 项"));
                }
            }
            SftpEvent::SizeProgress {
                request,
                phase,
                totals,
                error,
            } => self.apply_size_progress(request, phase, totals, error),
            SftpEvent::ArchiveCaps(caps) => {
                self.archive_caps = match caps {
                    Some(caps) => ArchiveCapsState::Ready(caps),
                    // 没探测成：不缓存，下次右键再试。
                    None => ArchiveCapsState::Idle,
                };
            }
            SftpEvent::ArchiveProgress { op, bytes } => {
                if let Some(busy) = self.busy.as_mut().filter(|busy| busy.op == op) {
                    busy.bytes = bytes;
                }
            }
            SftpEvent::CompressDone { op, result } => self.finish_compress(op, result, cx),
            SftpEvent::ExtractPlanned { op, target, result } => {
                self.finish_extract_plan(op, target, result, window, cx)
            }
            SftpEvent::PasteProgress { op, files_done } => {
                if let Some(busy) = self.busy.as_mut().filter(|busy| busy.op == op) {
                    busy.files = Some(files_done);
                }
            }
            SftpEvent::PasteDone { op, result } => self.finish_paste(op, result, cx),
            SftpEvent::ExtractDone { op, ask, result } => self.finish_extract(op, ask, result, cx),
            SftpEvent::MoveBrowsed {
                request,
                dir,
                result,
            } => self.apply_move_browsed(request, dir, result, window, cx),
            SftpEvent::MoveFolderMade { dir, path, result } => {
                self.apply_move_folder_made(dir, path, result, cx)
            }
            SftpEvent::MoveStep { job, result } => self.apply_move_step(job, result, window, cx),
        }
        cx.notify();
    }

    /// 排一个远端写操作（任务挂在 runtime 上，界面线程只等结果）。
    fn run_remote_op<F>(&mut self, op: F, refresh: Option<Side>)
    where
        F: Future<Output = Result<String, String>> + Send + 'static,
    {
        match runtime() {
            Ok(runtime) => {
                let sender = self.sender.clone();
                runtime.spawn(async move {
                    let event = match op.await {
                        Ok(message) => SftpEvent::OpDone {
                            ok: true,
                            message,
                            refresh,
                        },
                        Err(message) => SftpEvent::OpDone {
                            ok: false,
                            message,
                            refresh: None,
                        },
                    };
                    let _ = sender.send(event);
                });
            }
            Err(err) => self.warn(err),
        }
    }

    /// 浏览用的这条通道；没连上时给统一说法。
    fn connected_channel(&self) -> Result<Arc<SftpChannel>, String> {
        self.channel
            .clone()
            .ok_or_else(|| "还没连上，稍等一下".to_owned())
    }
}

impl Drop for SftpDemo {
    fn drop(&mut self) {
        // 窗口实体没了：别让安装前检查一直记着这个窗口的传输（只撤自己这一条登记）。
        self.install_transfers.set(0);
    }
}

/// 首屏：连接（或复用池里的连接）→ 开 SFTP 通道 → 真实家目录 → 列目录。
async fn load_first(
    host_id: i64,
    factory: Option<Arc<SshSessionFactory>>,
    start_remote: Option<String>,
) -> SftpEvent {
    let result = async {
        let factory = match factory {
            Some(factory) => factory,
            None => factory_for_host(host_id).await?,
        };
        let connection = factory.connection();
        let channel = SftpChannel::open(&connection)
            .await
            .map_err(|err| err.to_string())?;
        let (cwd, entries) = first_listing(&channel, start_remote.as_deref()).await?;
        Ok::<_, String>((factory, channel, cwd, entries))
    }
    .await;
    match result {
        Ok((factory, channel, cwd, entries)) => SftpEvent::Ready {
            factory,
            channel: Arc::new(channel),
            cwd,
            entries,
        },
        Err(message) => SftpEvent::ConnectFailed(message),
    }
}

/// 第一屏的目录：指定了起始目录（复制标签）并且读得出来就用它，否则是登录后的家目录。
/// 目录在复制之后被删 / 改了权限也不该让新标签整个失败，所以读不出来就退回家目录。
async fn first_listing(
    channel: &SftpChannel,
    wanted: Option<&str>,
) -> Result<(String, Vec<DirEntry>), String> {
    if let Some(dir) = wanted.filter(|dir| !dir.trim().is_empty())
        && let Ok(entries) = channel.read_dir(dir).await
    {
        return Ok((dir.to_owned(), entries));
    }
    let cwd = channel
        .realpath(".")
        .await
        .unwrap_or_else(|_| "/".to_owned());
    let entries = channel
        .read_dir(&cwd)
        .await
        .map_err(|err| err.to_string())?;
    Ok((cwd, entries))
}

/// 从主机库或环境变量打开一个 SFTP 浏览窗口。
pub fn open_sftp_window(cx: &mut App, host_id: i64) {
    open_sftp_window_with_factory(cx, host_id, None);
}

/// 从终端窗格交接：直接复用该窗格已认证的连接，不再走池子查找/握手。
pub fn open_sftp_window_with_factory(
    cx: &mut App,
    host_id: i64,
    factory: Option<Arc<SshSessionFactory>>,
) {
    let n = NEXT_SFTP.fetch_add(1, Ordering::Relaxed);
    let ime_key = format!("sftp-window-{n}");
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(
            size(::gpui::px(1100.), ::gpui::px(760.)),
            cx,
        )),
        ..Default::default()
    };
    let _ = cx.open_window(options, move |window, cx| {
        let view = cx.new(|cx| SftpDemo::new(window, cx, host_id, ime_key, factory, None));
        window.set_window_title(&format!("SFTP #{host_id}"));
        let close_view = view.clone();
        // 有未完成的传输 / 删除时这里返回 false，窗口里出现确认条；确认之后由按钮自己关窗口。
        window.on_window_should_close(cx, move |_window, cx| {
            close_view.update(cx, |this, cx| this.should_close(cx))
        });
        window.focus(&view.read(cx).focus_handle(), cx);
        view
    });
}

/// 嵌在主窗口里的用法（停靠面板和 SFTP 主视图标签共用同一个实体）。
impl SftpDemo {
    /// 在主窗口里开一个文件视图：`dock` 为真是停靠面板的布局（远端在上、本地在下），
    /// 否则是标签里的双栏布局。`factory` 是终端标签已认证的连接，传了就不再握手。
    pub fn new_embedded(
        window: &mut Window,
        cx: &mut Context<Self>,
        host_id: i64,
        factory: Option<Arc<SshSessionFactory>>,
        dock: bool,
        local_hidden: bool,
    ) -> Self {
        let n = NEXT_SFTP.fetch_add(1, Ordering::Relaxed);
        let mut this = Self::new(
            window,
            cx,
            host_id,
            format!("sftp-embedded-{n}"),
            factory,
            None,
        );
        this.dock = dock;
        this.local_hidden = local_hidden;
        this
    }

    /// 停靠面板 → 标签（或反过来）：同一个实体换个布局，连接和两栏目录原样带过去。
    pub fn set_dock_layout(&mut self, dock: bool, cx: &mut Context<Self>) {
        self.dock = dock;
        cx.notify();
    }

    pub fn set_local_hidden(&mut self, hidden: bool, cx: &mut Context<Self>) {
        self.local_hidden = hidden;
        cx.notify();
    }

    pub fn local_hidden(&self) -> bool {
        self.local_hidden
    }

    /// 还没结束的传输数 + 正在跑的删除：关面板 / 退出前要问（和窗口关闭用同一个口径）。
    pub fn unfinished(&self) -> usize {
        self.unfinished_work()
    }

    pub fn is_ready(&self) -> bool {
        self.phase == Phase::Ready
    }

    pub fn is_failed(&self) -> bool {
        self.phase == Phase::Failed
    }

    pub fn remote_cwd(&self) -> &str {
        &self.remote.path
    }

    pub fn remote_names(&self) -> Vec<String> {
        self.remote
            .raw
            .iter()
            .map(|entry| entry.name.clone())
            .collect()
    }

    /// 面板 / 标签被关掉：取消没传完的任务、停掉递归删除、放掉通道（连接回到池里给终端用）。
    pub fn shutdown_embedded(&mut self) {
        self.close();
    }

    /// 驱动脚本用：上传一个本地文件到远端当前目录（和菜单“上传到远端”同一条 `start_upload`）。
    pub fn script_upload(&mut self, local: &str, cx: &mut Context<Self>) -> Result<(), String> {
        let result = self.in_new_batch(|this| this.start_upload(local));
        cx.notify();
        result
    }

    /// 驱动脚本用：把远端当前目录里的某个文件下载到本地栏的当前目录。
    pub fn script_download(&mut self, name: &str, cx: &mut Context<Self>) -> Result<(), String> {
        let remote = join(&self.remote.path, name);
        let dir = Path::new(&self.local.path).to_path_buf();
        let result = self.in_new_batch(|this| this.start_download_to(&dir, &remote));
        cx.notify();
        result
    }

    pub fn script_local_dir(&mut self, dir: &Path, cx: &mut Context<Self>) {
        self.navigate(Side::Local, &dir.display().to_string(), NavMode::Visit, cx);
    }

    pub fn script_remote_dir(&mut self, path: &str, cx: &mut Context<Self>) {
        self.navigate(Side::Remote, path, NavMode::Visit, cx);
    }

    /// (已完成, 失败, 还没结束)。
    pub fn transfer_counts(&self) -> (usize, usize, usize) {
        let done = self
            .transfers
            .iter()
            .filter(|row| matches!(row.state, TransferState::Done))
            .count();
        let failed = self
            .transfers
            .iter()
            .filter(|row| matches!(row.state, TransferState::Failed(_)))
            .count();
        (done, failed, self.unfinished_work())
    }
}

#[cfg(test)]
mod move_e2e_tests;
#[cfg(test)]
mod paste_tests;
#[cfg(test)]
mod tests;
