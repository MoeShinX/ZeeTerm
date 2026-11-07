//! 更新检查窗口：取更新清单、下载安装包、校验 minisign 签名、生成安装计划。
//!
//! 发行版：当前版本是编译进来的包版本，公钥和更新源取自 `zssh_update::embedded_config`
//! （`native/zssh-update/updater.json`），清单和安装包只允许 `https://`，
//! 下载有大小上限和总时长上限，重定向只跟到 https。联网走「软件网络」代理设置。
//! 公钥还是占位值时（见 [`SIGNING_UNCONFIGURED`]）不请求、不下载、不安装。
//! 调试版额外认 `ZSSH_UPDATE_MANIFEST`（本地清单路径或 URL）、`ZSSH_CURRENT_VERSION`、
//! `ZSSH_UPDATE_PUBKEY`、`ZSSH_UPDATE_PROXY`，并放开 `http://` / `file://`；Linux 另有 `ZSSH_UPDATE_FLAVOR`
//! （`deb` / `tar` / `windows`，盖掉对安装方式的探测）、`ZSSH_UPDATE_AUTODOWNLOAD` / `ZSSH_UPDATE_AUTOINSTALL` /
//! `ZSSH_UPDATE_AUTORESTART`（查到新版本就下载、验签后就安装、装好就重启，自动化真机验证用）；这些变量
//! 经 `dev_hooks` 读取，release 里恒为未设置。
//!
//! 安装包先整个下到内存、在内存里验签，通过后才以独占方式写进
//! `<数据目录>/update-cache/<哈希前缀>/`；安装计划带着验签时的 SHA-256，
//! 启动安装器前（含"退出后安装"）会重新算一次哈希，不一致就拒绝。
//! 后台任务经 `std::sync::mpsc` 把进度交给界面，100ms tick 取一次。
//!
//! 点一下「立即安装」就调用 `zssh_update::plan_install` 生成计划，并用 `ShellExecuteW`
//! 启动已校验的安装器；启动成功后界面退出，安装器继续运行。安装前先问
//! `install_state::blocking_reason`，还有传输 / AI 回合在跑就不启动。
//!
//! 「退出后安装」只把计划排进进程级队列，界面不退出；真正的 `ShellExecuteW`
//! 放在 `App::on_app_quit` 注册的回调里，等程序退出时再启动。
//!
//! Linux（`linux.rs` 和 `zssh_update::linux`）：检查时先判断本机怎么装的（`dpkg -S` 认得 → deb，否则 tar.gz），
//! 只选清单里对应的 `linux-x86_64-deb` / `linux-x86_64-tar`；没有适用条目时给 `CheckKind::FoundManual`，
//! 界面引导到发布页。下载、验签、版本绑定与 Windows 是同一套（`download_with`）。点「立即安装」后在后台线程里
//! 安装：deb 经 `pkexec apt-get install`，tar.gz 解压后核对新程序的版本再原子替换程序文件；没有 `pkexec`、授权
//! 被取消、目录不可写时不报错，保留已校验的包并显示手动安装的方法。装好后由用户点「立即重启」（Linux 没有"退出后安装"）。
//!
//! 界面（V2-U22 重做，只动版式，流程不变）：每种状态都是"顶部信息区 / 中间内容区 / 底部按钮区"三段。
//! 画哪一屏、标题和按钮集合见 `layout`，更新说明（Markdown）先经 `notes` 改写成安全的版本再渲染，
//! `view` 是 `Render`。「跳过此版本」把版本号写进本机设置 `update_skipped_version`，自动检查据此不再提示它。

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use ::gpui::{App, FocusHandle, Subscription, Task, Window, WindowBounds, WindowOptions};
use gpui_kit::*;
use tokio::io::AsyncReadExt as _;
use zssh_update::linux::{Flavor, ManualReason};
use zssh_update::{InstallPlan, InstallRequest, Package, UpdateError, plan_install, sha256_hex};

use crate::i18n::{Msg, t};
use crate::install_state;

// 界面分三块：`layout` 决定画哪一屏、标题和按钮（纯函数），`notes` 处理更新说明（Markdown 安全改写），
// `view` 是 `Render`。验签 / 下载 / 安装的流程都在本文件，界面改版不碰它们。
mod layout;
mod linux;
mod notes;
mod view;

pub(crate) use linux::awaiting_restart;
use linux::{Platform, host_platform};

/// 更新检查窗口的编号；IME 按窗口 key 管理。
static NEXT_UPDATE: AtomicU64 = AtomicU64::new(1);

/// 「退出后安装」排好的计划；窗口关掉后仍然有效，进程退出时统一启动。
static PENDING_INSTALL: OnceLock<Mutex<Option<InstallPlan>>> = OnceLock::new();

/// 后台事件轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// 下载进度上报间隔；界面本来就 100ms 取一次，再密没有意义。
const REPORT_INTERVAL: Duration = Duration::from_millis(100);
/// 单次读取的块大小。
const CHUNK_SIZE: usize = 64 * 1024;
/// 更新包缓存目录名。
const CACHE_DIR: &str = "update-cache";
/// 更新签名公钥还是占位值时的提示：这时没法证明安装包来自发布者，所以什么都不请求、不下载、不安装。
///
/// 这里保留中文常量给还没有走 `i18n` 的调用方（`about_view` 的「关于」页）；更新窗口自己
/// 用 [`UpdateMsg::SigningUnconfigured`]，两种语言都从这里的中文原文取词。
pub const SIGNING_UNCONFIGURED: &str = "更新签名未配置：这个构建还没有设置更新签名公钥，不能在线更新（不会下载或安装任何更新）。请到发布页下载新的安装包。"; // i18n-allow: 旧常量，界面走 signing_unconfigured()

/// 「更新签名未配置」那句，按当前语言取。「关于」页也显示它，所以不走模块内部的键。
pub(crate) fn signing_unconfigured() -> &'static str {
    crate::i18n::t(UpdateMsg::SigningUnconfigured)
}

/// macOS：还没有自动更新（关于页和更新窗口说的是同一句）。
#[cfg(any(target_os = "macos", test))]
pub(crate) fn macos_no_auto_update() -> &'static str {
    crate::i18n::t(UpdateMsg::MacosNoAutoUpdate)
}
/// 安装包大小上限。清单里没有大小字段，用一个宽松但有限的值挡住"无限流"和撑满内存。
const MAX_PACKAGE_BYTES: u64 = 512 * 1024 * 1024;
/// 清单大小上限。
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
/// 下载安装包的总时长上限（含读完响应体）。
const PACKAGE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// 取清单的总时长上限。
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(30);
/// 最多跟随的重定向次数。
const MAX_REDIRECTS: usize = 5;
/// 启动安装器后等界面画出结果再退出。
const EXIT_DELAY: Duration = Duration::from_millis(300);

/// 更新窗口自己的界面文案：窗口标题、按钮、状态行、进度行和错误提示。
///
/// 只放**应用自己的**文字；清单里的更新说明正文、版本号、URL、文件名和路径属于远端 / 用户数据，
/// 原样显示，不经过这里。切语言即时生效（`t` 每次按当前语言取词）。
enum UpdateMsg {
    // ---- 窗口标题 / 主标题 / 副标题 ----
    WindowTitle,
    CheckingTitle,
    CheckFailedTitle,
    LatestVersion,
    FoundVersionPrefix,
    DownloadingTitlePrefix,
    VerifyingSignature,
    ReadyToInstallPrefix,
    UpdateToVersionPrefix,
    UpdateToVersionSuffix,
    CurrentVersionPrefix,
    PublishedSuffix,
    // ---- 按钮 ----
    CloseButton,
    RetryButton,
    SkipVersionButton,
    LaterButton,
    DownloadAndInstallButton,
    CancelButton,
    InstallNowButton,
    // ---- 正文提示 ----
    UpdateSourceTooltipPrefix,
    UpdateSourceTooltipSuffix,
    RecheckTooltip,
    CheckingMessage,
    LatestMessage,
    DownloadCompleteVerifying,
    SignatureVerifiedLabel,
    InstallerStartedNotice,
    NotesHeading,
    RestartAfterInstallCheckbox,
    EmptyNotes,
    ImagePlaceholder,
    // ---- 状态行 / 进度行 ----
    DownloadedPrefix,
    UnknownFile,
    FileSummaryPrefix,
    FileSizeSuffix,
    OpenParen,
    CloseParen,
    SignatureCheckPassed,
    DownloadCancelled,
    QueuedMessage,
    // ---- 失败原因 ----
    CheckFailedPrefix,
    StartDownloadFailedPrefix,
    PrepareInstallFailedPrefix,
    LaunchInstallerFailedPrefix,
    UpdateUrlInvalidPrefix,
    UpdateUrlNotLocalFile,
    UpdateUrlNoOtherHosts,
    UpdateUrlMustBeHttpsPrefix,
    TooManyRedirects,
    RedirectToNonHttps,
    HttpNotFound,
    HttpForbiddenPrefix,
    HttpTooManyRequests,
    HttpServerErrorPrefix,
    HttpServerErrorSuffix,
    DownloadFailedHttpPrefix,
    DownloadFailedPrefix,
    ErrorChainSeparator,
    TooLargePrefix,
    TooLargeSuffix,
    ReadLocalPackageFailedPrefix,
    DebugSourceLabel,
    OfficialSourceLabel,
    CancelledShort,
    NotesNotUtf8,
    NoUpdateSource,
    ReadManifestFailedPrefix,
    VerifyPackageFailedPrefix,
    CreateCacheDirFailedPrefix,
    CleanOldPackageFailedPrefix,
    WritePackageFailedPrefix,
    RememberSkipFailedPrefix,
    PackageChangedRefusedPrefix,
    /// 只有 Windows 的安装路径会用；保留键值方便翻译表对齐。
    #[cfg_attr(not(windows), allow(dead_code))]
    ShellExecuteErrorPrefix,
    /// 只有非 Windows 构建会用到（Windows 上走 `ShellExecuteW`）。
    #[cfg(not(windows))]
    InstallerWindowsOnly,
    UnsupportedUrlSchemePrefix,
    UrlWithoutFileName,
    InvalidFileNamePrefix,
    EmptyFileName,
    OpenSettingsDbFailedPrefix,
    ProxySettingsProblemPrefix,
    UpdateProxyConfigErrorPrefix,
    CreateDownloadClientFailedPrefix,
    PubkeyUnavailable,
    SigningUnconfigured,
    /// 只有 macOS 构建会用到（还没有 macOS 的发布包，也不能装 Windows 的）。
    #[cfg(any(target_os = "macos", test))]
    MacosNoAutoUpdate,
    NoReleaseNotes,
    PackageLabel,
    PlatformLabel,
    // ---- Linux：按钮 / 标题 / 正文 / 失败原因 ----
    RestartNowButton,
    RestartLaterButton,
    OpenReleasePageButton,
    RevealFileButton,
    InstallingTitle,
    InstalledTitlePrefix,
    ManualTitlePrefix,
    InstallingDebMessage,
    InstallingTarMessage,
    InstalledMessage,
    NoPackageMessage,
    ManualReasonNoPkexec,
    ManualReasonNotAuthorized,
    ManualReasonNotWritable,
    ManualDebHowTo,
    ManualTarHowTo,
    LinuxInstallFailedPrefix,
    DebInstallFailedPrefix,
    DebHashChanged,
    DebWrongPackage,
    DebNotRegularFile,
    DebTooLarge,
    RestartFailedPrefix,
}

impl Msg for UpdateMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::WindowTitle => "检查更新",
            Self::CheckingTitle => "正在检查更新…",
            Self::CheckFailedTitle => "检查更新失败",
            Self::LatestVersion => "已是最新版本",
            Self::FoundVersionPrefix => "发现新版本 ",
            Self::DownloadingTitlePrefix => "正在下载 ",
            Self::VerifyingSignature => "正在校验签名…",
            Self::ReadyToInstallPrefix => "已准备好安装 ",
            Self::UpdateToVersionPrefix => "更新到 ",
            Self::UpdateToVersionSuffix => " 失败",
            Self::CurrentVersionPrefix => "当前版本 ",
            Self::PublishedSuffix => " · 发布于 ",
            Self::CloseButton => "关闭",
            Self::RetryButton => "重试",
            Self::SkipVersionButton => "跳过此版本",
            Self::LaterButton => "稍后",
            Self::DownloadAndInstallButton => "下载并安装",
            Self::CancelButton => "取消",
            Self::InstallNowButton => "立即安装",
            Self::UpdateSourceTooltipPrefix => "更新来自",
            Self::UpdateSourceTooltipSuffix => {
                "。安装包下载后会先校验签名，点「立即安装」后才会启动安装程序。"
            }
            Self::RecheckTooltip => "重新检查",
            Self::CheckingMessage => "正在向发布页询问最新版本…",
            Self::LatestMessage => "你用的已经是最新版本，无需更新。",
            Self::DownloadCompleteVerifying => "下载完成，正在校验签名…",
            Self::SignatureVerifiedLabel => "签名已验证",
            Self::InstallerStartedNotice => "安装器已启动，程序即将退出",
            Self::NotesHeading => "更新内容",
            Self::RestartAfterInstallCheckbox => "安装后重启 ZeeTerm",
            Self::EmptyNotes => "这个版本没有附带更新说明",
            Self::ImagePlaceholder => "（图片）",
            Self::DownloadedPrefix => "已下载 ",
            Self::UnknownFile => "（未知文件）",
            Self::FileSummaryPrefix => "文件：",
            Self::FileSizeSuffix => " 字节",
            Self::OpenParen => "（",
            Self::CloseParen => "）",
            Self::SignatureCheckPassed => "签名校验通过",
            Self::DownloadCancelled => "已取消下载",
            Self::QueuedMessage => "已排队：退出程序后启动安装器",
            Self::CheckFailedPrefix => "检查更新失败：",
            Self::StartDownloadFailedPrefix => "启动下载失败：",
            Self::PrepareInstallFailedPrefix => "准备安装失败：",
            Self::LaunchInstallerFailedPrefix => "启动安装器失败：",
            Self::UpdateUrlInvalidPrefix => "更新地址无效：",
            Self::UpdateUrlNotLocalFile => "更新地址不是本地文件路径",
            Self::UpdateUrlNoOtherHosts => "更新地址不能指向其他主机的文件",
            Self::UpdateUrlMustBeHttpsPrefix => "更新地址必须是 https，实际是 ",
            Self::TooManyRedirects => "更新地址重定向次数过多",
            Self::RedirectToNonHttps => "更新地址重定向到了非 https 的地址，已拒绝",
            Self::HttpNotFound => {
                "下载失败：服务器上找不到这个文件（HTTP 404），可能这个版本还没发布或已被撤回"
            }
            Self::HttpForbiddenPrefix => "下载失败：服务器拒绝访问（HTTP ",
            Self::HttpTooManyRequests => {
                "下载失败：请求太频繁，被服务器限流了（HTTP 429），稍后再试"
            }
            Self::HttpServerErrorPrefix => "下载失败：服务器出错了（HTTP ",
            Self::HttpServerErrorSuffix => "），稍后再试",
            Self::DownloadFailedHttpPrefix => "下载失败：HTTP ",
            Self::DownloadFailedPrefix => "下载失败：",
            Self::ErrorChainSeparator => "：",
            Self::TooLargePrefix => "更新文件超过大小上限（",
            Self::TooLargeSuffix => " MiB），已中止",
            Self::ReadLocalPackageFailedPrefix => "读取本地更新包失败：",
            Self::DebugSourceLabel => "调试指定的更新说明",
            Self::OfficialSourceLabel => "官方发布页",
            Self::CancelledShort => "已取消",
            Self::NotesNotUtf8 => "更新说明不是有效的 UTF-8 文本",
            Self::NoUpdateSource => "未配置更新源",
            Self::ReadManifestFailedPrefix => "读取更新说明失败：",
            Self::VerifyPackageFailedPrefix => "校验更新包失败：",
            Self::CreateCacheDirFailedPrefix => "创建更新缓存目录失败：",
            Self::CleanOldPackageFailedPrefix => "清理旧的更新包失败：",
            Self::WritePackageFailedPrefix => "写入更新包失败：",
            Self::RememberSkipFailedPrefix => "没能记住跳过：",
            Self::PackageChangedRefusedPrefix => "安装包与验签时不一致，已拒绝启动：",
            Self::ShellExecuteErrorPrefix => "ShellExecuteW 返回错误码 ",
            #[cfg(not(windows))]
            Self::InstallerWindowsOnly => "只支持在 Windows 上启动安装器",
            Self::UnsupportedUrlSchemePrefix => "不支持的更新地址协议：",
            Self::UrlWithoutFileName => "更新地址里没有可用的文件名",
            Self::InvalidFileNamePrefix => "更新包文件名不合法：",
            Self::EmptyFileName => "更新包文件名为空",
            Self::OpenSettingsDbFailedPrefix => "打开设置库失败：",
            Self::ProxySettingsProblemPrefix => "软件网络的代理设置有问题：",
            Self::UpdateProxyConfigErrorPrefix => "更新代理配置错误：",
            Self::CreateDownloadClientFailedPrefix => "创建下载客户端失败：",
            Self::PubkeyUnavailable => "内置的更新公钥不可用，无法校验更新包",
            Self::SigningUnconfigured => SIGNING_UNCONFIGURED, // i18n-allow: 与 e2e 测试共用的中文原文
            #[cfg(any(target_os = "macos", test))]
            Self::MacosNoAutoUpdate => {
                "macOS 版暂不支持自动更新，也还没有 macOS 的发布包：ZeeTerm 不会下载或启动任何安装程序。"
            }
            Self::NoReleaseNotes => "（无更新说明）",
            Self::PackageLabel => "安装包：",
            Self::PlatformLabel => "平台：",
            Self::RestartNowButton => "立即重启",
            Self::RestartLaterButton => "稍后重启",
            Self::OpenReleasePageButton => "打开发布页",
            Self::RevealFileButton => "在文件管理器中显示",
            Self::InstallingTitle => "正在安装更新…",
            Self::InstalledTitlePrefix => "已安装 ",
            Self::ManualTitlePrefix => "已下载 ",
            Self::InstallingDebMessage => {
                "正在通过系统软件包管理器安装。请在系统弹出的授权窗口里确认。"
            }
            Self::InstallingTarMessage => "正在替换程序文件…",
            Self::InstalledMessage => "更新已安装。重启 ZeeTerm 后生效。",
            Self::NoPackageMessage => "这个版本没有适用于本机安装方式的安装包，请到发布页下载。",
            Self::ManualReasonNoPkexec => "这台电脑上没有 pkexec，没法弹出系统授权框。",
            Self::ManualReasonNotAuthorized => {
                "没有取得系统授权（授权框被取消，或者没有可用的授权代理）。"
            }
            Self::ManualReasonNotWritable => {
                "ZeeTerm 所在的目录没有写入权限，没法直接替换程序文件。"
            }
            Self::ManualDebHowTo => "安装包已下载并校验。双击它安装，或在终端运行：",
            Self::ManualTarHowTo => {
                "安装包已下载并校验。解压后运行其中的 install.sh，或自行替换程序文件："
            }
            Self::LinuxInstallFailedPrefix => "安装更新失败：",
            Self::DebInstallFailedPrefix => "安装失败，apt 的输出：\n",
            Self::DebHashChanged => {
                "安装包在验证之后被改动了，已拒绝安装（没有安装任何东西）。请重新下载。"
            }
            Self::DebWrongPackage => {
                "这个文件不是 ZeeTerm 的安装包，已拒绝安装（没有安装任何东西）。请重新下载。"
            }
            Self::DebNotRegularFile => {
                "缓存里的安装包不是普通文件（可能被换成了链接或别的东西），已拒绝安装（没有安装任何东西）。请重新下载。"
            }
            Self::DebTooLarge => "缓存里的安装包太大，已拒绝安装（没有安装任何东西）。请重新下载。",
            Self::RestartFailedPrefix => "没能重启：",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::WindowTitle => "Check for Updates",
            Self::CheckingTitle => "Checking for updates…",
            Self::CheckFailedTitle => "Update check failed",
            Self::LatestVersion => "You're up to date",
            Self::FoundVersionPrefix => "New version available: ",
            Self::DownloadingTitlePrefix => "Downloading ",
            Self::VerifyingSignature => "Verifying signature…",
            Self::ReadyToInstallPrefix => "Ready to install ",
            Self::UpdateToVersionPrefix => "Updating to ",
            Self::UpdateToVersionSuffix => " failed",
            Self::CurrentVersionPrefix => "Current version ",
            Self::PublishedSuffix => " · Published ",
            Self::CloseButton => "Close",
            Self::RetryButton => "Retry",
            Self::SkipVersionButton => "Skip This Version",
            Self::LaterButton => "Later",
            Self::DownloadAndInstallButton => "Download and Install",
            Self::CancelButton => "Cancel",
            Self::InstallNowButton => "Install Now",
            Self::UpdateSourceTooltipPrefix => "Updates come from ",
            Self::UpdateSourceTooltipSuffix => {
                ". The installer is signature-checked after download, and only starts after you click \"Install Now\"."
            }
            Self::RecheckTooltip => "Check Again",
            Self::CheckingMessage => "Asking the release page for the latest version…",
            Self::LatestMessage => "You're already on the latest version. No update is needed.",
            Self::DownloadCompleteVerifying => "Download complete, verifying signature…",
            Self::SignatureVerifiedLabel => "Signature verified",
            Self::InstallerStartedNotice => "The installer has started; the app is about to quit",
            Self::NotesHeading => "What's New",
            Self::RestartAfterInstallCheckbox => "Restart ZeeTerm after installing",
            Self::EmptyNotes => "This version has no release notes",
            Self::ImagePlaceholder => "(image)",
            Self::DownloadedPrefix => "Downloaded ",
            Self::UnknownFile => "(unknown file)",
            Self::FileSummaryPrefix => "File: ",
            Self::FileSizeSuffix => " bytes",
            Self::OpenParen => " (",
            Self::CloseParen => ")",
            Self::SignatureCheckPassed => "Signature check passed",
            Self::DownloadCancelled => "Download cancelled",
            Self::QueuedMessage => "Queued: the installer will start after you quit",
            Self::CheckFailedPrefix => "Update check failed: ",
            Self::StartDownloadFailedPrefix => "Failed to start the download: ",
            Self::PrepareInstallFailedPrefix => "Failed to prepare the installation: ",
            Self::LaunchInstallerFailedPrefix => "Failed to launch the installer: ",
            Self::UpdateUrlInvalidPrefix => "Invalid update address: ",
            Self::UpdateUrlNotLocalFile => "The update address is not a local file path",
            Self::UpdateUrlNoOtherHosts => {
                "The update address cannot point to a file on another host"
            }
            Self::UpdateUrlMustBeHttpsPrefix => "The update address must be https, but it is ",
            Self::TooManyRedirects => "The update address redirected too many times",
            Self::RedirectToNonHttps => {
                "The update address redirected to a non-https address; refused"
            }
            Self::HttpNotFound => {
                "Download failed: the file was not found on the server (HTTP 404); it may not be released yet or may have been withdrawn"
            }
            Self::HttpForbiddenPrefix => "Download failed: the server refused access (HTTP ",
            Self::HttpTooManyRequests => {
                "Download failed: too many requests, the server is rate limiting (HTTP 429); try again later"
            }
            Self::HttpServerErrorPrefix => "Download failed: the server had an error (HTTP ",
            Self::HttpServerErrorSuffix => "); try again later",
            Self::DownloadFailedHttpPrefix => "Download failed: HTTP ",
            Self::DownloadFailedPrefix => "Download failed: ",
            Self::ErrorChainSeparator => ": ",
            Self::TooLargePrefix => "The update file exceeds the size limit (",
            Self::TooLargeSuffix => " MiB), aborted",
            Self::ReadLocalPackageFailedPrefix => "Failed to read the local update package: ",
            Self::DebugSourceLabel => "Debug-specified update notes",
            Self::OfficialSourceLabel => "Official release page",
            Self::CancelledShort => "Cancelled",
            Self::NotesNotUtf8 => "The release notes are not valid UTF-8 text",
            Self::NoUpdateSource => "No update source is configured",
            Self::ReadManifestFailedPrefix => "Failed to read the update manifest: ",
            Self::VerifyPackageFailedPrefix => "Failed to verify the update package: ",
            Self::CreateCacheDirFailedPrefix => "Failed to create the update cache directory: ",
            Self::CleanOldPackageFailedPrefix => "Failed to remove the old update package: ",
            Self::WritePackageFailedPrefix => "Failed to write the update package: ",
            Self::RememberSkipFailedPrefix => "Could not remember the skip: ",
            Self::PackageChangedRefusedPrefix => {
                "The package no longer matches the verified bytes; launch refused: "
            }
            Self::ShellExecuteErrorPrefix => "ShellExecuteW returned error code ",
            #[cfg(not(windows))]
            Self::InstallerWindowsOnly => "The installer can only be launched on Windows",
            Self::UnsupportedUrlSchemePrefix => "Unsupported update address scheme: ",
            Self::UrlWithoutFileName => "The update address has no usable file name",
            Self::InvalidFileNamePrefix => "Invalid update package file name: ",
            Self::EmptyFileName => "The update package file name is empty",
            Self::OpenSettingsDbFailedPrefix => "Failed to open the settings database: ",
            Self::ProxySettingsProblemPrefix => {
                "The software network proxy setting has a problem: "
            }
            Self::UpdateProxyConfigErrorPrefix => "Bad update proxy configuration: ",
            Self::CreateDownloadClientFailedPrefix => "Failed to create the download client: ",
            Self::PubkeyUnavailable => {
                "The built-in update public key is unusable, so update packages cannot be verified"
            }
            Self::SigningUnconfigured => {
                "Update signing is not configured: this build has no update signing public key yet, so online updates are unavailable (no update will be downloaded or installed). Please download the latest installer from the release page."
            }
            #[cfg(any(target_os = "macos", test))]
            Self::MacosNoAutoUpdate => {
                "Automatic updates are not supported on macOS yet, and there is no macOS release package: ZeeTerm will not download or launch any installer."
            }
            Self::NoReleaseNotes => "(no release notes)",
            Self::PackageLabel => "Package: ",
            Self::PlatformLabel => "Platform: ",
            Self::RestartNowButton => "Restart now",
            Self::RestartLaterButton => "Restart later",
            Self::OpenReleasePageButton => "Open release page",
            Self::RevealFileButton => "Show in file manager",
            Self::InstallingTitle => "Installing the update…",
            Self::InstalledTitlePrefix => "Installed ",
            Self::ManualTitlePrefix => "Downloaded ",
            Self::InstallingDebMessage => {
                "Installing through the system package manager. Please confirm in the system authorization prompt."
            }
            Self::InstallingTarMessage => "Replacing the program files…",
            Self::InstalledMessage => "The update is installed. Restart ZeeTerm to use it.",
            Self::NoPackageMessage => {
                "This release has no package for the way ZeeTerm is installed here. Please download it from the release page."
            }
            Self::ManualReasonNoPkexec => {
                "pkexec is not available on this computer, so the system authorization prompt cannot be shown."
            }
            Self::ManualReasonNotAuthorized => {
                "System authorization was not granted (the prompt was cancelled, or no authentication agent is available)."
            }
            Self::ManualReasonNotWritable => {
                "ZeeTerm's install folder is not writable, so the program files cannot be replaced directly."
            }
            Self::ManualDebHowTo => {
                "The package is downloaded and verified. Double-click it to install, or run in a terminal:"
            }
            Self::ManualTarHowTo => {
                "The package is downloaded and verified. Extract it and run its install.sh, or replace the program files yourself:"
            }
            Self::LinuxInstallFailedPrefix => "Failed to install the update: ",
            Self::DebInstallFailedPrefix => "Installation failed. apt said:\n",
            Self::DebHashChanged => {
                "The package was modified after it was verified, so installation was refused (nothing was installed). Please download it again."
            }
            Self::DebWrongPackage => {
                "This file is not a ZeeTerm package, so installation was refused (nothing was installed). Please download it again."
            }
            Self::DebNotRegularFile => {
                "The cached package is not a regular file (it may have been replaced by a link or something else), so installation was refused (nothing was installed). Please download it again."
            }
            Self::DebTooLarge => {
                "The cached package is too large, so installation was refused (nothing was installed). Please download it again."
            }
            Self::RestartFailedPrefix => "Could not restart: ",
        }
    }
}

/// 一次检查的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckKind {
    /// 已是最新版本。
    Latest,
    /// 有新版本（`package` 一定有值）。
    Found,
    /// Linux：有新版本，但发布的清单里没有适用于本机安装方式的包（老版本发布 / 只发了另一种包）。
    /// 不下载、不安装，界面引导到发布页（`package` 为空，版本号在 `release_version`）。
    FoundManual,
    /// 没查成：网络、清单格式、选不出包、更新签名未配置……（`status` 里是中文原因）。
    Failed,
}

/// 检查更新的结果：`status` 是给人看的文字（失败时就是原因），其余是界面要用的结构化信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Checked {
    pub kind: CheckKind,
    pub status: String,
    /// 清单里选中的包；没有更新或清单出错时为 `None`。
    pub package: Option<Package>,
    /// 清单里的更新说明（Markdown 原文）。
    pub notes: Option<String>,
    /// 清单里的发布时间（RFC 3339 原文）。
    pub published: Option<String>,
    /// 清单公告的新版本号（`Found` 和 `FoundManual` 有值，不带 `v`）。
    pub release_version: Option<String>,
}

impl Checked {
    fn failed(status: impl Into<String>) -> Self {
        Self {
            kind: CheckKind::Failed,
            status: status.into(),
            package: None,
            notes: None,
            published: None,
            release_version: None,
        }
    }

    /// 失败原因，去掉"检查更新失败："这个前缀（界面标题已经说了"检查更新失败"）。
    ///
    /// 状态文字是在产生时按当时的语言拼好的，所以中文、英文两个前缀都认。
    fn failure_reason(&self) -> &str {
        let zh = UpdateMsg::CheckFailedPrefix.zh();
        let en = UpdateMsg::CheckFailedPrefix.en();
        self.status
            .strip_prefix(zh)
            .or_else(|| self.status.strip_prefix(en))
            .unwrap_or(&self.status)
    }
}

pub struct UpdateDemo {
    current: String,
    /// 检查结果；`None` 表示还在检查。
    checked: Option<Checked>,
    /// 更新说明（已做安全改写，见 `notes`），交给 `TextView` 渲染；没有说明时为 `None`。
    notes_markdown: Option<::gpui::SharedString>,
    /// 清单里选中的包；没有更新或清单出错时为 `None`。
    package: Option<Package>,
    phase: DownloadState,
    /// 下载速度（平滑后），只在下载中显示。
    speed: layout::SpeedMeter,
    /// 「跳过此版本」没存成时的提示。
    skip_error: Option<String>,
    /// 这个窗口的 IME key，窗口被程序自己关掉时要还给 `ime`。
    ime_key: String,
    /// 安装后是否重启；只在生成安装计划时使用。
    restart: bool,
    install: InstallState,
    /// 安装前安全检查拦下的原因；下一次点「立即安装」时重算。
    blocked: Option<String>,
    /// 「退出后安装」的成功提示；排队状态本身在进程级队列里，关窗不丢。
    queued: Option<String>,
    /// 当前下载任务的代号；重新检查/取消后旧事件按代号丢掉。
    generation: u64,
    job: Option<DownloadJob>,
    events: Receiver<DownloadEvent>,
    sender: Sender<DownloadEvent>,
    focus: FocusHandle,
    /// 锁屏页（V2-U26）：锁着时整个窗口只画它。
    lock_screen: Option<Entity<crate::app_lock::LockScreen>>,
    _ime_focus: Subscription,
    _ime_blur: Subscription,
    _tick: Task<()>,
}

impl UpdateDemo {
    /// `preset`：后台自动检查已经取到的结果，有它就不再重复请求清单。
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        ime_key: String,
        preset: Option<Checked>,
    ) -> Self {
        let focus = cx.focus_handle();
        let (_ime_focus, _ime_blur) = crate::ime::attach(cx, &focus, window, &ime_key);
        let (sender, events) = std::sync::mpsc::channel();
        let mut this = Self {
            current: current_version(),
            checked: None,
            notes_markdown: None,
            package: None,
            phase: DownloadState::Idle,
            speed: layout::SpeedMeter::default(),
            skip_error: None,
            ime_key,
            restart: true,
            install: InstallState::Idle,
            blocked: None,
            queued: None,
            generation: 0,
            job: None,
            events,
            sender,
            focus,
            lock_screen: None,
            _ime_focus,
            _ime_blur,
            _tick: Task::ready(()),
        };
        match preset {
            Some(checked) => this.apply_checked(checked),
            None => this.reload(cx),
        }
        // 100ms 取一次下载/校验进度；窗口没了 `update_in` 会报错，任务自己退出。
        this._tick = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                if this
                    .update_in(cx, |this, _window, cx| this.poll_events(cx))
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

    /// 重新取一次清单；界面上的「重新检查」也走这里。网络请求在后台跑，结果经事件回来。
    fn reload(&mut self, cx: &mut Context<Self>) {
        // Linux：安装正在后台跑（授权框、apt、替换文件），这时重新检查会丢掉它的结果。
        if matches!(self.install, InstallState::Installing(_)) {
            return;
        }
        self.cancel_job();
        self.current = current_version();
        self.package = None;
        self.notes_markdown = None;
        self.phase = DownloadState::Idle;
        self.install = InstallState::Idle;
        self.blocked = None;
        self.skip_error = None;
        match crate::ssh_backend::runtime() {
            Ok(runtime) => {
                self.checked = None;
                runtime.spawn(run_check(
                    self.generation,
                    self.current.clone(),
                    self.sender.clone(),
                ));
            }
            Err(err) => {
                self.checked = Some(Checked::failed(format!(
                    "{}{err}",
                    t(UpdateMsg::CheckFailedPrefix)
                )))
            }
        }
        cx.notify();
    }

    /// 收下一次检查的结果：选出的包、更新说明（先做安全改写）都在这里落到界面状态上。
    fn apply_checked(&mut self, checked: Checked) {
        self.package = checked.package.clone();
        self.notes_markdown =
            notes::display_markdown(checked.notes.as_deref()).map(::gpui::SharedString::from);
        // Linux：这个版本本进程里已经装好、只差重启（用户点过「稍后重启」）时，不当新版本再装一遍，
        // 直接回到「已安装，重启后生效」那一屏，还能点「立即重启」。
        let awaiting = checked
            .package
            .as_ref()
            .and_then(|package| linux::awaiting_restart(&package.version));
        self.install = state_after_check(std::mem::take(&mut self.install), awaiting);
        self.checked = Some(checked);
    }

    /// 当前画哪一屏。Linux 的安装过程（安装中 / 已装好 / 手动安装）盖在"已验证"之上。
    fn screen(&self) -> layout::Screen {
        match &self.install {
            InstallState::Installing(_) => layout::Screen::Installing,
            InstallState::Installed { .. } => layout::Screen::Installed,
            InstallState::Manual { .. } => layout::Screen::ManualInstall,
            _ => layout::screen(
                self.checked.as_ref().map(|checked| checked.kind),
                &self.phase,
            ),
        }
    }

    /// 清单公告的新版本号：有可下载的包时是包的版本，Linux 上没有适用的包时是清单里的版本。
    fn announced_version(&self) -> Option<String> {
        self.package
            .as_ref()
            .map(|package| package.version.clone())
            .or_else(|| {
                self.checked
                    .as_ref()
                    .and_then(|checked| checked.release_version.clone())
            })
    }

    /// 选中的包是不是 Linux 的包（决定安装走 pkexec / 替换文件，而不是 Windows 的安装器）。
    fn linux_flavor(&self) -> Option<Flavor> {
        self.package
            .as_ref()
            .and_then(|package| Flavor::from_platform_key(&package.platform_key))
    }

    /// 关掉这个窗口（程序自己关：要把 IME 的窗口记录还回去，和用户点关闭按钮走的收尾一致）。
    fn close_window(&mut self, window: &mut Window) {
        crate::ime::window_destroyed(&self.ime_key);
        window.remove_window();
    }

    /// 「跳过此版本」：记下版本号（之后的自动检查不再提示它，更新的版本照常提示），然后关窗口。
    fn skip_version(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(version) = self.announced_version() else {
            return;
        };
        match record_skipped_version(&version) {
            Ok(()) => self.close_window(window),
            Err(err) => {
                self.skip_error = Some(err);
                cx.notify();
            }
        }
    }

    /// 「稍后」：发现新版本时只关窗口；已验证时先排队"退出后安装"（没排上就留在窗口里说原因）再关。
    fn later(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // "退出后安装"只有 Windows 有（安装器可以等程序退出时再启动）；Linux 的安装要用户在场（授权框），稍后就只是关窗口。
        if matches!(self.phase, DownloadState::Verified { .. })
            && self.linux_flavor().is_none()
            && !self.click_queue_install(cx)
        {
            return;
        }
        self.close_window(window);
    }

    /// 作废当前下载任务；后台任务看到取消标志后删除半截文件。
    fn cancel_job(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(job) = self.job.take() {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// 在 Tokio runtime 上开始下载；界面只收事件，不等待网络。
    fn start_download(&mut self, cx: &mut Context<Self>) {
        let Some(package) = self.package.clone() else {
            return;
        };
        if !self.phase.can_start() {
            return;
        }
        let generation = self.generation.wrapping_add(1);
        self.generation = generation;
        self.phase.start();
        self.speed.reset();
        self.install = InstallState::Idle;

        let cancel = Arc::new(AtomicBool::new(false));
        self.job = Some(DownloadJob {
            cancel: Arc::clone(&cancel),
        });
        let sender = self.sender.clone();
        let cache_root = update_cache_dir();
        match crate::ssh_backend::runtime() {
            Ok(runtime) => {
                runtime.spawn(run_download(
                    generation, package, cache_root, cancel, sender,
                ));
            }
            Err(err) => {
                self.phase
                    .fail(format!("{}{err}", t(UpdateMsg::StartDownloadFailedPrefix)));
                self.job = None;
            }
        }
        cx.notify();
    }

    /// 取消下载：先改界面状态，后台任务随后删除半截文件。
    fn cancel_download(&mut self, cx: &mut Context<Self>) {
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Relaxed);
        }
        self.phase.cancel();
        cx.notify();
    }

    fn toggle_restart(&mut self, cx: &mut Context<Self>) {
        self.restart = !self.restart;
        // 参数变了，旧计划不能再显示；旧的拦截提示也一起清掉。
        self.install = InstallState::Idle;
        self.blocked = None;
        cx.notify();
    }

    /// 「立即安装」按钮：生成计划并直接启动安装器。
    ///
    /// 返回是否已经启动；调用方据此安排退出。
    fn click_install(&mut self, cx: &mut Context<Self>) -> bool {
        if let Some(flavor) = self.linux_flavor() {
            return self.click_install_linux(flavor, cx);
        }
        let (installer, version, sha256) = match (&self.phase, &self.package) {
            (DownloadState::Verified { path, sha256, .. }, Some(package)) => {
                (path.clone(), package.version.clone(), sha256.clone())
            }
            _ => return false,
        };
        let cache_root = update_cache_dir();
        let args = current_exe_args();
        let restart = self.restart;
        let (state, blocked, launched) = run_install(
            install_state::blocking_reason(),
            std::mem::take(&mut self.install),
            || build_install_plan(&installer, &cache_root, &version, restart, &args, &sha256),
            launch_installer,
        );
        self.install = state;
        self.blocked = blocked;
        if launched {
            // 安装器已经立即启动了，退出回调里不能再启动一次排队的计划。
            clear_pending_install();
            self.queued = None;
        }
        cx.notify();
        launched
    }

    /// Linux 的「立即安装」：点一下就开始（不再二次确认）。安装在后台线程里跑——deb 要等系统授权框和 apt，
    /// tar.gz 要解压、校验、替换文件，都不能卡住界面；结果经 [`DownloadEvent::LinuxInstall`] 回来。
    /// 安装本身不退出程序，所以总是返回 `false`；重启是装好之后用户另点的。
    fn click_install_linux(&mut self, flavor: Flavor, cx: &mut Context<Self>) -> bool {
        if !matches!(self.install, InstallState::Idle | InstallState::Failed(_)) {
            return false;
        }
        let (package, version, sha256) = match (&self.phase, &self.package) {
            (DownloadState::Verified { path, sha256, .. }, Some(package)) => {
                (path.clone(), package.version.clone(), sha256.clone())
            }
            _ => return false,
        };
        let runtime = match crate::ssh_backend::runtime() {
            Ok(runtime) => runtime,
            Err(err) => {
                self.install = InstallState::Failed(format!(
                    "{}{err}",
                    t(UpdateMsg::LinuxInstallFailedPrefix)
                ));
                cx.notify();
                return false;
            }
        };
        let job = linux::Job {
            flavor,
            package,
            cache_root: update_cache_dir(),
            version,
            sha256,
            exe: linux::running_exe(),
        };
        self.install = InstallState::Installing(flavor);
        self.blocked = None;
        self.queued = None;
        let (generation, sender) = (self.generation, self.sender.clone());
        runtime.spawn_blocking(move || {
            let outcome = linux::run_system(&job);
            let _ = sender.send(DownloadEvent::LinuxInstall {
                generation,
                outcome,
            });
        });
        cx.notify();
        false
    }

    /// 收下后台安装的结果。手动安装时顺手在文件管理器里显示那个文件（用户多半要双击它）。
    fn apply_linux_outcome(&mut self, outcome: linux::Outcome, cx: &mut Context<Self>) {
        match outcome {
            linux::Outcome::Installed { restart } => {
                if let Some(package) = &self.package {
                    linux::remember_installed(&package.version, restart.clone());
                }
                self.install = InstallState::Installed { restart };
            }
            linux::Outcome::Manual(reason) => {
                let (Some(flavor), DownloadState::Verified { path, .. }) =
                    (self.linux_flavor(), &self.phase)
                else {
                    return;
                };
                let path = path.clone();
                cx.reveal_path(&path);
                self.install = InstallState::Manual {
                    flavor,
                    reason,
                    path,
                };
            }
            linux::Outcome::Failed(message) => self.install = InstallState::Failed(message),
        }
    }

    /// Linux 装好之后的「立即重启」：安排新程序在本进程退出后启动，返回是否已安排好，调用方随后退出界面。
    /// 还有传输 / AI 回合在跑时不重启，原因显示在窗口里。
    fn click_restart(&mut self, cx: &mut Context<Self>) -> bool {
        let InstallState::Installed { restart } = &self.install else {
            return false;
        };
        if let Some(reason) = install_state::blocking_reason() {
            self.blocked = Some(reason);
            cx.notify();
            return false;
        }
        let started = match linux::restart(restart, &current_exe_args()) {
            Ok(()) => {
                self.blocked = None;
                true
            }
            Err(err) => {
                self.blocked = Some(err);
                false
            }
        };
        cx.notify();
        started
    }

    /// 手动安装时「在文件管理器中显示」。
    fn reveal_file(&mut self, cx: &mut Context<Self>) {
        if let InstallState::Manual { path, .. } = &self.install {
            cx.reveal_path(path);
        }
    }

    /// 「打开发布页」：用系统浏览器打开（只认 https，过 core 的网址检查）；打不开时把原因显示出来。
    fn open_release_page(&mut self, cx: &mut Context<Self>) {
        let Some(url) = linux::release_page_url() else {
            return;
        };
        if let Err(err) = zssh_core::external_url::open_external_url(&url) {
            self.skip_error = Some(err);
            cx.notify();
        }
    }

    /// 「退出后安装」：生成计划并排队，界面不退出，等进程结束时再启动安装器。
    fn click_queue_install(&mut self, cx: &mut Context<Self>) -> bool {
        let (installer, version, sha256) = match (&self.phase, &self.package) {
            (DownloadState::Verified { path, sha256, .. }, Some(package)) => {
                (path.clone(), package.version.clone(), sha256.clone())
            }
            _ => return false,
        };
        let cache_root = update_cache_dir();
        let args = current_exe_args();
        let restart = self.restart;
        let queued =
            match build_install_plan(&installer, &cache_root, &version, restart, &args, &sha256) {
                Ok(plan) => {
                    queue_install(plan);
                    self.queued = Some(t(UpdateMsg::QueuedMessage).to_owned());
                    self.blocked = None;
                    true
                }
                // 没排上队：沿用「安装」失败时的提示，之前排好的计划保持不动。
                Err(err) => {
                    self.install = InstallState::Failed(format!(
                        "{}{err}",
                        t(UpdateMsg::PrepareInstallFailedPrefix)
                    ));
                    false
                }
            };
        cx.notify();
        queued
    }

    /// 取走后台事件；过期的任务结果直接丢掉。
    fn poll_events(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        let mut auto_download = false;
        let mut auto_install = false;
        let mut auto_restart = false;
        while let Ok(event) = self.events.try_recv() {
            if event.generation() != self.generation {
                continue;
            }
            match event {
                DownloadEvent::Progress {
                    received, total, ..
                } => {
                    self.phase.progress(received, total);
                    self.speed.record(Instant::now(), received);
                }
                DownloadEvent::Verifying { .. } => self.phase.begin_verify(),
                DownloadEvent::Verified {
                    path, size, sha256, ..
                } => {
                    self.phase.verified(path, size, sha256);
                    self.job = None;
                    // 调试钩子：验签通过就直接点「立即安装」（只对 Linux 的包生效；自动化 / 真机验证用）。
                    auto_install = self.linux_flavor().is_some()
                        && crate::dev_hooks::var_os("ZSSH_UPDATE_AUTOINSTALL").is_some();
                }
                DownloadEvent::LinuxInstall { outcome, .. } => {
                    let installed = matches!(outcome, linux::Outcome::Installed { .. });
                    self.apply_linux_outcome(outcome, cx);
                    // 调试钩子：装好就直接重启。
                    auto_restart =
                        installed && crate::dev_hooks::var_os("ZSSH_UPDATE_AUTORESTART").is_some();
                }
                DownloadEvent::Checked { checked, .. } => {
                    self.apply_checked(checked);
                    // 调试钩子：查到新版本就直接开始下载（自动化 / 截图下载中的画面用，发行版里读不到）。
                    auto_download = self.package.is_some()
                        && crate::dev_hooks::var_os("ZSSH_UPDATE_AUTODOWNLOAD").is_some();
                }
                DownloadEvent::Failed { message, .. } => {
                    self.phase.fail(message);
                    self.job = None;
                }
                DownloadEvent::Cancelled { .. } => {
                    self.phase.cancel();
                    self.job = None;
                }
            }
            changed = true;
        }
        if auto_download {
            self.start_download(cx);
        }
        if auto_install {
            self.click_install(cx);
        }
        if auto_restart && self.click_restart(cx) {
            quit_after_paint(cx);
        }
        if changed {
            cx.notify();
        }
    }
}

/// 打开更新检查窗口；同一个进程可以开多个。手动检查（关于页、托盘）走这里：抢焦点，窗口里自己去取清单。
pub fn open_update_window(cx: &mut App) {
    open_window_with(cx, true, None);
}

/// 后台自动检查发现了新版本：把已经取到的结果交给窗口（不重复请求），并且**不抢焦点**——
/// 用户可能正在终端里打字，窗口只是出现在那里等他有空看。
pub fn open_update_window_for_found(cx: &mut App, checked: Checked) {
    open_window_with(cx, false, Some(checked));
}

/// 窗口默认尺寸（逻辑像素）和最小尺寸：内容少时卡片按内容算高度，不留大片空白。
const WINDOW_SIZE: (f32, f32) = (560., 600.);
const WINDOW_MIN_SIZE: (f32, f32) = (480., 420.);

/// 窗口标题：自绘标题栏上的文字，也是任务栏 / Alt+Tab 里的系统标题（截图脚本按它找窗口）。
fn window_title() -> &'static str {
    t(UpdateMsg::WindowTitle)
}

fn open_window_with(cx: &mut App, focus: bool, preset: Option<Checked>) {
    let n = NEXT_UPDATE.fetch_add(1, Ordering::Relaxed);
    let ime_key = format!("update-window-{n}");
    let close_key = ime_key.clone();
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(
            size(::gpui::px(WINDOW_SIZE.0), ::gpui::px(WINDOW_SIZE.1)),
            cx,
        )),
        window_min_size: Some(size(
            ::gpui::px(WINDOW_MIN_SIZE.0),
            ::gpui::px(WINDOW_MIN_SIZE.1),
        )),
        focus,
        // 标题栏自己画（跟着深 / 浅皮肤），系统不画。
        titlebar: Some(::gpui::TitlebarOptions {
            title: Some(window_title().into()),
            ..gpui_kit::component::TitleBar::title_bar_options()
        }),
        ..crate::window_frame::client_options()
    };
    let _ = cx.open_window(options, move |window, cx| {
        let view = cx.new(|cx| UpdateDemo::new(window, cx, ime_key, preset));
        window.set_window_title(window_title());
        crate::window_frame::on_should_close(window, cx, move |_window, _cx| {
            crate::ime::window_destroyed(&close_key);
            true
        });
        if focus {
            window.focus(&view.read(cx).focus_handle(), cx);
            // 手动检查（关于页、托盘）是用户刚点的，窗口要到最前面；从托盘点时不这样会停在后面。
            crate::tray::bring_to_front(window);
        }
        view
    });
}

/// 下载/校验阶段；单独抽出来方便单测状态迁移。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum DownloadState {
    #[default]
    Idle,
    Downloading {
        received: u64,
        total: Option<u64>,
    },
    Verifying,
    Verified {
        path: PathBuf,
        size: u64,
        /// 验签通过的字节的 SHA-256，安装计划要带着它。
        sha256: String,
    },
    Failed(String),
    Cancelled,
}

impl DownloadState {
    fn start(&mut self) {
        if self.can_start() {
            *self = Self::Downloading {
                received: 0,
                total: None,
            };
        }
    }

    fn progress(&mut self, received: u64, total: Option<u64>) {
        if matches!(self, Self::Downloading { .. }) {
            *self = Self::Downloading { received, total };
        }
    }

    fn begin_verify(&mut self) {
        if matches!(self, Self::Downloading { .. }) {
            *self = Self::Verifying;
        }
    }

    fn verified(&mut self, path: PathBuf, size: u64, sha256: String) {
        if matches!(self, Self::Verifying) {
            *self = Self::Verified { path, size, sha256 };
        }
    }

    fn fail(&mut self, message: String) {
        *self = Self::Failed(message);
    }

    fn cancel(&mut self) {
        if matches!(self, Self::Downloading { .. }) {
            *self = Self::Cancelled;
        }
    }

    /// 只有空闲、失败或已取消时可以重新下载。
    fn can_start(&self) -> bool {
        matches!(self, Self::Idle | Self::Failed(_) | Self::Cancelled)
    }

    fn text(&self) -> Option<String> {
        match self {
            Self::Idle => None,
            Self::Downloading { received, total } => Some(progress_text(*received, *total)),
            Self::Verifying => Some(t(UpdateMsg::VerifyingSignature).to_owned()),
            Self::Verified { .. } => Some(t(UpdateMsg::SignatureCheckPassed).to_owned()),
            Self::Failed(message) => Some(message.clone()),
            Self::Cancelled => Some(t(UpdateMsg::DownloadCancelled).to_owned()),
        }
    }
}

/// 安装状态：点「立即安装」一次就生成计划并启动安装器。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum InstallState {
    #[default]
    Idle,
    /// 安装器已交给系统，界面即将退出。
    Launched,
    Failed(String),
    /// Linux：后台正在安装（deb 是系统授权框和 apt 在跑，tar.gz 是在替换文件）。
    Installing(Flavor),
    /// Linux：已经装好，等用户重启；`restart` 是要启动的新程序。
    Installed {
        restart: PathBuf,
    },
    /// Linux：不能自动装（没有 pkexec、授权被取消、目录不可写），包已下载并校验，请用户手动装。
    Manual {
        flavor: Flavor,
        reason: ManualReason,
        path: PathBuf,
    },
}

/// 一次检查之后的安装状态：本进程里已经装好这个版本（`awaiting` 是重启要启动的程序）就回到「已安装」，
/// 否则保持原样。
fn state_after_check(state: InstallState, awaiting: Option<PathBuf>) -> InstallState {
    match awaiting {
        Some(restart) => InstallState::Installed { restart },
        None => state,
    }
}

/// 点「立即安装」时的纯状态迁移：生成计划并启动。
///
/// 返回 `(新状态, 是否已启动安装器)`，调用方据此决定是否退出。
/// 生成计划或启动失败都变成 [`InstallState::Failed`]，按钮仍可再点重试。
/// `plan` / `launch` 由调用方注入，测试里换成假实现，不会真的启动进程。
fn advance_install(
    state: InstallState,
    plan: impl FnOnce() -> Result<InstallPlan, String>,
    launch: impl FnOnce(&InstallPlan) -> Result<(), String>,
) -> (InstallState, bool) {
    match state {
        // 已经启动过，界面正在退出，不再响应点击。
        InstallState::Launched => (InstallState::Launched, false),
        _ => match plan() {
            Ok(plan) => match launch(&plan) {
                Ok(()) => (InstallState::Launched, true),
                Err(err) => (
                    InstallState::Failed(format!(
                        "{}{err}",
                        t(UpdateMsg::LaunchInstallerFailedPrefix)
                    )),
                    false,
                ),
            },
            Err(err) => (
                InstallState::Failed(format!("{}{err}", t(UpdateMsg::PrepareInstallFailedPrefix))),
                false,
            ),
        },
    }
}

/// 在 [`advance_install`] 前加安装前拦截：SFTP 传输 / AI 回合等还在跑时
/// （`blocking` 为 `Some`）不生成计划、不启动，状态原样还回，原因交给界面显示。
/// 返回 `(新状态, 拦截原因, 是否已启动)`。
fn run_install(
    blocking: Option<String>,
    state: InstallState,
    plan: impl FnOnce() -> Result<InstallPlan, String>,
    launch: impl FnOnce(&InstallPlan) -> Result<(), String>,
) -> (InstallState, Option<String>, bool) {
    if blocking.is_some() {
        return (state, blocking, false);
    }
    let (state, launched) = advance_install(state, plan, launch);
    (state, None, launched)
}

/// 取消标志；后台任务在每个数据块之间检查。
struct DownloadJob {
    cancel: Arc<AtomicBool>,
}

/// 后台任务 → 界面的消息（100ms tick 取走）。
#[derive(Debug)]
enum DownloadEvent {
    Progress {
        generation: u64,
        received: u64,
        total: Option<u64>,
    },
    Verifying {
        generation: u64,
    },
    Verified {
        generation: u64,
        path: PathBuf,
        size: u64,
        sha256: String,
    },
    Failed {
        generation: u64,
        message: String,
    },
    Cancelled {
        generation: u64,
    },
    /// 取清单的结果。
    Checked {
        generation: u64,
        checked: Checked,
    },
    /// Linux：后台安装任务的结果。
    LinuxInstall {
        generation: u64,
        outcome: linux::Outcome,
    },
}

impl DownloadEvent {
    fn generation(&self) -> u64 {
        match self {
            Self::Progress { generation, .. }
            | Self::Verifying { generation }
            | Self::Verified { generation, .. }
            | Self::Failed { generation, .. }
            | Self::Cancelled { generation }
            | Self::Checked { generation, .. }
            | Self::LinuxInstall { generation, .. } => *generation,
        }
    }
}

/// 取消 / 失败；下载和取清单共用。
enum DownloadFailure {
    Cancelled,
    Failed(String),
}

/// 更新源是不是放开了 `http://` / `file://`：只有调试构建放开。
fn allow_insecure_sources() -> bool {
    crate::dev_hooks::ENABLED
}

/// 更新清单和安装包的地址检查：发行版只认 `https://`。
///
/// `allow_insecure` 为真（调试构建）时才放行 `http://` 和本地 `file://`；
/// `file://` 带非本机主机名（Windows 上会变成 UNC 路径，打开就触发 SMB 认证）一律拒绝。
fn check_update_url(url: &str, allow_insecure: bool) -> Result<reqwest::Url, String> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|err| format!("{}{err}", t(UpdateMsg::UpdateUrlInvalidPrefix)))?;
    match parsed.scheme() {
        "https" => Ok(parsed),
        "http" if allow_insecure => Ok(parsed),
        "file" if allow_insecure => match parsed.host_str() {
            None | Some("") | Some("localhost") => Ok(parsed),
            Some(_) => Err(t(UpdateMsg::UpdateUrlNoOtherHosts).to_owned()),
        },
        other => Err(format!(
            "{}{other}",
            t(UpdateMsg::UpdateUrlMustBeHttpsPrefix)
        )),
    }
}

/// 重定向规则：次数有限，且不能跳到不允许的协议（尤其是 https → http）。
fn redirect_allowed(
    scheme: &str,
    previous: usize,
    allow_insecure: bool,
) -> Result<(), &'static str> {
    if previous > MAX_REDIRECTS {
        return Err(t(UpdateMsg::TooManyRedirects));
    }
    match scheme {
        "https" => Ok(()),
        "http" if allow_insecure => Ok(()),
        _ => Err(t(UpdateMsg::RedirectToNonHttps)),
    }
}

fn redirect_policy(allow_insecure: bool) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        match redirect_allowed(
            attempt.url().scheme(),
            attempt.previous().len(),
            allow_insecure,
        ) {
            Ok(()) => attempt.follow(),
            Err(message) => attempt.error(message),
        }
    })
}

/// reqwest 的错误只写"跟随重定向失败"这类外层原因，真正的缘由（比如我们自己的重定向规则）
/// 在 `source()` 链里，一起拼出来用户才看得懂。
fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        text.push_str(t(UpdateMsg::ErrorChainSeparator));
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// 服务器返回非 2xx 时给用户看的原因：最常见的几种说人话，其余带上状态码。
fn http_status_message(status: reqwest::StatusCode) -> String {
    let code = status.as_u16();
    match code {
        404 => t(UpdateMsg::HttpNotFound).to_owned(),
        401 | 403 => format!(
            "{}{code}{}",
            t(UpdateMsg::HttpForbiddenPrefix),
            t(UpdateMsg::CloseParen)
        ),
        429 => t(UpdateMsg::HttpTooManyRequests).to_owned(),
        500..=599 => format!(
            "{}{code}{}",
            t(UpdateMsg::HttpServerErrorPrefix),
            t(UpdateMsg::HttpServerErrorSuffix)
        ),
        _ => format!("{}{status}", t(UpdateMsg::DownloadFailedHttpPrefix)),
    }
}

fn too_large_message(max_bytes: u64) -> String {
    format!(
        "{}{}{}",
        t(UpdateMsg::TooLargePrefix),
        max_bytes / (1024 * 1024),
        t(UpdateMsg::TooLargeSuffix)
    )
}

/// 追加一个数据块，累计超过上限就报错；下载和读本地文件共用，方便单测。
fn append_capped(buffer: &mut Vec<u8>, chunk: &[u8], max_bytes: u64) -> Result<(), String> {
    if (buffer.len() as u64).saturating_add(chunk.len() as u64) > max_bytes {
        return Err(too_large_message(max_bytes));
    }
    buffer.extend_from_slice(chunk);
    Ok(())
}

/// 把 `url` 的内容读进内存。超过 `max_bytes`、超过 `timeout` 或被取消都会中止，
/// 不会留下任何文件。`progress(已收, 总数, 是否强制上报)` 由调用方节流后发给界面。
async fn fetch_bytes(
    client: &reqwest::Client,
    url: &reqwest::Url,
    max_bytes: u64,
    timeout: Duration,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, Option<u64>, bool),
) -> Result<Vec<u8>, DownloadFailure> {
    let failed = |message: String| DownloadFailure::Failed(message);
    // 预分配别太大：Content-Length 是对方说了算的。
    let prealloc = |total: Option<u64>| total.unwrap_or(0).min(16 * 1024 * 1024) as usize;

    if url.scheme() == "file" {
        let source = url
            .to_file_path()
            .map_err(|_| failed(t(UpdateMsg::UpdateUrlNotLocalFile).to_owned()))?;
        let mut input = tokio::fs::File::open(&source).await.map_err(|err| {
            failed(format!(
                "{}{err}",
                t(UpdateMsg::ReadLocalPackageFailedPrefix)
            ))
        })?;
        let total = input
            .metadata()
            .await
            .ok()
            .map(|meta| meta.len())
            .filter(|len| *len > 0);
        if total.is_some_and(|len| len > max_bytes) {
            return Err(failed(too_large_message(max_bytes)));
        }
        let mut data = Vec::with_capacity(prealloc(total));
        let mut buffer = vec![0u8; CHUNK_SIZE];
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(DownloadFailure::Cancelled);
            }
            let read = input.read(&mut buffer).await.map_err(|err| {
                failed(format!(
                    "{}{err}",
                    t(UpdateMsg::ReadLocalPackageFailedPrefix)
                ))
            })?;
            if read == 0 {
                break;
            }
            append_capped(&mut data, &buffer[..read], max_bytes).map_err(failed)?;
            progress(data.len() as u64, total, false);
        }
        progress(data.len() as u64, total, true);
        return Ok(data);
    }

    let mut response = client
        .get(url.clone())
        .timeout(timeout)
        .send()
        .await
        .map_err(|err| {
            failed(format!(
                "{}{}",
                t(UpdateMsg::DownloadFailedPrefix),
                error_chain(&err)
            ))
        })?;
    if !response.status().is_success() {
        return Err(failed(http_status_message(response.status())));
    }
    let total = response.content_length().filter(|len| *len > 0);
    if total.is_some_and(|len| len > max_bytes) {
        return Err(failed(too_large_message(max_bytes)));
    }
    let mut data = Vec::with_capacity(prealloc(total));
    while let Some(chunk) = response.chunk().await.map_err(|err| {
        failed(format!(
            "{}{}",
            t(UpdateMsg::DownloadFailedPrefix),
            error_chain(&err)
        ))
    })? {
        if cancel.load(Ordering::Relaxed) {
            return Err(DownloadFailure::Cancelled);
        }
        append_capped(&mut data, &chunk, max_bytes).map_err(failed)?;
        progress(data.len() as u64, total, false);
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(DownloadFailure::Cancelled);
    }
    progress(data.len() as u64, total, true);
    Ok(data)
}

/// 取清单并判断有没有更新；结果经事件交给界面。
async fn run_check(generation: u64, current: String, sender: Sender<DownloadEvent>) {
    let checked = check_for_update(&current).await;
    let _ = sender.send(DownloadEvent::Checked {
        generation,
        checked,
    });
}

/// 清单来源，按顺序尝试，第一个读成功的算数。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ManifestSource {
    /// 本地清单文件（仅调试钩子）。
    File(PathBuf),
    /// 清单地址：内嵌的更新源，或调试钩子给的 URL。
    Url(String),
}

/// `hook` 是 `ZSSH_UPDATE_MANIFEST`（调试钩子，已经过 `dev_hooks`）；没有就用内嵌更新源。
fn resolve_manifest_sources(
    hook: Option<std::ffi::OsString>,
    embedded: Result<zssh_update::UpdaterConfig, String>,
) -> Vec<ManifestSource> {
    if let Some(value) = hook.filter(|value| !value.is_empty()) {
        let text = value.to_string_lossy().trim().to_owned();
        let lower = text.to_ascii_lowercase();
        return vec![
            if lower.starts_with("http://") || lower.starts_with("https://") {
                ManifestSource::Url(text)
            } else {
                ManifestSource::File(PathBuf::from(value))
            },
        ];
    }
    embedded
        .map(|config| {
            config
                .endpoints
                .into_iter()
                .map(ManifestSource::Url)
                .collect()
        })
        .unwrap_or_default()
}

fn manifest_sources() -> Vec<ManifestSource> {
    resolve_manifest_sources(
        crate::dev_hooks::var_os("ZSSH_UPDATE_MANIFEST"),
        zssh_update::embedded_config(),
    )
}

/// 界面上显示的更新源说明。
fn source_label() -> &'static str {
    if crate::dev_hooks::var_os("ZSSH_UPDATE_MANIFEST").is_some_and(|value| !value.is_empty()) {
        t(UpdateMsg::DebugSourceLabel)
    } else {
        t(UpdateMsg::OfficialSourceLabel)
    }
}

async fn load_manifest_text(
    source: &ManifestSource,
    client: &reqwest::Client,
    allow_insecure: bool,
) -> Result<String, String> {
    let bytes = match source {
        ManifestSource::File(path) => {
            let meta = tokio::fs::metadata(path)
                .await
                .map_err(|err| err.to_string())?;
            if meta.len() > MAX_MANIFEST_BYTES {
                return Err(too_large_message(MAX_MANIFEST_BYTES));
            }
            tokio::fs::read(path).await.map_err(|err| err.to_string())?
        }
        ManifestSource::Url(url) => {
            let url = check_update_url(url, allow_insecure)?;
            let never = AtomicBool::new(false);
            fetch_bytes(
                client,
                &url,
                MAX_MANIFEST_BYTES,
                MANIFEST_TIMEOUT,
                &never,
                |_, _, _| {},
            )
            .await
            .map_err(|failure| match failure {
                DownloadFailure::Cancelled => t(UpdateMsg::CancelledShort).to_owned(),
                DownloadFailure::Failed(message) => message,
            })?
        }
    };
    String::from_utf8(bytes).map_err(|_| t(UpdateMsg::NotesNotUtf8).to_owned())
}

/// 读清单 + 选择包；读不到也算界面错误，不让窗口崩。
///
/// 更新签名公钥是占位值时直接返回"更新签名未配置"，一个请求都不发。
#[cfg_attr(target_os = "macos", allow(unreachable_code))]
pub(crate) async fn check_for_update(current: &str) -> Checked {
    // macOS：没有 macOS 的发布包；探测本机安装方式会退成 Windows，绝不能拿 Windows 的安装包来装。
    // 一个请求都不发，自动检查也就什么都不会做。
    #[cfg(target_os = "macos")]
    {
        let _ = current;
        return Checked::failed(macos_no_auto_update());
    }
    // 本机是怎么装的（Linux 上要问 dpkg，别占着界面线程，也别堵住 async 工作线程）。
    let platform = tokio::task::spawn_blocking(host_platform)
        .await
        .unwrap_or_else(|_| linux::fallback_platform());
    let pubkey = update_pubkey();
    // 公钥不可用时先不建客户端（建客户端要读设置库）：下面 `check_with` 会原样返回原因。
    let client = if pubkey.is_ok() {
        match update_client() {
            Ok(client) => client,
            Err(err) => {
                return Checked::failed(format!("{}{err}", t(UpdateMsg::CheckFailedPrefix)));
            }
        }
    } else {
        reqwest::Client::new()
    };
    check_with(
        current,
        &manifest_sources(),
        &client,
        allow_insecure_sources(),
        pubkey.as_deref(),
        &platform,
    )
    .await
}

/// [`check_for_update`] 的可注入版本：清单来源、客户端和协议放行由调用方给（测试用本机服务）。
async fn check_with(
    current: &str,
    sources: &[ManifestSource],
    client: &reqwest::Client,
    allow_insecure: bool,
    pubkey: Result<&str, &String>,
    platform: &Platform,
) -> Checked {
    // 没有可用的验签公钥（占位值、内置配置坏了）就不发任何请求：后面下的包也验不了。
    match pubkey {
        Err(message) => return Checked::failed(message.clone()),
        Ok(key) if zssh_update::is_placeholder_pubkey(key) => {
            return Checked::failed(t(UpdateMsg::SigningUnconfigured));
        }
        Ok(_) => {}
    }
    if sources.is_empty() {
        return Checked::failed(t(UpdateMsg::NoUpdateSource));
    }
    let mut last_error = String::new();
    for source in sources {
        match load_manifest_text(source, client, allow_insecure).await {
            Ok(text) => return evaluate_manifest_for(&text, current, allow_insecure, platform),
            Err(err) => last_error = err,
        }
    }
    Checked::failed(format!(
        "{}{last_error}",
        t(UpdateMsg::ReadManifestFailedPrefix)
    ))
}

/// 下载、校验、上报结果都在 runtime 线程里跑。
async fn run_download(
    generation: u64,
    package: Package,
    cache_root: PathBuf,
    cancel: Arc<AtomicBool>,
    sender: Sender<DownloadEvent>,
) {
    match download_and_verify(generation, &package, &cache_root, &cancel, &sender).await {
        Ok((path, size, sha256)) => {
            let _ = sender.send(DownloadEvent::Verified {
                generation,
                path,
                size,
                sha256,
            });
        }
        Err(DownloadFailure::Cancelled) => {
            let _ = sender.send(DownloadEvent::Cancelled { generation });
        }
        Err(DownloadFailure::Failed(message)) => {
            let _ = sender.send(DownloadEvent::Failed {
                generation,
                message,
            });
        }
    }
}

/// 下到内存 → 验签 → 把验签通过的字节写进缓存。返回 `(路径, 大小, SHA-256)`。
async fn download_and_verify(
    generation: u64,
    package: &Package,
    cache_root: &Path,
    cancel: &AtomicBool,
    sender: &Sender<DownloadEvent>,
) -> Result<(PathBuf, u64, String), DownloadFailure> {
    // 公钥是占位值就在这里挡住，连文件名都不用看，更不会发出下载请求。
    let pubkey = update_pubkey().map_err(DownloadFailure::Failed)?;
    let client = update_client().map_err(DownloadFailure::Failed)?;
    download_with(
        &DownloadContext {
            pubkey: &pubkey,
            client: &client,
            allow_insecure: allow_insecure_sources(),
        },
        generation,
        package,
        cache_root,
        cancel,
        sender,
    )
    .await
}

/// 下载用的环境：验签公钥、HTTP 客户端、是否放行 http / file（测试注入本机服务）。
struct DownloadContext<'a> {
    pubkey: &'a str,
    client: &'a reqwest::Client,
    allow_insecure: bool,
}

/// [`download_and_verify`] 的可注入版本。
async fn download_with(
    env: &DownloadContext<'_>,
    generation: u64,
    package: &Package,
    cache_root: &Path,
    cancel: &AtomicBool,
    sender: &Sender<DownloadEvent>,
) -> Result<(PathBuf, u64, String), DownloadFailure> {
    if zssh_update::is_placeholder_pubkey(env.pubkey) {
        return Err(DownloadFailure::Failed(
            t(UpdateMsg::SigningUnconfigured).to_owned(),
        ));
    }
    let url =
        check_update_url(&package.url, env.allow_insecure).map_err(DownloadFailure::Failed)?;
    let name = download_file_name(&package.url).map_err(DownloadFailure::Failed)?;
    // 先把文件名查一遍，名字不合法就不必下载。
    cache_path(cache_root, &name).map_err(DownloadFailure::Failed)?;
    if cancel.load(Ordering::Relaxed) {
        return Err(DownloadFailure::Cancelled);
    }

    let mut last_report = None;
    let bytes = fetch_bytes(
        env.client,
        &url,
        MAX_PACKAGE_BYTES,
        PACKAGE_TIMEOUT,
        cancel,
        |received, total, force| {
            report_progress(sender, generation, received, total, &mut last_report, force);
        },
    )
    .await?;

    let _ = sender.send(DownloadEvent::Verifying { generation });
    let package = package.clone();
    let cache_root = cache_root.to_path_buf();
    let pubkey = env.pubkey.to_owned();
    let stored = tokio::task::spawn_blocking(move || {
        verify_and_store(&bytes, &package, &pubkey, &cache_root, &name)
    })
    .await
    .map_err(|err| {
        DownloadFailure::Failed(format!("{}{err}", t(UpdateMsg::VerifyPackageFailedPrefix)))
    })?
    .map_err(DownloadFailure::Failed)?;
    if cancel.load(Ordering::Relaxed) {
        // 用户在校验期间取消了：别把这份文件留着当"已校验"。
        let _ = std::fs::remove_file(&stored.0);
        return Err(DownloadFailure::Cancelled);
    }
    Ok(stored)
}

/// 在内存里验包字节、签名和 trusted comment 里的版本，通过后才落盘。
///
/// 落盘的就是验签的那份字节：写到 `<缓存>/<哈希前 16 位>/<文件名>`，独占创建
/// （`create_new`），已有同名文件先删再建，删完被别的进程抢先建出来就报错而不是沿用。
/// 返回 `(路径, 大小, SHA-256)`。
fn verify_and_store(
    bytes: &[u8],
    package: &Package,
    pubkey: &str,
    cache_root: &Path,
    name: &str,
) -> Result<(PathBuf, u64, String), String> {
    zssh_update::verify_signature(bytes, &package.signature, pubkey, &package.version, true)
        .map_err(|err| crate::update_text::update_error(&err))?;
    let sha256 = sha256_hex(bytes);
    // 哈希目录让不同版本 / 不同内容互不覆盖：排队等退出安装的那份不会被后来的下载截断。
    let dir = cache_root.join(&sha256[..16]);
    let path = cache_path(&dir, name)?;
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("{}{err}", t(UpdateMsg::CreateCacheDirFailedPrefix)))?;
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(format!(
                "{}{err}",
                t(UpdateMsg::CleanOldPackageFailedPrefix)
            ));
        }
    }
    let write = || -> std::io::Result<()> {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()
    };
    if let Err(err) = write() {
        // 半截文件不能留着被误当成完整安装包。
        let _ = std::fs::remove_file(&path);
        return Err(format!("{}{err}", t(UpdateMsg::WritePackageFailedPrefix)));
    }
    Ok((path, bytes.len() as u64, sha256))
}

/// 验签用的公钥：调试钩子优先，否则用编译期内嵌的那一把。
pub(crate) fn update_pubkey() -> Result<String, String> {
    resolve_pubkey(
        crate::dev_hooks::var("ZSSH_UPDATE_PUBKEY"),
        zssh_update::embedded_config(),
    )
}

fn resolve_pubkey(
    hook: Option<String>,
    embedded: Result<zssh_update::UpdaterConfig, String>,
) -> Result<String, String> {
    let key = match hook
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        Some(key) => key,
        None => embedded.map(|config| config.pubkey).map_err(|err| {
            format!(
                "{}{}{err}",
                t(UpdateMsg::PubkeyUnavailable),
                t(UpdateMsg::ErrorChainSeparator)
            )
        })?,
    };
    // 占位公钥验不了任何签名：与其让后面报一个看不懂的验签错误，不如在这里就明说。
    if zssh_update::is_placeholder_pubkey(&key) {
        return Err(t(UpdateMsg::SigningUnconfigured).to_owned());
    }
    Ok(key)
}

/// 进度节流：首个数据块和每 100ms 各上报一次，结束再补一次。
fn report_progress(
    sender: &Sender<DownloadEvent>,
    generation: u64,
    received: u64,
    total: Option<u64>,
    last_report: &mut Option<Instant>,
    force: bool,
) {
    let due = force || last_report.is_none_or(|at| at.elapsed() >= REPORT_INTERVAL);
    if due {
        let _ = sender.send(DownloadEvent::Progress {
            generation,
            received,
            total,
        });
        *last_report = Some(Instant::now());
    }
}

/// 总大小未知或为 0 时只显示已下载字节数。
fn progress_text(received: u64, total: Option<u64>) -> String {
    let prefix = t(UpdateMsg::DownloadedPrefix);
    match total {
        Some(total) if total > 0 => format!("{prefix}{received} / {total}"),
        _ => format!("{prefix}{received}"),
    }
}

fn file_summary(path: &Path, size: u64) -> String {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(t(UpdateMsg::UnknownFile));
    format!(
        "{}{name}{}{size}{}{}",
        t(UpdateMsg::FileSummaryPrefix),
        t(UpdateMsg::OpenParen),
        t(UpdateMsg::FileSizeSuffix),
        t(UpdateMsg::CloseParen)
    )
}

fn update_cache_dir() -> PathBuf {
    crate::settings::data_dir().join(CACHE_DIR)
}

/// 把「跳过此版本」写进本机设置（`update_skipped_version`，不参与同步）。
fn record_skipped_version(version: &str) -> Result<(), String> {
    let db = crate::host_db::open_database().map_err(|err| {
        format!(
            "{}{}",
            t(UpdateMsg::RememberSkipFailedPrefix),
            crate::error_text::error_text(&err)
        )
    })?;
    skip_version_in(&db, version)
}

/// [`record_skipped_version`] 的可注入版本（测试用内存库）。
fn skip_version_in(db: &zssh_core::store::Database, version: &str) -> Result<(), String> {
    zssh_core::updater::skip_version(db, version).map_err(|err| {
        format!(
            "{}{}",
            t(UpdateMsg::RememberSkipFailedPrefix),
            crate::error_text::error_text(&err)
        )
    })
}

fn current_exe_args() -> Vec<String> {
    std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

/// 生成安装计划；不启动任何进程。失败时返回可直接显示的中文原因。
fn build_install_plan(
    installer: &Path,
    cache_root: &Path,
    version: &str,
    restart: bool,
    current_exe_args: &[String],
    sha256: &str,
) -> Result<InstallPlan, String> {
    let request = InstallRequest {
        installer: installer.to_path_buf(),
        cache_root: cache_root.to_path_buf(),
        version: version.to_owned(),
        restart,
        current_exe_args: current_exe_args.to_vec(),
        sha256: sha256.to_owned(),
    };
    plan_install(&request).map_err(|err| crate::update_text::install_error(&err))
}

/// 等界面把「安装器已启动」画出来再退出应用；不碰安装器进程，它自己继续跑。
fn quit_after_paint(cx: &App) {
    cx.spawn(async move |cx| {
        cx.background_executor().timer(EXIT_DELAY).await;
        cx.update(|app| app.quit());
    })
    .detach();
}

/// 取进程级的待安装队列。
fn pending_install() -> &'static Mutex<Option<InstallPlan>> {
    PENDING_INSTALL.get_or_init(|| Mutex::new(None))
}

/// 锁中毒只说明别的线程持锁时 panic 过，队列本身还能用，取回内部值继续。
fn locked_pending_install() -> MutexGuard<'static, Option<InstallPlan>> {
    match pending_install().lock() {
        Ok(guard) => guard,
        Err(poison) => poison.into_inner(),
    }
}

/// 排队等待退出后安装；已有计划直接覆盖，允许重复排队。
pub fn queue_install(plan: InstallPlan) {
    *locked_pending_install() = Some(plan);
}

/// 取走排队的计划；没有排队时返回 `None`。测试也用它检查队列。
fn take_pending_install() -> Option<InstallPlan> {
    locked_pending_install().take()
}

/// 清掉排队的计划；立即安装成功后调用，免得退出时再启动一次。
fn clear_pending_install() {
    *locked_pending_install() = None;
}

/// 排队等退出后安装的版本号（不带 `v`）；托盘据此显示「重启安装 vX」。没有排队时为 `None`。
///
/// 只有 Windows 有「退出后安装」排队（安装器可以等程序退出再启动）；Linux 的安装要用户在场（授权框），
/// 装好后由更新窗口里的「立即重启」接手，没有排队的计划，所以只有 Windows 的托盘会显示「重启安装 vX」。
#[cfg(windows)]
pub fn pending_install_version() -> Option<String> {
    locked_pending_install()
        .as_ref()
        .map(|plan| plan.version.clone())
}

/// 托盘的「重启安装」：把排队的计划改成"装完重启 ZeeTerm"。返回是否真有计划可改。
/// 实际启动安装器仍然在程序退出时（`run_pending_install`），由调用方随后退出程序。
#[cfg(windows)]
pub fn arm_pending_restart() -> bool {
    let mut slot = locked_pending_install();
    match slot.take() {
        Some(plan) => {
            *slot = Some(plan.with_restart(&current_exe_args()));
            true
        }
        None => false,
    }
}

/// 退出时启动排队好的安装器；由 `App::on_app_quit` 调用。
///
/// GPUI 给退出回调的时间很短，所以这里只做一次同步的 `ShellExecuteW`，
/// 不等待安装器。队列为空、启动失败都只记日志，不能 panic 也不能卡住退出。
pub fn run_pending_install() {
    let Some(plan) = take_pending_install() else {
        return;
    };
    match launch_installer(&plan) {
        Ok(()) => tracing::info!(
            version = %plan.version,
            program = %plan.program.display(),
            "退出时已启动安装器" // i18n-allow: 仅日志
        ),
        Err(err) => tracing::warn!(
            version = %plan.version,
            program = %plan.program.display(),
            %err,
            "退出时启动安装器失败" // i18n-allow: 仅日志
        ),
    }
}

/// 启动已校验的安装器。启动前一定先复验文件哈希：验签到这里可能隔了很久
/// （"退出后安装"），缓存目录又能被同用户的其他进程写入。
fn launch_installer(plan: &InstallPlan) -> Result<(), String> {
    launch_if_unchanged(plan, shell_execute_installer)
}

/// 复验通过才调用 `launch`；不一致或读不到文件就拒绝，一个字节的安装器都不启动。
/// `launch` 由调用方注入，测试里换成假实现。
///
/// 复验用 [`zssh_update::install::open_and_verify`]：它以"只共享读"打开安装包并把句柄
/// 一直持有到 `launch` 返回，所以复验和启动之间别的进程替换不了这个文件
/// （审查 M-18：以前只是"读完哈希再启动"，中间留了一个毫秒级窗口）。
fn launch_if_unchanged(
    plan: &InstallPlan,
    launch: impl FnOnce(&InstallPlan) -> Result<(), String>,
) -> Result<(), String> {
    let held = zssh_update::install::open_and_verify(plan).map_err(|err| {
        format!(
            "{}{}",
            t(UpdateMsg::PackageChangedRefusedPrefix),
            crate::update_text::install_error(&err)
        )
    })?;
    let result = launch(plan);
    drop(held);
    result
}

/// 用 `ShellExecuteW` 启动已校验的安装器。
///
/// 工作目录取安装器所在目录，参数按 Windows 命令行规则加引号；返回值大于 32
/// 才算成功，否则把错误码交给界面，程序继续运行。
#[cfg(windows)]
fn shell_execute_installer(plan: &InstallPlan) -> Result<(), String> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt as _;

    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{PCWSTR, w};

    fn wide(text: &OsStr) -> Vec<u16> {
        text.encode_wide().chain(std::iter::once(0)).collect()
    }

    let program = wide(plan.program.as_os_str());
    let args = quote_args(&plan.args);
    let args = wide(OsStr::new(&args));
    // 安装计划里的路径已经规范化过，父目录一定存在；空父目录交给系统默认值。
    let working_dir = plan.program.parent().unwrap_or(Path::new(""));
    let working_dir = (!working_dir.as_os_str().is_empty()).then(|| wide(working_dir.as_os_str()));

    // SAFETY: 各宽字符串都以 0 结尾，并且在调用期间一直存活。
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(program.as_ptr()),
            PCWSTR(args.as_ptr()),
            working_dir
                .as_deref()
                .map_or(PCWSTR::null(), |dir| PCWSTR(dir.as_ptr())),
            SW_SHOWNORMAL,
        )
    };
    // 文档：返回值大于 32 表示成功，否则是错误码。
    let code = result.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(format!("{}{code}", t(UpdateMsg::ShellExecuteErrorPrefix)))
    }
}

#[cfg(not(windows))]
fn shell_execute_installer(_plan: &InstallPlan) -> Result<(), String> {
    Err(t(UpdateMsg::InstallerWindowsOnly).to_owned())
}

/// 按 Windows 命令行规则拼一组参数。
#[cfg(any(windows, test))]
fn quote_args(args: &[String]) -> String {
    let mut line = String::new();
    for (index, arg) in args.iter().enumerate() {
        if index > 0 {
            line.push(' ');
        }
        push_quoted_arg(&mut line, arg);
    }
    line
}

/// 追加一个参数：空参数、含空白或引号的参数用双引号包起来。
///
/// 引号前的反斜杠要加倍再转义引号，结尾的反斜杠也要加倍，否则会被当成
/// 转义后面的结束引号。
#[cfg(any(windows, test))]
fn push_quoted_arg(line: &mut String, arg: &str) {
    let needs_quotes = arg.is_empty() || arg.chars().any(|ch| ch.is_whitespace() || ch == '"');
    if !needs_quotes {
        line.push_str(arg);
        return;
    }
    line.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                push_repeated(line, '\\', backslashes * 2 + 1);
                line.push('"');
                backslashes = 0;
            }
            _ => {
                push_repeated(line, '\\', backslashes);
                line.push(ch);
                backslashes = 0;
            }
        }
    }
    push_repeated(line, '\\', backslashes * 2);
    line.push('"');
}

#[cfg(any(windows, test))]
fn push_repeated(line: &mut String, ch: char, count: usize) {
    for _ in 0..count {
        line.push(ch);
    }
}

/// 从下载地址取文件名：只看最后一段，挡掉空名、`.`/`..` 和 Windows 非法字符。
fn download_file_name(url: &str) -> Result<String, String> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|err| format!("{}{err}", t(UpdateMsg::UpdateUrlInvalidPrefix)))?;
    match parsed.scheme() {
        "http" | "https" | "file" => {}
        other => {
            return Err(format!(
                "{}{other}",
                t(UpdateMsg::UnsupportedUrlSchemePrefix)
            ));
        }
    }
    let raw = parsed
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .unwrap_or("");
    let name = raw.trim();
    if name.is_empty() || name == "." || name == ".." {
        return Err(t(UpdateMsg::UrlWithoutFileName).to_owned());
    }
    validate_file_name(name)?;
    Ok(name.to_owned())
}

/// 缓存目录里的落盘位置；文件名必须只有一个普通段，不能跳出缓存根目录。
fn cache_path(cache_root: &Path, name: &str) -> Result<PathBuf, String> {
    validate_file_name(name)?;
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(format!("{}{name}", t(UpdateMsg::InvalidFileNamePrefix)));
    }
    Ok(cache_root.join(name))
}

fn validate_file_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err(t(UpdateMsg::EmptyFileName).to_owned());
    }
    if name.chars().any(|ch| {
        ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
    }) {
        return Err(format!("{}{name}", t(UpdateMsg::InvalidFileNamePrefix)));
    }
    Ok(())
}

/// 软件联网用的代理：调试钩子 `ZSSH_UPDATE_PROXY` 优先，其次是设置里的「软件网络」代理
/// （跟随系统代理 / SSH 默认代理 / 代理库里的某个 / 直连，和配置同步、下载 opencode 是同一个设置）。
/// `Ok(None)` 是直连；设置库打不开或代理配置有问题（例如指定的代理被删了）返回原因，不静默改成直连。
fn update_proxy() -> Result<Option<String>, String> {
    if let Some(hook) = crate::dev_hooks::var("ZSSH_UPDATE_PROXY")
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        return Ok(Some(hook));
    }
    let db = zssh_core::store::Database::open(&crate::settings::data_dir().join("zssh.db"))
        .map_err(|err| {
            format!(
                "{}{}",
                t(UpdateMsg::OpenSettingsDbFailedPrefix),
                crate::error_text::error_text(&err)
            )
        })?;
    proxy_from_settings(&db)
}

/// 设置库里的「软件网络」代理 -> 代理地址（含账号密码，不能打印）；`None` 是直连。
fn proxy_from_settings(db: &zssh_core::store::Database) -> Result<Option<String>, String> {
    zssh_core::sync::app_proxy::app_proxy(db)
        .map(|proxy| proxy.map(|proxy| proxy.url_with_credentials()))
        .map_err(|err| format!("{}{err}", t(UpdateMsg::ProxySettingsProblemPrefix)))
}

/// 不带代理的客户端构造器：超时、UA 和重定向规则都在这里，测试可以直接拿它连本地服务。
fn update_client_builder(allow_insecure: bool) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .user_agent(concat!("ZeeTerm/", env!("CARGO_PKG_VERSION")))
        .redirect(redirect_policy(allow_insecure))
}

fn update_client() -> Result<reqwest::Client, String> {
    let mut builder = update_client_builder(allow_insecure_sources());
    builder = match update_proxy()? {
        // 本机回环地址永远不走代理（调试用的本机夹具、以及误填的回环清单地址都直连）。
        Some(proxy) => builder.proxy(
            reqwest::Proxy::all(&proxy)
                .map_err(|err| format!("{}{err}", t(UpdateMsg::UpdateProxyConfigErrorPrefix)))?
                .no_proxy(reqwest::NoProxy::from_string("localhost,127.0.0.1,::1")),
        ),
        // 直连：也不读环境变量里的代理，和软件网络的其他联网一致。
        None => builder.no_proxy(),
    };
    builder
        .build()
        .map_err(|err| format!("{}{err}", t(UpdateMsg::CreateDownloadClientFailedPrefix)))
}

/// 当前版本：编译进来的包版本。调试钩子 `ZSSH_CURRENT_VERSION` 可以覆盖（为了测更新流程）。
pub(crate) fn current_version() -> String {
    resolve_current_version(crate::dev_hooks::var("ZSSH_CURRENT_VERSION"))
}

fn resolve_current_version(hook: Option<String>) -> String {
    hook.map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned())
}

/// 解析清单并按 Windows 的平台 key 选包（Windows 更新的测试入口；产品代码走 [`evaluate_manifest_for`]）。
#[cfg(test)]
fn evaluate_manifest(text: &str, current: &str, allow_insecure: bool) -> Checked {
    evaluate_manifest_for(text, current, allow_insecure, &Platform::Windows)
}

/// 解析清单并按本机平台选包；选出的安装包地址也要过协议检查。
///
/// Linux 上公告的新版本如果没有适用于本机安装方式的条目（老版本发布的清单只有 Windows 条目，
/// 或者 deb 安装的机器遇到只有 tar.gz 的清单），不算失败：给 [`CheckKind::FoundManual`]，界面引导到发布页。
/// Windows 上选不出包仍然是失败，行为和以前一样。
fn evaluate_manifest_for(
    text: &str,
    current: &str,
    allow_insecure: bool,
    platform: &Platform,
) -> Checked {
    match zssh_update::parse_manifest(text) {
        Ok(manifest) => {
            let outcome = manifest.select_for(current, platform.keys());
            if matches!(platform, Platform::Linux(_))
                && matches!(outcome, Err(UpdateError::NoPlatform))
            {
                let version = manifest.version.trim_start_matches('v').to_owned();
                return Checked {
                    kind: CheckKind::FoundManual,
                    status: format!(
                        "{}{version}\n{}",
                        t(UpdateMsg::FoundVersionPrefix),
                        t(UpdateMsg::NoPackageMessage)
                    ),
                    package: None,
                    notes: manifest.notes,
                    published: manifest.date,
                    release_version: Some(version),
                };
            }
            if let Ok(Some(package)) = &outcome
                && let Err(err) = check_update_url(&package.url, allow_insecure)
            {
                return Checked::failed(format!("{}{err}", t(UpdateMsg::CheckFailedPrefix)));
            }
            let status = status_text(&outcome, manifest.notes.as_deref());
            match outcome {
                Ok(Some(package)) => Checked {
                    kind: CheckKind::Found,
                    status,
                    release_version: Some(package.version.clone()),
                    package: Some(package),
                    notes: manifest.notes,
                    published: manifest.date,
                },
                Ok(None) => Checked {
                    kind: CheckKind::Latest,
                    status,
                    package: None,
                    notes: None,
                    published: None,
                    release_version: None,
                },
                Err(_) => Checked::failed(status),
            }
        }
        Err(err) => Checked::failed(status_text(&Err(err), None)),
    }
}

/// 纯文本状态，方便单测；文件 I/O 留在 [`check_manifest`] 里。
fn status_text(outcome: &Result<Option<Package>, UpdateError>, notes: Option<&str>) -> String {
    match outcome {
        Ok(None) => t(UpdateMsg::LatestVersion).to_owned(),
        Ok(Some(package)) => {
            let notes = notes
                .map(str::trim)
                .filter(|notes| !notes.is_empty())
                .unwrap_or(t(UpdateMsg::NoReleaseNotes));
            format!(
                "{}{}\n{}\n{}{}\n{}{}",
                t(UpdateMsg::FoundVersionPrefix),
                package.version,
                notes,
                t(UpdateMsg::PackageLabel),
                package.url,
                t(UpdateMsg::PlatformLabel),
                package.platform_key
            )
        }
        Err(err) => format!(
            "{}{}",
            t(UpdateMsg::CheckFailedPrefix),
            crate::update_text::update_error(err)
        ),
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn the_macos_no_update_text_exists_in_both_languages() {
        use crate::i18n::Msg as _;
        let zh = super::UpdateMsg::MacosNoAutoUpdate.zh();
        let en = super::UpdateMsg::MacosNoAutoUpdate.en();
        assert!(zh.contains("不会下载") && en.contains("will not download"));
        assert!(en.is_ascii(), "{en}");
    }
    // 不写 `use super::*`：父模块 glob 进来的 `gpui_kit::test` 会盖掉内置的
    // `#[test]` 属性宏，展开时直接撞递归上限。
    use super::{
        DownloadState, InstallState, advance_install, build_install_plan, cache_path,
        download_file_name, file_summary, progress_text, push_quoted_arg, queue_install,
        quote_args, run_install, state_after_check, take_pending_install,
    };
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard};
    use zssh_update::{InstallPlan, Package, UpdateError};

    /// 待安装队列是进程级全局，这几个用例必须串行，否则「空队列」用例可能
    /// 捡到别的用例刚排进去的计划，真的去启动安装器。
    fn queue_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        match LOCK.lock() {
            Ok(guard) => guard,
            Err(poison) => poison.into_inner(),
        }
    }

    fn sample_plan() -> InstallPlan {
        InstallPlan {
            program: PathBuf::from("C:\\cache\\zssh-setup.exe"),
            args: vec!["/P".to_owned(), "/UPDATE".to_owned()],
            restart: false,
            version: "0.3.0".to_owned(),
            sha256: "0".repeat(64),
        }
    }

    fn push_quoted(arg: &str) -> String {
        let mut line = String::new();
        push_quoted_arg(&mut line, arg);
        line
    }

    fn package() -> Package {
        Package {
            platform_key: "windows-x86_64-nsis".to_owned(),
            version: "0.3.0".to_owned(),
            url: "https://example.com/zssh-setup.exe".to_owned(),
            signature: "unused-in-window".to_owned(),
        }
    }

    #[test]
    fn no_update_says_latest() {
        assert_eq!(super::status_text(&Ok(None), None), "已是最新版本");
    }

    #[test]
    fn update_lists_version_notes_url_and_platform() {
        let text = super::status_text(&Ok(Some(package())), Some("修复若干问题"));
        assert!(text.contains("发现新版本 0.3.0"));
        assert!(text.contains("修复若干问题"));
        assert!(text.contains("https://example.com/zssh-setup.exe"));
        assert!(text.contains("windows-x86_64-nsis"));
    }

    #[test]
    fn update_without_notes_still_shows_package() {
        let text = super::status_text(&Ok(Some(package())), None);
        assert!(text.contains("（无更新说明）"));
        assert!(text.contains("安装包：https://example.com/zssh-setup.exe"));
    }

    #[test]
    fn error_keeps_update_error_text() {
        let text = super::status_text(&Err(UpdateError::NoPlatform), None);
        assert!(text.contains("没有适用于当前平台的安装包"));
    }

    #[test]
    fn url_file_name_uses_the_last_segment() {
        assert_eq!(
            download_file_name("https://example.com/a/b/zssh-setup.exe?token=1").unwrap(),
            "zssh-setup.exe"
        );
        assert_eq!(
            download_file_name("file:///C:/tmp/zssh-setup.exe").unwrap(),
            "zssh-setup.exe"
        );
    }

    #[test]
    fn url_without_a_safe_file_name_is_rejected() {
        for url in [
            "",
            "https://example.com/",
            "https://example.com",
            "https://example.com/dir/..",
            "ftp://example.com/zssh-setup.exe",
        ] {
            assert!(download_file_name(url).is_err(), "{url}");
        }
    }

    #[test]
    fn cache_path_rejects_paths_outside_the_cache() {
        let root = PathBuf::from("cache-root");
        assert_eq!(
            cache_path(&root, "zssh-setup.exe").unwrap(),
            root.join("zssh-setup.exe")
        );
        for name in [
            "",
            "..",
            "..\\evil.exe",
            "C:\\evil.exe",
            "sub\\evil.exe",
            "/etc/passwd",
        ] {
            assert!(cache_path(&root, name).is_err(), "{name}");
        }
    }

    #[test]
    fn progress_text_handles_known_and_unknown_totals() {
        assert_eq!(progress_text(10, Some(100)), "已下载 10 / 100");
        assert_eq!(progress_text(10, None), "已下载 10");
        assert_eq!(progress_text(0, Some(0)), "已下载 0");
        assert_eq!(progress_text(10, Some(0)), "已下载 10");
    }

    #[test]
    fn download_moves_from_downloading_to_verifying_to_verified() {
        let mut state = DownloadState::default();
        assert_eq!(state, DownloadState::Idle);

        state.start();
        state.progress(5, Some(10));
        assert_eq!(
            state,
            DownloadState::Downloading {
                received: 5,
                total: Some(10),
            }
        );

        state.begin_verify();
        assert_eq!(state, DownloadState::Verifying);

        state.verified(PathBuf::from("zssh-setup.exe"), 10, "ab".repeat(32));
        assert_eq!(
            state,
            DownloadState::Verified {
                path: PathBuf::from("zssh-setup.exe"),
                size: 10,
                sha256: "ab".repeat(32),
            }
        );
        assert!(!state.can_start());
    }

    #[test]
    fn failed_download_keeps_the_error_and_can_retry() {
        let mut state = DownloadState::default();
        state.start();
        state.fail("下载失败：连接被拒绝".to_owned());
        assert_eq!(
            state,
            DownloadState::Failed("下载失败：连接被拒绝".to_owned())
        );
        assert_eq!(state.text().as_deref(), Some("下载失败：连接被拒绝"));
        assert!(state.can_start());
    }

    #[test]
    fn cancel_marks_the_download_cancelled_and_can_retry() {
        let mut state = DownloadState::default();
        state.start();
        state.cancel();
        assert_eq!(state, DownloadState::Cancelled);
        assert_eq!(state.text().as_deref(), Some("已取消下载"));
        assert!(state.can_start());
    }

    #[test]
    fn file_summary_has_name_and_size() {
        let text = file_summary(Path::new("C:\\cache\\zssh-setup.exe"), 42);
        assert!(text.contains("zssh-setup.exe"));
        assert!(text.contains("42"));
    }

    #[test]
    fn install_plan_is_built_without_running_the_installer() {
        let root =
            std::env::temp_dir().join(format!("zssh-update-demo-{}-install", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时缓存目录");
        let installer = root.join("Z-SSH_0.3.0_x64-setup.exe");
        std::fs::write(&installer, b"fake installer").expect("写安装包");

        let plan = build_install_plan(
            &installer,
            &root,
            "0.3.0",
            true,
            &["--minimized".to_owned()],
            &zssh_update::sha256_hex(b"fake installer"),
        )
        .expect("安装计划应通过");

        let command = zssh_update::command_line(&plan);
        assert!(command.contains("setup.exe"));
        assert!(command.contains("/P"));
        assert!(command.contains("/UPDATE"));
        assert!(command.contains("/R"));
        assert!(command.contains("/ARGS"));
        assert!(command.contains("--minimized"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_version_already_installed_in_this_run_goes_back_to_the_installed_state() {
        let restart = PathBuf::from("/usr/bin/zeeterm");
        // 本进程里装好过这个版本：不管之前是什么状态，都回到「已安装，重启后生效」。
        for before in [
            InstallState::Idle,
            InstallState::Failed("x".to_owned()),
            InstallState::Launched,
        ] {
            assert_eq!(
                state_after_check(before, Some(restart.clone())),
                InstallState::Installed {
                    restart: restart.clone()
                }
            );
        }
        // 没装过：原样保留（包括已经在界面上的失败原因）。
        assert_eq!(
            state_after_check(InstallState::Idle, None),
            InstallState::Idle
        );
        assert_eq!(
            state_after_check(InstallState::Failed("x".to_owned()), None),
            InstallState::Failed("x".to_owned())
        );
    }

    #[test]
    fn one_click_builds_the_plan_and_launches() {
        let plan = sample_plan();
        let mut launches = 0;
        let (state, launched) = advance_install(
            InstallState::Idle,
            || Ok(plan.clone()),
            |launched_plan| {
                assert_eq!(launched_plan, &plan);
                launches += 1;
                Ok(())
            },
        );
        assert!(launched);
        assert_eq!(launches, 1);
        assert_eq!(state, InstallState::Launched);
    }

    #[test]
    fn clicking_again_after_launch_does_not_launch_twice() {
        let (state, launched) = advance_install(
            InstallState::Launched,
            || panic!("已启动后不能再生成计划"),
            |_| panic!("已启动后不能再启动安装器"),
        );
        assert!(!launched);
        assert_eq!(state, InstallState::Launched);
    }

    #[test]
    fn plan_failure_becomes_failed_and_a_retry_can_launch() {
        let (state, launched) = advance_install(
            InstallState::Idle,
            || Err("安装包不存在".to_owned()),
            |_| panic!("计划失败时不能启动安装器"),
        );
        assert!(!launched);
        assert_eq!(
            state,
            InstallState::Failed("准备安装失败：安装包不存在".to_owned())
        );

        // 失败后按钮仍可点：重试一次就装。
        let mut launches = 0;
        let (state, launched) = advance_install(
            state,
            || Ok(sample_plan()),
            |_| {
                launches += 1;
                Ok(())
            },
        );
        assert!(launched);
        assert_eq!(launches, 1);
        assert_eq!(state, InstallState::Launched);
    }

    #[test]
    fn launch_failure_reports_the_code_does_not_exit_and_can_be_retried() {
        let (state, launched) = advance_install(
            InstallState::Idle,
            || Ok(sample_plan()),
            |_| Err("ShellExecuteW 返回错误码 5".to_owned()),
        );
        assert!(!launched);
        assert_eq!(
            state,
            InstallState::Failed("启动安装器失败：ShellExecuteW 返回错误码 5".to_owned())
        );

        let (state, launched) = advance_install(state, || Ok(sample_plan()), |_| Ok(()));
        assert!(launched);
        assert_eq!(state, InstallState::Launched);
    }

    #[test]
    fn a_blocking_reason_stops_the_install_without_planning_or_launching() {
        let (state, blocked, launched) = run_install(
            Some("还有 1 个传输在进行".to_owned()),
            InstallState::Failed("启动安装器失败：x".to_owned()),
            || panic!("被拦住时不能生成计划"),
            |_| panic!("被拦住时不能启动安装器"),
        );
        assert!(!launched);
        assert_eq!(blocked.as_deref(), Some("还有 1 个传输在进行"));
        assert_eq!(state, InstallState::Failed("启动安装器失败：x".to_owned()));

        // 没有拦截时照常启动，并清掉旧的拦截原因。
        let (state, blocked, launched) =
            run_install(None, InstallState::Idle, || Ok(sample_plan()), |_| Ok(()));
        assert!(launched);
        assert_eq!(blocked, None);
        assert_eq!(state, InstallState::Launched);
    }

    #[test]
    fn quoting_handles_spaces_quotes_and_trailing_backslashes() {
        assert_eq!(push_quoted("--minimized"), "--minimized");
        assert_eq!(push_quoted(""), "\"\"");
        assert_eq!(
            push_quoted("C:\\Program Files\\Z-SSH"),
            "\"C:\\Program Files\\Z-SSH\""
        );
        assert_eq!(push_quoted("a\"b"), "\"a\\\"b\"");
        // 引号包住的结尾反斜杠要加倍，否则会被当成转义结束引号。
        assert_eq!(
            push_quoted("C:\\Program Files\\"),
            "\"C:\\Program Files\\\\\""
        );
        assert_eq!(
            quote_args(&[
                "/ARGS".to_owned(),
                "a b".to_owned(),
                "say \"hi\"".to_owned(),
            ]),
            "/ARGS \"a b\" \"say \\\"hi\\\"\""
        );
    }

    #[test]
    fn queued_install_is_taken_once() {
        let _guard = queue_lock();
        // 上一个用例若中途失败，队列里可能还留着计划，先清掉。
        let _ = take_pending_install();

        queue_install(sample_plan());
        assert_eq!(take_pending_install(), Some(sample_plan()));
        assert_eq!(take_pending_install(), None);
    }

    #[test]
    fn queueing_again_replaces_the_pending_plan() {
        let _guard = queue_lock();
        let _ = take_pending_install();

        queue_install(sample_plan());
        let mut replacement = sample_plan();
        replacement.version = "0.4.0".to_owned();
        replacement.restart = true;
        queue_install(replacement.clone());

        assert_eq!(take_pending_install(), Some(replacement));
        assert_eq!(take_pending_install(), None);
    }

    #[test]
    fn running_with_an_empty_queue_does_not_launch_anything() {
        let _guard = queue_lock();
        let _ = take_pending_install();

        // 队列为空时 `run_pending_install` 连 `ShellExecuteW` 都不会碰。
        super::run_pending_install();

        assert_eq!(take_pending_install(), None);
    }

    // ---- 协议、大小、超时、重定向 ----

    use super::{
        CheckKind, Checked, DownloadFailure, MAX_REDIRECTS, ManifestSource, SIGNING_UNCONFIGURED,
        UpdateMsg, allow_insecure_sources, append_capped, check_update_url, evaluate_manifest,
        fetch_bytes, launch_if_unchanged, redirect_allowed, resolve_current_version,
        resolve_manifest_sources, resolve_pubkey, skip_version_in, update_client_builder,
        verify_and_store,
    };
    use std::io::{Read as _, Write as _};
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("测试用 runtime")
            .block_on(future)
    }

    /// 不带系统代理的客户端：测试只连本机，不能被开发机上的系统代理截走。
    fn local_client(allow_insecure: bool) -> reqwest::Client {
        update_client_builder(allow_insecure)
            .no_proxy()
            .build()
            .expect("测试客户端")
    }

    /// 一次性本地 HTTP 服务：读完请求头后写出 `response`，`hold` 为真时再占着连接不放。
    fn serve_once(
        response: Vec<u8>,
        hold: Option<Duration>,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let port = listener.local_addr().expect("端口").port();
        let handle = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut seen = Vec::new();
            let mut buffer = [0u8; 1024];
            while !seen.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => seen.extend_from_slice(&buffer[..read]),
                }
            }
            // 客户端中途断开时写会失败，这正是"超限中止"想要的结果，不算错误。
            let _ = stream.write_all(&response);
            let _ = stream.flush();
            if let Some(hold) = hold {
                std::thread::sleep(hold);
            }
        });
        (format!("http://127.0.0.1:{port}/pkg.exe"), handle)
    }

    fn fetch(
        client: &reqwest::Client,
        url: &str,
        max_bytes: u64,
        timeout: Duration,
    ) -> Result<Vec<u8>, DownloadFailure> {
        let url = reqwest::Url::parse(url).expect("测试地址");
        let never = AtomicBool::new(false);
        block_on(fetch_bytes(
            client,
            &url,
            max_bytes,
            timeout,
            &never,
            |_, _, _| {},
        ))
    }

    fn failure_text(result: Result<Vec<u8>, DownloadFailure>) -> String {
        match result {
            Ok(bytes) => panic!("本该失败，却读到了 {} 字节", bytes.len()),
            Err(DownloadFailure::Cancelled) => "cancelled".to_owned(),
            Err(DownloadFailure::Failed(message)) => message,
        }
    }

    #[test]
    fn release_policy_rejects_http_and_file_sources() {
        for url in [
            "http://example.com/latest.json",
            "file:///C:/tmp/latest.json",
            "ftp://example.com/a.exe",
        ] {
            assert!(check_update_url(url, false).is_err(), "{url}");
        }
        assert!(check_update_url("https://example.com/latest.json", false).is_ok());
    }

    #[test]
    fn debug_policy_allows_local_sources_but_never_other_hosts_files() {
        assert!(check_update_url("http://127.0.0.1:1/a.exe", true).is_ok());
        assert!(check_update_url("file:///C:/tmp/a.exe", true).is_ok());
        // UNC 形式的 file 地址会触发 SMB 认证，调试版也不放。
        assert!(check_update_url("file://other-host/share/a.exe", true).is_err());
        assert!(check_update_url("ftp://example.com/a.exe", true).is_err());
    }

    /// release 构建里生效的就是 `false` 这一支（`cargo test --release` 时才会编进来）。
    #[cfg(not(debug_assertions))]
    #[test]
    fn this_release_build_rejects_http_and_file_sources() {
        assert!(!allow_insecure_sources());
        assert!(check_update_url("http://example.com/a.exe", allow_insecure_sources()).is_err());
        assert!(check_update_url("file:///C:/a.exe", allow_insecure_sources()).is_err());
    }

    #[test]
    fn insecure_sources_follow_the_build_kind() {
        assert_eq!(allow_insecure_sources(), cfg!(debug_assertions));
    }

    #[test]
    fn redirects_must_stay_on_https_and_be_limited() {
        assert!(redirect_allowed("https", 1, false).is_ok());
        assert!(redirect_allowed("http", 1, false).is_err());
        assert!(redirect_allowed("file", 1, false).is_err());
        assert!(redirect_allowed("http", 1, true).is_ok());
        assert!(redirect_allowed("https", MAX_REDIRECTS, false).is_ok());
        assert!(redirect_allowed("https", MAX_REDIRECTS + 1, false).is_err());
    }

    #[test]
    fn append_capped_stops_at_the_limit() {
        let mut buffer = Vec::new();
        assert!(append_capped(&mut buffer, b"abcd", 6).is_ok());
        assert!(append_capped(&mut buffer, b"ef", 6).is_ok());
        let err = append_capped(&mut buffer, b"g", 6).unwrap_err();
        assert!(err.contains("大小上限"), "{err}");
        assert_eq!(buffer, b"abcdef");
    }

    #[test]
    fn oversized_local_file_is_refused_before_reading_it() {
        let dir = std::env::temp_dir().join(format!("zssh-f2-big-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("big.exe");
        std::fs::write(&file, vec![7u8; 4096]).unwrap();
        let url = reqwest::Url::from_file_path(&file).expect("file 地址");
        let never = AtomicBool::new(false);
        let result = block_on(fetch_bytes(
            &local_client(true),
            &url,
            1024,
            Duration::from_secs(5),
            &never,
            |_, _, _| {},
        ));
        assert!(failure_text(result).contains("大小上限"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn http_download_within_the_limit_succeeds() {
        let body = b"installer bytes".to_vec();
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(&body);
        let (url, server) = serve_once(response, None);
        let got = fetch(&local_client(true), &url, 1024, Duration::from_secs(5));
        assert_eq!(got.ok(), Some(body));
        let _ = server.join();
    }

    #[test]
    fn http_download_aborts_when_content_length_exceeds_the_limit() {
        let response =
            b"HTTP/1.1 200 OK\r\nContent-Length: 999999\r\nConnection: close\r\n\r\nxx".to_vec();
        let (url, server) = serve_once(response, None);
        let text = failure_text(fetch(
            &local_client(true),
            &url,
            1024,
            Duration::from_secs(5),
        ));
        assert!(text.contains("大小上限"), "{text}");
        let _ = server.join();
    }

    #[test]
    fn http_download_aborts_when_a_stream_without_length_keeps_going() {
        // 没有 Content-Length 的响应：只能靠累计字节数拦住。
        let mut response = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        response.extend(std::iter::repeat_n(b'x', 64 * 1024));
        let (url, server) = serve_once(response, None);
        let text = failure_text(fetch(
            &local_client(true),
            &url,
            1000,
            Duration::from_secs(5),
        ));
        assert!(text.contains("大小上限"), "{text}");
        let _ = server.join();
    }

    #[test]
    fn http_download_aborts_after_the_total_time_limit() {
        // 头发出去了，响应体一直不来：总时长上限必须能打断它。
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npartial".to_vec();
        let (url, server) = serve_once(response, Some(Duration::from_secs(3)));
        let started = std::time::Instant::now();
        let text = failure_text(fetch(
            &local_client(true),
            &url,
            1024,
            Duration::from_millis(300),
        ));
        assert!(started.elapsed() < Duration::from_secs(3), "{text}");
        assert!(text.contains("下载失败"), "{text}");
        let _ = server.join();
    }

    #[test]
    fn redirect_to_a_non_https_address_is_refused_by_the_client() {
        let (url, server) = serve_once(
            b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/other.exe\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec(),
            None,
        );
        // 发行版配置（allow_insecure = false）：入口是调试用的 http，但跳转目标同样是 http，必须拒绝。
        let text = failure_text(fetch(
            &local_client(false),
            &url,
            1024,
            Duration::from_secs(5),
        ));
        assert!(text.contains("重定向"), "{text}");
        let _ = server.join();
    }

    // ---- 验签落盘、复验 ----

    fn keypair() -> (minisign::KeyPair, String) {
        use base64::Engine as _;
        let kp = minisign::KeyPair::generate_unencrypted_keypair().expect("生成测试密钥");
        let text = kp.pk.to_box().expect("公钥文本").to_string();
        (kp, base64::engine::general_purpose::STANDARD.encode(text))
    }

    fn sign(kp: &minisign::KeyPair, data: &[u8], version: &str) -> String {
        use base64::Engine as _;
        let signed = minisign::sign(
            Some(&kp.pk),
            &kp.sk,
            data,
            Some(&format!("version:{version}")),
            Some("zssh test"),
        )
        .expect("签名");
        base64::engine::general_purpose::STANDARD.encode(signed.to_string())
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zssh-f2-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    fn signed_package(kp: &minisign::KeyPair, bytes: &[u8]) -> Package {
        Package {
            platform_key: "windows-x86_64-nsis".to_owned(),
            version: "0.3.0".to_owned(),
            url: "https://example.com/zssh-setup.exe".to_owned(),
            signature: sign(kp, bytes, "0.3.0"),
        }
    }

    #[test]
    fn verified_bytes_are_written_to_a_fresh_hash_named_file() {
        let (kp, pubkey) = keypair();
        let cache = temp_dir("store");
        let bytes = b"fake nsis installer bytes";
        let package = signed_package(&kp, bytes);

        let (path, size, sha256) =
            verify_and_store(bytes, &package, &pubkey, &cache, "zssh-setup.exe").expect("应通过");
        assert_eq!(size, bytes.len() as u64);
        assert_eq!(sha256, zssh_update::sha256_hex(bytes));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(path.starts_with(cache.join(&sha256[..16])));

        // 同一个包再下一次：旧文件被替换成同样的内容，不会因为"已存在"失败。
        let again =
            verify_and_store(bytes, &package, &pubkey, &cache, "zssh-setup.exe").expect("重下");
        assert_eq!(again.0, path);
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn a_file_planted_at_the_target_is_replaced_not_trusted() {
        let (kp, pubkey) = keypair();
        let cache = temp_dir("planted");
        let bytes = b"real installer";
        let package = signed_package(&kp, bytes);
        let dir = cache.join(&zssh_update::sha256_hex(bytes)[..16]);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("zssh-setup.exe"), b"planted").unwrap();

        let (path, _, _) =
            verify_and_store(bytes, &package, &pubkey, &cache, "zssh-setup.exe").expect("应通过");
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn unverified_bytes_never_reach_the_cache() {
        let (kp, pubkey) = keypair();
        let cache = temp_dir("tampered");
        let package = signed_package(&kp, b"original");

        let err =
            verify_and_store(b"tampered", &package, &pubkey, &cache, "zssh-setup.exe").unwrap_err();
        assert!(err.contains("签名"), "{err}");
        // 缓存里一个文件、一个子目录都不该出现。
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0);

        let (_, other_pubkey) = keypair();
        assert!(
            verify_and_store(
                b"original",
                &package,
                &other_pubkey,
                &cache,
                "zssh-setup.exe"
            )
            .is_err()
        );
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn launch_is_refused_when_the_file_changed_after_verification() {
        let (kp, pubkey) = keypair();
        let cache = temp_dir("swap");
        let bytes = b"real installer";
        let package = signed_package(&kp, bytes);
        let (path, _, sha256) =
            verify_and_store(bytes, &package, &pubkey, &cache, "zssh-setup.exe").unwrap();
        let plan = build_install_plan(&path, &cache, "0.3.0", false, &[], &sha256).expect("计划");

        // 没动过：放行（注入假的启动函数，不会真的起进程）。
        let mut launched = 0;
        launch_if_unchanged(&plan, |_| {
            launched += 1;
            Ok(())
        })
        .expect("未改动应放行");
        assert_eq!(launched, 1);

        // 验签之后被换掉：拒绝，启动函数一次都不能被调用。
        std::fs::write(&path, b"malicious replacement").unwrap();
        let err = launch_if_unchanged(&plan, |_| panic!("被改过的安装包不能启动")).unwrap_err();
        assert!(err.contains("不一致"), "{err}");

        // 文件没了：同样拒绝。
        std::fs::remove_file(&path).unwrap();
        assert!(launch_if_unchanged(&plan, |_| panic!("不能启动")).is_err());
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn queued_install_is_rechecked_when_the_app_quits() {
        let _guard = queue_lock();
        let _ = take_pending_install();
        let (kp, pubkey) = keypair();
        let cache = temp_dir("queued-swap");
        let bytes = b"real installer";
        let package = signed_package(&kp, bytes);
        let (path, _, sha256) =
            verify_and_store(bytes, &package, &pubkey, &cache, "zssh-setup.exe").unwrap();
        let plan = build_install_plan(&path, &cache, "0.3.0", false, &[], &sha256).expect("计划");
        queue_install(plan);

        // 排队之后文件被换掉：退出回调里的复验必须拦下，不能走到 ShellExecuteW。
        std::fs::write(&path, b"swapped while waiting for quit").unwrap();
        super::run_pending_install();
        assert_eq!(take_pending_install(), None, "队列应已被取走");
        let _ = std::fs::remove_dir_all(&cache);
    }

    // ---- 版本、公钥、更新源 ----

    /// 读仓库根的文件（相对路径）。文件不存在时返回 `None`，对应的比对跳过；
    /// 读失败（不是"不存在"）照样 panic，不当成"没有"。
    fn repo_root_file(relative: &str) -> Option<String> {
        let path: std::path::PathBuf = [env!("CARGO_MANIFEST_DIR"), "..", "..", relative]
            .iter()
            .collect();
        match std::fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => panic!("读 {} 失败：{err}", path.display()),
        }
    }

    fn workspace_version(manifest: &str) -> String {
        let section = manifest
            .split("[workspace.package]")
            .nth(1)
            .expect("Cargo.toml 应有 [workspace.package]");
        section
            .lines()
            .find_map(|line| {
                line.trim()
                    .strip_prefix("version")
                    .and_then(|rest| rest.trim_start().strip_prefix('='))
                    .map(|value| value.trim().trim_matches('"').to_owned())
            })
            .expect("workspace 应有版本号")
    }

    #[test]
    fn compiled_version_matches_the_stable_release_line() {
        assert_ne!(env!("CARGO_PKG_VERSION"), "0.0.0");
        // 版本号以 native 工作区为准；根 Cargo.toml 存在时才比对（文件不存在就跳过）。
        match repo_root_file("Cargo.toml") {
            Some(root) => assert_eq!(env!("CARGO_PKG_VERSION"), workspace_version(&root)),
            None => println!("跳过与根 Cargo.toml 的版本比对：文件不存在"),
        }
    }

    #[test]
    fn current_version_defaults_to_the_compiled_version() {
        assert_eq!(resolve_current_version(None), env!("CARGO_PKG_VERSION"));
        assert_eq!(
            resolve_current_version(Some("  ".to_owned())),
            env!("CARGO_PKG_VERSION")
        );
        // 钩子在调试构建里才会被 `dev_hooks` 放进来；这里只验证"给了值就用它"。
        assert_eq!(resolve_current_version(Some(" 0.1.0 ".to_owned())), "0.1.0");
    }

    #[test]
    fn the_public_key_comes_from_the_hook_or_the_config_and_a_placeholder_is_refused() {
        let configured = |pubkey: &str| zssh_update::UpdaterConfig {
            pubkey: pubkey.to_owned(),
            endpoints: vec!["https://example.com/latest.json".to_owned()],
        };
        assert_eq!(
            resolve_pubkey(None, Ok(configured("real-key"))).unwrap(),
            "real-key"
        );
        assert_eq!(
            resolve_pubkey(Some(" ".to_owned()), Ok(configured("real-key"))).unwrap(),
            "real-key"
        );
        assert_eq!(
            resolve_pubkey(Some(" custom ".to_owned()), Ok(configured("real-key"))).unwrap(),
            "custom"
        );
        let err = resolve_pubkey(None, Err("坏了".to_owned())).unwrap_err();
        assert!(
            err.starts_with(crate::i18n::t(UpdateMsg::PubkeyUnavailable)),
            "{err}"
        );

        // 占位公钥：无论来自配置还是钩子，都明说"更新签名未配置"，不当成一把能用的钥匙。
        assert_eq!(
            resolve_pubkey(None, Ok(configured("PLACEHOLDER-not-set"))).unwrap_err(),
            SIGNING_UNCONFIGURED
        );
        assert_eq!(
            resolve_pubkey(Some("placeholder".to_owned()), Ok(configured("real-key"))).unwrap_err(),
            SIGNING_UNCONFIGURED
        );
        // 钩子给了真公钥时盖过配置里的占位值（调试构建里测更新流程靠它）。
        assert_eq!(
            resolve_pubkey(Some("test-key".to_owned()), Ok(configured("PLACEHOLDER"))).unwrap(),
            "test-key"
        );
    }

    #[test]
    fn the_embedded_config_has_a_real_key_or_refuses_every_update() {
        // 仓库里的 updater.json 在"占位"和"已填真公钥"两种状态下都应自洽：
        // 占位 -> 拿不到公钥（检查 / 下载 / 安装都会被挡住）；已填 -> 就是它。
        let config = zssh_update::embedded_config().expect("内嵌配置应能解析");
        match resolve_pubkey(None, zssh_update::embedded_config()) {
            Ok(key) => {
                assert!(config.signing_configured());
                assert_eq!(key, config.pubkey);
            }
            Err(message) => {
                assert!(!config.signing_configured());
                assert_eq!(message, SIGNING_UNCONFIGURED);
            }
        }
    }

    #[test]
    fn manifest_sources_come_from_the_embedded_endpoints_by_default() {
        let embedded = zssh_update::embedded_config().expect("内嵌配置");
        let sources = resolve_manifest_sources(None, zssh_update::embedded_config());
        assert_eq!(
            sources,
            embedded
                .endpoints
                .iter()
                .cloned()
                .map(ManifestSource::Url)
                .collect::<Vec<_>>()
        );
        assert!(resolve_manifest_sources(None, Err("坏了".to_owned())).is_empty());
        // 钩子给了本地路径 / URL：只用钩子，不再混入内嵌地址。
        assert_eq!(
            resolve_manifest_sources(
                Some("C:\\tmp\\latest.json".into()),
                zssh_update::embedded_config()
            ),
            vec![ManifestSource::File(PathBuf::from("C:\\tmp\\latest.json"))]
        );
        assert_eq!(
            resolve_manifest_sources(
                Some("http://127.0.0.1:1/latest.json".into()),
                zssh_update::embedded_config()
            ),
            vec![ManifestSource::Url(
                "http://127.0.0.1:1/latest.json".to_owned()
            )]
        );
        assert_eq!(
            resolve_manifest_sources(Some("".into()), zssh_update::embedded_config()).len(),
            embedded.endpoints.len()
        );
    }

    #[test]
    fn manifest_pointing_at_a_non_https_package_is_refused_in_release_mode() {
        let manifest = r#"{
            "version": "0.3.0",
            "platforms": { "windows-x86_64-nsis": { "url": "http://example.com/a.exe", "signature": "sig" } }
        }"#;
        let refused = evaluate_manifest(manifest, "0.2.14", false);
        assert!(refused.package.is_none());
        assert_eq!(refused.kind, CheckKind::Failed);
        assert!(refused.status.contains("https"), "{}", refused.status);
        // 调试配置放行同一份清单。
        let allowed = evaluate_manifest(manifest, "0.2.14", true);
        assert!(allowed.package.is_some());
        assert_eq!(allowed.kind, CheckKind::Found);
    }

    const FOUND_MANIFEST: &str = r####"{
        "version": "0.3.1",
        "notes": "### 新增\n- 更新窗口有了版式",
        "pub_date": "2026-11-03T08:00:00Z",
        "platforms": { "windows-x86_64": { "url": "https://example.com/a.exe", "signature": "sig" } }
    }"####;

    #[test]
    fn a_found_update_carries_the_notes_and_the_release_date_for_the_window() {
        let checked = evaluate_manifest(FOUND_MANIFEST, "0.2.14", false);
        assert_eq!(checked.kind, CheckKind::Found);
        assert_eq!(
            checked.package.as_ref().map(|p| p.version.as_str()),
            Some("0.3.1")
        );
        assert_eq!(
            checked.notes.as_deref(),
            Some("### 新增\n- 更新窗口有了版式")
        );
        assert_eq!(checked.published.as_deref(), Some("2026-11-03T08:00:00Z"));
    }

    #[test]
    fn latest_and_failures_have_no_package_and_no_notes() {
        let latest = evaluate_manifest(FOUND_MANIFEST, "0.3.1", false);
        assert_eq!(latest.kind, CheckKind::Latest);
        assert_eq!(latest.status, "已是最新版本");
        assert!(latest.package.is_none() && latest.notes.is_none() && latest.published.is_none());

        let broken = evaluate_manifest("这不是 JSON", "0.2.14", false);
        assert_eq!(broken.kind, CheckKind::Failed);
        assert!(
            broken.status.contains("更新说明格式错误"),
            "{}",
            broken.status
        );
        assert!(broken.package.is_none());

        let no_platform = r#"{ "version": "0.3.1", "platforms": {} }"#;
        let failed = evaluate_manifest(no_platform, "0.2.14", false);
        assert_eq!(failed.kind, CheckKind::Failed);
        assert!(failed.status.contains("没有适用于当前平台"));
    }

    const LINUX_MANIFEST: &str = r####"{
        "version": "0.3.1",
        "notes": "Linux 版",
        "pub_date": "2026-11-06T08:00:00Z",
        "platforms": {
            "windows-x86_64": { "url": "https://example.com/a.exe", "signature": "sig" },
            "linux-x86_64-deb": { "url": "https://example.com/a.deb", "signature": "sig" },
            "linux-x86_64-tar": { "url": "https://example.com/a.tar.gz", "signature": "sig" }
        }
    }"####;

    #[test]
    fn linux_checks_pick_the_entry_matching_how_zeeterm_is_installed() {
        use super::{Platform, evaluate_manifest_for};
        use zssh_update::linux::Flavor;

        for (platform, key, url) in [
            (
                Platform::Linux(Flavor::Deb),
                "linux-x86_64-deb",
                "https://example.com/a.deb",
            ),
            (
                Platform::Linux(Flavor::Tar),
                "linux-x86_64-tar",
                "https://example.com/a.tar.gz",
            ),
            (
                Platform::Windows,
                "windows-x86_64",
                "https://example.com/a.exe",
            ),
        ] {
            let checked = evaluate_manifest_for(LINUX_MANIFEST, "0.2.14", false, &platform);
            assert_eq!(checked.kind, CheckKind::Found, "{platform:?}");
            let package = checked.package.expect("应选出包");
            assert_eq!(package.platform_key, key);
            assert_eq!(package.url, url);
            assert_eq!(checked.release_version.as_deref(), Some("0.3.1"));
        }
        // 已是最新：Linux 也一样。
        let latest = evaluate_manifest_for(
            LINUX_MANIFEST,
            "0.3.1",
            false,
            &Platform::Linux(Flavor::Tar),
        );
        assert_eq!(latest.kind, CheckKind::Latest);
        assert!(latest.release_version.is_none());
    }

    #[test]
    fn linux_without_a_matching_entry_is_a_release_page_hint_while_windows_still_fails() {
        use super::{Platform, evaluate_manifest_for};
        use zssh_update::linux::Flavor;

        for platform in [Platform::Linux(Flavor::Deb), Platform::Linux(Flavor::Tar)] {
            // 老版本发布的清单：只有 Windows 条目。
            let checked = evaluate_manifest_for(FOUND_MANIFEST, "0.2.14", false, &platform);
            assert_eq!(checked.kind, CheckKind::FoundManual, "{platform:?}");
            assert!(checked.package.is_none());
            assert_eq!(checked.release_version.as_deref(), Some("0.3.1"));
            assert_eq!(
                checked.notes.as_deref(),
                Some("### 新增\n- 更新窗口有了版式")
            );
            assert_eq!(checked.published.as_deref(), Some("2026-11-03T08:00:00Z"));
            assert!(
                checked.status.contains("发现新版本 0.3.1"),
                "{}",
                checked.status
            );
            assert!(checked.status.contains("发布页"), "{}", checked.status);
        }
        // 清单版本带 v 前缀也行。
        let prefixed = r#"{ "version": "v0.3.1", "platforms": {} }"#;
        let checked =
            evaluate_manifest_for(prefixed, "0.2.14", false, &Platform::Linux(Flavor::Tar));
        assert_eq!(checked.release_version.as_deref(), Some("0.3.1"));
        // Windows：没有适用条目仍然是失败，文字和以前一样。
        let failed = evaluate_manifest_for(prefixed, "0.2.14", false, &Platform::Windows);
        assert_eq!(failed.kind, CheckKind::Failed);
        assert!(
            failed.status.contains("没有适用于当前平台"),
            "{}",
            failed.status
        );
    }

    #[test]
    fn a_linux_entry_without_a_signature_or_with_a_plain_http_address_is_an_error_not_a_hint() {
        use super::{Platform, evaluate_manifest_for};
        use zssh_update::linux::Flavor;

        let platform = Platform::Linux(Flavor::Deb);
        let unsigned = r#"{ "version": "0.3.1", "platforms": { "linux-x86_64-deb": { "url": "https://example.com/a.deb" } } }"#;
        let checked = evaluate_manifest_for(unsigned, "0.2.14", false, &platform);
        assert_eq!(checked.kind, CheckKind::Failed);
        assert!(checked.status.contains("缺少签名"), "{}", checked.status);
        let insecure = r#"{ "version": "0.3.1", "platforms": { "linux-x86_64-deb": { "url": "http://example.com/a.deb", "signature": "sig" } } }"#;
        let checked = evaluate_manifest_for(insecure, "0.2.14", false, &platform);
        assert_eq!(checked.kind, CheckKind::Failed);
        // 调试构建放开 http 之后才选得出来。
        let allowed = evaluate_manifest_for(insecure, "0.2.14", true, &platform);
        assert_eq!(allowed.kind, CheckKind::Found);
    }

    #[test]
    fn the_failure_reason_drops_the_title_prefix_only() {
        let failed = Checked::failed("检查更新失败：没有适用于当前平台的安装包");
        assert_eq!(failed.failure_reason(), "没有适用于当前平台的安装包");
        let other =
            Checked::failed("读取更新说明失败：下载失败：服务器上找不到这个文件（HTTP 404）");
        assert_eq!(
            other.failure_reason(),
            "读取更新说明失败：下载失败：服务器上找不到这个文件（HTTP 404）"
        );
        assert_eq!(
            Checked::failed(SIGNING_UNCONFIGURED).failure_reason(),
            SIGNING_UNCONFIGURED
        );
    }

    #[test]
    fn skipping_a_version_is_remembered_and_the_auto_check_stops_announcing_it() {
        use zssh_core::store::Database;
        use zssh_core::updater::{UpdatePrefs, load_prefs, should_announce};

        let db = Database::open_in_memory().expect("内存库");
        let before = load_prefs(&db).expect("读偏好");
        assert_eq!(before, UpdatePrefs::default());
        assert!(crate::update_auto::should_announce_found(
            &before,
            "0.2.14",
            &package(),
            None
        ));

        // 窗口里的包是 0.3.0；点「跳过此版本」写的是它的版本号。
        skip_version_in(&db, &package().version).expect("写入设置");
        let after = load_prefs(&db).expect("读偏好");
        assert_eq!(after.skipped_version.as_deref(), Some("0.3.0"));
        assert!(
            !crate::update_auto::should_announce_found(&after, "0.2.14", &package(), None),
            "跳过的版本，自动检查不再提示"
        );
        // 更新的版本照常提示；手动「检查更新」不受跳过影响（用户主动问了就如实告诉他）。
        let newer = Package {
            version: "0.3.1".to_owned(),
            ..package()
        };
        assert!(crate::update_auto::should_announce_found(
            &after, "0.2.14", &newer, None
        ));
        assert!(should_announce(&after, "0.2.14", "0.3.0", true));
        // 带 v 前缀的写法也认。
        skip_version_in(&db, "v0.3.1").expect("写入设置");
        assert_eq!(
            load_prefs(&db).expect("读偏好").skipped_version.as_deref(),
            Some("0.3.1")
        );
    }

    #[test]
    fn the_window_hands_the_text_view_sanitised_notes_or_nothing() {
        assert_eq!(super::notes::display_markdown(None), None);
        assert_eq!(super::notes::display_markdown(Some("  \n ")), None);
        let shown = super::notes::display_markdown(Some(
            "### 新增\n\n<script>x()</script>\n\n![图](http://e.com/a.png)",
        ))
        .expect("有说明");
        assert!(shown.starts_with("### 新增"));
        // 标签被转义成字面文字：每个 `<` 前都有反斜杠，不会被当成 HTML。
        assert!(shown.contains(r"\<script"), "{shown}");
        assert!(!shown.replace(r"\<", "").contains('<'), "{shown}");
        assert!(!shown.contains("!["), "{shown}");
    }
}

#[cfg(test)]
mod e2e_tests;
