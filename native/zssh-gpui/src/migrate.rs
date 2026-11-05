//! 从 Z-SSH 迁移（V2-U19）：设置 → 同步最上面的「从 Z-SSH 迁移」一块，以及欢迎页的提示卡 / 链接
//! 要用的全部逻辑。
//!
//! 两条路：
//! 1. **直接导入本机的 Z-SSH 数据**：找到 Z-SSH 的数据目录，把库复制到临时目录再读（核心的
//!    `zssh_import`，不碰原库、Z-SSH 开着就提示先退出），读出的数据交给现有的导入引擎；
//! 2. **导入 Z-SSH 导出的备份文件（`.zsshbak`）**：格式和 ZeeTerm 的离线备份是同一种（同一套加密 +
//!    同一份明文结构），所以直接复用 `SyncSnapshot::open`，不需要转换层。
//!
//! 两条路都先读出一份快照 → 预览（各类数量、会新增 / 更新 / 冲突多少）→ 选合并或替换 → 导入。
//! 导入前自动做一份本地备份（引擎里已有），导入和同步互斥（同一把 `SyncInFlight`）。
//!
//! 读库、DPAPI、SQLite、复制文件都在 [`crate::ssh_backend::runtime`] 的阻塞线程池里跑，界面只收 `mpsc`
//! 事件。逻辑函数都带着数据目录 / 加密实现参数，测试用它们绕开真实目录和 DPAPI。

#[cfg(debug_assertions)]
mod dev;
mod view;

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use ::gpui::{App, Task, Window};
use gpui_kit::component::input::InputState;
use gpui_kit::*;
use zssh_core::store::Database;
use zssh_core::sync::engine::SyncEngine;
use zssh_core::sync::error::SyncError;
use zssh_core::sync::import_preview::ImportPreview;
use zssh_core::sync::offline::{OfflineImportMode, OfflineImportReport};
use zssh_core::sync::package::SyncSnapshot;
use zssh_core::sync::sealer::{DpapiSealer, SecretSealer};
use zssh_core::sync::zssh_import::{
    DetectEnv, MIGRATE_DISMISSED_KEY, ZsshImportError, ZsshSource, detect, read_zssh, source_at,
};

use crate::i18n::{Msg, t};
use crate::sync::{ExclusiveKind, SyncInFlight, exclusive_busy_message};

/// 迁移块的界面文案（本文件里的固定文案）。
///
/// 数据文件夹、备份文件路径、核心库返回的错误原文一律原样显示。
#[derive(Clone, Copy, Debug)]
enum MigrateMsg {
    OwnFolder,
    NoDatabase,
    SaveFailedPrefix,
    CreateDataDirFailedPrefix,
    OpenDatabaseFailedPrefix,
    ReadSyncDeviceIdFailedPrefix,
    WrongBackupPassword,
    ChooseBackupFile,
    EnterBackupPassword,
    BusyDetecting,
    BusyReading,
    BusyPreviewing,
    BusyImporting,
    BackgroundTaskFailedPrefix,
    BackupPathPlaceholder,
    BackupPasswordPlaceholder,
}

impl Msg for MigrateMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::OwnFolder => "这是 ZeeTerm 自己的数据文件夹，不是 Z-SSH 的",
            Self::NoDatabase => {
                "这个文件夹里没有 Z-SSH 的数据库（应该有 data.db，默认在 %APPDATA%\\Z-ssh）"
            }
            Self::SaveFailedPrefix => "保存失败：",
            Self::CreateDataDirFailedPrefix => "创建数据目录失败：",
            Self::OpenDatabaseFailedPrefix => "打开数据库失败：",
            Self::ReadSyncDeviceIdFailedPrefix => "读取同步设备 ID 失败：",
            Self::WrongBackupPassword => "密码不对（或者这不是 Z-SSH / ZeeTerm 导出的备份文件）",
            Self::ChooseBackupFile => "请先选择 Z-SSH 导出的备份文件（.zsshbak）",
            Self::EnterBackupPassword => "请输入导出这份备份时设的密码",
            Self::BusyDetecting => "正在查找 Z-SSH 的数据…",
            Self::BusyReading => "正在读取…",
            Self::BusyPreviewing => "正在重新计算预览…",
            Self::BusyImporting => "正在导入…",
            Self::BackgroundTaskFailedPrefix => "启动后台任务失败：",
            Self::BackupPathPlaceholder => "Z-SSH 导出的备份文件（.zsshbak）的路径",
            Self::BackupPasswordPlaceholder => "导出这份备份时设的密码（不会保存）",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::OwnFolder => "This is ZeeTerm's own data folder, not a Z-SSH one",
            Self::NoDatabase => {
                "This folder has no Z-SSH database (expected data.db, normally under %APPDATA%\\Z-ssh)"
            }
            Self::SaveFailedPrefix => "Save failed: ",
            Self::CreateDataDirFailedPrefix => "Could not create the data directory: ",
            Self::OpenDatabaseFailedPrefix => "Could not open the database: ",
            Self::ReadSyncDeviceIdFailedPrefix => "Could not read the sync device ID: ",
            Self::WrongBackupPassword => {
                "Incorrect password (or this is not a backup exported by Z-SSH / ZeeTerm)"
            }
            Self::ChooseBackupFile => "Select the backup file exported by Z-SSH (.zsshbak) first",
            Self::EnterBackupPassword => "Enter the password used when this backup was exported",
            Self::BusyDetecting => "Looking for Z-SSH data…",
            Self::BusyReading => "Reading…",
            Self::BusyPreviewing => "Recalculating the preview…",
            Self::BusyImporting => "Importing…",
            Self::BackgroundTaskFailedPrefix => "Could not start the background task: ",
            Self::BackupPathPlaceholder => "Path to the backup file exported by Z-SSH (.zsshbak)",
            Self::BackupPasswordPlaceholder => {
                "Password used when this backup was exported (not saved)"
            }
        }
    }
}

/// 后台事件轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 调试构建里用来把"Z-SSH 的数据目录"指到临时目录里假数据的钩子（release 里不存在）。
#[cfg(debug_assertions)]
pub const SOURCE_DIR_HOOK: &str = "ZSSH_MIGRATE_SOURCE_DIR";

/// 迁移块通知主窗口的事情。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrateNotice {
    /// 导入结果里的「打开主机页」。
    OpenHosts,
    /// 导入成功了（欢迎页的提示卡不用再出现）。
    Imported,
}

pub type NoticeFn = Rc<dyn Fn(MigrateNotice, &mut Window, &mut App)>;

// ---- 探测 ----

/// 本机的 Z-SSH 数据（找到才有）。
pub fn detect_zssh(explicit: Option<PathBuf>) -> Option<ZsshSource> {
    detect(&detect_env(explicit))
}

fn detect_env(explicit: Option<PathBuf>) -> DetectEnv {
    #[cfg(debug_assertions)]
    let hook = crate::dev_hooks::var_os(SOURCE_DIR_HOOK).map(PathBuf::from);
    #[cfg(not(debug_assertions))]
    let hook = None;
    env_for(
        explicit,
        hook,
        std::env::var_os("APPDATA").map(PathBuf::from),
        crate::settings::data_dir(),
    )
}

/// 组装探测环境。调试钩子指定了假数据目录时**只**认它，不再看 `%APPDATA%`：自动化和截图不能去碰
/// （哪怕只是 stat）用户真实的 Z-SSH 目录。
pub(crate) fn env_for(
    explicit: Option<PathBuf>,
    hook: Option<PathBuf>,
    appdata: Option<PathBuf>,
    own_data_dir: PathBuf,
) -> DetectEnv {
    if let Some(dir) = hook {
        return DetectEnv {
            explicit_dir: Some(dir),
            appdata: None,
            own_data_dir: Some(own_data_dir),
        };
    }
    DetectEnv {
        explicit_dir: explicit,
        appdata: appdata.filter(|dir| !dir.as_os_str().is_empty()),
        own_data_dir: Some(own_data_dir),
    }
}

/// 用户手动指定的文件夹：不是有效的 Z-SSH 数据、或就是 ZeeTerm 自己的数据目录都不收。
pub fn pick_folder(dir: &Path) -> Result<ZsshSource, String> {
    if zssh_core::sync::zssh_import::same_dir(dir, &crate::settings::data_dir()) {
        return Err(t(MigrateMsg::OwnFolder).to_owned());
    }
    source_at(dir).ok_or_else(|| t(MigrateMsg::NoDatabase).to_owned())
}

// ---- 欢迎页提示卡 ----

/// 提示卡什么时候出现：本机有 Z-SSH 数据、ZeeTerm 的主机库还是空的、用户没点过「不再提示」。
pub fn tip_visible(found: bool, library_empty: bool, dismissed: bool) -> bool {
    found && library_empty && !dismissed
}

pub fn is_dismissed(db: &Database) -> bool {
    db.setting_get(MIGRATE_DISMISSED_KEY)
        .ok()
        .flatten()
        .is_some_and(|raw| raw.trim() == "true")
}

pub fn set_dismissed(db: &Database) -> Result<(), String> {
    db.setting_set(MIGRATE_DISMISSED_KEY, "true")
        .map_err(|err| {
            format!(
                "{}{}",
                t(MigrateMsg::SaveFailedPrefix),
                crate::error_text::error_text(&err)
            )
        })
}

// ---- 读取 / 预览 / 导入（后台线程里跑）----

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// 本机的 Z-SSH 数据库。
    Direct,
    /// Z-SSH（或 ZeeTerm）导出的备份文件。
    File,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// Z-SSH 开着，要用户先退出再重试。
    InUse,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub kind: FailureKind,
    pub message: String,
    pub origin: Origin,
}

impl Failure {
    fn other(origin: Origin, message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Other,
            message: message.into(),
            origin,
        }
    }
}

impl From<(ZsshImportError, Origin)> for Failure {
    fn from((err, origin): (ZsshImportError, Origin)) -> Self {
        let kind = if matches!(err, ZsshImportError::InUse) {
            FailureKind::InUse
        } else {
            FailureKind::Other
        };
        Self {
            kind,
            message: err.to_string(),
            origin,
        }
    }
}

/// 读出来、预览过、等用户点导入的数据。快照里有明文密码，只在内存里留到导入或取消。
pub struct Loaded {
    pub origin: Origin,
    pub snapshot: Arc<SyncSnapshot>,
    pub preview: ImportPreview,
    /// 没读全的地方（比如指纹文件读不了）。
    pub notes: Vec<String>,
}

fn open_db(data_dir: &Path) -> Result<Database, String> {
    std::fs::create_dir_all(data_dir)
        .map_err(|err| format!("{}{err}", t(MigrateMsg::CreateDataDirFailedPrefix)))?;
    Database::open(&data_dir.join("zssh.db")).map_err(|err| {
        format!(
            "{}{}",
            t(MigrateMsg::OpenDatabaseFailedPrefix),
            crate::error_text::error_text(&err)
        )
    })
}

/// 和备份页同一种引擎：同一个数据目录、同一份 known_hosts。
fn engine<'a>(
    db: &'a Database,
    sealer: &'a dyn SecretSealer,
    data_dir: &Path,
) -> Result<SyncEngine<'a>, String> {
    let device_id = db.sync_device_id().map_err(|err| {
        format!(
            "{}{}",
            t(MigrateMsg::ReadSyncDeviceIdFailedPrefix),
            crate::error_text::error_text(&err)
        )
    })?;
    Ok(SyncEngine::new(
        db,
        sealer,
        device_id,
        data_dir,
        Some(data_dir.join("known_hosts")),
        Some(env!("CARGO_PKG_VERSION").to_owned()),
    ))
}

fn preview_of(
    data_dir: &Path,
    sealer: &dyn SecretSealer,
    snapshot: &SyncSnapshot,
    mode: OfflineImportMode,
) -> Result<ImportPreview, String> {
    let db = open_db(data_dir)?;
    engine(&db, sealer, data_dir)?
        .preview_import(snapshot, mode)
        .map_err(|err| err.to_string())
}

/// 路径一：读本机的 Z-SSH 库并预览。
pub fn load_direct(
    source: &ZsshSource,
    data_dir: &Path,
    sealer: &dyn SecretSealer,
    mode: OfflineImportMode,
) -> Result<Loaded, Failure> {
    let read = read_zssh(source, sealer, data_dir).map_err(|err| (err, Origin::Direct))?;
    let preview = preview_of(data_dir, sealer, &read.snapshot, mode)
        .map_err(|message| Failure::other(Origin::Direct, message))?;
    Ok(Loaded {
        origin: Origin::Direct,
        snapshot: Arc::new(read.snapshot),
        preview,
        notes: read.notes,
    })
}

/// 路径二：解开备份文件并预览。密码错、文件坏在这里就报错，本机一个字节不动。
pub fn load_file(
    path: &Path,
    password: &str,
    data_dir: &Path,
    sealer: &dyn SecretSealer,
    mode: OfflineImportMode,
) -> Result<Loaded, Failure> {
    let fail = |message: String| Failure::other(Origin::File, message);
    let bytes = zssh_core::sync::offline::read_offline_file(path).map_err(fail)?;
    let snapshot = SyncSnapshot::open(&bytes, password).map_err(|err| {
        fail(match err {
            SyncError::WrongPassword => t(MigrateMsg::WrongBackupPassword).to_owned(),
            other => other.to_string(),
        })
    })?;
    let preview = preview_of(data_dir, sealer, &snapshot, mode).map_err(fail)?;
    Ok(Loaded {
        origin: Origin::File,
        snapshot: Arc::new(snapshot),
        preview,
        notes: Vec::new(),
    })
}

/// 导入。引擎先做本地备份再写库；成功后顺手记下"不再提示"（欢迎页的卡不用再出现）。
pub fn run_import(
    data_dir: &Path,
    sealer: &dyn SecretSealer,
    snapshot: &SyncSnapshot,
    mode: OfflineImportMode,
) -> Result<OfflineImportReport, String> {
    let db = open_db(data_dir)?;
    let report = engine(&db, sealer, data_dir)?
        .import_snapshot(snapshot.clone(), mode)
        .map_err(|err| err.to_string())?;
    // 记不上不算导入失败。
    let _ = set_dismissed(&db);
    Ok(report)
}

/// 离线备份文件要的输入检查。
pub fn file_request(path: &str, password: &str) -> Result<(PathBuf, String), String> {
    let path = path.trim();
    if path.is_empty() {
        return Err(t(MigrateMsg::ChooseBackupFile).to_owned());
    }
    if password.trim().is_empty() {
        return Err(t(MigrateMsg::EnterBackupPassword).to_owned());
    }
    Ok((PathBuf::from(path), password.to_owned()))
}

// ---- 界面状态 ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found {
    Checking,
    Yes(ZsshSource),
    No,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Busy {
    Detecting,
    Reading,
    Previewing,
    Importing,
}

impl Busy {
    pub fn label(self) -> &'static str {
        match self {
            Self::Detecting => t(MigrateMsg::BusyDetecting),
            Self::Reading => t(MigrateMsg::BusyReading),
            Self::Previewing => t(MigrateMsg::BusyPreviewing),
            Self::Importing => t(MigrateMsg::BusyImporting),
        }
    }
}

pub struct Done {
    pub origin: Origin,
    pub preview: ImportPreview,
    pub report: OfflineImportReport,
}

pub enum Stage {
    Idle,
    Preview(Box<Loaded>),
    Done(Box<Done>),
    Failed(Failure),
}

/// 迁移块的状态（不含输入框）。事件怎么改状态都在这里，方便不开窗口就测。
pub struct MigrateState {
    pub found: Found,
    pub stage: Stage,
    pub busy: Option<Busy>,
    pub mode: OfflineImportMode,
    /// 「替换」会删本机记录：第一次点导入只置为确认状态。
    pub replace_armed: bool,
    /// 手动指定文件夹时的说明（选的不是 Z-SSH 数据）。
    pub folder_error: Option<String>,
}

impl Default for MigrateState {
    fn default() -> Self {
        Self {
            found: Found::Checking,
            stage: Stage::Idle,
            busy: Some(Busy::Detecting),
            mode: OfflineImportMode::Merge,
            replace_armed: false,
            folder_error: None,
        }
    }
}

pub enum MigrateEvent {
    Detected(Option<ZsshSource>),
    Loaded(Box<Loaded>),
    LoadFailed(Failure),
    Repreviewed(Box<ImportPreview>),
    RepreviewFailed(String),
    Imported(Box<OfflineImportReport>),
    ImportFailed(String),
}

impl MigrateState {
    pub fn on_event(&mut self, event: MigrateEvent) {
        match event {
            MigrateEvent::Detected(found) => {
                self.busy = None;
                self.found = found.map_or(Found::No, Found::Yes);
            }
            MigrateEvent::Loaded(loaded) => {
                self.busy = None;
                self.replace_armed = false;
                self.stage = Stage::Preview(loaded);
            }
            MigrateEvent::LoadFailed(failure) => {
                self.busy = None;
                self.stage = Stage::Failed(failure);
            }
            MigrateEvent::Repreviewed(preview) => {
                self.busy = None;
                if let Stage::Preview(loaded) = &mut self.stage {
                    loaded.preview = *preview;
                }
            }
            MigrateEvent::RepreviewFailed(message) => {
                self.busy = None;
                let origin = self.origin().unwrap_or(Origin::Direct);
                self.stage = Stage::Failed(Failure::other(origin, message));
            }
            MigrateEvent::Imported(report) => {
                self.busy = None;
                self.replace_armed = false;
                if let Stage::Preview(loaded) = std::mem::replace(&mut self.stage, Stage::Idle) {
                    self.stage = Stage::Done(Box::new(Done {
                        origin: loaded.origin.clone(),
                        preview: loaded.preview.clone(),
                        report: *report,
                    }));
                }
            }
            MigrateEvent::ImportFailed(message) => {
                self.busy = None;
                self.replace_armed = false;
                let origin = self.origin().unwrap_or(Origin::Direct);
                self.stage = Stage::Failed(Failure::other(origin, message));
            }
        }
    }

    pub fn origin(&self) -> Option<Origin> {
        match &self.stage {
            Stage::Preview(loaded) => Some(loaded.origin.clone()),
            Stage::Done(done) => Some(done.origin.clone()),
            Stage::Failed(failure) => Some(failure.origin.clone()),
            Stage::Idle => None,
        }
    }

    /// 换导入方式：预览要按新方式重算；「替换」的确认作废。返回要不要重算。
    pub fn set_mode(&mut self, mode: OfflineImportMode) -> bool {
        if self.mode == mode {
            return false;
        }
        self.mode = mode;
        self.replace_armed = false;
        matches!(self.stage, Stage::Preview(_))
    }

    /// 点导入：「替换」第一次只置为确认状态，返回 `false`；其余直接放行。
    pub fn confirm_import(&mut self) -> bool {
        if self.mode == OfflineImportMode::Replace && !self.replace_armed {
            self.replace_armed = true;
            return false;
        }
        true
    }

    pub fn cancel(&mut self) {
        self.stage = Stage::Idle;
        self.replace_armed = false;
    }
}

/// 开始一次导入 / 恢复前抢同步权；抢不到就给用户一句话。
fn begin_exclusive() -> Result<SyncInFlight, String> {
    SyncInFlight::acquire(ExclusiveKind::Restore)
        .map_err(|holder| exclusive_busy_message(ExclusiveKind::Restore, holder).to_owned())
}

pub struct MigrateDemo {
    state: MigrateState,
    file_path: Entity<InputState>,
    file_password: Entity<InputState>,
    notice: NoticeFn,
    events: Receiver<MigrateEvent>,
    sender: Sender<MigrateEvent>,
    /// 手动指定过的文件夹（重新探测时保留）。
    manual_dir: Option<PathBuf>,
    /// 入口（提示卡、欢迎页链接）要求「找到后直接读取并预览」，但探测还没回来：回来后再开始。
    pending_direct: bool,
    _tick: Task<()>,
}

impl MigrateDemo {
    pub fn new_embedded(window: &mut Window, cx: &mut Context<Self>, notice: NoticeFn) -> Self {
        let file_path = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t(MigrateMsg::BackupPathPlaceholder))
        });
        let file_password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t(MigrateMsg::BackupPasswordPlaceholder))
        });
        let (sender, events) = std::sync::mpsc::channel();
        let mut this = Self {
            state: MigrateState::default(),
            file_path,
            file_password,
            notice,
            events,
            sender,
            manual_dir: None,
            pending_direct: false,
            _tick: Task::ready(()),
        };
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
        this.start_detect(cx);
        this
    }

    // ---- 后台任务 ----

    fn spawn(
        &mut self,
        busy: Busy,
        work: impl FnOnce(Sender<MigrateEvent>) + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let sender = self.sender.clone();
        match crate::ssh_backend::runtime() {
            Ok(runtime) => {
                self.state.busy = Some(busy);
                drop(runtime.spawn_blocking(move || work(sender)));
            }
            Err(err) => {
                self.state.stage = Stage::Failed(Failure::other(
                    Origin::Direct,
                    format!("{}{err}", t(MigrateMsg::BackgroundTaskFailedPrefix)),
                ));
            }
        }
        cx.notify();
    }

    /// 重新探测本机的 Z-SSH 数据（启动时、手动指定文件夹后、用户退出 Z-SSH 之后重试时）。
    pub fn start_detect(&mut self, cx: &mut Context<Self>) {
        if self.state.busy.is_some() && self.state.busy != Some(Busy::Detecting) {
            return;
        }
        let manual = self.manual_dir.clone();
        self.state.found = Found::Checking;
        self.spawn(
            Busy::Detecting,
            move |sender| {
                let _ = sender.send(MigrateEvent::Detected(detect_zssh(manual)));
            },
            cx,
        );
    }

    /// 路径一：读本机的 Z-SSH 并预览。
    pub fn start_direct(&mut self, cx: &mut Context<Self>) {
        if self.state.busy.is_some() {
            return;
        }
        let Found::Yes(source) = self.state.found.clone() else {
            return;
        };
        let (data_dir, mode) = (crate::settings::data_dir(), self.state.mode);
        self.state.stage = Stage::Idle;
        self.spawn(
            Busy::Reading,
            move |sender| {
                let event = match load_direct(&source, &data_dir, &DpapiSealer, mode) {
                    Ok(loaded) => MigrateEvent::Loaded(Box::new(loaded)),
                    Err(failure) => MigrateEvent::LoadFailed(failure),
                };
                let _ = sender.send(event);
            },
            cx,
        );
    }

    /// 入口要求"读本机的 Z-SSH 并预览"：已经探测到就马上开始，探测还没回来就等它回来。
    pub fn request_direct(&mut self, cx: &mut Context<Self>) {
        match self.state.found {
            Found::Yes(_) if self.state.busy.is_none() => self.start_direct(cx),
            Found::No => {}
            _ => self.pending_direct = true,
        }
    }
    /// 路径二：解开备份文件并预览。
    pub fn start_file(&mut self, cx: &mut Context<Self>) {
        if self.state.busy.is_some() {
            return;
        }
        let request = file_request(
            &self.file_path.read(cx).value(),
            &self.file_password.read(cx).value(),
        );
        let (path, password) = match request {
            Ok(request) => request,
            Err(message) => {
                self.state.stage = Stage::Failed(Failure::other(Origin::File, message));
                cx.notify();
                return;
            }
        };
        let (data_dir, mode) = (crate::settings::data_dir(), self.state.mode);
        self.state.stage = Stage::Idle;
        self.spawn(
            Busy::Reading,
            move |sender| {
                let event = match load_file(&path, &password, &data_dir, &DpapiSealer, mode) {
                    Ok(loaded) => MigrateEvent::Loaded(Box::new(loaded)),
                    Err(failure) => MigrateEvent::LoadFailed(failure),
                };
                let _ = sender.send(event);
            },
            cx,
        );
    }

    /// 选了导入方式：预览按新方式重算。
    pub fn set_mode(&mut self, mode: OfflineImportMode, cx: &mut Context<Self>) {
        if self.state.busy.is_some() {
            return;
        }
        if !self.state.set_mode(mode) {
            cx.notify();
            return;
        }
        let Stage::Preview(loaded) = &self.state.stage else {
            return;
        };
        let snapshot = loaded.snapshot.clone();
        let data_dir = crate::settings::data_dir();
        self.spawn(
            Busy::Previewing,
            move |sender| {
                let event = match preview_of(&data_dir, &DpapiSealer, &snapshot, mode) {
                    Ok(preview) => MigrateEvent::Repreviewed(Box::new(preview)),
                    Err(message) => MigrateEvent::RepreviewFailed(message),
                };
                let _ = sender.send(event);
            },
            cx,
        );
    }

    /// 点「导入」：替换要点两次；和同步互斥。
    pub fn start_import(&mut self, cx: &mut Context<Self>) {
        if self.state.busy.is_some() {
            return;
        }
        let Stage::Preview(loaded) = &self.state.stage else {
            return;
        };
        let snapshot = loaded.snapshot.clone();
        if !self.state.confirm_import() {
            cx.notify();
            return;
        }
        let guard = match begin_exclusive() {
            Ok(guard) => guard,
            Err(message) => {
                let origin = self.state.origin().unwrap_or(Origin::Direct);
                self.state.stage = Stage::Failed(Failure::other(origin, message));
                cx.notify();
                return;
            }
        };
        let (data_dir, mode) = (crate::settings::data_dir(), self.state.mode);
        self.spawn(
            Busy::Importing,
            move |sender| {
                // 守卫活到导入结束（含出错）；期间同步和别的恢复 / 导入进不来。
                let _guard = guard;
                let event = match run_import(&data_dir, &DpapiSealer, &snapshot, mode) {
                    Ok(report) => MigrateEvent::Imported(Box::new(report)),
                    Err(message) => MigrateEvent::ImportFailed(message),
                };
                let _ = sender.send(event);
            },
            cx,
        );
    }

    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.state.busy.is_some() {
            return;
        }
        self.state.cancel();
        cx.notify();
    }

    /// 「指定文件夹…」选好了一个。
    pub fn use_folder(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        match pick_folder(&dir) {
            Ok(source) => {
                self.state.folder_error = None;
                self.manual_dir = Some(dir);
                self.state.found = Found::Yes(source);
            }
            Err(message) => self.state.folder_error = Some(message),
        }
        cx.notify();
    }

    fn poll_events(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut changed = false;
        while let Ok(event) = self.events.try_recv() {
            changed = true;
            let imported = matches!(event, MigrateEvent::Imported(_));
            self.state.on_event(event);
            if imported {
                (self.notice)(MigrateNotice::Imported, window, cx);
            }
        }
        if self.pending_direct && self.state.busy.is_none() {
            self.pending_direct = false;
            if matches!(self.state.found, Found::Yes(_)) {
                self.start_direct(cx);
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
    }

    /// 画面上这一块里的文字（截图前的隐私检查用：数据文件夹、备份文件路径、结果文字）。
    #[cfg(debug_assertions)]
    pub(crate) fn privacy_texts(&self, cx: &App) -> Vec<String> {
        let state = &self.state;
        let mut texts = vec![
            view::found_text(&state.found, state.folder_error.as_deref()),
            self.file_path.read(cx).value().to_string(),
        ];
        match &state.stage {
            Stage::Failed(failure) => texts.push(failure.message.clone()),
            Stage::Preview(loaded) => {
                texts.extend(
                    view::preview_rows(&loaded.preview)
                        .into_iter()
                        .map(|(_, t)| t),
                );
                texts.extend(view::preview_warnings(&loaded.preview, &loaded.origin));
                texts.extend(loaded.notes.clone());
            }
            Stage::Done(done) => texts.extend(
                view::done_rows(&done.preview, &done.report)
                    .into_iter()
                    .map(|(_, t)| t),
            ),
            Stage::Idle => {}
        }
        texts
    }

    pub fn open_hosts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        (self.notice)(MigrateNotice::OpenHosts, window, cx);
    }
}

#[cfg(test)]
mod tests;
