//! 粘贴的执行（0.2.14 `src-tauri/src/commands/sftp_paste.rs`）。
//!
//! 远端按顺序选办法：
//! 1. 同一台主机且服务器声明了 `copy-data`：服务器自己拷；
//! 2. 同一台主机：后台 exec 通道里 `cp -a`；`cp` 跑了但失败**不会**自动换办法（界面给"改为经本机
//!    中转重试"），只有命令根本没法用（没有 cp / 不让开 exec）才往下走；
//! 3. 经本机中转：进传输队列（`Direction::RemoteCopy`，源可以在另一台主机上）。
//!
//! 本地只有本地 → 本地（`Direction::LocalCopy`，进队列）；远端和本地之间不能粘贴，请用上传 / 下载。
//!
//! 冲突在动手之前就决定好：服务器端复制一旦开始就不能中途问用户。进队列的复制，同名文件的冲突由
//! 引擎逐个文件问；只有"粘贴到源所在的目录"（名字必然撞上源自己）才先在这里问一次，建议名是
//! 「名字 - 副本」。整次粘贴共用一个批次，"全部应用"对整次粘贴生效。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zssh_core::ai::shell_kind::ShellKind;
use zssh_core::pathnorm::key as pathnorm_key;
use zssh_core::sftp::path as remote_path;
use zssh_core::sftp::server_copy::{
    CopyConflictDecision, CopyConflictInfo, CopyConflictResolver, CopyMethod, CopyOptions,
    CopyProgress, CpFailure, choose_server_copy, copy_with_copy_data, copy_with_cp,
    ensure_folder_not_inside, probe_remote_shell, shell_kind_label, supports_copy_data,
};
use zssh_core::sftp::transfer::backend::{SftpEndpoints, TransferChannelProvider};
use zssh_core::sftp::transfer::{
    ConflictAction, ConflictBatch, ConflictDecision, ConflictInfo, ConflictResolver, Direction,
    TransferEngine, TransferJob, copy_path, with_file_name,
};
use zssh_core::sftp::{SftpChannel, SftpError};
use zssh_core::ssh::{BoxFuture, Connection};

use super::SftpEvent;
use super::SftpMsg;
use super::clip::{
    ClipItem, ClipSource, ItemResult, ItemState, Method, Outcome, first_free_copy_path, name_of,
    refuse_into_self, remote_dest,
};
use super::engine::UiConflictResolver;
use super::menu::Side;
use crate::i18n::t;

/// 服务器上的 `cp` 最多等多久（大目录可能要几分钟；界面显示"服务器上复制中"和已用时间）。
const CP_WAIT: Duration = Duration::from_secs(60 * 60);

/// 一次粘贴。
#[derive(Clone)]
pub(super) struct PasteRequest {
    /// 这一次粘贴的编号（界面用它认领进度和结果）。
    pub(super) op: u64,
    /// 用户点了"改为经本机中转重试"：这次不用服务器端复制。
    pub(super) force_relay: bool,
    pub(super) side: Side,
    pub(super) items: Vec<ClipItem>,
    pub(super) target_dir: String,
    pub(super) source: ClipSource,
    /// 发起粘贴的视图：源就是这个视图时不用登记对端。
    pub(super) this_view: u64,
}

/// 执行粘贴需要的、属于目标视图的那些东西（都是 `Arc`，可以搬到后台任务里）。
#[derive(Clone)]
pub(super) struct PasteCtx {
    pub(super) browse: Arc<SftpChannel>,
    pub(super) connection: Arc<Connection>,
    pub(super) engine: Arc<TransferEngine>,
    pub(super) endpoints: Arc<SftpEndpoints>,
    pub(super) resolver: Arc<UiConflictResolver>,
    pub(super) cancel: Arc<AtomicBool>,
    pub(super) sender: Sender<SftpEvent>,
    /// 登录 shell 的类型：第一次探测，之后用缓存（视图级）。
    pub(super) shell_kind: Arc<Mutex<Option<ShellKind>>>,
    /// 把服务器当作没有 `copy-data`（只在调试构建的 `ZSSH_PASTE_NO_COPY_DATA` 钩子下为真）：
    /// 新版 OpenSSH 都有 copy-data，不关掉它就没法在真实服务器上走 `cp` 这一条路。
    pub(super) no_copy_data: bool,
}

impl PasteCtx {
    /// 服务器能不能用 `copy-data` 做服务器端复制。
    fn has_copy_data(&self) -> bool {
        !self.no_copy_data && supports_copy_data(self.browse.caps())
    }
}

/// 粘贴时提交进引擎的一个任务（界面据此建传输队列里的行）。
#[derive(Debug, Clone)]
pub(super) struct PastedJob {
    pub(super) id: u64,
    pub(super) name: String,
    pub(super) direction: Direction,
    pub(super) batch: Option<u64>,
    pub(super) source: String,
    pub(super) target: String,
}

/// 登记在引擎里的对端名字：每个视图一个。
fn peer_name(view: u64) -> String {
    format!("view-{view}")
}

enum Dest {
    /// 目标是这个路径，现在不存在（或者是要覆盖的文件）。
    Free(String),
    /// 合并进已经存在的文件夹。
    Merge(String),
    Skip,
}

/// 一次粘贴里"先在这里问"的冲突用的状态：和这次粘贴提交的传输任务共用同一个批次。
struct ConflictState {
    batch: Arc<ConflictBatch>,
}

pub(super) async fn run_paste(
    ctx: PasteCtx,
    request: PasteRequest,
) -> Result<(Outcome, Vec<PastedJob>), String> {
    if request.items.is_empty() {
        return Err(t(SftpMsg::MenuClipboardEmpty).to_owned());
    }
    if request.target_dir.trim().is_empty() {
        return Err(t(SftpMsg::PasteNoTargetFolder).to_owned());
    }
    let started = Instant::now();
    let (mut outcome, jobs) = match request.side {
        Side::Remote => paste_remote(&ctx, &request).await?,
        Side::Local => paste_local(&ctx.engine, &ctx.resolver, &request).await?,
    };
    outcome.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok((outcome, jobs))
}

// ---------------------------------------------------------------------------
// 远端
// ---------------------------------------------------------------------------

/// 合并进已有文件夹时，同名文件逐个问。顶层的「覆盖」只表示"合并这个文件夹"，里面的同名文件仍然
/// 逐个问；用户勾了"对后续都这样处理"之后，后面的同名文件按记住的动作处理（和传输引擎一致）。
/// "记住"放在这次粘贴的批次里：合并多个文件夹时第二个文件夹不会再从头问。
struct MergeConflictBridge<'a> {
    resolver: &'a UiConflictResolver,
    browse: &'a SftpChannel,
    batch: &'a Arc<ConflictBatch>,
}

impl MergeConflictBridge<'_> {
    async fn rename_target(
        &self,
        target: &str,
        rename_to: Option<&str>,
        from_memory: bool,
    ) -> Result<String, String> {
        let name = if from_memory {
            None
        } else {
            rename_to.map(str::trim).filter(|name| !name.is_empty())
        };
        let chosen = match name {
            Some(name) => {
                if name.contains('/') {
                    return Err(t(SftpMsg::PasteRenameEmpty).to_owned());
                }
                with_file_name(target, name)
            }
            None => {
                let mut index = 1;
                loop {
                    let candidate = copy_path(target, index);
                    if self.browse.lstat(&candidate).await.is_err() {
                        break candidate;
                    }
                    index += 1;
                }
            }
        };
        if self.browse.lstat(&chosen).await.is_ok() {
            return Err(format!(
                "{}{}{}",
                t(SftpMsg::PasteAlreadyExistsPrefix),
                name_of(&chosen),
                t(SftpMsg::PasteAlreadyExistsSuffix)
            ));
        }
        Ok(chosen)
    }
}

impl CopyConflictResolver for MergeConflictBridge<'_> {
    fn decide<'a>(
        &'a self,
        info: CopyConflictInfo,
    ) -> BoxFuture<'a, Result<CopyConflictDecision, SftpError>> {
        Box::pin(async move {
            let remembered = self.batch.action();
            let decision = match remembered {
                Some(action) => ConflictDecision {
                    action,
                    apply_to_all: true,
                    rename_to: None,
                },
                None => {
                    let suggested_target =
                        suggest_remote_copy_name(self.browse, &info.target).await;
                    self.resolver
                        .decide(ConflictInfo {
                            job: 0,
                            direction: Direction::RemoteCopy,
                            source: info.source.clone(),
                            target: info.target.clone(),
                            source_size: info.source_size,
                            target_size: info.target_size,
                            source_mtime: info.source_mtime,
                            target_mtime: info.target_mtime,
                            can_resume: false,
                            suggested_target,
                            batch: Some(self.batch.id()),
                        })
                        .await
                }
            };
            if decision.apply_to_all {
                self.batch.remember(decision.action);
            }
            match decision.action {
                ConflictAction::Skip => Ok(CopyConflictDecision::Skip),
                ConflictAction::Overwrite | ConflictAction::Resume => {
                    Ok(CopyConflictDecision::Overwrite)
                }
                ConflictAction::Rename => {
                    let chosen = self
                        .rename_target(
                            &info.target,
                            decision.rename_to.as_deref(),
                            remembered.is_some(),
                        )
                        .await
                        .map_err(SftpError::Remote)?;
                    Ok(CopyConflictDecision::Rename(chosen))
                }
            }
        })
    }
}

/// 冲突对话框里预填的远端副本名：第一个没被占用的「名字 - 副本」「名字 - 副本 2」…
/// 只是建议——查不了（出错）或者试了 1000 个还被占着，就退回固定的「名字 - 副本」，
/// 不能因为这个卡住提问。
async fn suggest_remote_copy_name(browse: &SftpChannel, target: &str) -> String {
    for index in 1..=1000 {
        let candidate = copy_path(target, index);
        match browse.lstat(&candidate).await {
            Ok(_) => {}
            Err(SftpError::NotFound { .. }) => return candidate,
            Err(_) => break,
        }
    }
    copy_path(target, 1)
}

/// 远端登录 shell 的类型：第一次探测，之后用缓存。认不出（Windows、乱码、超时）当作 `Other`。
async fn remote_shell_kind(ctx: &PasteCtx) -> ShellKind {
    if let Some(kind) = *ctx
        .shell_kind
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
    {
        return kind;
    }
    let kind = probe_remote_shell(&ctx.connection)
        .await
        .unwrap_or(ShellKind::Other);
    *ctx.shell_kind
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(kind);
    kind
}

fn method_of(method: CopyMethod) -> Method {
    match method {
        CopyMethod::CopyData => Method::CopyData,
        CopyMethod::ServerCp => Method::ServerCp,
        CopyMethod::Relay => Method::Relay,
    }
}

/// 把任务排进引擎（`submit_in_batch` 内部 `tokio::spawn`，这里本来就在 runtime 上）。
fn submit(
    engine: &Arc<TransferEngine>,
    batch: &Arc<ConflictBatch>,
    job: TransferJob,
    jobs: &mut Vec<PastedJob>,
) -> u64 {
    let id = engine.submit_in_batch(job.clone(), batch);
    jobs.push(PastedJob {
        id,
        name: name_of(&job.source),
        direction: job.direction,
        batch: Some(batch.id()),
        source: job.source,
        target: job.target,
    });
    id
}

#[allow(clippy::too_many_lines)]
async fn paste_remote(
    ctx: &PasteCtx,
    request: &PasteRequest,
) -> Result<(Outcome, Vec<PastedJob>), String> {
    let other_view = request.source.view != request.this_view;
    // 源在别的视图里：那个视图关了（通道提供者没人持有了）就没法复制。
    let source_provider = match (&request.source.provider, other_view) {
        (Some(weak), true) => Some(
            weak.upgrade()
                .ok_or_else(|| t(SftpMsg::PasteSourceClosed).to_owned())?,
        ),
        (None, true) => {
            return Err(t(SftpMsg::PasteSourceClosed).to_owned());
        }
        _ => None,
    };
    let same_host = request
        .source
        .server_key
        .as_deref()
        .is_none_or(|key| key == ctx.browse.server_key());

    let target_attrs = ctx
        .browse
        .stat(&request.target_dir)
        .await
        .map_err(|err| format!("{}{err}", t(SftpMsg::PasteTargetUnavailablePrefix)))?;
    if !target_attrs.is_dir() {
        return Err(format!(
            "{}{}",
            request.target_dir,
            t(SftpMsg::PasteNotAFolderSuffix)
        ));
    }
    if same_host {
        for item in &request.items {
            if let Some(reason) = refuse_into_self(true, item, &request.target_dir) {
                return Err(reason);
            }
            // 字符串比较看不出符号链接别名（目标文件夹可能指回源树里）：再按服务器上的真实路径
            // 核对一遍；中转、copy-data、cp 三条路都在这之后。
            if item.is_dir {
                ensure_folder_not_inside(ctx.browse.as_ref(), &item.path, &request.target_dir)
                    .await
                    .map_err(|err| match err {
                        SftpError::InvalidInput(_) => format!(
                            "{}{}{}",
                            t(SftpMsg::ClipIntoSelfPrefix),
                            name_of(&item.path),
                            t(SftpMsg::PasteIntoSelfLinkSuffix)
                        ),
                        other => other.to_string(),
                    })?;
            }
        }
    }

    // 这一次粘贴的冲突批次：先在这里问的和提交给引擎的任务共用，"全部应用"对整次粘贴生效。
    let batch = ctx.engine.new_batch();
    let mut conflicts = ConflictState {
        batch: Arc::clone(&batch),
    };
    let mut results: Vec<ItemResult> = Vec::new();
    let mut jobs: Vec<PastedJob> = Vec::new();
    // 这一次粘贴实际用的办法（第一个条目决定，后面的沿用）。
    let mut method: Option<CopyMethod> = request.force_relay.then_some(CopyMethod::Relay);
    // 临时文件名里的任务标识：一次粘贴一个，不会和别的任务撞。
    let owner = format!(
        "p{:x}{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );
    let op = request.op;
    let progress_sender = ctx.sender.clone();
    // 进度：界面显示"第 N 个文件"。
    let on_progress = move |progress: CopyProgress| {
        let _ = progress_sender.send(SftpEvent::PasteProgress {
            op,
            files_done: progress.files_done,
        });
    };
    let server_key = ctx.browse.server_key().to_owned();
    let copy_options = CopyOptions {
        owner: &owner,
        server: &server_key,
        cancel: ctx.cancel.as_ref(),
        progress: Some(&on_progress),
        conflict: None,
    };

    // 源在另一台主机：把它的数据面登记给这边的引擎。
    if !same_host && let Some(provider) = &source_provider {
        let provider: Arc<dyn TransferChannelProvider> = provider.clone();
        ctx.endpoints
            .register_peer(&peer_name(request.source.view), provider);
    }
    let peer = (!same_host).then(|| peer_name(request.source.view));
    let relay_job = |item: &ClipItem, dest: &str| TransferJob {
        direction: Direction::RemoteCopy,
        source: item.path.clone(),
        target: dest.to_owned(),
        recursive: true,
        peer: peer.clone(),
    };

    for item in &request.items {
        // 已经取消了：后面的条目都不再开始（已经开始的在下面按自己的错误收尾）。
        if ctx.cancel.load(Ordering::SeqCst) {
            results.push(ItemResult::new(item, ItemState::Cancelled, None));
            continue;
        }
        let dest = remote_dest(&request.target_dir, item);
        let same_entry =
            same_host && remote_path::normalize(&dest) == remote_path::normalize(&item.path);

        // 经本机中转的复制，冲突交给引擎逐个文件问；其余情况在这里先决定好。
        let relay_possible = !same_host || method == Some(CopyMethod::Relay) || request.force_relay;
        let needs_precheck = !relay_possible || same_entry;
        let dest = if needs_precheck {
            match resolve_remote_dest(ctx, &ctx.resolver, item, dest, same_entry, &mut conflicts)
                .await
            {
                Ok(Dest::Skip) => {
                    results.push(ItemResult::new(item, ItemState::Skipped, None));
                    continue;
                }
                // Merge 的信息在后面按"目标是不是已存在的目录"重新判断。
                Ok(Dest::Free(path)) | Ok(Dest::Merge(path)) => path,
                Err(message) => {
                    results.push(ItemResult::new(item, ItemState::Failed, Some(message)));
                    continue;
                }
            }
        } else {
            dest
        };

        // 源在另一台主机，或者已经决定用中转：数据经过本机，进传输队列。
        if !same_host || method == Some(CopyMethod::Relay) {
            if !same_host {
                method = Some(CopyMethod::Relay);
            }
            submit(&ctx.engine, &batch, relay_job(item, &dest), &mut jobs);
            results.push(ItemResult::new(item, ItemState::Queued, None));
            continue;
        }

        // 合并进已有文件夹：里面同名的文件要逐个问。cp 没法逐个问，有 copy-data 就用它；没有就
        // 经本机中转。
        let merging = item.is_dir
            && ctx
                .browse
                .lstat(&dest)
                .await
                .map(|attrs| attrs.is_dir())
                .unwrap_or(false);
        if merging && !ctx.has_copy_data() {
            method = Some(CopyMethod::Relay);
            submit(&ctx.engine, &batch, relay_job(item, &dest), &mut jobs);
            results.push(ItemResult::new(
                item,
                ItemState::Queued,
                Some(t(SftpMsg::PasteMergeNeedsRelay).to_owned()),
            ));
            continue;
        }

        // 没有 copy-data 时先确认远端是 POSIX shell 才用 cp；不是就经本机中转。探测结果缓存，
        // 后面的条目不用再问。
        let chosen = match method {
            Some(method) => method,
            None => {
                let has_copy_data = ctx.has_copy_data();
                let shell = if has_copy_data || request.force_relay {
                    None
                } else {
                    Some(remote_shell_kind(ctx).await)
                };
                let chosen = choose_server_copy(has_copy_data, request.force_relay, shell);
                if chosen == CopyMethod::Relay {
                    let reason = if request.force_relay {
                        t(SftpMsg::PasteAsYouChose).to_owned()
                    } else {
                        format!(
                            "{}{}{}",
                            t(SftpMsg::PasteShellNotPosixPrefix),
                            shell_kind_label(shell),
                            t(SftpMsg::ParenClose)
                        )
                    };
                    method = Some(CopyMethod::Relay);
                    submit(&ctx.engine, &batch, relay_job(item, &dest), &mut jobs);
                    results.push(ItemResult::new(
                        item,
                        ItemState::Queued,
                        Some(format!(
                            "{reason}{}",
                            t(SftpMsg::PasteRelaySuffix)
                        )),
                    ));
                    continue;
                }
                chosen
            }
        };
        match chosen {
            CopyMethod::CopyData => {
                let merge_bridge = MergeConflictBridge {
                    resolver: &ctx.resolver,
                    browse: ctx.browse.as_ref(),
                    batch: &batch,
                };
                let mut item_options = CopyOptions {
                    owner: &owner,
                    server: &server_key,
                    cancel: ctx.cancel.as_ref(),
                    progress: Some(&on_progress),
                    conflict: None,
                };
                if merging {
                    item_options.conflict = Some(&merge_bridge);
                }
                method = Some(CopyMethod::CopyData);
                match copy_with_copy_data(ctx.browse.as_ref(), &item.path, &dest, &item_options)
                    .await
                {
                    Ok(stats) => results.push(ItemResult::new(
                        item,
                        ItemState::Ok,
                        (!stats.skipped_links.is_empty())
                            .then(|| format!(
                            "{}{}{}",
                            t(SftpMsg::PasteSkippedLinksPrefix),
                            stats.skipped_links.len(),
                            t(SftpMsg::PasteSkippedLinksSuffix)
                        )),
                    )),
                    Err(SftpError::Cancelled) => {
                        results.push(ItemResult::new(item, ItemState::Cancelled, None))
                    }
                    Err(err) => results.push(ItemResult::new(
                        item,
                        ItemState::Failed,
                        Some(format!(
                            "{}{err}",
                            t(SftpMsg::PasteServerCopyFailedPrefix)
                        )),
                    )),
                }
            }
            CopyMethod::ServerCp => {
                match copy_with_cp(
                    ctx.connection.as_ref(),
                    ctx.browse.as_ref(),
                    &item.path,
                    &dest,
                    CP_WAIT,
                    &copy_options,
                )
                .await
                {
                    Ok(_) => {
                        method = Some(CopyMethod::ServerCp);
                        results.push(ItemResult::new(item, ItemState::Ok, None));
                    }
                    Err(CpFailure::NotAvailable(reason)) => {
                        // 命令没法用：改走中转（这是"用不了"，不是"做失败了"）。
                        tracing::info!(%reason, "服务器上没法用 cp，改经本机中转");
                        method = Some(CopyMethod::Relay);
                        submit(&ctx.engine, &batch, relay_job(item, &dest), &mut jobs);
                        results.push(ItemResult::new(
                            item,
                            ItemState::Queued,
                            Some(format!(
                            "{reason}{}",
                            t(SftpMsg::PasteRelaySuffix)
                        )),
                        ));
                    }
                    Err(CpFailure::Failed(message)) => {
                        method = Some(CopyMethod::ServerCp);
                        results.push(ItemResult::failed_relayable(item, message));
                    }
                    Err(CpFailure::Cancelled) => {
                        method = Some(CopyMethod::ServerCp);
                        results.push(ItemResult::new(item, ItemState::Cancelled, None));
                    }
                }
            }
            CopyMethod::Relay => unreachable!("中转在上面单独处理"),
        }
    }

    Ok((
        Outcome {
            method: method_of(method.unwrap_or(CopyMethod::Relay)),
            items: results,
            elapsed_ms: 0,
        },
        jobs,
    ))
}

async fn resolve_remote_dest(
    ctx: &PasteCtx,
    resolver: &UiConflictResolver,
    item: &ClipItem,
    dest: String,
    same_entry: bool,
    state: &mut ConflictState,
) -> Result<Dest, String> {
    let existing = match ctx.browse.lstat(&dest).await {
        Ok(attrs) => attrs,
        Err(_) => return Ok(Dest::Free(dest)),
    };
    let source_attrs = ctx.browse.lstat(&item.path).await.ok();
    let suggested_target = suggest_remote_copy_name(&ctx.browse, &dest).await;
    let decision = ask(
        resolver,
        state,
        ConflictInfo {
            job: 0,
            direction: Direction::RemoteCopy,
            source: item.path.clone(),
            target: dest.clone(),
            source_size: source_attrs.as_ref().and_then(|attrs| attrs.size),
            target_size: existing.size,
            source_mtime: source_attrs.as_ref().and_then(|attrs| attrs.mtime),
            target_mtime: existing.mtime,
            can_resume: false,
            suggested_target,
            batch: Some(state.batch.id()),
        },
        same_entry,
    )
    .await;
    match decision.action {
        ConflictAction::Skip => Ok(Dest::Skip),
        ConflictAction::Rename => {
            let chosen = match decision.rename_to {
                Some(name) => {
                    if name.contains('/') || name.trim().is_empty() {
                        return Err(t(SftpMsg::PasteRenameEmpty).to_owned());
                    }
                    with_file_name(&dest, &name)
                }
                None => {
                    let mut index = 1;
                    let mut candidate = copy_path(&dest, 1);
                    while ctx.browse.lstat(&candidate).await.is_ok() {
                        index += 1;
                        candidate = copy_path(&dest, index);
                    }
                    candidate
                }
            };
            if ctx.browse.lstat(&chosen).await.is_ok() {
                return Err(format!(
                "{}{}{}",
                t(SftpMsg::PasteAlreadyExistsPrefix),
                name_of(&chosen),
                t(SftpMsg::PasteAlreadyExistsSuffix)
            ));
            }
            Ok(Dest::Free(chosen))
        }
        ConflictAction::Overwrite | ConflictAction::Resume => {
            if same_entry {
                return Err(t(SftpMsg::PasteOverwriteSelf).to_owned());
            }
            if existing.is_dir() != item.is_dir {
                return Err(if existing.is_dir() {
                    t(SftpMsg::PasteTargetIsFolder).to_owned()
                } else {
                    t(SftpMsg::PasteTargetIsFile).to_owned()
                });
            }
            if existing.is_dir() {
                Ok(Dest::Merge(dest))
            } else {
                Ok(Dest::Free(dest))
            }
        }
    }
}

/// 问一次冲突（"对后续都这样处理"记在 `state` 里；覆盖自己这种不能记）。
async fn ask(
    resolver: &dyn ConflictResolver,
    state: &mut ConflictState,
    info: ConflictInfo,
    same_entry: bool,
) -> ConflictDecision {
    if let Some(action) = state.batch.action()
        && !(same_entry && action == ConflictAction::Overwrite)
    {
        return ConflictDecision {
            action,
            apply_to_all: true,
            rename_to: None,
        };
    }
    let decision = resolver.decide(info).await;
    // "对后续都这样处理"；但「覆盖」不能因为这一项是自己覆盖自己就记下来。
    if decision.apply_to_all && !(same_entry && decision.action == ConflictAction::Overwrite) {
        state.batch.remember(decision.action);
    }
    decision
}

// ---------------------------------------------------------------------------
// 本地
// ---------------------------------------------------------------------------

pub(super) async fn paste_local(
    engine: &Arc<TransferEngine>,
    resolver: &UiConflictResolver,
    request: &PasteRequest,
) -> Result<(Outcome, Vec<PastedJob>), String> {
    let target_dir = PathBuf::from(&request.target_dir);
    if !target_dir.is_dir() {
        return Err(format!(
            "{}{}",
            request.target_dir,
            t(SftpMsg::PasteNotAFolderSuffix)
        ));
    }
    for item in &request.items {
        if let Some(reason) = refuse_into_self(false, item, &request.target_dir) {
            return Err(reason);
        }
    }
    let batch = engine.new_batch();
    let mut conflicts = ConflictState {
        batch: Arc::clone(&batch),
    };
    let mut results = Vec::new();
    let mut jobs = Vec::new();

    for item in &request.items {
        let name = name_of(&item.path);
        let dest = target_dir.join(&name);
        let same_entry = pathnorm_key(&dest) == pathnorm_key(Path::new(&item.path));
        // 本地没有"服务器端"，复制全进传输队列：同名文件的冲突由引擎逐个文件问；
        // "粘贴到自己所在的文件夹"在这里先决定名字。
        let dest = if same_entry {
            match resolve_local_dest(resolver, item, dest, same_entry, &mut conflicts).await {
                Ok(Some(path)) => path,
                Ok(None) => {
                    results.push(ItemResult::new(item, ItemState::Skipped, None));
                    continue;
                }
                Err(message) => {
                    results.push(ItemResult::new(item, ItemState::Failed, Some(message)));
                    continue;
                }
            }
        } else {
            dest
        };
        submit(
            engine,
            &batch,
            TransferJob {
                direction: Direction::LocalCopy,
                source: item.path.clone(),
                target: dest.display().to_string(),
                recursive: true,
                peer: None,
            },
            &mut jobs,
        );
        results.push(ItemResult::new(item, ItemState::Queued, None));
    }

    Ok((
        Outcome {
            method: Method::Local,
            items: results,
            elapsed_ms: 0,
        },
        jobs,
    ))
}

async fn resolve_local_dest(
    resolver: &UiConflictResolver,
    item: &ClipItem,
    dest: PathBuf,
    same_entry: bool,
    state: &mut ConflictState,
) -> Result<Option<PathBuf>, String> {
    let Ok(existing) = std::fs::symlink_metadata(&dest) else {
        return Ok(Some(dest));
    };
    let source_meta = std::fs::symlink_metadata(&item.path).ok();
    let mtime = |meta: &std::fs::Metadata| {
        meta.modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|elapsed| u32::try_from(elapsed.as_secs()).ok())
    };
    let suggested_target = first_free_copy_path(&dest.display().to_string(), |candidate| {
        std::fs::symlink_metadata(candidate).is_ok()
    });
    let decision = ask(
        resolver,
        state,
        ConflictInfo {
            job: 0,
            direction: Direction::LocalCopy,
            source: item.path.clone(),
            target: dest.display().to_string(),
            source_size: source_meta.as_ref().map(|meta| meta.len()),
            target_size: Some(existing.len()),
            source_mtime: source_meta.as_ref().and_then(mtime),
            target_mtime: mtime(&existing),
            can_resume: false,
            suggested_target,
            batch: Some(state.batch.id()),
        },
        same_entry,
    )
    .await;
    match decision.action {
        ConflictAction::Skip => Ok(None),
        ConflictAction::Rename => {
            let chosen = match decision.rename_to {
                Some(name) => {
                    // 必须原样是合法的 Windows 文件名：`C:evil.txt` 会让 `with_file_name` 丢掉所在
                    // 目录，`a:b` 会写进备用数据流。拼好之后再核对一次仍在原目录里。
                    let name = zssh_core::sftp::local::check_child_name(&name)
                        .map_err(|err| err.to_string())?;
                    let chosen = dest.with_file_name(&name);
                    if let Some(parent) = dest.parent() {
                        zssh_core::sftp::local::ensure_inside(parent, &chosen)
                            .map_err(|err| err.to_string())?;
                    }
                    chosen
                }
                None => PathBuf::from(first_free_copy_path(
                    &dest.display().to_string(),
                    |candidate| std::fs::symlink_metadata(candidate).is_ok(),
                )),
            };
            if std::fs::symlink_metadata(&chosen).is_ok() {
                return Err(format!(
                    "{}{}{}",
                    t(SftpMsg::PasteAlreadyExistsPrefix),
                    name_of(&chosen.display().to_string()),
                    t(SftpMsg::PasteAlreadyExistsSuffix)
                ));
            }
            Ok(Some(chosen))
        }
        ConflictAction::Overwrite | ConflictAction::Resume => {
            if same_entry {
                return Err(t(SftpMsg::PasteOverwriteSelf).to_owned());
            }
            if existing.is_dir() != item.is_dir {
                return Err(t(SftpMsg::PasteKindsDiffer).to_owned());
            }
            Ok(Some(dest))
        }
    }
}
