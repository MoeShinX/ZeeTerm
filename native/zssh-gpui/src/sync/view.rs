//! 同步设置的画法（G11）：按 0.2.14 设置页同步一节的样子分成四张卡片——连接存储、主密码、
//! 同步、冲突。设置页里嵌的是这几张卡；独立窗口（调试入口）只是再套一层标题和滚动容器。
//!
//! 逻辑都在 `sync.rs`（F5 修好的那一套），这里只负责画和接线。每张卡、每一行都拆成
//! 单独的 `#[inline(never)]` 函数：debug 构建里一个函数的栈帧会把所有临时元素都留着，
//! 写成一大坨时单个栈帧能到几百 KB，叠上窗口绘制的调用链会撑爆主线程栈。

use std::time::Instant;

use ::gpui::{AnyElement, App, Context, ElementId, Entity, Hsla, SharedString, Window, px};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputContentType, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::sync::service::{ConcurrencyRecord, LastResult};

use super::status::format_stamp;
use super::strength::password_strength;
use super::{
    ProviderChoice, StorageConfig, SyncDemo, SyncMsg, auto_block_reason, conflict_row,
    deletion_lines, onedrive_account_label, probe_text, sync_report_lines, with_sync_guard,
};
use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::settings_view::card;
use crate::typography::{Role, Typo as _};

/// 同步设置页（`view.rs`）自己的界面文案：各卡片标题、字段标签、说明、按钮和状态行。
///
/// 只放应用自己的固定文案；地址、账号名、Gist ID、测试结果里的错误原文和设备名属于远端 /
/// 用户数据，原样拼在本地化前缀后面，不经过这里。id（`sync-save`、`sync-auto-toggle`……）
/// 和 `WebDAV` / `S3` / `GitHub Gist` / `OneDrive` 这些服务名不进这张表。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SyncViewMsg {
    // ---- 窗口标题 / 副标题 ----
    WindowTitle,
    HeaderSubtitle,
    // ---- 上次结果 ----
    NeverSynced,
    AutoSuffix,
    UploadedVersionPrefix,
    SyncedOkSuffix,
    StoppedForConfirmationSuffix,
    CancelledSuffix,
    LocalChangedSuffix,
    FailedSuffix,
    // ---- 并发保护 ----
    ConcurrencyNotProbed,
    ConcurrencyUnsupported,
    ConcurrencySupported,
    // ---- 连接存储 ----
    ConnectionCardTitle,
    ConnectionCardDescription,
    StorageType,
    WebdavAddress,
    WebdavAddressExample,
    Username,
    Password,
    Directory,
    DirectoryHint,
    SavedPasswordHint,
    PlainHttpTitle,
    PlainHttpBody,
    PlainHttpCheck,
    S3EndpointExample,
    S3Prefix,
    AddressStyle,
    PathStyle,
    VirtualHostStyle,
    SavedSecretHint,
    ApiBase,
    ApiBaseHint,
    Token,
    TokenHint,
    GistIdHint,
    SavedTokenHint,
    TokenHttpTitle,
    TokenHttpCheck,
    SignInWithMicrosoft,
    OneDriveIntro,
    Account,
    OneDriveTokenHint,
    OneDriveExpired,
    LoggingInBrowser,
    LoggingInHint,
    CancelLogin,
    Relogin,
    ConfirmDisconnect,
    Disconnect,
    ClientIdInvalid,
    DisconnectHint,
    OpenAppsPage,
    AdvancedExpanded,
    AdvancedCollapsed,
    OwnClientId,
    OwnClientIdHint,
    // ---- 存储连接按钮 ----
    ConfirmClearAgain,
    Clear,
    Save,
    Testing,
    TestConnection,
    LastProbe,
    // ---- 主密码 ----
    ForgotWarningTitle,
    ForgotWarningBody,
    CloudHasDataHint,
    MasterPassword,
    PasswordAgain,
    UnderstoodCheck,
    RememberTitle,
    RememberDescriptionPrefix,
    RememberA11y,
    MasterPasswordHint,
    MasterPasswordCardTitle,
    MasterPasswordCardDescription,
    ChangeMasterPassword,
    ChangeMasterPasswordDescription,
    Change,
    OldMasterPassword,
    NewMasterPassword,
    ConfirmNewMasterPassword,
    ChangeWarning,
    Changing,
    ChangeAndUpload,
    Cancel,
    // ---- 同步 ----
    Syncing,
    SyncNow,
    SyncResults,
    SyncCardTitle,
    SyncCardDescription,
    DeleteManyTitle,
    NothingChangedYet,
    DeletionChoices,
    UseCloudVersion,
    Continue,
    BaselineBrokenTitle,
    BaselineBrokenBody,
    ConfirmRebuild,
    RebuildBaseline,
    AutoSync,
    AutoSyncDescription,
    AutoSyncA11y,
    Interval,
    IntervalDescription,
    AutoSyncPausedPrefix,
    AutoSyncSkipsHint,
    // ---- 冲突 ----
    NoConflicts,
    ConfirmUseOther,
    RestoreThisRecord,
    UseOther,
    GotIt,
    ConflictsCardTitle,
    ConflictsCardDescription,
}

impl Msg for SyncViewMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::WindowTitle => "同步设置",
            Self::HeaderSubtitle => "连接信息只保存在这台电脑上，云端放加密后的同步包",
            Self::NeverSynced => "还没有同步过",
            Self::AutoSuffix => "（自动）",
            Self::UploadedVersionPrefix => "，上传版本 ",
            Self::SyncedOkSuffix => " 同步成功",
            Self::StoppedForConfirmationSuffix => " 停在了防误删确认上",
            Self::CancelledSuffix => " 已取消",
            Self::LocalChangedSuffix => " 同步期间本地有改动，稍后自动重新同步",
            Self::FailedSuffix => " 失败：",
            Self::ConcurrencyNotProbed => {
                "还没有检测这个存储是否支持并发保护，同步会按「写之前再读一次」处理。点「测试连接」检测。"
            }
            Self::ConcurrencyUnsupported => {
                "这个服务不支持并发保护，多台电脑同时同步时有很小的概率覆盖。"
            }
            Self::ConcurrencySupported => "支持并发保护（多台电脑同时同步不会互相覆盖）。",
            Self::ConnectionCardTitle => "连接存储",
            Self::ConnectionCardDescription => {
                "同步包整体加密后存到你自己的 WebDAV、S3 兼容存储、GitHub Gist 或 OneDrive。\
                 这里的密码 / Secret / 令牌 / 登录信息只用本机的 Windows 账户加密保存，不会进同步包。"
            }
            Self::StorageType => "存储类型",
            Self::WebdavAddress => "WebDAV 地址",
            Self::WebdavAddressExample => "例如 https://dav.example.com/remote.php/dav/files/me/",
            Self::Username => "用户名",
            Self::Password => "密码",
            Self::Directory => "存放目录（可选）",
            Self::DirectoryHint => "同步包放在这个子目录里，不存在会自动创建。",
            Self::SavedPasswordHint => "已保存密码；留空 = 继续用已保存的密码",
            Self::PlainHttpTitle => "地址是 http://，密码会以明文发送",
            Self::PlainHttpBody => {
                "同一网络里的人可以看到你的用户名和密码。同步包本身是加密的，但账号密码不是。\
                 只在可信的内网里这样用。"
            }
            Self::PlainHttpCheck => "我了解密码会以明文发送，仍然使用 http://",
            Self::S3EndpointExample => {
                "例如 https://s3.us-east-1.amazonaws.com、http://127.0.0.1:9000"
            }
            Self::S3Prefix => "对象路径前缀（可选）",
            Self::AddressStyle => "地址风格",
            Self::PathStyle => "路径风格（endpoint/bucket/key）",
            Self::VirtualHostStyle => "虚拟主机风格（bucket.endpoint/key）",
            Self::SavedSecretHint => "已保存 Secret；留空 = 继续用已保存的 Secret",
            Self::ApiBase => "接口地址",
            Self::ApiBaseHint => "留空 = https://api.github.com（GitHub Enterprise 填自己的）",
            Self::Token => "个人访问令牌",
            Self::TokenHint => {
                "需要 gist 权限。令牌只用本机的 Windows 账户加密保存，不会进同步包。"
            }
            Self::GistIdHint => {
                "留空 = 第一次同步时自动新建一个 secret Gist。第二台电脑上填同一个 Gist ID 和一个有 \
                 Gist 权限的令牌。secret Gist 拿到链接的人都能打开，但里面是密文。"
            }
            Self::SavedTokenHint => "已保存令牌；留空 = 继续用已保存的令牌",
            Self::TokenHttpTitle => "接口地址是 http://，令牌会以明文发送",
            Self::TokenHttpCheck => "我了解令牌会以明文发送，仍然使用 http://",
            Self::SignInWithMicrosoft => "用微软账号登录",
            Self::OneDriveIntro => {
                "点「登录」后，ZeeTerm 会用系统默认浏览器打开微软的登录页；登录并同意后浏览器会自动\
                 跳回本机，不用复制任何东西。同步包只放在 OneDrive 的应用专用文件夹（OneDrive 里 \
                 Apps 下以应用名命名的文件夹）里，ZeeTerm 看不到你的其他文件。目前只支持个人微软账号\
                 （outlook.com、hotmail.com 等），工作或学校账号暂不支持。"
            }
            Self::Account => "账号",
            Self::OneDriveTokenHint => {
                "登录信息（令牌）用本机的 Windows 账户加密保存，不会进同步包。"
            }
            Self::OneDriveExpired => {
                "OneDrive 登录已失效，请重新登录。自动同步已暂停，重新登录之后恢复。"
            }
            Self::LoggingInBrowser => "正在浏览器中登录…",
            Self::LoggingInHint => {
                "请在刚刚打开的浏览器页面里登录并同意授权，最多等 5 分钟。完成后浏览器会自动回到本机，\
                 显示「授权成功」。浏览器这一步用的是系统浏览器自己的网络设置；登录之后 ZeeTerm 换\
                 令牌和同步走「连接」→「软件网络」里设置的代理。"
            }
            Self::CancelLogin => "取消登录",
            Self::Relogin => "重新登录",
            Self::ConfirmDisconnect => "再次点击「确认断开」",
            Self::Disconnect => "断开连接",
            Self::ClientIdInvalid => {
                "client ID 的格式不对，应该形如 xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx。"
            }
            Self::DisconnectHint => {
                "断开连接只删除这台电脑上保存的登录信息。如果还要彻底撤销 ZeeTerm 的授权，请在微软账户的\
                 「应用和服务」页面里移除它。"
            }
            Self::OpenAppsPage => "在浏览器中打开「应用和服务」页面",
            Self::AdvancedExpanded => "高级 ▾",
            Self::AdvancedCollapsed => "高级 ▸",
            Self::OwnClientId => "自己的 client ID",
            Self::OwnClientIdHint => {
                "填了就用它登录，不用程序里内置的。client ID 只存在这台电脑上，不会同步。"
            }
            Self::ConfirmClearAgain => "再次点击「确认清除」",
            Self::Clear => "清除",
            Self::Save => "保存",
            Self::Testing => "测试中…",
            Self::TestConnection => "测试连接",
            Self::LastProbe => "最近一次测试连接的结果：",
            Self::ForgotWarningTitle => "忘记主密码就无法恢复云端数据。",
            Self::ForgotWarningBody => {
                "主密码不会上传，也没有「找回密码」——忘了只能删掉云端数据重新开始。请把它记在\
                 安全的地方。"
            }
            Self::CloudHasDataHint => {
                "云端已经有同步数据。输入当初设置的主密码来解开它（这台电脑上的主机会和云端合并，\
                 两边都不会丢）。"
            }
            Self::MasterPassword => "主密码",
            Self::PasswordAgain => "再输入一次",
            Self::UnderstoodCheck => "我知道忘记主密码就无法恢复云端数据",
            Self::RememberTitle => "在这台电脑上记住主密码",
            Self::RememberDescriptionPrefix => "用 Windows 账户加密保存，下次不用再输入。",
            Self::RememberA11y => "在这台电脑上记住主密码",
            Self::MasterPasswordHint => {
                "主密码用来解开云端同步包，至少 8 个字符；不上传、不写日志。"
            }
            Self::MasterPasswordCardTitle => "主密码",
            Self::MasterPasswordCardDescription => {
                "同步包用主密码加密后才上传，云端（和存储服务商）看不到你的任何内容。"
            }
            Self::ChangeMasterPassword => "修改主密码",
            Self::ChangeMasterPasswordDescription => {
                "需要旧密码。会用新密码重新加密云端数据；其他电脑之后要输入新的主密码。"
            }
            Self::Change => "修改…",
            Self::OldMasterPassword => "旧主密码",
            Self::NewMasterPassword => "新主密码",
            Self::ConfirmNewMasterPassword => "确认新主密码",
            Self::ChangeWarning => "会用新密码重新加密云端数据。忘记主密码就无法恢复云端数据。",
            Self::Changing => "修改中…",
            Self::ChangeAndUpload => "修改并上传",
            Self::Cancel => "取消",
            Self::Syncing => "同步中…",
            Self::SyncNow => "立即同步",
            Self::SyncResults => "同步结果",
            Self::SyncCardTitle => "同步",
            Self::SyncCardDescription => "手动同步，或者让它在后台自动做。",
            Self::DeleteManyTitle => "这次同步会删除很多记录",
            Self::NothingChangedYet => "还没有改动任何数据。",
            Self::DeletionChoices => {
                "· 继续：按合并结果删除这些记录（删除会同步到云端）。\n\
                 · 用云端版本覆盖本地：放弃本地的版本，以云端为准。\n\
                 · 取消：本地和云端都不动。"
            }
            Self::UseCloudVersion => "用云端版本覆盖本地",
            Self::Continue => "继续",
            Self::BaselineBrokenTitle => "同步基线损坏，暂停同步",
            Self::BaselineBrokenBody => {
                "基线是「上次同步成功时的状态」，用来判断哪边改过什么。重建后下一次同步按首次\
                 同步处理：两边的数据都会保留，同名的不会去重（可能出现重复的主机，之后手动删就\
                 行）；重建之前在另一边删掉的记录可能会回来。"
            }
            Self::ConfirmRebuild => "确认重建",
            Self::RebuildBaseline => "重建同步基线",
            Self::AutoSync => "自动同步",
            Self::AutoSyncDescription => {
                "应用启动后每隔一段时间自动同步一次。遇到需要确认的情况会停下来等你。"
            }
            Self::AutoSyncA11y => "自动同步",
            Self::Interval => "定时同步间隔（分钟）",
            Self::IntervalDescription => {
                "1–1440，回车或移开焦点保存。本机记住主密码、保存好存储设置才会自动同步，失败后按 1x / 2x / 4x \
                 间隔退避。"
            }
            Self::AutoSyncPausedPrefix => "自动同步暂停：",
            Self::AutoSyncSkipsHint => "不同步：执行模式、智能体路径、窗口状态等本机设置。",
            Self::NoConflicts => "没有冲突。",
            Self::ConfirmUseOther => "确认改用",
            Self::RestoreThisRecord => "恢复这条记录",
            Self::UseOther => "改用另一份",
            Self::GotIt => "我知道了",
            Self::ConflictsCardTitle => "冲突",
            Self::ConflictsCardDescription => {
                "两台电脑改了同一条记录时，默认采用修改时间较新的那份；一边删除、另一边修改时，删除生效。\
                 没被采用的那份留在这里，保留 90 天后自动清理。"
            }
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::WindowTitle => "Sync Settings",
            Self::HeaderSubtitle => {
                "Connection details stay on this computer; only the encrypted sync package goes to the cloud"
            }
            Self::NeverSynced => "Never synced",
            Self::AutoSuffix => " (auto)",
            Self::UploadedVersionPrefix => ", uploaded version ",
            Self::SyncedOkSuffix => " synced successfully",
            Self::StoppedForConfirmationSuffix => " stopped at the deletion confirmation",
            Self::CancelledSuffix => " cancelled",
            Self::LocalChangedSuffix => {
                " local changes during the sync; it retries automatically later"
            }
            Self::FailedSuffix => " failed: ",
            Self::ConcurrencyNotProbed => {
                "It is not known yet whether this storage supports concurrency protection; the sync reads again before writing. Click “Test Connection” to check."
            }
            Self::ConcurrencyUnsupported => {
                "This service does not support concurrency protection, so two computers syncing at the same time have a small chance of overwriting each other."
            }
            Self::ConcurrencySupported => {
                "Concurrency protection is supported (two computers syncing at the same time do not overwrite each other)."
            }
            Self::ConnectionCardTitle => "Storage Connection",
            Self::ConnectionCardDescription => {
                "The encrypted sync package is stored in your own WebDAV, S3-compatible storage, GitHub Gist or OneDrive. \
                 The password / secret / token / sign-in info here is encrypted with your Windows account and never \
                 enters the sync package."
            }
            Self::StorageType => "Storage Type",
            Self::WebdavAddress => "WebDAV URL",
            Self::WebdavAddressExample => {
                "For example https://dav.example.com/remote.php/dav/files/me/"
            }
            Self::Username => "Username",
            Self::Password => "Password",
            Self::Directory => "Directory (optional)",
            Self::DirectoryHint => {
                "The sync package goes into this subdirectory; it is created if missing."
            }
            Self::SavedPasswordHint => "Password saved; leave empty to keep using it",
            Self::PlainHttpTitle => "The URL is http://, so the password is sent in clear text",
            Self::PlainHttpBody => {
                "Anyone on the same network can see your username and password. The sync package itself is \
                 encrypted, but the credentials are not. Only do this on a trusted internal network."
            }
            Self::PlainHttpCheck => {
                "I understand the password is sent in clear text and still want to use http://"
            }
            Self::S3EndpointExample => {
                "For example https://s3.us-east-1.amazonaws.com, http://127.0.0.1:9000"
            }
            Self::S3Prefix => "Object path prefix (optional)",
            Self::AddressStyle => "Address Style",
            Self::PathStyle => "Path style (endpoint/bucket/key)",
            Self::VirtualHostStyle => "Virtual-host style (bucket.endpoint/key)",
            Self::SavedSecretHint => "Secret saved; leave empty to keep using it",
            Self::ApiBase => "API URL",
            Self::ApiBaseHint => {
                "Leave empty for https://api.github.com (GitHub Enterprise users enter their own)"
            }
            Self::Token => "Personal Access Token",
            Self::TokenHint => {
                "Needs the gist scope. The token is encrypted with your Windows account and never enters the sync package."
            }
            Self::GistIdHint => {
                "Leave empty to create a secret Gist on the first sync. On the second computer enter the same Gist ID \
                 and a token with Gist access. Anyone with the link can open a secret Gist, but the content is encrypted."
            }
            Self::SavedTokenHint => "Token saved; leave empty to keep using it",
            Self::TokenHttpTitle => "The API URL is http://, so the token is sent in clear text",
            Self::TokenHttpCheck => {
                "I understand the token is sent in clear text and still want to use http://"
            }
            Self::SignInWithMicrosoft => "Sign in with a Microsoft account",
            Self::OneDriveIntro => {
                "After you click “Sign In”, ZeeTerm opens the Microsoft sign-in page in your default browser; after \
                 signing in and consenting, the browser returns here automatically and you copy nothing. The sync \
                 package is stored only in OneDrive's app-specific folder (the folder named after the app under Apps \
                 in OneDrive), and ZeeTerm cannot see your other files. Only personal Microsoft accounts \
                 (outlook.com, hotmail.com etc.) are supported; work or school accounts are not yet."
            }
            Self::Account => "Account",
            Self::OneDriveTokenHint => {
                "The sign-in info (token) is encrypted with your Windows account and never enters the sync package."
            }
            Self::OneDriveExpired => {
                "The OneDrive sign-in has expired; please sign in again. Auto sync is paused and resumes after you sign in again."
            }
            Self::LoggingInBrowser => "Signing in in the browser…",
            Self::LoggingInHint => {
                "Sign in and grant access in the browser page that just opened; this waits at most 5 minutes. The \
                 browser then returns here and shows “Authorization successful”. This browser step uses the system \
                 browser's own network settings; after signing in, ZeeTerm exchanges the token and syncs through the \
                 proxy set under “Connection” → “App Network”."
            }
            Self::CancelLogin => "Cancel Sign-in",
            Self::Relogin => "Sign In Again",
            Self::ConfirmDisconnect => "Click “Confirm Disconnect” again",
            Self::Disconnect => "Disconnect",
            Self::ClientIdInvalid => {
                "The client ID has the wrong format; it should look like xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx."
            }
            Self::DisconnectHint => {
                "Disconnecting only deletes the sign-in info saved on this computer. To revoke ZeeTerm's authorization \
                 completely, remove it on the Microsoft account's “Apps and services” page."
            }
            Self::OpenAppsPage => "Open the “Apps and services” page in the browser",
            Self::AdvancedExpanded => "Advanced ▾",
            Self::AdvancedCollapsed => "Advanced ▸",
            Self::OwnClientId => "Own client ID",
            Self::OwnClientIdHint => {
                "If filled in, it is used for sign-in instead of the built-in one. The client ID stays on this computer and is never synced."
            }
            Self::ConfirmClearAgain => "Click “Confirm Clear” again",
            Self::Clear => "Clear",
            Self::Save => "Save",
            Self::Testing => "Testing…",
            Self::TestConnection => "Test Connection",
            Self::LastProbe => "Result of the last connection test:",
            Self::ForgotWarningTitle => {
                "If you forget the master password, the cloud data cannot be recovered."
            }
            Self::ForgotWarningBody => {
                "The master password is never uploaded and there is no password recovery — if you forget it, the \
                 only option is to delete the cloud data and start over. Keep it somewhere safe."
            }
            Self::CloudHasDataHint => {
                "The cloud already has sync data. Enter the master password you set originally to unlock it (the hosts \
                 on this computer are merged with the cloud, and neither side is lost)."
            }
            Self::MasterPassword => "Master Password",
            Self::PasswordAgain => "Enter it again",
            Self::UnderstoodCheck => {
                "I understand that forgetting the master password means the cloud data cannot be recovered"
            }
            Self::RememberTitle => "Remember the master password on this computer",
            Self::RememberDescriptionPrefix => {
                "Encrypted with your Windows account, so you do not have to enter it next time. "
            }
            Self::RememberA11y => "Remember the master password on this computer",
            Self::MasterPasswordHint => {
                "The master password unlocks the cloud sync package and needs at least 8 characters; it is never uploaded or logged."
            }
            Self::MasterPasswordCardTitle => "Master Password",
            Self::MasterPasswordCardDescription => {
                "The sync package is encrypted with the master password before upload, so the cloud (and the storage provider) cannot see any of your content."
            }
            Self::ChangeMasterPassword => "Change Master Password",
            Self::ChangeMasterPasswordDescription => {
                "The old password is required. The cloud data is re-encrypted with the new password; other computers must enter the new one afterwards."
            }
            Self::Change => "Change…",
            Self::OldMasterPassword => "Old Master Password",
            Self::NewMasterPassword => "New Master Password",
            Self::ConfirmNewMasterPassword => "Confirm New Master Password",
            Self::ChangeWarning => {
                "The cloud data is re-encrypted with the new password. If you forget the master password, the cloud data cannot be recovered."
            }
            Self::Changing => "Changing…",
            Self::ChangeAndUpload => "Change and Upload",
            Self::Cancel => "Cancel",
            Self::Syncing => "Syncing…",
            Self::SyncNow => "Sync Now",
            Self::SyncResults => "Sync Results",
            Self::SyncCardTitle => "Sync",
            Self::SyncCardDescription => {
                "Sync manually, or let it run automatically in the background."
            }
            Self::DeleteManyTitle => "This sync would delete many records",
            Self::NothingChangedYet => "Nothing has been changed yet.",
            Self::DeletionChoices => {
                "· Continue: delete these records as the merge result says (the deletion syncs to the cloud).\n\
                 · Use the cloud version: discard the local version and take the cloud one.\n\
                 · Cancel: nothing changes locally or in the cloud."
            }
            Self::UseCloudVersion => "Use the Cloud Version",
            Self::Continue => "Continue",
            Self::BaselineBrokenTitle => "The sync baseline is broken; sync is paused",
            Self::BaselineBrokenBody => {
                "The baseline is “the state at the last successful sync” and is used to tell which side changed what. \
                 After rebuilding, the next sync is treated as the first one: both sides keep their data and \
                 same-named records are not deduplicated (duplicate hosts may appear; delete them manually \
                 afterwards); records deleted on the other side before the rebuild may come back."
            }
            Self::ConfirmRebuild => "Confirm Rebuild",
            Self::RebuildBaseline => "Rebuild Sync Baseline",
            Self::AutoSync => "Auto Sync",
            Self::AutoSyncDescription => {
                "After the app starts, sync automatically at a fixed interval. It stops and waits for you when something needs confirmation."
            }
            Self::AutoSyncA11y => "Auto Sync",
            Self::Interval => "Sync Interval (minutes)",
            Self::IntervalDescription => {
                "1–1440; saved on Enter or when focus leaves. Auto sync only runs when the master password is \
                 remembered and storage settings are saved; after a failure it backs off at 1x / 2x / 4x the interval."
            }
            Self::AutoSyncPausedPrefix => "Auto sync paused: ",
            Self::AutoSyncSkipsHint => {
                "Not synced: local settings such as execution mode, agent paths and window state."
            }
            Self::NoConflicts => "No conflicts.",
            Self::ConfirmUseOther => "Confirm Switch",
            Self::RestoreThisRecord => "Restore This Record",
            Self::UseOther => "Use the Other Copy",
            Self::GotIt => "Got It",
            Self::ConflictsCardTitle => "Conflicts",
            Self::ConflictsCardDescription => {
                "When two computers changed the same record, the newer modification wins by default; when one side \
                 deleted and the other modified, the deletion wins. The copy that was not used stays here and is \
                 cleaned up automatically after 90 days."
            }
        }
    }
}

/// 表单里标签的宽度。
const LABEL_WIDTH: f32 = 112.0;

// ---------------------------------------------------------------------------
// 纯函数：文案和判断
// ---------------------------------------------------------------------------

/// OneDrive 登录按钮能不能点：「高级」里填了就看格式，没填就用内置的 client ID（总是可用）。
pub(super) fn can_login(custom_client_id: &str) -> bool {
    let custom = custom_client_id.trim();
    if custom.is_empty() {
        return true;
    }
    zssh_core::sync::onedrive::config::is_valid_client_id(custom)
}

/// 「立即同步」那一行下面的说明（0.2.14 的 `lastResultText`）。
pub(super) fn last_result_text(last: Option<&LastResult>) -> String {
    let Some(last) = last else {
        return t(SyncViewMsg::NeverSynced).to_owned();
    };
    let when = format_stamp(last.at);
    let how = if last.auto {
        t(SyncViewMsg::AutoSuffix)
    } else {
        ""
    };
    match last.status.as_str() {
        "completed" => {
            let version = last
                .version
                .as_deref()
                .filter(|v| !v.trim().is_empty())
                .map(|v| format!("{}{v}", t(SyncViewMsg::UploadedVersionPrefix)))
                .unwrap_or_default();
            format!(
                "{when}{how}{ok}{version}",
                ok = t(SyncViewMsg::SyncedOkSuffix)
            )
        }
        "needs_confirmation" => {
            format!(
                "{when}{how}{}",
                t(SyncViewMsg::StoppedForConfirmationSuffix)
            )
        }
        "cancelled" => format!("{when}{}", t(SyncViewMsg::CancelledSuffix)),
        "local_changed" => {
            format!("{when}{how}{}", t(SyncViewMsg::LocalChangedSuffix))
        }
        "error" => format!(
            "{when}{how}{}{}",
            t(SyncViewMsg::FailedSuffix),
            last.summary
        ),
        other => format!("{when}{how} {other}"),
    }
}

/// 并发保护的提示（0.2.14 的 `concurrencyNotice`）：没检测过 / 不支持都要明说，不能悄悄当成支持。
/// 返回（是否正常, 文字）；没连存储就没有提示。
pub(super) fn concurrency_notice(
    configured: bool,
    record: Option<&ConcurrencyRecord>,
) -> Option<(bool, &'static str)> {
    use zssh_core::sync::remote::ConcurrencyMode;
    if !configured {
        return None;
    }
    Some(match record.map(|record| record.mode) {
        None => (false, t(SyncViewMsg::ConcurrencyNotProbed)),
        Some(ConcurrencyMode::ReadBeforeWrite) => (false, t(SyncViewMsg::ConcurrencyUnsupported)),
        Some(ConcurrencyMode::Conditional) => (true, t(SyncViewMsg::ConcurrencySupported)),
    })
}

/// 地址是不是明文 http://（WebDAV 地址 / GitHub 接口地址）。
pub(super) fn is_plain_http(url: &str) -> bool {
    url.trim().to_ascii_lowercase().starts_with("http://")
}

// ---------------------------------------------------------------------------
// 小部件
// ---------------------------------------------------------------------------

pub(super) fn muted(text: impl Into<SharedString>, tokens: &Tokens) -> AnyElement {
    div()
        .typo(Role::Caption, tokens)
        .child(text.into())
        .into_any_element()
}

pub(super) fn colored(text: impl Into<SharedString>, color: Hsla) -> AnyElement {
    div()
        .text_xs()
        .text_color(color)
        .child(text.into())
        .into_any_element()
}

/// 有颜色的提示框（0.2.14 的琥珀 / 红色 `rounded-md border bg-…/10` 那种）。
pub(super) fn banner(color: Hsla, children: Vec<AnyElement>) -> AnyElement {
    v_flex()
        .w_full()
        .gap_2()
        .p_3()
        .rounded_md()
        .border_1()
        .border_color(color.opacity(0.5))
        .bg(color.opacity(0.1))
        .text_xs()
        .children(children)
        .into_any_element()
}

/// 字段上方的小标签（V2-U17 起表单都是「上标签、下输入框」的单栏，不再左标签对齐）。
fn label_above(label: &'static str, tokens: &Tokens) -> impl IntoElement {
    div().typo(Role::FieldLabel, tokens).child(label)
}

/// 一个字段：标签在上、输入框在下，整行宽。
pub(super) fn field(
    label: &'static str,
    input: &Entity<InputState>,
    tokens: &Tokens,
) -> AnyElement {
    v_flex()
        .w_full()
        .gap_1()
        .child(label_above(label, tokens))
        .child(Input::new(input).w_full())
        .into_any_element()
}

pub(super) fn password_field(
    label: &'static str,
    input: &Entity<InputState>,
    tokens: &Tokens,
) -> AnyElement {
    v_flex()
        .w_full()
        .gap_1()
        .child(label_above(label, tokens))
        .child(
            Input::new(input)
                .w_full()
                .content_type(InputContentType::Password),
        )
        .into_any_element()
}

fn label_row(label: &'static str, value: String, tokens: &Tokens) -> AnyElement {
    h_flex()
        .w_full()
        .gap_2()
        .items_center()
        .child(
            div()
                .w(px(LABEL_WIDTH))
                .flex_none()
                .text_sm()
                .text_color(tokens.text_muted)
                .child(label),
        )
        .child(div().min_w_0().text_sm().child(value))
        .into_any_element()
}

/// 设置行：左边标题 + 说明，右边是控件（同设置页其他行）。
pub(super) fn setting(
    title: &'static str,
    description: impl Into<SharedString>,
    control: impl IntoElement,
    tokens: &Tokens,
) -> AnyElement {
    h_flex()
        .w_full()
        .min_h(px(28.0))
        .gap_4()
        .items_center()
        .justify_between()
        .child(
            v_flex()
                .min_w_0()
                .gap_0p5()
                .child(div().typo(Role::ItemName, tokens).child(title))
                .child(muted(description, tokens)),
        )
        .child(h_flex().flex_none().gap_1().items_center().child(control))
        .into_any_element()
}

/// 强度条：四格，颜色跟着评分（红 / 橙 / 黄 / 绿）。
fn strength_bar(password: &str, tokens: &Tokens) -> Option<AnyElement> {
    let strength = password_strength(password);
    if strength.score == 0 {
        return None;
    }
    let fill: Hsla = match strength.score {
        1 => ::gpui::rgb(0xEF4444).into(),
        2 => ::gpui::rgb(0xF97316).into(),
        3 => ::gpui::rgb(0xEAB308).into(),
        _ => ::gpui::rgb(0x10B981).into(),
    };
    Some(
        h_flex()
            .id("password-strength")
            .gap_2()
            .items_center()
            .child(h_flex().gap_0p5().children((1..=4u8).map(|step| {
                div()
                    .w(px(30.0))
                    .h(px(6.0))
                    .rounded_full()
                    .bg(if step <= strength.score {
                        fill
                    } else {
                        tokens.subtle
                    })
            })))
            .child(muted(strength.label, tokens))
            .into_any_element(),
    )
}

/// 点一下就在 `SyncDemo` 上调一个方法的按钮。
fn act_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    entity: &Entity<SyncDemo>,
    disabled: bool,
    run: impl Fn(&mut SyncDemo, &mut Window, &mut Context<SyncDemo>) + 'static,
) -> Button {
    let entity = entity.clone();
    Button::new(id)
        .label(label)
        .compact()
        .disabled(disabled)
        .on_click(move |_, window, cx| {
            entity.update(cx, |this, cx| run(this, window, cx));
        })
}

// ---------------------------------------------------------------------------
// 卡片
// ---------------------------------------------------------------------------

impl SyncDemo {
    /// 根：嵌在设置页里只画卡片；独立窗口再加标题和滚动。
    pub(super) fn render_root(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tokens = Tokens::current(cx);
        let cards = self.cards(&tokens, cx);
        // 密码输入框里有没有字，状态条要知道（没记住主密码时输入框就是内存里的主密码）。
        super::status::set_password_in_memory(!self.master_password.read(cx).value().is_empty());
        if self.embedded {
            return v_flex()
                .id("sync-section")
                .w_full()
                .gap_3()
                .children(cards)
                .into_any_element();
        }
        v_flex()
            .id("sync-settings-scroll")
            .size_full()
            .p_4()
            .gap_3()
            .track_focus(&self.focus)
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .bg(tokens.background)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_lg().child(t(SyncViewMsg::WindowTitle)))
                    .child(muted(t(SyncViewMsg::HeaderSubtitle), &tokens)),
            )
            .children(cards)
            .into_any_element()
    }

    /// 全部卡片。和 0.2.14 一样：连接了存储才有主密码 / 同步 / 冲突三张。
    #[inline(never)]
    fn cards(&mut self, tokens: &Tokens, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let entity = cx.entity();
        let mut cards = vec![self.connection_card(&entity, tokens, cx)];
        if self.saved.is_some() {
            cards.push(self.password_card(&entity, tokens, cx));
            cards.push(self.sync_card(&entity, tokens, cx));
            cards.push(self.conflicts_card(&entity, tokens));
        }
        // 云端备份（U33）：不依赖主同步，没配置主同步也能单独用。
        cards.push(self.cloud_backup_card(&entity, tokens, cx));
        cards
    }

    pub(super) fn busy(&self) -> bool {
        self.testing || self.syncing || self.signing_in || self.cloud.is_busy()
    }

    // ---- 连接存储 ----

    #[inline(never)]
    fn connection_card(&self, entity: &Entity<Self>, tokens: &Tokens, cx: &App) -> AnyElement {
        let mut rows: Vec<AnyElement> = vec![self.provider_row(entity, tokens)];
        rows.push(match self.drafts.provider {
            ProviderChoice::WebDav => self.webdav_rows(entity, tokens, cx),
            ProviderChoice::S3 => self.s3_rows(entity, tokens),
            ProviderChoice::Github => self.github_rows(entity, tokens, cx),
            ProviderChoice::OneDrive => self.onedrive_rows(entity, tokens, cx),
        });
        rows.push(self.connection_buttons(entity));
        if !self.status.is_empty() {
            rows.push(
                div()
                    .id("sync-status-text")
                    .text_sm()
                    .child(self.status.clone())
                    .into_any_element(),
            );
        }
        if let Some(probe) = self.probe.as_ref() {
            rows.push(self.probe_rows(&probe_text(probe), tokens));
        }
        if let Some((ok, text)) =
            concurrency_notice(self.saved.is_some(), self.concurrency.as_ref())
        {
            rows.push(
                div()
                    .id("sync-concurrency-notice")
                    .child(colored(
                        text,
                        if ok {
                            tokens.dot_connected
                        } else {
                            tokens.dot_connecting
                        },
                    ))
                    .into_any_element(),
            );
        }
        card(
            "sync-connection",
            t(SyncViewMsg::ConnectionCardTitle),
            Some(t(SyncViewMsg::ConnectionCardDescription)),
            tokens,
            rows,
        )
    }

    fn provider_row(&self, entity: &Entity<Self>, tokens: &Tokens) -> AnyElement {
        const CHOICES: [ProviderChoice; 4] = [
            ProviderChoice::WebDav,
            ProviderChoice::S3,
            ProviderChoice::Github,
            ProviderChoice::OneDrive,
        ];
        let current = self.drafts.provider;
        let target = entity.clone();
        v_flex()
            .w_full()
            .gap_1()
            .child(label_above(t(SyncViewMsg::StorageType), tokens))
            .child(crate::main_window::widgets::segmented_bar(
                "sync-provider",
                CHOICES
                    .iter()
                    .map(|choice| (choice.label().to_owned(), *choice == current, None))
                    .collect(),
                tokens,
                move |index, _, cx| {
                    if let Some(choice) = CHOICES.get(index).copied() {
                        target.update(cx, |this, cx| this.switch_provider(choice, cx));
                    }
                },
            ))
            .into_any_element()
    }

    #[inline(never)]
    fn webdav_rows(&self, entity: &Entity<Self>, tokens: &Tokens, cx: &App) -> AnyElement {
        let mut rows = vec![
            field(t(SyncViewMsg::WebdavAddress), &self.url, tokens),
            muted(t(SyncViewMsg::WebdavAddressExample), tokens),
            field(t(SyncViewMsg::Username), &self.username, tokens),
            password_field(t(SyncViewMsg::Password), &self.password, tokens),
            field(t(SyncViewMsg::Directory), &self.directory, tokens),
            muted(t(SyncViewMsg::DirectoryHint), tokens),
        ];
        if self.webdav_has_secret {
            rows.push(muted(t(SyncViewMsg::SavedPasswordHint), tokens));
        }
        if is_plain_http(self.url.read(cx).value().as_ref()) {
            let allow = entity.clone();
            rows.push(banner(
                tokens.dot_connecting,
                vec![
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(tokens.caution)
                        .child(t(SyncViewMsg::PlainHttpTitle))
                        .into_any_element(),
                    div()
                        .child(t(SyncViewMsg::PlainHttpBody))
                        .into_any_element(),
                    Checkbox::new("sync-allow-http")
                        .checked(self.allow_insecure_http)
                        .label(t(SyncViewMsg::PlainHttpCheck))
                        .on_click(move |_, _, cx| {
                            allow.update(cx, |this, cx| {
                                this.allow_insecure_http = !this.allow_insecure_http;
                                cx.notify();
                            });
                        })
                        .into_any_element(),
                ],
            ));
        }
        v_flex().w_full().gap_2().children(rows).into_any_element()
    }

    #[inline(never)]
    fn s3_rows(&self, entity: &Entity<Self>, tokens: &Tokens) -> AnyElement {
        let style = entity.clone();
        let path_style = self.drafts.s3.path_style;
        let mut rows = vec![
            field("Endpoint", &self.s3_endpoint, tokens),
            muted(t(SyncViewMsg::S3EndpointExample), tokens),
            field("Region", &self.s3_region, tokens),
            field("Bucket", &self.s3_bucket, tokens),
            field("Access Key", &self.s3_access_key, tokens),
            password_field("Secret", &self.s3_secret, tokens),
            field(t(SyncViewMsg::S3Prefix), &self.s3_prefix, tokens),
            v_flex()
                .w_full()
                .gap_1()
                .child(label_above(t(SyncViewMsg::AddressStyle), tokens))
                .child(
                    Button::new("sync-s3-path-style")
                        .label(if path_style {
                            t(SyncViewMsg::PathStyle)
                        } else {
                            t(SyncViewMsg::VirtualHostStyle)
                        })
                        .compact()
                        .on_click(move |_, _, cx| {
                            style.update(cx, |this, cx| {
                                this.drafts.s3.path_style = !this.drafts.s3.path_style;
                                cx.notify();
                            });
                        }),
                )
                .into_any_element(),
        ];
        if self.s3_has_secret {
            rows.push(muted(t(SyncViewMsg::SavedSecretHint), tokens));
        }
        v_flex().w_full().gap_2().children(rows).into_any_element()
    }

    #[inline(never)]
    fn github_rows(&self, entity: &Entity<Self>, tokens: &Tokens, cx: &App) -> AnyElement {
        let mut rows = vec![
            field(t(SyncViewMsg::ApiBase), &self.github_api_base, tokens),
            muted(t(SyncViewMsg::ApiBaseHint), tokens),
            password_field(t(SyncViewMsg::Token), &self.github_token, tokens),
            muted(t(SyncViewMsg::TokenHint), tokens),
            field("Gist ID", &self.github_gist_id, tokens),
            muted(t(SyncViewMsg::GistIdHint), tokens),
        ];
        if self.github_has_secret {
            rows.push(muted(t(SyncViewMsg::SavedTokenHint), tokens));
        }
        let base = self.github_api_base.read(cx).value().to_string();
        if is_plain_http(&base) {
            let allow = entity.clone();
            rows.push(banner(
                tokens.dot_connecting,
                vec![
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(tokens.caution)
                        .child(t(SyncViewMsg::TokenHttpTitle))
                        .into_any_element(),
                    Checkbox::new("sync-github-allow-http")
                        .checked(self.github_allow_insecure_http)
                        .label(t(SyncViewMsg::TokenHttpCheck))
                        .on_click(move |_, _, cx| {
                            allow.update(cx, |this, cx| {
                                this.github_allow_insecure_http = !this.github_allow_insecure_http;
                                cx.notify();
                            });
                        })
                        .into_any_element(),
                ],
            ));
        }
        v_flex().w_full().gap_2().children(rows).into_any_element()
    }

    /// OneDrive：说明、登录 / 取消、已登录的账号和断开、撤销授权的提示、「高级」里的 client ID。
    #[inline(never)]
    fn onedrive_rows(&self, entity: &Entity<Self>, tokens: &Tokens, cx: &App) -> AnyElement {
        let busy = self.busy();
        let signed_in = matches!(self.saved, Some(StorageConfig::Onedrive(_)));
        let custom = self.onedrive_client_id.read(cx).value().to_string();
        let allowed = can_login(&custom);
        let custom_invalid = !custom.trim().is_empty()
            && !zssh_core::sync::onedrive::config::is_valid_client_id(&custom);
        let mut rows: Vec<AnyElement> = vec![
            banner(
                tokens.text_muted,
                vec![
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .child(t(SyncViewMsg::SignInWithMicrosoft))
                        .into_any_element(),
                    div()
                        .child(t(SyncViewMsg::OneDriveIntro))
                        .into_any_element(),
                ],
            ),
            label_row(
                t(SyncViewMsg::Account),
                onedrive_account_label(self.saved.as_ref()),
                tokens,
            ),
        ];
        if signed_in {
            rows.push(muted(t(SyncViewMsg::OneDriveTokenHint), tokens));
            if with_sync_guard(|guard| matches!(guard.pause, Some(super::AutoPause::Auth(_)))) {
                rows.push(banner(
                    tokens.danger,
                    vec![colored(t(SyncViewMsg::OneDriveExpired), tokens.danger)],
                ));
            }
        }
        if self.signing_in {
            let cancel = entity.clone();
            rows.push(banner(
                tokens.text_muted,
                vec![
                    div()
                        .id("onedrive-logging-in")
                        .font_weight(FontWeight::MEDIUM)
                        .child(t(SyncViewMsg::LoggingInBrowser))
                        .into_any_element(),
                    div()
                        .child(t(SyncViewMsg::LoggingInHint))
                        .into_any_element(),
                    Button::new("sync-onedrive-login-cancel")
                        .label(t(SyncViewMsg::CancelLogin))
                        .compact()
                        .on_click(move |_, _, cx| {
                            cancel.update(cx, |this, cx| this.cancel_onedrive_login(cx));
                        })
                        .into_any_element(),
                ],
            ));
        } else {
            let login = entity.clone();
            let mut buttons = h_flex().gap_2().items_center().child(
                Button::new("sync-onedrive-login")
                    .label(if signed_in {
                        t(SyncViewMsg::Relogin)
                    } else {
                        t(SyncViewMsg::SignInWithMicrosoft)
                    })
                    .compact()
                    .disabled(busy || !allowed)
                    .on_click(move |_, _, cx| {
                        login.update(cx, |this, cx| this.start_onedrive_login(cx));
                    }),
            );
            if signed_in {
                let logout = entity.clone();
                buttons = buttons.child(
                    Button::new("sync-onedrive-logout")
                        .label(if self.logout_confirm.is_armed() {
                            t(SyncViewMsg::ConfirmDisconnect)
                        } else {
                            t(SyncViewMsg::Disconnect)
                        })
                        .compact()
                        .disabled(busy)
                        .on_click(move |_, window, cx| {
                            logout.update(cx, |this, cx| this.onedrive_logout(window, cx));
                        }),
                );
            }
            rows.push(buttons.into_any_element());
            if custom_invalid {
                rows.push(colored(
                    t(SyncViewMsg::ClientIdInvalid),
                    tokens.dot_connecting,
                ));
            }
        }
        // 撤销授权：断开只删本机的登录信息，要彻底撤销得去微软账户的「应用和服务」页面。
        let revoke = entity.clone();
        rows.push(
            v_flex()
                .id("onedrive-revoke-hint")
                .gap_1()
                .child(muted(t(SyncViewMsg::DisconnectHint), tokens))
                .child(
                    h_flex().child(
                        Button::new("sync-onedrive-revoke")
                            .label(t(SyncViewMsg::OpenAppsPage))
                            .compact()
                            .on_click(move |_, _, cx| {
                                revoke.update(cx, |this, cx| this.open_revoke_page(cx));
                            }),
                    ),
                )
                .into_any_element(),
        );
        // 「高级」：自己的 client ID。
        let toggle = entity.clone();
        rows.push(
            v_flex()
                .gap_1()
                .child(
                    h_flex().child(
                        Button::new("sync-onedrive-advanced")
                            .label(if self.onedrive_advanced {
                                t(SyncViewMsg::AdvancedExpanded)
                            } else {
                                t(SyncViewMsg::AdvancedCollapsed)
                            })
                            .ghost()
                            .compact()
                            .on_click(move |_, _, cx| {
                                toggle.update(cx, |this, cx| this.toggle_onedrive_advanced(cx));
                            }),
                    ),
                )
                .when(self.onedrive_advanced, |col| {
                    col.child(field(
                        t(SyncViewMsg::OwnClientId),
                        &self.onedrive_client_id,
                        tokens,
                    ))
                    .child(muted(t(SyncViewMsg::OwnClientIdHint), tokens))
                })
                .into_any_element(),
        );
        v_flex().w_full().gap_2().children(rows).into_any_element()
    }

    /// 保存 / 测试连接 / 清除。OneDrive 的连接由登录保存，没有「保存」；测试连接和清除照常。
    fn connection_buttons(&self, entity: &Entity<Self>) -> AnyElement {
        let busy = self.busy();
        let onedrive = self.drafts.provider == ProviderChoice::OneDrive;
        let clear_label = if self.clear_confirm.is_armed() {
            t(SyncViewMsg::ConfirmClearAgain)
        } else {
            t(SyncViewMsg::Clear)
        };
        h_flex()
            .gap_2()
            .items_center()
            .when(!onedrive, |row| {
                row.child(act_button(
                    "sync-save",
                    t(SyncViewMsg::Save),
                    entity,
                    busy,
                    |this, window, cx| this.save(window, cx),
                ))
            })
            .child(act_button(
                "sync-test",
                if self.testing {
                    t(SyncViewMsg::Testing)
                } else {
                    t(SyncViewMsg::TestConnection)
                },
                entity,
                busy,
                |this, _, cx| this.start_test(cx),
            ))
            .child(act_button(
                "sync-clear",
                clear_label,
                entity,
                busy,
                |this, window, cx| this.clear(window, cx),
            ))
            .into_any_element()
    }

    /// 最近一次测试连接的结果：读 / 写 / 并发保护分别显示（✓ / ✗）。
    fn probe_rows(&self, text: &str, tokens: &Tokens) -> AnyElement {
        v_flex()
            .id("sync-probe")
            .gap_1()
            .child(muted(t(SyncViewMsg::LastProbe), tokens))
            .children(text.lines().map(|line| {
                // 测试结果文字是生成那一刻的语言写的；之后切了语言，这里仍要认得出，所以中英两种写法都认。
                let has = |key: SyncMsg| line.contains(key.zh()) || line.contains(key.en());
                let begins =
                    |key: SyncMsg| line.starts_with(key.zh()) || line.starts_with(key.en());
                let is = |key: SyncMsg| line == key.zh() || line == key.en();
                let failed = has(SyncMsg::CheckFailedMiddle);
                let warn = begins(SyncMsg::ConcurrencyPrefix)
                    && (has(SyncMsg::ConcurrencyReadBeforeWrite)
                        || has(SyncMsg::ConcurrencyNotChecked));
                // 缩进的说明行和「云端：…」不是检查项，不打勾也不上色。
                let detail = begins(SyncMsg::ConcurrencyNotePrefix)
                    || is(SyncMsg::CloudHasData)
                    || is(SyncMsg::CloudEmpty);
                let color = if failed {
                    tokens.danger
                } else if warn {
                    tokens.dot_connecting
                } else if detail {
                    tokens.text_muted
                } else {
                    tokens.dot_connected
                };
                let mark = if failed {
                    "✗ "
                } else if warn {
                    "⚠ "
                } else if detail {
                    ""
                } else {
                    "✓ "
                };
                colored(format!("{mark}{line}"), color)
            }))
            .into_any_element()
    }

    // ---- 主密码 ----

    #[inline(never)]
    fn password_card(&self, entity: &Entity<Self>, tokens: &Tokens, cx: &App) -> AnyElement {
        let busy = self.busy();
        let creating = self.needs_password_confirm();
        let mut rows: Vec<AnyElement> = Vec::new();
        if creating {
            rows.push(banner(
                tokens.danger,
                vec![
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(tokens.danger)
                        .child(t(SyncViewMsg::ForgotWarningTitle))
                        .into_any_element(),
                    div()
                        .child(t(SyncViewMsg::ForgotWarningBody))
                        .into_any_element(),
                ],
            ));
        } else if !self.ready {
            rows.push(muted(t(SyncViewMsg::CloudHasDataHint), tokens));
        }
        rows.push(password_field(
            t(SyncViewMsg::MasterPassword),
            &self.master_password,
            tokens,
        ));
        let typed = self.master_password.read(cx).value().to_string();
        if creating {
            rows.extend(strength_bar(&typed, tokens));
            rows.push(password_field(
                t(SyncViewMsg::PasswordAgain),
                &self.master_password_confirm,
                tokens,
            ));
            let flip = entity.clone();
            rows.push(
                Checkbox::new("sync-understood")
                    .checked(self.understood)
                    .label(t(SyncViewMsg::UnderstoodCheck))
                    .on_click(move |_, _, cx| {
                        flip.update(cx, |this, cx| this.toggle_understood(cx));
                    })
                    .into_any_element(),
            );
        }
        // 记住主密码：F5 的语义（同步成功才真正写盘 / 清除），开关旁边的提示说清当前状态。
        let remember = entity.clone();
        rows.push(setting(
            t(SyncViewMsg::RememberTitle),
            format!(
                "{}{}",
                t(SyncViewMsg::RememberDescriptionPrefix),
                self.remember.hint()
            ),
            Switch::new("sync-remember-master-password")
                .checked(self.remember.toggled)
                .disabled(busy)
                .accessibility_label(t(SyncViewMsg::RememberA11y))
                .on_click(move |_, _, cx| {
                    remember.update(cx, |this, cx| this.toggle_remember(cx));
                }),
            tokens,
        ));
        rows.push(muted(t(SyncViewMsg::MasterPasswordHint), tokens));
        if !creating && self.ready {
            rows.push(self.change_password_rows(entity, tokens, cx));
        }
        card(
            "sync-password",
            t(SyncViewMsg::MasterPasswordCardTitle),
            Some(t(SyncViewMsg::MasterPasswordCardDescription)),
            tokens,
            rows,
        )
    }

    /// 「修改主密码」：一行入口，点开后是旧密码 / 新密码 / 确认三个框。
    fn change_password_rows(&self, entity: &Entity<Self>, tokens: &Tokens, cx: &App) -> AnyElement {
        let busy = self.busy() || self.change_pw.busy;
        let open = entity.clone();
        let entry = setting(
            t(SyncViewMsg::ChangeMasterPassword),
            t(SyncViewMsg::ChangeMasterPasswordDescription),
            Button::new("sync-change-password-open")
                .label(t(SyncViewMsg::Change))
                .compact()
                .disabled(busy || self.change_pw.open)
                .on_click(move |_, _, cx| {
                    open.update(cx, |this, cx| this.open_change_password(cx));
                }),
            tokens,
        );
        if !self.change_pw.open {
            return entry;
        }
        let new_text = self.change_pw.new.read(cx).value().to_string();
        let mut form: Vec<AnyElement> = vec![
            password_field(
                t(SyncViewMsg::OldMasterPassword),
                &self.change_pw.old,
                tokens,
            ),
            password_field(
                t(SyncViewMsg::NewMasterPassword),
                &self.change_pw.new,
                tokens,
            ),
        ];
        form.extend(strength_bar(&new_text, tokens));
        form.push(password_field(
            t(SyncViewMsg::ConfirmNewMasterPassword),
            &self.change_pw.confirm,
            tokens,
        ));
        form.push(muted(t(SyncViewMsg::ChangeWarning), tokens));
        if let Some(error) = &self.change_pw.error {
            form.push(
                div()
                    .id("sync-change-password-error")
                    .child(colored(error.clone(), tokens.danger))
                    .into_any_element(),
            );
        }
        form.push(
            h_flex()
                .gap_2()
                .child(act_button(
                    "sync-change-password-submit",
                    if self.change_pw.busy {
                        t(SyncViewMsg::Changing)
                    } else {
                        t(SyncViewMsg::ChangeAndUpload)
                    },
                    entity,
                    busy,
                    |this, _, cx| this.submit_change_password(cx),
                ))
                .child(act_button(
                    "sync-change-password-cancel",
                    t(SyncViewMsg::Cancel),
                    entity,
                    self.change_pw.busy,
                    |this, window, cx| this.close_change_password(window, cx),
                ))
                .into_any_element(),
        );
        v_flex()
            .w_full()
            .gap_2()
            .child(entry)
            .child(
                v_flex()
                    .id("sync-change-password-form")
                    .w_full()
                    .gap_2()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(tokens.border)
                    .children(form),
            )
            .into_any_element()
    }

    // ---- 同步 ----

    #[inline(never)]
    fn sync_card(&self, entity: &Entity<Self>, tokens: &Tokens, cx: &App) -> AnyElement {
        let busy = self.busy();
        let mut rows: Vec<AnyElement> = Vec::new();
        let sync_label = if self.syncing {
            t(SyncViewMsg::Syncing)
        } else {
            t(SyncViewMsg::SyncNow)
        };
        rows.push(setting(
            t(SyncViewMsg::SyncNow),
            last_result_text(self.auto.last.as_ref()),
            act_button(
                "sync-now",
                sync_label,
                entity,
                busy || self.baseline_broken,
                |this, window, cx| this.start_sync(window, cx),
            ),
            tokens,
        ));
        if let Some(error) = &self.sync_state.error {
            rows.push(
                div()
                    .id("sync-error")
                    .child(colored(error.clone(), tokens.danger))
                    .into_any_element(),
            );
        }
        if self.sync_state.needs_confirmation() {
            rows.push(self.deletion_panel(entity, tokens, busy));
        }
        if self.baseline_broken {
            rows.push(self.baseline_panel(entity, tokens, busy));
        }
        if let Some(report) = self.sync_state.report.as_ref() {
            let lines = sync_report_lines(report);
            if !lines.is_empty() {
                rows.push(
                    v_flex()
                        .id("sync-report")
                        .gap_0p5()
                        .p_2()
                        .rounded_md()
                        .border_1()
                        .border_color(tokens.border)
                        .child(div().text_sm().child(t(SyncViewMsg::SyncResults)))
                        .children(lines.into_iter().map(|line| muted(line, tokens)))
                        .into_any_element(),
                );
            }
        }
        rows.push(self.auto_rows(entity, tokens, cx));
        card(
            "sync-run",
            t(SyncViewMsg::SyncCardTitle),
            Some(t(SyncViewMsg::SyncCardDescription)),
            tokens,
            rows,
        )
    }

    /// 防误删确认：会删多少、前几条是什么，三个选择（0.2.14 的 `DeletionDialog`）。
    fn deletion_panel(&self, entity: &Entity<Self>, tokens: &Tokens, busy: bool) -> AnyElement {
        let plan = self
            .sync_state
            .report
            .as_ref()
            .map(|report| report.deletion.clone());
        let mut body: Vec<AnyElement> = vec![
            div()
                .font_weight(FontWeight::MEDIUM)
                .text_color(tokens.caution)
                .child(t(SyncViewMsg::DeleteManyTitle))
                .into_any_element(),
            div()
                .child(t(SyncViewMsg::NothingChangedYet))
                .into_any_element(),
        ];
        if let Some(plan) = plan {
            body.push(
                div()
                    .id("sync-pending-summary")
                    .child(plan.summary())
                    .into_any_element(),
            );
            for line in deletion_lines(&plan) {
                body.push(muted(line, tokens));
            }
        }
        body.push(muted(t(SyncViewMsg::DeletionChoices), tokens));
        body.push(
            h_flex()
                .gap_2()
                .child(act_button(
                    "sync-delete-cancel",
                    t(SyncViewMsg::Cancel),
                    entity,
                    busy,
                    |this, _, cx| this.cancel_deletion(cx),
                ))
                .child(act_button(
                    "sync-delete-use-cloud",
                    t(SyncViewMsg::UseCloudVersion),
                    entity,
                    busy,
                    |this, _, cx| this.confirm_deletion_use_cloud(cx),
                ))
                .child(
                    act_button(
                        "sync-delete-continue",
                        t(SyncViewMsg::Continue),
                        entity,
                        busy,
                        |this, _, cx| this.confirm_deletion(cx),
                    )
                    .danger(),
                )
                .into_any_element(),
        );
        div()
            .id("sync-pending")
            .child(banner(tokens.dot_connecting, body))
            .into_any_element()
    }

    /// 基线损坏：同步停下来，等用户重建（两步确认）。
    fn baseline_panel(&self, entity: &Entity<Self>, tokens: &Tokens, busy: bool) -> AnyElement {
        div()
            .id("sync-baseline-broken")
            .child(banner(
                tokens.danger,
                vec![
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(tokens.danger)
                        .child(t(SyncViewMsg::BaselineBrokenTitle))
                        .into_any_element(),
                    div()
                        .child(t(SyncViewMsg::BaselineBrokenBody))
                        .into_any_element(),
                    h_flex()
                        .child(act_button(
                            "sync-reset-baseline",
                            if self.baseline_reset_armed {
                                t(SyncViewMsg::ConfirmRebuild)
                            } else {
                                t(SyncViewMsg::RebuildBaseline)
                            },
                            entity,
                            busy,
                            |this, _, cx| this.reset_baseline(cx),
                        ))
                        .into_any_element(),
                ],
            ))
            .into_any_element()
    }

    /// 自动同步：开关、间隔、暂停原因。
    #[inline(never)]
    fn auto_rows(&self, entity: &Entity<Self>, tokens: &Tokens, _cx: &App) -> AnyElement {
        let busy = self.busy();
        let enabled = self.auto.settings.enabled;
        let toggle = entity.clone();
        let pause = enabled
            .then(|| {
                auto_block_reason(
                    true,
                    self.remember.saved,
                    self.saved.is_some(),
                    self.ready,
                    self.baseline_broken,
                    with_sync_guard(|guard| guard.block_reason(Instant::now())),
                )
            })
            .flatten();
        let mut rows: Vec<AnyElement> = vec![
            setting(
                t(SyncViewMsg::AutoSync),
                t(SyncViewMsg::AutoSyncDescription),
                Switch::new("sync-auto-toggle")
                    .checked(enabled)
                    .disabled(busy)
                    .accessibility_label(t(SyncViewMsg::AutoSyncA11y))
                    .on_click(move |_, window, cx| {
                        toggle.update(cx, |this, cx| this.toggle_auto(window, cx));
                    }),
                tokens,
            ),
            setting(
                t(SyncViewMsg::Interval),
                t(SyncViewMsg::IntervalDescription),
                Input::new(&self.auto_interval).w(px(96.0)),
                tokens,
            ),
        ];
        if let Some(reason) = pause {
            rows.push(
                div()
                    .id("sync-auto-pause")
                    .child(colored(
                        format!("{}{reason}", t(SyncViewMsg::AutoSyncPausedPrefix)),
                        tokens.dot_connecting,
                    ))
                    .into_any_element(),
            );
        }
        if let Some(error) = &self.auto.error {
            rows.push(colored(error.clone(), tokens.danger));
        }
        rows.push(muted(t(SyncViewMsg::AutoSyncSkipsHint), tokens));
        v_flex().w_full().gap_1().children(rows).into_any_element()
    }

    // ---- 冲突 ----

    #[inline(never)]
    fn conflicts_card(&self, entity: &Entity<Self>, tokens: &Tokens) -> AnyElement {
        let busy = self.busy();
        let rows_data: Vec<_> = self
            .conflicts
            .items
            .iter()
            .map(|view| conflict_row(view, self.conflicts.is_armed(view.id)))
            .collect();
        let mut rows: Vec<AnyElement> = Vec::new();
        if let Some(error) = &self.conflicts.error {
            rows.push(
                div()
                    .id("sync-conflict-error")
                    .child(colored(error.clone(), tokens.danger))
                    .into_any_element(),
            );
        }
        if rows_data.is_empty() && self.conflicts.error.is_none() {
            rows.push(
                div()
                    .id("conflicts-empty")
                    .child(muted(t(SyncViewMsg::NoConflicts), tokens))
                    .into_any_element(),
            );
        }
        for row in rows_data {
            let id = row.id;
            let armed = row.armed;
            let mut lines: Vec<AnyElement> = vec![
                h_flex()
                    .gap_2()
                    .items_baseline()
                    .child(
                        div()
                            .px_1p5()
                            .rounded_sm()
                            .bg(tokens.subtle)
                            .text_size(px(11.0))
                            .text_color(tokens.text_muted)
                            .child(row.entity),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child(row.title),
                    )
                    .into_any_element(),
                muted(row.kept, tokens),
                muted(row.other, tokens),
            ];
            if !row.can_use_other && !row.message.is_empty() {
                lines.push(muted(row.message, tokens));
            }
            let mut buttons = h_flex().gap_2();
            if row.can_use_other {
                buttons = buttons.child(act_button(
                    format!("sync-conflict-use-{id}"),
                    if armed {
                        t(SyncViewMsg::ConfirmUseOther)
                    } else if row.deletion {
                        t(SyncViewMsg::RestoreThisRecord)
                    } else {
                        t(SyncViewMsg::UseOther)
                    },
                    entity,
                    busy,
                    move |this, _, cx| this.click_use_other(id, cx),
                ));
            }
            if armed {
                buttons = buttons.child(act_button(
                    format!("sync-conflict-cancel-{id}"),
                    t(SyncViewMsg::Cancel),
                    entity,
                    busy,
                    |this, _, cx| this.cancel_use_other(cx),
                ));
            }
            buttons = buttons.child(
                act_button(
                    format!("sync-conflict-dismiss-{id}"),
                    t(SyncViewMsg::GotIt),
                    entity,
                    busy,
                    move |this, _, cx| this.dismiss_conflict(id, cx),
                )
                .ghost(),
            );
            lines.push(buttons.into_any_element());
            rows.push(
                v_flex()
                    .id(("sync-conflict", id as usize))
                    .gap_1()
                    .py_2()
                    .border_t_1()
                    .border_color(tokens.border)
                    .children(lines)
                    .into_any_element(),
            );
        }
        card(
            "sync-conflicts",
            t(SyncViewMsg::ConflictsCardTitle),
            Some(t(SyncViewMsg::ConflictsCardDescription)),
            tokens,
            rows,
        )
    }
}

#[cfg(test)]
mod tests {
    use zssh_core::sync::remote::ConcurrencyMode;
    use zssh_core::sync::service::{ConcurrencyRecord, LastResult};

    use super::{can_login, concurrency_notice, is_plain_http, last_result_text};

    const GUID: &str = "11111111-2222-3333-4444-555555555555";

    #[test]
    fn login_uses_the_built_in_id_unless_a_custom_one_is_filled_in_and_well_formed() {
        // 不填：用内置的，总能登录。
        assert!(can_login(""));
        assert!(can_login("   "));
        // 填了就看格式，内置的不能替它兜底。
        assert!(can_login(GUID));
        assert!(can_login(&format!("  {GUID}  ")));
        assert!(!can_login("not-a-guid"));
        assert!(!can_login("11111111-2222-3333-4444-55555555555"));
    }

    #[test]
    fn last_result_reads_like_the_old_front_end() {
        assert_eq!(last_result_text(None), "还没有同步过");
        let mut last = LastResult {
            at: 1_700_000_000,
            status: "completed".to_owned(),
            summary: String::new(),
            version: Some("v7".to_owned()),
            conflicts: 0,
            auto: true,
        };
        let text = last_result_text(Some(&last));
        assert!(text.contains("（自动） 同步成功，上传版本 v7"), "{text}");
        last.status = "error".to_owned();
        last.summary = "连接超时".to_owned();
        last.auto = false;
        assert!(last_result_text(Some(&last)).ends_with(" 失败：连接超时"));
        last.status = "needs_confirmation".to_owned();
        assert!(last_result_text(Some(&last)).ends_with("停在了防误删确认上"));
        last.status = "cancelled".to_owned();
        assert!(last_result_text(Some(&last)).ends_with(" 已取消"));
    }

    #[test]
    fn concurrency_notice_never_pretends_support() {
        assert_eq!(concurrency_notice(false, None), None);
        let (ok, text) = concurrency_notice(true, None).unwrap();
        assert!(!ok && text.contains("还没有检测"), "{text}");
        let record = |mode| ConcurrencyRecord {
            mode,
            notes: Vec::new(),
            checked_at: 0,
        };
        let (ok, text) =
            concurrency_notice(true, Some(&record(ConcurrencyMode::ReadBeforeWrite))).unwrap();
        assert!(!ok && text.contains("不支持"), "{text}");
        let (ok, text) =
            concurrency_notice(true, Some(&record(ConcurrencyMode::Conditional))).unwrap();
        assert!(ok && text.starts_with("支持并发保护"), "{text}");
    }

    #[test]
    fn plain_http_is_detected_case_insensitively() {
        assert!(is_plain_http("http://dav.local/x"));
        assert!(is_plain_http("  HTTP://dav.local"));
        assert!(!is_plain_http("https://dav.local"));
        assert!(!is_plain_http(""));
    }
}
