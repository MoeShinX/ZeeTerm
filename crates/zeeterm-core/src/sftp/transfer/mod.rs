//! 传输引擎（M3-1 第 3 节）：队列、并发、进度、冲突、暂停续传、失败重试。
//!
//! 边界情况的清单参考 Netcatty `electron/bridges/transferBridge.cjs` 与 `sftpBridge.cjs`
//! （暂存文件、续传对账、暂停/续传的竞争、冲突处理、本地与远端的名字差异），实现是
//! 独立写的 Rust，不是代码移植。
//!
//! 结构：一个 [`TransferEngine`] 管着一批任务，每个任务展开成文件清单后**逐文件**
//! 传（同时进行的文件数受 `concurrency` 限制，默认 3）。单文件内部按批发多个读请求
//! （流水线，见 [`file::copy_file`]）。
//!
//! 通道预算（M3-1 第 1 节）：传输**不开在浏览用的那条 SFTP 通道上**，而是问
//! [`TransferChannelProvider`] 要一条；服务器拒绝开通道（`MaxSessions` 到顶）时，
//! provider 会改走**一条专用连接**（[`SftpReconnect`]），终端和别的功能都不受影响。
//!
//! 进度与状态通过 [`TransferEvent`] 往外发（有节流，不是每块都发）；暂停/取消是对
//! 任务的 [`ControlFlag`] 置位，传输循环在批之间检查。

pub mod backend;
pub mod endpoint;
pub mod file;
pub mod lock;
pub mod memory;
pub mod plan;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, Notify, Semaphore};

use crate::error::{reason_keys, Reason};
use crate::sftp::error::SftpError;
use crate::ssh::BoxFuture;

use backend::TransferEndpoints;
use endpoint::FileEndpoint;
#[cfg(test)]
use file::copy_file;
use file::{copy_file_sized, tails_match, ControlFlag, CopyOutcome, CopyTuning};

/// 任务编号。
pub type TransferId = u64;

/// 传输方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Upload,
    Download,
    /// 远端 → 远端（复制粘贴的中转路径：数据经过本机，两端都是 SFTP；源可以在另一台主机上）。
    RemoteCopy,
    /// 本地 → 本地（复制粘贴）。
    LocalCopy,
}

/// 一个传输任务（单文件或文件夹）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferJob {
    pub direction: Direction,
    /// 上传：本地源路径；下载：远端源路径。
    pub source: String,
    /// 上传：远端目标路径；下载：本地目标路径。
    pub target: String,
    /// 目录任务要不要递归展开。
    pub recursive: bool,
    /// 只有 `RemoteCopy` 用：源在另一台主机时，那台主机的数据面在
    /// [`backend::SftpEndpoints::register_peer`] 里登记的名字；`None` = 源和目标在同一台主机。
    pub peer: Option<String>,
}

/// 任务状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferState {
    Queued,
    Planning,
    Running,
    /// 用户按了暂停（临时文件留着，可以续传）。
    Paused,
    /// 断线了，等重连之后自动接着传。
    WaitingReconnect,
    /// 目标已存在，正在等用户回答怎么处理（界面上显示「等待处理同名文件」，而不是一直
    /// 「准备中」让人以为卡住了）。回答之后回到 `Running`；超时 / 关掉按跳过。
    WaitingConflict,
    /// 传完了。
    Done,
    /// 失败（带原因）。
    Failed(String),
    /// 用户取消。
    Cancelled,
    /// 按冲突策略跳过（目标已存在且用户选了"跳过"）。
    Skipped,
}

impl TransferState {
    /// 终态（不会再变）。
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TransferState::Done
                | TransferState::Failed(_)
                | TransferState::Cancelled
                | TransferState::Skipped
        )
    }
}

/// 进度报告（`TransferEvent` 里的载荷）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressReport {
    pub id: TransferId,
    pub direction: Direction,
    pub state: TransferState,
    /// 队列里的文件总数（展开之后才知道；展开前是 0）。
    pub total_files: u64,
    pub done_files: u64,
    pub total_bytes: u64,
    pub done_bytes: u64,
    /// 瞬时速度（字节/秒）。
    pub speed_bps: u64,
    /// 剩余秒数（速度未知时是 `None`）。
    pub eta_secs: Option<u64>,
    /// 正在传的文件。
    pub current: Option<String>,
    /// 要不要告诉用户的说明（改过的名字、跳过的链接、重连次数…）。
    pub notes: Vec<Reason>,
}

/// 引擎往外发的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferEvent {
    /// 进度更新（节流后有变化才发）。
    Progress(ProgressReport),
    /// 任务结束（终态；`state` 里带原因）。
    Finished(ProgressReport),
}

/// 目标已存在时的处理方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictAction {
    /// 覆盖。
    Overwrite,
    /// 跳过这个文件。
    Skip,
    /// 换个名字（`name (2).ext`）。
    Rename,
    /// 续传（只在目标比源小的时候可选）。
    Resume,
}

/// 冲突信息（交给上层决定）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictInfo {
    pub job: TransferId,
    pub direction: Direction,
    pub source: String,
    pub target: String,
    pub source_size: Option<u64>,
    pub target_size: Option<u64>,
    /// 源 / 目标的修改时间（冲突对话框要显示；拿不到是 None）。
    pub source_mtime: Option<u32>,
    pub target_mtime: Option<u32>,
    /// 能不能续传（目标比源小）。
    pub can_resume: bool,
    /// 对话框里预填的新名字（完整路径）：第一个没被占用的自动名字；检查不了或者试到上限
    /// 就退回固定的 `名字 (2)` / `名字 - 副本`，不能因为检查失败卡住提问。
    pub suggested_target: String,
    /// 这次提问属于哪个批次（同一次提交 / 粘贴）。界面用它把同批次还在排队的提问一起回答；
    /// `None` = 不属于任何批次（单独提交的任务、重试过的任务）。
    pub batch: Option<u64>,
}

/// 冲突决定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictDecision {
    pub action: ConflictAction,
    /// 后面的文件都按这个来（界面上的"对后续文件都这样处理"）。
    pub apply_to_all: bool,
    /// 选"重命名"时用户改过的目标名（None = 用自动生成的 `名字 (2)`）。
    /// 只对这一次生效：勾了"对后续都这样"时，后面的文件仍用自动名字。
    pub rename_to: Option<String>,
}

impl ConflictDecision {
    /// 用户没改对话框预填的建议名，就不算手动名字。
    ///
    /// 界面把建议名预填进输入框，选"重命名"时总是把输入框的值送回来；如果直接当手动名，
    /// 提问到回答之间这个名字被别人占了，这个文件就会因为「新名字已存在」失败。
    /// 和建议名一样（只比文件名）= 用户没动过 = 走自动起名，被占就往后找；只有改过才是手动名。
    #[must_use]
    pub fn without_suggested_name(mut self, suggested_target: &str) -> Self {
        let suggested = suggested_target
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(suggested_target);
        if self
            .rename_to
            .as_deref()
            .is_some_and(|name| name.trim() == suggested)
        {
            self.rename_to = None;
        }
        self
    }
}

/// 同一次提交（一次 `sftp_transfer_submit`、一次粘贴）里所有任务共享的"全部应用"选择。
///
/// 引擎按任务各记一份 `conflict_all`，一次传 10 个同名文件就是 10 个任务，第一个对话框里勾了
/// "都这样处理"只管它自己。批次把这个选择放到任务之上：同批次里任何一个任务勾了，后面撞上
/// 同名的任务直接按它处理，不再问。
#[derive(Debug)]
pub struct ConflictBatch {
    id: u64,
    action: Mutex<Option<ConflictAction>>,
}

impl ConflictBatch {
    fn new(id: u64) -> Self {
        Self {
            id,
            action: Mutex::new(None),
        }
    }

    /// 批次号（在同一个引擎里唯一；提问事件带着它）。
    pub fn id(&self) -> u64 {
        self.id
    }

    /// 这个批次已经记住的"全部应用"动作。
    pub fn action(&self) -> Option<ConflictAction> {
        *self
            .action
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 记下"全部应用"的动作（后写的覆盖先写的）。
    pub fn remember(&self, action: ConflictAction) {
        *self
            .action
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(action);
    }
}

/// 冲突回调：界面弹窗实现它；测试里用固定策略。
pub trait ConflictResolver: Send + Sync {
    fn decide<'a>(&'a self, info: ConflictInfo) -> BoxFuture<'a, ConflictDecision>;
}

/// 默认策略：目标已存在就覆盖（没有界面时用；测试里也用它）。
pub struct OverwriteAll;

impl ConflictResolver for OverwriteAll {
    fn decide<'a>(&'a self, _info: ConflictInfo) -> BoxFuture<'a, ConflictDecision> {
        Box::pin(async {
            ConflictDecision {
                action: ConflictAction::Overwrite,
                apply_to_all: true,
                rename_to: None,
            }
        })
    }
}

/// 全跳过（测试用）。
pub struct SkipAll;

impl ConflictResolver for SkipAll {
    fn decide<'a>(&'a self, _info: ConflictInfo) -> BoxFuture<'a, ConflictDecision> {
        Box::pin(async {
            ConflictDecision {
                action: ConflictAction::Skip,
                apply_to_all: true,
                rename_to: None,
            }
        })
    }
}

/// 传输参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferOptions {
    /// 同时进行的文件数（默认 3）。
    pub concurrency: usize,
    /// 是否开流水线（单个大文件同时发多个读请求）。
    pub pipeline: bool,
    /// 流水线的在途请求数。
    pub in_flight: usize,
    /// 每块字节数（`None` = 按服务器 limits）。
    pub chunk_len: Option<u32>,
    /// 保留修改时间。
    pub preserve_mtime: bool,
    /// 保留权限。
    pub preserve_permissions: bool,
    /// 断线重试次数上限。
    pub max_retries: u32,
    /// 进度的最小发送间隔（节流）。
    pub progress_interval: Duration,
    /// 续传对账的窗口大小。
    pub tail_window: u64,
}

impl Default for TransferOptions {
    fn default() -> Self {
        Self {
            concurrency: 3,
            pipeline: true,
            in_flight: 8,
            chunk_len: None,
            preserve_mtime: false,
            preserve_permissions: false,
            max_retries: 3,
            progress_interval: Duration::from_millis(200),
            tail_window: file::TAIL_WINDOW,
        }
    }
}

/// 任务在引擎里的状态（进度 + 控制）。
struct JobState {
    id: TransferId,
    job: TransferJob,
    control: ControlFlag,
    /// 所属批次；重试时摘掉（重试要重新问，不受批次里旧的选择影响）。
    batch: Mutex<Option<Arc<ConflictBatch>>>,
    /// 用户暂停之后唤醒的开关。
    resume_signal: Notify,
    progress: Mutex<JobProgress>,
    state: Mutex<TransferState>,
    /// 同一任务同时只允许一轮 run_job。重试必须等上一轮把临时文件的收尾做完，
    /// 否则两个 run 会同时打开同一个 `.zssh-part`（Linux 上已复现过内容重复）。
    run_lock: tokio::sync::Mutex<()>,
    /// 每个目标的临时文件里**已确认的连续前缀**长度（目标路径 → 字节数）。
    /// 只在整批写都成功之后才往前走。续传点由它决定，而不是临时文件的长度：
    /// 一批并发写里失败时，后面的块可能已经落盘，长度大于前缀、中间有洞，
    /// 而这时连接多半也断了，截回前缀（或删掉）都会失败。
    confirmed: Mutex<HashMap<String, u64>>,
    /// 每个文件在本任务里的运行状态（键 = 计划里的目标路径）：实际目标、已计入进度的字节。
    /// 暂停后重进、取消清理、断线重试都读这一份，不各自在局部变量里记。
    files: Mutex<HashMap<String, FileRun>>,
}

/// 一个文件在任务里的运行状态（暂停 / 重试 / 取消之间不能丢）。
#[derive(Debug, Clone, Default)]
struct FileRun {
    /// 冲突里选了"改名"之后实际写入的目标；`None` = 还在用计划里的目标。
    /// 临时文件、续传位置、取消清理、提交都认它，不认计划里的原目标。
    actual_target: Option<String>,
    /// 这个文件已经计入任务 `done_bytes` 的字节数。重传（续传位置回退、从头来、重试）时先扣掉
    /// 它再按新起点计，所以同一段数据不会被累加两次，进度也不会超过总量。
    counted_bytes: u64,
    /// 这个文件的源路径（清理别的名字下的临时文件时要用它建端点）；开始传这个文件时记下。
    source: Option<String>,
    /// 目标端可能还留着本任务临时文件的那些目标路径（尝试过的实际目标）。
    ///
    /// 重试 / 重新提问之后实际目标可能变了（换了改名结果、改成覆盖或跳过），旧名字下的
    /// `<目标>.<任务标识>.zssh-part` 就没人用了，也没人会清：靠这份记录在换目标时删掉。
    /// 记录不随重试清空；文件传完（临时文件已改名为最终文件）或临时文件已删掉才去掉对应项。
    parts: Vec<String>,
}

impl JobState {
    /// 记下这个文件在 `target` 下可能有本任务的临时文件。
    fn note_part(&self, planned_target: &str, source: &str, target: &str) {
        let mut files = self
            .files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let run = files.entry(planned_target.to_owned()).or_default();
        run.source.get_or_insert_with(|| source.to_owned());
        if !run.parts.iter().any(|part| part == target) {
            run.parts.push(target.to_owned());
        }
    }

    /// 去掉一项记录（`target` 下的临时文件已经没了 / 已经提交成最终文件）。
    fn forget_part(&self, planned_target: &str, target: &str) {
        if let Some(run) = self
            .files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_mut(planned_target)
        {
            run.parts.retain(|part| part != target);
        }
    }

    /// 这个文件记着的临时文件目标里不是 `keep` 的那些，连同源路径一起返回。
    fn stale_parts(
        &self,
        planned_target: &str,
        keep: Option<&str>,
    ) -> (Option<String>, Vec<String>) {
        let files = self
            .files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match files.get(planned_target) {
            Some(run) => (
                run.source.clone(),
                run.parts
                    .iter()
                    .filter(|part| Some(part.as_str()) != keep)
                    .cloned()
                    .collect(),
            ),
            None => (None, Vec::new()),
        }
    }

    /// 重试：进度归零要清的（实际目标、已计入字节）清掉，但仍可能留着临时文件的文件保留它的记录。
    fn reset_for_retry(&self) {
        let mut files = self
            .files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        files.retain(|_, run| !run.parts.is_empty());
        for run in files.values_mut() {
            run.actual_target = None;
            run.counted_bytes = 0;
        }
    }

    fn actual_target(&self, planned_target: &str) -> Option<String> {
        self.files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(planned_target)
            .and_then(|run| run.actual_target.clone())
    }

    fn set_actual_target(&self, planned_target: &str, actual: String) {
        self.files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(planned_target.to_owned())
            .or_default()
            .actual_target = Some(actual);
    }

    /// 把这个文件已计入的字节数改成 `new`，返回改之前的值。
    fn replace_counted(&self, planned_target: &str, new: u64) -> u64 {
        let mut files = self
            .files
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let run = files.entry(planned_target.to_owned()).or_default();
        std::mem::replace(&mut run.counted_bytes, new)
    }
}

impl JobState {
    async fn wait_while_paused(&self, mut on_paused: impl FnMut() + Send) -> file::ControlState {
        loop {
            // 先登记再读状态，避免 resume / cancel 在读到 Paused 后发出的广播丢失。
            let notified = self.resume_signal.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.control.state() {
                file::ControlState::Paused => {
                    on_paused();
                    notified.await;
                }
                state => return state,
            }
        }
    }

    fn is_cancelled(&self) -> bool {
        self.control.state() == file::ControlState::Cancelled
    }

    /// 等到任务被取消（取消的广播和暂停共用 `resume_signal`；先登记再读状态，不丢通知）。
    async fn wait_cancelled(&self) {
        loop {
            let notified = self.resume_signal.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }

    fn confirmed_prefix(&self, target: &str) -> Option<u64> {
        self.confirmed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(target)
            .copied()
    }

    fn record_confirmed(&self, target: &str, len: u64) {
        self.confirmed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(target.to_owned(), len);
    }
}

/// 续传从哪里开始：临时文件实际长度和本任务记下的已确认前缀取较小者。
///
/// 没有记录（`None`）就是 0——不知道哪一段是连续写好的，宁可从头传。
/// 记录只在本进程内存里，而临时文件名里带着本进程引擎的随机标识（见 `owner_of`）：
/// 换了进程找不到旧的临时文件，所以记录和临时文件的有效期正好一致，不需要落盘。
fn resume_point(actual_len: u64, confirmed: Option<u64>) -> u64 {
    confirmed.map_or(0, |confirmed| actual_len.min(confirmed))
}

struct JobProgress {
    total_files: u64,
    done_files: u64,
    total_bytes: u64,
    done_bytes: u64,
    current: Option<String>,
    notes: Vec<Reason>,
    last_emit: Option<Instant>,
    /// 速度采样（最近一次：时刻 + 当时已传字节）。
    sample: Option<(Instant, u64)>,
    speed_bps: u64,
    /// "对后续文件都这样"记住的冲突策略。
    conflict_all: Option<ConflictAction>,
    /// 已经问过冲突的目标（按计划里的目标路径记）和用户当时的选择。**暂停再续传不重新问**：
    /// 暂停之前用户已经选过（覆盖 / 续传 / 改名），续传接着断点传就行；重新问一遍
    /// 的话，没人盯着就会按超时跳过，传了一半的文件被丢掉（M3-2 实测踩到）。
    /// 记的是选择本身而不只是"问过"：只有用户真的同意覆盖，提交时才允许替换目标（C05）。
    conflicted_targets: std::collections::HashMap<String, RememberedConflict>,
}

/// 一个目标上用户做过的冲突选择（暂停后重进同一个文件时沿用）。
///
/// 选了"改名"的不记在这里：改名后的实际目标在 [`FileRun::actual_target`] 里，只此一份。
#[derive(Debug, Clone, PartialEq, Eq)]
enum RememberedConflict {
    /// 同意覆盖（选了覆盖或续传）。
    Overwrite,
}

impl JobProgress {
    fn new() -> Self {
        Self {
            total_files: 0,
            done_files: 0,
            total_bytes: 0,
            done_bytes: 0,
            current: None,
            notes: Vec::new(),
            last_emit: None,
            sample: None,
            speed_bps: 0,
            conflict_all: None,
            conflicted_targets: Default::default(),
        }
    }
}

/// 任务号 / 批次号在整个进程里唯一，不按引擎各数各的。
///
/// 为什么：断线重建引擎后，界面里旧引擎留下的行（失败 / 已取消的）还在，如果新引擎又从 1
/// 开始发号，新任务的进度会按号落到旧行上，旧行的暂停 / 取消 / 重试也会命中新任务。
/// 号只存在于内存里（不落盘、不跨进程），所以进程内唯一就够了。
static NEXT_TRANSFER_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_BATCH_ID: AtomicU64 = AtomicU64::new(1);

/// 传输引擎。
pub struct TransferEngine {
    /// 数据面（生产是本地 ↔ 远端；单测是内存替身）。
    endpoints: Arc<dyn TransferEndpoints>,
    resolver: Arc<dyn ConflictResolver>,
    options: TransferOptions,
    events: mpsc::UnboundedSender<TransferEvent>,
    jobs: Mutex<HashMap<TransferId, Arc<JobState>>>,
    /// 这个引擎的标识：和任务号一起组成临时文件名里的任务标识（不同引擎、不同次启动不会撞）。
    nonce: String,
    /// 同时进行的文件数上限。
    permits: Arc<Semaphore>,
    /// 本地目录树的规划一次只做一个（先到先得）：规划放进了阻塞线程池，不排队的话
    /// 同一批提交的任务谁先规划完谁先传，传输顺序就不再是提交顺序了。
    local_plan_gate: tokio::sync::Mutex<()>,
    /// 自己（弱引用）：后台任务拿着它，引擎被丢掉时任务自然退出。
    this: std::sync::OnceLock<std::sync::Weak<TransferEngine>>,
}

impl TransferEngine {
    /// 建一个引擎；返回的接收端拿事件（进度、结束）。
    pub fn new(
        endpoints: Arc<dyn TransferEndpoints>,
        resolver: Arc<dyn ConflictResolver>,
        options: TransferOptions,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<TransferEvent>) {
        let (events, receiver) = mpsc::unbounded_channel();
        let this = std::sync::OnceLock::new();
        let engine = Arc::new_cyclic(|weak| {
            let _ = this.set(weak.clone());
            Self {
                endpoints,
                resolver,
                permits: Arc::new(Semaphore::new(options.concurrency.max(1))),
                local_plan_gate: tokio::sync::Mutex::new(()),
                options,
                events,
                jobs: Mutex::new(HashMap::new()),
                nonce: uuid::Uuid::new_v4().simple().to_string()[..8].to_owned(),
                this,
            }
        });
        (engine, receiver)
    }

    pub fn options(&self) -> &TransferOptions {
        &self.options
    }

    /// 开一个冲突批次：同一次提交 / 粘贴的所有任务用 [`Self::submit_in_batch`] 排进同一个批次，
    /// 共享"全部应用"的选择。
    pub fn new_batch(&self) -> Arc<ConflictBatch> {
        Arc::new(ConflictBatch::new(
            NEXT_BATCH_ID.fetch_add(1, Ordering::SeqCst),
        ))
    }

    /// 把一个任务排进队列（立即返回任务号；真正干活在后台）。不属于任何批次：
    /// "全部应用"只管这个任务自己。
    pub fn submit(&self, job: TransferJob) -> TransferId {
        self.submit_with(job, None)
    }

    /// 同 [`Self::submit`]，但任务属于一个批次。
    pub fn submit_in_batch(&self, job: TransferJob, batch: &Arc<ConflictBatch>) -> TransferId {
        self.submit_with(job, Some(Arc::clone(batch)))
    }

    fn submit_with(&self, job: TransferJob, batch: Option<Arc<ConflictBatch>>) -> TransferId {
        let id = NEXT_TRANSFER_ID.fetch_add(1, Ordering::SeqCst);
        let state = Arc::new(JobState {
            id,
            job: job.clone(),
            control: ControlFlag::new(),
            batch: Mutex::new(batch),
            resume_signal: Notify::new(),
            progress: Mutex::new(JobProgress::new()),
            state: Mutex::new(TransferState::Queued),
            run_lock: tokio::sync::Mutex::new(()),
            confirmed: Mutex::new(HashMap::new()),
            files: Mutex::new(HashMap::new()),
        });
        self.jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, Arc::clone(&state));
        self.emit(&state, true);

        // 后台任务拿 Weak：引擎被丢掉时任务自然结束（不会拽着引擎不放）。
        let weak = self
            .this
            .get()
            .cloned()
            .unwrap_or_else(std::sync::Weak::<TransferEngine>::new);
        tokio::spawn(async move {
            if let Some(engine) = weak.upgrade() {
                engine.run_job(state, false).await;
            }
        });
        id
    }

    /// 重试一个失败的任务（M3-2 第 1 节的"重试"命令）。
    ///
    /// 同一个任务号重跑：进度归零、重新展开；上次失败留下的 `.zssh-part` 先做尾部
    /// 对账，对得上的接着传，对不上的从头传。失败 / 取消 / 跳过的任务能重试；
    /// 跳过的任务重试时会重新问一次同名文件怎么处理（上次记住的选择已清掉）。
    pub fn retry(&self, id: TransferId) -> bool {
        let Some(state) = self.job(id) else {
            return false;
        };
        if !matches!(
            self.state_of(&state),
            TransferState::Failed(_) | TransferState::Cancelled | TransferState::Skipped
        ) {
            return false;
        }
        {
            let mut progress = self.lock_progress(&state);
            progress.total_files = 0;
            progress.done_files = 0;
            progress.total_bytes = 0;
            progress.done_bytes = 0;
            progress.current = None;
            progress.sample = None;
            progress.speed_bps = 0;
            progress.last_emit = None;
            // "对后续都这样"和已问过的目标只对一次任务生效；重试要重新问。
            progress.conflict_all = None;
            progress.conflicted_targets.clear();
        }
        // 进度归零了，每个文件"已计入的字节"和"改过的名字"也要清（重试重新问冲突）。
        // `confirmed`（续传前缀）不清：重试就是要接着上次的临时文件传。
        // 上次尝试留下的临时文件的记录也不清：重试选的目标变了，要靠它删掉旧名字下没人用的临时文件。
        state.reset_for_retry();
        // 同理，重试的任务脱离原来的批次：批次里旧的"全部应用"不再替它回答，它重新问，
        // 重试后勾的"全部应用"也只管它自己。
        *state
            .batch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        state.control.reset();
        {
            // set_state 对终态只读，重试要显式改回去。
            let mut current = state
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *current = TransferState::Queued;
        }
        self.emit(&state, true);
        let weak = self
            .this
            .get()
            .cloned()
            .unwrap_or_else(std::sync::Weak::<TransferEngine>::new);
        tokio::spawn(async move {
            if let Some(engine) = weak.upgrade() {
                engine.run_job(state, true).await;
            }
        });
        true
    }

    /// 清掉已经结束的任务（界面上的"清除已完成"）。返回清了几个。
    pub fn clear_finished(&self) -> usize {
        let mut jobs = self
            .jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let before = jobs.len();
        jobs.retain(|_, state| {
            !state
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_terminal()
        });
        before - jobs.len()
    }

    /// 所有任务的最新报告（界面初始化 / 诊断用），按任务号排序。
    pub fn reports(&self) -> Vec<ProgressReport> {
        let jobs = self
            .jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut ids: Vec<&TransferId> = jobs.keys().collect();
        ids.sort();
        ids.into_iter()
            .filter_map(|id| jobs.get(id).map(|state| self.report_of(state)))
            .collect()
    }

    /// 由 JobState 拼一份最新报告。
    fn report_of(&self, state: &Arc<JobState>) -> ProgressReport {
        let progress = self.lock_progress(state);
        ProgressReport {
            id: state.id,
            direction: state.job.direction,
            state: self.state_of(state),
            total_files: progress.total_files,
            done_files: progress.done_files,
            total_bytes: progress.total_bytes,
            done_bytes: progress.done_bytes,
            speed_bps: progress.speed_bps,
            eta_secs: None,
            current: progress.current.clone(),
            notes: progress.notes.clone(),
        }
    }

    async fn run_job(self: Arc<Self>, state: Arc<JobState>, prefer_resume: bool) {
        // 上一轮还没退出时（例如刚看到 Failed 事件就点了重试），这一轮在这里等。
        let _run = state.run_lock.lock().await;
        Arc::clone(&self)
            .run_job_locked(Arc::clone(&state), prefer_resume)
            .await;
        // 取消是终点：不管在哪一步取消的，任务记着的临时文件（含上一次失败尝试用别的名字留下的、
        // 还没轮到的文件的）都不会再有人用，统一在这里清。失败不清：留着给重试续传。
        if self.state_of(&state) == TransferState::Cancelled {
            self.reap_all_parts(&state).await;
        }
    }

    async fn run_job_locked(self: Arc<Self>, state: Arc<JobState>, prefer_resume: bool) {
        // 排队期间就被取消了：连规划都不用做（更不能建目录）。
        if state.is_cancelled() {
            self.finish_with_error(&state, SftpError::Cancelled);
            return;
        }
        self.set_state(&state, TransferState::Planning);
        self.emit(&state, true);

        // 展开任务（下载要读远端目录，走数据面）。取消一到就丢掉还在读目录的规划。
        let plan = match self.until_cancelled(&state, self.build_plan(&state)).await {
            Ok(plan) => plan,
            Err(err) => {
                self.finish_with_error(&state, err);
                return;
            }
        };
        // 同步的规划（本地树）中途没法打断，做完之后补一次检查，免得接着去建目录。
        if state.is_cancelled() {
            self.finish_with_error(&state, SftpError::Cancelled);
            return;
        }
        {
            let mut progress = self.lock_progress(&state);
            progress.total_files = plan.files.len() as u64;
            progress.total_bytes = plan.total_bytes();
            for note in &plan.notes {
                progress.notes.push(note.clone());
            }
        }
        self.emit(&state, true);

        // 先建目录（目标端）。
        if let Err(err) = self.create_dirs(&state, &plan).await {
            self.finish_with_error(&state, err);
            return;
        }

        self.set_state(&state, TransferState::Running);
        // 状态变成传输中要马上告诉界面：不然在第一条进度到来之前（含等同名文件的回答）
        // 界面一直停在「准备中」。
        self.emit(&state, true);
        let mut failed: Option<SftpError> = None;
        let mut completed_files = 0u64;
        let mut skipped_files = 0u64;
        // 用下标循环：**暂停之后要重跑同一个文件**（不是跳过去）。
        let mut index = 0usize;
        while index < plan.files.len() {
            let planned = plan.files[index].clone();
            // 暂停：等 resume；取消：收尾。
            if state
                .wait_while_paused(|| {
                    self.set_state(&state, TransferState::Paused);
                    self.emit(&state, true);
                })
                .await
                == file::ControlState::Cancelled
            {
                // 暂停中被取消：临时文件还留在目标端，也要清掉。
                self.discard_partial(&state, &planned).await;
                self.set_state(&state, TransferState::Cancelled);
                self.emit(&state, true);
                return;
            }
            // 从暂停回来：状态要变回「传输中」，不然界面一直显示已暂停。
            if self.state_of(&state) == TransferState::Paused {
                self.set_state(&state, TransferState::Running);
                self.emit(&state, true);
            }

            // 文件级并发许可。
            let _permit = match Arc::clone(&self.permits).acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => {
                    self.set_state(&state, TransferState::Cancelled);
                    self.emit(&state, true);
                    return;
                }
            };

            self.lock_progress(&state).current = Some(planned.target.clone());
            match self.run_one_file(&state, &planned, prefer_resume).await {
                Ok(FileOutcome::Done) => {
                    completed_files += 1;
                    self.lock_progress(&state).done_files += 1;
                }
                Ok(FileOutcome::Skipped) => {
                    skipped_files += 1;
                    self.lock_progress(&state).done_files += 1;
                }
                Ok(FileOutcome::Paused) => {
                    // 暂停：已传的字节保留在临时文件里，**同一个文件下次接着传**。
                    self.set_state(&state, TransferState::Paused);
                    self.emit(&state, true);
                    continue;
                }
                Err(SftpError::Cancelled) => {
                    self.set_state(&state, TransferState::Cancelled);
                    self.emit(&state, true);
                    return;
                }
                Err(err) => {
                    failed = Some(err);
                    break;
                }
            }
            index += 1;
            self.emit(&state, false);
        }

        let final_state = match failed {
            Some(err) => TransferState::Failed(err.to_string()),
            // 给结束状态之前再看一次取消：没有文件的任务（空目录）不会进上面的逐文件检查点，
            // 以前取消了也报「完成」。
            None if state.is_cancelled() => TransferState::Cancelled,
            // 全都按"跳过"处理了：这个任务算"跳过"（不是失败）。
            None if completed_files == 0 && skipped_files > 0 => TransferState::Skipped,
            None => TransferState::Done,
        };
        self.set_state(&state, final_state);
        self.emit(&state, true);
    }

    /// 展开任务（按方向走本地树或远端树）。
    async fn build_plan(&self, state: &Arc<JobState>) -> Result<plan::TransferPlan, SftpError> {
        let job = &state.job;
        match job.direction {
            // 本地树的遍历是同步的：放进阻塞线程池（不占着异步工作线程），并且每个目录 / 条目
            // 之前看一次取消，取消后规划很快停下。
            Direction::Upload => {
                let (state, job) = (Arc::clone(state), job.clone());
                self.run_local_plan(move || {
                    plan::plan_upload_with(
                        std::path::Path::new(&job.source),
                        &job.target,
                        job.recursive,
                        plan::MAX_PLAN_DEPTH,
                        &|| state.is_cancelled(),
                    )
                })
                .await
            }
            Direction::Download => {
                let lister: &dyn backend::RemoteLister = self.endpoints.as_ref();
                plan::plan_download(
                    lister,
                    &job.source,
                    std::path::Path::new(&job.target),
                    job.recursive,
                )
                .await
            }
            Direction::RemoteCopy => {
                self.endpoints
                    .plan_remote_copy(job.peer.as_deref(), &job.source, &job.target, job.recursive)
                    .await
            }
            Direction::LocalCopy => {
                let (state, job) = (Arc::clone(state), job.clone());
                self.run_local_plan(move || {
                    plan::plan_local_copy_with(
                        std::path::Path::new(&job.source),
                        std::path::Path::new(&job.target),
                        job.recursive,
                        plan::MAX_PLAN_DEPTH,
                        &|| state.is_cancelled(),
                    )
                })
                .await
            }
        }
    }

    /// 目标端的目录先建出来。每建一个目录之前都看一次取消：取消之后不能再新建目录。
    async fn create_dirs(
        &self,
        state: &Arc<JobState>,
        plan: &plan::TransferPlan,
    ) -> Result<(), SftpError> {
        match state.job.direction {
            Direction::Upload | Direction::RemoteCopy => {
                // 按深度排序再建：父目录一定排在子目录前面（不管展开顺序怎么来的）。
                let mut dirs: Vec<&String> = plan.dirs.iter().collect();
                dirs.sort_by_key(|dir| dir.matches('/').count());
                for dir in dirs {
                    if state.is_cancelled() {
                        return Err(SftpError::Cancelled);
                    }
                    self.until_cancelled(state, self.endpoints.create_target_dir(dir))
                        .await?;
                }
                Ok(())
            }
            Direction::Download | Direction::LocalCopy => {
                for dir in &plan.dirs {
                    if state.is_cancelled() {
                        return Err(SftpError::Cancelled);
                    }
                    let path = crate::sftp::local::long_path(std::path::Path::new(dir));
                    std::fs::create_dir_all(&path)
                        .map_err(|err| crate::sftp::error::from_io(err, &dir.clone()))?;
                }
                Ok(())
            }
        }
    }

    /// 跑 `work`，同时盯着取消：取消一到就丢掉它（停在它当前的 await 上，不再继续），
    /// 返回 [`SftpError::Cancelled`]。只给没有副作用、或副作用可以半途丢弃的步骤用（读目录、建目录）。
    async fn until_cancelled<T>(
        &self,
        state: &Arc<JobState>,
        work: impl std::future::Future<Output = Result<T, SftpError>>,
    ) -> Result<T, SftpError> {
        tokio::select! {
            biased;
            () = state.wait_cancelled() => Err(SftpError::Cancelled),
            out = work => out,
        }
    }

    /// 传一个文件（含冲突处理、断线重试、续传）。
    async fn run_one_file(
        self: &Arc<Self>,
        state: &Arc<JobState>,
        planned: &plan::PlannedFile,
        prefer_resume: bool,
    ) -> Result<FileOutcome, SftpError> {
        let mut attempt = 0u32;
        // 重试的任务：上次留下的 .zssh-part 先当续传起点试（尾部对账把着关）。
        let mut resume_from_partial = prefer_resume;
        // 暂停后重进同一个文件时，之前选的"改名"结果在任务状态里（局部变量会随返回丢掉，
        // 重进就会回到原目标或重新提问）。
        let mut renamed_target: Option<String> = state.actual_target(&planned.target);
        // 改名时已经抢到手的新名字的目标锁：交给下一轮用，中间不放手（见 `pick_free_name`）。
        let mut renamed_guard: Option<lock::TargetGuard> = None;
        // 用户是否**真的**同意过覆盖这个目标。开始时目标不存在（没问过）就是 false：
        // 提交用不覆盖的改名，传输期间别人建的同名文件不会被悄悄盖掉（C05）。
        let mut overwrite_authorized = false;
        // 提交时才发现目标冒出来了：重新走冲突流程，临时文件已经完整，接着它提交。
        let mut recheck_conflict = false;
        // 已经因为「提交时目标冒出来了」重查过几次（上限见 `MAX_COMMIT_RECHECKS`）。
        let mut commit_rechecks = 0u32;
        let owner = self.owner_of(state);

        loop {
            // 目标路径（可能因为冲突策略被改名 / 换了名字）。
            let target = renamed_target
                .clone()
                .unwrap_or_else(|| planned.target.clone());
            // 同一个最终目标同时只让一个任务写；这一轮结束（成功、失败、暂停）就放掉。
            let _target_guard = match renamed_guard.take() {
                Some(guard) => guard,
                None => self.lock_target(state.job.direction, &target).await?,
            };
            let pair = match self
                .endpoints
                .pair(
                    state.job.direction,
                    state.job.peer.as_deref(),
                    &planned.source,
                    &target,
                    &owner,
                )
                .await
            {
                Ok(pair) => pair,
                Err(err) if err.is_retryable() && attempt < self.options.max_retries => {
                    attempt += 1;
                    self.wait_for_retry(state, attempt, &err).await?;
                    continue;
                }
                Err(err) => return Err(err),
            };
            let source = Arc::clone(&pair.source);
            let target_endpoint = Arc::clone(&pair.target);

            // 冲突：目标已存在时问一次（同一个任务里"都这样处理"只问一次）。
            if renamed_target.is_none() {
                match self
                    .resolve_conflict(state, &source, &target_endpoint, planned, &target)
                    .await?
                {
                    ConflictOutcome::Proceed { resume, overwrite } => {
                        resume_from_partial = resume || recheck_conflict;
                        overwrite_authorized = overwrite;
                        recheck_conflict = false;
                    }
                    ConflictOutcome::Skip => {
                        // 跳过了：已经传完的（重查冲突时）、或上一次尝试留下的临时文件都用不上了。
                        if recheck_conflict {
                            let _ = target_endpoint.remove_partial().await;
                            state.forget_part(&planned.target, &target);
                        }
                        self.reap_parts(state, planned, None).await;
                        return Ok(FileOutcome::Skipped);
                    }
                    ConflictOutcome::Renamed(name, guard) => {
                        state.set_actual_target(&planned.target, name.clone());
                        renamed_target = Some(name);
                        renamed_guard = guard;
                        continue;
                    }
                }
            }

            // 最终目标定下来了。上一次尝试（失败后重试、重查冲突）用过别的名字的话，那个名字下的
            // 临时文件再也不会被用到，在这里删掉；就是这个目标的临时文件留着，下面接着续传。
            self.reap_parts(state, planned, Some(&target)).await;
            state.note_part(&planned.target, &planned.source, &target);

            // 续传：先看临时文件有多少，再和源对一下尾部；对不上从头来。
            // 源大小：已知就是个数；`None` 是源在规划之后被删了（这里直接按「源不存在」失败，
            // 不碰目标），或者服务器没回大小（按读到文件末尾为准，见 `source_size_or_missing`）。
            let source_size = self
                .source_size_or_missing(&source, &planned.source)
                .await?;
            let actual_len = if resume_from_partial {
                target_endpoint.partial_len().await?
            } else {
                0
            };
            // 续传点以「已确认的连续前缀」为准，不信临时文件的长度（可能带洞，见 `JobState::confirmed`）。
            let mut resume_len = resume_point(actual_len, state.confirmed_prefix(&target));
            if source_size.is_none() {
                // 大小未知没有办法判断临时文件是不是这个源的前缀，也没有「读到哪算完」的数可对：
                // 不续传，从头传（`open_write(0)` 会截断旧的临时文件）。
                resume_len = 0;
            }
            if actual_len > 0 && resume_len == 0 {
                self.push_note(
                    state,
                    Reason::new(
                        reason_keys::XFER_NOTE_NO_RESUME_POINT,
                        format!("{} 没有已确认的续传位置，从头传", planned.target),
                    )
                    .with("target", &planned.target),
                );
            }
            if source_size.is_some_and(|source_size| resume_len > source_size) {
                // 临时文件比源还长（源换了 / 上次传的是别的文件）：放弃续传。
                target_endpoint.remove_partial().await?;
                resume_len = 0;
            }
            if resume_len > 0 && actual_len > resume_len {
                // 确认的前缀之后可能有乱序落盘的块：先截掉再续。截不动就从头传
                // （`open_write(0)` 会截断临时文件），不冒险接在一个带洞的文件后面。
                if target_endpoint.truncate_partial(resume_len).await.is_err() {
                    self.push_note(
                        state,
                        Reason::new(
                            reason_keys::XFER_NOTE_CANNOT_TRUNCATE,
                            format!("{} 的临时文件无法截回续传位置，从头传", planned.target),
                        )
                        .with("target", &planned.target),
                    );
                    resume_len = 0;
                }
            }
            if resume_len > 0
                && !tails_match(
                    &source,
                    &target_endpoint,
                    resume_len,
                    self.options.tail_window,
                )
                .await?
            {
                self.push_note(
                    state,
                    Reason::new(
                        reason_keys::XFER_NOTE_RESUME_MISMATCH,
                        format!("{} 的续传位置对不上，从头传", planned.target),
                    )
                    .with("target", &planned.target),
                );
                target_endpoint.remove_partial().await?;
                resume_len = 0;
            }

            let tuning = CopyTuning {
                // 没显式配置时用数据面建议的块大小（SFTP 那边按方向取服务器声明的读 / 写限制）。
                chunk_len: self.options.chunk_len.unwrap_or_else(|| {
                    self.endpoints
                        .suggested_chunk_len(state.job.direction, state.job.peer.as_deref())
                }),
                in_flight: if self.options.pipeline {
                    self.options.in_flight.max(1)
                } else {
                    1
                },
            };
            // 本文件计入进度的字节数对齐到这次的起点：续传前缀算已完成，回退 / 从头来则先扣掉
            // 之前多计的，同一段数据不会被累加两次（断线重试、重传都走这里）。
            self.set_counted(state, &planned.target, planned.size, resume_len);
            // 上传 Unix 本地文件：远端新文件按源权限创建（0600 的私钥不能落成服务器默认的 0644）。
            // 源没有 Unix 权限位（Windows）时 `local_source_mode` 给 `None`，保持服务器默认。
            if matches!(state.job.direction, Direction::Upload) {
                target_endpoint.set_create_permissions(local_source_mode(&planned.source));
            }
            let confirmed_key = target.clone();
            let planned_key = planned.target.clone();
            let outcome = copy_file_sized(
                Arc::clone(&source),
                Arc::clone(&target_endpoint),
                source_size,
                resume_len,
                tuning,
                &state.control,
                {
                    let state = Arc::clone(state);
                    let engine = Arc::clone(self);
                    let planned_size = planned.size;
                    move |done, _total| {
                        // `done` 只在整批写成功之后才报：就是已确认的连续前缀。
                        state.record_confirmed(&confirmed_key, done);
                        engine.set_counted(&state, &planned_key, planned_size, done);
                        engine.emit(&state, false);
                    }
                },
            )
            .await;

            match outcome {
                Ok(CopyOutcome::Completed { bytes }) => {
                    // 大小未知时，读到的字节数就是文件大小（resume_len 一定是 0）。
                    let copied_len = source_size.unwrap_or(resume_len + bytes);
                    // 收尾：把临时文件改名成最终名字。只有用户同意过覆盖才允许替换；
                    // 其余情况由系统在改名那一刻判断目标是否存在（不是先查再改）。
                    let overwrite = renamed_target.is_none() && overwrite_authorized;
                    // 这是提交的取消边界；进入 commit 后不能中途丢弃改名请求，
                    // 否则无法判断最终文件是否已经替换成功。
                    if state.control.state() == file::ControlState::Cancelled {
                        if target_endpoint.remove_partial().await.is_ok() {
                            state.forget_part(&planned.target, &target);
                        }
                        return Err(SftpError::Cancelled);
                    }
                    // Unix 上下载的新文件权限不能比远端宽：临时文件在改名成最终名字之前定好权限
                    // （无论是否开了「保留权限」；续传来的旧临时文件、覆盖、改名下载也一样）。
                    // Windows 没有对应概念，不多发这一次属性查询。下载的远端权限位在展开计划时就已经拿到
                    // （目录列表 / 单文件的 stat 都带着），有就直接用，不再为每个文件多问一次服务端；
                    // 服务端没给时才补一次 `attributes()`。本地到本地的复制读源文件自己的权限位
                    // （保住执行位），和下载走同一套规则。
                    if cfg!(unix) {
                        match state.job.direction {
                            Direction::Download => {
                                let permissions = match planned.permissions {
                                    Some(mode) => Some(mode & 0o7777),
                                    None => source.attributes().await.ok().and_then(|(_, p)| p),
                                };
                                let _ = target_endpoint.prepare_commit(permissions).await;
                            }
                            Direction::LocalCopy => {
                                let _ = target_endpoint
                                    .prepare_commit(local_source_mode(&planned.source))
                                    .await;
                            }
                            // 上传：远端临时文件在改名成最终名字之前收窄到不比源宽
                            // （创建时已经按源权限建了，这里兜住服务器忽略创建属性、续传来的旧临时文件）。
                            Direction::Upload => {
                                let _ = target_endpoint
                                    .prepare_commit(local_source_mode(&planned.source))
                                    .await;
                            }
                            Direction::RemoteCopy => {}
                        }
                    }
                    if let Err(err) = target_endpoint.commit(copied_len, overwrite).await {
                        if !overwrite
                            && renamed_target.is_none()
                            && matches!(err, SftpError::AlreadyExists { .. })
                        {
                            // 传输期间别人在目标位置建了同名文件：按冲突处理（问用户 /
                            // 沿用"对后续都这样"），不能当成失败，更不能盖掉它。
                            self.push_note(
                                state,
                                Reason::new(
                                    reason_keys::XFER_NOTE_TARGET_APPEARED,
                                    format!(
                                        "传输期间 {target} 被别的程序建出来了，重新确认怎么处理"
                                    ),
                                )
                                .with("target", &target),
                            );
                            // 重查时端点说「目标不存在」、提交又说「已存在」（两边对同一个
                            // 路径的判断不一致，例如悬空链接）会永远转下去：超过次数就把
                            // 「已存在」当结果报出来，不能无限循环。
                            commit_rechecks += 1;
                            if commit_rechecks > MAX_COMMIT_RECHECKS {
                                if target_endpoint.remove_partial().await.is_ok() {
                                    state.forget_part(&planned.target, &target);
                                }
                                return Err(SftpError::AlreadyExists { path: target });
                            }
                            recheck_conflict = true;
                            resume_from_partial = true;
                            continue;
                        }
                        // 提交阶段断线也算可重试（临时文件还在，能接着来）。
                        if err.is_retryable() && attempt < self.options.max_retries {
                            attempt += 1;
                            self.wait_for_retry(state, attempt, &err).await?;
                            resume_from_partial = true;
                            continue;
                        }
                        return Err(err);
                    }
                    if self.options.preserve_mtime || self.options.preserve_permissions {
                        let (mtime, permissions) =
                            source.attributes().await.unwrap_or((None, None));
                        let _ = target_endpoint
                            .set_attributes(
                                if self.options.preserve_mtime {
                                    mtime
                                } else {
                                    None
                                },
                                if self.options.preserve_permissions {
                                    permissions
                                } else {
                                    None
                                },
                            )
                            .await;
                    }
                    // 临时文件已经改名成最终文件，没有临时文件要记着了。
                    state.forget_part(&planned.target, &target);
                    // 传完的文件整个算进度（大小以计划为准，没有就用这次读到的源大小）。
                    self.set_counted(
                        state,
                        &planned.target,
                        planned.size,
                        planned.size.unwrap_or(copied_len),
                    );
                    self.lock_progress(state).current = None;
                    return Ok(FileOutcome::Done);
                }
                Ok(CopyOutcome::Paused { .. }) => return Ok(FileOutcome::Paused),
                Ok(CopyOutcome::Cancelled { .. }) => {
                    if target_endpoint.remove_partial().await.is_ok() {
                        state.forget_part(&planned.target, &target);
                    }
                    return Err(SftpError::Cancelled);
                }
                Err(err) => {
                    if err.is_disconnect() && attempt < self.options.max_retries {
                        attempt += 1;
                        self.endpoints.reset().await;
                        self.wait_for_retry(state, attempt, &err).await?;
                        resume_from_partial = true;
                        continue;
                    }
                    return Err(err);
                }
            }
        }
    }

    /// 源文件大小。`Ok(None)` = 文件在，但服务器没回大小（SFTP 协议允许），按读到文件末尾复制；
    /// 文件已经不在（规划之后被删了）直接返回 `NotFound`。
    ///
    /// 为什么要多问一次：`size()` 对这两种情况都返回 `None`，当成 0 字节复制会把已有的目标
    /// 换成空文件，结果还是「完成」。
    async fn source_size_or_missing(
        &self,
        source: &Arc<dyn FileEndpoint>,
        source_path: &str,
    ) -> Result<Option<u64>, SftpError> {
        let size = source.size().await?;
        if size.is_none() && !source.exists().await? {
            return Err(SftpError::NotFound {
                path: source_path.to_owned(),
            });
        }
        Ok(size)
    }

    /// 把一个文件计入进度的字节数改成 `new`（`done_bytes` 同步加减差值）。
    ///
    /// 不超过计划里的文件大小：源在传输期间变大时，多出来的部分不能让进度超过总量。
    /// 大小未知（`None`）的文件总量里按 0 计，这里不封顶（速度和进度条还要用它）。
    fn set_counted(
        &self,
        state: &Arc<JobState>,
        planned_target: &str,
        planned_size: Option<u64>,
        new: u64,
    ) {
        let new = planned_size.map_or(new, |size| new.min(size));
        let old = state.replace_counted(planned_target, new);
        let mut progress = self.lock_progress(state);
        progress.done_bytes = progress.done_bytes.saturating_sub(old).saturating_add(new);
    }

    /// 临时文件名里的任务标识：同一个任务的重试 / 续传不变，别的任务不同。
    fn owner_of(&self, state: &JobState) -> String {
        format!("{}{}", self.nonce, state.id)
    }

    /// 拿目标的写锁。远端锁是**进程级共享**的，键 = 数据面连的服务器身份（实际连接的
    /// 用户名@主机:端口）+ 路径：每个标签、每个文件侧栏各有一个引擎，不管怎么连上的，
    /// 上传同一台服务器的同一个路径都互斥。
    async fn lock_target(
        &self,
        direction: Direction,
        target: &str,
    ) -> Result<lock::TargetGuard, SftpError> {
        match direction {
            Direction::Download | Direction::LocalCopy => {
                lock::TargetLocks::local().try_acquire_local(std::path::Path::new(target))
            }
            Direction::Upload | Direction::RemoteCopy => {
                let server = self.endpoints.server_identity().await?;
                lock::TargetLocks::remote().try_acquire_remote(&server, target)
            }
        }
    }

    /// 取消时把这个文件在目标端的临时文件删掉（尽力而为：连不上就算了，不报错）。
    ///
    /// 清的是**实际目标**（冲突里改过名就是新名字）的临时文件；原目标不碰。
    async fn discard_partial(&self, state: &Arc<JobState>, planned: &plan::PlannedFile) {
        let target = state
            .actual_target(&planned.target)
            .unwrap_or_else(|| planned.target.clone());
        if let Ok(pair) = self
            .endpoints
            .pair(
                state.job.direction,
                state.job.peer.as_deref(),
                &planned.source,
                &target,
                &self.owner_of(state),
            )
            .await
        {
            if pair.target.remove_partial().await.is_ok() {
                state.forget_part(&planned.target, &target);
            }
        }
        // 前几次尝试用别的名字留下的也一并清（取消之后它们不会再有人续传）。
        self.reap_parts(state, planned, None).await;
    }

    /// 删掉这个文件记着的、不在 `keep` 位置的临时文件（`keep = None` 就是全删）。
    ///
    /// 删的永远是 `part_name(<目标>, <本任务标识>)`，也就是本任务自己的临时文件；用户自己的
    /// 同名 `.zssh-part` 和别的任务的临时文件名字对不上，碰不到。尽力而为：删不掉（连接不在、
    /// 没权限）就保留记录，下次有机会再删。
    async fn reap_parts(
        &self,
        state: &Arc<JobState>,
        planned: &plan::PlannedFile,
        keep: Option<&str>,
    ) {
        let (_, stale) = state.stale_parts(&planned.target, keep);
        for part_target in stale {
            self.remove_part_at(state, &planned.target, &planned.source, &part_target)
                .await;
        }
    }

    /// 任务结束于取消时，清掉所有文件记着的临时文件（包括还没轮到的文件上一次尝试留下的）。
    async fn reap_all_parts(&self, state: &Arc<JobState>) {
        let pending: Vec<(String, String, Vec<String>)> = {
            let files = state
                .files
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            files
                .iter()
                .filter(|(_, run)| !run.parts.is_empty())
                .map(|(planned, run)| {
                    (
                        planned.clone(),
                        run.source.clone().unwrap_or_default(),
                        run.parts.clone(),
                    )
                })
                .collect()
        };
        for (planned_target, source, parts) in pending {
            for part_target in parts {
                self.remove_part_at(state, &planned_target, &source, &part_target)
                    .await;
            }
        }
    }

    async fn remove_part_at(
        &self,
        state: &Arc<JobState>,
        planned_target: &str,
        source: &str,
        part_target: &str,
    ) {
        let pair = self
            .endpoints
            .pair(
                state.job.direction,
                state.job.peer.as_deref(),
                source,
                part_target,
                &self.owner_of(state),
            )
            .await;
        if let Ok(pair) = pair {
            if pair.target.remove_partial().await.is_ok() {
                state.forget_part(planned_target, part_target);
            }
        }
    }

    /// 断线 / 可重试错误：报"等待重连"，退避一下再试。
    async fn wait_for_retry(
        &self,
        state: &Arc<JobState>,
        attempt: u32,
        err: &SftpError,
    ) -> Result<(), SftpError> {
        self.set_state(state, TransferState::WaitingReconnect);
        self.push_note(
            state,
            Reason::new(
                reason_keys::XFER_NOTE_RECONNECT,
                format!("连接断了（第 {attempt} 次重试）：{err}"),
            )
            .with("attempt", attempt)
            .caused_by(err),
        );
        self.emit(state, true);
        let delay = backoff_delay(attempt);
        // 退避期间也响应取消。
        let deadline = Instant::now() + delay;
        while Instant::now() < deadline {
            if state.control.state() == file::ControlState::Cancelled {
                return Err(SftpError::Cancelled);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // 退避结束要重连了：不再是「等待重连」。
        self.set_state(state, TransferState::Running);
        self.emit(state, true);
        Ok(())
    }

    /// 目标已存在时怎么处理。
    async fn resolve_conflict(
        &self,
        state: &Arc<JobState>,
        source: &Arc<dyn FileEndpoint>,
        target: &Arc<dyn FileEndpoint>,
        planned: &plan::PlannedFile,
        target_path: &str,
    ) -> Result<ConflictOutcome, SftpError> {
        // 暂停之后重进这个文件：之前已经问过（也答过），接着断点传，不再问。
        let remembered_here = self
            .lock_progress(state)
            .conflicted_targets
            .get(&planned.target)
            .cloned();
        match remembered_here {
            Some(RememberedConflict::Overwrite) => {
                return Ok(ConflictOutcome::Proceed {
                    resume: true,
                    overwrite: true,
                })
            }
            None => {}
        }
        let Some(existing_size) = target.existing_size().await? else {
            // 没有冲突。临时文件名里带着本任务的标识，能找到的临时文件只可能是这个任务暂停 /
            // 断线前自己留下的：接着它传（尾部对账把关，对不上从头来）。以前这里返回
            // `resume: false`，把暂停前传好的部分整个丢掉、从 0 重传（R-9 F14）。
            // 没问过用户，所以不授权覆盖。
            return Ok(ConflictOutcome::Proceed {
                resume: true,
                overwrite: false,
            });
        };
        // 源已经不在了就不用问用户怎么处理冲突了：覆盖一个不存在的源没有意义（见 `source_size_or_missing`）。
        let source_size = self.source_size_or_missing(source, &planned.source).await?;
        // 修改时间给冲突对话框用（两端各拿一次；拿不到就是 None，不拦着）。
        let source_mtime = source.attributes().await.ok().and_then(|(mtime, _)| mtime);
        let target_mtime = target.existing_mtime().await.unwrap_or(None);
        // 续传只在"目标比源小"的时候有意义（不然没有要补的部分）。
        let can_resume = source_size.is_some_and(|source_size| existing_size < source_size);
        // 先看任务自己记的，再看批次的（同一次提交里别的任务勾过"全部应用"）。
        let batch = state
            .batch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let remembered = self
            .lock_progress(state)
            .conflict_all
            .or_else(|| batch.as_ref().and_then(|batch| batch.action()));
        let decision = match remembered {
            Some(action) => ConflictDecision {
                action,
                apply_to_all: true,
                rename_to: None,
            },
            None => {
                // 等用户回答期间让界面看得出是在等人（回答最长等 2 分钟，不能显示成「准备中」）。
                self.set_state(state, TransferState::WaitingConflict);
                self.emit(state, true);
                let suggested_target = self.suggest_free_name(state, planned, target_path).await;
                let suggested_name = suggested_target.clone();
                let decision = self
                    .resolver
                    .decide(ConflictInfo {
                        job: state.id,
                        direction: state.job.direction,
                        source: planned.source.clone(),
                        target: target_path.to_owned(),
                        source_size,
                        target_size: Some(existing_size),
                        source_mtime,
                        target_mtime,
                        can_resume,
                        suggested_target,
                        batch: batch.as_ref().map(|batch| batch.id()),
                    })
                    .await
                    .without_suggested_name(&suggested_name);
                self.set_state(state, TransferState::Running);
                self.emit(state, true);
                decision
            }
        };
        if decision.apply_to_all {
            self.lock_progress(state).conflict_all = Some(decision.action);
            if let Some(batch) = &batch {
                batch.remember(decision.action);
            }
        }
        let outcome = self
            .apply_conflict_decision(state, planned, &decision, can_resume, target_path)
            .await?;
        let remembered = match &outcome {
            ConflictOutcome::Proceed { .. } => Some(RememberedConflict::Overwrite),
            ConflictOutcome::Renamed(..) | ConflictOutcome::Skip => None,
        };
        if let Some(remembered) = remembered {
            self.lock_progress(state)
                .conflicted_targets
                .insert(planned.target.clone(), remembered);
        }
        Ok(outcome)
    }

    /// 把用户的冲突选择变成这个文件接下来怎么做。
    async fn apply_conflict_decision(
        &self,
        state: &Arc<JobState>,
        planned: &plan::PlannedFile,
        decision: &ConflictDecision,
        can_resume: bool,
        target_path: &str,
    ) -> Result<ConflictOutcome, SftpError> {
        Ok(match decision.action {
            ConflictAction::Overwrite => ConflictOutcome::Proceed {
                resume: false,
                overwrite: true,
            },
            ConflictAction::Skip => {
                self.push_note(
                    state,
                    Reason::new(
                        reason_keys::XFER_NOTE_SKIP_EXISTING,
                        format!("目标已存在，跳过 {target_path}"),
                    )
                    .with("path", target_path),
                );
                ConflictOutcome::Skip
            }
            ConflictAction::Resume => {
                if !can_resume {
                    self.push_note(
                        state,
                        Reason::new(
                            reason_keys::XFER_NOTE_CANNOT_RESUME,
                            format!("{target_path} 不能续传（目标不比源小），改成覆盖"),
                        )
                        .with("path", target_path),
                    );
                }
                ConflictOutcome::Proceed {
                    resume: true,
                    overwrite: true,
                }
            }
            ConflictAction::Rename => {
                // 目标换个名字：用户给过就用用户的，没给就 `name (2).ext`。
                // 对话框传回来的只是一个**文件名**（不是路径）：要接在原目标所在的
                // 目录后面，不然会写到相对路径上（远端是家目录、本地是工作目录）。
                // 带路径分隔符的名字不接受，退回自动名字（Windows 上 `\` 也是分隔符，
                // Linux 上它是名字里的普通字符）。
                let manual = decision
                    .rename_to
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty() && !name.contains(is_path_separator))
                    .map(|name| with_file_name(target_path, name));
                let (renamed, guard) = match manual {
                    Some(path) => self.claim_manual_name(state, planned, path).await?,
                    None => self.pick_free_name(state, planned, target_path).await?,
                };
                self.push_note(
                    state,
                    Reason::new(
                        reason_keys::XFER_NOTE_RENAMED_TO,
                        format!("目标已存在，改成 {renamed}"),
                    )
                    .with("renamed", &renamed),
                );
                ConflictOutcome::Renamed(renamed, Some(guard))
            }
        })
    }

    /// 第 `n` 个（从 1 数起）自动名字：上传 / 下载是 `名字 (2)`、`名字 (3)`…；
    /// 复制粘贴是 `名字 - 副本`、`名字 - 副本 2`…（和粘贴时的习惯一致）。
    fn auto_candidate(direction: Direction, target_path: &str, n: usize) -> String {
        match direction {
            Direction::RemoteCopy | Direction::LocalCopy => copy_path(target_path, n),
            Direction::Upload | Direction::Download => renamed_path(target_path, n + 1),
        }
    }

    /// 目标端这个路径上是不是已经有东西（文件、目录、链接都算占用）。
    async fn path_taken(
        &self,
        state: &Arc<JobState>,
        planned: &plan::PlannedFile,
        path: &str,
    ) -> Result<bool, SftpError> {
        let pair = self
            .endpoints
            .pair(
                state.job.direction,
                state.job.peer.as_deref(),
                &planned.source,
                path,
                &self.owner_of(state),
            )
            .await?;
        Ok(pair.target.existing_size().await?.is_some())
    }

    /// 自动改名：往后找第一个没被占用的名字，**同时把它的目标锁抢下来**一路带到传输。
    ///
    /// 为什么选名字和上锁要在一起：同一时刻两个任务都想改名到同一个空名字时，光看「现在
    /// 有没有这个文件」是看不出来的（先到的那个文件还没写出来）。谁先抢到锁谁用，后到的
    /// 看到 `TargetBusy` 就当这个名字被占了，接着往后找；此时原目标的锁还在手里，所以
    /// 选名到开写之间没有空隙。
    async fn pick_free_name(
        &self,
        state: &Arc<JobState>,
        planned: &plan::PlannedFile,
        target_path: &str,
    ) -> Result<(String, lock::TargetGuard), SftpError> {
        for n in 1..=MAX_RENAME_TRIES {
            let candidate = Self::auto_candidate(state.job.direction, target_path, n);
            let guard = match self.lock_target(state.job.direction, &candidate).await {
                Ok(guard) => guard,
                // 别的任务正要写这个名字。
                Err(SftpError::TargetBusy { .. }) => continue,
                Err(err) => return Err(err),
            };
            if self.path_taken(state, planned, &candidate).await? {
                continue;
            }
            return Ok((candidate, guard));
        }
        Err(SftpError::NoFreeName {
            path: target_path.to_owned(),
            tried: MAX_RENAME_TRIES,
        })
    }

    /// 用户在对话框里手动输入的新名字：必须没被占用（也不能是别的任务正在写的），
    /// 否则这个文件按失败处理，不静默覆盖、也不悄悄换成别的名字。
    async fn claim_manual_name(
        &self,
        state: &Arc<JobState>,
        planned: &plan::PlannedFile,
        path: String,
    ) -> Result<(String, lock::TargetGuard), SftpError> {
        let exists = |path: &str| SftpError::RenameTargetExists {
            name: path.rsplit(['/', '\\']).next().unwrap_or(path).to_owned(),
        };
        // 先查再锁：新名字和原目标其实是同一个文件（只是大小写不同的写法）时，
        // 锁会说「别的任务在写」，实际该告诉用户「新名字也已存在」。
        if self.path_taken(state, planned, &path).await? {
            return Err(exists(&path));
        }
        let guard = self.lock_target(state.job.direction, &path).await?;
        // 拿到锁之后再看一眼：刚才查完到上锁之间，别的任务可能刚好写完这个名字。
        if self.path_taken(state, planned, &path).await? {
            return Err(exists(&path));
        }
        Ok((path, guard))
    }

    /// 对话框里预填的名字：第一个没被占用的自动名字。只是建议，不上锁；检查失败
    /// 或试到上限就退回固定的第一个名字（不能因为这个卡住提问）。
    async fn suggest_free_name(
        &self,
        state: &Arc<JobState>,
        planned: &plan::PlannedFile,
        target_path: &str,
    ) -> String {
        let direction = state.job.direction;
        for n in 1..=MAX_RENAME_TRIES {
            let candidate = Self::auto_candidate(direction, target_path, n);
            match self.path_taken(state, planned, &candidate).await {
                Ok(false) => return candidate,
                Ok(true) => {}
                Err(_) => break,
            }
        }
        Self::auto_candidate(direction, target_path, 1)
    }

    fn finish_with_error(&self, state: &Arc<JobState>, err: SftpError) {
        // 取消不是失败：规划 / 建目录阶段被取消，最终状态是「已取消」。
        let next = match err {
            SftpError::Cancelled => TransferState::Cancelled,
            other => TransferState::Failed(other.to_string()),
        };
        self.set_state(state, next);
        self.emit(state, true);
    }

    fn set_state(&self, state: &Arc<JobState>, next: TransferState) {
        let mut current = state
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if current.is_terminal() {
            return;
        }
        *current = next;
    }

    fn state_of(&self, state: &Arc<JobState>) -> TransferState {
        state
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn lock_progress<'a>(
        &self,
        state: &'a Arc<JobState>,
    ) -> std::sync::MutexGuard<'a, JobProgress> {
        state
            .progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn push_note(&self, state: &Arc<JobState>, note: Reason) {
        let mut progress = self.lock_progress(state);
        if !progress.notes.contains(&note) {
            progress.notes.push(note);
        }
    }

    /// 发一条进度事件（`force = false` 时按 `progress_interval` 节流）。
    fn emit(&self, state: &Arc<JobState>, force: bool) {
        let now = Instant::now();
        let report = {
            let mut progress = self.lock_progress(state);
            let current = self.state_of(state);
            let throttled = progress
                .last_emit
                .is_some_and(|last| now.duration_since(last) < self.options.progress_interval);
            if throttled && !force && !current.is_terminal() {
                return;
            }
            progress.last_emit = Some(now);
            // 速度：拿最近一次采样算。
            match progress.sample {
                Some((at, bytes)) => {
                    let elapsed = now.duration_since(at).as_secs_f64();
                    if elapsed > 0.05 {
                        let delta = progress.done_bytes.saturating_sub(bytes) as f64;
                        let instant = (delta / elapsed) as u64;
                        // 平滑一下：新旧各一半（网络速度本来就有抖动）。
                        progress.speed_bps = (progress.speed_bps + instant) / 2;
                        progress.sample = Some((now, progress.done_bytes));
                    }
                }
                None => progress.sample = Some((now, progress.done_bytes)),
            }
            let remaining = progress.total_bytes.saturating_sub(progress.done_bytes);
            let eta_secs = remaining.checked_div(progress.speed_bps);
            ProgressReport {
                id: state.id,
                direction: state.job.direction,
                state: current.clone(),
                total_files: progress.total_files,
                done_files: progress.done_files,
                total_bytes: progress.total_bytes,
                done_bytes: progress.done_bytes,
                speed_bps: progress.speed_bps,
                eta_secs,
                current: progress.current.clone(),
                notes: progress.notes.clone(),
            }
        };
        let event = if report.state.is_terminal() {
            TransferEvent::Finished(report)
        } else {
            TransferEvent::Progress(report)
        };
        let _ = self.events.send(event);
    }

    // ---------- 对外操作 ----------

    /// 暂停一个任务（当前文件传完这一批就停，临时文件留着）。
    pub fn pause(&self, id: TransferId) -> bool {
        let Some(state) = self.job(id) else {
            return false;
        };
        state.control.pause();
        true
    }

    /// 继续（暂停或等待重连之后）。
    pub fn resume(&self, id: TransferId) -> bool {
        let Some(state) = self.job(id) else {
            return false;
        };
        state.control.resume();
        state.resume_signal.notify_waiters();
        true
    }

    /// 取消（已删的不回滚；临时文件删掉）。
    pub fn cancel(&self, id: TransferId) -> bool {
        let Some(state) = self.job(id) else {
            return false;
        };
        state.control.cancel();
        state.resume_signal.notify_waiters();
        true
    }

    /// 现在这个任务的状态（诊断 / 测试）。
    pub fn state(&self, id: TransferId) -> Option<TransferState> {
        self.job(id).map(|state| self.state_of(&state))
    }

    /// 任务的最新报告（诊断 / 测试）。
    pub fn report(&self, id: TransferId) -> Option<ProgressReport> {
        self.job(id).map(|state| self.report_of(&state))
    }

    fn job(&self, id: TransferId) -> Option<Arc<JobState>> {
        self.jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&id)
            .cloned()
    }
}

impl TransferEngine {
    /// 在阻塞线程池里跑一次同步的本地规划，按提交顺序排队（见 `local_plan_gate`）。
    /// 线程没跑起来（池关了、任务 panic）按内部错误报。
    async fn run_local_plan(
        &self,
        work: impl FnOnce() -> Result<plan::TransferPlan, SftpError> + Send + 'static,
    ) -> Result<plan::TransferPlan, SftpError> {
        // 排队时被取消：整个规划 future 被丢掉，排队也就跟着取消了。
        let _turn = self.local_plan_gate.lock().await;
        tokio::task::spawn_blocking(work).await.map_err(|err| {
            SftpError::Internal(
                Reason::new(
                    reason_keys::SFTP_TASK_LOCAL_READ,
                    format!("本地读任务没跑起来：{err}"),
                )
                .with("cause", &err),
            )
        })?
    }
}

/// 一个文件传完之后的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileOutcome {
    Done,
    Skipped,
    Paused,
}

/// 冲突处理之后的走向。
enum ConflictOutcome {
    /// `overwrite`：用户同意过替换已有的目标（只有这时提交才用覆盖式改名）。
    Proceed {
        resume: bool,
        overwrite: bool,
    },
    Skip,
    /// 改成这个名字。带着的锁是新名字的目标锁（选名字的时候就抢下来了）；
    /// 暂停后重进时记住的旧名字没有锁（`None`），照常在下一轮重新拿。
    Renamed(String, Option<lock::TargetGuard>),
}

/// 提交时发现目标冒出来了，重新走冲突流程最多这么多次。
///
/// 正常情况一次就够：重查时用户（或「全部应用」）会选覆盖 / 改名 / 跳过，之后不再冲突；
/// 目标在重查的间隙里又被别人建出来是少见的巧合，所以给几次余地。再多就说明端点对「目标是否
/// 存在」的判断和提交时的判断对不上，继续转只会卡死，按「已存在」报错。
const MAX_COMMIT_RECHECKS: u32 = 3;

/// 自动改名最多往后试这么多个名字，再多就按失败处理（不能无限循环）。
const MAX_RENAME_TRIES: usize = 1000;

/// 退避：200ms、400ms、800ms…（上限 5 秒）。
pub fn backoff_delay(attempt: u32) -> Duration {
    let millis = 200u64.saturating_mul(1u64 << attempt.min(5));
    Duration::from_millis(millis.min(5_000))
}

/// 目标改名：`/a/photo.jpg` + 2 → `/a/photo (2).jpg`。
///
/// 目录分隔符：Windows 上 `/` 和 `\` 都认（本地 Windows 路径是反斜杠）：只认 `/` 的话，
/// `C:\my.dir\file` 这种"目录名里有点、文件没扩展名"的路径会被改在目录名上。
/// Linux 上只认 `/`，`\` 是文件名里的普通字符（`a\b.txt` 是一个名字）。
pub fn renamed_path(path: &str, index: usize) -> String {
    let name_start = name_start(path);
    match path.rfind('.') {
        Some(dot) if dot > name_start => {
            format!("{} ({index}){}", &path[..dot], &path[dot..])
        }
        _ => format!("{path} ({index})"),
    }
}

/// 复制粘贴的"副本"名：`/a/b.txt` → `/a/b - 副本.txt`；`index` 大于 1 时是 `b - 副本 2.txt`。
pub fn copy_path(path: &str, index: usize) -> String {
    let name_start = name_start(path);
    let suffix = if index <= 1 {
        " - 副本".to_owned()
    } else {
        format!(" - 副本 {index}")
    };
    match path.rfind('.') {
        Some(dot) if dot > name_start => format!("{}{suffix}{}", &path[..dot], &path[dot..]),
        _ => format!("{path}{suffix}"),
    }
}

/// 把路径的最后一段换成 `name`：`/a/b.txt` + `c.txt` → `/a/c.txt`
/// （`C:\a\b.txt` 同理）。冲突对话框里用户改的新名字只是个文件名。
pub fn with_file_name(path: &str, name: &str) -> String {
    format!("{}{name}", &path[..name_start(path)])
}

/// 是不是路径分隔符：Windows 上 `/` 和 `\` 都是，其他系统只有 `/`。
fn is_path_separator(c: char) -> bool {
    c == '/' || (cfg!(windows) && c == '\\')
}

/// 路径最后一段（文件名）从第几个字节开始。
fn name_start(path: &str) -> usize {
    path.rfind(is_path_separator)
        .map(|separator| separator + 1)
        .unwrap_or(0)
}

/// 本地复制的源文件权限位（跟随符号链接，读的是链接指向的文件）；读不到就 `None`（按默认权限）。
#[cfg(unix)]
fn local_source_mode(path: &str) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::metadata(path)
        .ok()
        .map(|meta| meta.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn local_source_mode(_path: &str) -> Option<u32> {
    None
}

#[cfg(test)]
mod chunk_tests;
#[cfg(test)]
mod tests;
