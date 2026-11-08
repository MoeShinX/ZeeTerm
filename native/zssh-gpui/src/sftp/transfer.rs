//! 传输：引擎的建立、上传 / 下载任务的提交（一次用户动作一个批次）、进度行、同名文件提问的回答、
//! 暂停 / 继续 / 取消 / 重试、拖放上传、环境变量给的自动化钩子。

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use ::gpui::{ExternalPaths, Window};
use gpui_kit::*;
use zssh_core::sftp::local::download_target;
use zssh_core::sftp::path::file_name;
use zssh_core::sftp::transfer::backend::{
    SftpChannelProvider, SftpEndpoints, TransferChannelProvider,
};
use zssh_core::sftp::transfer::{
    ConflictAction, ConflictResolver, Direction, ProgressReport, TransferEngine, TransferId,
    TransferJob, TransferState,
};

use super::engine::{
    ConflictPrompt, DropKind, UiConflictResolver, can_pause, classify_dropped_path,
    conflict_action_label, conflict_decision, direction_label, local_file_name,
    resolve_remote_child, same_scope, upload_dir_target, upload_target,
};
use super::local_listing::{LocalRequest, request_listing};
use super::menu::MenuAction;
use super::model::NavMode;
use super::paste::PeerRef;
use super::{RemoteOp, SftpDemo, SftpEvent, SftpMsg, TestHook, TransferRow};
use crate::error_text::error_text;
use crate::i18n::t;
use crate::ssh_backend::runtime;

/// 失败的行重新提交所需的东西（纯数据，方便单测）。
pub(super) struct ResubmitPlan {
    pub(super) job: TransferJob,
    /// 跨主机复制：要登记进新引擎的 (登记名, 源主机数据面)；同一台主机上的任务是 `None`。
    pub(super) peer_provider: Option<(String, Arc<dyn TransferChannelProvider>)>,
}

/// 由一个失败的行重建任务。行记着跨主机复制的源端就带上它（`peer` 用同一个登记名），
/// 源主机的数据面升不上去（源标签已关闭 / 已重连换了新的）就拒绝，不回退成 `peer: None`。
pub(super) fn plan_resubmit(row: &TransferRow) -> Result<ResubmitPlan, String> {
    let peer_provider = match &row.peer {
        None => None,
        Some(peer) => {
            let provider = peer
                .provider
                .upgrade()
                .ok_or_else(|| t(SftpMsg::PasteSourceClosed).to_owned())?;
            Some((peer.name.clone(), provider))
        }
    };
    Ok(ResubmitPlan {
        job: TransferJob {
            direction: row.direction,
            source: row.source.clone(),
            target: row.target.clone(),
            recursive: true,
            peer: row.peer.as_ref().map(|peer| peer.name.clone()),
        },
        peer_provider,
    })
}

impl SftpDemo {
    // ---- 远端编辑器 ----

    /// 打开远端编辑器（菜单、双击和自动化钩子共用这一条）。目录不能用文件句柄读写，直接说清楚。
    pub(super) fn start_edit(
        &mut self,
        path: String,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let channel = self.connected_channel()?;
        if let Some(entry) = self.remote.raw.iter().find(|entry| entry.path == path) {
            if entry.is_enterable() {
                return Err(format!(
                    "{}{}",
                    entry.name,
                    t(SftpMsg::TransferDirCannotEditSuffix)
                ));
            }
            // 有损解码出来的名字不能拿去操作：服务器上的原名不是这个。
            if !entry.name_is_utf8 {
                return Err(format!(
                    "{}{}",
                    entry.name,
                    t(SftpMsg::TransferNotUtf8CannotEditSuffix)
                ));
            }
        }
        self.say(format!(
            "{}{}",
            t(SftpMsg::TransferOpeningPrefix),
            file_name(&path)
        ));
        // 正式界面：主窗口里开一个编辑器标签（同一个文件不重复开）；没有主窗口（调试用的独立 SFTP 窗口）
        // 才退回独立的编辑窗口。
        let spec = crate::remote_editor::OpenSpec {
            host_id: self.host_id,
            host_label: self.host_label.clone(),
            path: path.clone(),
            source: crate::remote_editor::ChannelSource::Ready(channel.clone()),
        };
        if !crate::main_window::editor_tabs::open_in_main_window(cx, spec) {
            crate::remote_editor::open_remote_editor_window(
                cx,
                channel,
                self.host_id,
                path,
                self.host_label.clone(),
            );
        }
        Ok(())
    }

    // ---- 引擎 ----

    /// 建立传输引擎：它向 provider 另开一条 SFTP 通道，浏览用的那条不受影响。
    pub(super) fn start_engine(&mut self) {
        let Some(factory) = self.factory.clone() else {
            return;
        };
        let provider = Arc::new(SftpChannelProvider::new(factory.connection(), None));
        let endpoints = Arc::new(SftpEndpoints::new(provider.clone()));
        // 同名文件由这个窗口回答；自动化钩子有值时它直接答，不弹面板。
        let resolver = Arc::new(UiConflictResolver::new(
            self.sender.clone(),
            self.conflict_automation,
        ));
        let decision_maker: Arc<dyn ConflictResolver> = resolver.clone();
        let (engine, mut events) = TransferEngine::new(
            endpoints.clone(),
            decision_maker,
            // 设置页「SFTP」里的同时传输数、保留修改时间 / 权限（打开这个 SFTP 标签时读一次）。
            crate::app_prefs::AppPrefs::load_default_db().transfer_options(),
        );
        match runtime() {
            Ok(runtime) => {
                let sender = self.sender.clone();
                // 新引擎的代号：旧引擎的转发任务看到代号变了就退出（断线后重建引擎时，旧任务号的事件
                // 不能落到新引擎的行上）。
                let generation = self.engine_gen.fetch_add(1, Ordering::SeqCst) + 1;
                let current = Arc::clone(&self.engine_gen);
                runtime.spawn(async move {
                    while let Some(event) = events.recv().await {
                        if current.load(Ordering::SeqCst) != generation {
                            break;
                        }
                        // 窗口关掉之后接收端已经没了，转发任务自己退出。
                        if sender.send(super::SftpEvent::Transfer(event)).is_err() {
                            break;
                        }
                    }
                });
                self.resolver = Some(resolver);
                self.provider = Some(provider);
                self.engine = Some(engine);
                self.endpoints = Some(endpoints);
            }
            Err(err) => self.warn(err),
        }
    }

    // ---- 自动化钩子 ----

    /// 环境变量给的自动化动作：连上并且首条目录读回来之后各跑一次。
    /// 传输立刻排进引擎；远端写操作排进队列按顺序跑。
    pub(super) fn run_hooks(&mut self, cx: &mut Context<Self>) {
        let hooks = std::mem::take(&mut self.pending_hooks);
        for hook in hooks {
            let result = match hook {
                // 钩子也落在本地栏的初始目录里，和手动下载同一个落点。
                TestHook::Download(remote) => {
                    let dir = Path::new(&self.local.path).to_path_buf();
                    self.in_new_batch(|this| this.start_download_to(&dir, &remote))
                }
                TestHook::DownloadDir(remote) => {
                    let dir = Path::new(&self.local.path).to_path_buf();
                    self.in_new_batch(|this| this.start_dir_download_to(&dir, &remote))
                }
                TestHook::Upload(local) => self.in_new_batch(|this| this.start_upload(&local)),
                TestHook::UploadDir(local) => {
                    self.in_new_batch(|this| this.start_dir_upload(&local))
                }
                TestHook::Mkdir(name) => {
                    self.pending_ops.push_back(RemoteOp::Mkdir(name));
                    Ok(())
                }
                TestHook::Rename { old, new } => {
                    self.pending_ops.push_back(RemoteOp::Rename { old, new });
                    Ok(())
                }
                TestHook::Delete(name) => {
                    self.pending_ops.push_back(RemoteOp::Delete(name));
                    Ok(())
                }
                TestHook::Chmod { name, mode } => {
                    self.pending_ops.push_back(RemoteOp::Chmod { name, mode });
                    Ok(())
                }
                TestHook::Drop(path) => {
                    let mut paths = ExternalPaths::default();
                    paths.0.push(std::path::PathBuf::from(path));
                    self.handle_drop(&paths, None, cx);
                    Ok(())
                }
                // 打开编辑器不动远端列表，和其他钩子一样只做一次。
                TestHook::Edit(name) => resolve_remote_child(&self.remote.path, &name)
                    .and_then(|path| self.start_edit(path, cx)),
                TestHook::Invalid(message) => Err(message),
            };
            if let Err(err) = result {
                self.warn(format!("{}{err}", t(SftpMsg::OpsAutoOpNotStartedPrefix)));
            }
        }
        self.run_next_hook_op(cx);
    }

    // ---- 进度 ----

    /// 把未结束的传输数报给安装前检查；已经结束的行不再算。
    pub(super) fn sync_install_state(&self) {
        self.install_transfers.set(
            self.transfers
                .iter()
                .filter(|row| !row.state.is_terminal())
                .count(),
        );
    }

    /// 进度事件 → 对应的行；结束事件顺手更新状态栏。
    pub(super) fn apply_report(&mut self, report: ProgressReport) {
        let Some(row) = self.transfers.iter_mut().find(|row| row.id == report.id) else {
            return;
        };
        row.apply(&report);
        let name = row.name.clone();
        // 每一份进度都可能让任务进入 / 离开"未结束"，同步给安装前检查。
        self.sync_install_state();
        if !report.state.is_terminal() {
            return;
        }
        // 任务结束了：它在面板里没人回答的提问作废，批次里的任务都结束了就清掉"全部应用"的选择。
        self.drop_conflicts_of(report.id);
        self.forget_finished_batches();
        let label = direction_label(report.direction);
        let (text, failed) = match &report.state {
            TransferState::Done => (
                format!("{label}{}{name}", t(SftpMsg::TransferDoneSuffix)),
                false,
            ),
            TransferState::Failed(reason) => (
                format!(
                    "{label}{}{name}{}{reason}{}",
                    t(SftpMsg::TransferFailedSuffix),
                    t(SftpMsg::ParenOpen),
                    t(SftpMsg::ParenClose)
                ),
                true,
            ),
            TransferState::Cancelled => (
                format!("{label}{}{name}", t(SftpMsg::TransferCancelledSuffix)),
                false,
            ),
            TransferState::Skipped => (
                format!("{label}{}{name}", t(SftpMsg::TransferSkippedSuffix)),
                false,
            ),
            _ => return,
        };
        let downloaded = matches!(report.direction, Direction::Download | Direction::LocalCopy)
            && matches!(&report.state, TransferState::Done);
        if failed {
            self.warn(text);
        } else {
            self.say(text);
        }
        if downloaded {
            self.reload_local();
        }
    }

    /// 下载完成后刷新本地栏，刚落地文件立刻能看见（状态栏不动）。
    ///
    /// 本地目录可能在慢盘 / 网络盘上，同步读目录会把界面卡住（审查 M-15）：
    /// 交给后台线程池，读完以 `LocalListed` 事件回到界面。
    fn reload_local(&mut self) {
        let path = self.local.path.clone();
        if path.is_empty() {
            return;
        }
        let sender = self.sender.clone();
        let requested = path.clone();
        let _ = request_listing(
            &self.local_lister,
            &self.local_gate,
            LocalRequest::Dir(path),
            move |reply| {
                let _ = sender.send(SftpEvent::LocalListed {
                    reply,
                    requested,
                    mode: NavMode::Refresh,
                    notice: None,
                    initial: false,
                });
            },
        );
    }

    /// 任务取消 / 结束：它在面板上等回答的提问一并撤掉，引擎那边按跳过收尾。
    pub(super) fn drop_conflicts_of(&mut self, id: TransferId) {
        if let Some(resolver) = &self.resolver {
            resolver.cancel_job(id);
        }
        self.conflicts.retain(|prompt| prompt.info.job != id);
    }

    /// 批次里的任务都结束了：它记住的「全部覆盖 / 跳过」随之作废，下一次提交重新问。
    pub(super) fn forget_finished_batches(&self) {
        let Some(resolver) = &self.resolver else {
            return;
        };
        let active = self.active_batches();
        resolver.retain_batches(&active);
    }

    fn active_batches(&self) -> std::collections::HashSet<u64> {
        self.transfers
            .iter()
            .filter(|row| !row.state.is_terminal())
            .filter_map(|row| row.batch)
            .collect()
    }

    // ---- 提交 ----

    /// 系统文件拖放：拖进来的文件 / 目录都落到远端当前目录。
    ///
    /// 拖动是显式动作；同名文件由引擎逐个提问。一项失败不影响其他项，
    /// 状态栏报开始了几项，失败时带上第一条错误。
    pub(super) fn handle_drop(
        &mut self,
        paths: &ExternalPaths,
        onto_dir: Option<String>,
        cx: &mut Context<Self>,
    ) {
        // 落在目录行上就传进那个目录，否则是远端栏的当前目录。
        let remote_dir = onto_dir.unwrap_or_else(|| self.remote.path.clone());
        let mut started = 0usize;
        let mut first_error: Option<String> = None;
        // 一次拖放 = 一次提交 = 一个批次：这次点的「全部覆盖」不会影响下一次拖放。
        self.batch_scope = self.engine.as_ref().map(|engine| engine.new_batch());
        for path in paths.paths() {
            let local = path.display().to_string();
            let result = match classify_dropped_path(path) {
                DropKind::File => self.start_upload_to(&local, &remote_dir),
                DropKind::Dir => self.start_dir_upload_to(&local, &remote_dir),
                DropKind::Missing => {
                    Err(format!("{}{local}", t(SftpMsg::TransferDropMissingPrefix)))
                }
            };
            match result {
                Ok(()) => started += 1,
                Err(err) => {
                    if first_error.is_none() {
                        first_error = Some(err);
                    }
                }
            }
        }
        self.batch_scope = None;
        match (started, first_error) {
            (0, Some(err)) => self.warn(err),
            (0, None) => self.warn(t(SftpMsg::TransferDropNone)),
            (count, None) => {
                let items =
                    crate::i18n::count(count as u64, t(SftpMsg::ItemCountSuffix), "item", "items");
                self.say(format!("{}{items}", t(SftpMsg::TransferDropCountPrefix)))
            }
            (count, Some(err)) => {
                let items =
                    crate::i18n::count(count as u64, t(SftpMsg::ItemCountSuffix), "item", "items");
                self.warn(format!(
                    "{}{items}{}{err}",
                    t(SftpMsg::TransferDropCountPrefix),
                    t(SftpMsg::OpsQueuedSomeSuffix)
                ))
            }
        }
        self.queue_open = true;
        cx.notify();
    }

    /// 排一个下载任务；本地落点的名字交给 `download_target`（非法字符、越界都挡在那一层）。
    /// 目标已存在时由引擎的同名文件提问来处理。
    pub(super) fn start_download_to(
        &mut self,
        local_dir: &Path,
        remote: &str,
    ) -> Result<(), String> {
        self.download_job(local_dir, remote, false)
    }

    /// 排一个整目录下载：远端目录 → `local_dir/<远端目录名>`，展开和建目录都由引擎做。
    /// 树里面的同名文件由引擎逐个问，界面的同名文件面板回答。
    pub(super) fn start_dir_download_to(
        &mut self,
        local_dir: &Path,
        remote: &str,
    ) -> Result<(), String> {
        self.download_job(local_dir, remote, true)
    }

    fn download_job(
        &mut self,
        local_dir: &Path,
        remote: &str,
        recursive: bool,
    ) -> Result<(), String> {
        let remote = remote.trim();
        if remote.is_empty() {
            return Err(t(SftpMsg::TransferRemotePathEmpty).to_owned());
        }
        std::fs::create_dir_all(local_dir).map_err(|err| {
            format!(
                "{}{}{}{err}{}",
                t(SftpMsg::TransferCreateLocalDirFailedPrefix),
                local_dir.display(),
                t(SftpMsg::ParenOpen),
                t(SftpMsg::ParenClose)
            )
        })?;
        let target = download_target(local_dir, remote).map_err(|err| error_text(&err))?;
        let name = file_name(remote);
        self.submit_job(
            TransferJob {
                direction: Direction::Download,
                source: remote.to_owned(),
                target: target.display().to_string(),
                recursive,
                peer: None,
            },
            name,
        )
    }

    /// 排一个上传任务；远端名字沿用本地文件名，落在当前远端目录里。
    pub(super) fn start_upload(&mut self, local_path: &str) -> Result<(), String> {
        let dir = self.remote.path.clone();
        self.start_upload_to(local_path, &dir)
    }

    /// 同 [`Self::start_upload`]，落在指定的远端目录里（拖到对面栏的目录行上时用）。
    pub(super) fn start_upload_to(
        &mut self,
        local_path: &str,
        remote_dir: &str,
    ) -> Result<(), String> {
        let local = local_path.trim();
        if local.is_empty() {
            return Err(t(SftpMsg::TransferUploadFilePrompt).to_owned());
        }
        let path = Path::new(local);
        if path.is_dir() {
            return Err(format!("{local}{}", t(SftpMsg::TransferIsDirectorySuffix)));
        }
        if !path.is_file() {
            return Err(format!(
                "{}{local}",
                t(SftpMsg::TransferLocalFileMissingPrefix)
            ));
        }
        if remote_dir.is_empty() {
            return Err(t(SftpMsg::RemoteDirNotReady).to_owned());
        }
        let target = upload_target(remote_dir, local)?;
        self.submit_job(
            TransferJob {
                direction: Direction::Upload,
                source: local.to_owned(),
                target,
                recursive: false,
                peer: None,
            },
            local_file_name(local),
        )
    }

    /// 排一个整目录上传：本地目录 → 远端 `cwd/<本地目录名>`，整棵本地树由引擎展开。
    pub(super) fn start_dir_upload(&mut self, local_path: &str) -> Result<(), String> {
        let dir = self.remote.path.clone();
        self.start_dir_upload_to(local_path, &dir)
    }

    /// 同 [`Self::start_dir_upload`]，落在指定的远端目录里。
    pub(super) fn start_dir_upload_to(
        &mut self,
        local_path: &str,
        remote_dir: &str,
    ) -> Result<(), String> {
        let local = local_path.trim();
        if local.is_empty() {
            return Err(t(SftpMsg::TransferUploadDirPrompt).to_owned());
        }
        if !Path::new(local).is_dir() {
            return Err(format!(
                "{}{local}",
                t(SftpMsg::TransferLocalDirMissingPrefix)
            ));
        }
        if remote_dir.is_empty() {
            return Err(t(SftpMsg::RemoteDirNotReady).to_owned());
        }
        let target = upload_dir_target(remote_dir, local)?;
        self.submit_job(
            TransferJob {
                direction: Direction::Upload,
                source: local.to_owned(),
                target,
                recursive: true,
                peer: None,
            },
            local_file_name(local),
        )
    }

    /// 把这一次用户动作（一次上传 / 下载 / 拖放）提交的所有任务排进同一个新批次。
    /// 「全部覆盖 / 全部跳过」只在这个批次里生效；动作结束批次作用域就清掉。
    pub(super) fn in_new_batch<R>(&mut self, run: impl FnOnce(&mut Self) -> R) -> R {
        self.batch_scope = self.engine.as_ref().map(|engine| engine.new_batch());
        let result = run(self);
        self.batch_scope = None;
        result
    }

    /// 提交任务。`submit` 内部 `tokio::spawn`，所以要套一层 runtime 上下文。
    /// 在 [`Self::in_new_batch`] 里调用时任务属于那个批次。
    fn submit_job(&mut self, job: TransferJob, name: String) -> Result<(), String> {
        self.submit_job_from(job, name, None)
    }

    /// 同 [`Self::submit_job`]；`peer_ref` 是跨主机复制的源端，记在新行上供以后重试用。
    fn submit_job_from(
        &mut self,
        job: TransferJob,
        name: String,
        peer_ref: Option<PeerRef>,
    ) -> Result<(), String> {
        let engine = Arc::clone(
            self.engine
                .as_ref()
                .ok_or_else(|| t(SftpMsg::TransferEngineNotReady).to_owned())?,
        );
        let runtime = runtime()?;
        let direction = job.direction;
        let (source, target) = (job.source.clone(), job.target.clone());
        let batch = self.batch_scope.clone();
        let id = {
            let _guard = runtime.enter();
            match &batch {
                Some(batch) => engine.submit_in_batch(job, batch),
                None => engine.submit(job),
            }
        };
        let mut row = TransferRow::new(
            id,
            name.clone(),
            direction,
            batch.as_ref().map(|batch| batch.id()),
            source,
            target,
        );
        row.peer = peer_ref;
        self.transfers.push(row);
        self.sync_install_state();
        self.say(format!(
            "{}{}{}{name}",
            t(SftpMsg::TransferQueuedPrefix),
            direction_label(direction),
            t(SftpMsg::TransferQueuedMiddle)
        ));
        Ok(())
    }

    // ---- 队列里一行的操作 ----

    pub(super) fn cancel_transfer(&mut self, id: TransferId, cx: &mut Context<Self>) {
        let Some(engine) = &self.engine else {
            return;
        };
        let text = if engine.cancel(id) {
            t(SftpMsg::TransferCancelling)
        } else {
            t(SftpMsg::TransferAlreadyFinished)
        };
        self.say(text);
        // 取消请求不立刻改行状态（要等 `ProgressReport`），但这里顺手同步一次，
        // 免得引擎已经收尾、界面还记着这个任务。
        self.sync_install_state();
        // 它如果正停在"等待处理同名文件"上，面板里那条提问没有意义了：撤掉并让引擎收尾。
        self.drop_conflicts_of(id);
        cx.notify();
    }

    /// "暂停"按钮：只改引擎里的控制标志，不阻塞界面；行状态继续跟
    /// `ProgressReport` 走（真正停下要等当前这一批传完）。
    pub(super) fn pause_transfer(&mut self, id: TransferId, cx: &mut Context<Self>) {
        let Some(engine) = &self.engine else {
            return;
        };
        let text = if engine.pause(id) {
            t(SftpMsg::TransferPausing)
        } else {
            t(SftpMsg::TransferAlreadyFinished)
        };
        self.say(text);
        cx.notify();
    }

    /// "继续"按钮：把控制标志改回运行并唤醒等待中的任务。
    pub(super) fn resume_transfer(&mut self, id: TransferId, cx: &mut Context<Self>) {
        let Some(engine) = &self.engine else {
            return;
        };
        let text = if engine.resume(id) {
            t(SftpMsg::TransferResuming)
        } else {
            t(SftpMsg::TransferAlreadyFinished)
        };
        self.say(text);
        cx.notify();
    }

    /// "重试"：失败 / 已取消 / 已跳过的任务用同一个任务号重跑（续传对账把着关；跳过的会重新问同名文件）。
    pub(super) fn retry_transfer(&mut self, id: TransferId, cx: &mut Context<Self>) {
        // `retry` 里面会 `tokio::spawn`：和 `submit` 一样要在 runtime 上下文里调。
        let Ok(runtime) = runtime() else {
            self.warn(t(SftpMsg::OpsNoRuntime));
            return;
        };
        let _guard = runtime.enter();
        let retried = self.engine.as_ref().is_some_and(|engine| engine.retry(id));
        // 引擎不认这个任务号（断线时旧引擎已经停了，重连后是新引擎）：失败的行按原来的来源 / 目标重新排一个。
        if retried {
            self.say(t(SftpMsg::TransferRetrying));
        } else {
            match self.resubmit_failed(id) {
                Ok(true) => self.say(t(SftpMsg::TransferRetrying)),
                Ok(false) => self.say(t(SftpMsg::TransferCannotRetry)),
                // 跨主机复制的源已经关了：明说原因，行原样保留。
                Err(message) => self.warn(message),
            }
        }
        cx.notify();
    }

    /// 失败的行在当前引擎里已经没有对应的任务（旧引擎在断线时停了）：按它的方向 / 来源 / 目标重新提交，
    /// 旧行换成新行。只处理 `Failed` 的行。
    ///
    /// - `Ok(true)`：已重新排上；`Ok(false)`：不适用（没有引擎、不是失败行、提交没成），行原样保留；
    /// - `Err(原因)`：跨主机复制的源主机已经关闭，**拒绝**重试（行原样保留）——绝不退回成
    ///   「源 = 目标主机」，那样目标机碰巧有同路径时会复制错内容还报成功。
    pub(super) fn resubmit_failed(&mut self, id: TransferId) -> Result<bool, String> {
        if self.engine.is_none() {
            return Ok(false);
        }
        let Some(position) = self
            .transfers
            .iter()
            .position(|row| row.id == id && matches!(row.state, TransferState::Failed(_)))
        else {
            return Ok(false);
        };
        let plan = plan_resubmit(&self.transfers[position])?;
        // 新引擎的 `peers` 是空的：先把源主机的数据面登记进去，任务才找得到它。
        if let Some((name, provider)) = plan.peer_provider {
            let Some(endpoints) = &self.endpoints else {
                return Err(t(SftpMsg::PasteSourceClosed).to_owned());
            };
            endpoints.register_peer(&name, provider);
        }
        let row = self.transfers.remove(position);
        if self
            .submit_job_from(plan.job, row.name.clone(), row.peer.clone())
            .is_err()
        {
            self.transfers
                .insert(position.min(self.transfers.len()), row);
            return Ok(false);
        }
        Ok(true)
    }

    /// 队列里一行的右键菜单项被点了。
    pub(super) fn run_transfer_menu(
        &mut self,
        action: MenuAction,
        id: TransferId,
        cx: &mut Context<Self>,
    ) {
        match action {
            MenuAction::TransferRetry => self.retry_transfer(id, cx),
            MenuAction::TransferPause => self.pause_transfer(id, cx),
            MenuAction::TransferResume => self.resume_transfer(id, cx),
            MenuAction::TransferCancel => self.cancel_transfer(id, cx),
            _ => {}
        }
    }

    /// 清掉已经结束的任务（引擎里的记录和界面上的行一起清）。
    pub(super) fn clear_finished(&mut self, cx: &mut Context<Self>) {
        if let Some(engine) = &self.engine {
            engine.clear_finished();
        }
        let before = self.transfers.len();
        self.transfers.retain(|row| !row.state.is_terminal());
        self.sync_install_state();
        self.forget_finished_batches();
        let removed = before - self.transfers.len();
        self.say(if removed > 0 {
            format!(
                "{}{removed}{}",
                t(SftpMsg::TransferClearedPrefix),
                t(SftpMsg::TransferClearedSuffix)
            )
        } else {
            t(SftpMsg::TransferNothingCleared).to_owned()
        });
        cx.notify();
    }

    // ---- 同名文件面板 ----

    /// 把重命名输入框预填成引擎建议的新名字（面板显示新一条提问时调用）。
    pub(super) fn prefill_conflict_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self
            .conflicts
            .front()
            .map(|prompt| file_name(&prompt.info.suggested_target))
            .unwrap_or_default();
        self.conflict_name_input
            .update(cx, |state, cx| state.set_value(value, window, cx));
    }

    /// 同名文件面板的单个回答：把这个文件的选择交给引擎，再显示下一个提问。
    /// 重命名的新名字不合格、或者要续传但这一对不能续传时，提问留在面板上，状态栏说明原因。
    pub(super) fn answer_conflict(
        &mut self,
        action: ConflictAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(prompt) = self.conflicts.front() else {
            return;
        };
        if action == ConflictAction::Resume && !prompt.info.can_resume {
            self.warn(t(SftpMsg::TransferCannotResume));
            cx.notify();
            return;
        }
        let typed = self.conflict_name_input.read(cx).value().to_string();
        let decision = match conflict_decision(action, prompt.info.direction, &typed) {
            Ok(decision) => decision,
            Err(message) => {
                self.warn(format!(
                    "{}{message}",
                    t(SftpMsg::TransferRenameInvalidPrefix)
                ));
                cx.notify();
                return;
            }
        };
        let Some(prompt) = self.conflicts.pop_front() else {
            return;
        };
        let target_name = file_name(&prompt.info.target);
        if let Some(resolver) = &self.resolver {
            let _ = resolver.respond(prompt.id, decision);
        }
        // 下一个提问多半是别的文件：输入框换成它的建议名，免得留着旧名字误按"重命名"。
        self.prefill_conflict_name(window, cx);
        let label = conflict_action_label(action);
        let waiting = self.conflicts.len();
        self.say(if waiting == 0 {
            format!(
                "{}{label} {target_name}",
                t(SftpMsg::TransferConflictPrefix)
            )
        } else {
            format!(
                "{}{label} {target_name}{}{waiting}{}",
                t(SftpMsg::TransferConflictPrefix),
                t(SftpMsg::TransferConflictMorePrefix),
                t(SftpMsg::TransferConflictMoreSuffix)
            )
        });
        cx.notify();
    }

    /// 「全部覆盖 / 全部跳过」：只对**当前这一批**（这次提交）有效——这一批里正在等的提问一起
    /// 回答，这一批后面的冲突也不再问；别的批次、以后的提交照常问。
    pub(super) fn answer_all_conflicts(
        &mut self,
        action: ConflictAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(front) = self.conflicts.front() else {
            return;
        };
        let (batch, job) = (front.info.batch, front.info.job);
        if let Some(resolver) = &self.resolver {
            resolver.respond_scope(action, batch, job);
        }
        let before = self.conflicts.len();
        self.conflicts
            .retain(|p: &ConflictPrompt| !same_scope(batch, job, p.info.batch, p.info.job));
        let count = before - self.conflicts.len();
        self.prefill_conflict_name(window, cx);
        self.say(format!(
            "{}{}{}{count}{}",
            t(SftpMsg::TransferConflictPrefix),
            conflict_action_label(action),
            t(SftpMsg::TransferConflictApplyAllMiddle),
            t(SftpMsg::TransferConflictApplyAllSuffix)
        ));
        cx.notify();
    }

    // ---- 暂停钩子 ----

    /// `ZSSH_SFTP_PAUSE_MS` 从 Ready 开始计时（首条目录回来之后才有任务可暂停）。
    pub(super) fn arm_pause_hook(&mut self) {
        let Some(ms) = self.pause_hook_ms else {
            return;
        };
        self.pause_hook_deadline = Instant::now().checked_add(Duration::from_millis(ms));
    }

    /// 暂停钩子的心跳：到点先暂停，2 秒后再继续。返回是否改动了任务。
    ///
    /// 只用界面上可暂停的那批行（已经结束 / 已暂停的不重复按），
    /// 这样大文件测试里正在传、还在排队、正在重连的任务都会被按到。
    pub(super) fn tick_pause_hook(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(deadline) = self.pause_hook_deadline else {
            return false;
        };
        if Instant::now() < deadline {
            return false;
        }
        if self.pause_hook_resuming {
            self.pause_hook_deadline = None;
            self.pause_hook_resuming = false;
            self.pause_hook_ms = None;
            self.apply_pause_hook(false, cx);
        } else {
            self.pause_hook_resuming = true;
            self.pause_hook_deadline = Some(Instant::now() + Duration::from_secs(2));
            self.apply_pause_hook(true, cx);
        }
        true
    }

    /// 暂停钩子的批量动作：`pause = true` 暂停可暂停的行，否则继续刚暂停的那批任务。
    fn apply_pause_hook(&mut self, pause: bool, cx: &mut Context<Self>) {
        let Some(engine) = &self.engine else {
            return;
        };
        let mut accepted = 0usize;
        if pause {
            self.pause_hook_ids.clear();
            for row in &self.transfers {
                if !can_pause(&row.state) {
                    continue;
                }
                if engine.pause(row.id) {
                    self.pause_hook_ids.push(row.id);
                    accepted += 1;
                }
            }
        } else {
            // 按任务号继续，不按行状态：暂停请求和界面收到的 `Paused` 之间可能差一拍。
            for id in std::mem::take(&mut self.pause_hook_ids) {
                if engine.resume(id) {
                    accepted += 1;
                }
            }
        }
        self.say(if pause {
            format!(
                "{}{accepted}{}",
                t(SftpMsg::TransferAutoPausePrefix),
                t(SftpMsg::TransferAutoCountSuffix)
            )
        } else {
            format!(
                "{}{accepted}{}",
                t(SftpMsg::TransferAutoResumePrefix),
                t(SftpMsg::TransferAutoCountSuffix)
            )
        });
        cx.notify();
    }
}
