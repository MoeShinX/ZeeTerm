//! 同步设置：WebDAV / S3 / GitHub Gist / OneDrive 连接信息的保存、测试和清除，
//! 「立即同步」，以及同步留下的冲突：改用另一份、恢复被删记录或者忽略。
//!
//! G11 起这套逻辑的主入口是主窗口设置页里的同步一节（`new_embedded`，画法在 `view.rs`，底部同步状态条在
//! `status.rs`）；独立的同步窗口只剩调试入口（`ZSSH_SYNC=1`，只在 debug 构建里读）。两种形态共用同一个
//! `SyncDemo` 和下面所有的保护逻辑。
//!
//! 可以选 WebDAV、S3 兼容存储、GitHub Gist 或 OneDrive。前三家的连接信息（含密码 /
//! Secret / 令牌）由 `zssh_core::sync::service` 用本机 DPAPI 加密后存进设置表；界面只
//! 拿得到 `ConfigView`，看不到已保存的密码。OneDrive 没有可填的密码：点「登录 OneDrive」
//! 走核心的浏览器授权，令牌和连接设置都由核心保存，界面只拿到账号显示名和「高级」里
//! 填的 client ID。四套表单各留一份内存草稿，切换存储只换显示的那套，其他没保存的
//! 输入不会被清掉。
//!
//! 测试连接、同步和 OneDrive 登录都要走网络，放在 [`crate::ssh_backend::runtime`] 的
//! 阻塞线程里跑，结果经 `mpsc` 回到界面，100ms 取一次，窗口不阻塞；登录等浏览器的
//! 那一步可以点「取消登录」通知核心停等。同步用的主密码和存储的连接密码是两回事：
//! 主密码只用来解开云端同步包。默认只存在内存里；打开「记住主密码」并且同步成功后，
//! 才交给核心用本机 DPAPI 加密保存，不上传、不写日志。
//! 防误删确认时密码短暂留在 [`SyncState`] 里，等用户点「允许删除并继续」或
//! 「取消」再交给核心重放。
//!
//! 自动同步：开关打开、本机保存了存储设置并且记住了主密码时，进程级调度器每 30 秒
//! 看一次，到间隔就在后台跑一次同步；失败按 1x / 2x / 4x 退避，手动同步或成功一次后
//! 清零。调度器由 [`spawn_auto_sync`] 在主窗口启动时拉起（一个进程一个，不跟窗口开关
//! 走），任务挂在 `App` 上，进程退出时一起结束；同步窗口只注册一个接收端，用来显示上次
//! 结果和暂停原因。

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::armed::ArmedConfirm;
use crate::i18n::{Msg, t};
use ::gpui::{
    App, FocusHandle, ScrollHandle, Subscription, Task, Window, WindowBounds, WindowOptions,
};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::*;
use tokio::sync::Notify;
use zssh_core::store::Database;
use zssh_core::sync::apply::{ApplyReport, ChangeCounts};
use zssh_core::sync::conflicts::{self, ConflictView};
use zssh_core::sync::engine::{SyncEngine, SyncReport, SyncStatus};
use zssh_core::sync::error::SyncError;
use zssh_core::sync::github::{self, GithubConfig};
use zssh_core::sync::guard::DeletionDecision;
use zssh_core::sync::merge::Conflict;
use zssh_core::sync::onedrive::browser::{BrowserOpener, SystemBrowser};
use zssh_core::sync::onedrive::oauth::Endpoints;
use zssh_core::sync::onedrive::tokens::{DbTokenStore, TokenStore};
use zssh_core::sync::remote::{ConcurrencyMode, ConnectionProbe};
use zssh_core::sync::s3::S3Config;
use zssh_core::sync::sealer::{DpapiSealer, SecretSealer};
use zssh_core::sync::service::{
    self, AutoSettings, ConcurrencyRecord, ConfigView, LastResult, SignedIn, StorageConfig,
    SyncContext,
};
use zssh_core::sync::webdav::WebDavConfig;

/// 同步设置窗口的编号；IME 按窗口 key 管理。
static NEXT_SYNC: AtomicU64 = AtomicU64::new(1);

/// 后台事件轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 同步设置（`sync.rs`）自己的界面文案：占位提示、状态行、确认提示、校验错误前缀。
///
/// 只放应用自己的固定文案；存储连接信息、路径、账号名、Gist ID、Gist 名称和核心库的错误原文
/// 属于远端 / 用户数据，原样拼在本地化前缀后面，不经过这里。带参数的句子保留 `format!`，
/// 这里只提供固定片段。`webdav` / `s3` / `github` / `onedrive` 这些协议键不进这张表。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SyncMsg {
    // ---- 占位提示 ----
    PasswordSaved,
    PasswordEmpty,
    S3SecretEmpty,
    GithubTokenEmpty,
    MasterPasswordPlaceholder,
    MasterPasswordConfirmPlaceholder,
    RememberSaved,
    RememberPending,
    RememberClearPending,
    RememberOff,
    MasterPasswordRemembered,
    MasterPasswordForgotten,
    WebdavUrlPlaceholder,
    SyncSubdirPlaceholder,
    UsernamePlaceholder,
    S3EndpointPlaceholder,
    S3RegionPlaceholder,
    S3BucketPlaceholder,
    S3PrefixPlaceholder,
    GithubApiPlaceholder,
    GithubGistPlaceholder,
    OnedriveClientIdPlaceholder,
    // ---- 状态行 ----
    OneDriveSavedByLogin,
    SavedSettingsPrefix,
    SavedSettingsSuffix,
    TestingConnection,
    WaitingForBrowser,
    CancellingLogin,
    SignedInOneDrive,
    SignedInAsPrefix,
    NotSignedInOneDrive,
    OneDriveNotSignedIn,
    SyncCompletedRememberFailedPrefix,
    SyncCompletedClearFailedPrefix,
    BaselineResetConfirm,
    BaselineResetDone,
    ClearConfirm,
    ClearedSettings,
    ClearedSettingsButPasswordFailedPrefix,
    LogoutConfirm,
    LoggedOutOneDrive,
    // ---- 任务启动失败（前缀，后面接错误原文） ----
    StartTestFailedPrefix,
    StartSyncFailedPrefix,
    StartLoginFailedPrefix,
    ReadDeviceIdFailedPrefix,
    SaveConcurrencyFailedPrefix,
    CreateGistFailedPrefix,
    SaveGistIdFailedPrefix,
    SaveGistIdFailedSuffix,
    OpenDatabaseFailedPrefix,
    CreateDataDirFailedPrefix,
    // ---- 互斥 / 保护状态 ----
    SyncAlreadyRunning,
    BusyRestoring,
    BusySyncing,
    BusyOtherRestore,
    PendingDeletionHint,
    RateLimitedPrefix,
    RateLimitedSuffix,
    RateLimitedTail,
    AuthRejectedPrefix,
    AuthRejectedSuffix,
    WrongMasterPassword,
    AutoSyncTimeoutPrefix,
    AutoSyncTimeoutSuffix,
    // ---- 自动同步暂停原因 ----
    AutoOff,
    AutoNoStorage,
    AutoNoPassword,
    AutoBaselineBroken,
    AutoNotReady,
    BaselineBrokenMessage,
    IntervalError,
    UnderstoodRequired,
    // ---- 表单校验 ----
    WebdavUrlRequired,
    WebdavUsernameRequired,
    WebdavInsecureHttp,
    S3EndpointRequired,
    S3BucketRequired,
    S3BucketSlash,
    S3AccessKeyRequired,
    S3RegionRequired,
    S3SecretRequired,
    S3EndpointScheme,
    S3EndpointNoHost,
    GithubTokenRequired,
    GithubApiScheme,
    GithubApiNoHost,
    GithubInsecureHttp,
    OneDriveSignInRequired,
    SaveSettingsFirstPrefix,
    SaveSettingsFirstSuffix,
    SaveCurrentSettingsFirst,
    MasterPasswordTooShortPrefix,
    MasterPasswordTooShortMiddle,
    MasterPasswordTooShortSuffix,
    MasterPasswordMismatch,
    // ---- 连接测试结果 ----
    ProbeRead,
    ProbeWrite,
    ConcurrencyPrefix,
    ConcurrencyNotePrefix,
    ConcurrencyNotChecked,
    CloudHasData,
    CloudEmpty,
    CheckOkMiddle,
    CheckFailedMiddle,
    ConcurrencyConditional,
    ConcurrencyReadBeforeWrite,
    // ---- 同步结果 ----
    StatusPrefix,
    SummaryPrefix,
    BackupPrefix,
    ConflictCountPrefix,
    ConflictCountSuffix,
    MoreConflictsPrefix,
    MoreConflictsSuffix,
    WillDelete,
    DeletionSamplePrefix,
    DeletionSampleMiddle,
    ConflictKeySeparator,
    MoreDeletionsPrefix,
    MoreDeletionsSuffix,
    StatusCompleted,
    StatusLocalChanged,
    StatusNeedsConfirmation,
    StatusCancelled,
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
    SkippedHostKeysSuffix,
    // ---- 冲突面板 ----
    KeptDeletion,
    KeptChangePrefix,
    KeptChangeMiddle,
    OtherLastContentPrefix,
    OtherLastContentMiddle,
    OtherChangePrefix,
    OtherChangeMiddle,
    EntityHost,
    EntityGroup,
    EntityIdentity,
    EntityProxy,
    EntityKnownHost,
    EntitySetting,
    ThisComputer,
    OtherComputerPrefix,
    OtherComputerSuffix,
    OtherComputer,
    UnsupportedProviderPrefix,
    UnsupportedProviderSuffix,
    NoSyncDatabase,
    // ---- 窗口标题 ----
    WindowTitle,
}

impl Msg for SyncMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::PasswordSaved => "已保存（留空保持不变）",
            Self::PasswordEmpty => "密码",
            Self::S3SecretEmpty => "Secret Key",
            Self::GithubTokenEmpty => "个人访问令牌（需要 gist 权限）",
            Self::MasterPasswordPlaceholder => "主密码（至少 8 个字符）",
            Self::MasterPasswordConfirmPlaceholder => "再输入一次主密码",
            Self::RememberSaved => "已记住（本机加密）",
            Self::RememberPending => "同步成功后记住",
            Self::RememberClearPending => "同步成功后清除",
            Self::RememberOff => "不记住",
            Self::MasterPasswordRemembered => "已记住主密码",
            Self::MasterPasswordForgotten => "已清除记住的主密码",
            Self::WebdavUrlPlaceholder => "https://dav.example.com/remote.php/dav/files/<用户>/",
            Self::SyncSubdirPlaceholder => "同步子目录（可空）",
            Self::UsernamePlaceholder => "用户名",
            Self::S3EndpointPlaceholder => {
                "https://s3.us-east-1.amazonaws.com 或 https://<账号>.r2.cloudflarestorage.com"
            }
            Self::S3RegionPlaceholder => "Region（例如 us-east-1；R2 填 auto）",
            Self::S3BucketPlaceholder => "Bucket 名称",
            Self::S3PrefixPlaceholder => "对象前缀（可空）",
            Self::GithubApiPlaceholder => "https://api.github.com（留空用默认）",
            Self::GithubGistPlaceholder => "留空 = 第一次同步时自动新建",
            Self::OnedriveClientIdPlaceholder => {
                "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx（留空用内置的）"
            }
            Self::OneDriveSavedByLogin => {
                "OneDrive 的连接由「登录 OneDrive」保存，不用再点「保存」"
            }
            Self::SavedSettingsPrefix => "已保存 ",
            Self::SavedSettingsSuffix => " 同步设置",
            Self::TestingConnection => "正在测试连接…",
            Self::WaitingForBrowser => "正在等待浏览器授权…",
            Self::CancellingLogin => "正在取消登录…",
            Self::SignedInOneDrive => "已登录 OneDrive",
            Self::SignedInAsPrefix => "已登录：",
            Self::NotSignedInOneDrive => "还没有登录 OneDrive",
            Self::OneDriveNotSignedIn => "未登录",
            Self::SyncCompletedRememberFailedPrefix => "同步完成，但记住主密码失败：",
            Self::SyncCompletedClearFailedPrefix => "同步完成，但清除记住的主密码失败：",
            Self::BaselineResetConfirm => {
                "再次点击「确认重建」：下一次同步按首次同步处理，两边的数据都会保留，\
                 同名的不会去重；重建之前在另一边删掉的记录可能会回来"
            }
            Self::BaselineResetDone => "已重建同步基线，下一次同步按首次同步处理",
            Self::ClearConfirm => "再次点击「确认清除」会删除本机保存的同步设置（云端数据不动）",
            Self::ClearedSettings => "已清除本机保存的同步设置（云端数据不动）",
            Self::ClearedSettingsButPasswordFailedPrefix => {
                "已清除本机保存的同步设置（云端数据不动），但清除记住的主密码失败："
            }
            Self::LogoutConfirm => {
                "再次点击「确认退出」会删除本机保存的 OneDrive 登录信息（云端同步包不动）"
            }
            Self::LoggedOutOneDrive => "已退出 OneDrive（云端同步包不动）",
            Self::StartTestFailedPrefix => "启动测试任务失败：",
            Self::StartSyncFailedPrefix => "启动同步任务失败：",
            Self::StartLoginFailedPrefix => "启动登录任务失败：",
            Self::ReadDeviceIdFailedPrefix => "读取同步设备 ID 失败：",
            Self::SaveConcurrencyFailedPrefix => "保存并发保护检测结果失败：",
            Self::CreateGistFailedPrefix => "新建 Gist 失败：",
            Self::SaveGistIdFailedPrefix => "保存新建的 Gist ID 失败（",
            Self::SaveGistIdFailedSuffix => "）：",
            Self::OpenDatabaseFailedPrefix => "打开数据库失败：",
            Self::CreateDataDirFailedPrefix => "创建数据目录失败：",
            Self::SyncAlreadyRunning => "已经有一个同步正在进行，请稍后再试",
            Self::BusyRestoring => "正在恢复备份或导入离线包，结束后再同步",
            Self::BusySyncing => "同步正在进行，结束后再恢复或导入",
            Self::BusyOtherRestore => "另一个恢复或导入正在进行，请稍后再试",
            Self::PendingDeletionHint => "有待确认的删除，请先在同步窗口处理",
            Self::RateLimitedPrefix => "",
            Self::RateLimitedSuffix => " 限流，",
            Self::RateLimitedTail => "后重试",
            Self::AuthRejectedPrefix => "存储拒绝了凭据（",
            Self::AuthRejectedSuffix => "），修改设置或手动同步一次后恢复",
            Self::WrongMasterPassword => "主密码不对（可能在别处改过），重新输入后手动同步一次",
            Self::AutoSyncTimeoutPrefix => "自动同步超过 ",
            Self::AutoSyncTimeoutSuffix => " 分钟没有完成，已放弃这一次",
            Self::AutoOff => "自动同步未开启",
            Self::AutoNoStorage => "还没有保存同步存储",
            Self::AutoNoPassword => "还没有记住主密码",
            Self::AutoBaselineBroken => "同步基线损坏，请先重建基线",
            Self::AutoNotReady => "还没有成功同步过，先手动同步一次",
            Self::BaselineBrokenMessage => "同步基线损坏，请先点「重建同步基线」",
            Self::IntervalError => "间隔需要 1–1440 之间的整数（分钟）",
            Self::UnderstoodRequired => "请先勾选「我知道忘记主密码就无法恢复云端数据」",
            Self::WebdavUrlRequired => "请填写 WebDAV 地址",
            Self::WebdavUsernameRequired => "请填写 WebDAV 用户名",
            Self::WebdavInsecureHttp => {
                "地址是 http://，密码会以明文发送；需要先勾选「我了解密码会以明文发送，仍然使用 http://」"
            }
            Self::S3EndpointRequired => "请填写 S3 Endpoint",
            Self::S3BucketRequired => "请填写 S3 Bucket",
            Self::S3BucketSlash => "S3 Bucket 不能包含 /",
            Self::S3AccessKeyRequired => "请填写 S3 Access Key",
            Self::S3RegionRequired => "请填写 S3 Region",
            Self::S3SecretRequired => "请填写 S3 Secret Key",
            Self::S3EndpointScheme => "S3 Endpoint 必须以 http:// 或 https:// 开头",
            Self::S3EndpointNoHost => "S3 Endpoint 缺少主机名",
            Self::GithubTokenRequired => "请填写 GitHub 令牌",
            Self::GithubApiScheme => {
                "API 地址必须以 https:// 或 http:// 开头（留空用 https://api.github.com）"
            }
            Self::GithubApiNoHost => "API 地址缺少主机名",
            Self::GithubInsecureHttp => {
                "接口地址是 http://，令牌会以明文发送；需要先勾选「我了解令牌会以明文发送，仍然使用 http://」"
            }
            Self::OneDriveSignInRequired => "请先登录 OneDrive",
            Self::SaveSettingsFirstPrefix => "请先保存 ",
            Self::SaveSettingsFirstSuffix => " 同步设置再同步",
            Self::SaveCurrentSettingsFirst => "请先保存当前存储设置",
            Self::MasterPasswordTooShortPrefix => "主密码至少需要 ",
            Self::MasterPasswordTooShortMiddle => " 个字符（现在 ",
            Self::MasterPasswordTooShortSuffix => " 个）",
            Self::MasterPasswordMismatch => "两次输入的主密码不一致",
            Self::ProbeRead => "读取",
            Self::ProbeWrite => "写入",
            Self::ConcurrencyPrefix => "并发保护：",
            Self::ConcurrencyNotePrefix => "  · ",
            Self::ConcurrencyNotChecked => "并发保护：未检测（读写未通过）",
            Self::CloudHasData => "云端：已有同步数据（第二台电脑需要输入主密码解开）",
            Self::CloudEmpty => "云端：还没有同步数据",
            Self::CheckOkMiddle => "：成功 — ",
            Self::CheckFailedMiddle => "：失败 — ",
            Self::ConcurrencyConditional => "支持条件写入（服务器保证原子）",
            Self::ConcurrencyReadBeforeWrite => "不支持条件写入（写前再读，仍可能覆盖）",
            Self::StatusPrefix => "状态：",
            Self::SummaryPrefix => "摘要：",
            Self::BackupPrefix => "备份：",
            Self::ConflictCountPrefix => "冲突 ",
            Self::ConflictCountSuffix => " 项：",
            Self::MoreConflictsPrefix => "  …还有 ",
            Self::MoreConflictsSuffix => " 条",
            Self::WillDelete => "将被删除：",
            Self::DeletionSamplePrefix => "  · ",
            Self::DeletionSampleMiddle => "：",
            Self::ConflictKeySeparator => "：",
            Self::MoreDeletionsPrefix => "  …另有 ",
            Self::MoreDeletionsSuffix => " 条",
            Self::StatusCompleted => "完成",
            Self::StatusLocalChanged => "本地有变化",
            Self::StatusNeedsConfirmation => "需要确认",
            Self::StatusCancelled => "已取消",
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
            Self::SkippedHostKeysPrefix => "；跳过 ",
            Self::SkippedHostKeysSuffix => " 条可疑主机密钥记录",
            Self::KeptDeletion => "保留：删除（这条记录在另一边已被删除）",
            Self::KeptChangePrefix => "保留：",
            Self::KeptChangeMiddle => " 的改动：",
            Self::OtherLastContentPrefix => "另一份：",
            Self::OtherLastContentMiddle => " 最后的内容：",
            Self::OtherChangePrefix => "另一份：",
            Self::OtherChangeMiddle => " 的改动：",
            Self::EntityHost => "主机",
            Self::EntityGroup => "分组",
            Self::EntityIdentity => "凭据",
            Self::EntityProxy => "代理",
            Self::EntityKnownHost => "主机指纹",
            Self::EntitySetting => "设置",
            Self::ThisComputer => "本机",
            Self::OtherComputerPrefix => "另一台电脑（",
            Self::OtherComputerSuffix => "）",
            Self::OtherComputer => "另一台电脑",
            Self::UnsupportedProviderPrefix => "本机保存的同步存储是 ",
            Self::UnsupportedProviderSuffix => "，原生窗口目前不支持编辑它；保存别的存储会替换它。",
            Self::NoSyncDatabase => "本机还没有同步数据库",
            Self::WindowTitle => "同步设置",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::PasswordSaved => "Saved (leave empty to keep it)",
            Self::PasswordEmpty => "Password",
            Self::S3SecretEmpty => "Secret Key",
            Self::GithubTokenEmpty => "Personal access token (needs the gist scope)",
            Self::MasterPasswordPlaceholder => "Master password (at least 8 characters)",
            Self::MasterPasswordConfirmPlaceholder => "Enter the master password again",
            Self::RememberSaved => "Remembered (encrypted on this computer)",
            Self::RememberPending => "Remember after a successful sync",
            Self::RememberClearPending => "Clear after a successful sync",
            Self::RememberOff => "Do not remember",
            Self::MasterPasswordRemembered => "Master password remembered",
            Self::MasterPasswordForgotten => "Remembered master password cleared",
            Self::WebdavUrlPlaceholder => "https://dav.example.com/remote.php/dav/files/<user>/",
            Self::SyncSubdirPlaceholder => "Sync subdirectory (optional)",
            Self::UsernamePlaceholder => "Username",
            Self::S3EndpointPlaceholder => {
                "https://s3.us-east-1.amazonaws.com or https://<account>.r2.cloudflarestorage.com"
            }
            Self::S3RegionPlaceholder => "Region (for example us-east-1; use auto for R2)",
            Self::S3BucketPlaceholder => "Bucket name",
            Self::S3PrefixPlaceholder => "Object prefix (optional)",
            Self::GithubApiPlaceholder => "https://api.github.com (leave empty for the default)",
            Self::GithubGistPlaceholder => "Leave empty = created automatically on the first sync",
            Self::OnedriveClientIdPlaceholder => {
                "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx (leave empty for the built-in one)"
            }
            Self::OneDriveSavedByLogin => {
                "The OneDrive connection is saved by “Sign in to OneDrive”; no need to click “Save”"
            }
            Self::SavedSettingsPrefix => "Saved ",
            Self::SavedSettingsSuffix => " sync settings",
            Self::TestingConnection => "Testing the connection…",
            Self::WaitingForBrowser => "Waiting for browser authorization…",
            Self::CancellingLogin => "Cancelling sign-in…",
            Self::SignedInOneDrive => "Signed in to OneDrive",
            Self::SignedInAsPrefix => "Signed in as ",
            Self::NotSignedInOneDrive => "Not signed in to OneDrive yet",
            Self::OneDriveNotSignedIn => "Not signed in",
            Self::SyncCompletedRememberFailedPrefix => {
                "Sync completed, but remembering the master password failed: "
            }
            Self::SyncCompletedClearFailedPrefix => {
                "Sync completed, but clearing the remembered master password failed: "
            }
            Self::BaselineResetConfirm => {
                "Click “Confirm Reset” again: the next sync is treated as the first one, both sides keep \
                 their data and same-named records are not deduplicated; records deleted on the other side \
                 before the reset may come back"
            }
            Self::BaselineResetDone => {
                "Sync baseline rebuilt; the next sync is treated as the first one"
            }
            Self::ClearConfirm => {
                "Click “Confirm Clear” again to delete the sync settings saved on this computer (cloud data is untouched)"
            }
            Self::ClearedSettings => {
                "Deleted the sync settings saved on this computer (cloud data is untouched)"
            }
            Self::ClearedSettingsButPasswordFailedPrefix => {
                "Deleted the sync settings saved on this computer (cloud data is untouched), but clearing the remembered master password failed: "
            }
            Self::LogoutConfirm => {
                "Click “Confirm Sign Out” again to delete the OneDrive sign-in info saved on this computer (the cloud sync package is untouched)"
            }
            Self::LoggedOutOneDrive => {
                "Signed out of OneDrive (the cloud sync package is untouched)"
            }
            Self::StartTestFailedPrefix => "Could not start the connection test: ",
            Self::StartSyncFailedPrefix => "Could not start the sync task: ",
            Self::StartLoginFailedPrefix => "Could not start the sign-in task: ",
            Self::ReadDeviceIdFailedPrefix => "Could not read the sync device ID: ",
            Self::SaveConcurrencyFailedPrefix => "Could not save the concurrency probe result: ",
            Self::CreateGistFailedPrefix => "Could not create the Gist: ",
            Self::SaveGistIdFailedPrefix => "Could not save the new Gist ID (",
            Self::SaveGistIdFailedSuffix => "): ",
            Self::OpenDatabaseFailedPrefix => "Could not open the database: ",
            Self::CreateDataDirFailedPrefix => "Could not create the data directory: ",
            Self::SyncAlreadyRunning => "A sync is already running, please try again later",
            Self::BusyRestoring => "A restore or offline import is running; sync when it finishes",
            Self::BusySyncing => "A sync is running; restore or import when it finishes",
            Self::BusyOtherRestore => {
                "Another restore or import is running, please try again later"
            }
            Self::PendingDeletionHint => {
                "A deletion is waiting for confirmation; handle it in the sync window first"
            }
            Self::RateLimitedPrefix => "",
            Self::RateLimitedSuffix => " rate limited, retry after ",
            Self::RateLimitedTail => "",
            Self::AuthRejectedPrefix => "The storage rejected the credentials (",
            Self::AuthRejectedSuffix => "); fix the settings or sync manually to recover",
            Self::WrongMasterPassword => {
                "The master password is wrong (it may have been changed elsewhere); enter it again and sync manually"
            }
            Self::AutoSyncTimeoutPrefix => "Auto sync did not finish within ",
            Self::AutoSyncTimeoutSuffix => " minutes and was abandoned",
            Self::AutoOff => "Auto sync is off",
            Self::AutoNoStorage => "No sync storage saved yet",
            Self::AutoNoPassword => "Master password not remembered yet",
            Self::AutoBaselineBroken => "The sync baseline is broken; rebuild it first",
            Self::AutoNotReady => "Never synced successfully; sync manually once first",
            Self::BaselineBrokenMessage => {
                "The sync baseline is broken; click “Rebuild Sync Baseline” first"
            }
            Self::IntervalError => "The interval must be a whole number from 1–1440 (minutes)",
            Self::UnderstoodRequired => {
                "Please tick “I understand that forgetting the master password means the cloud data cannot be recovered” first"
            }
            Self::WebdavUrlRequired => "Please enter the WebDAV URL",
            Self::WebdavUsernameRequired => "Please enter the WebDAV username",
            Self::WebdavInsecureHttp => {
                "The URL is http://, so the password is sent in clear text; tick “I understand the password is sent in clear text and still want to use http://” first"
            }
            Self::S3EndpointRequired => "Please enter the S3 Endpoint",
            Self::S3BucketRequired => "Please enter the S3 Bucket",
            Self::S3BucketSlash => "The S3 Bucket cannot contain /",
            Self::S3AccessKeyRequired => "Please enter the S3 Access Key",
            Self::S3RegionRequired => "Please enter the S3 Region",
            Self::S3SecretRequired => "Please enter the S3 Secret Key",
            Self::S3EndpointScheme => "The S3 Endpoint must start with http:// or https://",
            Self::S3EndpointNoHost => "The S3 Endpoint is missing a host name",
            Self::GithubTokenRequired => "Please enter a GitHub token",
            Self::GithubApiScheme => {
                "The API URL must start with https:// or http:// (leave empty to use https://api.github.com)"
            }
            Self::GithubApiNoHost => "The API URL is missing a host name",
            Self::GithubInsecureHttp => {
                "The API URL is http://, so the token is sent in clear text; tick “I understand the token is sent in clear text and still want to use http://” first"
            }
            Self::OneDriveSignInRequired => "Please sign in to OneDrive first",
            Self::SaveSettingsFirstPrefix => "Please save the ",
            Self::SaveSettingsFirstSuffix => " sync settings before syncing",
            Self::SaveCurrentSettingsFirst => "Please save the current storage settings first",
            Self::MasterPasswordTooShortPrefix => "The master password must be at least ",
            Self::MasterPasswordTooShortMiddle => " characters (currently ",
            Self::MasterPasswordTooShortSuffix => ")",
            Self::MasterPasswordMismatch => "The two master passwords do not match",
            Self::ProbeRead => "Read",
            Self::ProbeWrite => "Write",
            Self::ConcurrencyPrefix => "Concurrency protection: ",
            Self::ConcurrencyNotePrefix => "  · ",
            Self::ConcurrencyNotChecked => {
                "Concurrency protection: not checked (read/write did not pass)"
            }
            Self::CloudHasData => {
                "Cloud: sync data already exists (a second computer must enter the master password to unlock it)"
            }
            Self::CloudEmpty => "Cloud: no sync data yet",
            Self::CheckOkMiddle => ": succeeded — ",
            Self::CheckFailedMiddle => ": failed — ",
            Self::ConcurrencyConditional => {
                "Conditional writes are supported (the server guarantees atomicity)"
            }
            Self::ConcurrencyReadBeforeWrite => {
                "Conditional writes are not supported (read before write; may still overwrite)"
            }
            Self::StatusPrefix => "Status: ",
            Self::SummaryPrefix => "Summary: ",
            Self::BackupPrefix => "Backup: ",
            Self::ConflictCountPrefix => "Conflicts: ",
            Self::ConflictCountSuffix => " items:",
            Self::MoreConflictsPrefix => "  …and ",
            Self::MoreConflictsSuffix => " more",
            Self::WillDelete => "Will be deleted:",
            Self::DeletionSamplePrefix => "  · ",
            Self::DeletionSampleMiddle => ": ",
            Self::ConflictKeySeparator => ": ",
            Self::MoreDeletionsPrefix => "  …and ",
            Self::MoreDeletionsSuffix => " more",
            Self::StatusCompleted => "Completed",
            Self::StatusLocalChanged => "Local changes",
            Self::StatusNeedsConfirmation => "Needs confirmation",
            Self::StatusCancelled => "Cancelled",
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
            Self::SkippedHostKeysPrefix => "; skipped ",
            Self::SkippedHostKeysSuffix => " suspicious host key records",
            Self::KeptDeletion => "Kept: the deletion (this record was deleted on the other side)",
            Self::KeptChangePrefix => "Kept: ",
            Self::KeptChangeMiddle => " changes: ",
            Self::OtherLastContentPrefix => "Other copy: the last content on ",
            Self::OtherLastContentMiddle => ": ",
            Self::OtherChangePrefix => "Other copy: ",
            Self::OtherChangeMiddle => " changes: ",
            Self::EntityHost => "Host",
            Self::EntityGroup => "Group",
            Self::EntityIdentity => "Credential",
            Self::EntityProxy => "Proxy",
            Self::EntityKnownHost => "Host key",
            Self::EntitySetting => "Setting",
            Self::ThisComputer => "this computer",
            Self::OtherComputerPrefix => "the other computer (",
            Self::OtherComputerSuffix => ")",
            Self::OtherComputer => "the other computer",
            Self::UnsupportedProviderPrefix => "The sync storage saved on this computer is ",
            Self::UnsupportedProviderSuffix => {
                ", which the native window cannot edit yet; saving another storage replaces it."
            }
            Self::NoSyncDatabase => "There is no sync database on this computer yet",
            Self::WindowTitle => "Sync Settings",
        }
    }
}

/// 同步结果里最多列几条冲突摘要。
const MAX_CONFLICT_LINES: usize = 5;

/// OneDrive 登录等浏览器授权的最长时间：用 core 的值（0.2.14 同一个，5 分钟）。
const ONEDRIVE_LOGIN_TIMEOUT: Duration = zssh_core::sync::onedrive::login::LOGIN_TIMEOUT;

/// 自动同步的间隔范围（分钟）；和核心 `load_auto` / `save_auto` 的夹取规则一致。
const MIN_AUTO_INTERVAL_MINUTES: u32 = 1;
const MAX_AUTO_INTERVAL_MINUTES: u32 = 24 * 60;

/// 自动同步调度器多久看一次。
const AUTO_SYNC_TICK: Duration = Duration::from_secs(30);

/// 一次自动同步最多跑多久；网络卡住时别把这一拍一直占着。
const AUTO_SYNC_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// 防误删确认最多列几个名字；和 0.2.14（`DeletionPlan::samples`）同一个上限。
const MAX_DELETION_NAMES: usize = zssh_core::sync::guard::MAX_SAMPLES;

/// 上次结果是同步失败时的状态键。
const ERROR_STATUS: &str = "error";

mod actions;
mod cloud_backup;
#[cfg(debug_assertions)]
mod dev;
mod status;
mod strength;
// 依赖 debug-only 的 sync_stub（见 main.rs），release 下没有它，测试模块也不能编译。
#[cfg(all(test, debug_assertions))]
mod stub_tests;
// 同上：用 debug-only 的桩跑云端备份。
#[cfg(all(test, debug_assertions))]
mod cloud_backup_tests;
mod view;

pub(crate) use status::{
    SyncBarModel, Tone as SyncTone, bar_model, format_stamp, spawn_status_refresher,
};

/// 同步窗口目前接的四家存储。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ProviderChoice {
    #[default]
    WebDav,
    S3,
    Github,
    OneDrive,
}

impl ProviderChoice {
    /// 和核心 `StorageConfig::provider()` 返回的键一致。
    fn key(self) -> &'static str {
        match self {
            Self::WebDav => "webdav",
            Self::S3 => "s3",
            Self::Github => "github",
            Self::OneDrive => "onedrive",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::WebDav => "WebDAV",
            Self::S3 => "S3",
            Self::Github => "GitHub Gist",
            Self::OneDrive => "OneDrive",
        }
    }

    fn from_provider(provider: &str) -> Option<Self> {
        match provider {
            "webdav" => Some(Self::WebDav),
            "s3" => Some(Self::S3),
            "github" => Some(Self::Github),
            "onedrive" => Some(Self::OneDrive),
            _ => None,
        }
    }
}

pub struct SyncDemo {
    url: Entity<InputState>,
    directory: Entity<InputState>,
    username: Entity<InputState>,
    password: Entity<InputState>,
    s3_endpoint: Entity<InputState>,
    s3_region: Entity<InputState>,
    s3_bucket: Entity<InputState>,
    s3_access_key: Entity<InputState>,
    s3_secret: Entity<InputState>,
    s3_prefix: Entity<InputState>,
    github_api_base: Entity<InputState>,
    github_token: Entity<InputState>,
    github_gist_id: Entity<InputState>,
    /// OneDrive「高级」里的 client ID；空 = 用编进程序里的那个。
    onedrive_client_id: Entity<InputState>,
    /// 同步包的主密码；默认只存在内存里，打开「记住」并在同步成功后由核心加密保存。
    master_password: Entity<InputState>,
    /// 第一次设置主密码时的"再输一遍"；已经和云端对上过的电脑不显示。
    master_password_confirm: Entity<InputState>,
    /// 这台电脑已经用主密码和云端对上过（核心的 `sync_ready`）。
    ready: bool,
    /// 同步基线读不出来：同步和自动同步都停，等用户重建。
    baseline_broken: bool,
    /// 「重建基线」的二次确认。
    baseline_reset_armed: bool,
    /// 「记住主密码」的开关和本机是否已经保存；默认关闭。
    remember: RememberPassword,
    allow_insecure_http: bool,
    github_allow_insecure_http: bool,
    /// 四套表单的草稿和当前选中的存储；切换只改选中项，其他原样留着。
    drafts: ProviderDrafts,
    /// 本机已保存的 WebDAV / S3 / GitHub / OneDrive 配置；密码和令牌只在内存里，界面不显示。
    saved: Option<StorageConfig>,
    webdav_has_secret: bool,
    s3_has_secret: bool,
    github_has_secret: bool,
    status: String,
    probe: Option<ConnectionProbe>,
    testing: bool,
    /// 后台同步任务是否在跑；同一时间只允许一个。
    syncing: bool,
    /// 正在等浏览器完成 OneDrive 授权。
    signing_in: bool,
    /// 这次登录的取消开关；点「取消登录」时通知核心停等。
    onedrive_cancel: Option<Arc<Notify>>,
    sync_state: SyncState,
    /// 云端没被采用的那一份留在这里，用户可以改用、恢复或忽略。
    conflicts: ConflictPanel,
    clear_confirm: ClearConfirm,
    logout_confirm: LogoutConfirm,
    /// 「自动同步」一块的显示数据：设置、上次结果和读写错误。
    auto: AutoPanel,
    /// 「间隔（分钟）」输入框；回车或失焦时保存。
    auto_interval: Entity<InputState>,
    events: Receiver<SyncEvent>,
    sender: Sender<SyncEvent>,
    /// 嵌在设置页里：只画卡片，不画窗口标题和滚动容器，也不接输入法的英文区
    /// （主窗口自己管输入法）。
    embedded: bool,
    /// 第一次设置主密码时的「我知道忘记就无法恢复云端数据」勾选。
    understood: bool,
    /// 「修改主密码」表单。
    change_pw: actions::ChangePw,
    /// 「云端备份」一节：备份目标列表、添加 / 编辑表单、进行中的任务（U33）。
    cloud: cloud_backup::CloudBackup,
    /// 上次看到的「同步账号设置被改过」计数（从备份恢复 / 导入会改本机的同步设置）。
    accounts_epoch: u64,
    /// OneDrive「高级」折叠区展开了没有。
    onedrive_advanced: bool,
    /// 本机保存的并发保护检测结果（测试连接时写入）。
    concurrency: Option<ConcurrencyRecord>,
    scroll: ScrollHandle,
    focus: FocusHandle,
    _ime: Vec<Subscription>,
    _auto_interval: Subscription,
    _tick: Task<()>,
}

impl SyncDemo {
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

        let (saved, saved_view, mut status) = load_saved();
        // 主密码是另一条设置：库里没有就当作没记住，不为了读它新建库。
        let remembered_password = match load_master_password_at(&crate::settings::data_dir()) {
            Ok(password) => password,
            Err(message) => {
                if !status.is_empty() {
                    status.push('\n');
                }
                status.push_str(&message);
                None
            }
        };
        let remember = RememberPassword::from_saved(remembered_password.as_deref());
        let provider = saved_view
            .as_ref()
            .and_then(|view| ProviderChoice::from_provider(&view.provider))
            .unwrap_or_default();
        // 只回填本机保存的那一家；其他几家保持草稿默认值。
        let webdav_form = saved_view.as_ref().map(form_from_view).unwrap_or_default();
        let s3_form = saved_view
            .as_ref()
            .map(s3_form_from_view)
            .unwrap_or_default();
        let github_form = saved_view
            .as_ref()
            .map(github_form_from_view)
            .unwrap_or_default();
        let onedrive_form = saved_view
            .as_ref()
            .map(onedrive_form_from_view)
            .unwrap_or_default();
        let webdav_has_secret = saved_view
            .as_ref()
            .is_some_and(|view| view.provider == ProviderChoice::WebDav.key() && view.has_secret);
        let s3_has_secret = saved_view
            .as_ref()
            .is_some_and(|view| view.provider == ProviderChoice::S3.key() && view.has_secret);
        let github_has_secret = saved_view
            .as_ref()
            .is_some_and(|view| view.provider == ProviderChoice::Github.key() && view.has_secret);

        let url_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(webdav_form.url.clone())
                .placeholder(t(SyncMsg::WebdavUrlPlaceholder))
        });
        let directory_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(webdav_form.directory.clone())
                .placeholder(t(SyncMsg::SyncSubdirPlaceholder))
        });
        let username_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(webdav_form.username.clone())
                .placeholder(t(SyncMsg::UsernamePlaceholder))
        });
        let password_input = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(if webdav_has_secret {
                    t(SyncMsg::PasswordSaved)
                } else {
                    t(SyncMsg::PasswordEmpty)
                })
        });
        let s3_endpoint_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(s3_form.endpoint.clone())
                .placeholder(t(SyncMsg::S3EndpointPlaceholder))
        });
        let s3_region_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(s3_form.region.clone())
                .placeholder(t(SyncMsg::S3RegionPlaceholder))
        });
        let s3_bucket_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(s3_form.bucket.clone())
                .placeholder(t(SyncMsg::S3BucketPlaceholder))
        });
        let s3_access_key_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(s3_form.access_key.clone())
                .placeholder("Access Key")
        });
        let s3_secret_input = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(if s3_has_secret {
                    t(SyncMsg::PasswordSaved)
                } else {
                    t(SyncMsg::S3SecretEmpty)
                })
        });
        let s3_prefix_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(s3_form.prefix.clone())
                .placeholder(t(SyncMsg::S3PrefixPlaceholder))
        });
        let github_api_base_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(github_form.api_base.clone())
                .placeholder(t(SyncMsg::GithubApiPlaceholder))
        });
        let github_token_input = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(if github_has_secret {
                    t(SyncMsg::PasswordSaved)
                } else {
                    t(SyncMsg::GithubTokenEmpty)
                })
        });
        let github_gist_id_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(github_form.gist_id.clone())
                .placeholder(t(SyncMsg::GithubGistPlaceholder))
        });
        let onedrive_client_id_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(onedrive_form.client_id.clone())
                .placeholder(t(SyncMsg::OnedriveClientIdPlaceholder))
        });
        let master_password_input = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .default_value(remembered_password.clone().unwrap_or_default())
                .placeholder(t(SyncMsg::MasterPasswordPlaceholder))
        });
        let master_password_confirm_input = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t(SyncMsg::MasterPasswordConfirmPlaceholder))
        });
        let ready = load_ready_at(&crate::settings::data_dir());
        let baseline_broken = baseline_broken_at(&crate::settings::data_dir());
        // 自动同步的设置和上次结果；读不出来时只显示错误，不影响上面的同步设置。
        let auto = load_auto_panel(&crate::settings::data_dir());
        let auto_interval_input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(auto.settings.interval_minutes.to_string())
                .placeholder("1 – 1440")
        });
        // 间隔输入框只在回车 / 失焦时保存，边打字边写库没意义。
        let auto_interval_sub = cx.subscribe_in(
            &auto_interval_input,
            window,
            |this, _input, event, window, cx| this.on_auto_interval_event(event, window, cx),
        );

        let change_pw = actions::ChangePw::new(window, cx);
        let concurrency = load_concurrency_at(&crate::settings::data_dir());
        let (sender, events) = std::sync::mpsc::channel();
        // 打开窗口就读一次冲突；读不出来只显示错误，不影响上面的同步设置。
        let conflicts = match load_conflicts_at(&crate::settings::data_dir()) {
            Ok(items) => ConflictPanel::new(items, None),
            Err(message) => ConflictPanel::new(Vec::new(), Some(message)),
        };
        let mut this = Self {
            url: url_input,
            directory: directory_input,
            username: username_input,
            password: password_input,
            s3_endpoint: s3_endpoint_input,
            s3_region: s3_region_input,
            s3_bucket: s3_bucket_input,
            s3_access_key: s3_access_key_input,
            s3_secret: s3_secret_input,
            s3_prefix: s3_prefix_input,
            github_api_base: github_api_base_input,
            github_token: github_token_input,
            github_gist_id: github_gist_id_input,
            onedrive_client_id: onedrive_client_id_input,
            master_password: master_password_input,
            master_password_confirm: master_password_confirm_input,
            ready,
            baseline_broken,
            baseline_reset_armed: false,
            remember,
            allow_insecure_http: webdav_form.allow_insecure_http,
            github_allow_insecure_http: github_form.allow_insecure_http,
            drafts: ProviderDrafts {
                provider,
                webdav: webdav_form,
                s3: s3_form,
                github: github_form,
                onedrive: onedrive_form,
            },
            saved,
            webdav_has_secret,
            s3_has_secret,
            github_has_secret,
            status,
            probe: None,
            testing: false,
            syncing: false,
            signing_in: false,
            onedrive_cancel: None,
            sync_state: SyncState::default(),
            conflicts,
            clear_confirm: ClearConfirm::default(),
            logout_confirm: LogoutConfirm::default(),
            auto,
            auto_interval: auto_interval_input,
            events,
            sender,
            embedded,
            understood: false,
            change_pw,
            cloud: cloud_backup::CloudBackup::load(&crate::settings::data_dir()),
            accounts_epoch: ACCOUNTS_EPOCH.load(Ordering::SeqCst),
            onedrive_advanced: false,
            concurrency,
            scroll: ScrollHandle::new(),
            focus,
            _ime: ime,
            _auto_interval: auto_interval_sub,
            _tick: Task::ready(()),
        };
        // 自动同步的结果由进程级调度器推过来；窗口关掉后发送失败，调度器会清掉接收端。
        listen_auto(this.sender.clone());
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
        this
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// 读取 WebDAV 表单当前值。密码只在这里短暂停留，不写日志。
    fn webdav_form(&self, cx: &App) -> WebDavForm {
        WebDavForm {
            url: self.url.read(cx).value().to_string(),
            directory: self.directory.read(cx).value().to_string(),
            username: self.username.read(cx).value().to_string(),
            password: self.password.read(cx).value().to_string(),
            allow_insecure_http: self.allow_insecure_http,
        }
    }

    /// 读取 S3 表单当前值。Secret 只在这里短暂停留，不写日志。
    fn s3_form(&self, cx: &App) -> S3Form {
        S3Form {
            endpoint: self.s3_endpoint.read(cx).value().to_string(),
            region: self.s3_region.read(cx).value().to_string(),
            bucket: self.s3_bucket.read(cx).value().to_string(),
            access_key: self.s3_access_key.read(cx).value().to_string(),
            secret_key: self.s3_secret.read(cx).value().to_string(),
            prefix: self.s3_prefix.read(cx).value().to_string(),
            path_style: self.drafts.s3.path_style,
        }
    }

    /// 读取 GitHub 表单当前值。令牌只在这里短暂停留，不写日志。
    fn github_form(&self, cx: &App) -> GithubForm {
        GithubForm {
            api_base: self.github_api_base.read(cx).value().to_string(),
            token: self.github_token.read(cx).value().to_string(),
            gist_id: self.github_gist_id.read(cx).value().to_string(),
            allow_insecure_http: self.github_allow_insecure_http,
        }
    }

    /// 读取 OneDrive 表单当前值；只有「高级」里的 client ID。
    fn onedrive_form(&self, cx: &App) -> OneDriveForm {
        OneDriveForm {
            client_id: self.onedrive_client_id.read(cx).value().to_string(),
        }
    }

    /// 把四套输入框的当前值收进草稿；保存、测试和切换存储前都先调一次。
    fn capture(&mut self, cx: &App) {
        self.drafts.webdav = self.webdav_form(cx);
        self.drafts.s3 = self.s3_form(cx);
        self.drafts.github = self.github_form(cx);
        self.drafts.onedrive = self.onedrive_form(cx);
    }

    /// 切换「记住主密码」：只改开关，真正写盘要等一次成功的同步。
    fn toggle_remember(&mut self, cx: &mut Context<Self>) {
        self.remember.toggle();
        cx.notify();
    }

    /// 同步成功后按开关记住或忘掉主密码；取消、失败和需要确认都不动本机的保存。
    /// 密码只进核心接口，错误和 footer 都不带密码原文。
    fn apply_remember_after_sync(&mut self, status: SyncStatus, password: &str) {
        let action = remember_action(self.remember.toggled, Some(status));
        let data_dir = crate::settings::data_dir();
        match action {
            RememberAction::Save => match save_master_password_at(&data_dir, password) {
                Ok(()) => {
                    self.remember.saved = true;
                    if let Some(text) = remember_footer(action) {
                        self.status = text.to_owned();
                    }
                }
                Err(message) => {
                    self.status =
                        format!("{}{message}", t(SyncMsg::SyncCompletedRememberFailedPrefix));
                }
            },
            RememberAction::Clear => match clear_master_password_at(&data_dir) {
                Ok(()) => {
                    self.remember.saved = false;
                    if let Some(text) = remember_footer(action) {
                        self.status = text.to_owned();
                    }
                }
                Err(message) => {
                    self.status =
                        format!("{}{message}", t(SyncMsg::SyncCompletedClearFailedPrefix));
                }
            },
            // 取消、失败和需要确认不改变本机是否保存；footer 也保留原来的内容。
            RememberAction::Keep => {}
        }
    }

    /// 切换「自动同步」开关；顺手把输入框里的间隔一起保存下来。
    fn toggle_auto(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.auto_interval.read(cx).value().to_string();
        let minutes = parse_interval(&text).unwrap_or(self.auto.settings.interval_minutes);
        let settings = AutoSettings {
            enabled: !self.auto.settings.enabled,
            interval_minutes: minutes,
        };
        self.store_auto(settings, window, cx);
    }

    /// 间隔输入框：回车或失焦时才保存，其他输入事件不动本机设置。
    fn on_auto_interval_event(
        &mut self,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
            self.commit_interval(window, cx);
        }
    }

    /// 保存间隔：1–1440 之外的输入不存，恢复成上次保存的值并提示原因。
    fn commit_interval(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.auto_interval.read(cx).value().to_string();
        let Some(minutes) = parse_interval(&text) else {
            self.auto.error = Some(t(SyncMsg::IntervalError).to_owned());
            let saved = self.auto.settings.interval_minutes.to_string();
            self.auto_interval
                .update(cx, |state, cx| state.set_value(saved.as_str(), window, cx));
            cx.notify();
            return;
        };
        let settings = AutoSettings {
            enabled: self.auto.settings.enabled,
            interval_minutes: minutes,
        };
        self.store_auto(settings, window, cx);
    }

    /// 写本机设置；成功后把输入框对齐到实际保存的值（和核心的夹取规则一致）。
    fn store_auto(&mut self, settings: AutoSettings, window: &mut Window, cx: &mut Context<Self>) {
        match save_auto_at(&crate::settings::data_dir(), &settings) {
            Ok(()) => {
                let text = settings.interval_minutes.to_string();
                self.auto.settings = settings;
                self.auto.error = None;
                self.auto_interval
                    .update(cx, |state, cx| state.set_value(text.as_str(), window, cx));
            }
            Err(message) => self.auto.error = Some(message),
        }
        cx.notify();
    }

    /// 手动同步结束：写「上次结果」，并让自动同步的退避清零、间隔从这一刻重新算。
    fn save_manual_result(&mut self, result: LastResult) {
        let data_dir = crate::settings::data_dir();
        if let Err(message) = save_last_result_at(&data_dir, &result) {
            // 写不进本机设置不影响这次同步本身；下次打开窗口只是看不到新结果。
            tracing::warn!(%message, "保存同步结果失败");
        }
        with_auto_clock(|clock| clock.manual(result.at));
        self.auto.last = Some(result);
    }

    /// 切换显示哪一家；四套草稿都留着，没保存的输入不会被清掉。
    fn switch_provider(&mut self, provider: ProviderChoice, cx: &mut Context<Self>) {
        if self.drafts.provider == provider {
            return;
        }
        self.capture(cx);
        self.drafts.select(provider);
        self.clear_confirm.disarm();
        self.logout_confirm.disarm();
        // 上一个存储的连接测试结果不适用于新选中的存储。
        self.probe = None;
        cx.notify();
    }

    /// 保存：密码 / Secret 留空时沿用已保存的那份，不把旧密码擦掉。
    /// OneDrive 没有可填的连接信息（令牌和设置由登录保存），这里不写任何东西。
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.testing || self.syncing || self.signing_in {
            return;
        }
        self.clear_confirm.disarm();
        self.logout_confirm.disarm();
        if self.drafts.provider == ProviderChoice::OneDrive {
            self.status = t(SyncMsg::OneDriveSavedByLogin).to_owned();
            cx.notify();
            return;
        }
        self.capture(cx);
        let provider = self.drafts.provider;
        let config = match self.drafts.selected_config(self.saved.as_ref()) {
            Ok(config) => config,
            Err(message) => {
                self.status = message;
                cx.notify();
                return;
            }
        };
        match save_config_at(&crate::settings::data_dir(), &config) {
            Ok(()) => {
                // 换了存储设置：之前的限流等待和"凭据被拒"的暂停不再适用。
                clear_sync_guard();
                self.saved = Some(config);
                self.refresh_secret_ui(window, cx);
                // 保存成功后清掉刚提交的密码，别让它留在输入框和草稿里。
                match provider {
                    ProviderChoice::WebDav => {
                        self.drafts.webdav.password.clear();
                        self.password
                            .update(cx, |state, cx| state.set_value("", window, cx));
                    }
                    ProviderChoice::S3 => {
                        self.drafts.s3.secret_key.clear();
                        self.s3_secret
                            .update(cx, |state, cx| state.set_value("", window, cx));
                    }
                    ProviderChoice::Github => {
                        self.drafts.github.token.clear();
                        self.github_token
                            .update(cx, |state, cx| state.set_value("", window, cx));
                    }
                    // 上面已经提前返回；这里只是让 match 保持穷尽。
                    ProviderChoice::OneDrive => {}
                }
                self.probe = None;
                self.status = format!(
                    "{}{}{}",
                    t(SyncMsg::SavedSettingsPrefix),
                    provider.label(),
                    t(SyncMsg::SavedSettingsSuffix)
                );
            }
            Err(message) => self.status = message,
        }
        cx.notify();
    }

    /// 测试连接：配置构造和保存用的是同一套校验；网络请求在后台跑。
    /// OneDrive 的配置和令牌由登录保存，测试时直接用本机保存的那份。
    fn start_test(&mut self, cx: &mut Context<Self>) {
        if self.testing || self.syncing || self.signing_in {
            return;
        }
        self.clear_confirm.disarm();
        self.logout_confirm.disarm();
        self.capture(cx);
        let config = match self.drafts.selected_config(self.saved.as_ref()) {
            Ok(config) => config,
            Err(message) => {
                self.status = message;
                cx.notify();
                return;
            }
        };
        let data_dir = crate::settings::data_dir();
        let sender = self.sender.clone();
        match crate::ssh_backend::runtime() {
            Ok(runtime) => {
                self.testing = true;
                self.probe = None;
                self.status = t(SyncMsg::TestingConnection).to_owned();
                let handle = runtime.handle().clone();
                drop(runtime.spawn_blocking(move || {
                    let event = match probe_connection_at(&handle, &data_dir, &config) {
                        Ok(probe) => SyncEvent::Tested(probe),
                        Err(message) => SyncEvent::TestFailed(message),
                    };
                    let _ = sender.send(event);
                }));
            }
            Err(err) => self.status = format!("{}{err}", t(SyncMsg::StartTestFailedPrefix)),
        }
        cx.notify();
    }

    /// 点「立即同步」：用本机已保存的存储设置跑一次真正的同步。
    /// 表单里还没保存的改动不参与同步；选中的存储和保存的不一致时先让用户保存，
    /// 避免「看到的是 A、同步的是 B」。
    fn start_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.testing || self.syncing || self.signing_in {
            return;
        }
        // 以本机保存的配置为准（可能有另一个窗口刚改过），不用界面里可能过期的副本。
        let saved = match load_saved_config(&crate::settings::data_dir()) {
            Ok(saved) => saved,
            Err(message) => {
                self.sync_state.fail(message);
                cx.notify();
                return;
            }
        };
        self.saved = saved;
        self.refresh_secret_ui(window, cx);
        if let Err(message) = sync_guard(self.drafts.provider, self.saved.as_ref()) {
            self.sync_state.fail(message);
            cx.notify();
            return;
        }
        if self.baseline_broken {
            self.sync_state
                .fail(t(SyncMsg::BaselineBrokenMessage).to_owned());
            cx.notify();
            return;
        }
        let password = self.master_password.read(cx).value().to_string();
        // 第一次设置主密码要输两遍：云端是空的时候，输错的主密码会把整份数据加密成
        // "没人知道密码"的包，换电脑解不开。
        let confirm = self
            .needs_password_confirm()
            .then(|| self.master_password_confirm.read(cx).value().to_string());
        if let Err(message) = check_master_password_entry(&password, confirm.as_deref()) {
            self.sync_state.fail(message);
            cx.notify();
            return;
        }
        // 第一次设置主密码还要勾「我知道忘记主密码就无法恢复云端数据」（0.2.14 没勾按钮就点不了）。
        if confirm.is_some() && !self.understood {
            self.sync_state
                .fail(t(SyncMsg::UnderstoodRequired).to_owned());
            cx.notify();
            return;
        }
        self.clear_confirm.disarm();
        self.logout_confirm.disarm();
        self.baseline_reset_armed = false;
        self.spawn_sync(password, None, cx);
    }

    /// 这台电脑还没和云端对上过、也没有记住主密码、云端又不是已经有数据（测试连接
    /// 确认过）时，是"第一次设置主密码"，要输两遍。没测试过连接时不知道云端有没有数据，
    /// 按第一次处理——多输一遍不吃亏，输错一遍才是大事。
    fn needs_password_confirm(&self) -> bool {
        !self.ready
            && !self.remember.saved
            && !self
                .probe
                .as_ref()
                .is_some_and(|probe| probe.has_existing_data)
    }

    /// 点「重建同步基线」：第一次只进入确认状态，第二次才删基线文件。
    fn reset_baseline(&mut self, cx: &mut Context<Self>) {
        if self.testing || self.syncing || self.signing_in {
            return;
        }
        if !self.baseline_reset_armed {
            self.baseline_reset_armed = true;
            self.status = t(SyncMsg::BaselineResetConfirm).to_owned();
            cx.notify();
            return;
        }
        self.baseline_reset_armed = false;
        match reset_baseline_at(&crate::settings::data_dir()) {
            Ok(()) => {
                self.baseline_broken = baseline_broken_at(&crate::settings::data_dir());
                self.status = t(SyncMsg::BaselineResetDone).to_owned();
            }
            Err(message) => self.status = message,
        }
        cx.notify();
    }

    /// 点「允许删除并继续」：拿确认时留下的密码重放一次，带 `Continue`。
    fn confirm_deletion(&mut self, cx: &mut Context<Self>) {
        if self.testing || self.syncing {
            return;
        }
        let Some(password) = self.sync_state.pending_password().map(str::to_owned) else {
            return;
        };
        self.spawn_sync(password, Some(DeletionDecision::Continue), cx);
    }

    /// 点「取消」：走核心的取消路径，让引擎自己把这次同步标记为已取消。
    fn cancel_deletion(&mut self, cx: &mut Context<Self>) {
        if self.testing || self.syncing {
            return;
        }
        let Some(password) = self.sync_state.pending_password().map(str::to_owned) else {
            return;
        };
        self.spawn_sync(password, Some(DeletionDecision::Cancel), cx);
    }

    /// 在阻塞线程池里跑同步；密码只进闭包，不写日志。
    /// 和自动同步共用一个进程级的同步开关：同库不并发跑两次。
    fn spawn_sync(
        &mut self,
        password: String,
        decision: Option<DeletionDecision>,
        cx: &mut Context<Self>,
    ) {
        // 还在限流等待期内：不去碰服务（再试一次只会让限流更久），告诉用户还要等多久。
        // 没限流就是用户亲自点的：清掉"凭据被拒"的暂停再试一次，还是被拒会重新暂停。
        let limited = with_sync_guard(|guard| {
            let limited = guard.rate_limit_remaining(Instant::now());
            if limited.is_none() {
                guard.manual_retry();
            }
            limited
        });
        if let Some((service, retry_after_secs)) = limited {
            let err = SyncError::RateLimited {
                service,
                retry_after_secs,
            };
            self.sync_state.fail(crate::error_text::error_text(&err));
            cx.notify();
            return;
        }
        let guard = match SyncInFlight::acquire(ExclusiveKind::Sync) {
            Ok(guard) => guard,
            Err(holder) => {
                self.sync_state
                    .fail(exclusive_busy_message(ExclusiveKind::Sync, holder).to_owned());
                cx.notify();
                return;
            }
        };
        let data_dir = crate::settings::data_dir();
        let sender = self.sender.clone();
        match crate::ssh_backend::runtime() {
            Ok(runtime) => {
                self.syncing = true;
                self.sync_state.begin();
                let handle = runtime.handle().clone();
                drop(runtime.spawn_blocking(move || {
                    // 守卫活到这次同步结束（含出错）；期间自动同步的这一拍会跳过。
                    let _guard = guard;
                    let outcome = run_sync_at(&handle, &data_dir, &password, decision);
                    // 主同步成功才顺带备份；备份在同步结果送回界面之后再做，哪个目标慢都不会拖住这次同步的结果。
                    let backup_password = matches!(
                        &outcome,
                        Ok(report) if report.status == SyncStatus::Completed
                    )
                    .then(|| password.clone());
                    let event = match outcome {
                        Ok(report) => {
                            with_sync_guard(|guard| {
                                guard.note(&SyncOutcome::Report(report.status), Instant::now());
                            });
                            SyncEvent::Synced {
                                report: Box::new(report),
                                password,
                            }
                        }
                        Err(failure) => {
                            with_sync_guard(|guard| {
                                guard.note(
                                    &SyncOutcome::Failed(failure.kind.clone()),
                                    Instant::now(),
                                );
                            });
                            SyncEvent::SyncFailed(failure)
                        }
                    };
                    let _ = sender.send(event);
                    if let Some(password) = backup_password {
                        let db = match open_db_at(&data_dir) {
                            Ok(db) => db,
                            Err(err) => {
                                let _ = sender.send(SyncEvent::PostSyncBackupError(err));
                                return;
                            }
                        };
                        let sealer = DpapiSealer::new(db.vault_account().to_owned());
                        cloud_backup::backup_after_sync(
                            &handle,
                            &data_dir,
                            &sealer,
                            &password,
                        );
                        let _ = sender.send(SyncEvent::PostSyncBackupsDone);
                    }
                }));
            }
            Err(err) => self
                .sync_state
                .fail(format!("{}{err}", t(SyncMsg::StartSyncFailedPrefix))),
        }
        cx.notify();
    }

    /// 清除：第一次点击只进入确认状态，第二次才真的删本机设置。
    /// 清的是 OneDrive 时连本机令牌一起删（和「退出 OneDrive」同一个核心接口）。
    fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.testing || self.syncing || self.signing_in {
            return;
        }
        self.logout_confirm.disarm();
        if self.clear_confirm.take().is_none() {
            self.clear_confirm.arm(());
            self.status = t(SyncMsg::ClearConfirm).to_owned();
            cx.notify();
            return;
        }
        let data_dir = crate::settings::data_dir();
        let cleared = if matches!(self.saved, Some(StorageConfig::Onedrive(_))) {
            disconnect_onedrive_at(&data_dir)
        } else {
            clear_config_at(&data_dir)
        };
        match cleared {
            Ok(()) => {
                clear_sync_guard();
                self.saved = None;
                self.webdav_has_secret = false;
                self.s3_has_secret = false;
                self.github_has_secret = false;
                self.allow_insecure_http = false;
                self.github_allow_insecure_http = false;
                self.probe = None;
                self.reset_inputs(window, cx);
                // 存储设置没了，记住的主密码也没有意义；本机密文和输入框一起清掉。
                self.remember = RememberPassword::default();
                self.status = match clear_master_password_at(&data_dir) {
                    Ok(()) => t(SyncMsg::ClearedSettings).to_owned(),
                    Err(message) => format!(
                        "{}{message}",
                        t(SyncMsg::ClearedSettingsButPasswordFailedPrefix)
                    ),
                };
            }
            Err(message) => self.status = message,
        }
        cx.notify();
    }

    /// 登录 OneDrive：浏览器授权交给核心，等结果的那几分钟在阻塞线程里跑，界面不卡。
    /// 登录成功时核心自己保存连接设置和令牌，这里只负责刷新显示。
    fn start_onedrive_login(&mut self, cx: &mut Context<Self>) {
        if self.testing || self.syncing || self.signing_in {
            return;
        }
        self.clear_confirm.disarm();
        self.logout_confirm.disarm();
        self.capture(cx);
        // 空 = 用编进程序里的 client ID，由核心判断有没有。
        let client_id = self.drafts.onedrive.client_id().to_owned();
        let data_dir = crate::settings::data_dir();
        let sender = self.sender.clone();
        match crate::ssh_backend::runtime() {
            Ok(runtime) => {
                let cancel = Arc::new(Notify::new());
                self.onedrive_cancel = Some(cancel.clone());
                self.signing_in = true;
                self.probe = None;
                self.status = t(SyncMsg::WaitingForBrowser).to_owned();
                let handle = runtime.handle().clone();
                drop(runtime.spawn_blocking(move || {
                    let event = match sign_in_onedrive_at(&handle, &data_dir, &client_id, &cancel) {
                        Ok(signed) => SyncEvent::SignedIn(Box::new(signed)),
                        Err(message) => SyncEvent::SignInFailed(message),
                    };
                    let _ = sender.send(event);
                }));
            }
            Err(err) => self.status = format!("{}{err}", t(SyncMsg::StartLoginFailedPrefix)),
        }
        cx.notify();
    }

    /// 取消登录：通知核心停等浏览器回调，结果（「已取消登录」）还是走事件回到界面。
    fn cancel_onedrive_login(&mut self, cx: &mut Context<Self>) {
        let Some(cancel) = self.onedrive_cancel.as_ref() else {
            return;
        };
        cancel.notify_one();
        self.status = t(SyncMsg::CancellingLogin).to_owned();
        cx.notify();
    }

    /// 退出 OneDrive：第一次点击只进入确认状态，第二次才删本机登录信息和令牌。
    fn onedrive_logout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.testing || self.syncing || self.signing_in {
            return;
        }
        self.clear_confirm.disarm();
        if !matches!(self.saved, Some(StorageConfig::Onedrive(_))) {
            self.logout_confirm.disarm();
            self.status = t(SyncMsg::NotSignedInOneDrive).to_owned();
            cx.notify();
            return;
        }
        if self.logout_confirm.take().is_none() {
            self.logout_confirm.arm(());
            self.status = t(SyncMsg::LogoutConfirm).to_owned();
            cx.notify();
            return;
        }
        match disconnect_onedrive_at(&crate::settings::data_dir()) {
            Ok(()) => {
                clear_sync_guard();
                self.saved = None;
                self.refresh_secret_ui(window, cx);
                self.reset_onedrive_inputs(window, cx);
                self.probe = None;
                self.status = t(SyncMsg::LoggedOutOneDrive).to_owned();
            }
            Err(message) => self.status = message,
        }
        cx.notify();
    }

    /// OneDrive 表单清空：client ID 输入框和对应的草稿。
    fn reset_onedrive_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.onedrive_client_id
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.drafts.onedrive = OneDriveForm::default();
    }

    /// 已保存配置变化后，刷新各家密码框的占位提示（内容不回填）。
    fn refresh_secret_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.webdav_has_secret = matches!(
            &self.saved,
            Some(StorageConfig::Webdav(config)) if !config.password.is_empty()
        );
        self.s3_has_secret = matches!(
            &self.saved,
            Some(StorageConfig::S3(config)) if !config.secret_key.is_empty()
        );
        self.github_has_secret = matches!(
            &self.saved,
            Some(StorageConfig::Github(config)) if !config.token.trim().is_empty()
        );
        let webdav_placeholder = if self.webdav_has_secret {
            t(SyncMsg::PasswordSaved)
        } else {
            t(SyncMsg::PasswordEmpty)
        };
        let s3_placeholder = if self.s3_has_secret {
            t(SyncMsg::PasswordSaved)
        } else {
            t(SyncMsg::S3SecretEmpty)
        };
        let github_placeholder = if self.github_has_secret {
            t(SyncMsg::PasswordSaved)
        } else {
            t(SyncMsg::GithubTokenEmpty)
        };
        self.password.update(cx, |state, cx| {
            state.set_placeholder(webdav_placeholder, window, cx);
        });
        self.s3_secret.update(cx, |state, cx| {
            state.set_placeholder(s3_placeholder, window, cx);
        });
        self.github_token.update(cx, |state, cx| {
            state.set_placeholder(github_placeholder, window, cx);
        });
    }

    fn reset_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.url
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.directory
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.username
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.password.update(cx, |state, cx| {
            state.set_placeholder(t(SyncMsg::PasswordEmpty), window, cx);
            state.set_value("", window, cx);
        });
        self.s3_endpoint
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.s3_region
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.s3_bucket
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.s3_access_key
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.s3_secret.update(cx, |state, cx| {
            state.set_placeholder(t(SyncMsg::S3SecretEmpty), window, cx);
            state.set_value("", window, cx);
        });
        self.s3_prefix
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.github_api_base
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.github_token.update(cx, |state, cx| {
            state.set_placeholder(t(SyncMsg::GithubTokenEmpty), window, cx);
            state.set_value("", window, cx);
        });
        self.github_gist_id
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.master_password
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.drafts.webdav = WebDavForm::default();
        self.drafts.s3 = S3Form::default();
        self.drafts.github = GithubForm::default();
        self.reset_onedrive_inputs(window, cx);
    }

    /// 取走后台结果；同一时间只允许一个测试、同步或登录任务。
    fn poll_events(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut changed = false;
        while let Ok(event) = self.events.try_recv() {
            changed = true;
            match event {
                SyncEvent::Tested(probe) => {
                    self.testing = false;
                    self.probe = Some(probe);
                    self.status.clear();
                    self.concurrency = load_concurrency_at(&crate::settings::data_dir());
                }
                SyncEvent::TestFailed(message) => {
                    self.testing = false;
                    self.status = message;
                }
                SyncEvent::Synced { report, password } => {
                    self.syncing = false;
                    let status = report.status;
                    if status == SyncStatus::Completed {
                        // 成功读到了基线、也和云端对上了。
                        self.ready = true;
                        self.baseline_broken = false;
                        self.master_password_confirm
                            .update(cx, |state, cx| state.set_value("", window, cx));
                    }
                    // `finish` 会拿走报告；先留下状态给「记住主密码」判断。
                    let result = last_result_from_report(&report, false, now_secs());
                    self.sync_state.finish(*report, password.clone());
                    self.apply_remember_after_sync(status, &password);
                    self.save_manual_result(result);
                    self.reload_saved_gist(window, cx);
                    // 同步可能新留下冲突，结果出来后立刻重读。
                    self.refresh_conflicts(cx);
                }
                SyncEvent::SyncFailed(failure) => {
                    self.syncing = false;
                    self.baseline_broken = failure.baseline_broken;
                    let message = failure.message;
                    let result = last_result_error(&message, false, now_secs());
                    self.sync_state.fail(message);
                    self.save_manual_result(result);
                    self.reload_saved_gist(window, cx);
                    self.refresh_conflicts(cx);
                }
                SyncEvent::AutoSynced(result) => {
                    // 调度器已经把结果写进本机设置；这里只更新显示。
                    self.auto.last = Some(result);
                    self.auto.error = None;
                    // 自动同步能跑起来就说明本机确实记住了主密码（可能是另一个窗口刚保存的）。
                    self.remember.saved = true;
                    // 自动同步成功之后会顺带备份到各个云端备份目标，结果在目标自己的状态里。
                    self.reload_targets_now();
                    // 自动同步也可能新建 Gist、留下冲突。
                    self.ready = load_ready_at(&crate::settings::data_dir());
                    self.baseline_broken = baseline_broken_at(&crate::settings::data_dir());
                    self.reload_saved_gist(window, cx);
                    self.refresh_conflicts(cx);
                }
                SyncEvent::SignedIn(signed) => {
                    self.signing_in = false;
                    self.onedrive_cancel = None;
                    self.on_onedrive_signed_in(*signed, window, cx);
                }
                SyncEvent::SignInFailed(message) => {
                    self.signing_in = false;
                    self.onedrive_cancel = None;
                    // 核心没写任何设置；把 `LoginError` 的原文显示出来就行。
                    self.status = message;
                }
                SyncEvent::PasswordChanged { new } => {
                    self.finish_change_password(&new, window, cx);
                }
                SyncEvent::PasswordChangeFailed(message) => {
                    self.change_pw.busy = false;
                    self.change_pw.error = Some(message);
                }
                SyncEvent::TargetBackedUp { id, result } => {
                    self.on_target_backed_up(&id, result, cx);
                }
                SyncEvent::TargetRestored { id, result } => {
                    self.on_target_restored(&id, result, cx);
                }
                SyncEvent::TargetSignedIn(result) => self.on_target_signed_in(result, cx),
                SyncEvent::PostSyncBackupsDone => self.reload_targets_now(),
            }
        }
        // 备份页恢复 / 导入时写回了同步账号设置：把这一页的显示也换成新的。
        let epoch = ACCOUNTS_EPOCH.load(Ordering::SeqCst);
        if epoch != self.accounts_epoch {
            self.accounts_epoch = epoch;
            self.reload_after_accounts_change(window, cx);
            changed = true;
        }
        if changed {
            // 同步 / 登录 / 改密码刚结束：不等定时器，马上让底部状态条重读本机设置。
            status::request_refresh();
            cx.notify();
        }
    }

    /// 登录成功：核心已经存好令牌和连接设置，这里重新读一遍本机设置，把「高级」里的
    /// client ID 换成核心实际保存的（裁剪过），再显示账号。
    fn on_onedrive_signed_in(
        &mut self,
        signed: SignedIn,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let account = signed.config.account.trim().to_owned();
        let client_id = signed.config.client_id.trim().to_owned();
        // 重新登录是 OneDrive "凭据被拒"的出路。
        clear_sync_guard();
        self.reload_saved(window, cx);
        self.onedrive_client_id.update(cx, |state, cx| {
            state.set_value(client_id.as_str(), window, cx);
        });
        self.drafts.onedrive = OneDriveForm {
            client_id: client_id.clone(),
        };
        self.probe = None;
        // 核心通常能读到账号显示名；读不到时只说「已登录」，不编一个名字。
        self.status = if account.is_empty() {
            t(SyncMsg::SignedInOneDrive).to_owned()
        } else {
            format!("{}{account}", t(SyncMsg::SignedInAsPrefix))
        };
    }

    /// 重新读一遍本机保存的连接设置；读不出来时保留界面里原来的副本（错误由调用方显示）。
    fn reload_saved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(saved) = load_saved_config(&crate::settings::data_dir()) else {
            return;
        };
        self.saved = saved;
        self.refresh_secret_ui(window, cx);
    }

    /// 同步可能在后台新建了 Gist（Gist ID 留空的第一次同步）：重新读本机设置，把新 ID
    /// 回填进表单，免得再点一次「保存」把设置清空、下次同步又建一个。
    fn reload_saved_gist(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reload_saved(window, cx);
        let Some(StorageConfig::Github(config)) = &self.saved else {
            return;
        };
        if self.drafts.provider != ProviderChoice::Github
            || !self.github_gist_id.read(cx).value().is_empty()
            || config.gist_id.is_empty()
        {
            return;
        }
        let gist_id = config.gist_id.clone();
        self.github_gist_id.update(cx, |state, cx| {
            state.set_value(gist_id.as_str(), window, cx)
        });
        self.drafts.github.gist_id = gist_id;
    }

    /// 重新读一遍冲突列表；失败时保留旧列表，只把错误显示出来。
    fn refresh_conflicts(&mut self, cx: &mut Context<Self>) {
        self.conflicts.disarm();
        match load_conflicts_at(&crate::settings::data_dir()) {
            Ok(items) => self.conflicts.set_loaded(items),
            Err(message) => self.conflicts.set_error(message),
        }
        cx.notify();
    }

    /// 「改用另一份」：第一次点击只进入确认状态，第二次才真的写回本地。
    /// 写失败时这条冲突留在列表里，错误原文显示。
    fn click_use_other(&mut self, id: i64, cx: &mut Context<Self>) {
        if self.testing || self.syncing {
            return;
        }
        if !self.conflicts.confirm(id) {
            cx.notify();
            return;
        }
        match use_other_at(&crate::settings::data_dir(), id) {
            Ok(()) => self.refresh_conflicts(cx),
            Err(message) => {
                self.conflicts.set_error(message);
                cx.notify();
            }
        }
    }

    /// 取消确认：下一次点「改用另一份」又要重新确认。
    fn cancel_use_other(&mut self, cx: &mut Context<Self>) {
        self.conflicts.disarm();
        cx.notify();
    }

    /// 「忽略」：直接清掉一条冲突；失败时这条冲突留在列表里。
    fn dismiss_conflict(&mut self, id: i64, cx: &mut Context<Self>) {
        if self.testing || self.syncing {
            return;
        }
        match dismiss_at(&crate::settings::data_dir(), id) {
            Ok(()) => self.refresh_conflicts(cx),
            Err(message) => {
                self.conflicts.set_error(message);
                cx.notify();
            }
        }
    }
}

impl Render for SyncDemo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_root(window, cx)
    }
}

/// 打开同步设置窗口；同一个进程可以开多个。
pub fn open_sync_window(cx: &mut App) {
    let n = NEXT_SYNC.fetch_add(1, Ordering::Relaxed);
    let ime_key = format!("sync-window-{n}");
    let close_key = ime_key.clone();
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(
            size(::gpui::px(820.), ::gpui::px(620.)),
            cx,
        )),
        focus: true,
        ..Default::default()
    };
    let _ = cx.open_window(options, move |window, cx| {
        let view = cx.new(|cx| SyncDemo::new(window, cx, ime_key));
        window.set_window_title(t(SyncMsg::WindowTitle));
        window.on_window_should_close(cx, move |_window, _cx| {
            crate::ime::window_destroyed(&close_key);
            true
        });
        window.focus(&view.read(cx).focus_handle(), cx);
        view
    });
}

/// 后台任务 → 界面的消息（100ms tick 取走）。
enum SyncEvent {
    Tested(ConnectionProbe),
    TestFailed(String),
    /// 同步结果和这次用的主密码一起带回：需要确认时要用同一个密码重放。
    /// 密码只在这里、[`SyncState`] 和「记住主密码」的保存接口里短暂停留，不写日志。
    Synced {
        report: Box<SyncReport>,
        password: String,
    },
    SyncFailed(SyncFailure),
    /// 进程级调度器跑完一次自动同步；结果已经写进本机设置，这里只用来刷新界面。
    AutoSynced(LastResult),
    /// OneDrive 浏览器授权成功；核心已经保存好令牌和连接设置。
    SignedIn(Box<SignedIn>),
    /// 登录失败，内容是 `LoginError` 的原文。
    SignInFailed(String),
    /// 修改主密码成功；带上新密码，窗口里的输入框和记住的那份要换成它。
    PasswordChanged {
        new: String,
    },
    PasswordChangeFailed(String),
    /// 云端备份：某个目标的「立即备份」结束。
    TargetBackedUp {
        id: String,
        result: Result<zssh_core::sync::backup_targets::OutcomeStatus, String>,
    },
    /// 云端备份：「从这里恢复」结束。
    TargetRestored {
        id: String,
        result: Result<Box<zssh_core::sync::offline::OfflineImportReport>, String>,
    },
    /// 云端备份表单里的 OneDrive 登录结束。
    TargetSignedIn(Result<zssh_core::sync::onedrive::config::OneDriveConfig, String>),
    /// 手动同步成功之后顺带做的云端备份跑完了（每个目标的结果已经写进它自己的状态）。
    PostSyncBackupsDone,
    /// 手动同步成功之后顺带做的云端备份失败。
    PostSyncBackupError(String),
}

// ---------------------------------------------------------------------------
// 自动同步的进程级调度器
//
// 一个进程只有一个调度任务：主窗口启动时 [`spawn_auto_sync`] 拉起，和同步设置窗口
// 的开关无关。任务挂在 `App` 上，进程退出时一起结束；每一拍只计时，判断和同步都在
// 阻塞线程池里做，界面线程不碰数据库和网络。

/// 同步和"改本地数据的大动作"（恢复备份、导入离线包）互斥：0 = 空闲，其余是持有者。
/// 手动和自动同步共用，同库并发同步会互相覆盖；恢复中同步、同步中恢复也会互相踩。
static SYNC_IN_FLIGHT: AtomicU8 = AtomicU8::new(0);

/// 调度器已经拉起过（一个进程一个，不是每个窗口一个）。
static AUTO_SCHEDULER_STARTED: AtomicBool = AtomicBool::new(false);

/// 「同步账号设置被改过」的计数：备份页从备份 / 离线文件写回了同步设置，或者用户确认用备份里的替换本机的。
/// 同步页每 100ms 看一眼，变了就重读显示。
static ACCOUNTS_EPOCH: AtomicU64 = AtomicU64::new(0);

/// 本机的同步账号设置被备份页改过：让同步页重读，并清掉之前“凭据被拒 / 限流”的运行时暂停（设置换了就不再适用）。
pub(crate) fn accounts_changed() {
    clear_sync_guard();
    ACCOUNTS_EPOCH.fetch_add(1, Ordering::SeqCst);
}

/// 调度器自己的计时和退避。只在内存里，重启后从「上次结果」的时间重新开始算。
static AUTO_CLOCK: OnceLock<Mutex<AutoClock>> = OnceLock::new();

/// 打开的同步窗口；调度器跑完自动同步后把结果推给它们。
static AUTO_LISTENERS: OnceLock<Mutex<Vec<Sender<SyncEvent>>>> = OnceLock::new();

/// 谁占着同步权。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExclusiveKind {
    /// 同步（手动或自动）。
    Sync = 1,
    /// 恢复备份 / 导入离线包。
    Restore = 2,
}

impl ExclusiveKind {
    fn from_raw(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(Self::Sync),
            2 => Some(Self::Restore),
            _ => None,
        }
    }

    fn work_kind(self) -> crate::install_state::WorkKind {
        match self {
            Self::Sync => crate::install_state::WorkKind::Sync,
            Self::Restore => crate::install_state::WorkKind::Restore,
        }
    }
}

/// 测试专用：同步权、保护状态和自动计时都是进程级的，碰它们的测试（含备份窗口的
/// 恢复互斥测试）拿这把锁串行。
#[cfg(test)]
pub(crate) fn exclusive_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 同步 / 恢复期间持有；`Drop` 时放掉开关，出错和提前返回都不会把同步权锁死。
/// 同时在安装前检查里登记一条：只要有同步或恢复在跑，更新器就不会启动安装器。
#[derive(Debug)]
pub(crate) struct SyncInFlight {
    _install: crate::install_state::Registration,
}

impl SyncInFlight {
    /// 抢到独占权返回守卫；已经有别的在跑时返回占着的是谁。
    pub(crate) fn acquire(kind: ExclusiveKind) -> Result<Self, ExclusiveKind> {
        match SYNC_IN_FLIGHT.compare_exchange(0, kind as u8, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => Ok(Self {
                _install: crate::install_state::Registration::started(kind.work_kind()),
            }),
            // 极端竞态下读到 0（刚好被放掉）：按同步在跑处理，让用户稍后再点。
            Err(raw) => Err(ExclusiveKind::from_raw(raw).unwrap_or(ExclusiveKind::Sync)),
        }
    }
}

impl Drop for SyncInFlight {
    fn drop(&mut self) {
        SYNC_IN_FLIGHT.store(0, Ordering::SeqCst);
    }
}

/// 抢不到独占权时给用户看的话：区分"想做什么"和"被谁占着"。
pub(crate) fn exclusive_busy_message(wanted: ExclusiveKind, holder: ExclusiveKind) -> &'static str {
    match (wanted, holder) {
        (ExclusiveKind::Sync, ExclusiveKind::Sync) => t(SyncMsg::SyncAlreadyRunning),
        (ExclusiveKind::Sync, ExclusiveKind::Restore) => t(SyncMsg::BusyRestoring),
        (ExclusiveKind::Restore, ExclusiveKind::Sync) => t(SyncMsg::BusySyncing),
        (ExclusiveKind::Restore, ExclusiveKind::Restore) => t(SyncMsg::BusyOtherRestore),
    }
}

// ---------------------------------------------------------------------------
// 同步的运行时保护（对标 0.2.14 的 `SyncController`）
//
// 限流、凭据被拒、主密码不对、待确认的删除这几类状态是进程级的：手动同步和自动
// 同步共用，窗口开不开都有效。失败时必须停下来，不能每个间隔再打一次存储服务。

/// 同步失败的种类；只区分"调度器要不要停"。
#[derive(Clone, Debug, PartialEq, Eq)]
enum FailureKind {
    /// 存储服务限流：到点之前谁都不发请求。
    RateLimited {
        service: String,
        retry_after_secs: u64,
    },
    /// 凭据被存储服务拒绝：换了配置才可能好。
    AuthFailed(String),
    /// 主密码不对（别处改过或输错）。
    WrongPassword,
    /// 同步基线损坏：要用户点「重建基线」。
    BaselineBroken,
    Other,
}

/// 一次失败：原文给界面，种类给保护逻辑，`baseline_broken` 是失败后重新检查基线的结果。
#[derive(Clone, Debug)]
struct SyncFailure {
    message: String,
    kind: FailureKind,
    baseline_broken: bool,
}

impl From<String> for SyncFailure {
    fn from(message: String) -> Self {
        Self {
            message,
            kind: FailureKind::Other,
            baseline_broken: false,
        }
    }
}

impl SyncFailure {
    fn from_sync_error(err: &SyncError) -> Self {
        Self {
            message: crate::error_text::error_text(err),
            kind: classify_failure(err),
            baseline_broken: false,
        }
    }
}

/// 核心错误 → 失败种类。基线损坏核心没有单独的错误类型，用 0.2.14 同一个判断：
/// 格式错误且文字里有"基线"。
fn classify_failure(err: &SyncError) -> FailureKind {
    match err {
        SyncError::RateLimited {
            service,
            retry_after_secs,
        } => FailureKind::RateLimited {
            service: service.clone(),
            retry_after_secs: *retry_after_secs,
        },
        SyncError::AuthFailed(reason) => FailureKind::AuthFailed(reason.zh().to_owned()),
        SyncError::WrongPassword => FailureKind::WrongPassword,
        SyncError::Format(reason) if reason.contains("基线") => FailureKind::BaselineBroken, // i18n-allow: 匹配核心库的中文错误原文（界面语言无关）
        _ => FailureKind::Other,
    }
}

/// 一次同步的结果，给保护逻辑记账用。
#[derive(Clone, Debug, PartialEq, Eq)]
enum SyncOutcome {
    Report(SyncStatus),
    Failed(FailureKind),
}

/// 自动同步被暂停的原因（凭据 / 主密码），等用户处理。
#[derive(Clone, Debug, PartialEq, Eq)]
enum AutoPause {
    Auth(String),
    WrongPassword,
}

/// 进程级的同步保护状态。`now` 由调用方传入，测试不用等真实时间。
#[derive(Debug, Default)]
struct SyncGuardState {
    /// 限流：到这个时刻之前不再发请求，和服务名。
    rate_limit: Option<(Instant, String)>,
    pause: Option<AutoPause>,
    /// 防误删停在确认上：不替用户选，自动同步一律不再跑，直到用户处理。
    pending_confirmation: bool,
}

impl SyncGuardState {
    /// 还在限流等待期内：（服务名，还要等多少秒）。过期的顺手清掉。
    fn rate_limit_remaining(&mut self, now: Instant) -> Option<(String, u64)> {
        let (until, service) = self.rate_limit.as_ref()?;
        if *until > now {
            let secs = (*until - now).as_secs().max(1);
            return Some((service.clone(), secs));
        }
        self.rate_limit = None;
        None
    }

    /// 记一次同步结果。
    fn note(&mut self, outcome: &SyncOutcome, now: Instant) {
        match outcome {
            SyncOutcome::Report(SyncStatus::Completed) => {
                self.rate_limit = None;
                self.pause = None;
                self.pending_confirmation = false;
            }
            SyncOutcome::Report(SyncStatus::NeedsConfirmation) => {
                self.pending_confirmation = true;
            }
            SyncOutcome::Report(SyncStatus::Cancelled) => {
                self.pending_confirmation = false;
            }
            // 本地有变化：云端已更新、本地没动，下一个间隔自然会再来一轮。
            SyncOutcome::Report(SyncStatus::LocalChanged) => {}
            SyncOutcome::Failed(FailureKind::RateLimited {
                service,
                retry_after_secs,
            }) => {
                self.rate_limit = Some((
                    now + Duration::from_secs(*retry_after_secs),
                    service.clone(),
                ));
            }
            SyncOutcome::Failed(FailureKind::AuthFailed(message)) => {
                self.pause = Some(AutoPause::Auth(message.clone()));
            }
            SyncOutcome::Failed(FailureKind::WrongPassword) => {
                self.pause = Some(AutoPause::WrongPassword);
            }
            // 基线损坏由基线检查单独拦；其他失败走退避，不在这里暂停。
            SyncOutcome::Failed(FailureKind::BaselineBroken | FailureKind::Other) => {}
        }
    }

    /// 用户亲自点了「立即同步」：再试一次；还是被拒会重新暂停。
    fn manual_retry(&mut self) {
        self.pause = None;
    }

    /// 换了存储设置 / 登录：之前的限流和暂停都不再适用。
    fn config_changed(&mut self) {
        self.rate_limit = None;
        self.pause = None;
    }

    /// 自动同步这一拍该不该停，以及停的原因（给界面显示）。
    fn block_reason(&mut self, now: Instant) -> Option<String> {
        if self.pending_confirmation {
            return Some(t(SyncMsg::PendingDeletionHint).to_owned());
        }
        if let Some((service, secs)) = self.rate_limit_remaining(now) {
            return Some(format!(
                "{}{service}{}{}{}",
                t(SyncMsg::RateLimitedPrefix),
                t(SyncMsg::RateLimitedSuffix),
                zssh_core::sync::storage::wait_text(secs),
                t(SyncMsg::RateLimitedTail)
            ));
        }
        match &self.pause {
            Some(AutoPause::Auth(message)) => Some(format!(
                "{}{message}{}",
                t(SyncMsg::AuthRejectedPrefix),
                t(SyncMsg::AuthRejectedSuffix)
            )),
            Some(AutoPause::WrongPassword) => Some(t(SyncMsg::WrongMasterPassword).to_owned()),
            None => None,
        }
    }
}

static SYNC_GUARD: OnceLock<Mutex<SyncGuardState>> = OnceLock::new();

fn with_sync_guard<R>(f: impl FnOnce(&mut SyncGuardState) -> R) -> R {
    let guard = SYNC_GUARD.get_or_init(|| Mutex::new(SyncGuardState::default()));
    let mut guard = guard
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

/// 换了存储设置 / 登录 / 退出：清掉限流和暂停。
fn clear_sync_guard() {
    with_sync_guard(SyncGuardState::config_changed);
}

/// 同步基线现在读不读得了。坏了（解不开、JSON 损坏、版本太新）要提供「重建基线」，
/// 同步和自动同步在重建前都不能跑——悄悄当成首次同步会让删掉的记录复活。
/// 没有数据库就没有基线可言，按没坏处理。
fn baseline_broken_at(data_dir: &Path) -> bool {
    let Ok(db) = open_existing_db_at(data_dir) else {
        return false;
    };
    baseline_broken_with(&db, &DpapiSealer::new(db.vault_account().to_owned()), data_dir)
}

/// 同 [`baseline_broken_at`]，数据库和加密实现由调用方给，测试用它绕开 DPAPI。
fn baseline_broken_with(
    db: &Database,
    sealer: &dyn zssh_core::sync::sealer::SecretSealer,
    data_dir: &Path,
) -> bool {
    SyncEngine::new(db, sealer, "", data_dir, None, None)
        .load_baseline()
        .is_err()
}

/// 重建同步基线：删掉损坏的基线，下次同步按首次同步处理（两边的数据都保留）。
/// 和同步互斥；走核心的 `reset_baseline`。
fn reset_baseline_at(data_dir: &Path) -> Result<(), String> {
    let _guard = SyncInFlight::acquire(ExclusiveKind::Restore)
        .map_err(|holder| exclusive_busy_message(ExclusiveKind::Restore, holder).to_owned())?;
    let db = open_db_at(data_dir)?;
    SyncEngine::new(&db, &DpapiSealer::new(db.vault_account().to_owned()), "", data_dir, None, None)
        .reset_baseline()
        .map_err(|err| crate::error_text::error_text(&err))
}

/// 自动同步的计时：上次同步（自动或手动）的起点和连续失败次数。
///
/// 不落盘：重启后从本机保存的「上次结果」时间重新算，失败次数从 0 开始。
#[derive(Debug, Default)]
struct AutoClock {
    last_run_at: Option<i64>,
    backoff: AutoBackoff,
}

impl AutoClock {
    /// 自动同步开始：间隔从这一刻算，跑多久都不影响下一拍的时间。
    fn started(&mut self, now: i64) {
        self.last_run_at = Some(now);
    }

    fn succeeded(&mut self) {
        self.backoff.reset();
    }

    fn failed(&mut self) {
        self.backoff.failed();
    }

    /// 手动同步：正常算一次同步，退避清零、间隔从现在重新算。
    fn manual(&mut self, now: i64) {
        self.last_run_at = Some(now);
        self.backoff.reset();
    }

    /// 这一拍到没到点（间隔 = 设置里的分钟数 × 退避倍数）。
    /// `saved_last` 是本机保存的上次结果时间：进程刚启动、内存里还没记过时用它接着算。
    fn due(&self, saved_last: Option<i64>, now: i64, interval_minutes: u32) -> bool {
        auto_due(
            self.last_run_at.or(saved_last),
            now,
            self.backoff.interval_secs(interval_minutes),
        )
    }
}

/// 自动同步失败后的退避倍数：1x、2x、4x，最多 4x。
/// 手动同步一次或自动同步成功一次就清零。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AutoBackoff {
    /// 连续失败次数；只用来算倍数，不显示。
    failures: u32,
}

impl AutoBackoff {
    fn multiplier(self) -> u32 {
        match self.failures {
            0 => 1,
            1 => 2,
            _ => 4,
        }
    }

    /// 这一拍至少要隔多久才能再自动同步。
    fn interval_secs(self, minutes: u32) -> i64 {
        i64::from(clamp_interval(minutes)) * 60 * i64::from(self.multiplier())
    }

    fn failed(&mut self) {
        self.failures = self.failures.saturating_add(1);
    }

    fn reset(&mut self) {
        self.failures = 0;
    }
}

/// 到没到自动同步的点。没有记录（从没同步过）按到点处理，让第一次自动同步不等
/// 一个间隔；时钟倒退（系统时间被调回）按没到点处理。
fn auto_due(last_run_at: Option<i64>, now: i64, interval_secs: i64) -> bool {
    match last_run_at {
        None => true,
        Some(last) => now.saturating_sub(last) >= interval_secs,
    }
}

/// 自动同步在本机配置上能不能跑，加上 0.2.14 控制器里的几道门：还没和云端对上过
/// （第一次必须手动，用户要先确认主密码）、基线损坏、保护状态（限流 / 凭据被拒 /
/// 主密码不对 / 待确认的删除）。返回暂停原因，`None` = 可以跑。
fn auto_block_reason(
    enabled: bool,
    has_password: bool,
    has_config: bool,
    ready: bool,
    baseline_broken: bool,
    guard_reason: Option<String>,
) -> Option<String> {
    if let Some(reason) = auto_pause_reason(enabled, has_password, has_config) {
        return Some(reason.to_owned());
    }
    if baseline_broken {
        return Some(t(SyncMsg::AutoBaselineBroken).to_owned());
    }
    if !ready {
        return Some(t(SyncMsg::AutoNotReady).to_owned());
    }
    guard_reason
}

/// 自动同步能不能跑：返回暂停原因，`None` = 可以跑。
/// 开关关着也算暂停，但界面只在打开时显示「自动同步暂停」。
fn auto_pause_reason(enabled: bool, has_password: bool, has_config: bool) -> Option<&'static str> {
    if !enabled {
        return Some(t(SyncMsg::AutoOff));
    }
    if !has_config {
        return Some(t(SyncMsg::AutoNoStorage));
    }
    if !has_password {
        return Some(t(SyncMsg::AutoNoPassword));
    }
    None
}

/// 拉起进程级的自动同步调度器：主窗口启动时调一次，多个同步窗口共用。
///
/// 任务挂在 `App` 上；进程退出（`App` 被丢掉）时任务一起结束，不会留下后台线程。
pub fn spawn_auto_sync(cx: &mut App) {
    if AUTO_SCHEDULER_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(AUTO_SYNC_TICK).await;
            auto_sync_tick();
        }
    })
    .detach();
}

/// 调度器的一拍：先把判断和同步放进阻塞线程池，界面线程只负责计时。
fn auto_sync_tick() {
    let Ok(runtime) = crate::ssh_backend::runtime() else {
        return;
    };
    let handle = runtime.handle().clone();
    drop(runtime.spawn_blocking(move || {
        let data_dir = crate::settings::data_dir();
        if let Some(result) = auto_tick_at(&handle, &data_dir, now_secs()) {
            broadcast_auto_result(result);
        }
    }));
}

/// 自动同步的一拍（阻塞线程）：真的跑了一次就返回这次的结果，其他情况返回 `None`。
///
/// 需要用户处理的状态下一律不跑：开关关了、没保存存储、没记住主密码、还没到间隔、
/// 或者手动同步正在跑（同一个同步开关）。
fn auto_tick_at(handle: &tokio::runtime::Handle, data_dir: &Path, now: i64) -> Option<LastResult> {
    let db = open_db_at(data_dir)?;
    let sealer = DpapiSealer::new(db.vault_account().to_owned());
    auto_tick_with(handle, data_dir, now, &sealer)
}

/// 同 [`auto_tick_at`]，加密实现由调用方给，测试用它绕开 DPAPI。
fn auto_tick_with(
    handle: &tokio::runtime::Handle,
    data_dir: &Path,
    now: i64,
    sealer: &dyn SecretSealer,
) -> Option<LastResult> {
    let db = open_existing_db_at(data_dir).ok()?;
    let settings = service::load_auto(&db).ok()?;
    let configured = service::load_config(&db, sealer).ok().flatten().is_some();
    // 读不出来的主密码（换了电脑、密文损坏）按「没记住」处理：不跑，也不猜。
    let password = service::load_master_password(&db, sealer).ok().flatten();
    let saved_last = service::load_last_result(&db).ok().flatten();
    // 读不出来的"已对上过"按没对上处理：宁可不自动跑。
    let ready = service::is_ready(&db).unwrap_or(false);
    let baseline_broken = baseline_broken_with(&db, sealer, data_dir);
    drop(db);
    let guard_reason = with_sync_guard(|guard| guard.block_reason(Instant::now()));
    if auto_block_reason(
        settings.enabled,
        password.is_some(),
        configured,
        ready,
        baseline_broken,
        guard_reason,
    )
    .is_some()
    {
        return None;
    }
    let password = password?;
    let saved_last = saved_last.as_ref().map(|result| result.at);
    if !with_auto_clock(|clock| clock.due(saved_last, now, settings.interval_minutes)) {
        return None;
    }
    // 同库只跑一个同步：手动同步（或恢复 / 导入）还在跑时这一拍直接跳过，下一拍再看。
    let guard = SyncInFlight::acquire(ExclusiveKind::Sync).ok()?;
    with_auto_clock(|clock| clock.started(now));
    let outcome = run_auto_sync_with(handle, data_dir, sealer, password.as_str());
    // 主同步成功之后顺带备份到各个云端备份目标；失败只记在目标自己的状态里，不影响这次同步的结果。
    if matches!(&outcome, Ok(report) if report.status == SyncStatus::Completed) {
        cloud_backup::backup_after_sync(handle, data_dir, sealer, password.as_str());
    }
    drop(guard);
    let finished = now_secs();
    let result = match outcome {
        Ok(report) => {
            with_sync_guard(|guard| {
                guard.note(&SyncOutcome::Report(report.status), Instant::now());
            });
            with_auto_clock(AutoClock::succeeded);
            last_result_from_report(&report, true, finished)
        }
        Err(failure) => {
            with_sync_guard(|guard| {
                guard.note(&SyncOutcome::Failed(failure.kind.clone()), Instant::now());
            });
            with_auto_clock(AutoClock::failed);
            last_result_error(&failure.message, true, finished)
        }
    };
    // 结果写进本机设置：窗口没开、或者重启之后也看得到上次结果。
    if let Ok(db) = open_existing_db_at(data_dir) {
        let _ = service::save_last_result(&db, &result);
    }
    Some(result)
}

/// 自动同步在阻塞线程里跑一次：和「立即同步」同一套核心入口，只是带上总超时。
/// 主密码不出这个函数，也不写日志。
fn run_auto_sync_with(
    handle: &tokio::runtime::Handle,
    data_dir: &Path,
    sealer: &dyn SecretSealer,
    password: &str,
) -> Result<SyncReport, SyncFailure> {
    let db = open_existing_db_at(data_dir)?;
    ensure_github_gist(handle, &db, sealer)?;
    let device_id = db.sync_device_id().map_err(|err| {
        format!(
            "{}{}",
            t(SyncMsg::ReadDeviceIdFailedPrefix),
            crate::error_text::error_text(&err)
        )
    })?;
    let context = SyncContext {
        db: &db,
        sealer,
        device_id,
        data_dir: data_dir.to_path_buf(),
        known_hosts_path: Some(data_dir.join("known_hosts")),
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        onedrive_endpoints: onedrive_endpoints(),
    };
    let result = handle.block_on(async {
        match tokio::time::timeout(AUTO_SYNC_TIMEOUT, context.run_sync(password, None)).await {
            Ok(result) => result.map_err(|err| SyncFailure::from_sync_error(&err)),
            Err(_) => Err(SyncFailure::from(format!(
                "{}{}{}",
                t(SyncMsg::AutoSyncTimeoutPrefix),
                AUTO_SYNC_TIMEOUT.as_secs() / 60,
                t(SyncMsg::AutoSyncTimeoutSuffix)
            ))),
        }
    });
    result.map_err(|failure| finish_failure(failure, data_dir, sealer))
}

/// 失败后重新检查基线能不能读，界面据此提供「重建基线」。
fn finish_failure(
    mut failure: SyncFailure,
    data_dir: &Path,
    sealer: &dyn SecretSealer,
) -> SyncFailure {
    failure.baseline_broken = failure.kind == FailureKind::BaselineBroken
        || open_existing_db_at(data_dir)
            .is_ok_and(|db| baseline_broken_with(&db, sealer, data_dir));
    failure
}

/// 调度器的内存状态；锁中毒时拿回内部值继续用（这些状态丢了也只是多跑一次）。
fn with_auto_clock<R>(f: impl FnOnce(&mut AutoClock) -> R) -> R {
    let clock = AUTO_CLOCK.get_or_init(|| Mutex::new(AutoClock::default()));
    let mut clock = clock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut clock)
}

/// 让一个同步窗口接收自动同步的结果；窗口关掉后接收端失效，下次广播时清掉。
fn listen_auto(sender: Sender<SyncEvent>) {
    let listeners = AUTO_LISTENERS.get_or_init(|| Mutex::new(Vec::new()));
    let mut listeners = listeners
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    listeners.push(sender);
}

/// 自动同步跑完：把结果推给已打开的窗口（没开窗口也没关系，结果已经在本机设置里）。
fn broadcast_auto_result(result: LastResult) {
    let Some(listeners) = AUTO_LISTENERS.get() else {
        return;
    };
    let mut listeners = listeners
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    listeners.retain(|sender| sender.send(SyncEvent::AutoSynced(result.clone())).is_ok());
}

/// WebDAV 表单里的原始值；`build_webdav` 负责裁剪、合并旧密码和校验。
///
/// 三个表单的 `Debug` 都是手写的：密码 / Secret / 令牌是用户刚敲进去的明文，断言失败或 `{:?}` 时不能露出来。
#[derive(Clone, Default, PartialEq, Eq)]
struct WebDavForm {
    url: String,
    directory: String,
    username: String,
    password: String,
    allow_insecure_http: bool,
}

impl std::fmt::Debug for WebDavForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebDavForm")
            .field("url", &self.url)
            .field("directory", &self.directory)
            .field("username", &self.username)
            .field("password", &redacted(&self.password))
            .field("allow_insecure_http", &self.allow_insecure_http)
            .finish()
    }
}

/// `Debug` 里代替秘密的占位：只说有没有填，不说内容和长度。
fn redacted(secret: &str) -> &'static str {
    if secret.is_empty() {
        "<空>" // i18n-allow: Debug 输出的占位，不上屏
    } else {
        "<已隐藏>" // i18n-allow: Debug 输出的占位，不上屏
    }
}

/// S3 表单里的原始值；`build_s3` 负责裁剪、合并旧 Secret 和校验。
#[derive(Clone, PartialEq, Eq)]
struct S3Form {
    endpoint: String,
    region: String,
    bucket: String,
    access_key: String,
    secret_key: String,
    prefix: String,
    path_style: bool,
}

impl std::fmt::Debug for S3Form {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Form")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("access_key", &self.access_key)
            .field("secret_key", &redacted(&self.secret_key))
            .field("prefix", &self.prefix)
            .field("path_style", &self.path_style)
            .finish()
    }
}

impl Default for S3Form {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            region: String::new(),
            bucket: String::new(),
            access_key: String::new(),
            secret_key: String::new(),
            prefix: String::new(),
            // 和核心 `S3Config` 的默认一致：路径风格兼容 MinIO 这类自建服务。
            path_style: true,
        }
    }
}

/// GitHub Gist 表单里的原始值；`build_github` 负责裁剪、合并旧令牌和校验。
#[derive(Clone, Default, PartialEq, Eq)]
struct GithubForm {
    /// 空 = 用核心的默认地址 `https://api.github.com`。
    api_base: String,
    token: String,
    /// 空 = 第一次同步时让 ZeeTerm 新建一个。
    gist_id: String,
    allow_insecure_http: bool,
}

impl std::fmt::Debug for GithubForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GithubForm")
            .field("api_base", &self.api_base)
            .field("token", &redacted(&self.token))
            .field("gist_id", &self.gist_id)
            .field("allow_insecure_http", &self.allow_insecure_http)
            .finish()
    }
}

/// OneDrive 表单里的原始值。只有「高级」里的 client ID；账号和令牌都由登录流程
/// 交给核心保存，界面上不回填、也拿不到。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct OneDriveForm {
    /// 空 = 用编进程序里的 client ID。
    client_id: String,
}

impl OneDriveForm {
    /// 交给核心里登录用的 client ID：先裁掉首尾空白，空串表示「用内置的」。
    fn client_id(&self) -> &str {
        self.client_id.trim()
    }
}

/// 四套表单的草稿。切换存储只改 `provider`，其他没保存的输入原样留着。
#[derive(Default)]
struct ProviderDrafts {
    provider: ProviderChoice,
    webdav: WebDavForm,
    s3: S3Form,
    github: GithubForm,
    onedrive: OneDriveForm,
}

impl ProviderDrafts {
    fn select(&mut self, provider: ProviderChoice) {
        self.provider = provider;
    }

    /// 当前选中的那家 → `StorageConfig`；校验规则和保存、测试共用。
    fn selected_config(&self, saved: Option<&StorageConfig>) -> Result<StorageConfig, String> {
        match self.provider {
            ProviderChoice::WebDav => build_webdav(&self.webdav, saved),
            ProviderChoice::S3 => build_s3(&self.s3, saved),
            ProviderChoice::Github => build_github(&self.github, saved),
            // OneDrive 的连接和令牌都由登录保存，表单里的 client ID 不能单独拼出连接；
            // 测试连接和同步都用本机保存的那份。
            ProviderChoice::OneDrive => build_onedrive(saved),
        }
    }
}

/// 已保存配置 → WebDAV 表单。密码字段永远留空：界面拿不到、也不回填已保存的密码。
/// 不是 WebDAV 时给默认值，避免把 S3 的配置误填进另一套草稿。
fn form_from_view(view: &ConfigView) -> WebDavForm {
    if view.provider != ProviderChoice::WebDav.key() {
        return WebDavForm::default();
    }
    WebDavForm {
        url: view.url.clone(),
        directory: view.directory.clone(),
        username: view.username.clone(),
        password: String::new(),
        allow_insecure_http: view.allow_insecure_http,
    }
}

/// 已保存配置 → S3 表单。Secret 永远留空；不是 S3 时给默认值。
fn s3_form_from_view(view: &ConfigView) -> S3Form {
    if view.provider != ProviderChoice::S3.key() {
        return S3Form::default();
    }
    S3Form {
        endpoint: view.endpoint.clone(),
        region: view.region.clone(),
        bucket: view.bucket.clone(),
        access_key: view.access_key.clone(),
        secret_key: String::new(),
        prefix: view.prefix.clone(),
        path_style: view.path_style,
    }
}

/// 已保存配置 → GitHub 表单。令牌永远留空（界面拿不到）；不是 GitHub 时给默认值。
/// `ConfigView.api_base` 是核心算好的实际地址，所以留空保存过的配置这里回填默认地址。
fn github_form_from_view(view: &ConfigView) -> GithubForm {
    if view.provider != ProviderChoice::Github.key() {
        return GithubForm::default();
    }
    GithubForm {
        api_base: view.api_base.clone(),
        token: String::new(),
        gist_id: view.gist_id.clone(),
        allow_insecure_http: view.allow_insecure_http,
    }
}

/// 已保存配置 → OneDrive 表单。只有「高级」里的 client ID 会回填；账号由登录流程
/// 记下，只在界面上显示。不是 OneDrive 时给默认值。
fn onedrive_form_from_view(view: &ConfigView) -> OneDriveForm {
    if view.provider != ProviderChoice::OneDrive.key() {
        return OneDriveForm::default();
    }
    OneDriveForm {
        client_id: view.client_id.clone(),
    }
}

/// OneDrive 的「账号」一行：登录过就显示 `ConfigView.account`，否则「未登录」。
fn onedrive_account_label(saved: Option<&StorageConfig>) -> String {
    let Some(config @ StorageConfig::Onedrive(_)) = saved else {
        return t(SyncMsg::OneDriveNotSignedIn).to_owned();
    };
    let account = config.view().account;
    if account.trim().is_empty() {
        t(SyncMsg::OneDriveNotSignedIn).to_owned()
    } else {
        account
    }
}

/// OneDrive 没有可以离线填写的连接信息：没登录过就没有可用的 `StorageConfig`，
/// 测试连接和同步都要先让用户登录。
fn build_onedrive(saved: Option<&StorageConfig>) -> Result<StorageConfig, String> {
    match saved {
        Some(config @ StorageConfig::Onedrive(_)) => Ok(config.clone()),
        _ => Err(t(SyncMsg::OneDriveSignInRequired).to_owned()),
    }
}

/// WebDAV 表单 → `StorageConfig`。地址和用户名不能为空；`http://` 必须显式确认。
/// 密码留空时用 `merge_saved_secret` 沿用已保存的那份。
fn build_webdav(form: &WebDavForm, saved: Option<&StorageConfig>) -> Result<StorageConfig, String> {
    let config = WebDavConfig {
        url: form.url.trim().to_owned(),
        directory: form.directory.trim().to_owned(),
        username: form.username.trim().to_owned(),
        password: form.password.clone(),
        allow_insecure_http: form.allow_insecure_http,
    };
    if config.url.is_empty() {
        return Err(t(SyncMsg::WebdavUrlRequired).to_owned());
    }
    if config.username.is_empty() {
        return Err(t(SyncMsg::WebdavUsernameRequired).to_owned());
    }
    if config.is_plain_http() && !config.allow_insecure_http {
        return Err(t(SyncMsg::WebdavInsecureHttp).to_owned());
    }
    let mut config = StorageConfig::Webdav(config);
    config.merge_saved_secret(saved_for_same_location(&config, saved));
    Ok(config)
}

/// 密码 / Secret / 令牌留空 = 沿用已保存的，但**只在地址没变时**：把地址改成另一台
/// 服务器、密码框留空，旧服务器的密码不能被带到新地址上去（审查 D2-16）。
/// 地址变了就按"没有已保存的"处理，留空的密码就是空的（S3 / GitHub 会提示重新填）。
fn saved_for_same_location<'a>(
    config: &StorageConfig,
    saved: Option<&'a StorageConfig>,
) -> Option<&'a StorageConfig> {
    fn same(a: &str, b: &str) -> bool {
        a.trim()
            .trim_end_matches('/')
            .eq_ignore_ascii_case(b.trim().trim_end_matches('/'))
    }
    let saved = saved?;
    let unchanged = match (config, saved) {
        (StorageConfig::Webdav(new), StorageConfig::Webdav(old)) => same(&new.url, &old.url),
        (StorageConfig::S3(new), StorageConfig::S3(old)) => same(&new.endpoint, &old.endpoint),
        (StorageConfig::Github(new), StorageConfig::Github(old)) => {
            same(new.effective_api_base(), old.effective_api_base())
        }
        // 不同家 / OneDrive 没有可沿用的密码，交给核心原样处理。
        _ => true,
    };
    unchanged.then_some(saved)
}

/// S3 表单 → `StorageConfig`。Endpoint 只允许 http(s)，Bucket / Region / Access Key
/// 不能为空；Secret 留空时用 `merge_saved_secret` 沿用已保存的那份。
fn build_s3(form: &S3Form, saved: Option<&StorageConfig>) -> Result<StorageConfig, String> {
    let endpoint = form.endpoint.trim().to_owned();
    if endpoint.is_empty() {
        return Err(t(SyncMsg::S3EndpointRequired).to_owned());
    }
    validate_s3_endpoint(&endpoint)?;
    let bucket = form.bucket.trim().to_owned();
    if bucket.is_empty() {
        return Err(t(SyncMsg::S3BucketRequired).to_owned());
    }
    if bucket.contains('/') {
        return Err(t(SyncMsg::S3BucketSlash).to_owned());
    }
    let access_key = form.access_key.trim().to_owned();
    if access_key.is_empty() {
        return Err(t(SyncMsg::S3AccessKeyRequired).to_owned());
    }
    let region = form.region.trim().to_owned();
    if region.is_empty() {
        return Err(t(SyncMsg::S3RegionRequired).to_owned());
    }
    let mut config = StorageConfig::S3(S3Config {
        endpoint,
        region,
        bucket,
        access_key,
        secret_key: form.secret_key.clone(),
        prefix: form.prefix.trim().to_owned(),
        path_style: form.path_style,
    });
    config.merge_saved_secret(saved_for_same_location(&config, saved));
    if let StorageConfig::S3(config) = &config
        && config.secret_key.is_empty()
    {
        return Err(t(SyncMsg::S3SecretRequired).to_owned());
    }
    Ok(config)
}

/// Endpoint 只接受 http(s)，和核心拼地址时的限制一致；提前报错，不用等测试连接。
fn validate_s3_endpoint(endpoint: &str) -> Result<(), String> {
    let lower = endpoint.trim().to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .ok_or_else(|| t(SyncMsg::S3EndpointScheme).to_owned())?;
    if rest.is_empty() || rest.starts_with('/') {
        return Err(t(SyncMsg::S3EndpointNoHost).to_owned());
    }
    Ok(())
}

/// GitHub 表单 → `StorageConfig`。API 地址可空（核心用默认地址）；`http://` 必须显式
/// 确认，因为令牌会以明文发出去。令牌留空时用 `merge_saved_secret` 沿用已保存的那份。
fn build_github(form: &GithubForm, saved: Option<&StorageConfig>) -> Result<StorageConfig, String> {
    let api_base = form.api_base.trim().to_owned();
    if !api_base.is_empty() {
        validate_github_api_base(&api_base, form.allow_insecure_http)?;
    }
    let mut config = StorageConfig::Github(GithubConfig {
        api_base,
        token: form.token.clone(),
        gist_id: form.gist_id.trim().to_owned(),
        allow_insecure_http: form.allow_insecure_http,
    });
    config.merge_saved_secret(saved_for_same_location(&config, saved));
    if let StorageConfig::Github(config) = &config
        && config.token.trim().is_empty()
    {
        return Err(t(SyncMsg::GithubTokenRequired).to_owned());
    }
    Ok(config)
}

/// API 地址的规则和核心 [`github::GithubApi::new`] 一致：https，或者确认过的 http；
/// 提前报错，不用等测试连接。
fn validate_github_api_base(api_base: &str, allow_insecure_http: bool) -> Result<(), String> {
    let lower = api_base.trim().to_ascii_lowercase();
    let (scheme, rest) = if let Some(rest) = lower.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = lower.strip_prefix("http://") {
        ("http", rest)
    } else {
        return Err(t(SyncMsg::GithubApiScheme).to_owned());
    };
    if rest.is_empty() || rest.starts_with('/') {
        return Err(t(SyncMsg::GithubApiNoHost).to_owned());
    }
    if scheme == "http" && !allow_insecure_http {
        return Err(t(SyncMsg::GithubInsecureHttp).to_owned());
    }
    Ok(())
}

/// 同步只用本机保存的设置；选中的存储和保存的不一致时先让用户保存，
/// 免得「看到的是 A、同步的是 B」。OneDrive 没登录过直接提示先登录。
fn sync_guard(selected: ProviderChoice, saved: Option<&StorageConfig>) -> Result<(), String> {
    if selected == ProviderChoice::OneDrive && !matches!(saved, Some(StorageConfig::Onedrive(_))) {
        return Err(t(SyncMsg::OneDriveSignInRequired).to_owned());
    }
    match saved {
        None => Err(format!(
            "{}{}{}",
            t(SyncMsg::SaveSettingsFirstPrefix),
            selected.label(),
            t(SyncMsg::SaveSettingsFirstSuffix)
        )),
        Some(config) if config.provider() != selected.key() => {
            Err(t(SyncMsg::SaveCurrentSettingsFirst).to_owned())
        }
        Some(_) => Ok(()),
    }
}

/// 连接测试结果给界面的文本：读、写、并发保护（逐条说明）和云端有没有数据。
/// 失败原因原样显示，不改写核心返回的内容。
fn probe_text(probe: &ConnectionProbe) -> String {
    let mut lines = vec![
        check_line(t(SyncMsg::ProbeRead), &probe.read),
        check_line(t(SyncMsg::ProbeWrite), &probe.write),
    ];
    match &probe.concurrency {
        Some(concurrency) => {
            lines.push(format!(
                "{}{}",
                t(SyncMsg::ConcurrencyPrefix),
                concurrency_label(concurrency.mode)
            ));
            for note in &concurrency.notes {
                lines.push(format!("{}{note}", t(SyncMsg::ConcurrencyNotePrefix)));
            }
        }
        None => lines.push(t(SyncMsg::ConcurrencyNotChecked).to_owned()),
    }
    lines.push(if probe.has_existing_data {
        t(SyncMsg::CloudHasData).to_owned()
    } else {
        t(SyncMsg::CloudEmpty).to_owned()
    });
    lines.join("\n")
}

fn check_line(label: &str, check: &zssh_core::sync::remote::Check) -> String {
    if check.ok {
        format!(
            "{prefix}{label}{middle}{message}",
            prefix = "",
            middle = t(SyncMsg::CheckOkMiddle),
            message = check.message
        )
    } else {
        format!(
            "{prefix}{label}{middle}{message}",
            prefix = "",
            middle = t(SyncMsg::CheckFailedMiddle),
            message = check.message
        )
    }
}

fn concurrency_label(mode: ConcurrencyMode) -> &'static str {
    match mode {
        ConcurrencyMode::Conditional => t(SyncMsg::ConcurrencyConditional),
        ConcurrencyMode::ReadBeforeWrite => t(SyncMsg::ConcurrencyReadBeforeWrite),
    }
}

/// 「记住主密码」的开关状态。窗口打开时按本机是否有保存决定；默认关闭。
///
/// 开关和「本机有一份保存」分开记：关掉开关只是不再保存，真正删掉密文要等
/// 下一次同步成功，界面这时提示「同步成功后清除」。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RememberPassword {
    toggled: bool,
    saved: bool,
}

impl RememberPassword {
    /// 打开窗口时用：读到已保存的主密码就打开开关；空串当作没保存。
    fn from_saved(saved: Option<&str>) -> Self {
        match saved {
            Some(text) if !text.is_empty() => Self {
                toggled: true,
                saved: true,
            },
            _ => Self::default(),
        }
    }

    fn toggle(&mut self) {
        self.toggled = !self.toggled;
    }

    /// 开关旁边的提示；`saved` 是打开窗口时读到的本机状态。
    fn hint(self) -> &'static str {
        match (self.toggled, self.saved) {
            (true, true) => t(SyncMsg::RememberSaved),
            (true, false) => t(SyncMsg::RememberPending),
            (false, true) => t(SyncMsg::RememberClearPending),
            (false, false) => t(SyncMsg::RememberOff),
        }
    }
}

/// 一次同步结束后对「记住主密码」要做的动作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RememberAction {
    /// 开关打开、同步成功：写进本机（DPAPI 加密）。
    Save,
    /// 开关关闭、同步成功：删掉本机保存的那份。
    Clear,
    /// 取消、失败、需要确认：本机保存和界面都不动。
    Keep,
}

/// 只有真正同步完（完成 / 本地有变化）才算数；防误删确认和取消都不写。
fn remember_action(toggled: bool, status: Option<SyncStatus>) -> RememberAction {
    match status {
        Some(SyncStatus::Completed | SyncStatus::LocalChanged) => {
            if toggled {
                RememberAction::Save
            } else {
                RememberAction::Clear
            }
        }
        _ => RememberAction::Keep,
    }
}

/// 同步结束后 footer 要显示的文字；`Keep` 不覆盖原有状态。
fn remember_footer(action: RememberAction) -> Option<&'static str> {
    match action {
        RememberAction::Save => Some(t(SyncMsg::MasterPasswordRemembered)),
        RememberAction::Clear => Some(t(SyncMsg::MasterPasswordForgotten)),
        RememberAction::Keep => None,
    }
}

// 清除 / 退出 OneDrive 的两段确认：第一次点击只装填，第二次 `take().is_some()` 才执行。
// 不带载荷，就是通用的 `ArmedConfirm<()>`（见 `crate::armed`）。
type ClearConfirm = ArmedConfirm<()>;
type LogoutConfirm = ArmedConfirm<()>;

/// 冲突面板：列表、错误和「改用另一份」的两段确认。
///
/// 只有在同一行上连点两次才会真的执行；换一行、点「取消」或者刷新列表都会
/// 清掉确认状态，免得隔了一会儿误点。
#[derive(Default)]
struct ConflictPanel {
    items: Vec<ConflictView>,
    /// 读取或操作失败的原因，原文显示；不清空已有列表。
    error: Option<String>,
    /// 已经点过一次「改用另一份」的冲突 id。
    armed: Option<i64>,
}

impl ConflictPanel {
    fn new(items: Vec<ConflictView>, error: Option<String>) -> Self {
        Self {
            items,
            error,
            armed: None,
        }
    }

    fn set_loaded(&mut self, items: Vec<ConflictView>) {
        self.items = items;
        self.error = None;
        self.armed = None;
    }

    /// 失败只记错误：列表原样留着，免得看起来像冲突已经被处理掉。
    fn set_error(&mut self, message: String) {
        self.error = Some(message);
    }

    fn disarm(&mut self) {
        self.armed = None;
    }

    fn is_armed(&self, id: i64) -> bool {
        self.armed == Some(id)
    }

    /// 返回 true 表示这次点击已经确认、可以执行；第一次点击只置位。
    fn confirm(&mut self, id: i64) -> bool {
        if self.armed == Some(id) {
            self.armed = None;
            true
        } else {
            self.armed = Some(id);
            false
        }
    }
}

/// 一条冲突在界面上要显示的文字。纯数据，方便单测。
#[derive(Clone, Debug, PartialEq, Eq)]
struct ConflictRow {
    id: i64,
    /// 记录名称；核心没给名称时回退到主键。
    title: String,
    entity: String,
    kept: String,
    other: String,
    deletion: bool,
    can_use_other: bool,
    message: String,
    armed: bool,
}

/// 冲突 → 显示文字。文案和前端设置页一致：本机 / 另一台电脑（设备 ID 前 8 位）。
fn conflict_row(view: &ConflictView, armed: bool) -> ConflictRow {
    let title = if view.name.trim().is_empty() {
        view.key.clone()
    } else {
        view.name.clone()
    };
    let kept = if view.deletion {
        t(SyncMsg::KeptDeletion).to_owned()
    } else {
        format!(
            "{}{}{}{}",
            t(SyncMsg::KeptChangePrefix),
            device_label(view.kept_is_local, view.kept_device.as_deref()),
            t(SyncMsg::KeptChangeMiddle),
            view.kept_summary
        )
    };
    let other = if view.deletion {
        format!(
            "{}{}{}{}",
            t(SyncMsg::OtherLastContentPrefix),
            device_label(view.other_is_local, view.other_device.as_deref()),
            t(SyncMsg::OtherLastContentMiddle),
            view.other_summary
        )
    } else {
        format!(
            "{}{}{}{}",
            t(SyncMsg::OtherChangePrefix),
            device_label(view.other_is_local, view.other_device.as_deref()),
            t(SyncMsg::OtherChangeMiddle),
            view.other_summary
        )
    };
    ConflictRow {
        id: view.id,
        title,
        entity: entity_label(&view.entity).to_owned(),
        kept,
        other,
        deletion: view.deletion,
        can_use_other: view.can_use_other,
        message: view.message.clone(),
        armed,
    }
}

fn entity_label(entity: &str) -> &str {
    match entity {
        "host" => t(SyncMsg::EntityHost),
        "group" => t(SyncMsg::EntityGroup),
        "identity" => t(SyncMsg::EntityIdentity),
        "proxy" => t(SyncMsg::EntityProxy),
        "known_host" => t(SyncMsg::EntityKnownHost),
        "setting" => t(SyncMsg::EntitySetting),
        other => other,
    }
}

/// 哪台电脑：本机，或者另一台（设备 ID 只显示前 8 位）。
fn device_label(is_local: bool, device: Option<&str>) -> String {
    if is_local {
        return t(SyncMsg::ThisComputer).to_owned();
    }
    match device {
        Some(device) if !device.is_empty() => {
            format!(
                "{}{}{}",
                t(SyncMsg::OtherComputerPrefix),
                device.chars().take(8).collect::<String>(),
                t(SyncMsg::OtherComputerSuffix)
            )
        }
        _ => t(SyncMsg::OtherComputer).to_owned(),
    }
}

/// 同步结果和防误删确认状态。
///
/// `NeedsConfirmation` 时把这次用的主密码留在这里，等用户选「允许删除并继续」
/// 或「取消」再原样交给核心；其他状态下密码立刻丢掉。为了不把密码带进日志，
/// 这里不实现 `Debug`。
#[derive(Default)]
struct SyncState {
    report: Option<SyncReport>,
    /// 只给防误删确认用；字符串内容不打印。
    pending: Option<String>,
    /// 后台同步的失败原因，原文显示。
    error: Option<String>,
}

impl SyncState {
    /// 开始新一轮：上一次的结果和密码都不能留给这一次用。
    fn begin(&mut self) {
        self.report = None;
        self.pending = None;
        self.error = None;
    }

    /// 后台同步返回：需要确认时留下密码，其他情况清掉。
    fn finish(&mut self, report: SyncReport, password: String) {
        self.pending = if report.status == SyncStatus::NeedsConfirmation {
            Some(password)
        } else {
            None
        };
        self.error = None;
        self.report = Some(report);
    }

    /// 同步失败：错误原样保留，旧结果和密码都不再有效。
    fn fail(&mut self, message: String) {
        self.report = None;
        self.pending = None;
        self.error = Some(message);
    }

    fn needs_confirmation(&self) -> bool {
        self.pending.is_some()
    }

    fn pending_password(&self) -> Option<&str> {
        self.pending.as_deref()
    }
}

/// 「自动同步」一块的显示数据。设置和上次结果都来自本机设置表；读不出来时保留
/// 默认值 / 旧值，只把错误显示出来。
#[derive(Debug, Default)]
struct AutoPanel {
    settings: AutoSettings,
    last: Option<LastResult>,
    error: Option<String>,
}

/// 间隔按核心的规则夹到 1–1440 分钟。
fn clamp_interval(minutes: u32) -> u32 {
    minutes.clamp(MIN_AUTO_INTERVAL_MINUTES, MAX_AUTO_INTERVAL_MINUTES)
}

/// 读输入框里的间隔：不是整数或者超出 1–1440 就给 `None`（不偷偷改成边界值，
/// 用户才知道自己填错了）。
fn parse_interval(text: &str) -> Option<u32> {
    let minutes = text.trim().parse::<u32>().ok()?;
    if !(MIN_AUTO_INTERVAL_MINUTES..=MAX_AUTO_INTERVAL_MINUTES).contains(&minutes) {
        return None;
    }
    Some(clamp_interval(minutes))
}

/// 一次同步结果 → 本机保存的「上次结果」；`auto` 标这次是不是自动触发的。
fn last_result_from_report(report: &SyncReport, auto: bool, at: i64) -> LastResult {
    LastResult {
        at,
        status: report_status_key(report.status).to_owned(),
        summary: report.summary.clone(),
        version: report.cloud_version.clone(),
        conflicts: report.conflicts.len(),
        auto,
    }
}

/// 同步失败时的「上次结果」：摘要放错误原文，不带密码。
fn last_result_error(message: &str, auto: bool, at: i64) -> LastResult {
    LastResult {
        at,
        status: ERROR_STATUS.to_owned(),
        summary: message.to_owned(),
        version: None,
        conflicts: 0,
        auto,
    }
}

/// 和 `LastResult.status` 的约定一致（核心只存字符串）。
fn report_status_key(status: SyncStatus) -> &'static str {
    match status {
        SyncStatus::Completed => "completed",
        SyncStatus::LocalChanged => "local_changed",
        SyncStatus::NeedsConfirmation => "needs_confirmation",
        SyncStatus::Cancelled => "cancelled",
    }
}

/// 打开窗口时读自动同步设置和上次结果；读不出来只显示错误，不影响同步设置本身。
fn load_auto_panel(data_dir: &Path) -> AutoPanel {
    let mut panel = AutoPanel::default();
    match load_auto_at(data_dir) {
        Ok(settings) => panel.settings = settings,
        Err(message) => panel.error = Some(message),
    }
    match load_last_result_at(data_dir) {
        Ok(last) => panel.last = last,
        Err(message) => {
            if panel.error.is_none() {
                panel.error = Some(message);
            }
        }
    }
    panel
}

/// 打开窗口时读本机设置；库文件不存在就直接当没配置，不为了读设置新建库。
fn load_saved() -> (Option<StorageConfig>, Option<ConfigView>, String) {
    match load_saved_config(&crate::settings::data_dir()) {
        Ok(Some(config)) => {
            let view = config.view();
            if ProviderChoice::from_provider(&view.provider).is_some() {
                (Some(config), Some(view), String::new())
            } else {
                let message = format!(
                    "{}{}{}",
                    t(SyncMsg::UnsupportedProviderPrefix),
                    provider_label(config.provider()),
                    t(SyncMsg::UnsupportedProviderSuffix)
                );
                (None, None, message)
            }
        }
        Ok(None) => (None, None, String::new()),
        Err(message) => (None, None, message),
    }
}

fn provider_label(provider: &str) -> &str {
    match provider {
        "webdav" => "WebDAV",
        "s3" => "S3",
        "github" => "GitHub Gist",
        "onedrive" => "OneDrive",
        other => other,
    }
}

fn load_saved_config(data_dir: &Path) -> Result<Option<StorageConfig>, String> {
    let path = data_dir.join("zssh.db");
    if !path.is_file() {
        return Ok(None);
    }
    let db = Database::open(&path).map_err(|err| {
        format!(
            "{}{}",
            t(SyncMsg::OpenDatabaseFailedPrefix),
            crate::error_text::error_text(&err)
        )
    })?;
    service::load_config(&db, &DpapiSealer::new(db.vault_account().to_owned())).map_err(|err| crate::error_text::error_text(&err))
}

/// 冲突列表：库文件不存在就当没有冲突，不为了看列表新建一个库。
fn load_conflicts_at(data_dir: &Path) -> Result<Vec<ConflictView>, String> {
    if !data_dir.join("zssh.db").is_file() {
        return Ok(Vec::new());
    }
    let db = open_existing_db_at(data_dir)?;
    let device_id = db.sync_device_id().map_err(|err| {
        format!(
            "{}{}",
            t(SyncMsg::ReadDeviceIdFailedPrefix),
            crate::error_text::error_text(&err)
        )
    })?;
    conflicts::list(&db, &DpapiSealer::new(db.vault_account().to_owned()), &device_id, now_secs())
        .map_err(|err| crate::error_text::error_text(&err))
}

/// 改用另一份：结果写回本地库，成功后由调用方刷新列表。
/// 会改本地数据，和同步 / 恢复互斥（0.2.14 同样走独占锁）。
fn use_other_at(data_dir: &Path, id: i64) -> Result<(), String> {
    let _guard = SyncInFlight::acquire(ExclusiveKind::Restore)
        .map_err(|holder| exclusive_busy_message(ExclusiveKind::Restore, holder).to_owned())?;
    let db = open_existing_db_at(data_dir)?;
    let device_id = db.sync_device_id().map_err(|err| {
        format!(
            "{}{}",
            t(SyncMsg::ReadDeviceIdFailedPrefix),
            crate::error_text::error_text(&err)
        )
    })?;
    let known_hosts = data_dir.join("known_hosts");
    conflicts::use_other(
        &db,
        &DpapiSealer,
        &device_id,
        Some(known_hosts.as_path()),
        id,
        now_secs(),
    )
    .map_err(|err| crate::error_text::error_text(&err))
}

/// 忽略：只删本地冲突记录，不动云端和本地数据。
fn dismiss_at(data_dir: &Path, id: i64) -> Result<(), String> {
    let db = open_existing_db_at(data_dir)?;
    conflicts::dismiss(&db, id).map_err(|err| crate::error_text::error_text(&err))
}

/// 打开已有的库。冲突只可能出现在已经有同步数据的库里，缺文件时报错而不是
/// 顺手建一个空库。
fn open_existing_db_at(data_dir: &Path) -> Result<Database, String> {
    let path = data_dir.join("zssh.db");
    if !path.is_file() {
        return Err(t(SyncMsg::NoSyncDatabase).to_owned());
    }
    Database::open(&path).map_err(|err| {
        format!(
            "{}{}",
            t(SyncMsg::OpenDatabaseFailedPrefix),
            crate::error_text::error_text(&err)
        )
    })
}

/// 冲突的保留期按秒算，和核心用同一个时钟。
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn save_config_at(data_dir: &Path, config: &StorageConfig) -> Result<(), String> {
    let db = open_db_at(data_dir)?;
    service::save_config(&db, &DpapiSealer::new(db.vault_account().to_owned()), config)
        .map_err(|err| crate::error_text::error_text(&err))
}

fn clear_config_at(data_dir: &Path) -> Result<(), String> {
    let db = open_db_at(data_dir)?;
    service::clear_config(&db).map_err(|err| crate::error_text::error_text(&err))
}

/// 打开窗口时读自动同步设置；库文件不存在就用核心的默认值，不为了读设置新建库。
/// 本机保存的并发保护检测结果；没有或读不出来都是 `None`（界面提示「还没有检测」）。
fn load_concurrency_at(data_dir: &Path) -> Option<ConcurrencyRecord> {
    if !data_dir.join("zssh.db").is_file() {
        return None;
    }
    let db = open_existing_db_at(data_dir).ok()?;
    service::load_concurrency(&db).ok().flatten()
}

fn load_auto_at(data_dir: &Path) -> Result<AutoSettings, String> {
    if !data_dir.join("zssh.db").is_file() {
        return Ok(AutoSettings::default());
    }
    let db = open_existing_db_at(data_dir)?;
    service::load_auto(&db).map_err(|err| crate::error_text::error_text(&err))
}

/// 保存自动同步设置（开关或间隔）：显式操作，库不存在就建一个。
fn save_auto_at(data_dir: &Path, settings: &AutoSettings) -> Result<(), String> {
    let db = open_db_at(data_dir)?;
    service::save_auto(&db, settings).map_err(|err| crate::error_text::error_text(&err))
}

/// 读上次同步结果；库文件不存在就当没有，不为了读它新建库。
fn load_last_result_at(data_dir: &Path) -> Result<Option<LastResult>, String> {
    if !data_dir.join("zssh.db").is_file() {
        return Ok(None);
    }
    let db = open_existing_db_at(data_dir)?;
    service::load_last_result(&db).map_err(|err| crate::error_text::error_text(&err))
}

/// 写上次同步结果（手动同步结束时用；自动同步由调度器在后台写）。
fn save_last_result_at(data_dir: &Path, result: &LastResult) -> Result<(), String> {
    let db = open_db_at(data_dir)?;
    service::save_last_result(&db, result).map_err(|err| crate::error_text::error_text(&err))
}

/// 打开窗口时读本机记住的主密码；库文件不存在就当没记住，不为了读它新建库。
fn load_master_password_at(data_dir: &Path) -> Result<Option<String>, String> {
    if !data_dir.join("zssh.db").is_file() {
        return Ok(None);
    }
    let db = open_existing_db_at(data_dir)?;
    service::load_master_password(&db, &DpapiSealer::new(db.vault_account().to_owned()))
        .map(|password| password.map(|password| password.as_str().to_owned()))
        .map_err(|err| crate::error_text::error_text(&err))
}

/// 同步成功后记住主密码：交给核心用本机 DPAPI 加密，界面和日志都拿不到密文。
fn save_master_password_at(data_dir: &Path, password: &str) -> Result<(), String> {
    let db = open_db_at(data_dir)?;
    service::save_master_password(&db, &DpapiSealer::new(db.vault_account().to_owned()), password)
        .map_err(|err| crate::error_text::error_text(&err))
}

/// 开关关掉或者清除存储时删掉本机保存的主密码。
fn clear_master_password_at(data_dir: &Path) -> Result<(), String> {
    let db = open_db_at(data_dir)?;
    service::clear_master_password(&db).map_err(|err| crate::error_text::error_text(&err))
}

/// 退出 OneDrive：删本机令牌和连接设置（云端同步包不动）。「清除」清的是 OneDrive 时
/// 也走这里，免得本机留下一份没人用的刷新令牌。
fn disconnect_onedrive_at(data_dir: &Path) -> Result<(), String> {
    let db = open_db_at(data_dir)?;
    service::onedrive_disconnect(&db).map_err(|err| crate::error_text::error_text(&err))
}

/// 后台线程自己开库；不复用界面里的连接，免得跨线程持有 SQLite 连接。
fn open_db_at(data_dir: &Path) -> Result<Database, String> {
    std::fs::create_dir_all(data_dir)
        .map_err(|err| format!("{}{err}", t(SyncMsg::CreateDataDirFailedPrefix)))?;
    Database::open(&data_dir.join("zssh.db")).map_err(|err| {
        format!(
            "{}{}",
            t(SyncMsg::OpenDatabaseFailedPrefix),
            crate::error_text::error_text(&err)
        )
    })
}

/// OneDrive 的服务地址：生产是微软的；debug 构建里验收脚本可以换成本机的桩（不连真实服务）。
pub(crate) fn onedrive_endpoints() -> Endpoints {
    #[cfg(debug_assertions)]
    if let Some(stub) = crate::sync_stub::onedrive_endpoints() {
        return stub;
    }
    Endpoints::microsoft()
}

/// 打开授权页的方式：生产是系统默认浏览器；debug 构建里验收脚本可以换成不开浏览器的桩。
pub(crate) fn onedrive_opener() -> Box<dyn BrowserOpener> {
    #[cfg(debug_assertions)]
    if let Some(opener) = crate::sync_stub::onedrive_opener() {
        return opener;
    }
    Box::new(SystemBrowser)
}

/// 在阻塞线程里跑一次 OneDrive 登录：浏览器授权、换令牌、存本机都交给核心。
/// client ID 和取消开关只进参数；令牌不经过界面，也不写日志。
fn sign_in_onedrive_at(
    handle: &tokio::runtime::Handle,
    data_dir: &Path,
    client_id: &str,
    cancel: &Notify,
) -> Result<SignedIn, String> {
    let db = open_db_at(data_dir)?;
    let sealer = DpapiSealer::new(db.vault_account().to_owned());
    let endpoints = onedrive_endpoints();
    let browser = onedrive_opener();
    handle
        .block_on(service::onedrive_sign_in(
            &db,
            &sealer,
            &endpoints,
            client_id,
            ONEDRIVE_LOGIN_TIMEOUT,
            browser.as_ref(),
            cancel,
        ))
        .map_err(|err| crate::error_text::error_text(&err))
}

/// 在阻塞线程里用进程级 Tokio runtime 跑一次连接测试；代理取默认代理设置。
fn probe_connection_at(
    handle: &tokio::runtime::Handle,
    data_dir: &Path,
    config: &StorageConfig,
) -> Result<ConnectionProbe, String> {
    let db = open_db_at(data_dir)?;
    let sealer = DpapiSealer::new(db.vault_account().to_owned());
    probe_connection_with(handle, &db, &sealer, config)
}

/// 同 [`probe_connection_at`]，库和加密实现由调用方给，测试用它绕开 DPAPI。
fn probe_connection_with(
    handle: &tokio::runtime::Handle,
    db: &Database,
    sealer: &dyn SecretSealer,
    config: &StorageConfig,
) -> Result<ConnectionProbe, String> {
    let proxy = service::default_proxy(db).map_err(|err| crate::error_text::error_text(&err))?;
    let probe = handle.block_on(async {
        let tokens: Arc<dyn TokenStore + '_> = Arc::new(DbTokenStore::new(db, sealer));
        service::test_connection(config, proxy.as_ref(), tokens, &onedrive_endpoints()).await
    });
    save_probed_concurrency(db, &probe)?;
    Ok(probe)
}

/// 把探测到的并发保护能力存下来，之后的同步按它走（条件写入 / 写前再读）。
/// 没存的话 `effective_mode` 一律退回"写前再读"，界面上显示的"支持条件写入"就成了假话。
/// 读写都没通、没探测到时什么也不存。
fn save_probed_concurrency(db: &Database, probe: &ConnectionProbe) -> Result<(), String> {
    let Some(concurrency) = &probe.concurrency else {
        return Ok(());
    };
    service::save_concurrency(
        db,
        &ConcurrencyRecord {
            mode: concurrency.mode,
            notes: concurrency.notes.clone(),
            checked_at: now_secs(),
        },
    )
    .map_err(|err| {
        format!(
            "{}{}",
            t(SyncMsg::SaveConcurrencyFailedPrefix),
            crate::error_text::error_text(&err)
        )
    })
}

/// 在阻塞线程里跑一次真正的同步；界面线程不开库、不碰网络。
/// 主密码只进参数，不写日志。
fn run_sync_at(
    handle: &tokio::runtime::Handle,
    data_dir: &Path,
    password: &str,
    decision: Option<DeletionDecision>,
) -> Result<SyncReport, SyncFailure> {
    let db = open_db_at(data_dir)?;
    let sealer = DpapiSealer::new(db.vault_account().to_owned());
    run_sync_with(handle, data_dir, &sealer, password, decision)
}

/// 同 [`run_sync_at`]，加密实现由调用方给，测试用它绕开 DPAPI。
fn run_sync_with(
    handle: &tokio::runtime::Handle,
    data_dir: &Path,
    sealer: &dyn SecretSealer,
    password: &str,
    decision: Option<DeletionDecision>,
) -> Result<SyncReport, SyncFailure> {
    let result = run_sync_inner(handle, data_dir, sealer, password, decision);
    result.map_err(|failure| finish_failure(failure, data_dir, sealer))
}

fn run_sync_inner(
    handle: &tokio::runtime::Handle,
    data_dir: &Path,
    sealer: &dyn SecretSealer,
    password: &str,
    decision: Option<DeletionDecision>,
) -> Result<SyncReport, SyncFailure> {
    let db = open_db_at(data_dir)?;
    // GitHub 的 Gist ID 允许留空：第一次同步时新建，之后再同步都用这个。
    ensure_github_gist(handle, &db, sealer)?;
    let device_id = db.sync_device_id().map_err(|err| {
        format!(
            "{}{}",
            t(SyncMsg::ReadDeviceIdFailedPrefix),
            crate::error_text::error_text(&err)
        )
    })?;
    let context = SyncContext {
        db: &db,
        sealer,
        device_id,
        data_dir: data_dir.to_path_buf(),
        known_hosts_path: Some(data_dir.join("known_hosts")),
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        onedrive_endpoints: onedrive_endpoints(),
    };
    handle
        .block_on(context.run_sync(password, decision))
        .map_err(|err| SyncFailure::from_sync_error(&err))
}

/// GitHub 第一次同步：设置里还没有 Gist ID 时先新建一个 secret Gist，把 ID 写回本机
/// 设置，后面的同步和下一次「立即同步」都用它。其他存储什么都不做。
fn ensure_github_gist(
    handle: &tokio::runtime::Handle,
    db: &Database,
    sealer: &dyn SecretSealer,
) -> Result<(), String> {
    let loaded =
        service::load_config(db, sealer).map_err(|err| crate::error_text::error_text(&err))?;
    let Some(StorageConfig::Github(mut config)) = loaded else {
        return Ok(());
    };
    if !config.gist_id.trim().is_empty() {
        return Ok(());
    }
    let proxy = service::default_proxy(db).map_err(|err| crate::error_text::error_text(&err))?;
    let id = handle
        .block_on(github::create_gist(&config, proxy.as_ref()))
        .map_err(|err| {
            format!(
                "{}{}",
                t(SyncMsg::CreateGistFailedPrefix),
                crate::error_text::error_text(&err)
            )
        })?;
    config.gist_id = id.clone();
    // 同步包只写这个 Gist；先存下来，中途失败也不会每点一次就多建一个。
    service::save_config(db, sealer, &StorageConfig::Github(config)).map_err(|err| {
        format!(
            "{}{id}{}{}",
            t(SyncMsg::SaveGistIdFailedPrefix),
            t(SyncMsg::SaveGistIdFailedSuffix),
            crate::error_text::error_text(&err)
        )
    })
}

/// 主密码长度按核心的 [`service::MIN_MASTER_PASSWORD_CHARS`] 校验；按字符数
/// 而不是字节数算，中文密码不会被当成「够长」。
fn validate_master_password(password: &str) -> Result<(), String> {
    let chars = password.chars().count();
    if chars < service::MIN_MASTER_PASSWORD_CHARS {
        return Err(format!(
            "{}{}{}{chars}{}",
            t(SyncMsg::MasterPasswordTooShortPrefix),
            service::MIN_MASTER_PASSWORD_CHARS,
            t(SyncMsg::MasterPasswordTooShortMiddle),
            t(SyncMsg::MasterPasswordTooShortSuffix)
        ));
    }
    Ok(())
}

/// 同步结果给界面的文本行：状态、摘要、应用统计、备份名和冲突摘要。
fn sync_report_lines(report: &SyncReport) -> Vec<String> {
    let mut lines = vec![
        format!(
            "{}{}",
            t(SyncMsg::StatusPrefix),
            sync_status_label(report.status)
        ),
        format!("{}{}", t(SyncMsg::SummaryPrefix), report.summary),
    ];
    if report.status == SyncStatus::NeedsConfirmation {
        lines.extend(deletion_lines(&report.deletion));
    }
    if let Some(applied) = report.applied.as_ref() {
        lines.push(applied_text(applied));
    }
    if let Some(backup) = report.backup.as_ref() {
        lines.push(format!("{}{}", t(SyncMsg::BackupPrefix), backup.name));
    }
    if !report.conflicts.is_empty() {
        lines.push(format!(
            "{}{}{}",
            t(SyncMsg::ConflictCountPrefix),
            report.conflicts.len(),
            t(SyncMsg::ConflictCountSuffix)
        ));
        for conflict in report.conflicts.iter().take(MAX_CONFLICT_LINES) {
            lines.push(conflict_line(conflict));
        }
        if report.conflicts.len() > MAX_CONFLICT_LINES {
            lines.push(format!(
                "{}{}{}",
                t(SyncMsg::MoreConflictsPrefix),
                report.conflicts.len() - MAX_CONFLICT_LINES,
                t(SyncMsg::MoreConflictsSuffix)
            ));
        }
    }
    lines
}

/// 防误删确认要让用户看得出删的是什么：最多 [`MAX_DELETION_NAMES`] 个名字，
/// 剩下的说"另有 N 条"；格式和 0.2.14 的对话框一致（`主机：名字`）。
fn deletion_lines(plan: &zssh_core::sync::guard::DeletionPlan) -> Vec<String> {
    if plan.samples.is_empty() {
        return Vec::new();
    }
    let shown = plan.samples.len().min(MAX_DELETION_NAMES);
    let mut lines = vec![t(SyncMsg::WillDelete).to_owned()];
    for sample in plan.samples.iter().take(shown) {
        lines.push(format!(
            "{}{}{}{}",
            t(SyncMsg::DeletionSamplePrefix),
            entity_label(&sample.entity),
            t(SyncMsg::DeletionSampleMiddle),
            sample.name
        ));
    }
    let more = plan.deleted_records.saturating_sub(shown);
    if more > 0 {
        lines.push(format!(
            "{}{more}{}",
            t(SyncMsg::MoreDeletionsPrefix),
            t(SyncMsg::MoreDeletionsSuffix)
        ));
    }
    lines
}

/// 手动同步前对主密码输入的校验：长度够；第一次设置时两遍要一致。
fn check_master_password_entry(password: &str, confirm: Option<&str>) -> Result<(), String> {
    validate_master_password(password)?;
    if let Some(confirm) = confirm
        && confirm != password
    {
        return Err(t(SyncMsg::MasterPasswordMismatch).to_owned());
    }
    Ok(())
}

/// 读"这台电脑已经和云端对上过"；库不存在或读不出来都按没对上处理。
fn load_ready_at(data_dir: &Path) -> bool {
    if !data_dir.join("zssh.db").is_file() {
        return false;
    }
    open_existing_db_at(data_dir)
        .ok()
        .and_then(|db| service::is_ready(&db).ok())
        .unwrap_or(false)
}

fn sync_status_label(status: SyncStatus) -> &'static str {
    match status {
        SyncStatus::Completed => t(SyncMsg::StatusCompleted),
        SyncStatus::LocalChanged => t(SyncMsg::StatusLocalChanged),
        SyncStatus::NeedsConfirmation => t(SyncMsg::StatusNeedsConfirmation),
        SyncStatus::Cancelled => t(SyncMsg::StatusCancelled),
    }
}

/// 一类记录的「新增/更新/删除」片段：`prefix` 后面接三个数字。
fn counts_fragment(prefix: &str, counts: &ChangeCounts) -> String {
    format!(
        "{prefix}{}{updated}{}{deleted}{}",
        counts.inserted,
        counts.updated,
        counts.deleted,
        updated = t(SyncMsg::AppliedUpdated),
        deleted = t(SyncMsg::AppliedDeleted),
    )
}

/// 各类记录的写入统计。核心的 `ApplyReport` 只有这几项；主机记忆和转发规则
/// 的条数只在恢复备份的 `RestoreReport.extras` 里，同步结果拿不到。
fn applied_text(applied: &ApplyReport) -> String {
    let mut text = counts_fragment(t(SyncMsg::AppliedPrefix), &applied.hosts);
    text.push_str(&counts_fragment(
        t(SyncMsg::AppliedIdentitiesPrefix),
        &applied.identities,
    ));
    text.push_str(&counts_fragment(
        t(SyncMsg::AppliedGroupsPrefix),
        &applied.groups,
    ));
    text.push_str(&counts_fragment(
        t(SyncMsg::AppliedProxiesPrefix),
        &applied.proxies,
    ));
    text.push_str(&format!(
        "{settings}{settings_written}{known_hosts}{known_hosts_added}\
         {conflicts}{conflicts_stored}{tail}",
        settings = t(SyncMsg::AppliedSettingsPrefix),
        settings_written = applied.settings_written,
        known_hosts = t(SyncMsg::AppliedKnownHostsPrefix),
        known_hosts_added = applied.known_hosts_added,
        conflicts = t(SyncMsg::AppliedConflictsPrefix),
        conflicts_stored = applied.conflicts_stored,
        tail = t(SyncMsg::AppliedSuffix),
    ));
    if applied.known_hosts_skipped > 0 {
        text.push_str(&format!(
            "{}{}{}",
            t(SyncMsg::SkippedHostKeysPrefix),
            applied.known_hosts_skipped,
            t(SyncMsg::SkippedHostKeysSuffix)
        ));
    }
    text
}

/// 一条冲突摘要：哪个实体、哪条记录、核心给的说明。
fn conflict_line(conflict: &Conflict) -> String {
    format!(
        "{}{} {}{}{}",
        t(SyncMsg::DeletionSamplePrefix),
        conflict.entity,
        conflict.key,
        t(SyncMsg::ConflictKeySeparator),
        conflict.message
    )
}

#[cfg(test)]
mod tests {
    // 不写 `use super::*`：父模块 glob 进来的 `gpui_kit::test` 会盖掉内置的
    // `#[test]` 属性宏，展开时直接撞递归上限。
    use zssh_core::store::Database;
    use zssh_core::sync::apply::{ApplyReport, ChangeCounts, apply_snapshot};
    use zssh_core::sync::backup::BackupInfo;
    use zssh_core::sync::conflicts::{self, ConflictView};
    use zssh_core::sync::engine::{SyncReport, SyncStatus};
    use zssh_core::sync::error::SyncError;
    use zssh_core::sync::github::{DEFAULT_API_BASE, GithubConfig};
    use zssh_core::sync::guard::DeletionPlan;
    use zssh_core::sync::merge::Conflict;
    use zssh_core::sync::onedrive::config::OneDriveConfig;
    use zssh_core::sync::package::SyncSnapshot;
    use zssh_core::sync::remote::{Check, ConcurrencyMode, ConcurrencyProbe, ConnectionProbe};
    use zssh_core::sync::s3::S3Config;
    use zssh_core::sync::sealer::SecretSealer;
    use zssh_core::sync::service::{self, AutoSettings, StorageConfig};
    use zssh_core::sync::webdav::WebDavConfig;

    use super::{
        AutoBackoff, AutoClock, ClearConfirm, ConflictPanel, GithubForm, LogoutConfirm,
        OneDriveForm, ProviderChoice, ProviderDrafts, RememberAction, RememberPassword, S3Form,
        SyncState, WebDavForm, auto_due, auto_pause_reason, build_github, build_onedrive, build_s3,
        build_webdav, clamp_interval, conflict_row, form_from_view, github_form_from_view,
        last_result_error, last_result_from_report, onedrive_account_label,
        onedrive_form_from_view, parse_interval, probe_text, remember_action, remember_footer,
        s3_form_from_view, sync_guard, sync_report_lines, sync_status_label,
        validate_master_password,
    };

    const PASSWORD: &str = "secret-password";

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
                return Err(SyncError::InvalidData(zssh_core::error::Reason::text(
                    "不是 TestSealer 的密文".to_owned(),
                )));
            }
            Ok(blob[2..].iter().map(|byte| byte ^ self.0).collect())
        }
    }

    fn base_form() -> WebDavForm {
        WebDavForm {
            url: "https://dav.example.com/remote.php/dav/files/me/".to_owned(),
            directory: "zssh".to_owned(),
            username: "alice".to_owned(),
            password: PASSWORD.to_owned(),
            allow_insecure_http: false,
        }
    }

    fn saved(password: &str) -> StorageConfig {
        StorageConfig::Webdav(WebDavConfig {
            url: "https://dav.example.com/remote.php/dav/files/me/".to_owned(),
            directory: "zssh".to_owned(),
            username: "alice".to_owned(),
            password: password.to_owned(),
            allow_insecure_http: false,
        })
    }

    fn webdav_config(config: &StorageConfig) -> &WebDavConfig {
        match config {
            StorageConfig::Webdav(config) => config,
            other => panic!("应为 WebDAV，实际是 {}", other.provider()),
        }
    }

    fn base_s3_form() -> S3Form {
        S3Form {
            endpoint: "https://s3.us-east-1.amazonaws.com".to_owned(),
            region: "us-east-1".to_owned(),
            bucket: "zssh-sync".to_owned(),
            access_key: "AKIAEXAMPLE".to_owned(),
            secret_key: "s3-secret-key".to_owned(),
            prefix: "zssh".to_owned(),
            path_style: true,
        }
    }

    fn saved_s3(secret: &str) -> StorageConfig {
        StorageConfig::S3(S3Config {
            endpoint: "https://s3.us-east-1.amazonaws.com".to_owned(),
            region: "us-east-1".to_owned(),
            bucket: "zssh-sync".to_owned(),
            access_key: "AKIAEXAMPLE".to_owned(),
            secret_key: secret.to_owned(),
            prefix: "zssh".to_owned(),
            path_style: true,
        })
    }

    fn s3_config(config: &StorageConfig) -> &S3Config {
        match config {
            StorageConfig::S3(config) => config,
            other => panic!("应为 S3，实际是 {}", other.provider()),
        }
    }

    const GIST_ID: &str = "0123456789abcdef0123456789abcdef";
    const GITHUB_TOKEN: &str = "ghp_example_token";

    fn base_github_form() -> GithubForm {
        GithubForm {
            api_base: "https://api.github.com".to_owned(),
            token: GITHUB_TOKEN.to_owned(),
            gist_id: GIST_ID.to_owned(),
            allow_insecure_http: false,
        }
    }

    fn saved_github(token: &str) -> StorageConfig {
        StorageConfig::Github(GithubConfig {
            api_base: String::new(),
            token: token.to_owned(),
            gist_id: GIST_ID.to_owned(),
            allow_insecure_http: false,
        })
    }

    fn github_config(config: &StorageConfig) -> &GithubConfig {
        match config {
            StorageConfig::Github(config) => config,
            other => panic!("应为 GitHub，实际是 {}", other.provider()),
        }
    }

    const ONEDRIVE_CLIENT_ID: &str = "11111111-2222-3333-4444-555555555555";
    const ONEDRIVE_ACCOUNT: &str = "Some Person";

    fn saved_onedrive(client_id: &str, account: &str) -> StorageConfig {
        StorageConfig::Onedrive(OneDriveConfig {
            client_id: client_id.to_owned(),
            account: account.to_owned(),
        })
    }

    fn onedrive_config(config: &StorageConfig) -> &OneDriveConfig {
        match config {
            StorageConfig::Onedrive(config) => config,
            other => panic!("应为 OneDrive，实际是 {}", other.provider()),
        }
    }

    #[test]
    fn form_maps_to_webdav_config() {
        let mut form = base_form();
        form.url = " https://dav.example.com/remote.php/dav/files/me/ ".to_owned();
        form.directory = " zssh/sync ".to_owned();
        form.username = " alice ".to_owned();

        let config = build_webdav(&form, None).expect("配置应有效");
        let webdav = webdav_config(&config);
        assert_eq!(
            webdav.url,
            "https://dav.example.com/remote.php/dav/files/me/"
        );
        assert_eq!(webdav.directory, "zssh/sync");
        assert_eq!(webdav.username, "alice");
        assert_eq!(webdav.password, PASSWORD);
        assert!(!webdav.allow_insecure_http);
    }

    #[test]
    fn empty_url_and_username_are_rejected() {
        let mut form = base_form();
        form.url = "  ".to_owned();
        assert!(
            build_webdav(&form, None)
                .expect_err("空地址不应通过")
                .contains("地址")
        );

        let mut form = base_form();
        form.username = " ".to_owned();
        assert!(
            build_webdav(&form, None)
                .expect_err("空用户名不应通过")
                .contains("用户名")
        );
    }

    #[test]
    fn empty_password_keeps_saved_secret_and_new_password_replaces_it() {
        let saved = saved("old-secret");

        let mut form = base_form();
        form.password.clear();
        let config = build_webdav(&form, Some(&saved)).expect("沿用旧密码应通过");
        assert_eq!(webdav_config(&config).password, "old-secret");

        form.password = "new-secret".to_owned();
        let config = build_webdav(&form, Some(&saved)).expect("新密码应通过");
        assert_eq!(webdav_config(&config).password, "new-secret");

        let config = build_webdav(&form, None).expect("没有旧配置时应通过");
        assert_eq!(webdav_config(&config).password, "new-secret");

        form.password.clear();
        let config = build_webdav(&form, None).expect("没有旧配置时密码为空");
        assert!(webdav_config(&config).password.is_empty());
    }

    #[test]
    fn plain_http_requires_the_toggle() {
        let mut form = base_form();
        form.url = "http://dav.example.com/remote.php/dav/files/me/".to_owned();

        let error = build_webdav(&form, None).expect_err("http 未确认时不应通过");
        assert!(error.contains("http://"));
        assert!(error.contains("明文"));
        assert!(error.contains("我了解密码会以明文发送"));

        form.allow_insecure_http = true;
        let config = build_webdav(&form, None).expect("确认后应通过");
        assert!(config.is_plain_http());
        assert!(webdav_config(&config).allow_insecure_http);
    }

    #[test]
    fn s3_form_maps_to_s3_config() {
        let mut form = base_s3_form();
        form.endpoint = " https://s3.us-east-1.amazonaws.com ".to_owned();
        form.region = " us-east-1 ".to_owned();
        form.bucket = " zssh-sync ".to_owned();
        form.access_key = " AKIAEXAMPLE ".to_owned();
        form.prefix = " zssh/sync ".to_owned();
        form.path_style = false;

        let config = build_s3(&form, None).expect("配置应有效");
        assert_eq!(config.provider(), "s3");
        let s3 = s3_config(&config);
        assert_eq!(s3.endpoint, "https://s3.us-east-1.amazonaws.com");
        assert_eq!(s3.region, "us-east-1");
        assert_eq!(s3.bucket, "zssh-sync");
        assert_eq!(s3.access_key, "AKIAEXAMPLE");
        assert_eq!(s3.secret_key, "s3-secret-key");
        assert_eq!(s3.prefix, "zssh/sync");
        assert!(!s3.path_style);
    }

    #[test]
    fn s3_endpoint_must_be_http_or_https() {
        let mut form = base_s3_form();
        for endpoint in ["ftp://s3.example.com", "s3.example.com", "https:///bucket"] {
            form.endpoint = endpoint.to_owned();
            let error = build_s3(&form, None).expect_err("非法地址不应通过");
            assert!(error.contains("Endpoint"), "{endpoint}: {error}");
        }

        // http 是允许的（自建 MinIO 常见）；S3 的 Secret 不走明文 URL。
        form.endpoint = "http://127.0.0.1:9000".to_owned();
        assert!(build_s3(&form, None).is_ok());
    }

    #[test]
    fn s3_requires_bucket_access_key_and_region() {
        let mut form = base_s3_form();
        form.bucket = " ".to_owned();
        assert!(
            build_s3(&form, None)
                .expect_err("空 Bucket 不应通过")
                .contains("Bucket")
        );

        let mut form = base_s3_form();
        form.access_key = " ".to_owned();
        assert!(
            build_s3(&form, None)
                .expect_err("空 Access Key 不应通过")
                .contains("Access Key")
        );

        let mut form = base_s3_form();
        form.region = " ".to_owned();
        assert!(
            build_s3(&form, None)
                .expect_err("空 Region 不应通过")
                .contains("Region")
        );
    }

    #[test]
    fn empty_s3_secret_keeps_saved_secret_and_new_secret_replaces_it() {
        let saved = saved_s3("old-secret");

        let mut form = base_s3_form();
        form.secret_key.clear();
        let config = build_s3(&form, Some(&saved)).expect("沿用旧 Secret 应通过");
        assert_eq!(s3_config(&config).secret_key, "old-secret");

        form.secret_key = "new-secret".to_owned();
        let config = build_s3(&form, Some(&saved)).expect("新 Secret 应通过");
        assert_eq!(s3_config(&config).secret_key, "new-secret");

        form.secret_key.clear();
        assert!(
            build_s3(&form, None)
                .expect_err("没有旧配置时空 Secret 不应通过")
                .contains("Secret Key")
        );
    }

    #[test]
    fn github_form_maps_to_github_config() {
        let mut form = base_github_form();
        form.api_base = " https://ghe.example.com/api/v3 ".to_owned();
        form.gist_id = " abc123 ".to_owned();

        let config = build_github(&form, None).expect("配置应有效");
        assert_eq!(config.provider(), "github");
        let github = github_config(&config);
        assert_eq!(github.api_base, "https://ghe.example.com/api/v3");
        assert_eq!(github.gist_id, "abc123");
        assert_eq!(github.token, GITHUB_TOKEN);
        assert!(!github.allow_insecure_http);
        assert!(!config.is_plain_http());
    }

    #[test]
    fn empty_github_api_base_means_the_default() {
        let mut form = base_github_form();
        form.api_base = "   ".to_owned();

        let config = build_github(&form, None).expect("留空 API 地址应通过");
        let github = github_config(&config);
        assert!(github.api_base.is_empty(), "留空保存，默认地址交给核心");
        assert_eq!(github.effective_api_base(), DEFAULT_API_BASE);
        // 设置页回填时给的是实际地址。
        assert_eq!(config.view().api_base, DEFAULT_API_BASE);
    }

    #[test]
    fn empty_github_token_keeps_saved_token_and_new_token_replaces_it() {
        let saved_config = saved_github("old-token");

        let mut form = base_github_form();
        form.token.clear();
        let config = build_github(&form, Some(&saved_config)).expect("沿用旧令牌应通过");
        assert_eq!(github_config(&config).token, "old-token");

        form.token = "new-token".to_owned();
        let config = build_github(&form, Some(&saved_config)).expect("新令牌应通过");
        assert_eq!(github_config(&config).token, "new-token");

        form.token.clear();
        assert!(
            build_github(&form, None)
                .expect_err("没有旧配置时空令牌不应通过")
                .contains("令牌")
        );
        // 保存的是别的存储时不能拿它的密码来顶令牌。
        assert!(build_github(&form, Some(&saved(PASSWORD))).is_err());
    }

    #[test]
    fn github_api_base_must_be_http_or_https() {
        let mut form = base_github_form();
        for api_base in [
            "ftp://api.github.com",
            "api.github.com",
            "https:///api/v3",
            "https://",
        ] {
            form.api_base = api_base.to_owned();
            let error = build_github(&form, None).expect_err("非法地址不应通过");
            assert!(error.contains("API 地址"), "{api_base}: {error}");
        }

        form.api_base = "http://ghe.example.com/api/v3".to_owned();
        let error = build_github(&form, None).expect_err("http 未确认时不应通过");
        assert!(error.contains("http://"), "{error}");
        assert!(error.contains("明文"), "{error}");
        assert!(error.contains("我了解令牌会以明文发送"), "{error}");

        form.allow_insecure_http = true;
        let config = build_github(&form, None).expect("确认后应通过");
        assert!(config.is_plain_http());
        assert!(github_config(&config).allow_insecure_http);

        form.api_base = "https://ghe.example.com/api/v3".to_owned();
        assert!(build_github(&form, None).is_ok());
    }

    #[test]
    fn switching_to_github_keeps_every_draft() {
        let mut drafts = ProviderDrafts {
            webdav: base_form(),
            s3: base_s3_form(),
            github: base_github_form(),
            ..Default::default()
        };
        let webdav_before = drafts.webdav.clone();
        let s3_before = drafts.s3.clone();
        let github_before = drafts.github.clone();

        drafts.select(ProviderChoice::Github);
        assert_eq!(drafts.provider, ProviderChoice::Github);
        assert_eq!(
            drafts.webdav, webdav_before,
            "切到 GitHub 不能丢 WebDAV 草稿"
        );
        assert_eq!(drafts.s3, s3_before, "切到 GitHub 不能丢 S3 草稿");
        assert_eq!(drafts.github, github_before);

        drafts.github.gist_id = "another-gist".to_owned();
        drafts.select(ProviderChoice::WebDav);
        assert_eq!(drafts.provider, ProviderChoice::WebDav);
        assert_eq!(drafts.webdav, webdav_before);
        assert_eq!(drafts.s3, s3_before);
        assert_eq!(
            drafts.github.gist_id, "another-gist",
            "切回 WebDAV 后 GitHub 的修改还在"
        );

        drafts.select(ProviderChoice::Github);
        assert_eq!(
            drafts
                .selected_config(None)
                .expect("GitHub 配置应有效")
                .provider(),
            "github"
        );
    }

    #[test]
    fn onedrive_form_from_view_reads_client_id_and_account_label() {
        let config = saved_onedrive(ONEDRIVE_CLIENT_ID, ONEDRIVE_ACCOUNT);
        let view = config.view();
        let form = onedrive_form_from_view(&view);
        assert_eq!(form.client_id, ONEDRIVE_CLIENT_ID);
        // 保存的是别的存储时，OneDrive 表单保持默认值。
        let webdav_view = saved(PASSWORD).view();
        assert_eq!(
            onedrive_form_from_view(&webdav_view),
            OneDriveForm::default()
        );
        assert_eq!(
            ProviderChoice::from_provider(&view.provider),
            Some(ProviderChoice::OneDrive)
        );

        // 「账号」一行：登录过显示账号，没登录（或账号为空）显示「未登录」。
        assert_eq!(onedrive_account_label(None), "未登录");
        assert_eq!(onedrive_account_label(Some(&saved(PASSWORD))), "未登录");
        assert_eq!(
            onedrive_account_label(Some(&saved_onedrive(ONEDRIVE_CLIENT_ID, ""))),
            "未登录"
        );
        assert_eq!(
            onedrive_account_label(Some(&config)),
            ONEDRIVE_ACCOUNT,
            "登录后显示 ConfigView.account"
        );
    }

    #[test]
    fn onedrive_test_and_sync_need_a_sign_in() {
        let drafts = ProviderDrafts {
            provider: ProviderChoice::OneDrive,
            onedrive: OneDriveForm {
                client_id: format!("  {ONEDRIVE_CLIENT_ID} "),
            },
            ..Default::default()
        };
        // 只有表单里的 client ID、没有登录：拼不出能用的连接。
        assert_eq!(
            drafts.selected_config(None).expect_err("没登录不能测试"),
            "请先登录 OneDrive"
        );
        assert_eq!(
            drafts
                .selected_config(Some(&saved(PASSWORD)))
                .expect_err("保存的是别的存储时也不能测 OneDrive"),
            "请先登录 OneDrive"
        );
        assert_eq!(
            sync_guard(ProviderChoice::OneDrive, None).expect_err("没登录不能同步"),
            "请先登录 OneDrive"
        );
        assert_eq!(
            sync_guard(ProviderChoice::OneDrive, Some(&saved(PASSWORD)))
                .expect_err("保存的是别的存储时也不能同步"),
            "请先登录 OneDrive"
        );

        // 登录之后直接用本机保存的那份（令牌对应的账号在核心里），表单不参与。
        let config = saved_onedrive(ONEDRIVE_CLIENT_ID, ONEDRIVE_ACCOUNT);
        assert_eq!(
            drafts.selected_config(Some(&config)).expect("登录后应可用"),
            config
        );
        assert!(sync_guard(ProviderChoice::OneDrive, Some(&config)).is_ok());
        assert_eq!(build_onedrive(Some(&config)).expect("登录后应可用"), config);
    }

    #[test]
    fn onedrive_logout_needs_two_steps() {
        let mut confirm = LogoutConfirm::default();
        assert!(!confirm.is_armed());
        assert!(confirm.take().is_none(), "第一次点击只进入确认状态");

        confirm.arm(());
        assert!(confirm.is_armed(), "按钮要显示「再次点击」");
        assert!(confirm.take().is_some(), "第二次点击才真的退出");
        assert!(!confirm.is_armed());
        assert!(confirm.take().is_none(), "确认状态只能用一次");

        confirm.arm(());
        confirm.disarm();
        assert!(confirm.take().is_none(), "取消（或切换存储）后要重新确认");
    }

    #[test]
    fn switching_to_onedrive_keeps_every_draft() {
        let mut drafts = ProviderDrafts {
            webdav: base_form(),
            s3: base_s3_form(),
            github: base_github_form(),
            onedrive: OneDriveForm {
                client_id: ONEDRIVE_CLIENT_ID.to_owned(),
            },
            ..Default::default()
        };
        let webdav_before = drafts.webdav.clone();
        let s3_before = drafts.s3.clone();
        let github_before = drafts.github.clone();
        let onedrive_before = drafts.onedrive.clone();

        drafts.select(ProviderChoice::OneDrive);
        assert_eq!(drafts.provider, ProviderChoice::OneDrive);
        assert_eq!(
            drafts.webdav, webdav_before,
            "切到 OneDrive 不能丢 WebDAV 草稿"
        );
        assert_eq!(drafts.s3, s3_before, "切到 OneDrive 不能丢 S3 草稿");
        assert_eq!(
            drafts.github, github_before,
            "切到 OneDrive 不能丢 Gist 草稿"
        );
        assert_eq!(drafts.onedrive, onedrive_before);

        drafts.onedrive.client_id = "  changed  ".to_owned();
        drafts.select(ProviderChoice::Github);
        assert_eq!(drafts.provider, ProviderChoice::Github);
        assert_eq!(drafts.webdav, webdav_before);
        assert_eq!(drafts.s3, s3_before);
        assert_eq!(
            drafts.onedrive.client_id, "  changed  ",
            "切走后 OneDrive 的修改还在"
        );

        drafts.select(ProviderChoice::OneDrive);
        assert_eq!(drafts.onedrive.client_id(), "changed");
    }

    #[test]
    fn onedrive_save_disconnect_round_trip() {
        let db = Database::open_in_memory().expect("内存库");
        let sealer = TestSealer(0x77);
        let config = saved_onedrive(ONEDRIVE_CLIENT_ID, ONEDRIVE_ACCOUNT);

        service::save_config(&db, &sealer, &config).expect("保存");
        let loaded = service::load_config(&db, &sealer)
            .expect("读取")
            .expect("应已保存");
        assert_eq!(loaded, config);
        assert_eq!(loaded.provider(), "onedrive");
        assert_eq!(onedrive_config(&loaded).account, ONEDRIVE_ACCOUNT);
        assert_eq!(
            onedrive_form_from_view(&loaded.view()).client_id,
            ONEDRIVE_CLIENT_ID
        );
        // 账号是用户的个人信息，不进 Debug 输出。
        assert!(!format!("{loaded:?}").contains(ONEDRIVE_ACCOUNT));

        // 「退出 OneDrive」和清 OneDrive 的「清除」都用这个核心接口。
        service::onedrive_disconnect(&db).expect("退出");
        assert!(service::load_config(&db, &sealer).expect("读取").is_none());
    }

    #[test]
    fn sync_guard_accepts_a_saved_github_config() {
        let github = saved_github("old-token");
        assert!(sync_guard(ProviderChoice::Github, Some(&github)).is_ok());

        let error = sync_guard(ProviderChoice::Github, Some(&saved(PASSWORD)))
            .expect_err("选中 GitHub 但保存的是 WebDAV 时应提示先保存");
        assert_eq!(error, "请先保存当前存储设置");
        let error = sync_guard(ProviderChoice::WebDav, Some(&github))
            .expect_err("选中 WebDAV 但保存的是 GitHub 时应提示先保存");
        assert_eq!(error, "请先保存当前存储设置");
        let error = sync_guard(ProviderChoice::Github, None).expect_err("没保存时不能同步");
        assert!(error.contains("GitHub Gist"), "{error}");
    }

    #[test]
    fn saved_github_populates_only_the_github_form() {
        let config = saved_github("old-token");
        let view = config.view();
        let github = github_form_from_view(&view);
        assert_eq!(github.api_base, DEFAULT_API_BASE);
        assert_eq!(github.gist_id, GIST_ID);
        assert!(github.token.is_empty(), "令牌不回填");
        assert!(!github.allow_insecure_http);
        // 保存的是 GitHub 时，另外两套草稿保持默认值。
        assert_eq!(form_from_view(&view), WebDavForm::default());
        assert_eq!(s3_form_from_view(&view), S3Form::default());

        let webdav_view = saved(PASSWORD).view();
        assert_eq!(github_form_from_view(&webdav_view), GithubForm::default());
    }

    #[test]
    fn github_save_load_and_clear_round_trip() {
        let db = Database::open_in_memory().expect("内存库");
        let sealer = TestSealer(0x29);
        let config = build_github(&base_github_form(), None).expect("配置应有效");

        service::save_config(&db, &sealer, &config).expect("保存");
        let loaded = service::load_config(&db, &sealer)
            .expect("读取")
            .expect("应已保存");
        assert_eq!(loaded, config);

        let view = loaded.view();
        assert_eq!(view.provider, "github");
        assert_eq!(view.gist_id, GIST_ID);
        assert!(view.has_secret);
        // `Debug` 不打印令牌，界面也拿不到。
        assert!(!format!("{loaded:?}").contains(GITHUB_TOKEN));
        assert!(github_form_from_view(&view).token.is_empty());

        service::clear_config(&db).expect("清除");
        assert!(service::load_config(&db, &sealer).expect("读取").is_none());
    }

    #[test]
    fn onedrive_form_keeps_only_the_client_id_and_trims_it() {
        let form = OneDriveForm {
            client_id: format!("  {ONEDRIVE_CLIENT_ID}  "),
        };
        assert_eq!(form.client_id(), ONEDRIVE_CLIENT_ID);
        // 空（或只有空白）= 用编进程序里的 client ID，交给核心判断有没有。
        assert_eq!(OneDriveForm::default().client_id(), "");
        assert_eq!(
            OneDriveForm {
                client_id: "   ".to_owned()
            }
            .client_id(),
            ""
        );
    }

    #[test]
    fn switching_provider_keeps_both_drafts() {
        let mut drafts = ProviderDrafts {
            webdav: base_form(),
            s3: base_s3_form(),
            ..Default::default()
        };
        let webdav_before = drafts.webdav.clone();
        let s3_before = drafts.s3.clone();

        drafts.select(ProviderChoice::S3);
        assert_eq!(drafts.provider, ProviderChoice::S3);
        assert_eq!(drafts.webdav, webdav_before, "切到 S3 不能丢 WebDAV 草稿");
        assert_eq!(drafts.s3, s3_before);

        drafts.s3.bucket = "another-bucket".to_owned();
        drafts.select(ProviderChoice::WebDav);
        assert_eq!(drafts.provider, ProviderChoice::WebDav);
        assert_eq!(drafts.webdav, webdav_before);
        assert_eq!(
            drafts.s3.bucket, "another-bucket",
            "切回 WebDAV 后 S3 的修改还在"
        );
    }

    #[test]
    fn selected_config_uses_the_selected_provider() {
        let mut drafts = ProviderDrafts {
            webdav: base_form(),
            s3: base_s3_form(),
            ..Default::default()
        };

        drafts.select(ProviderChoice::S3);
        assert_eq!(
            drafts
                .selected_config(None)
                .expect("S3 配置应有效")
                .provider(),
            "s3"
        );

        drafts.select(ProviderChoice::WebDav);
        assert_eq!(
            drafts
                .selected_config(None)
                .expect("WebDAV 配置应有效")
                .provider(),
            "webdav"
        );
    }

    #[test]
    fn sync_needs_the_selected_provider_to_match_saved() {
        let webdav = saved(PASSWORD);
        let s3 = saved_s3("old-secret");

        assert!(sync_guard(ProviderChoice::WebDav, Some(&webdav)).is_ok());
        assert!(sync_guard(ProviderChoice::S3, Some(&s3)).is_ok());

        let error = sync_guard(ProviderChoice::S3, Some(&webdav))
            .expect_err("选中 S3 但保存的是 WebDAV 时应提示先保存");
        assert_eq!(error, "请先保存当前存储设置");
        let error = sync_guard(ProviderChoice::WebDav, Some(&s3))
            .expect_err("选中 WebDAV 但保存的是 S3 时应提示先保存");
        assert_eq!(error, "请先保存当前存储设置");

        assert!(sync_guard(ProviderChoice::WebDav, None).is_err());
        assert!(sync_guard(ProviderChoice::S3, None).is_err());
    }

    #[test]
    fn saved_s3_populates_only_the_s3_form() {
        let config = saved_s3("old-secret");
        let view = config.view();
        let s3 = s3_form_from_view(&view);
        assert_eq!(s3.endpoint, "https://s3.us-east-1.amazonaws.com");
        assert_eq!(s3.region, "us-east-1");
        assert_eq!(s3.bucket, "zssh-sync");
        assert_eq!(s3.access_key, "AKIAEXAMPLE");
        assert_eq!(s3.prefix, "zssh");
        assert!(s3.path_style);
        assert!(s3.secret_key.is_empty(), "Secret 不回填");
        // 保存的是 S3 时，WebDAV 草稿保持默认值。
        assert_eq!(form_from_view(&view), WebDavForm::default());

        let webdav_saved = saved(PASSWORD);
        let webdav_view = webdav_saved.view();
        assert_eq!(form_from_view(&webdav_view).username, "alice");
        assert_eq!(s3_form_from_view(&webdav_view), S3Form::default());
    }

    #[test]
    fn s3_save_load_and_clear_round_trip() {
        let db = Database::open_in_memory().expect("内存库");
        let sealer = TestSealer(0x51);
        let config = build_s3(&base_s3_form(), None).expect("配置应有效");

        service::save_config(&db, &sealer, &config).expect("保存");
        let loaded = service::load_config(&db, &sealer)
            .expect("读取")
            .expect("应已保存");
        assert_eq!(loaded, config);

        let view = loaded.view();
        assert_eq!(view.provider, "s3");
        assert_eq!(view.bucket, "zssh-sync");
        assert!(view.has_secret);
        // `Debug` 不打印 Secret，界面也拿不到。
        assert!(!format!("{loaded:?}").contains("s3-secret-key"));
        assert!(s3_form_from_view(&view).secret_key.is_empty());

        service::clear_config(&db).expect("清除");
        assert!(service::load_config(&db, &sealer).expect("读取").is_none());
    }

    #[test]
    fn probe_text_lists_every_result() {
        let probe = ConnectionProbe {
            read: Check {
                ok: true,
                message: "已连接，云端已有同步数据".to_owned(),
            },
            write: Check {
                ok: true,
                message: "可以写入".to_owned(),
            },
            concurrency: Some(ConcurrencyProbe {
                mode: ConcurrencyMode::Conditional,
                notes: vec![
                    "服务器返回了 ETag".to_owned(),
                    "If-None-Match 被拒绝（正确）".to_owned(),
                ],
            }),
            has_existing_data: true,
        };

        let text = probe_text(&probe);
        assert!(text.contains("读取：成功"));
        assert!(text.contains("已连接，云端已有同步数据"));
        assert!(text.contains("写入：成功"));
        assert!(text.contains("可以写入"));
        assert!(text.contains("支持条件写入"));
        assert!(text.contains("服务器返回了 ETag"));
        assert!(text.contains("If-None-Match 被拒绝（正确）"));
        assert!(text.contains("云端：已有同步数据"));
    }

    #[test]
    fn probe_text_reports_failures_verbatim() {
        let probe = ConnectionProbe {
            read: Check {
                ok: false,
                message: "连接超时：timed out".to_owned(),
            },
            write: Check {
                ok: false,
                message: "读取失败，没有继续检查写权限".to_owned(),
            },
            concurrency: None,
            has_existing_data: false,
        };

        let text = probe_text(&probe);
        assert!(text.contains("读取：失败"));
        assert!(text.contains("连接超时：timed out"));
        assert!(text.contains("写入：失败"));
        assert!(text.contains("读取失败，没有继续检查写权限"));
        assert!(text.contains("并发保护：未检测"));
        assert!(text.contains("云端：还没有同步数据"));
        assert!(!text.contains("支持条件写入"));
    }

    #[test]
    fn read_before_write_mode_is_explained() {
        let probe = ConnectionProbe {
            read: Check {
                ok: true,
                message: "已连接".to_owned(),
            },
            write: Check {
                ok: true,
                message: "可以写入".to_owned(),
            },
            concurrency: Some(ConcurrencyProbe {
                mode: ConcurrencyMode::ReadBeforeWrite,
                notes: vec!["If-Match 被忽略".to_owned()],
            }),
            has_existing_data: false,
        };

        let text = probe_text(&probe);
        assert!(text.contains("不支持条件写入"));
        assert!(text.contains("写前再读"));
        assert!(text.contains("If-Match 被忽略"));
        assert!(text.contains("云端：还没有同步数据"));
    }

    #[test]
    fn clear_needs_two_steps() {
        let mut confirm = ClearConfirm::default();
        assert!(!confirm.is_armed());
        assert!(confirm.take().is_none(), "第一次点击只进入确认状态");

        confirm.arm(());
        assert!(confirm.is_armed());
        assert!(confirm.take().is_some(), "第二次点击才真的清除");
        assert!(!confirm.is_armed());
        assert!(confirm.take().is_none(), "确认状态只能用一次");

        confirm.arm(());
        confirm.disarm();
        assert!(confirm.take().is_none(), "取消后不能再确认");
    }

    #[test]
    fn save_load_and_clear_round_trip() {
        let db = Database::open_in_memory().expect("内存库");
        let sealer = TestSealer(0x37);
        let config = build_webdav(&base_form(), None).expect("配置应有效");

        service::save_config(&db, &sealer, &config).expect("保存");
        let loaded = service::load_config(&db, &sealer)
            .expect("读取")
            .expect("应已保存");
        assert_eq!(loaded, config);

        let view = loaded.view();
        assert_eq!(view.provider, "webdav");
        assert_eq!(view.url, "https://dav.example.com/remote.php/dav/files/me/");
        assert_eq!(view.username, "alice");
        assert!(view.has_secret);
        // `Debug` 不打印密码，界面也拿不到密码。
        assert!(!format!("{loaded:?}").contains(PASSWORD));
        assert!(form_from_view(&view).password.is_empty());

        service::clear_config(&db).expect("清除");
        assert!(service::load_config(&db, &sealer).expect("读取").is_none());
    }

    /// 各类记录用不同的数字，断言时能分清是哪一项。
    fn applied() -> ApplyReport {
        ApplyReport {
            groups: ChangeCounts {
                inserted: 10,
                updated: 11,
                deleted: 12,
            },
            hosts: ChangeCounts {
                inserted: 1,
                updated: 2,
                deleted: 3,
            },
            identities: ChangeCounts {
                inserted: 4,
                updated: 5,
                deleted: 6,
            },
            proxies: ChangeCounts {
                inserted: 7,
                updated: 8,
                deleted: 9,
            },
            settings_written: 4,
            known_hosts_added: 5,
            known_hosts_skipped: 0,
            conflicts_stored: 6,
        }
    }

    fn backup() -> BackupInfo {
        BackupInfo {
            path: "C:\\data\\sync-backups\\zssh-backup-20260101-010203.bin".to_owned(),
            name: "zssh-backup-20260101-010203.bin".to_owned(),
            size: 1536,
            stamp: "20260101-010203".to_owned(),
            sequence: 1,
        }
    }

    fn conflict(index: usize) -> Conflict {
        Conflict {
            entity: "host".to_owned(),
            key: format!("uid-{index}"),
            kept: "{}".to_owned(),
            other: "{}".to_owned(),
            message: format!("两台电脑都改过（{index}）"),
        }
    }

    fn report(status: SyncStatus, summary: &str, applied: Option<ApplyReport>) -> SyncReport {
        SyncReport {
            status,
            summary: summary.to_owned(),
            conflicts: Vec::new(),
            nulled_references: Vec::new(),
            deletion: DeletionPlan::default(),
            backup: None,
            applied,
            cloud_version: None,
            retries: 0,
        }
    }

    #[test]
    fn master_password_needs_at_least_eight_chars() {
        let error = validate_master_password("1234567").expect_err("7 个字符不应通过");
        assert!(error.contains('8'), "{error}");
        assert!(error.contains("至少"), "{error}");
        assert!(error.contains("现在 7 个"), "{error}");

        assert!(validate_master_password("12345678").is_ok());
        // 按字符数算：8 个汉字也是 8 个字符。
        assert!(validate_master_password("密码密码密码密码").is_ok());
        assert!(validate_master_password("").is_err());
    }

    #[test]
    fn remember_master_password_starts_off_and_turns_on_for_a_saved_password() {
        let off = RememberPassword::default();
        assert!(!off.toggled, "「记住主密码」默认关闭");
        assert!(!off.saved);
        assert_eq!(off.hint(), "不记住");

        let mut toggled = RememberPassword::default();
        toggled.toggle();
        assert!(toggled.toggled);
        assert!(!toggled.saved, "开关打开不等于已经写盘");
        assert_eq!(toggled.hint(), "同步成功后记住");
        toggled.toggle();
        assert!(!toggled.toggled);
        assert_eq!(toggled.hint(), "不记住");

        // 打开窗口时读到已保存的主密码：开关打开并显示「已记住」。
        let loaded = RememberPassword::from_saved(Some(PASSWORD));
        assert!(loaded.toggled);
        assert!(loaded.saved);
        assert_eq!(loaded.hint(), "已记住（本机加密）");

        // 没有保存、或者保存的是空串：开关保持关闭。
        assert!(!RememberPassword::from_saved(None).toggled);
        assert!(!RememberPassword::from_saved(Some("")).toggled);

        // 已经保存过、用户把开关关掉：要等下一次同步成功才删。
        let mut clearing = loaded;
        clearing.toggle();
        assert!(!clearing.toggled);
        assert_eq!(clearing.hint(), "同步成功后清除");
    }

    #[test]
    fn successful_sync_saves_or_clears_and_other_results_keep_the_state() {
        for status in [SyncStatus::Completed, SyncStatus::LocalChanged] {
            assert_eq!(
                remember_action(true, Some(status)),
                RememberAction::Save,
                "开关打开时同步成功要记住"
            );
            assert_eq!(
                remember_action(false, Some(status)),
                RememberAction::Clear,
                "开关关闭时同步成功要清掉本机保存"
            );
        }
        for status in [SyncStatus::NeedsConfirmation, SyncStatus::Cancelled] {
            assert_eq!(
                remember_action(true, Some(status)),
                RememberAction::Keep,
                "没真正同步完时不能写"
            );
            assert_eq!(remember_action(false, Some(status)), RememberAction::Keep);
        }
        // 同步失败没有状态：本机保存和界面都不动。
        assert_eq!(remember_action(true, None), RememberAction::Keep);
        assert_eq!(remember_action(false, None), RememberAction::Keep);

        // footer 文案：成功记住 / 成功清除；其他结果不覆盖原有状态。
        assert_eq!(remember_footer(RememberAction::Save), Some("已记住主密码"));
        assert_eq!(
            remember_footer(RememberAction::Clear),
            Some("已清除记住的主密码")
        );
        assert_eq!(remember_footer(RememberAction::Keep), None);
        assert_eq!(remember_footer(RememberAction::Save), Some("已记住主密码"));
        assert_eq!(
            remember_footer(RememberAction::Clear),
            Some("已清除记住的主密码")
        );
    }

    #[test]
    fn master_password_save_load_and_clear_round_trip() {
        let db = Database::open_in_memory().expect("内存库");
        let sealer = TestSealer(0x41);

        assert!(
            service::load_master_password(&db, &sealer)
                .expect("读取")
                .is_none(),
            "没保存过时应为空"
        );

        service::save_master_password(&db, &sealer, PASSWORD).expect("保存");
        let loaded = service::load_master_password(&db, &sealer)
            .expect("读取")
            .expect("应已记住");
        assert_eq!(loaded.as_str(), PASSWORD);
        // 另一台电脑的 sealer 解不开这份密文，界面也只把它当错误显示，不回显原文。
        assert!(
            service::load_master_password(&db, &TestSealer(0x42)).is_err(),
            "换一台电脑读不出来"
        );

        service::clear_master_password(&db).expect("清除");
        assert!(
            service::load_master_password(&db, &sealer)
                .expect("读取")
                .is_none(),
            "清除后应为空"
        );
    }

    #[test]
    fn completed_report_shows_status_summary_counts_and_backup() {
        let mut report = report(
            SyncStatus::Completed,
            "同步完成：12 条记录",
            Some(applied()),
        );
        report.backup = Some(backup());

        let text = sync_report_lines(&report).join("\n");
        assert!(text.starts_with("状态：完成"), "{text}");
        assert!(text.contains("摘要：同步完成：12 条记录"), "{text}");
        assert!(text.contains("主机 新增1/更新2/删除3"), "{text}");
        assert!(text.contains("凭据 新增4/更新5/删除6"), "{text}");
        assert!(text.contains("分组 新增10/更新11/删除12"), "{text}");
        assert!(text.contains("代理 新增7/更新8/删除9"), "{text}");
        assert!(text.contains("设置 4 项"), "{text}");
        assert!(text.contains("known_hosts 5 条"), "{text}");
        assert!(text.contains("冲突 6 条"), "{text}");
        assert!(
            text.contains("备份：zssh-backup-20260101-010203.bin"),
            "{text}"
        );
        assert!(!text.contains("冲突 0 项"), "没有冲突时不该多一行：{text}");
    }

    #[test]
    fn each_status_has_a_chinese_label() {
        let cases = [
            (SyncStatus::Completed, "完成"),
            (SyncStatus::LocalChanged, "本地有变化"),
            (SyncStatus::NeedsConfirmation, "需要确认"),
            (SyncStatus::Cancelled, "已取消"),
        ];
        for (status, label) in cases {
            assert_eq!(sync_status_label(status), label);
            let lines = sync_report_lines(&report(status, "摘要原文", None));
            assert_eq!(lines[0], format!("状态：{label}"));
            assert_eq!(lines[1], "摘要：摘要原文");
            assert!(!lines.iter().any(|line| line.starts_with("应用：")));
            assert!(!lines.iter().any(|line| line.starts_with("备份：")));
        }
    }

    #[test]
    fn report_lists_at_most_five_conflicts_and_counts_the_rest() {
        let mut report = report(SyncStatus::Completed, "同步完成", None);
        report.conflicts = (0..7).map(conflict).collect();

        let text = sync_report_lines(&report).join("\n");
        assert!(text.contains("冲突 7 项："), "{text}");
        for index in 0..5 {
            assert!(text.contains(&format!("uid-{index}")), "{text}");
        }
        assert!(!text.contains("uid-5"), "{text}");
        assert!(!text.contains("uid-6"), "{text}");
        assert!(text.contains("…还有 2 条"), "{text}");
    }

    #[test]
    fn deletion_confirmation_keeps_password_until_next_result() {
        let mut state = SyncState::default();
        assert!(!state.needs_confirmation());
        assert_eq!(state.pending_password(), None);

        state.finish(
            report(SyncStatus::NeedsConfirmation, "会删除 5 条记录", None),
            "master-secret".to_owned(),
        );
        assert!(state.needs_confirmation());
        assert_eq!(state.pending_password(), Some("master-secret"));
        assert!(state.report.is_some());

        // 「允许删除并继续」用同一个密码重放，成功后确认状态清掉。
        state.finish(
            report(SyncStatus::Completed, "同步完成", None),
            "master-secret".to_owned(),
        );
        assert!(!state.needs_confirmation());
        assert_eq!(state.pending_password(), None);
        assert!(state.report.is_some());
    }

    #[test]
    fn cancellation_and_errors_clear_confirmation() {
        let mut state = SyncState::default();
        state.finish(
            report(SyncStatus::NeedsConfirmation, "需要确认", None),
            "master-secret".to_owned(),
        );
        state.finish(
            report(SyncStatus::Cancelled, "已取消这次同步", None),
            "master-secret".to_owned(),
        );
        assert!(!state.needs_confirmation());
        assert_eq!(state.pending_password(), None);

        state.finish(
            report(SyncStatus::NeedsConfirmation, "需要确认", None),
            "master-secret".to_owned(),
        );
        state.fail("网络不可达".to_owned());
        assert!(!state.needs_confirmation());
        assert!(state.report.is_none());
        assert_eq!(state.error.as_deref(), Some("网络不可达"));

        // 新一轮开始也要清掉上一轮的结果和密码。
        state.finish(
            report(SyncStatus::NeedsConfirmation, "需要确认", None),
            "master-secret".to_owned(),
        );
        state.begin();
        assert!(!state.needs_confirmation());
        assert!(state.report.is_none());
        assert!(state.error.is_none());
    }

    /// 冲突行的测试样本：默认是“另一台电脑改的、本机也改过”。
    fn conflict_view() -> ConflictView {
        ConflictView {
            id: 7,
            at: 1_800_000_000,
            entity: "host".to_owned(),
            key: "uid-7".to_owned(),
            name: "web-1".to_owned(),
            kept_summary: "alice@10.0.0.5:22".to_owned(),
            kept_device: Some("aaaaaaaa-1111-2222-3333-444444444444".to_owned()),
            kept_is_local: false,
            other_summary: "bob@10.0.0.6:22".to_owned(),
            other_device: Some("bbbbbbbb-1111-2222-3333-444444444444".to_owned()),
            other_is_local: true,
            deletion: false,
            can_use_other: true,
            message: String::new(),
        }
    }

    #[test]
    fn conflict_row_shows_both_sides_and_which_computer_they_came_from() {
        let row = conflict_row(&conflict_view(), false);

        assert_eq!(row.title, "web-1");
        assert_eq!(row.entity, "主机");
        assert!(
            row.kept.contains("另一台电脑（aaaaaaaa）"),
            "保留那份要标出是哪台电脑：{}",
            row.kept
        );
        assert!(row.kept.contains("alice@10.0.0.5:22"), "{}", row.kept);
        assert!(row.kept.contains("的改动"), "{}", row.kept);
        assert!(row.other.contains("本机"), "{}", row.other);
        assert!(row.other.contains("bob@10.0.0.6:22"), "{}", row.other);
        assert!(!row.deletion, "普通冲突不能标成删除冲突");
        assert!(row.can_use_other);
        assert!(row.message.is_empty());
        assert!(!row.armed);
    }

    #[test]
    fn conflict_row_falls_back_to_key_and_supports_unknown_devices() {
        let mut view = conflict_view();
        view.name = "   ".to_owned();
        view.kept_device = None;
        let row = conflict_row(&view, false);
        assert_eq!(row.title, "uid-7");
        assert!(row.kept.contains("另一台电脑"), "{}", row.kept);
        assert!(
            !row.kept.contains('（'),
            "不知道设备时不该编一个 ID：{}",
            row.kept
        );
    }

    #[test]
    fn deletion_conflict_is_marked_and_shows_the_last_content() {
        let mut view = conflict_view();
        view.deletion = true;
        view.kept_summary = "已删除".to_owned();
        view.other_is_local = true;

        let row = conflict_row(&view, false);
        assert!(row.deletion);
        assert!(row.kept.contains("删除"), "{}", row.kept);
        assert!(
            row.other.contains("本机 最后的内容"),
            "删除冲突里另一份是最后的内容：{}",
            row.other
        );
        assert!(!row.other.contains("的改动"), "{}", row.other);
    }

    #[test]
    fn known_host_conflict_shows_the_reason_and_hides_use_other() {
        let mut view = conflict_view();
        view.entity = "known_host".to_owned();
        view.name = "h.example.internal:22".to_owned();
        view.kept_summary = "ssh-ed25519 公钥".to_owned();
        view.can_use_other = false;
        view.message = "主机指纹不能在这里改用另一份".to_owned();

        let row = conflict_row(&view, false);
        assert_eq!(row.entity, "主机指纹");
        assert!(!row.can_use_other, "主机指纹不能给出「改用另一份」");
        assert_eq!(row.message, "主机指纹不能在这里改用另一份");
        // 另一份的摘要仍要显示，用户才知道冲突的是哪把钥匙。
        assert!(row.other.contains("bob@10.0.0.6:22"), "{}", row.other);
    }

    #[test]
    fn deletion_still_allows_use_other_only_when_the_core_allows_it() {
        let mut view = conflict_view();
        view.deletion = true;
        view.can_use_other = true;
        assert!(conflict_row(&view, false).can_use_other, "允许恢复删除记录");

        view.can_use_other = false;
        assert!(
            !conflict_row(&view, false).can_use_other,
            "核心不允许时删除冲突也不能给按钮"
        );

        // 已经点过一次也仍然显示按钮（变成「确认改用」）。
        let mut view = conflict_view();
        view.can_use_other = true;
        let row = conflict_row(&view, true);
        assert!(row.can_use_other);
        assert!(row.armed);
    }

    #[test]
    fn use_other_needs_a_second_click_and_cancel_disarms() {
        let mut panel = ConflictPanel::default();
        assert!(!panel.confirm(7), "第一次点击只进入确认状态");
        assert!(panel.is_armed(7));

        assert!(!panel.confirm(8), "换一行要重新确认");
        assert!(!panel.is_armed(7), "换行后原来那行不再处于确认状态");
        assert!(panel.is_armed(8));

        assert!(panel.confirm(8), "第二次点击才真的执行");
        assert!(!panel.is_armed(8), "执行后确认状态清掉");
        assert!(!panel.confirm(8), "同一次确认只能用一次，下次还要重新点");

        panel.confirm(9);
        panel.disarm();
        assert!(!panel.is_armed(9));
        assert!(!panel.confirm(9), "取消后要重新确认");
    }

    #[test]
    fn in_memory_db_lists_and_dismisses_a_stored_conflict() {
        let db = Database::open_in_memory().expect("内存库");
        let sealer = TestSealer(0x63);
        let now = 1_800_000_000;
        let snapshot = SyncSnapshot::empty("device-a", now);
        let stored = Conflict {
            entity: "setting".to_owned(),
            key: "theme".to_owned(),
            kept: "\"dark\"".to_owned(),
            other: "\"light\"".to_owned(),
            message: "两台电脑都改过".to_owned(),
        };
        apply_snapshot(&db, &snapshot, &sealer, None, &[stored], now).expect("写入冲突");

        let views = conflicts::list(&db, &sealer, "device-a", now).expect("列出冲突");
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].name, "theme");
        assert!(!views[0].deletion);
        assert!(views[0].can_use_other);

        conflicts::dismiss(&db, views[0].id).expect("忽略");
        assert!(
            conflicts::list(&db, &sealer, "device-a", now)
                .expect("再列一次")
                .is_empty()
        );
    }

    // -----------------------------------------------------------------------
    // 自动同步

    #[test]
    fn auto_due_waits_for_the_interval_and_a_missing_record_is_due() {
        assert!(auto_due(None, 1_000, 900), "从没同步过时第一拍就跑");
        assert!(!auto_due(Some(1_000), 1_000, 900));
        assert!(!auto_due(Some(1_000), 1_899, 900), "差一秒都不算到点");
        assert!(auto_due(Some(1_000), 1_900, 900));
        assert!(auto_due(Some(1_000), 2_000, 900));
        assert!(!auto_due(Some(2_000), 1_000, 900), "时钟倒退不算到点");
    }

    #[test]
    fn auto_backoff_doubles_caps_at_four_times_and_resets() {
        let mut backoff = AutoBackoff::default();
        assert_eq!(backoff.multiplier(), 1);
        assert_eq!(backoff.interval_secs(15), 900);

        backoff.failed();
        assert_eq!(backoff.multiplier(), 2);
        assert_eq!(backoff.interval_secs(15), 1_800, "第一次失败退到 2x");

        backoff.failed();
        assert_eq!(backoff.multiplier(), 4);
        assert_eq!(backoff.interval_secs(15), 3_600, "第二次失败退到 4x");

        for _ in 0..10 {
            backoff.failed();
        }
        assert_eq!(backoff.multiplier(), 4, "最多退到 4x");
        assert_eq!(backoff.interval_secs(15), 3_600);

        backoff.reset();
        assert_eq!(backoff.multiplier(), 1);
        assert_eq!(backoff.interval_secs(15), 900);
    }

    #[test]
    fn manual_sync_resets_the_backoff_and_restarts_the_interval() {
        let mut clock = AutoClock::default();
        clock.started(1_000);
        clock.failed();
        clock.failed();
        assert!(
            !clock.due(None, 1_000 + 900, 15),
            "退到 4x 时 15 分钟还不能跑"
        );
        assert!(clock.due(None, 1_000 + 3_600, 15));

        // 手动同步：退避清零，间隔从手动同步这一刻重新算。
        clock.manual(5_000);
        assert!(!clock.due(None, 5_000 + 899, 15));
        assert!(clock.due(None, 5_000 + 900, 15), "手动同步后回到 1x");

        // 自动同步成功一次也清零。
        clock.started(9_000);
        clock.failed();
        clock.succeeded();
        assert!(clock.due(None, 9_000 + 900, 15));
    }

    #[test]
    fn auto_interval_is_clamped_to_one_to_1440_minutes() {
        assert_eq!(clamp_interval(0), 1);
        assert_eq!(clamp_interval(1), 1);
        assert_eq!(clamp_interval(720), 720);
        assert_eq!(clamp_interval(1_440), 1_440);
        assert_eq!(clamp_interval(1_441), 1_440);
        assert_eq!(clamp_interval(u32::MAX), 1_440);

        assert_eq!(parse_interval("30"), Some(30));
        assert_eq!(parse_interval(" 45 "), Some(45));
        assert_eq!(parse_interval("1"), Some(1));
        assert_eq!(parse_interval("1440"), Some(1_440));
        assert_eq!(parse_interval("0"), None);
        assert_eq!(parse_interval("1441"), None);
        assert_eq!(parse_interval("abc"), None);
        assert_eq!(parse_interval(""), None);
        assert_eq!(parse_interval("-5"), None);
    }

    #[test]
    fn auto_settings_round_trip_in_memory_and_clamp_on_save_and_load() {
        let db = Database::open_in_memory().expect("内存库");

        let settings = AutoSettings {
            enabled: false,
            interval_minutes: 30,
        };
        service::save_auto(&db, &settings).expect("保存");
        assert_eq!(service::load_auto(&db).expect("读取"), settings);

        // 核心保存和读取时都把间隔夹到 1–1440。
        service::save_auto(
            &db,
            &AutoSettings {
                enabled: true,
                interval_minutes: 0,
            },
        )
        .expect("保存");
        let loaded = service::load_auto(&db).expect("读取");
        assert!(loaded.enabled);
        assert_eq!(loaded.interval_minutes, 1);

        service::save_auto(
            &db,
            &AutoSettings {
                enabled: true,
                interval_minutes: 5_000,
            },
        )
        .expect("保存");
        assert_eq!(
            service::load_auto(&db).expect("读取").interval_minutes,
            1_440
        );
    }

    #[test]
    fn auto_pause_reasons() {
        assert_eq!(auto_pause_reason(false, true, true), Some("自动同步未开启"));
        assert_eq!(
            auto_pause_reason(true, true, false),
            Some("还没有保存同步存储")
        );
        assert_eq!(
            auto_pause_reason(true, false, true),
            Some("还没有记住主密码")
        );
        assert_eq!(
            auto_pause_reason(true, false, false),
            Some("还没有保存同步存储"),
            "没配存储时先说存储"
        );
        assert_eq!(auto_pause_reason(true, true, true), None);
    }

    #[test]
    fn last_results_keep_the_auto_flag() {
        let mut report = report(SyncStatus::NeedsConfirmation, "会删除 5 条记录", None);
        report.conflicts = vec![conflict(1), conflict(2)];
        report.cloud_version = Some("version-1".to_owned());

        let auto = last_result_from_report(&report, true, 42);
        assert_eq!(auto.status, "needs_confirmation");
        assert_eq!(auto.summary, "会删除 5 条记录");
        assert_eq!(auto.version.as_deref(), Some("version-1"));
        assert_eq!(auto.conflicts, 2);
        assert!(auto.auto);

        let manual = last_result_from_report(&report, false, 42);
        assert!(!manual.auto);

        let error = last_result_error("网络不可达", false, 7);
        assert_eq!(error.status, "error");
        assert_eq!(error.summary, "网络不可达");
        assert!(!error.auto);
        assert_eq!(error.at, 7);
        assert_eq!(error.conflicts, 0);
        assert!(error.version.is_none());
    }

    #[test]
    fn exec_mode_is_not_part_of_sync() {
        use zssh_core::store::settings::AI_EXEC_MODE_KEY;
        use zssh_core::sync::settings::SettingsPolicy;

        assert!(!SettingsPolicy::CURRENT.is_synced(AI_EXEC_MODE_KEY));
    }
}

/// V2-F5：同步保护、与恢复互斥、并发保护能力落库、首次主密码两遍、防误删名字。
/// 用进程内的 WebDAV 桩（只听本机回环端口）跑真实的探测 / 同步 / 自动同步路径。
#[cfg(test)]
mod protection_tests {
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::time::{Duration, Instant};

    use zssh_core::model::{AuthMethod, HostDraft, ProxyMode};
    use zssh_core::store::Database;
    use zssh_core::sync::engine::{SyncEngine, SyncStatus};
    use zssh_core::sync::error::SyncError;
    use zssh_core::sync::guard::{DeletedSample, DeletionPlan};
    use zssh_core::sync::remote::{Check, ConcurrencyMode, ConcurrencyProbe, ConnectionProbe};
    use zssh_core::sync::sealer::SecretSealer;
    use zssh_core::sync::service::{self, AutoSettings, StorageConfig};
    use zssh_core::sync::webdav::WebDavConfig;

    use super::{
        AutoClock, AutoPause, ExclusiveKind, FailureKind, SyncGuardState, SyncInFlight,
        SyncOutcome, WebDavForm, auto_block_reason, auto_tick_with, baseline_broken_with,
        build_webdav, check_master_password_entry, classify_failure, deletion_lines,
        exclusive_busy_message, load_ready_at, probe_connection_with, reset_baseline_at,
        run_sync_with, save_probed_concurrency, with_auto_clock, with_sync_guard,
    };

    const PASSWORD: &str = "protection-test-password";

    struct TestSealer(u8);

    impl SecretSealer for TestSealer {
        fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, SyncError> {
            let mut out = vec![b'T', self.0];
            out.extend(plaintext.iter().map(|byte| byte ^ self.0));
            Ok(out)
        }

        fn open(&self, blob: &[u8]) -> Result<Vec<u8>, SyncError> {
            if blob.len() < 2 || blob[0] != b'T' || blob[1] != self.0 {
                return Err(SyncError::InvalidData(zssh_core::error::Reason::text(
                    "不是 TestSealer 的密文".to_owned(),
                )));
            }
            Ok(blob[2..].iter().map(|byte| byte ^ self.0).collect())
        }
    }

    /// 同步权和保护状态是进程级的：碰它们的测试拿这把锁串行，进出都把状态清干净。
    struct Serial {
        _lock: MutexGuard<'static, ()>,
    }

    impl Serial {
        fn new() -> Self {
            let lock = super::exclusive_test_lock();
            with_sync_guard(|guard| *guard = SyncGuardState::default());
            // 间隔计时和退避也是进程级的，前一个测试留下的不能影响这一个。
            with_auto_clock(|clock| *clock = AutoClock::default());
            Self { _lock: lock }
        }
    }

    impl Drop for Serial {
        fn drop(&mut self) {
            with_sync_guard(|guard| *guard = SyncGuardState::default());
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zssh-sync-f5-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    fn host(name: &str) -> HostDraft {
        HostDraft {
            protocol: None,
            id: None,
            group_id: None,
            name: name.to_owned(),
            host: format!("{name}.example.invalid"),
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

    // -- 进程内 WebDAV 桩 ---------------------------------------------------

    #[derive(Default)]
    struct DavState {
        objects: HashMap<String, (Vec<u8>, u64)>,
        next_etag: u64,
        requests: usize,
        /// 对同步包的每次 PUT：（If-Match, If-None-Match）。
        package_puts: Vec<(Option<String>, Option<String>)>,
    }

    struct DavStub {
        base_url: String,
        state: Arc<Mutex<DavState>>,
    }

    impl DavStub {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("绑定回环端口");
            let port = listener.local_addr().expect("本地地址").port();
            let state = Arc::new(Mutex::new(DavState::default()));
            let shared = state.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let state = shared.clone();
                    std::thread::spawn(move || {
                        let _ = serve(stream, &state);
                    });
                }
            });
            Self {
                base_url: format!("http://127.0.0.1:{port}/dav"),
                state,
            }
        }

        fn config(&self) -> StorageConfig {
            StorageConfig::Webdav(WebDavConfig {
                url: self.base_url.clone(),
                directory: "zssh".to_owned(),
                username: "u".to_owned(),
                password: "p".to_owned(),
                allow_insecure_http: true,
            })
        }

        fn requests(&self) -> usize {
            self.state.lock().expect("桩状态").requests
        }

        fn package_puts(&self) -> Vec<(Option<String>, Option<String>)> {
            self.state.lock().expect("桩状态").package_puts.clone()
        }
    }

    /// 处理一个连接上的一个请求：GET / PUT / DELETE / MKCOL，PUT 和 DELETE 认 ETag 条件。
    fn serve(stream: TcpStream, state: &Mutex<DavState>) -> std::io::Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut request_line = String::new();
        reader.read_line(&mut request_line)?;
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_owned();
        let path = parts.next().unwrap_or_default().to_owned();
        let mut headers: HashMap<String, String> = HashMap::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
            }
        }
        let length = headers
            .get("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body)?;

        let mut state = state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.requests += 1;
        let if_match = headers.get("if-match").cloned();
        let if_none_match = headers.get("if-none-match").cloned();
        let etag_of = |n: u64| format!("\"{n}\"");
        let (status, extra, payload): (&str, String, Vec<u8>) = match method.as_str() {
            "MKCOL" => ("201 Created", String::new(), Vec::new()),
            "GET" => match state.objects.get(&path) {
                Some((bytes, n)) => (
                    "200 OK",
                    format!("ETag: {}\r\n", etag_of(*n)),
                    bytes.clone(),
                ),
                None => ("404 Not Found", String::new(), Vec::new()),
            },
            "PUT" => {
                if path.ends_with("zssh-sync.json") {
                    state
                        .package_puts
                        .push((if_match.clone(), if_none_match.clone()));
                }
                let current = state.objects.get(&path).map(|(_, n)| *n);
                let failed = match (&if_match, &if_none_match, current) {
                    (_, Some(_), Some(_)) => true,
                    (Some(wanted), _, current) => {
                        current.map(etag_of).as_deref() != Some(wanted.as_str())
                    }
                    _ => false,
                };
                if failed {
                    ("412 Precondition Failed", String::new(), Vec::new())
                } else {
                    state.next_etag += 1;
                    let n = state.next_etag;
                    state.objects.insert(path, (body, n));
                    (
                        "201 Created",
                        format!("ETag: {}\r\n", etag_of(n)),
                        Vec::new(),
                    )
                }
            }
            "DELETE" => {
                if state.objects.remove(&path).is_some() {
                    ("204 No Content", String::new(), Vec::new())
                } else {
                    ("404 Not Found", String::new(), Vec::new())
                }
            }
            _ => ("405 Method Not Allowed", String::new(), Vec::new()),
        };
        let mut stream = stream;
        write!(
            stream,
            "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        )?;
        stream.write_all(&payload)?;
        stream.flush()
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime")
    }

    /// 一个配好 WebDAV 桩、记住主密码、开了自动同步的数据目录。
    fn configured_dir(name: &str, stub: &DavStub, sealer: &TestSealer) -> (PathBuf, Database) {
        let dir = temp_dir(name);
        let db = Database::open(&dir.join("zssh.db")).expect("打开库");
        service::save_config(&db, sealer, &stub.config()).expect("保存存储设置");
        service::save_master_password(&db, sealer, PASSWORD).expect("记住主密码");
        service::save_auto(
            &db,
            &AutoSettings {
                enabled: true,
                interval_minutes: 1,
            },
        )
        .expect("开自动同步");
        db.host_save(&host("web")).expect("加主机");
        (dir, db)
    }

    fn corrupt_baseline(dir: &Path) {
        std::fs::write(dir.join("sync-baseline.bin"), b"not a sealed baseline").expect("写坏基线");
    }

    // -- D2-06 / D2-12：与恢复互斥、登记安装前检查 -----------------------------

    #[test]
    fn sync_and_restore_exclude_each_other_and_report_who_holds_the_lock() {
        let _serial = Serial::new();
        let sync = SyncInFlight::acquire(ExclusiveKind::Sync).expect("空闲时能抢到");
        // 同步进行中：不能恢复，也不能再开一个同步。
        let holder =
            SyncInFlight::acquire(ExclusiveKind::Restore).expect_err("同步进行中不允许恢复");
        assert_eq!(holder, ExclusiveKind::Sync);
        assert!(
            exclusive_busy_message(ExclusiveKind::Restore, holder).contains("同步正在进行"),
            "恢复被拒时要说清是同步占着"
        );
        assert_eq!(
            SyncInFlight::acquire(ExclusiveKind::Sync).err(),
            Some(ExclusiveKind::Sync)
        );
        // 安装前检查在同步期间被拦住。
        let reason = crate::install_state::blocking_reason().expect("同步中要拦安装");
        assert!(reason.contains("同步"), "{reason}");
        drop(sync);

        // 恢复进行中：不能同步（自动同步那一拍也抢不到）。
        let restore = SyncInFlight::acquire(ExclusiveKind::Restore).expect("放掉后能抢到");
        let holder = SyncInFlight::acquire(ExclusiveKind::Sync).expect_err("恢复中不允许同步");
        assert_eq!(holder, ExclusiveKind::Restore);
        assert!(exclusive_busy_message(ExclusiveKind::Sync, holder).contains("恢复"));
        let reason = crate::install_state::blocking_reason().expect("恢复中要拦安装");
        assert!(reason.contains("备份恢复"), "{reason}");
        drop(restore);
        assert!(
            SyncInFlight::acquire(ExclusiveKind::Sync).is_ok(),
            "放掉后又空闲"
        );
    }

    // -- D2-07：探测到的并发保护能力要落库，同步时使用 -------------------------

    #[test]
    fn probed_conditional_mode_is_saved_and_used_by_the_next_sync() {
        let _serial = Serial::new();
        let stub = DavStub::start();
        let sealer = TestSealer(0x21);
        let (dir, db) = configured_dir("probe-save", &stub, &sealer);
        let runtime = runtime();
        let handle = runtime.handle().clone();

        assert_eq!(
            service::effective_mode(&db).expect("读取模式"),
            ConcurrencyMode::ReadBeforeWrite,
            "没测试过连接时按不支持处理"
        );
        let probe = probe_connection_with(&handle, &db, &sealer, &stub.config()).expect("测试连接");
        assert_eq!(
            probe.concurrency.as_ref().map(|c| c.mode),
            Some(ConcurrencyMode::Conditional),
            "桩支持 ETag 和条件写入: {probe:?}"
        );
        assert_eq!(
            service::effective_mode(&db).expect("读取模式"),
            ConcurrencyMode::Conditional,
            "探测结果要保存下来"
        );
        let record = service::load_concurrency(&db)
            .expect("读取记录")
            .expect("应有记录");
        assert!(!record.notes.is_empty());

        // 第一次同步（云端为空）用 If-None-Match，第二次更新用 If-Match：条件写入真的在用。
        let report = run_sync_with(&handle, &dir, &sealer, PASSWORD, None).expect("首次同步");
        assert_eq!(report.status, SyncStatus::Completed);
        db.host_save(&host("db")).expect("再加一台");
        let report = run_sync_with(&handle, &dir, &sealer, PASSWORD, None).expect("第二次同步");
        assert_eq!(report.status, SyncStatus::Completed);
        let puts = stub.package_puts();
        assert_eq!(puts.len(), 2, "{puts:?}");
        assert_eq!(
            puts[0].1.as_deref(),
            Some("*"),
            "首次上传用 If-None-Match: *"
        );
        assert!(puts[1].0.is_some(), "更新要带 If-Match: {puts:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_without_concurrency_saves_nothing() {
        let db = Database::open_in_memory().expect("内存库");
        let failed = ConnectionProbe {
            read: Check {
                ok: false,
                message: "连不上".to_owned(),
            },
            write: Check {
                ok: false,
                message: "没有继续检查写权限".to_owned(),
            },
            concurrency: None,
            has_existing_data: false,
        };
        save_probed_concurrency(&db, &failed).expect("不应报错");
        assert!(service::load_concurrency(&db).expect("读取").is_none());

        let ok = ConnectionProbe {
            concurrency: Some(ConcurrencyProbe {
                mode: ConcurrencyMode::Conditional,
                notes: vec!["服务器返回了 ETag".to_owned()],
            }),
            ..failed
        };
        save_probed_concurrency(&db, &ok).expect("保存");
        let record = service::load_concurrency(&db)
            .expect("读取")
            .expect("有记录");
        assert_eq!(record.mode, ConcurrencyMode::Conditional);
        assert_eq!(record.notes, vec!["服务器返回了 ETag".to_owned()]);
    }

    // -- D2-08：自动同步保护 --------------------------------------------------

    #[test]
    fn failures_are_classified_like_the_stable_controller() {
        assert_eq!(
            classify_failure(&SyncError::RateLimited {
                service: "GitHub".to_owned(),
                retry_after_secs: 90,
            }),
            FailureKind::RateLimited {
                service: "GitHub".to_owned(),
                retry_after_secs: 90,
            }
        );
        assert_eq!(
            classify_failure(&SyncError::AuthFailed(zssh_core::error::Reason::text(
                "令牌过期".to_owned()
            ))),
            FailureKind::AuthFailed("令牌过期".to_owned())
        );
        assert_eq!(
            classify_failure(&SyncError::WrongPassword),
            FailureKind::WrongPassword
        );
        assert_eq!(
            classify_failure(&SyncError::Format(zssh_core::error::Reason::text(
                "同步基线损坏：EOF".to_owned()
            ))),
            FailureKind::BaselineBroken
        );
        assert_eq!(
            classify_failure(&SyncError::Format(zssh_core::error::Reason::text(
                "同步包格式不对".to_owned()
            ))),
            FailureKind::Other
        );
        assert_eq!(
            classify_failure(&SyncError::Storage(zssh_core::error::Reason::text(
                "超时".to_owned()
            ))),
            FailureKind::Other
        );
    }

    #[test]
    fn rate_limit_blocks_until_it_expires_and_a_completed_sync_clears_it() {
        let start = Instant::now();
        let mut guard = SyncGuardState::default();
        assert_eq!(guard.block_reason(start), None);

        guard.note(
            &SyncOutcome::Failed(FailureKind::RateLimited {
                service: "GitHub".to_owned(),
                retry_after_secs: 120,
            }),
            start,
        );
        let reason = guard
            .block_reason(start + Duration::from_secs(10))
            .expect("限流期内要停");
        assert!(
            reason.contains("GitHub") && reason.contains("限流"),
            "{reason}"
        );
        let (service, left) = guard
            .rate_limit_remaining(start + Duration::from_secs(10))
            .expect("还在等");
        assert_eq!((service.as_str(), left), ("GitHub", 110));
        // 过期后放行，并且把过期的记录清掉。
        assert_eq!(guard.block_reason(start + Duration::from_secs(121)), None);
        assert!(guard.rate_limit.is_none());

        // 没到期但同步成功了（比如换了令牌）：也放行。
        guard.note(
            &SyncOutcome::Failed(FailureKind::RateLimited {
                service: "GitHub".to_owned(),
                retry_after_secs: 120,
            }),
            start,
        );
        guard.note(&SyncOutcome::Report(SyncStatus::Completed), start);
        assert_eq!(guard.block_reason(start), None);
    }

    #[test]
    fn rejected_credentials_and_wrong_master_password_pause_until_the_user_acts() {
        let now = Instant::now();
        let mut guard = SyncGuardState::default();
        guard.note(
            &SyncOutcome::Failed(FailureKind::AuthFailed("401".to_owned())),
            now,
        );
        assert_eq!(guard.pause, Some(AutoPause::Auth("401".to_owned())));
        assert!(
            guard
                .block_reason(now)
                .expect("凭据被拒要停")
                .contains("401")
        );
        // 时间过去多久都不会自己恢复。
        assert!(
            guard
                .block_reason(now + Duration::from_secs(86_400))
                .is_some()
        );
        // 用户亲自再点一次同步：先放开再试。
        guard.manual_retry();
        assert_eq!(guard.block_reason(now), None);

        guard.note(&SyncOutcome::Failed(FailureKind::WrongPassword), now);
        assert!(
            guard
                .block_reason(now)
                .expect("主密码错要停")
                .contains("主密码")
        );
        // 换了存储设置也算处理过。
        guard.config_changed();
        assert_eq!(guard.block_reason(now), None);

        // 普通失败（网络）走退避，不在这里暂停。
        guard.note(&SyncOutcome::Failed(FailureKind::Other), now);
        assert_eq!(guard.block_reason(now), None);
    }

    #[test]
    fn a_pending_deletion_confirmation_stops_auto_sync_until_resolved() {
        let now = Instant::now();
        let mut guard = SyncGuardState::default();
        guard.note(&SyncOutcome::Report(SyncStatus::NeedsConfirmation), now);
        assert!(
            guard
                .block_reason(now)
                .expect("待确认要停")
                .contains("确认")
        );
        // 取消 / 完成之后才放行。
        guard.note(&SyncOutcome::Report(SyncStatus::Cancelled), now);
        assert_eq!(guard.block_reason(now), None);
        guard.note(&SyncOutcome::Report(SyncStatus::NeedsConfirmation), now);
        guard.note(&SyncOutcome::Report(SyncStatus::Completed), now);
        assert_eq!(guard.block_reason(now), None);
    }

    #[test]
    fn auto_block_reason_orders_the_gates() {
        let go = |ready, broken, guard: Option<&str>| {
            auto_block_reason(true, true, true, ready, broken, guard.map(str::to_owned))
        };
        assert_eq!(go(true, false, None), None);
        assert_eq!(
            go(false, false, None).as_deref(),
            Some("还没有成功同步过，先手动同步一次")
        );
        assert_eq!(
            go(true, true, None).as_deref(),
            Some("同步基线损坏，请先重建基线")
        );
        assert_eq!(go(true, false, Some("限流")).as_deref(), Some("限流"));
        // 本机配置不全时先报配置的原因。
        assert_eq!(
            auto_block_reason(true, false, true, true, false, None).as_deref(),
            Some("还没有记住主密码")
        );
        assert_eq!(
            auto_block_reason(false, true, true, true, false, None).as_deref(),
            Some("自动同步未开启")
        );
    }

    #[test]
    fn auto_tick_only_talks_to_the_server_when_every_gate_is_open() {
        let _serial = Serial::new();
        let stub = DavStub::start();
        let sealer = TestSealer(0x22);
        let (dir, db) = configured_dir("auto-gates", &stub, &sealer);
        let runtime = runtime();
        let handle = runtime.handle().clone();
        // 每一拍用递增的"现在"，免得内存里的间隔计时把后面的拍挡掉而冒充通过。
        let mut now = 1_000_000_000i64;
        let mut tick = || {
            now += 100_000;
            auto_tick_with(&handle, &dir, now, &sealer)
        };

        // 门 1：还没和云端对上过（ready=false）→ 不跑，也不碰服务器。
        assert!(!load_ready_at_with(&db));
        assert!(tick().is_none(), "没 ready 不能自动同步");
        assert_eq!(stub.requests(), 0);

        // 门 2：基线损坏 → 不跑。
        service::set_ready(&db, true).expect("标记已对上");
        corrupt_baseline(&dir);
        assert!(baseline_broken_with(&db, &sealer, &dir));
        assert!(tick().is_none(), "基线损坏不能自动同步");
        assert_eq!(stub.requests(), 0);
        std::fs::remove_file(dir.join("sync-baseline.bin")).expect("删坏基线");

        // 门 3：限流 / 凭据被拒 / 主密码错 / 待确认 → 不跑。
        let now_i = Instant::now();
        for outcome in [
            SyncOutcome::Failed(FailureKind::RateLimited {
                service: "测试服务".to_owned(),
                retry_after_secs: 600,
            }),
            SyncOutcome::Failed(FailureKind::AuthFailed("拒绝".to_owned())),
            SyncOutcome::Failed(FailureKind::WrongPassword),
            SyncOutcome::Report(SyncStatus::NeedsConfirmation),
        ] {
            with_sync_guard(|guard| {
                *guard = SyncGuardState::default();
                guard.note(&outcome, now_i);
            });
            assert!(tick().is_none(), "{outcome:?} 时不能自动同步");
            assert_eq!(stub.requests(), 0, "{outcome:?} 时不能碰服务器");
        }
        with_sync_guard(|guard| *guard = SyncGuardState::default());

        // 对照：全部放行时这一拍确实同步了（否则上面的"没碰服务器"可能只是桩没起来）。
        let result = tick().expect("全部放行应当跑一次自动同步");
        assert_eq!(result.status, "completed", "{result:?}");
        assert!(result.auto);
        assert!(stub.requests() > 0);
        assert!(!stub.package_puts().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn load_ready_at_with(db: &Database) -> bool {
        service::is_ready(db).unwrap_or(false)
    }

    #[test]
    fn wrong_master_password_pauses_the_scheduler_after_one_attempt() {
        let _serial = Serial::new();
        let stub = DavStub::start();
        let sealer = TestSealer(0x23);
        let (dir, db) = configured_dir("auto-auth", &stub, &sealer);
        service::set_ready(&db, true).expect("标记已对上");
        // 桩不验登录密码，所以用"云端是另一台电脑用别的主密码传上去的包"来触发
        // WrongPassword，再看调度器是不是只试一次就停。
        let other_password = "another-master-password";
        let runtime = runtime();
        let handle = runtime.handle().clone();
        let other_dir = temp_dir("auto-auth-other");
        {
            let other_db = Database::open(&other_dir.join("zssh.db")).expect("另一台电脑的库");
            service::save_config(&other_db, &sealer, &stub.config()).expect("同一个云端");
            other_db.host_save(&host("elsewhere")).expect("加主机");
        }
        let report = run_sync_with(&handle, &other_dir, &sealer, other_password, None)
            .expect("另一台电脑先把包传上去");
        assert_eq!(report.status, SyncStatus::Completed);

        let first = auto_tick_with(&handle, &dir, 2_000_000_000, &sealer)
            .expect("第一次会试一下，并因主密码不对失败");
        assert_eq!(first.status, "error");
        assert!(
            first.summary.contains("主密码"),
            "失败原因原样显示: {}",
            first.summary
        );
        let requests_after_first = stub.requests();
        assert!(requests_after_first > 0);

        // 之后无论过多久都不再打服务器，直到用户处理。
        for step in 1..=5 {
            let again = auto_tick_with(&handle, &dir, 2_000_000_000 + step * 1_000_000, &sealer);
            assert!(again.is_none(), "主密码错之后自动同步必须停下");
        }
        assert_eq!(
            stub.requests(),
            requests_after_first,
            "停下之后不能再有请求"
        );
        assert!(
            with_sync_guard(|guard| guard.block_reason(Instant::now()))
                .expect("界面要能看到暂停原因")
                .contains("主密码")
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other_dir);
    }

    // -- 基线损坏：识别、拦同步、重建 -----------------------------------------

    #[test]
    fn a_broken_baseline_is_flagged_blocks_sync_and_can_be_rebuilt() {
        let _serial = Serial::new();
        let stub = DavStub::start();
        let sealer = TestSealer(0x24);
        let (dir, db) = configured_dir("baseline", &stub, &sealer);
        let runtime = runtime();
        let handle = runtime.handle().clone();

        let report = run_sync_with(&handle, &dir, &sealer, PASSWORD, None).expect("首次同步");
        assert_eq!(report.status, SyncStatus::Completed);
        assert!(
            !baseline_broken_with(&db, &sealer, &dir),
            "正常基线读得出来"
        );

        corrupt_baseline(&dir);
        assert!(baseline_broken_with(&db, &sealer, &dir));
        let failure = run_sync_with(&handle, &dir, &sealer, PASSWORD, None)
            .expect_err("基线坏了同步必须失败，不能悄悄当成首次同步");
        assert!(failure.baseline_broken, "失败后要标出基线损坏: {failure:?}");

        // 重建走核心的 reset_baseline，并且和同步互斥。
        let held = SyncInFlight::acquire(ExclusiveKind::Sync).expect("占住同步权");
        let busy = reset_baseline_at(&dir).expect_err("同步进行中不能重建基线");
        assert!(busy.contains("同步正在进行"), "{busy}");
        drop(held);
        // reset_baseline_at 用生产 DPAPI 的实现，但重建只删文件、不加解密。
        reset_baseline_at(&dir).expect("重建基线");
        assert!(!baseline_broken_with(&db, &sealer, &dir));
        assert!(!dir.join("sync-baseline.bin").exists());
        SyncEngine::new(&db, &sealer, "x", &dir, None, None)
            .reset_baseline()
            .expect("没有基线时重建也不报错");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- D2-09：首次主密码两遍、防误删名字 --------------------------------------

    #[test]
    fn first_time_master_password_must_match_when_confirmation_is_required() {
        assert!(check_master_password_entry("long-enough-pw", None).is_ok());
        assert!(check_master_password_entry("long-enough-pw", Some("long-enough-pw")).is_ok());
        assert_eq!(
            check_master_password_entry("long-enough-pw", Some("long-enough-px")).unwrap_err(),
            "两次输入的主密码不一致"
        );
        assert_eq!(
            check_master_password_entry("long-enough-pw", Some("")).unwrap_err(),
            "两次输入的主密码不一致",
            "第二遍留空也算不一致"
        );
        // 长度先于一致性检查：太短的两遍都输对了也不行。
        assert!(
            check_master_password_entry("short", Some("short"))
                .unwrap_err()
                .contains("至少")
        );
    }

    #[test]
    fn ready_flag_is_read_from_the_local_settings() {
        let dir = temp_dir("ready-flag");
        assert!(!load_ready_at(&dir), "没有库按没对上处理");
        let db = Database::open(&dir.join("zssh.db")).expect("打开库");
        assert!(!load_ready_at(&dir));
        service::set_ready(&db, true).expect("标记");
        assert!(load_ready_at(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn plan(names: &[(&str, &str)], deleted_records: usize) -> DeletionPlan {
        DeletionPlan {
            local_records: 20,
            deleted_records,
            local_hosts: 10,
            deleted_hosts: names.iter().filter(|(entity, _)| *entity == "host").count(),
            samples: names
                .iter()
                .map(|(entity, name)| DeletedSample {
                    entity: (*entity).to_owned(),
                    name: (*name).to_owned(),
                })
                .collect(),
        }
    }

    #[test]
    fn deletion_confirmation_lists_the_names_up_to_five_and_counts_the_rest() {
        let few = deletion_lines(&plan(&[("host", "web-1"), ("identity", "deploy-key")], 2));
        assert_eq!(
            few,
            vec![
                "将被删除：".to_owned(),
                "  · 主机：web-1".to_owned(),
                "  · 凭据：deploy-key".to_owned(),
            ],
            "刚好列全时不写「另有」"
        );

        let many = deletion_lines(&plan(
            &[
                ("host", "h0"),
                ("host", "h1"),
                ("host", "h2"),
                ("group", "g0"),
                ("proxy", "p0"),
            ],
            12,
        ));
        assert_eq!(many.len(), 1 + 5 + 1);
        assert_eq!(many[1], "  · 主机：h0");
        assert_eq!(many[4], "  · 分组：g0");
        assert_eq!(many[5], "  · 代理：p0");
        assert_eq!(many[6], "  …另有 7 条");

        assert!(deletion_lines(&DeletionPlan::default()).is_empty());
    }

    #[test]
    fn using_the_other_side_of_a_conflict_waits_for_sync_to_finish() {
        let _serial = Serial::new();
        let held = SyncInFlight::acquire(ExclusiveKind::Sync).expect("占住同步权");
        // 在拿到独占权之前就返回，连数据库都不会去打开（这个目录里没有库）。
        let dir = temp_dir("use-other-busy");
        let error = super::use_other_at(&dir, 1).unwrap_err();
        assert!(error.contains("同步正在进行"), "{error}");
        drop(held);
        let error = super::use_other_at(&dir, 1).unwrap_err();
        assert!(
            !error.contains("同步正在进行"),
            "放掉后走到打开数据库那一步: {error}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_report_view_shows_the_names_only_when_confirmation_is_needed() {
        use zssh_core::sync::engine::SyncReport;
        let report = |status| SyncReport {
            status,
            summary: "这次同步会删除 2 条记录".to_owned(),
            conflicts: Vec::new(),
            nulled_references: Vec::new(),
            deletion: plan(&[("host", "web-1"), ("host", "web-2")], 2),
            backup: None,
            applied: None,
            cloud_version: None,
            retries: 0,
        };
        let lines = super::sync_report_lines(&report(SyncStatus::NeedsConfirmation));
        assert!(lines.contains(&"  · 主机：web-1".to_owned()), "{lines:?}");
        assert!(lines.contains(&"  · 主机：web-2".to_owned()), "{lines:?}");
        let lines = super::sync_report_lines(&report(SyncStatus::Completed));
        assert!(
            !lines.iter().any(|line| line.contains("web-1")),
            "{lines:?}"
        );
    }

    // -- D2-16：地址改了不带旧密码 ---------------------------------------------

    #[test]
    fn changing_the_address_does_not_carry_the_old_password_over() {
        let saved = StorageConfig::Webdav(WebDavConfig {
            url: "https://old.example.com/dav/".to_owned(),
            directory: "zssh".to_owned(),
            username: "alice".to_owned(),
            password: "old-server-secret".to_owned(),
            allow_insecure_http: false,
        });
        let form = |url: &str| WebDavForm {
            url: url.to_owned(),
            directory: "zssh".to_owned(),
            username: "alice".to_owned(),
            password: String::new(),
            allow_insecure_http: false,
        };
        let password_of = |config: StorageConfig| match config {
            StorageConfig::Webdav(config) => config.password,
            other => panic!("不是 WebDAV：{other:?}"),
        };
        // 地址没变（含大小写、结尾斜杠不同）：留空沿用旧密码。
        for same in [
            "https://old.example.com/dav/",
            "HTTPS://OLD.example.com/dav",
        ] {
            let config = build_webdav(&form(same), Some(&saved)).expect("有效");
            assert_eq!(password_of(config), "old-server-secret", "{same}");
        }
        // 地址变了：留空就是空，旧服务器的密码不能被带过去。
        let config =
            build_webdav(&form("https://other.example.com/dav/"), Some(&saved)).expect("有效");
        assert_eq!(password_of(config), "");
    }
}

#[cfg(test)]
mod redaction_tests {
    // 同 `tests`：不写 `use super::*`（会盖掉内置的 `#[test]`）。
    use super::{GithubForm, S3Form, WebDavForm};

    #[test]
    fn the_sync_forms_never_show_their_secrets_in_debug_output() {
        let webdav = WebDavForm {
            url: "https://dav.example.com/".to_owned(),
            username: "alice".to_owned(),
            password: "dav-password-hunter2".to_owned(),
            ..Default::default()
        };
        let s3 = S3Form {
            bucket: "backups".to_owned(),
            access_key: "AKIAEXAMPLE".to_owned(),
            secret_key: "s3-secret-hunter2".to_owned(),
            ..Default::default()
        };
        let github = GithubForm {
            token: "ghp_token_hunter2".to_owned(),
            gist_id: "abc123".to_owned(),
            ..Default::default()
        };
        for text in [
            format!("{webdav:?}"),
            format!("{webdav:#?}"),
            format!("{s3:?}"),
            format!("{s3:#?}"),
            format!("{github:?}"),
            format!("{github:#?}"),
        ] {
            assert!(!text.contains("hunter2"), "{text}");
            assert!(text.contains("<已隐藏>"), "{text}");
        }
        // 非秘密字段照常显示，空的秘密也看得出是空的。
        let text = format!("{:?}", WebDavForm::default());
        assert!(
            text.contains("<空>") && !text.contains("<已隐藏>"),
            "{text}"
        );
        assert!(format!("{webdav:?}").contains("dav.example.com"));
        assert!(format!("{s3:?}").contains("AKIAEXAMPLE"));
        assert!(format!("{github:?}").contains("abc123"));
    }
}
