//! 备份与恢复：本地 DPAPI 加密备份的列表、创建和恢复，以及离线同步包的导出/导入。
//!
//! G11 起主入口是主窗口设置页里的备份一节（`new_embedded`，画法在 `view.rs`）；独立的备份窗口只剩调试入口
//! （`ZSSH_BACKUP=1`，只在 debug 构建里读）。两种形态共用同一个 `BackupDemo` 和下面的互斥 / 确认逻辑。
//!
//! 备份文件和离线包可能有几十 MB，读列表要逐个解密、创建/恢复/导出/导入也要读写
//! 整个库，所以都在 [`crate::ssh_backend::runtime`] 的阻塞线程池里跑；界面只收
//! `mpsc` 事件，100ms 取一次，不阻塞窗口。
//!
//! 恢复分两步：第一次点「恢复」只把这一行置为确认状态，点「确认恢复」才真正写库；
//! 走核心的 `SyncEngine::restore_backup`（检查备份路径、带上同步基线里的删除记录），
//! 恢复前会先自动做一份安全备份。离线包用核心的 `export_offline` / `import_offline`
//! （和 0.2.14 同一套）：导出时墓碑不带走，导入有「合并」和「替换」两种，**不动同步基线**，
//! 所以导入的记录不会在下一次云同步里被当成"云端删了"而删掉。导出先写临时文件再
//! 原子替换，目标文件已存在时先置为覆盖确认状态，再点「确认覆盖」才写；「替换」会删掉
//! 文件里没有的本机记录，同样要确认一次。
//!
//! 恢复和导入会改本地数据，与同步互斥（同一个 [`crate::sync::SyncInFlight`]）：
//! 同步进行中不允许恢复 / 导入，恢复 / 导入进行中不允许同步；进行期间安装前检查也会拦着
//! 更新器。

#[cfg(debug_assertions)]
mod dev;
mod view;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use crate::armed::ArmedConfirm;
use crate::i18n::{Msg, t};
use ::gpui::{
    App, FocusHandle, ScrollHandle, Subscription, Task, Window, WindowBounds, WindowOptions,
};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::*;
use zssh_core::ai::history::now_ms;
use zssh_core::store::Database;
use zssh_core::sync::accounts::{AccountsRestore, SyncAccounts};
use zssh_core::sync::apply::{ApplyReport, ChangeCounts};
use zssh_core::sync::backup::{BackupDetail, BackupInfo, BackupStore, RestoreReport};
use zssh_core::sync::engine::SyncEngine;
use zssh_core::sync::local_extras::LocalExtras;
use zssh_core::sync::offline::{MIN_PASSWORD_CHARS, OfflineImportMode, OfflineImportReport};
use zssh_core::sync::sealer::{DpapiSealer, SecretSealer};
use zssh_core::sync::snapshot::snapshot_local;

use crate::sync::{ExclusiveKind, SyncInFlight, exclusive_busy_message};

/// 备份与恢复的界面文案：卡片标题、按钮、状态行、确认提示、错误前缀。
///
/// 只放**应用自己的**固定文案；备份文件名、路径、云端账号名和核心库的错误原文属于远端 / 用户
/// 数据，原样拼在本地化前缀后面，不经过这里。带参数的句子保留 `format!`，这里只提供固定片段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BackupMsg {
    // ---- 窗口标题 / 后台任务 ----
    WindowTitle,
    BusyList,
    BusyCreate,
    BusyRestore,
    BusyExport,
    BusyImport,
    // ---- 占位提示 ----
    PasswordPlaceholder,
    PasswordConfirmPlaceholder,
    PackagePathPlaceholder,
    // ---- 保留份数 ----
    KeepRangePrefix,
    KeepRangeSuffix,
    // ---- 任务启动失败（前缀，后面接错误原文） ----
    StartLoadFailedPrefix,
    StartCreateFailedPrefix,
    StartRestoreFailedPrefix,
    StartExportFailedPrefix,
    StartImportFailedPrefix,
    // ---- 状态行 ----
    OverwriteExistsPrefix,
    OverwriteExistsSuffix,
    CreatedPrefix,
    ExportedPrefix,
    ExportedBytesOpen,
    ExportedBytesSuffix,
    // ---- 恢复 / 导入带回的同步账号设置 ----
    AccountsAppliedLine,
    AccountsNeedsConfirmationLine,
    AccountsWriteBackFailedPrefix,
    AccountsReplaced,
    AccountsReplaceFailedPrefix,
    AccountsKept,
    // ---- 确认提示 ----
    ConfirmWarning,
    ReplaceWarning,
    // ---- 输入校验 ----
    PasswordEmpty,
    PasswordTooShortPrefix,
    PasswordTooShortSuffix,
    PasswordMismatch,
    PackagePathEmpty,
    // ---- 导入结果摘要 ----
    ImportSummaryPrefix,
    ImportSummaryUpdated,
    ImportSummaryDeleted,
    ImportSummaryIdentities,
    ImportSummaryProxies,
    ImportSummarySuffix,
    ConflictsKeptNewerPrefix,
    ConflictsKeptNewerSuffix,
    NulledReferencesPrefix,
    NulledReferencesSuffix,
    SafetyBackupPrefix,
    // ---- 应用统计 ----
    AppliedPrefix,
    AppliedUpdated,
    AppliedDeleted,
    AppliedIdentitiesPrefix,
    AppliedGroupsPrefix,
    AppliedProxiesPrefix,
    AppliedSettingsPrefix,
    AppliedKnownHostsPrefix,
    AppliedConflictsPrefix,
    AppliedSuffix,
    SkippedHostKeysPrefix,
    SkippedHostKeysInlinePrefix,
    SkippedHostKeysSuffix,
    // ---- 恢复结果摘要 ----
    RestoreSummaryPrefix,
    RestoreSummaryHostMemory,
    RestoreSummaryForwards,
    RestoreSummarySafetyBackupPrefix,
    // ---- 本机设置读写失败（前缀，后面接错误原文） ----
    SaveKeepFailedPrefix,
    ReadDeviceIdFailedPrefix,
    CreatePackageDirFailedPrefix,
    WritePackageFailedPrefix,
    CreateDataDirFailedPrefix,
    OpenDatabaseFailedPrefix,
}

impl Msg for BackupMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::WindowTitle => "备份与恢复",
            Self::BusyList => "正在读取备份列表…",
            Self::BusyCreate => "正在创建备份…",
            Self::BusyRestore => "正在恢复备份…",
            Self::BusyExport => "正在导出离线包…",
            Self::BusyImport => "正在导入离线包…",
            Self::PasswordPlaceholder => "主密码（只用于离线包，不会保存）",
            Self::PasswordConfirmPlaceholder => "导出时再输入一次",
            Self::PackagePathPlaceholder => "离线包路径",
            Self::KeepRangePrefix => "请输入 1–",
            Self::KeepRangeSuffix => " 的整数",
            Self::StartLoadFailedPrefix => "启动读取备份任务失败：",
            Self::StartCreateFailedPrefix => "启动备份任务失败：",
            Self::StartRestoreFailedPrefix => "启动恢复任务失败：",
            Self::StartExportFailedPrefix => "启动导出任务失败：",
            Self::StartImportFailedPrefix => "启动导入任务失败：",
            Self::OverwriteExistsPrefix => "离线包已存在：",
            Self::OverwriteExistsSuffix => "，再次点击「确认覆盖」才会覆盖",
            Self::CreatedPrefix => "备份已创建：",
            Self::ExportedPrefix => "离线包已导出：",
            Self::ExportedBytesOpen => "（",
            Self::ExportedBytesSuffix => " 字节）",
            Self::AccountsAppliedLine => {
                "\n备份里的同步账号设置（存储连接、登录信息、云端备份目标）已一并写回，不用重新填"
            }
            Self::AccountsNeedsConfirmationLine => {
                "\n备份里带着同步账号设置，本机已经配置了同步，等你决定要不要替换"
            }
            Self::AccountsWriteBackFailedPrefix => "\n备份里的同步账号设置没能写回：",
            Self::AccountsReplaced => "已用备份里的同步账号设置替换了本机现在的",
            Self::AccountsReplaceFailedPrefix => "替换同步账号设置失败：",
            Self::AccountsKept => "已保留本机现在的同步账号设置",
            Self::ConfirmWarning => "恢复前会自动创建一份安全备份，并覆盖当前主机/凭据/设置。",
            Self::ReplaceWarning => {
                "替换会让本机和离线包一模一样：包里没有的本机主机、凭据、分组、代理都会被删掉。\
                 导入前会自动备份，再点一次「确认替换导入」继续"
            }
            Self::PasswordEmpty => "密码不能为空",
            Self::PasswordTooShortPrefix => "密码至少 ",
            Self::PasswordTooShortSuffix => " 个字符",
            Self::PasswordMismatch => "两次输入的密码不一样",
            Self::PackagePathEmpty => "包路径不能为空",
            Self::ImportSummaryPrefix => "已导入：主机新增 ",
            Self::ImportSummaryUpdated => " / 更新 ",
            Self::ImportSummaryDeleted => " / 删除 ",
            Self::ImportSummaryIdentities => "，凭据新增 ",
            Self::ImportSummaryProxies => "，代理新增 ",
            Self::ImportSummarySuffix => "。",
            Self::ConflictsKeptNewerPrefix => "",
            Self::ConflictsKeptNewerSuffix => " 条两边都改过，保留了较新的，另一份在冲突页。",
            Self::NulledReferencesPrefix => "有 ",
            Self::NulledReferencesSuffix => " 处引用指向不存在的记录，已清空。",
            Self::SafetyBackupPrefix => "导入前的状态已自动备份为 ",
            Self::AppliedPrefix => "应用：主机 新增",
            Self::AppliedUpdated => "/更新",
            Self::AppliedDeleted => "/删除",
            Self::AppliedIdentitiesPrefix => "；凭据 新增",
            Self::AppliedGroupsPrefix => "；分组 新增",
            Self::AppliedProxiesPrefix => "；代理 新增",
            Self::AppliedSettingsPrefix => "；设置 ",
            Self::AppliedKnownHostsPrefix => " 项；known_hosts ",
            Self::AppliedConflictsPrefix => " 条；冲突 ",
            Self::AppliedSuffix => " 条",
            Self::SkippedHostKeysPrefix => "\n跳过 ",
            Self::SkippedHostKeysInlinePrefix => "；跳过 ",
            Self::SkippedHostKeysSuffix => " 条可疑主机密钥记录",
            Self::RestoreSummaryPrefix => "恢复完成：主机 新增",
            Self::RestoreSummaryHostMemory => " 条；主机记忆 ",
            Self::RestoreSummaryForwards => " 条；转发规则 ",
            Self::RestoreSummarySafetyBackupPrefix => " 条\n恢复前的安全备份：",
            Self::SaveKeepFailedPrefix => "保存备份保留份数失败：",
            Self::ReadDeviceIdFailedPrefix => "读取同步设备 ID 失败：",
            Self::CreatePackageDirFailedPrefix => "创建离线包目录失败：",
            Self::WritePackageFailedPrefix => "写入离线包失败：",
            Self::CreateDataDirFailedPrefix => "创建数据目录失败：",
            Self::OpenDatabaseFailedPrefix => "打开数据库失败：",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::WindowTitle => "Backup & Restore",
            Self::BusyList => "Reading the backup list…",
            Self::BusyCreate => "Creating a backup…",
            Self::BusyRestore => "Restoring the backup…",
            Self::BusyExport => "Exporting the offline package…",
            Self::BusyImport => "Importing the offline package…",
            Self::PasswordPlaceholder => "Master password (offline packages only, never saved)",
            Self::PasswordConfirmPlaceholder => "Enter it again to export",
            Self::PackagePathPlaceholder => "Offline package path",
            Self::KeepRangePrefix => "Enter 1–",
            Self::KeepRangeSuffix => " as a whole number",
            Self::StartLoadFailedPrefix => "Could not start the backup list task: ",
            Self::StartCreateFailedPrefix => "Could not start the backup task: ",
            Self::StartRestoreFailedPrefix => "Could not start the restore task: ",
            Self::StartExportFailedPrefix => "Could not start the export task: ",
            Self::StartImportFailedPrefix => "Could not start the import task: ",
            Self::OverwriteExistsPrefix => "The offline package already exists: ",
            Self::OverwriteExistsSuffix => ". Click “Confirm Overwrite” again to overwrite it",
            Self::CreatedPrefix => "Backup created: ",
            Self::ExportedPrefix => "Offline package exported: ",
            Self::ExportedBytesOpen => " (",
            Self::ExportedBytesSuffix => " bytes)",
            Self::AccountsAppliedLine => {
                "\nThe sync account settings from this backup (storage connection, sign-in info, cloud backup targets) were written back too; no need to fill them in again"
            }
            Self::AccountsNeedsConfirmationLine => {
                "\nThis backup carries sync account settings and this computer already has sync configured; waiting for you to decide whether to replace them"
            }
            Self::AccountsWriteBackFailedPrefix => {
                "\nThe sync account settings in this backup could not be written back: "
            }
            Self::AccountsReplaced => {
                "Replaced this computer's sync account settings with the ones from the backup"
            }
            Self::AccountsReplaceFailedPrefix => "Could not replace the sync account settings: ",
            Self::AccountsKept => "Kept this computer's current sync account settings",
            Self::ConfirmWarning => {
                "A safety backup is created first, and the current hosts/credentials/settings are overwritten."
            }
            Self::ReplaceWarning => {
                "Replacing makes this computer identical to the offline package: hosts, credentials, groups and proxies that the package does not contain are deleted. A backup is made automatically before importing; click “Confirm Replace Import” again to continue"
            }
            Self::PasswordEmpty => "The password cannot be empty",
            Self::PasswordTooShortPrefix => "The password must be at least ",
            Self::PasswordTooShortSuffix => " characters",
            Self::PasswordMismatch => "The two passwords do not match",
            Self::PackagePathEmpty => "The package path cannot be empty",
            Self::ImportSummaryPrefix => "Imported: hosts added ",
            Self::ImportSummaryUpdated => " / updated ",
            Self::ImportSummaryDeleted => " / deleted ",
            Self::ImportSummaryIdentities => ", credentials added ",
            Self::ImportSummaryProxies => ", proxies added ",
            Self::ImportSummarySuffix => ".",
            Self::ConflictsKeptNewerPrefix => "",
            Self::ConflictsKeptNewerSuffix => {
                " records were changed on both sides; the newer one was kept and the other is on the conflicts page."
            }
            Self::NulledReferencesPrefix => "",
            Self::NulledReferencesSuffix => {
                " references pointed at records that no longer exist and were cleared."
            }
            Self::SafetyBackupPrefix => {
                "The state before importing was backed up automatically as "
            }
            Self::AppliedPrefix => "Applied: hosts added ",
            Self::AppliedUpdated => "/updated",
            Self::AppliedDeleted => "/deleted",
            Self::AppliedIdentitiesPrefix => "; credentials added",
            Self::AppliedGroupsPrefix => "; groups added",
            Self::AppliedProxiesPrefix => "; proxies added",
            Self::AppliedSettingsPrefix => "; settings ",
            Self::AppliedKnownHostsPrefix => " items; known_hosts ",
            Self::AppliedConflictsPrefix => " entries; conflicts ",
            Self::AppliedSuffix => " entries",
            Self::SkippedHostKeysPrefix => "\nSkipped ",
            Self::SkippedHostKeysInlinePrefix => "; skipped ",
            Self::SkippedHostKeysSuffix => " suspicious host key records",
            Self::RestoreSummaryPrefix => "Restore complete: hosts added",
            Self::RestoreSummaryHostMemory => "; host memory ",
            Self::RestoreSummaryForwards => " entries; port forwards ",
            Self::RestoreSummarySafetyBackupPrefix => " entries\nSafety backup before restoring: ",
            Self::SaveKeepFailedPrefix => "Could not save how many backups to keep: ",
            Self::ReadDeviceIdFailedPrefix => "Could not read the sync device ID: ",
            Self::CreatePackageDirFailedPrefix => {
                "Could not create the offline package directory: "
            }
            Self::WritePackageFailedPrefix => "Could not write the offline package: ",
            Self::CreateDataDirFailedPrefix => "Could not create the data directory: ",
            Self::OpenDatabaseFailedPrefix => "Could not open the database: ",
        }
    }
}

/// 备份与恢复窗口的编号；IME 按窗口 key 管理。
static NEXT_BACKUP: AtomicU64 = AtomicU64::new(1);

/// 后台事件轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// `sync_backup_keep` 读不出来时用几份：和核心里没设置时的默认是同一个常量。
const FALLBACK_KEEP: usize = zssh_core::store::settings::SYNC_BACKUP_KEEP_DEFAULT;

/// 保留份数输入框里的文字 → 份数。只接受 1–200 的整数（不带正负号、小数点、单位）；别的都给一句话
/// 说明范围，调用方不保存。
pub(crate) fn parse_keep(text: &str) -> Result<usize, String> {
    let max = zssh_core::store::settings::SYNC_BACKUP_KEEP_MAX;
    let text = text.trim();
    let hint = || {
        format!(
            "{}{max}{}",
            t(BackupMsg::KeepRangePrefix),
            t(BackupMsg::KeepRangeSuffix)
        )
    };
    if text.is_empty() || !text.chars().all(|c| c.is_ascii_digit()) {
        return Err(hint());
    }
    match text.parse::<usize>() {
        Ok(keep) if (1..=max).contains(&keep) => Ok(keep),
        _ => Err(hint()),
    }
}

pub struct BackupDemo {
    backups: Vec<BackupDetail>,
    status: String,
    busy: Option<Busy>,
    confirm: RestoreConfirm,
    /// 离线包的主密码；只存在内存里，界面不清空（失败后可以直接重试）。
    password: Entity<InputState>,
    /// 导出时再输一遍：忘了密码文件就打不开，输错一遍比没输更糟。
    password_confirm: Entity<InputState>,
    package_path: Entity<InputState>,
    overwrite: OverwriteConfirm,
    /// 导入方式：合并（两边都留）或替换（本机变成和文件一样）。
    import_mode: OfflineImportMode,
    /// 「替换」会删本机记录，点第一次只置为确认状态。
    replace_confirm: OverwriteConfirm,
    /// 本地备份最多保留几份（设置里的值；读不出来是 [`FALLBACK_KEEP`]）。
    keep: usize,
    /// 恢复 / 导入时备份里带着同步账号设置、而本机已经配置了同步：等用户决定要不要替换（默认不替换）。
    pending_accounts: Option<PendingAccounts>,
    /// 保留份数输入框；回车 / 失焦时按 [`parse_keep`] 校验后保存。
    keep_input: Entity<InputState>,
    /// 输入框里的内容不合法时的提示（就地显示在说明下面）。
    keep_error: Option<String>,
    /// 嵌在设置页里：只画卡片，不画窗口标题，也不接输入法的英文区。
    embedded: bool,
    events: Receiver<BackupEvent>,
    sender: Sender<BackupEvent>,
    scroll: ScrollHandle,
    focus: FocusHandle,
    _ime: Vec<Subscription>,
    _keep_sub: Subscription,
    _tick: Task<()>,
}

impl BackupDemo {
    pub fn new(window: &mut Window, cx: &mut Context<Self>, ime_key: String) -> Self {
        Self::build(window, cx, Some(ime_key))
    }

    /// 嵌进设置页的形态（G11）：同一份状态和逻辑，只是不带窗口外壳。
    pub fn new_embedded(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::build(window, cx, None)
    }

    fn build(window: &mut Window, cx: &mut Context<Self>, ime_key: Option<String>) -> Self {
        let focus = cx.focus_handle();
        let embedded = ime_key.is_none();
        let mut ime = Vec::new();
        if let Some(ime_key) = ime_key {
            ime.push(cx.on_focus(&focus, window, {
                let key = ime_key.clone();
                move |_this, _window, _cx| {
                    crate::ime::apply(&key, true);
                }
            }));
            ime.push(cx.on_blur(&focus, window, move |_this, _window, _cx| {
                crate::ime::apply(&ime_key, false);
            }));
        }
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t(BackupMsg::PasswordPlaceholder))
        });
        let password_confirm = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t(BackupMsg::PasswordConfirmPlaceholder))
        });
        // `zeeterm-日期.zsshbak`（扩展名和 0.2.14 一样，互相能导入）；0.2.14 是弹保存对话框选位置，这里默认放在
        // 数据目录里，用「选择…」可以换。
        let default_path = crate::settings::data_dir().join(view::default_file_name());
        let package_path = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(default_path.display().to_string())
                .placeholder(t(BackupMsg::PackagePathPlaceholder))
        });
        let keep_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(FALLBACK_KEEP.to_string())
                .placeholder("1–200")
        });
        let keep_sub = cx.subscribe_in(
            &keep_input,
            window,
            |this, _input, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                    this.commit_keep_input(cx);
                }
            },
        );
        let (sender, events) = std::sync::mpsc::channel();
        let mut this = Self {
            backups: Vec::new(),
            status: String::new(),
            busy: None,
            confirm: RestoreConfirm::default(),
            password,
            password_confirm,
            package_path,
            overwrite: OverwriteConfirm::default(),
            import_mode: OfflineImportMode::Merge,
            replace_confirm: OverwriteConfirm::default(),
            keep: FALLBACK_KEEP,
            pending_accounts: None,
            keep_input,
            keep_error: None,
            embedded,
            events,
            sender,
            scroll: ScrollHandle::new(),
            focus,
            _ime: ime,
            _keep_sub: keep_sub,
            _tick: Task::ready(()),
        };
        // 100ms 取一次后台结果；窗口没了 `update_in` 会报错，任务自己退出。
        this._tick = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                if this
                    .update_in(cx, |this, window, cx| this.poll_events(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        this.start_load(cx);
        this
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// 重新读一遍备份列表；打开窗口和操作完成后都走这里。
    fn start_load(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let dir = crate::settings::data_dir();
        let sender = self.sender.clone();
        match crate::ssh_backend::runtime() {
            Ok(runtime) => {
                self.busy = Some(Busy::List);
                // 不等结果：任务通过 mpsc 回报，窗口关了通道自然断开。
                drop(runtime.spawn_blocking(move || {
                    let event = match list_backups(&dir) {
                        Ok(details) => BackupEvent::Loaded {
                            details,
                            keep: read_keep(&dir),
                        },
                        Err(message) => BackupEvent::LoadFailed(message),
                    };
                    let _ = sender.send(event);
                }));
            }
            Err(err) => self.status = format!("{}{err}", t(BackupMsg::StartLoadFailedPrefix)),
        }
        cx.notify();
    }

    /// 新建一份备份；快照、本地附加数据和保留份数都在后台读。
    fn start_create(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let dir = crate::settings::data_dir();
        let sender = self.sender.clone();
        match crate::ssh_backend::runtime() {
            Ok(runtime) => {
                self.busy = Some(Busy::Create);
                self.status.clear();
                drop(runtime.spawn_blocking(move || {
                    let event = match create_backup(&dir) {
                        Ok(info) => BackupEvent::Created(info),
                        Err(message) => BackupEvent::CreateFailed(message),
                    };
                    let _ = sender.send(event);
                }));
            }
            Err(err) => self.status = format!("{}{err}", t(BackupMsg::StartCreateFailedPrefix)),
        }
        cx.notify();
    }

    /// 恢复指定备份；确认状态已经由调用方取走。和同步互斥：同步进行中不恢复。
    fn start_restore(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let guard = match begin_restore() {
            Ok(guard) => guard,
            Err(message) => {
                self.status = message;
                cx.notify();
                return;
            }
        };
        let dir = crate::settings::data_dir();
        let sender = self.sender.clone();
        match crate::ssh_backend::runtime() {
            Ok(runtime) => {
                self.busy = Some(Busy::Restore);
                self.status.clear();
                drop(runtime.spawn_blocking(move || {
                    // 守卫活到恢复结束（含出错）；期间同步和别的恢复 / 导入都进不来。
                    let _guard = guard;
                    let event = match restore_backup(&dir, &path) {
                        Ok(report) => BackupEvent::Restored(Box::new(report)),
                        Err(message) => BackupEvent::RestoreFailed(message),
                    };
                    let _ = sender.send(event);
                }));
            }
            Err(err) => self.status = format!("{}{err}", t(BackupMsg::StartRestoreFailedPrefix)),
        }
        cx.notify();
    }

    /// 第一次点某一行的「恢复」：只置为确认状态，不动数据库。
    fn arm_restore(&mut self, path: String, name: String, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.confirm.arm(path, name);
        cx.notify();
    }

    /// 点「确认恢复」：取走确认状态，只可能恢复一次。
    fn confirm_restore(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let Some(armed) = self.confirm.confirm() else {
            return;
        };
        self.start_restore(PathBuf::from(armed.path), cx);
    }

    fn cancel_restore(&mut self, cx: &mut Context<Self>) {
        self.confirm.disarm();
        cx.notify();
    }

    /// 点「导出离线包」：先校验输入（密码两遍一致、不少于核心的最短长度），
    /// 目标文件已存在时只置为覆盖确认状态。
    fn start_export(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let request = export_request(
            &self.password.read(cx).value(),
            &self.password_confirm.read(cx).value(),
            &self.package_path.read(cx).value(),
        );
        let (password, path) = match request {
            Ok(request) => request,
            Err(message) => {
                self.status = message;
                cx.notify();
                return;
            }
        };
        // 存在性检查放在界面线程：确认状态和用户看到的是同一份文件。
        let exists = path.exists();
        match self.overwrite.check(&path, exists) {
            OverwriteCheck::Confirm => {
                self.status = format!(
                    "{}{}{}",
                    t(BackupMsg::OverwriteExistsPrefix),
                    path.display(),
                    t(BackupMsg::OverwriteExistsSuffix)
                );
                cx.notify();
            }
            OverwriteCheck::Write => self.spawn_export(password, path, cx),
        }
    }

    /// 点「确认覆盖」：写的就是置为确认状态时那个路径。密码和确认框在这期间可能被改过，
    /// 所以和第一次点击用同一套校验。
    fn confirm_overwrite(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let password = self.password.read(cx).value().to_string();
        if let Err(message) = validate_export_password(&password)
            .and_then(|()| check_confirmation(&password, &self.password_confirm.read(cx).value()))
        {
            self.status = message;
            cx.notify();
            return;
        }
        let Some(path) = self.overwrite.take() else {
            return;
        };
        self.spawn_export(password, path, cx);
    }

    fn cancel_overwrite(&mut self, cx: &mut Context<Self>) {
        self.overwrite.disarm();
        cx.notify();
    }

    /// 在阻塞线程池里导出；密码只进闭包，不写日志。
    fn spawn_export(&mut self, password: String, path: PathBuf, cx: &mut Context<Self>) {
        let dir = crate::settings::data_dir();
        let sender = self.sender.clone();
        match crate::ssh_backend::runtime() {
            Ok(runtime) => {
                self.busy = Some(Busy::Export);
                self.status.clear();
                drop(runtime.spawn_blocking(move || {
                    let event = match export_offline_to(&dir, &DpapiSealer, &password, &path) {
                        Ok(bytes) => BackupEvent::Exported { path, bytes },
                        Err(message) => BackupEvent::ExportFailed(message),
                    };
                    let _ = sender.send(event);
                }));
            }
            Err(err) => self.status = format!("{}{err}", t(BackupMsg::StartExportFailedPrefix)),
        }
        cx.notify();
    }

    /// 选导入方式。换了方式要重新确认「替换」。
    fn set_import_mode(&mut self, mode: OfflineImportMode, cx: &mut Context<Self>) {
        if self.import_mode != mode {
            self.import_mode = mode;
            self.replace_confirm.disarm();
            cx.notify();
        }
    }

    /// 点「导入离线包」：读文件、解密、合并都在后台。「替换」会删掉文件里没有的本机
    /// 记录，第一次点击只置为确认状态。
    fn start_import(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let request = offline_request(
            &self.password.read(cx).value(),
            &self.package_path.read(cx).value(),
        );
        let (password, path) = match request {
            Ok(request) => request,
            Err(message) => {
                self.status = message;
                cx.notify();
                return;
            }
        };
        if self.import_mode == OfflineImportMode::Replace {
            match self.replace_confirm.check(&path, true) {
                OverwriteCheck::Confirm => {
                    self.status = t(BackupMsg::ReplaceWarning).to_owned();
                    cx.notify();
                    return;
                }
                OverwriteCheck::Write => {}
            }
        }
        self.spawn_import(path, password, self.import_mode, cx);
    }

    /// 在阻塞线程里导入；导入会改本地数据，和同步互斥。
    fn spawn_import(
        &mut self,
        path: PathBuf,
        password: String,
        mode: OfflineImportMode,
        cx: &mut Context<Self>,
    ) {
        let guard = match begin_restore() {
            Ok(guard) => guard,
            Err(message) => {
                self.status = message;
                cx.notify();
                return;
            }
        };
        let dir = crate::settings::data_dir();
        let sender = self.sender.clone();
        match crate::ssh_backend::runtime() {
            Ok(runtime) => {
                self.busy = Some(Busy::Import);
                self.status.clear();
                drop(runtime.spawn_blocking(move || {
                    let _guard = guard;
                    let event =
                        match import_offline_from(&dir, &DpapiSealer, &path, &password, mode) {
                            Ok(report) => BackupEvent::Imported(Box::new(report)),
                            Err(message) => BackupEvent::ImportFailed(message),
                        };
                    let _ = sender.send(event);
                }));
            }
            Err(err) => self.status = format!("{}{err}", t(BackupMsg::StartImportFailedPrefix)),
        }
        cx.notify();
    }

    /// 取走后台事件；同一时间只允许一个任务，所以不用代号。
    fn poll_events(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut changed = false;
        while let Ok(event) = self.events.try_recv() {
            changed = true;
            match event {
                BackupEvent::Loaded { details, keep } => {
                    self.busy = None;
                    self.keep = keep;
                    self.show_keep(window, cx);
                    // 列表刷新后原来确认的那一行可能已经被保留策略删掉了。
                    if let Some(armed) = self.confirm.armed()
                        && !details.iter().any(|detail| detail.info.path == armed.path)
                    {
                        self.confirm.disarm();
                    }
                    self.backups = details;
                }
                BackupEvent::LoadFailed(message) => {
                    self.busy = None;
                    self.status = message;
                }
                BackupEvent::Created(info) => {
                    self.busy = None;
                    self.status = format!("{}{}", t(BackupMsg::CreatedPrefix), info.name);
                    self.start_load(cx);
                }
                BackupEvent::CreateFailed(message) => {
                    self.busy = None;
                    self.status = message;
                }
                BackupEvent::Restored(report) => {
                    self.busy = None;
                    self.confirm.disarm();
                    self.status = restore_summary(&report);
                    let accounts = report.accounts.clone();
                    self.take_accounts(accounts, AccountsOrigin::Local);
                    self.start_load(cx);
                }
                BackupEvent::RestoreFailed(message) => {
                    self.busy = None;
                    self.confirm.disarm();
                    self.status = message;
                }
                BackupEvent::Exported { path, bytes } => {
                    self.busy = None;
                    self.overwrite.disarm();
                    self.status = format!(
                        "{}{}{}{bytes}{}",
                        t(BackupMsg::ExportedPrefix),
                        path.display(),
                        t(BackupMsg::ExportedBytesOpen),
                        t(BackupMsg::ExportedBytesSuffix)
                    );
                }
                BackupEvent::ExportFailed(message) => {
                    self.busy = None;
                    self.overwrite.disarm();
                    self.status = message;
                }
                BackupEvent::Imported(report) => {
                    self.busy = None;
                    self.replace_confirm.disarm();
                    self.status = import_summary(&report);
                    let accounts = report.accounts.clone();
                    self.take_accounts(accounts, AccountsOrigin::Offline);
                    // 本地写过了：列表里多了一份导入前的安全备份。
                    self.start_load(cx);
                }
                BackupEvent::ImportFailed(message) => {
                    self.busy = None;
                    self.replace_confirm.disarm();
                    self.status = message;
                }
            }
        }
        if changed {
            cx.notify();
        }
    }
}

/// 备份里带着、而本机已经配置了同步的那份同步账号设置，和它是从哪来的（提示框画在对应的卡片里）。
struct PendingAccounts {
    accounts: Box<SyncAccounts>,
    origin: AccountsOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccountsOrigin {
    /// 本地备份的恢复。
    Local,
    /// 离线备份文件的导入。
    Offline,
}

impl BackupDemo {
    /// 处理恢复 / 导入报告里的同步账号设置：已经写回的提示一句并通知同步页重读；要问用户的留着等回答；
    /// 写回出错的把原因说出来（数据本身已经恢复好了）。
    fn take_accounts(&mut self, accounts: AccountsRestore, origin: AccountsOrigin) {
        self.pending_accounts = None;
        match accounts {
            AccountsRestore::NotInBackup => {}
            AccountsRestore::Applied => {
                self.status.push_str(t(BackupMsg::AccountsAppliedLine));
                crate::sync::accounts_changed();
            }
            AccountsRestore::NeedsConfirmation(accounts) => {
                self.status
                    .push_str(t(BackupMsg::AccountsNeedsConfirmationLine));
                self.pending_accounts = Some(PendingAccounts { accounts, origin });
            }
            AccountsRestore::Failed(message) => {
                self.status.push_str(&format!(
                    "{}{message}",
                    t(BackupMsg::AccountsWriteBackFailedPrefix)
                ));
            }
        }
    }

    /// 「替换为备份里的」：用备份里的同步账号设置换掉本机现在的。写本机设置很快，不用后台线程。
    fn replace_accounts(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_accounts.take() else {
            return;
        };
        let result = open_db_at(&crate::settings::data_dir()).and_then(|db| {
            pending
                .accounts
                .apply(&db, &DpapiSealer)
                .map_err(|err| err.to_string())
        });
        match result {
            Ok(()) => {
                self.status = t(BackupMsg::AccountsReplaced).to_owned();
                crate::sync::accounts_changed();
            }
            Err(message) => {
                self.status = format!("{}{message}", t(BackupMsg::AccountsReplaceFailedPrefix))
            }
        }
        cx.notify();
    }

    /// 「保留本机现在的」（默认）：什么也不改。
    fn keep_accounts(&mut self, cx: &mut Context<Self>) {
        if self.pending_accounts.take().is_some() {
            self.status = t(BackupMsg::AccountsKept).to_owned();
            cx.notify();
        }
    }
}

impl Render for BackupDemo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_root(window, cx)
    }
}

/// 打开备份与恢复窗口；同一个进程可以开多个。
pub fn open_backup_window(cx: &mut App) {
    let n = NEXT_BACKUP.fetch_add(1, Ordering::Relaxed);
    let ime_key = format!("backup-window-{n}");
    let close_key = ime_key.clone();
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(
            size(::gpui::px(820.), ::gpui::px(760.)),
            cx,
        )),
        focus: true,
        ..Default::default()
    };
    let _ = cx.open_window(options, move |window, cx| {
        let view = cx.new(|cx| BackupDemo::new(window, cx, ime_key));
        window.set_window_title(t(BackupMsg::WindowTitle));
        window.on_window_should_close(cx, move |_window, _cx| {
            crate::ime::window_destroyed(&close_key);
            true
        });
        window.focus(&view.read(cx).focus_handle(), cx);
        view
    });
}

/// 后台任务 → 界面的消息（100ms tick 取走）。
enum BackupEvent {
    Loaded {
        details: Vec<BackupDetail>,
        keep: usize,
    },
    LoadFailed(String),
    Created(BackupInfo),
    CreateFailed(String),
    Restored(Box<RestoreReport>),
    RestoreFailed(String),
    Exported {
        path: PathBuf,
        bytes: usize,
    },
    ExportFailed(String),
    Imported(Box<OfflineImportReport>),
    ImportFailed(String),
}

/// 当前在跑的后台任务；同时只允许一个。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Busy {
    List,
    Create,
    Restore,
    Export,
    Import,
}

impl Busy {
    fn label(self) -> &'static str {
        match self {
            Self::List => t(BackupMsg::BusyList),
            Self::Create => t(BackupMsg::BusyCreate),
            Self::Restore => t(BackupMsg::BusyRestore),
            Self::Export => t(BackupMsg::BusyExport),
            Self::Import => t(BackupMsg::BusyImport),
        }
    }
}

/// 已确认、等待真正执行的恢复；`path` 是备份文件的完整路径。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ArmedRestore {
    path: String,
    name: String,
}

/// 恢复的两段确认状态。同一时间最多一行处于确认状态；状态本体是通用的
/// [`ArmedConfirm`]，这里只提供「按行装填」的入口。
#[derive(Debug, Default)]
struct RestoreConfirm {
    confirm: ArmedConfirm<ArmedRestore>,
}

impl RestoreConfirm {
    /// 置为确认状态；点另一行会替换掉原来那行。
    fn arm(&mut self, path: impl Into<String>, name: impl Into<String>) {
        self.confirm.arm(ArmedRestore {
            path: path.into(),
            name: name.into(),
        });
    }

    fn disarm(&mut self) {
        self.confirm.disarm();
    }

    fn armed(&self) -> Option<&ArmedRestore> {
        self.confirm.armed()
    }

    fn armed_for(&self, path: &str) -> Option<&ArmedRestore> {
        self.confirm.armed().filter(|armed| armed.path == path)
    }

    #[cfg(test)]
    fn is_armed(&self, path: &str) -> bool {
        self.armed_for(path).is_some()
    }

    /// 取走确认状态：只有第一次调用拿得到，避免连点恢复两次。
    fn confirm(&mut self) -> Option<ArmedRestore> {
        self.confirm.take()
    }
}

/// 开始一次恢复 / 导入：抢同步权，抢不到就返回要给用户看的话。守卫活到操作结束，
/// 期间同步进不来，安装前检查也会拦着更新器。
fn begin_restore() -> Result<SyncInFlight, String> {
    SyncInFlight::acquire(ExclusiveKind::Restore)
        .map_err(|holder| exclusive_busy_message(ExclusiveKind::Restore, holder).to_owned())
}

/// 离线包导入前的输入校验。密码不裁剪（前后空格可能是密码的一部分），路径裁剪后使用。
fn offline_request(password: &str, path: &str) -> Result<(String, PathBuf), String> {
    validate_password(password)?;
    let path = path.trim();
    if path.is_empty() {
        return Err(t(BackupMsg::PackagePathEmpty).to_owned());
    }
    Ok((password.to_owned(), PathBuf::from(path)))
}

/// 导出前的输入校验：和 0.2.14 一样要求密码不少于 [`MIN_PASSWORD_CHARS`] 个字符、
/// 再输一遍一致（忘了密码文件就打不开，输错一遍比没输更糟）。
fn export_request(password: &str, confirm: &str, path: &str) -> Result<(String, PathBuf), String> {
    validate_export_password(password)?;
    check_confirmation(password, confirm)?;
    offline_request(password, path)
}

fn validate_export_password(password: &str) -> Result<(), String> {
    validate_password(password)?;
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(format!(
            "{}{MIN_PASSWORD_CHARS}{}",
            t(BackupMsg::PasswordTooShortPrefix),
            t(BackupMsg::PasswordTooShortSuffix)
        ));
    }
    Ok(())
}

fn check_confirmation(password: &str, confirm: &str) -> Result<(), String> {
    if password != confirm {
        return Err(t(BackupMsg::PasswordMismatch).to_owned());
    }
    Ok(())
}

fn validate_password(password: &str) -> Result<(), String> {
    if password.trim().is_empty() {
        return Err(t(BackupMsg::PasswordEmpty).to_owned());
    }
    Ok(())
}

/// 覆盖已有离线包的两段确认。
#[derive(Debug, Default)]
struct OverwriteConfirm {
    armed: ArmedConfirm<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverwriteCheck {
    /// 文件不存在，或者这次已经确认过了：可以直接写。
    Write,
    /// 文件存在但还没确认：只置为确认状态，不写。
    Confirm,
}

impl OverwriteConfirm {
    /// 文件存在时第一次返回 [`OverwriteCheck::Confirm`]，再次调用返回 `Write`。
    /// 换了路径要重新确认，避免把「确认覆盖 A」用到 B 上。
    fn check(&mut self, path: &Path, exists: bool) -> OverwriteCheck {
        if exists && !self.is_armed_for(path) {
            self.armed.arm(path.to_owned());
            return OverwriteCheck::Confirm;
        }
        self.armed.disarm();
        OverwriteCheck::Write
    }

    fn is_armed_for(&self, path: &Path) -> bool {
        self.armed.armed().is_some_and(|armed| armed == path)
    }

    fn disarm(&mut self) {
        self.armed.disarm();
    }

    /// 取走确认状态；点「确认覆盖」时只可能写一次。
    fn take(&mut self) -> Option<PathBuf> {
        self.armed.take()
    }

    #[cfg(test)]
    fn is_armed(&self) -> bool {
        self.armed.is_armed()
    }
}

/// 导入结果给界面的文本：第一行和 0.2.14 的提示一致，后面是各类记录的写入统计。
fn import_summary(report: &OfflineImportReport) -> String {
    let applied = &report.applied;
    let mut text = format!(
        "{head}{hosts}{updated}{hosts_updated}{deleted}{hosts_deleted}\
         {identities}{identities_added}{proxies}{proxies_added}{tail}",
        head = t(BackupMsg::ImportSummaryPrefix),
        hosts = applied.hosts.inserted,
        updated = t(BackupMsg::ImportSummaryUpdated),
        hosts_updated = applied.hosts.updated,
        deleted = t(BackupMsg::ImportSummaryDeleted),
        hosts_deleted = applied.hosts.deleted,
        identities = t(BackupMsg::ImportSummaryIdentities),
        identities_added = applied.identities.inserted,
        proxies = t(BackupMsg::ImportSummaryProxies),
        proxies_added = applied.proxies.inserted,
        tail = t(BackupMsg::ImportSummarySuffix),
    );
    if report.conflicts > 0 {
        text.push_str(&format!(
            "{}{}{}",
            t(BackupMsg::ConflictsKeptNewerPrefix),
            report.conflicts,
            t(BackupMsg::ConflictsKeptNewerSuffix)
        ));
    }
    if !report.nulled_references.is_empty() {
        text.push_str(&format!(
            "{}{}{}",
            t(BackupMsg::NulledReferencesPrefix),
            report.nulled_references.len(),
            t(BackupMsg::NulledReferencesSuffix)
        ));
    }
    text.push_str(&format!(
        "{}{}",
        t(BackupMsg::SafetyBackupPrefix),
        report.safety_backup.name
    ));
    text.push('\n');
    text.push_str(&applied_text(applied));
    text
}

/// 一类记录的「新增/更新/删除」片段：`prefix` 后面接三个数字。
fn counts_fragment(prefix: &str, counts: &ChangeCounts) -> String {
    format!(
        "{prefix}{}{updated}{}{deleted}{}",
        counts.inserted,
        counts.updated,
        counts.deleted,
        updated = t(BackupMsg::AppliedUpdated),
        deleted = t(BackupMsg::AppliedDeleted),
    )
}

fn applied_text(applied: &ApplyReport) -> String {
    let mut text = counts_fragment(t(BackupMsg::AppliedPrefix), &applied.hosts);
    text.push_str(&counts_fragment(
        t(BackupMsg::AppliedIdentitiesPrefix),
        &applied.identities,
    ));
    text.push_str(&counts_fragment(
        t(BackupMsg::AppliedGroupsPrefix),
        &applied.groups,
    ));
    text.push_str(&counts_fragment(
        t(BackupMsg::AppliedProxiesPrefix),
        &applied.proxies,
    ));
    text.push_str(&format!(
        "{settings}{settings_written}{known_hosts}{known_hosts_added}\
         {conflicts}{conflicts_stored}{tail}",
        settings = t(BackupMsg::AppliedSettingsPrefix),
        settings_written = applied.settings_written,
        known_hosts = t(BackupMsg::AppliedKnownHostsPrefix),
        known_hosts_added = applied.known_hosts_added,
        conflicts = t(BackupMsg::AppliedConflictsPrefix),
        conflicts_stored = applied.conflicts_stored,
        tail = t(BackupMsg::AppliedSuffix),
    ));
    if applied.known_hosts_skipped > 0 {
        text.push_str(&format!(
            "{}{}{}",
            t(BackupMsg::SkippedHostKeysInlinePrefix),
            applied.known_hosts_skipped,
            t(BackupMsg::SkippedHostKeysSuffix)
        ));
    }
    text
}

/// 恢复结果：各类记录的新增/更新/删除数、设置和 known_hosts 条数，以及安全备份名。
fn restore_summary(report: &RestoreReport) -> String {
    let applied = &report.applied;
    let mut text = counts_fragment(t(BackupMsg::RestoreSummaryPrefix), &applied.hosts);
    text.push_str(&counts_fragment(
        t(BackupMsg::AppliedIdentitiesPrefix),
        &applied.identities,
    ));
    text.push_str(&counts_fragment(
        t(BackupMsg::AppliedGroupsPrefix),
        &applied.groups,
    ));
    text.push_str(&counts_fragment(
        t(BackupMsg::AppliedProxiesPrefix),
        &applied.proxies,
    ));
    text.push_str(&format!(
        "{settings}{settings_written}{known_hosts}{known_hosts_added}\
         {conflicts}{conflicts_stored}{host_memory}{host_memory_count}\
         {forwards}{forwards_count}{safety_backup}{safety_backup_name}",
        settings = t(BackupMsg::AppliedSettingsPrefix),
        settings_written = applied.settings_written,
        known_hosts = t(BackupMsg::AppliedKnownHostsPrefix),
        known_hosts_added = applied.known_hosts_added,
        conflicts = t(BackupMsg::AppliedConflictsPrefix),
        conflicts_stored = applied.conflicts_stored,
        host_memory = t(BackupMsg::RestoreSummaryHostMemory),
        host_memory_count = report.extras.host_memory,
        forwards = t(BackupMsg::RestoreSummaryForwards),
        forwards_count = report.extras.forwards,
        safety_backup = t(BackupMsg::RestoreSummarySafetyBackupPrefix),
        safety_backup_name = report.safety_backup.name,
    ));
    if applied.known_hosts_skipped > 0 {
        // 放在安全备份提示之前的同一行末尾不好找，单独起一行。
        text.push_str(&format!(
            "{}{}{}",
            t(BackupMsg::SkippedHostKeysPrefix),
            applied.known_hosts_skipped,
            t(BackupMsg::SkippedHostKeysSuffix)
        ));
    }
    text
}

/// 本地备份最多保留几份（设置项 `sync_backup_keep`，1–200）；读不出来用 [`FALLBACK_KEEP`]。
fn read_keep(data_dir: &Path) -> usize {
    if !data_dir.join("zssh.db").is_file() {
        return FALLBACK_KEEP;
    }
    open_db_at(data_dir)
        .ok()
        .and_then(|db| db.sync_backup_keep().ok())
        .unwrap_or(FALLBACK_KEEP)
}

/// 改保留份数。和 0.2.14 一样：调小以后多出来的旧备份要到下一次备份时才清理，这里不删任何东西。
fn write_keep(data_dir: &Path, keep: usize) -> Result<usize, String> {
    let keep = keep.clamp(1, zssh_core::store::settings::SYNC_BACKUP_KEEP_MAX);
    let db = open_db_at(data_dir)?;
    db.setting_set(
        zssh_core::store::settings::SYNC_BACKUP_KEEP_KEY,
        &keep.to_string(),
    )
    .map_err(|err| format!("{}{err}", t(BackupMsg::SaveKeepFailedPrefix)))?;
    Ok(keep)
}

/// 读备份列表；界面路径用生产 DPAPI。
fn list_backups(data_dir: &Path) -> Result<Vec<BackupDetail>, String> {
    list_backups_with(data_dir, &DpapiSealer)
}

/// 同 [`list_backups`]，加密实现由调用方给，测试用它绕开 DPAPI。
fn list_backups_with(
    data_dir: &Path,
    sealer: &dyn SecretSealer,
) -> Result<Vec<BackupDetail>, String> {
    BackupStore::new(data_dir, sealer)
        .list_details()
        .map_err(|err| err.to_string())
}

/// 新建备份；界面路径用生产 DPAPI。
fn create_backup(data_dir: &Path) -> Result<BackupInfo, String> {
    create_backup_with(data_dir, &DpapiSealer, env!("CARGO_PKG_VERSION"))
}

/// 同 [`create_backup`]，加密实现和版本号由调用方给，测试用它绕开 DPAPI。
fn create_backup_with(
    data_dir: &Path,
    sealer: &dyn SecretSealer,
    app_version: &str,
) -> Result<BackupInfo, String> {
    let db = open_db_at(data_dir)?;
    let device_id = db
        .sync_device_id()
        .map_err(|err| format!("{}{err}", t(BackupMsg::ReadDeviceIdFailedPrefix)))?;
    let keep = db.sync_backup_keep().unwrap_or(FALLBACK_KEEP);
    let known_hosts = data_dir.join("known_hosts");
    let now = now_secs();
    let snapshot = snapshot_local(&db, sealer, &device_id, Some(&known_hosts), now)
        .map_err(|err| err.to_string())?;
    let local = LocalExtras::read(&db).map_err(|err| err.to_string())?;
    // 本地备份带上同步账号设置（整个备份文件由本机 DPAPI 加密，不会上传）。
    let accounts = SyncAccounts::read(&db, sealer).map_err(|err| err.to_string())?;
    BackupStore::new(data_dir, sealer)
        .create(
            &snapshot,
            &local,
            accounts.as_ref(),
            now,
            keep,
            Some(app_version),
        )
        .map_err(|err| err.to_string())
}

/// 恢复备份；界面路径用生产 DPAPI。
fn restore_backup(data_dir: &Path, path: &Path) -> Result<RestoreReport, String> {
    restore_backup_with(data_dir, path, &DpapiSealer)
}

/// 同 [`restore_backup`]，加密实现由调用方给，测试用它绕开 DPAPI。
fn restore_backup_with(
    data_dir: &Path,
    path: &Path,
    sealer: &dyn SecretSealer,
) -> Result<RestoreReport, String> {
    let db = open_db_at(data_dir)?;
    let device_id = db
        .sync_device_id()
        .map_err(|err| format!("{}{err}", t(BackupMsg::ReadDeviceIdFailedPrefix)))?;
    // 走引擎的入口而不是直接调 `BackupStore::restore`：引擎会先检查备份必须在备份目录内、
    // 文件名合法，并把同步基线里的删除记录一并带上，恢复出来的记录不会被基线墓碑再删掉。
    sync_engine(&db, sealer, device_id, data_dir)
        .restore_backup(path)
        .map_err(|err| err.to_string())
}

/// 离线包的同步引擎；界面路径用生产 DPAPI，测试传本地 sealer 绕开 DPAPI。
fn sync_engine<'a>(
    db: &'a Database,
    sealer: &'a dyn SecretSealer,
    device_id: String,
    data_dir: &Path,
) -> SyncEngine<'a> {
    SyncEngine::new(
        db,
        sealer,
        device_id,
        data_dir,
        Some(data_dir.join("known_hosts")),
        Some(env!("CARGO_PKG_VERSION").to_owned()),
    )
}

/// 导出离线包并写到 `path`，返回字节数。核心错误原样显示（含密码太短等）。
///
/// 用核心的 `export_offline`（墓碑不带走，换一台电脑导入不会误删那边的记录），
/// 写文件先写同目录的临时文件再改名替换：写到一半失败（磁盘满、U 盘拔了）时，
/// 已经确认覆盖的旧离线包不会被截断成半截文件。
fn export_offline_to(
    data_dir: &Path,
    sealer: &dyn SecretSealer,
    password: &str,
    path: &Path,
) -> Result<usize, String> {
    let db = open_db_at(data_dir)?;
    let device_id = db
        .sync_device_id()
        .map_err(|err| format!("{}{err}", t(BackupMsg::ReadDeviceIdFailedPrefix)))?;
    let engine = sync_engine(&db, sealer, device_id, data_dir);
    let bytes = engine
        .export_offline(password)
        .map_err(|err| err.to_string())?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("{}{err}", t(BackupMsg::CreatePackageDirFailedPrefix)))?;
    }
    write_atomic(path, &bytes)
        .map_err(|err| format!("{}{err}", t(BackupMsg::WritePackageFailedPrefix)))?;
    Ok(bytes.len())
}

/// 先写同目录的临时文件（并刷到磁盘），再改名替换目标。任何一步失败都删掉临时文件，
/// 目标文件原样留着。临时文件名带进程号，两个窗口同时导出到同一个目标也不会互相踩。
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut temp_name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    temp_name.push(format!(".{}.zssh-tmp", std::process::id()));
    let temp = path.with_file_name(temp_name);
    let written = (|| {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    let result = written.and_then(|()| std::fs::rename(&temp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// 读离线包并导入。用核心的 `import_offline`：导入前自动备份，不动同步基线、不带墓碑，
/// 所以下一次云同步不会把刚导入的记录当成"云端删了"而删掉。
fn import_offline_from(
    data_dir: &Path,
    sealer: &dyn SecretSealer,
    path: &Path,
    password: &str,
    mode: OfflineImportMode,
) -> Result<OfflineImportReport, String> {
    let bytes = zssh_core::sync::offline::read_offline_file(path)?;
    let db = open_db_at(data_dir)?;
    let device_id = db
        .sync_device_id()
        .map_err(|err| format!("{}{err}", t(BackupMsg::ReadDeviceIdFailedPrefix)))?;
    sync_engine(&db, sealer, device_id, data_dir)
        .import_offline(&bytes, password, mode)
        .map_err(|err| err.to_string())
}

/// 核心的备份文件名和记录时间都用 Unix **秒**（`compact_timestamp` 只认 10 位
/// 日期），所以从 `now_ms()` 换算过来；直接传毫秒会让文件名不符合备份命名规则。
fn now_secs() -> i64 {
    (now_ms() / 1000) as i64
}

/// 后台任务自己开库；不复用界面里的连接，免得跨线程持有 SQLite 连接。
fn open_db_at(data_dir: &Path) -> Result<Database, String> {
    std::fs::create_dir_all(data_dir)
        .map_err(|err| format!("{}{err}", t(BackupMsg::CreateDataDirFailedPrefix)))?;
    Database::open(&data_dir.join("zssh.db"))
        .map_err(|err| format!("{}{err}", t(BackupMsg::OpenDatabaseFailedPrefix)))
}

#[cfg(test)]
mod tests {
    // 不写 `use super::*`：父模块 glob 进来的 `gpui_kit::test` 会盖掉内置的
    // `#[test]` 属性宏，展开时直接撞递归上限。
    use std::path::{Path, PathBuf};

    use zssh_core::model::{AuthMethod, HostDraft, ProxyMode};
    use zssh_core::store::Database;
    use zssh_core::sync::apply::{ApplyReport, ChangeCounts};
    use zssh_core::sync::backup::BackupInfo;
    use zssh_core::sync::engine::{SyncEngine, SyncStatus};
    use zssh_core::sync::error::SyncError;
    use zssh_core::sync::offline::{OfflineImportMode, OfflineImportReport};
    use zssh_core::sync::sealer::SecretSealer;

    use super::{
        OverwriteCheck, OverwriteConfirm, RestoreConfirm, create_backup_with, export_offline_to,
        export_request, import_offline_from, import_summary, list_backups_with, offline_request,
        parse_keep, read_keep, restore_backup_with, restore_summary, write_keep,
    };

    /// 和核心里的 TestSealer 同一套可逆混淆；那个类型是 `cfg(test)` 私有的，
    /// 这个二进制测试拿不到，只能在这里放一份。
    struct TestSealer(u8);

    impl SecretSealer for TestSealer {
        fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, SyncError> {
            let mut out = vec![b'T', self.0];
            out.extend(plaintext.iter().map(|byte| byte ^ self.0));
            Ok(out)
        }

        fn open(&self, blob: &[u8]) -> Result<Vec<u8>, SyncError> {
            if blob.len() < 2 || blob[0] != b'T' || blob[1] != self.0 {
                return Err(SyncError::InvalidData("不是 TestSealer 的密文".to_owned()));
            }
            Ok(blob[2..].iter().map(|byte| byte ^ self.0).collect())
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zssh-backup-demo-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    #[test]
    fn the_keep_default_is_five_and_a_stored_value_wins() {
        let dir = temp_dir("keep-default");
        // 还没有数据库 / 没设置过：5。
        assert_eq!(read_keep(&dir), 5);
        drop(Database::open(&dir.join("zssh.db")).expect("建库"));
        assert_eq!(read_keep(&dir), 5);
        // 以前存过 20 的用户保持 20。
        assert_eq!(write_keep(&dir, 20), Ok(20));
        assert_eq!(read_keep(&dir), 20);
    }

    #[test]
    fn the_keep_box_only_accepts_whole_numbers_from_1_to_200() {
        for ok in ["1", "5", " 30 ", "200", "007"] {
            assert!(parse_keep(ok).is_ok(), "{ok:?}");
        }
        assert_eq!(parse_keep("30"), Ok(30));
        for bad in [
            "",
            "  ",
            "0",
            "201",
            "-1",
            "+5",
            "abc",
            "5份",
            "3.5",
            "1e2",
            "99999999999999999999",
        ] {
            let err = parse_keep(bad).expect_err(bad);
            assert!(err.contains("1–200"), "{bad:?}：{err}");
        }
    }

    #[test]
    fn invalid_input_is_not_saved_and_a_valid_one_is() {
        // 界面的保存链路：输入框文字 → parse_keep → write_keep。
        let dir = temp_dir("keep-input");
        let save = |text: &str| parse_keep(text).and_then(|keep| write_keep(&dir, keep));
        for bad in ["0", "201", "abc"] {
            assert!(save(bad).is_err(), "{bad}");
            assert_eq!(read_keep(&dir), 5, "{bad} 不能保存");
        }
        assert_eq!(save("30"), Ok(30));
        assert_eq!(read_keep(&dir), 30);
        // 之后再输非法值，已经存的 30 不动。
        assert!(save("0").is_err());
        assert_eq!(read_keep(&dir), 30);
    }

    fn host_draft() -> HostDraft {
        host_draft_named("测试机", "example.invalid")
    }

    fn host_draft_named(name: &str, host: &str) -> HostDraft {
        HostDraft {
            protocol: None,
            id: None,
            group_id: None,
            name: name.to_owned(),
            host: host.to_owned(),
            port: 22,
            username: "tester".to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: None,
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: ProxyMode::Default,
            tags: Vec::new(),
            sort: 0,
            legacy_algorithms: false,
        }
    }

    fn applied() -> ApplyReport {
        ApplyReport {
            groups: ChangeCounts::default(),
            hosts: ChangeCounts {
                inserted: 1,
                updated: 0,
                deleted: 2,
            },
            identities: ChangeCounts::default(),
            proxies: ChangeCounts::default(),
            settings_written: 3,
            known_hosts_added: 4,
            known_hosts_skipped: 0,
            conflicts_stored: 1,
        }
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(crate::format::size(0), "0 B");
        assert_eq!(crate::format::size(1023), "1023 B");
        assert_eq!(crate::format::size(1024), "1.0 KB");
        assert_eq!(crate::format::size(1536), "1.5 KB");
        assert_eq!(crate::format::size(1024 * 1024), "1.0 MB");
        assert_eq!(crate::format::size(5 * 1024 * 1024 * 1024), "5.0 GB");
        assert_eq!(crate::format::size(1024 * 1024 * 1024 * 1024), "1.0 TB");
    }

    #[test]
    fn first_click_arms_and_only_one_row_stays_armed() {
        let mut confirm = RestoreConfirm::default();
        assert!(!confirm.is_armed("a"));

        confirm.arm("a", "甲");
        assert!(confirm.is_armed("a"));
        assert_eq!(confirm.armed().map(|armed| armed.name.as_str()), Some("甲"));

        confirm.arm("b", "乙");
        assert!(!confirm.is_armed("a"));
        assert!(confirm.is_armed("b"));
    }

    #[test]
    fn cancel_disarms() {
        let mut confirm = RestoreConfirm::default();
        confirm.arm("a", "甲");
        confirm.disarm();
        assert!(!confirm.is_armed("a"));
        assert!(confirm.confirm().is_none());
    }

    #[test]
    fn confirm_can_only_be_taken_once() {
        let mut confirm = RestoreConfirm::default();
        confirm.arm("a", "甲");
        let first = confirm.confirm().expect("第一次确认应成功");
        assert_eq!(first.path, "a");
        assert_eq!(first.name, "甲");
        assert!(confirm.confirm().is_none());
        assert!(!confirm.is_armed("a"));
    }

    #[test]
    fn create_list_restore_round_trip() {
        let dir = temp_dir("round-trip");
        let sealer = TestSealer(0x5a);

        {
            let db = Database::open(&dir.join("zssh.db")).expect("打开临时库");
            db.host_save(&host_draft()).expect("保存主机");
        }

        let info = create_backup_with(&dir, &sealer, "9.9.9").expect("创建备份");
        let details = list_backups_with(&dir, &sealer).expect("读取备份列表");
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].info.name, info.name);
        assert!(details[0].readable);
        assert_eq!(details[0].app_version.as_deref(), Some("9.9.9"));
        assert_eq!(details[0].host_count, Some(1));

        {
            let db = Database::open(&dir.join("zssh.db")).expect("重新打开临时库");
            let hosts = db.host_list().expect("读取主机");
            assert_eq!(hosts.len(), 1);
            db.host_delete(hosts[0].id).expect("删除主机");
        }

        let report = restore_backup_with(&dir, Path::new(&info.path), &sealer).expect("恢复备份");
        assert_eq!(report.applied.hosts.inserted, 1);
        assert!(report.safety_backup.name.starts_with("zssh-backup-"));
        let summary = restore_summary(&report);
        assert!(summary.contains("恢复完成"));
        assert!(summary.contains(&report.safety_backup.name));

        {
            let db = Database::open(&dir.join("zssh.db")).expect("再次打开临时库");
            assert_eq!(db.host_list().expect("读取恢复后的主机").len(), 1);
        }
        // 恢复前的安全备份和新恢复的备份都在列表里。
        assert_eq!(
            list_backups_with(&dir, &sealer)
                .expect("读取备份列表")
                .len(),
            2
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_and_whitespace_inputs_are_rejected() {
        assert_eq!(
            offline_request("", "C:\\data\\offline-export.zssh").unwrap_err(),
            "密码不能为空"
        );
        assert_eq!(
            offline_request("  \t ", "C:\\data\\offline-export.zssh").unwrap_err(),
            "密码不能为空"
        );
        assert_eq!(offline_request("secret", "").unwrap_err(), "包路径不能为空");
        assert_eq!(
            offline_request("secret", " \r\n ").unwrap_err(),
            "包路径不能为空"
        );

        // 路径裁剪，密码原样保留（前后空格可能是密码的一部分）。
        let (password, path) =
            offline_request("  secret  ", "  C:\\data\\offline-export.zssh  ").expect("输入合法");
        assert_eq!(password, "  secret  ");
        assert_eq!(path, PathBuf::from("C:\\data\\offline-export.zssh"));
    }

    #[test]
    fn overwrite_needs_two_steps_and_cancel_disarms() {
        let mut confirm = OverwriteConfirm::default();
        let path = PathBuf::from("C:\\data\\offline-export.zssh");

        assert_eq!(confirm.check(&path, true), OverwriteCheck::Confirm);
        assert!(confirm.is_armed());
        assert!(confirm.is_armed_for(&path));

        // 第二次才真正写。
        assert_eq!(confirm.check(&path, true), OverwriteCheck::Write);
        assert!(!confirm.is_armed());

        // 取消覆盖后再点导出要重新确认。
        assert_eq!(confirm.check(&path, true), OverwriteCheck::Confirm);
        confirm.disarm();
        assert!(!confirm.is_armed());
        assert_eq!(confirm.check(&path, true), OverwriteCheck::Confirm);

        // 目标不存在时直接写。
        let fresh = PathBuf::from("C:\\data\\new.zssh");
        assert_eq!(confirm.check(&fresh, false), OverwriteCheck::Write);
        assert!(!confirm.is_armed());
    }

    #[test]
    fn overwrite_confirmation_is_per_path() {
        let mut confirm = OverwriteConfirm::default();
        let a = PathBuf::from("C:\\data\\a.zssh");
        let b = PathBuf::from("C:\\data\\b.zssh");

        assert_eq!(confirm.check(&a, true), OverwriteCheck::Confirm);
        // 换了路径要重新确认，不能把「确认覆盖 a」用到 b 上。
        assert_eq!(confirm.check(&b, true), OverwriteCheck::Confirm);
        assert!(confirm.is_armed_for(&b));
        assert!(!confirm.is_armed_for(&a));
    }

    fn safety_backup() -> BackupInfo {
        BackupInfo {
            path: "C:\\data\\backups\\zssh-backup-20260101-010203.bin".to_owned(),
            name: "zssh-backup-20260101-010203.bin".to_owned(),
            size: 1,
            stamp: "20260101-010203".to_owned(),
            sequence: 1,
        }
    }

    #[test]
    fn import_summary_matches_the_stable_wording_and_shows_counts() {
        let report = OfflineImportReport {
            safety_backup: safety_backup(),
            applied: applied(),
            conflicts: 0,
            nulled_references: Vec::new(),
            accounts: zssh_core::sync::accounts::AccountsRestore::NotInBackup,
        };
        let text = import_summary(&report);
        assert!(
            text.starts_with("已导入：主机新增 1 / 更新 0 / 删除 2，凭据新增 0，代理新增 0。"),
            "{text}"
        );
        assert!(
            text.contains("导入前的状态已自动备份为 zssh-backup-20260101-010203.bin"),
            "{text}"
        );
        assert!(!text.contains("两边都改过"), "{text}");
        assert!(text.contains("设置 3 项"), "{text}");
        assert!(text.contains("known_hosts 4 条"), "{text}");

        let with_conflicts = OfflineImportReport {
            conflicts: 2,
            nulled_references: vec!["host:a".to_owned()],
            ..report
        };
        let text = import_summary(&with_conflicts);
        assert!(
            text.contains("2 条两边都改过，保留了较新的，另一份在冲突页。"),
            "{text}"
        );
        assert!(text.contains("1 处引用指向不存在的记录"), "{text}");
    }

    #[test]
    fn export_needs_a_long_enough_password_typed_twice() {
        assert_eq!(
            export_request("", "", "C:\\x\\a.zssh").unwrap_err(),
            "密码不能为空"
        );
        assert_eq!(
            export_request("short", "short", "C:\\x\\a.zssh").unwrap_err(),
            "密码至少 8 个字符"
        );
        assert_eq!(
            export_request("long-enough-1", "long-enough-2", "C:\\x\\a.zssh").unwrap_err(),
            "两次输入的密码不一样"
        );
        assert_eq!(
            export_request("long-enough-1", "long-enough-1", "  ").unwrap_err(),
            "包路径不能为空"
        );
        let (password, path) =
            export_request("long-enough-1", "long-enough-1", " C:\\x\\a.zssh ").expect("输入合法");
        assert_eq!(password, "long-enough-1");
        assert_eq!(path, PathBuf::from("C:\\x\\a.zssh"));
        // 导入只要求非空：包是别人导出的，长度由导出那边管。
        assert!(offline_request("short", "C:\\x\\a.zssh").is_ok());
    }

    const PACKAGE_PASSWORD: &str = "offline-test-password";

    fn host_names(dir: &Path) -> Vec<String> {
        let db = Database::open(&dir.join("zssh.db")).expect("打开库");
        let mut names: Vec<String> = db
            .host_list()
            .expect("读取主机")
            .into_iter()
            .map(|host| host.name)
            .collect();
        names.sort();
        names
    }

    fn seed(dir: &Path, names: &[&str]) {
        let db = Database::open(&dir.join("zssh.db")).expect("打开库");
        for name in names {
            db.host_save(&host_draft_named(name, &format!("{name}.example.invalid")))
                .expect("保存主机");
        }
    }

    #[test]
    fn offline_package_round_trip_merge_and_replace() {
        let source_dir = temp_dir("offline-source");
        let target_dir = temp_dir("offline-target");
        let source_sealer = TestSealer(0x5a);
        let target_sealer = TestSealer(0x33);
        seed(&source_dir, &["web", "db"]);
        seed(&target_dir, &["local-only"]);

        let path = source_dir.join("offline-export.zssh");
        let written = export_offline_to(&source_dir, &source_sealer, PACKAGE_PASSWORD, &path)
            .expect("导出离线包");
        assert!(written > 0);
        assert_eq!(
            std::fs::metadata(&path).expect("离线包存在").len(),
            written as u64
        );

        // 合并：两边都留。
        let report = import_offline_from(
            &target_dir,
            &target_sealer,
            &path,
            PACKAGE_PASSWORD,
            OfflineImportMode::Merge,
        )
        .expect("合并导入");
        assert_eq!(report.applied.hosts.inserted, 2);
        assert_eq!(report.applied.hosts.deleted, 0);
        assert_eq!(host_names(&target_dir), vec!["db", "local-only", "web"]);
        assert!(report.safety_backup.name.starts_with("zssh-backup-"));

        // 替换：本机变成和包一样，包里没有的被删掉。
        let report = import_offline_from(
            &target_dir,
            &target_sealer,
            &path,
            PACKAGE_PASSWORD,
            OfflineImportMode::Replace,
        )
        .expect("替换导入");
        assert_eq!(report.applied.hosts.deleted, 1);
        assert_eq!(host_names(&target_dir), vec!["db", "web"]);

        // 密码错：报错，本机一个字节不动。
        let error = import_offline_from(
            &target_dir,
            &target_sealer,
            &path,
            "not-the-password",
            OfflineImportMode::Replace,
        )
        .unwrap_err();
        assert!(error.contains("主密码错误"), "{error}");
        assert_eq!(host_names(&target_dir), vec!["db", "web"]);

        let _ = std::fs::remove_dir_all(&source_dir);
        let _ = std::fs::remove_dir_all(&target_dir);
    }

    #[test]
    fn a_short_export_password_is_refused_by_the_core_too() {
        let dir = temp_dir("offline-short");
        let error =
            export_offline_to(&dir, &TestSealer(1), "short", &dir.join("a.zssh")).unwrap_err();
        assert!(error.contains("至少"), "{error}");
        assert!(!dir.join("a.zssh").exists(), "失败时不能留下文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D2-01：离线导入不改同步基线，之后的云同步不会把刚导入的记录删掉。
    /// 旧实现用云同步的 `import_package`，把"导入结果"存成基线；云端并不是那个状态，
    /// 下一次同步就把导入的记录当成"云端删了"。
    #[tokio::test]
    async fn offline_import_leaves_the_baseline_alone_and_the_next_cloud_sync_keeps_the_records() {
        use zssh_core::sync::storage::MemoryStorage;

        let cloud = MemoryStorage::new();
        let sealer = TestSealer(0x44);
        let other_sealer = TestSealer(0x45);
        let this_dir = temp_dir("baseline-this");
        let other_dir = temp_dir("baseline-other");
        seed(&this_dir, &["shared"]);
        // 两台电脑各自建的同名主机 uid 不同，会被当成两台；这里用不同的名字。
        seed(&other_dir, &["imported"]);

        // 这台电脑先和云端对上一次：基线里有 shared。
        {
            let db = Database::open(&this_dir.join("zssh.db")).expect("打开库");
            let engine = SyncEngine::new(&db, &sealer, "dev-this", &this_dir, None, None);
            let report = engine
                .sync(&cloud, PACKAGE_PASSWORD, None)
                .await
                .expect("首次同步");
            assert_eq!(report.status, SyncStatus::Completed);
        }
        let baseline_path = this_dir.join("sync-baseline.bin");
        let baseline_before = std::fs::read(&baseline_path).expect("基线应已写下");
        let cloud_version_before = cloud.current_version();

        // 别的电脑导出离线包，导入到这台。
        let package = other_dir.join("offline-export.zssh");
        export_offline_to(&other_dir, &other_sealer, PACKAGE_PASSWORD, &package).expect("导出");
        import_offline_from(
            &this_dir,
            &sealer,
            &package,
            PACKAGE_PASSWORD,
            OfflineImportMode::Merge,
        )
        .expect("合并导入");
        assert_eq!(host_names(&this_dir), vec!["imported", "shared"]);
        assert_eq!(
            std::fs::read(&baseline_path).expect("基线还在"),
            baseline_before,
            "导入不能改同步基线"
        );
        assert_eq!(
            cloud.current_version(),
            cloud_version_before,
            "导入不碰云端"
        );

        // 下一次云同步：导入的记录是"本地新增"，要被推上云，而不是被删掉。
        {
            let db = Database::open(&this_dir.join("zssh.db")).expect("打开库");
            let engine = SyncEngine::new(&db, &sealer, "dev-this", &this_dir, None, None);
            let report = engine
                .sync(&cloud, PACKAGE_PASSWORD, None)
                .await
                .expect("导入后的同步");
            assert_eq!(report.status, SyncStatus::Completed, "{}", report.summary);
        }
        assert_eq!(
            host_names(&this_dir),
            vec!["imported", "shared"],
            "云同步后导入的记录还在"
        );
        assert_ne!(
            cloud.current_version(),
            cloud_version_before,
            "导入的记录被推上了云端"
        );

        let _ = std::fs::remove_dir_all(&this_dir);
        let _ = std::fs::remove_dir_all(&other_dir);
    }

    /// D2-14：导出先写临时文件再改名；写失败时旧的离线包原样留着。
    #[test]
    fn export_replaces_the_target_atomically_and_leaves_no_temp_file() {
        let dir = temp_dir("atomic-ok");
        let target = dir.join("offline-export.zssh");
        std::fs::write(&target, b"old package").expect("先放一个旧包");
        super::write_atomic(&target, b"new package").expect("原子替换");
        assert_eq!(std::fs::read(&target).expect("读取"), b"new package");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("列目录")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec!["offline-export.zssh".to_owned()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_write_keeps_the_old_package_and_cleans_up() {
        let dir = temp_dir("atomic-fail");
        let target = dir.join("offline-export.zssh");
        std::fs::write(&target, b"old package").expect("先放一个旧包");

        // 写不了临时文件（同名位置被一个目录占了）：旧包原样，目标没被截断。
        let temp = dir.join(format!(
            "offline-export.zssh.{}.zssh-tmp",
            std::process::id()
        ));
        std::fs::create_dir(&temp).expect("占住临时文件名");
        let error = super::write_atomic(&target, b"new package").unwrap_err();
        assert!(!error.to_string().is_empty());
        assert_eq!(std::fs::read(&target).expect("读取"), b"old package");
        std::fs::remove_dir(&temp).expect("清理占位目录");

        // 改名失败（目标是个目录）：临时文件被删掉，目录还在。
        let blocked = dir.join("blocked.zssh");
        std::fs::create_dir(&blocked).expect("目标是目录");
        assert!(super::write_atomic(&blocked, b"new package").is_err());
        assert!(blocked.is_dir());
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("列目录")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".zssh-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不能留临时文件：{leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D2-06：恢复走引擎入口——备份必须在备份目录里，同步基线一并参与。
    #[test]
    fn restore_goes_through_the_engine_checks() {
        let dir = temp_dir("restore-engine");
        let sealer = TestSealer(0x5a);
        seed(&dir, &["web"]);
        let info = create_backup_with(&dir, &sealer, "9.9.9").expect("创建备份");

        // 备份目录以外的文件：引擎的 check_path 拒绝（直接调 BackupStore::restore 不会拦）。
        let outside = dir.join("outside.bin");
        std::fs::copy(&info.path, &outside).expect("拷到备份目录外");
        let error = restore_backup_with(&dir, &outside, &sealer).unwrap_err();
        assert!(!error.is_empty());
        assert_eq!(host_names(&dir), vec!["web"], "被拒绝时本机不变");

        // 同步基线读不出来：引擎要把基线里的删除记录一起带上，读不了就不恢复，
        // 不能当成"没有删除"悄悄继续。
        std::fs::write(dir.join("sync-baseline.bin"), b"garbage").expect("写坏基线");
        let error = restore_backup_with(&dir, Path::new(&info.path), &sealer).unwrap_err();
        assert!(!error.is_empty());
        std::fs::remove_file(dir.join("sync-baseline.bin")).expect("删掉坏基线");

        // 好的情况照常恢复，并且不碰基线文件。
        {
            let db = Database::open(&dir.join("zssh.db")).expect("打开库");
            let hosts = db.host_list().expect("读取主机");
            db.host_delete(hosts[0].id).expect("删除主机");
        }
        let report = restore_backup_with(&dir, Path::new(&info.path), &sealer).expect("恢复备份");
        assert_eq!(report.applied.hosts.inserted, 1);
        assert_eq!(host_names(&dir), vec!["web"]);
        assert!(
            !dir.join("sync-baseline.bin").exists(),
            "恢复不能凭空写基线"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// D2-06：恢复 / 导入与同步互斥，进行期间安装前检查会拦着更新器。
    #[test]
    fn restore_and_import_exclude_sync() {
        use crate::sync::{ExclusiveKind, SyncInFlight};

        let _serial = crate::sync::exclusive_test_lock();
        let sync = SyncInFlight::acquire(ExclusiveKind::Sync).expect("占住同步权");
        let error = super::begin_restore().unwrap_err();
        assert!(error.contains("同步正在进行"), "{error}");
        drop(sync);

        let restore = super::begin_restore().expect("空闲时能开始恢复");
        assert!(
            SyncInFlight::acquire(ExclusiveKind::Sync).is_err(),
            "恢复进行中不能同步"
        );
        assert!(
            super::begin_restore().is_err(),
            "恢复进行中也不能再开一个恢复 / 导入"
        );
        let reason = crate::install_state::blocking_reason().expect("恢复中要拦安装");
        assert!(reason.contains("备份恢复"), "{reason}");
        drop(restore);
        assert!(SyncInFlight::acquire(ExclusiveKind::Sync).is_ok());
    }
}
