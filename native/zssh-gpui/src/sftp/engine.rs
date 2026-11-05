//! SFTP 窗口的“引擎侧”代码：覆盖确认、同名文件提问者、递归删除、本地列表、路径与名字规则、
//! 自动化钩子的环境变量解析。这里没有界面状态，都是可以单独测试的函数和类型。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

use zssh_core::sftp::fs::{self, CancelHandle, RecursiveDelete};
use zssh_core::sftp::local::check_child_name;
use zssh_core::sftp::path::{file_name, join, normalize, parent, relative};
use zssh_core::sftp::transfer::{
    ConflictAction, ConflictDecision, ConflictInfo, ConflictResolver, Direction, TransferId,
    TransferState,
};
use zssh_core::sftp::{SftpChannel, SftpError};
use zssh_core::ssh::BoxFuture;
use zssh_core::store::Database;

use super::{SftpEvent, TestHook};
use crate::error_text::error_text;
use crate::i18n::t;
use crate::settings::data_dir;

/// `ZSSH_SFTP_CONFLICT` 的取值：自动化时由 resolver 直接回答，不弹面板。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConflictAutomation {
    Overwrite,
    Skip,
    Rename,
    OverwriteAll,
    SkipAll,
}

impl ConflictAutomation {
    /// 自动化的回答。`overwrite` / `skip` 每次都由 resolver 回答（引擎照常走冲突流程），
    /// `-all` 直接让引擎记住"后面的都这样处理"。
    pub(super) fn decision(self) -> ConflictDecision {
        let (action, apply_to_all) = match self {
            Self::Overwrite => (ConflictAction::Overwrite, false),
            Self::Skip => (ConflictAction::Skip, false),
            // 自动化不知道要改成什么名字：交给引擎按 `名字 (2)` 自动起名。
            Self::Rename => (ConflictAction::Rename, false),
            Self::OverwriteAll => (ConflictAction::Overwrite, true),
            Self::SkipAll => (ConflictAction::Skip, true),
        };
        ConflictDecision {
            action,
            apply_to_all,
            rename_to: None,
        }
    }
}

/// 引擎正在等回答的同名文件。并发默认 3，可能同时有好几个，按到达顺序显示最前面那个。
pub(super) struct ConflictPrompt {
    pub(super) id: u64,
    pub(super) info: ConflictInfo,
}

/// 回答"跳过"：界面没了 / 回答通道断了时用，引擎按跳过收尾，不会一直挂着。
pub(super) fn skip_decision() -> ConflictDecision {
    ConflictDecision {
        action: ConflictAction::Skip,
        apply_to_all: false,
        rename_to: None,
    }
}

/// 界面按钮 → 引擎的回答。
///
/// 重命名用输入框里的名字：空着 = 交给引擎按 `名字 (2)` 自动起名；写了就要通过落点那一端的
/// 名字规则（下载落在本机，要挡 Windows 非法字符、保留名、备用数据流），不合格的不送出去。
pub(super) fn conflict_decision(
    action: ConflictAction,
    direction: Direction,
    typed_name: &str,
) -> Result<ConflictDecision, String> {
    if !matches!(action, ConflictAction::Rename) {
        return Ok(ConflictDecision {
            action,
            apply_to_all: false,
            rename_to: None,
        });
    }
    Ok(ConflictDecision {
        action,
        apply_to_all: false,
        rename_to: conflict_rename_name(direction, typed_name)?,
    })
}

/// 冲突「重命名」的新名字。落在本机（下载、本地复制）的名字复用核心的
/// [`check_child_name`]；落在远端（上传、远端复制）的名字只能是一级、非空、不是 `.` / `..`。
pub(super) fn conflict_rename_name(
    direction: Direction,
    typed_name: &str,
) -> Result<Option<String>, String> {
    let typed = typed_name.trim();
    if typed.is_empty() {
        return Ok(None);
    }
    if typed.contains(['/', '\\']) {
        return Err(format!(
            "{}{typed}",
            t(super::SftpMsg::NamePathSeparatorPrefix)
        ));
    }
    match direction {
        Direction::Download | Direction::LocalCopy => check_child_name(typed)
            .map(Some)
            .map_err(|err| error_text(&err)),
        Direction::Upload | Direction::RemoteCopy => {
            if typed.contains('\0') {
                return Err(t(super::SftpMsg::ConflictNameNullChar).to_owned());
            }
            valid_remote_name(typed).map(Some)
        }
    }
}

pub(super) fn conflict_action_label(action: ConflictAction) -> &'static str {
    match action {
        ConflictAction::Overwrite => t(super::SftpMsg::ConflictActionOverwrite),
        ConflictAction::Skip => t(super::SftpMsg::ConflictActionSkip),
        ConflictAction::Rename => t(super::SftpMsg::ConflictActionRename),
        ConflictAction::Resume => t(super::SftpMsg::ConflictActionResume),
    }
}

/// 面板上两边的对比：`(源, 目标, 提示)`。大小和修改时间拿不到就不显示那一项。
pub(super) fn conflict_compare(info: &ConflictInfo) -> (String, String, String) {
    pub(super) fn side(label: &str, size: Option<u64>, mtime: Option<u32>) -> String {
        let mut parts = Vec::new();
        if let Some(size) = size {
            parts.push(crate::format::size(size));
        }
        if let Some(mtime) = mtime {
            parts.push(format_mtime(mtime));
        }
        if parts.is_empty() {
            format!(
                "{label}{}{}",
                t(super::SftpMsg::ConflictSideMiddle),
                t(super::SftpMsg::ConflictUnknownSizeTime)
            )
        } else {
            format!(
                "{label}{}{}",
                t(super::SftpMsg::ConflictSideMiddle),
                parts.join(" · ")
            )
        }
    }
    let source = side(
        t(super::SftpMsg::ConflictSource),
        info.source_size,
        info.source_mtime,
    );
    let target = side(
        t(super::SftpMsg::ConflictTarget),
        info.target_size,
        info.target_mtime,
    );
    let mut hints = Vec::new();
    if let (Some(source_size), Some(target_size)) = (info.source_size, info.target_size) {
        hints.push(match source_size.cmp(&target_size) {
            std::cmp::Ordering::Equal => t(super::SftpMsg::ConflictSizeEqual).to_owned(),
            std::cmp::Ordering::Greater => t(super::SftpMsg::ConflictTargetSmaller).to_owned(),
            std::cmp::Ordering::Less => t(super::SftpMsg::ConflictTargetBigger).to_owned(),
        });
    }
    if let (Some(source_mtime), Some(target_mtime)) = (info.source_mtime, info.target_mtime) {
        hints.push(match source_mtime.cmp(&target_mtime) {
            std::cmp::Ordering::Equal => t(super::SftpMsg::ConflictMtimeEqual).to_owned(),
            std::cmp::Ordering::Greater => t(super::SftpMsg::ConflictSourceNewer).to_owned(),
            std::cmp::Ordering::Less => t(super::SftpMsg::ConflictTargetNewer).to_owned(),
        });
    }
    if info.can_resume {
        hints.push(t(super::SftpMsg::ConflictResumeHint).to_owned());
    }
    (source, target, hints.join(" · "))
}

/// 还在等回答的一条提问：属于哪个任务、哪个批次，以及回答通道。
pub(super) struct PendingDecision {
    pub(super) job: TransferId,
    pub(super) batch: Option<u64>,
    pub(super) sender: oneshot::Sender<ConflictDecision>,
}

/// 还在等回答的提问：编号 → 提问。
pub(super) type PendingDecisions = HashMap<u64, PendingDecision>;

/// 「全部覆盖 / 全部跳过」按谁的范围算：有批次就是整个批次，没有批次（单独提交、重试过的任务）
/// 只是这个任务自己。
pub(super) fn same_scope(
    batch: Option<u64>,
    job: TransferId,
    other_batch: Option<u64>,
    other_job: TransferId,
) -> bool {
    match batch {
        Some(batch) => other_batch == Some(batch),
        None => other_batch.is_none() && other_job == job,
    }
}

/// 界面的同名文件回答者：`decide()` 把提问塞进窗口事件，然后等界面（或自动化钩子）
/// 通过 [`Self::respond`] / [`Self::respond_scope`] 回答。
pub(super) struct UiConflictResolver {
    /// 窗口事件通道。`Sender` 不是 `Sync`，用 `Mutex` 包一层；发一条就放开，
    /// 不和界面线程抢着等。
    pub(super) sender: Mutex<Sender<SftpEvent>>,
    /// 还在等回答的提问。界面关掉 / resolver 被丢掉时这些发送端一起释放，
    /// `decide()` 收到错误就按跳过收尾。
    pub(super) pending: Mutex<PendingDecisions>,
    /// 「全部覆盖 / 全部跳过」：**按批次**记住。引擎自己也在批次里记了一份，这里多记是为了
    /// 堵住"回答已发出、引擎还没来得及记住"的那一小段空档里到达的提问。批次里的任务都
    /// 结束后由界面清掉（[`Self::retain_batches`]），不会漏到下一次提交。
    pub(super) batch_all: Mutex<HashMap<u64, ConflictAction>>,
    /// `ZSSH_SFTP_CONFLICT`：有值就不弹面板，直接回答。
    pub(super) automation: Option<ConflictAutomation>,
    /// 窗口已经关了：之后来的冲突直接按跳过，不再等一个已经不存在的界面。
    pub(super) closed: AtomicBool,
    pub(super) next_id: AtomicU64,
}

impl UiConflictResolver {
    pub(super) fn new(sender: Sender<SftpEvent>, automation: Option<ConflictAutomation>) -> Self {
        Self {
            sender: Mutex::new(sender),
            pending: Mutex::new(HashMap::new()),
            batch_all: Mutex::new(HashMap::new()),
            automation,
            closed: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
        }
    }

    pub(super) fn lock_pending(&self) -> std::sync::MutexGuard<'_, PendingDecisions> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(super) fn lock_batch_all(&self) -> std::sync::MutexGuard<'_, HashMap<u64, ConflictAction>> {
        self.batch_all
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(super) fn batch_action(&self, batch: Option<u64>) -> Option<ConflictAction> {
        let batch = batch?;
        self.lock_batch_all().get(&batch).copied()
    }

    /// 回答一个提问。界面已经走了（这个编号不在等）时返回 `false`。
    pub(super) fn respond(&self, id: u64, decision: ConflictDecision) -> bool {
        match self.lock_pending().remove(&id) {
            Some(pending) => pending.sender.send(decision).is_ok(),
            None => false,
        }
    }

    /// 「全部覆盖 / 全部跳过」：回答**同一批次**里所有正在等的提问（没有批次就只是这个任务的），
    /// 并让这个批次后面的冲突也按它处理。别的批次 / 以后的提交不受影响。
    /// 只有覆盖 / 跳过能"全部应用"；重命名每次都要看具体文件，后续仍走自动起名。
    /// 返回回答了几条。
    pub(super) fn respond_scope(
        &self,
        action: ConflictAction,
        batch: Option<u64>,
        job: TransferId,
    ) -> usize {
        if matches!(action, ConflictAction::Overwrite | ConflictAction::Skip)
            && let Some(batch) = batch
        {
            self.lock_batch_all().insert(batch, action);
        }
        let answered: Vec<PendingDecision> = {
            let mut pending = self.lock_pending();
            let ids: Vec<u64> = pending
                .iter()
                .filter(|(_, p)| same_scope(batch, job, p.batch, p.job))
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter()
                .filter_map(|id| pending.remove(&id))
                .collect()
        };
        let count = answered.len();
        for pending in answered {
            let _ = pending.sender.send(ConflictDecision {
                action,
                apply_to_all: true,
                rename_to: None,
            });
        }
        count
    }

    /// 任务取消 / 结束了：它还在等的提问按跳过收尾，免得面板上留着一个没人处理的问题。
    pub(super) fn cancel_job(&self, job: TransferId) {
        let gone: Vec<PendingDecision> = {
            let mut pending = self.lock_pending();
            let ids: Vec<u64> = pending
                .iter()
                .filter(|(_, p)| p.job == job)
                .map(|(id, _)| *id)
                .collect();
            ids.into_iter()
                .filter_map(|id| pending.remove(&id))
                .collect()
        };
        for pending in gone {
            let _ = pending.sender.send(skip_decision());
        }
    }

    /// 只留还有任务在跑的批次的"全部应用"选择，其余作废。
    pub(super) fn retain_batches(&self, active: &std::collections::HashSet<u64>) {
        self.lock_batch_all()
            .retain(|batch, _| active.contains(batch));
    }

    /// 窗口关掉：正在等的按跳过收尾，之后来的冲突也不再等界面。
    pub(super) fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let gone: Vec<PendingDecision> = self.lock_pending().drain().map(|(_, p)| p).collect();
        for pending in gone {
            let _ = pending.sender.send(skip_decision());
        }
        self.lock_batch_all().clear();
    }
}

impl ConflictResolver for UiConflictResolver {
    fn decide<'a>(&'a self, info: ConflictInfo) -> BoxFuture<'a, ConflictDecision> {
        Box::pin(async move {
            // 窗口已经关了：不再走界面，也别让引擎停在"等待处理同名文件"上。
            if self.closed.load(Ordering::SeqCst) {
                return skip_decision();
            }
            if let Some(automation) = self.automation {
                return automation.decision();
            }
            if let Some(action) = self.batch_action(info.batch) {
                return ConflictDecision {
                    action,
                    apply_to_all: true,
                    rename_to: None,
                };
            }
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            let (sender, answer) = oneshot::channel();
            let (job, batch) = (info.job, info.batch);
            let sent = {
                let mut pending = self.lock_pending();
                // 先登记再发事件：界面收到事件时一定能回答到这个编号。
                pending.insert(id, PendingDecision { job, batch, sender });
                let sent = self
                    .sender
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .send(SftpEvent::Conflict { id, info })
                    .is_ok();
                if !sent {
                    pending.remove(&id);
                }
                sent
            };
            if !sent {
                // 窗口已经关了：别让引擎停在"等待处理同名文件"上。
                return skip_decision();
            }
            // 回答通道被丢掉（resolver 被丢）时同样按跳过收尾。
            answer.await.unwrap_or_else(|_| skip_decision())
        })
    }
}

/// 关窗口怎么处理：用户已经确认过，或者没有未完成的传输 / 删除，就关；否则先问。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CloseDecision {
    Close,
    Ask,
}

pub(super) fn close_decision(confirmed: bool, unfinished: usize) -> CloseDecision {
    if confirmed || unfinished == 0 {
        CloseDecision::Close
    } else {
        CloseDecision::Ask
    }
}

/// 目录列表的请求序号：每发起一次读取就取一个新号，结果回来时只有号码还是最新的才算数。
/// 慢的旧请求（大目录）晚于新请求返回时，不能盖掉用户已经看到的新目录。
#[derive(Debug, Default)]
pub(super) struct ListingGate {
    pub(super) latest: u64,
}

impl ListingGate {
    pub(super) fn next(&mut self) -> u64 {
        self.latest += 1;
        self.latest
    }

    pub(super) fn is_latest(&self, seq: u64) -> bool {
        seq == self.latest
    }
}

/// 正在跑的递归删除：界面显示进度，取消按钮和关窗口都靠 `cancel` 叫停。
pub(super) struct DeleteJob {
    pub(super) id: u64,
    pub(super) name: String,
    pub(super) cancel: CancelHandle,
    pub(super) deleted: u64,
    pub(super) current: String,
    pub(super) cancelling: bool,
}

/// 删除的结局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DeleteOutcome {
    Done(u64),
    /// 被取消：已经删掉的不回滚；`remaining` 是取消后数出来还剩多少项（数不出来是 `None`）。
    Cancelled {
        deleted: u64,
        remaining: Option<u64>,
    },
    Failed {
        deleted: u64,
        message: String,
    },
}

/// 结局 → 状态栏上的话。
pub(super) fn delete_outcome_text(name: &str, outcome: &DeleteOutcome) -> String {
    match outcome {
        DeleteOutcome::Done(count) if *count > 1 => format!(
            "{}{name}{}{count}{}",
            t(super::SftpMsg::DeleteDonePrefix),
            t(super::SftpMsg::DeleteCountOpen),
            t(super::SftpMsg::DeleteCountClose)
        ),
        DeleteOutcome::Done(_) => format!("{}{name}", t(super::SftpMsg::DeleteDonePrefix)),
        DeleteOutcome::Cancelled { deleted, remaining } => match remaining {
            Some(remaining) if *remaining >= REMAINING_COUNT_CAP => format!(
                "{}{name}{}{deleted}{}{remaining}{}",
                t(super::SftpMsg::DeleteCancelledPrefix),
                t(super::SftpMsg::DeleteProgressMiddle),
                t(super::SftpMsg::DeleteCancelAtLeastMiddle),
                t(super::SftpMsg::DeleteCancelExactSuffix)
            ),
            Some(remaining) => format!(
                "{}{name}{}{deleted}{}{remaining}{}",
                t(super::SftpMsg::DeleteCancelledPrefix),
                t(super::SftpMsg::DeleteProgressMiddle),
                t(super::SftpMsg::DeleteCancelExactMiddle),
                t(super::SftpMsg::DeleteCancelExactSuffix)
            ),
            None => format!(
                "{}{name}{}{deleted}{}",
                t(super::SftpMsg::DeleteCancelledPrefix),
                t(super::SftpMsg::DeleteProgressMiddle),
                t(super::SftpMsg::DeleteCancelUnknownSuffix)
            ),
        },
        DeleteOutcome::Failed { deleted, message } => format!(
            "{}{name}{}{deleted}{}{message}{}",
            t(super::SftpMsg::DeleteFailedPrefix),
            t(super::SftpMsg::DeleteFailedCountOpen),
            t(super::SftpMsg::DeleteFailedCountClose),
            t(super::SftpMsg::ParenClose)
        ),
    }
}

/// 跑一次递归删除并给出结局。文件和目录都走核心的 [`RecursiveDelete`]（遇到符号链接只删链接）；
/// 被取消时数一遍还剩多少项，让用户知道停在了哪。`on_progress` 每删一项调一次。
pub(super) async fn run_delete(
    channel: &SftpChannel,
    path: &str,
    deleter: &RecursiveDelete,
    mut on_progress: impl FnMut(zssh_core::sftp::DeleteProgress) + Send,
) -> DeleteOutcome {
    let deleted_so_far = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&deleted_so_far);
    let result = deleter
        .run(channel, path, |progress| {
            counter.store(progress.deleted, Ordering::SeqCst);
            on_progress(progress);
        })
        .await;
    let deleted = deleted_so_far.load(Ordering::SeqCst);
    match result {
        Ok(count) => DeleteOutcome::Done(count),
        Err(SftpError::Cancelled) => DeleteOutcome::Cancelled {
            deleted,
            remaining: count_remaining(channel, path).await.ok(),
        },
        Err(err) => DeleteOutcome::Failed {
            deleted,
            message: error_text(&err),
        },
    }
}

/// 一次删除多个路径（选中了好几项）：一项接一项走核心的递归删除，进度里的已删项数是累计的。
/// 取消 / 失败就停在当前项，没开始的路径不动；取消后数一遍还剩多少（当前项剩余 + 没开始的各项）。
pub(super) async fn run_delete_many(
    channel: &SftpChannel,
    paths: &[String],
    deleter: &RecursiveDelete,
    mut on_progress: impl FnMut(zssh_core::sftp::DeleteProgress) + Send,
) -> DeleteOutcome {
    let mut done_before = 0u64;
    for (index, path) in paths.iter().enumerate() {
        let base = done_before;
        let outcome = run_delete(channel, path, deleter, |progress| {
            on_progress(zssh_core::sftp::DeleteProgress {
                deleted: base + progress.deleted,
                current: progress.current,
            });
        })
        .await;
        match outcome {
            DeleteOutcome::Done(count) => done_before += count,
            DeleteOutcome::Cancelled { deleted, remaining } => {
                let mut left = remaining;
                for later in &paths[index + 1..] {
                    left = match (left, count_remaining(channel, later).await.ok()) {
                        (Some(a), Some(b)) => Some(a + b),
                        _ => None,
                    };
                }
                return DeleteOutcome::Cancelled {
                    deleted: base + deleted,
                    remaining: left,
                };
            }
            DeleteOutcome::Failed { deleted, message } => {
                return DeleteOutcome::Failed {
                    deleted: base + deleted,
                    message: if paths.len() > 1 {
                        format!(
                            "{message}{}{path}{}",
                            t(super::SftpMsg::DeleteNotRemovedMiddle),
                            t(super::SftpMsg::DeleteNotRemovedSuffix)
                        )
                    } else {
                        message
                    },
                };
            }
        }
    }
    DeleteOutcome::Done(done_before)
}

/// 数剩余项时最多数这么多，再多就只说"至少"。
pub(super) const REMAINING_COUNT_CAP: u64 = 100_000;

/// 取消后数一遍 `path` 下还剩多少项（目录算一项、符号链接算一项且不跟进去）。
/// 路径已经不存在算 0。数到上限就停，调用方显示"至少"。
pub(super) async fn count_remaining(channel: &SftpChannel, path: &str) -> Result<u64, SftpError> {
    let mut count = 0u64;
    let mut pending = vec![path.to_owned()];
    while let Some(dir) = pending.pop() {
        let entries = match channel.read_dir(&dir).await {
            Ok(entries) => entries,
            // 根本身已经不在了（或者本来就是个文件）：当作没有子项。
            Err(SftpError::NotFound { .. }) if dir == path => return Ok(0),
            Err(err) if dir == path => {
                // 路径是个文件：算一项；其它错误原样往上抛。
                return match fs::stat(channel, path, false).await {
                    Ok(info) if !info.is_dir() => Ok(1),
                    _ => Err(err),
                };
            }
            // 子目录读不了：跳过这一支，数目只会偏小。
            Err(_) => continue,
        };
        count += 1;
        for entry in entries {
            if entry.kind.is_dir() && !entry.kind.is_symlink() {
                pending.push(entry.path);
            } else {
                count += 1;
            }
            if count >= REMAINING_COUNT_CAP {
                return Ok(count);
            }
        }
    }
    Ok(count)
}

/// 主机名只用于标题和头部：有名字用名字，没有才用地址；读不到就退回编号，不影响浏览。
pub(super) fn host_label(host_id: i64) -> String {
    Database::open(&data_dir().join("zssh.db"))
        .ok()
        .and_then(|db| db.host_get(host_id).ok())
        .map(|host| super::header::display_name(&host.name, &host.host, host_id))
        .unwrap_or_else(|| format!("{}{host_id}", t(super::SftpMsg::HostFallback)))
}

/// 权限位 → `0644`（固定四位，去掉文件类型位）。
pub(super) fn format_mode(mode: u32) -> String {
    format!("{:04o}", mode & 0o777)
}

/// 权限输入：3-4 位八进制，四位时和网页端一样忽略第一位（setuid 等特殊位不在这里设）。
/// 空串、非八进制字符、位数不对都直接拒绝，不猜。
pub(super) fn parse_octal_mode(text: &str) -> Result<u32, String> {
    let trimmed = text.trim();
    let valid =
        (3..=4).contains(&trimmed.len()) && trimmed.bytes().all(|byte| matches!(byte, b'0'..=b'7'));
    if !valid {
        return Err(format!(
            "{}{trimmed:?}",
            t(super::SftpMsg::OctalErrorPrefix)
        ));
    }
    let digits = if trimmed.len() == 4 {
        &trimmed[1..]
    } else {
        trimmed
    };
    u32::from_str_radix(digits, 8)
        .map_err(|_| format!("{}{trimmed:?}", t(super::SftpMsg::OctalErrorPrefix)))
}

/// Unix 秒 → `YYYY-MM-DD HH:MM UTC`。只为了在属性行里看清时间，
/// 不引时间库（`u32` 的 mtime 落在这个范围内，不需要时区数据）。
pub(super) fn format_mtime(seconds: u32) -> String {
    let days = i64::from(seconds) / 86_400;
    let day_seconds = i64::from(seconds) % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = day_seconds / 3600;
    let minute = (day_seconds % 3600) / 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02} UTC")
}

/// 1970-01-01 起的天数 → 公历年月日（Howard Hinnant 的 civil_from_days）。
pub(super) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    (year, month as u32, day as u32)
}

pub(super) fn direction_label(direction: Direction) -> &'static str {
    match direction {
        Direction::Upload => t(super::SftpMsg::DirectionUpload),
        Direction::Download => t(super::SftpMsg::DirectionDownload),
        Direction::RemoteCopy => t(super::SftpMsg::DirectionRemoteCopy),
        Direction::LocalCopy => t(super::SftpMsg::DirectionLocalCopy),
    }
}

pub(super) fn state_label(state: &TransferState) -> String {
    let label = match state {
        TransferState::Queued => t(super::SftpMsg::StateQueued),
        TransferState::Planning => t(super::SftpMsg::StatePlanning),
        TransferState::Running => t(super::SftpMsg::StateRunning),
        TransferState::Paused => t(super::SftpMsg::StatePaused),
        TransferState::WaitingReconnect => t(super::SftpMsg::StateWaitingReconnect),
        TransferState::WaitingConflict => t(super::SftpMsg::StateWaitingConflict),
        TransferState::Done => t(super::SftpMsg::StateDone),
        TransferState::Failed(reason) => {
            return format!("{}{reason}", t(super::SftpMsg::StateFailedPrefix));
        }
        TransferState::Cancelled => t(super::SftpMsg::StateCancelled),
        TransferState::Skipped => t(super::SftpMsg::StateSkipped),
    };
    label.to_owned()
}

/// 能不能按"暂停"：除了已经暂停和终态，引擎都会接受（排队的任务会在开跑后停下，
/// 等待重连 / 等同名回答的任务会在回到传输循环时停下）。
pub(super) fn can_pause(state: &TransferState) -> bool {
    matches!(
        state,
        TransferState::Queued
            | TransferState::Planning
            | TransferState::Running
            | TransferState::WaitingReconnect
            | TransferState::WaitingConflict
    )
}

/// 上传落点：当前远端目录 + 本地文件名。Windows 的 `\` 先换成 `/` 再取最后一段。
pub(super) fn upload_target(cwd: &str, local_path: &str) -> Result<String, String> {
    let name = local_file_name(local_path);
    if name.is_empty() {
        return Err(format!(
            "{local_path:?}{}",
            t(super::SftpMsg::NoUsableFileName)
        ));
    }
    Ok(join(cwd, &name))
}

/// 整目录上传的远端落点：当前远端目录 + 本地目录名，和单文件的规则一致。
///
/// 名字里带 `:`、`/`、`\` 或被规范化成 `.` / `..` 的（比如盘符根 `C:\`）直接拒绝：
/// 否则 `join` 出来的目标会跑出远端当前目录。
pub(super) fn upload_dir_target(cwd: &str, local_dir: &str) -> Result<String, String> {
    let name = local_file_name(local_dir);
    if name.is_empty() || name == "." || name == ".." || name.contains([':', '\\', '/']) {
        return Err(format!(
            "{local_dir:?}{}",
            t(super::SftpMsg::NoUsableDirName)
        ));
    }
    Ok(join(cwd, &name))
}

pub(super) fn local_file_name(local_path: &str) -> String {
    file_name(&local_path.replace('\\', "/"))
}

/// 从系统拖进来的路径能怎么传。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DropKind {
    File,
    Dir,
    /// 不存在、读不到属性，或者不是普通文件 / 目录（设备、坏链接等）。
    Missing,
}

/// 判断拖入的路径：只认"确实是文件 / 确实是目录"，其余都算 `Missing`。
/// 跟随符号链接（和 `Path::is_file` / `is_dir` 一致），拖进来的链接指向目录就按目录传。
pub(super) fn classify_dropped_path(path: &Path) -> DropKind {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => DropKind::File,
        Ok(meta) if meta.is_dir() => DropKind::Dir,
        _ => DropKind::Missing,
    }
}

/// 新建目录 / 重命名的名字：只能是一级名字。名字里带 `/` 或 `\` 就等于换目录，
/// 建目录、改名都不许这么干；`.` / `..` 也不是能用的名字。
pub(super) fn valid_remote_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(t(super::SftpMsg::NameEmpty).to_owned());
    }
    if name.contains(['/', '\\']) {
        return Err(format!(
            "{}{name}",
            t(super::SftpMsg::NamePathSeparatorPrefix)
        ));
    }
    if name == "." || name == ".." {
        return Err(format!("{}{name}", t(super::SftpMsg::NameCannotBePrefix)));
    }
    Ok(name.to_owned())
}

/// 新建目录的落点：远端当前目录 + 名字。
pub(super) fn mkdir_target(cwd: &str, raw_name: &str) -> Result<String, String> {
    if cwd.trim().is_empty() {
        return Err(t(super::SftpMsg::RemoteDirNotReady).to_owned());
    }
    Ok(join(cwd, &valid_remote_name(raw_name)?))
}

/// 重命名的落点：原条目的父目录 + 新名字（改名不换目录）。
pub(super) fn rename_target(old_path: &str, raw_name: &str) -> Result<String, String> {
    let name = valid_remote_name(raw_name)?;
    Ok(join(&parent(old_path), &name))
}

/// 自动化钩子里的相对名字 → 远端路径。只接受当前目录下面的路径：
/// 绝对路径、借 `..` 跳到当前目录外面的都拒绝——钩子是省事用的，不能变成删错地方的工具。
pub(super) fn resolve_remote_child(cwd: &str, raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(t(super::SftpMsg::NameEmpty).to_owned());
    }
    if trimmed.starts_with('/') {
        return Err(format!(
            "{}{trimmed}",
            t(super::SftpMsg::OnlyRelativeNamesPrefix)
        ));
    }
    if cwd.trim().is_empty() {
        return Err(t(super::SftpMsg::RemoteDirNotReady).to_owned());
    }
    let target = normalize(&join(cwd, trimmed));
    match relative(cwd, &target) {
        Some(rest) if !rest.is_empty() => Ok(target),
        _ => Err(format!(
            "{trimmed}{}",
            t(super::SftpMsg::NotUnderCurrentDirSuffix)
        )),
    }
}

/// 建一个远端目录（当前目录下面的那一级）。
pub(super) async fn create_remote_dir(
    channel: &SftpChannel,
    target: &str,
) -> Result<String, String> {
    channel.create_dir(target).await.map_err(|err| {
        format!(
            "{}{target}{}{}{}",
            t(super::SftpMsg::MkdirFailedPrefix),
            t(super::SftpMsg::MkdirFailedMiddle),
            error_text(&err),
            t(super::SftpMsg::ParenClose)
        )
    })?;
    Ok(format!("{}{target}", t(super::SftpMsg::MkdirDonePrefix)))
}

/// 本地栏的初始目录：`ZSSH_SFTP_LOCAL` 指到哪就是哪（**必须已经存在**），
/// 否则 `%TEMP%\zssh-sftp-dl`（不存在就建）。两种落点都在临时区，不碰用户的家目录 /
/// 文档 / 桌面——自动化钩子默认就写这里。
pub(super) fn initial_local_dir() -> PathBuf {
    if let Some(dir) = env_value("ZSSH_SFTP_LOCAL").map(PathBuf::from)
        && dir.is_dir()
    {
        return dir;
    }
    let dir = default_download_dir();
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 不碰磁盘的“暂定落点”：`ZSSH_SFTP_LOCAL` 指到哪就是哪（还没验证），否则默认临时目录。
/// 视图创建时界面线程先用它显示路径，真正的落点由后台线程用 [`initial_local_dir`] 定。
pub(super) fn initial_local_candidate() -> PathBuf {
    env_value("ZSSH_SFTP_LOCAL")
        .map(PathBuf::from)
        .unwrap_or_else(default_download_dir)
}

/// 默认本地目录：`%TEMP%\zssh-sftp-dl`，不往用户的文档 / 桌面写东西。
pub(super) fn default_download_dir() -> PathBuf {
    std::env::temp_dir().join("zssh-sftp-dl")
}

/// 本地路径输入：绝对路径原样用，相对路径按本地栏的当前目录拼；空串给 `None`。
pub(super) fn resolve_local_path(cwd: &Path, typed: &str) -> Option<PathBuf> {
    let trimmed = typed.trim();
    if trimmed.is_empty() {
        return None;
    }
    let typed = PathBuf::from(trimmed);
    Some(if typed.is_absolute() {
        typed
    } else {
        cwd.join(typed)
    })
}

/// 显示用的绝对路径（`std::path::absolute` 不解 `..`，够用；失败就原样显示）。
pub(super) fn absolute_local(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// 自动化钩子：`ZSSH_SFTP_DOWNLOAD[_DIR]` / `ZSSH_SFTP_UPLOAD[_DIR]` /
/// `ZSSH_SFTP_MKDIR` / `ZSSH_SFTP_RENAME` / `ZSSH_SFTP_DELETE` / `ZSSH_SFTP_CHMOD` /
/// `ZSSH_SFTP_EDIT` 设了才生效。
/// 这些钩子是显式动作；传输遇到同名文件时由 `ZSSH_SFTP_CONFLICT` 或界面的同名文件面板回答。
pub(super) fn test_hooks() -> Vec<TestHook> {
    let mut hooks = Vec::new();
    if let Some(remote) = env_value("ZSSH_SFTP_DOWNLOAD") {
        hooks.push(TestHook::Download(remote));
    }
    if let Some(remote) = env_value("ZSSH_SFTP_DOWNLOAD_DIR") {
        hooks.push(TestHook::DownloadDir(remote));
    }
    if let Some(local) = env_value("ZSSH_SFTP_UPLOAD") {
        hooks.push(TestHook::Upload(local));
    }
    if let Some(local) = env_value("ZSSH_SFTP_UPLOAD_DIR") {
        hooks.push(TestHook::UploadDir(local));
    }
    if let Some(name) = env_value("ZSSH_SFTP_MKDIR") {
        hooks.push(TestHook::Mkdir(name));
    }
    if let Some(value) = env_value("ZSSH_SFTP_RENAME") {
        hooks.push(match parse_rename_hook(&value) {
            Ok((old, new)) => TestHook::Rename { old, new },
            Err(message) => TestHook::Invalid(message),
        });
    }
    if let Some(name) = env_value("ZSSH_SFTP_DELETE") {
        hooks.push(TestHook::Delete(name));
    }
    // 排在 mkdir / rename / delete 后面：钩子按数组顺序进队列，前一个结束才跑下一个。
    if let Some(value) = env_value("ZSSH_SFTP_CHMOD") {
        hooks.push(match parse_chmod_hook(&value) {
            Ok((name, mode)) => TestHook::Chmod { name, mode },
            Err(message) => TestHook::Invalid(message),
        });
    }
    if let Some(path) = env_value("ZSSH_SFTP_DROP") {
        hooks.push(TestHook::Drop(path));
    }
    // 最后打开：编辑窗口是独立窗口，前面的钩子先排上队。
    if let Some(name) = env_value("ZSSH_SFTP_EDIT") {
        hooks.push(TestHook::Edit(name));
    }
    hooks
}

/// `旧名:新名`：按第一个冒号切，新名字里可以再有冒号。
pub(super) fn parse_rename_hook(value: &str) -> Result<(String, String), String> {
    let Some((old, new)) = value.split_once(':') else {
        return Err(format!(
            "{}{value:?}",
            t(super::SftpMsg::HookRenameFormatPrefix)
        ));
    };
    let (old, new) = (old.trim(), new.trim());
    if old.is_empty() || new.is_empty() {
        return Err(format!(
            "{}{value:?}",
            t(super::SftpMsg::HookRenameNameEmptyPrefix)
        ));
    }
    Ok((old.to_owned(), new.to_owned()))
}

/// `名字:权限`：按第一个冒号切，名字是当前目录下面的相对路径。
pub(super) fn parse_chmod_hook(value: &str) -> Result<(String, u32), String> {
    let Some((name, mode)) = value.split_once(':') else {
        return Err(format!(
            "{}{value:?}",
            t(super::SftpMsg::HookChmodFormatPrefix)
        ));
    };
    let name = name.trim();
    if name.is_empty() {
        return Err(format!(
            "{}{value:?}",
            t(super::SftpMsg::HookChmodNameEmptyPrefix)
        ));
    }
    let mode = parse_octal_mode(mode).map_err(|err| {
        format!(
            "{}{value:?}{}{err}{}",
            t(super::SftpMsg::HookChmodModeInvalidPrefix),
            t(super::SftpMsg::MkdirFailedMiddle),
            t(super::SftpMsg::ParenClose)
        )
    })?;
    Ok((name.to_owned(), mode))
}

pub(super) fn env_value(name: &str) -> Option<String> {
    crate::dev_hooks::var(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// `ZSSH_SFTP_CONFLICT=overwrite|skip|rename|overwrite-all|skip-all`：
/// 自动化测试里让 resolver 直接回答，不弹同名文件面板。没设置 = 界面面板。
pub(super) fn conflict_automation() -> (Option<ConflictAutomation>, Option<String>) {
    match env_value("ZSSH_SFTP_CONFLICT") {
        Some(value) => match parse_conflict_hook(&value) {
            Ok(automation) => (Some(automation), None),
            Err(message) => (None, Some(message)),
        },
        None => (None, None),
    }
}

/// 解析 `ZSSH_SFTP_CONFLICT`。大小写不敏感；值写错时返回错误，由调用方按界面处理。
pub(super) fn parse_conflict_hook(value: &str) -> Result<ConflictAutomation, String> {
    let trimmed = value.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "overwrite" => Ok(ConflictAutomation::Overwrite),
        "skip" => Ok(ConflictAutomation::Skip),
        "rename" => Ok(ConflictAutomation::Rename),
        "overwrite-all" => Ok(ConflictAutomation::OverwriteAll),
        "skip-all" => Ok(ConflictAutomation::SkipAll),
        _ => Err(format!(
            "{}{trimmed:?}",
            t(super::SftpMsg::HookConflictInvalidPrefix)
        )),
    }
}

/// `ZSSH_SFTP_PAUSE_MS=<n>`：Ready 之后 n 毫秒暂停所有可暂停的任务，2 秒后继续。
/// 只给手动 / 大文件测试用；没设置或者值不合法都当没设置（不猜用户想干什么）。
pub(super) fn pause_hook_ms() -> Option<u64> {
    parse_pause_hook_ms(env_value("ZSSH_SFTP_PAUSE_MS").as_deref())
}

pub(super) fn parse_pause_hook_ms(value: Option<&str>) -> Option<u64> {
    value?.trim().parse::<u64>().ok().filter(|ms| *ms > 0)
}
