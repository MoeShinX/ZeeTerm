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
#[cfg(debug_assertions)]
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use ::gpui::{App, FocusHandle, Subscription, Task, Window, WindowBounds, WindowOptions};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::*;
use zssh_core::sftp::archive::{ArchiveCaps, CompressOutcome, ExtractOutcome, ExtractPlan};
#[cfg(debug_assertions)]
use zssh_core::sftp::path::join;
use zssh_core::sftp::size::SizeTotals;
use zssh_core::sftp::transfer::backend::{SftpChannelProvider, SftpEndpoints};
use zssh_core::sftp::transfer::{
    ConflictBatch, ConflictInfo, Direction, ProgressReport, TransferEngine, TransferEvent,
    TransferId, TransferState,
};
use zssh_core::sftp::{DirEntry, SftpChannel};
use zssh_core::store::Database;

use crate::error_text::error_text;
use crate::i18n::{Msg, t};
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
mod local_listing;
mod local_ops;
mod local_path;
mod local_start;
mod menu;
mod model;
mod move_flow;
mod move_to;
mod nav;
mod ops;
mod orphan;
mod pane;
mod paste;
mod props;
mod queue;
mod reconnect;
mod script;
mod transfer;
mod view;
mod view_dialogs;
mod view_move;

use dialogs::Dialog;
pub use drag::{SftpDrag, terminal_insert_text};
use engine::*;
pub(crate) use follow::FOLLOW_POLL_MS;
use local_listing::{LocalLister, LocalReply, SeqGate};
use menu::Side;
use model::NavMode;
use pane::Pane;

/// 本文件（SFTP 文件视图）自己的界面文案。
///
/// 只放应用自己的固定文案；远端或用户的数据（文件名、路径、用户名、属主 / 组、命令输出、
/// 核心库的错误原文）一律原样显示，不进这张表。带参数的句子保留 `format!`，这里只提供
/// 固定片段。id（`sftp-back-remote`、`xfer-pause`……）是稳定标识，不进这张表。
#[derive(Clone, Copy, Debug)]
enum SftpMsg {
    // ---- 通用 ----
    FilterPlaceholder,
    ConflictNamePlaceholder,
    NotConnectedYet,
    ItemCountSuffix,
    ParenOpen,
    ParenClose,
    // ---- 窗口 / 状态栏 ----
    ConnectingPrefix,
    ConnectFailedPrefix,
    CloseBusyPrefix,
    CloseBusySuffix,
    WaitingConflict,
    Deleting,
    CancelDeleting,
    DeleteProgressMiddle,
    DeleteProgressSuffix,
    ChmodProgressPrefix,
    ChmodProgressSuffix,
    // ---- 头部 ----
    HostFallback,
    PhaseConnecting,
    PhaseReady,
    PhaseFailed,
    PhaseDisconnected,
    DisconnectedStatus,
    TransferInterrupted,
    // ---- 跟随终端目录 ----
    FollowOn,
    FollowOff,
    FollowStopped,
    // ---- 复制标签 ----
    CloneNotConnected,
    CloneDisconnected,
    // ---- 本地目录读取 ----
    LocalReadInternalError,
    LocalReadQueueFull,
    // ---- 本地写操作 ----
    LocalDiskPrefix,
    LocalDiskSuffix,
    DirNameEmpty,
    FileNameEmpty,
    PathEmpty,
    AlreadyExists,
    SameNameFile,
    // ---- 拖放 ----
    DragControlChars,
    DragNotUtf8,
    // ---- 递归改权限 ----
    ChmodCancelledPrefix,
    ChmodCancelledMiddle,
    ChmodCancelledSuffix,
    ChmodDonePrefix,
    ChmodDoneMiddle,
    ChmodDoneSuffix,
    ChmodSkippedLinksPrefix,
    ChmodSkippedLinksSuffix,
    ChmodSkippedNonUtf8Prefix,
    ChmodSkippedNonUtf8Suffix,
    ChmodFailedPrefix,
    ChmodFailedMiddle,
    ChmodFailedSuffix,
    ChmodSymlinkRefused,
    // ---- 同名文件 / 传输引擎 ----
    NamePathSeparatorPrefix,
    ConflictNameNullChar,
    ConflictActionOverwrite,
    ConflictActionSkip,
    ConflictActionRename,
    ConflictActionResume,
    ConflictSource,
    ConflictTarget,
    ConflictSideMiddle,
    ConflictUnknownSizeTime,
    ConflictSizeEqual,
    ConflictTargetSmaller,
    ConflictTargetBigger,
    ConflictMtimeEqual,
    ConflictSourceNewer,
    ConflictTargetNewer,
    ConflictResumeHint,
    // ---- 删除结果 ----
    DeleteDonePrefix,
    DeleteCountOpen,
    DeleteCountClose,
    DeleteCancelledPrefix,
    DeleteCancelAtLeastMiddle,
    DeleteCancelExactMiddle,
    DeleteCancelExactSuffix,
    DeleteCancelUnknownSuffix,
    DeleteFailedPrefix,
    DeleteFailedCountOpen,
    DeleteFailedCountClose,
    DeleteNotRemovedMiddle,
    DeleteNotRemovedSuffix,
    // ---- 远端名字规则 / 写操作 ----
    OctalErrorPrefix,
    DirectionUpload,
    DirectionDownload,
    DirectionRemoteCopy,
    DirectionLocalCopy,
    StateQueued,
    StatePlanning,
    StateRunning,
    StatePaused,
    StateWaitingReconnect,
    StateWaitingConflict,
    StateDone,
    StateFailedPrefix,
    StateCancelled,
    StateSkipped,
    NoUsableFileName,
    NoUsableDirName,
    NameEmpty,
    NameCannotBePrefix,
    RemoteDirNotReady,
    OnlyRelativeNamesPrefix,
    NotUnderCurrentDirSuffix,
    MkdirFailedPrefix,
    MkdirFailedMiddle,
    MkdirDonePrefix,
    HookRenameFormatPrefix,
    HookRenameNameEmptyPrefix,
    HookChmodFormatPrefix,
    HookChmodNameEmptyPrefix,
    HookChmodModeInvalidPrefix,
    HookConflictInvalidPrefix,
    // ---- 右键菜单 ----
    MenuArchivePending,
    MenuNotUtf8,
    MenuFeaturePending,
    MenuSelectOne,
    MenuSingleOnly,
    MenuDriveNotAllowed,
    MenuSelectFile,
    MenuEditSingle,
    MenuDriveEdit,
    MenuFolderEdit,
    MenuSpecialEdit,
    MenuFileTooBigPrefix,
    MenuFileTooBigSuffix,
    MenuClipboardEmpty,
    MenuCrossSidePaste,
    MenuEnterFolder,
    MenuEnterDir,
    MenuNoUnzip,
    MenuNoTar,
    MenuNoZip,
    MenuExtractToPrefix,
    MenuExtractToSuffix,
    MenuExtractHere,
    MenuCompressAs,
    MenuNew,
    MenuFolder,
    MenuFile,
    MenuPaste,
    MenuRefresh,
    MenuProperties,
    MenuOpen,
    MenuEditBuiltin,
    MenuDownloadToLocal,
    MenuUploadToRemote,
    MenuDownloadTo,
    MenuCopy,
    MenuCopyPath,
    MenuRename,
    MenuMoveTo,
    MenuDelete,
    MenuPermissions,
    MenuTransferRetry,
    MenuTransferPause,
    MenuTransferResume,
    MenuTransferCancel,
    // ---- 对话框 ----
    DialogNameEmpty,
    DialogNameSlash,
    DialogNameBackslash,
    DialogNameBadCharPrefix,
    DialogLocalDirRequired,
    DialogWindowsBadChars,
    DialogPlaceRemote,
    DialogPlaceLocal,
    DialogDeleteTitlePrefix,
    DialogDeleteTitleMiddle,
    DialogDeleteTitleSuffix,
    DialogDeleteDescFolder,
    DialogDeleteDescLinks,
    DialogDeleteDescFiles,
    DialogDeleteMorePrefix,
    DialogDeleteMoreSuffix,
    // ---- 浏览 / 路径栏 ----
    NavReadingPrefix,
    NavReadingThisComputer,
    NavLocalPrefix,
    ItemsVisibleJoin,
    ItemsVisibleEnd,
    StartCustomMissing,
    NavLocalCountPrefix,
    NavNotADirectoryPrefix,
    NavLocalReadFailedPrefix,
    NavPathEmptyWait,
    NavLocalPathEmpty,
    NavPathPlaceholder,
    NavLocalEditorNotReady,
    // ---- 文件操作 ----
    OpsShellNotPosix,
    OpsPathCopied,
    OpsCancelNoOverwritePrefix,
    OpsCancelNoOverwriteSuffix,
    OpsNameFolder,
    OpsNameFile,
    OpsNameNewName,
    OpsStartNewFolder,
    OpsDoneNewFolder,
    OpsFailNewFolder,
    OpsStartNewFile,
    OpsDoneNewFile,
    OpsFailNewFile,
    OpsStartRename,
    OpsDoneRename,
    OpsFailRename,
    OpsFailedSuffix,
    OpsErrorJoin,
    OpsRuntimeUnavailable,
    OpsCreatingFilePrefix,
    OpsCreateFileFailedPrefix,
    OpsCreatedFilePrefix,
    OpsCreatingDirPrefix,
    OpsRenamingPrefix,
    OpsRenamedPrefix,
    OpsRenameFailedPrefix,
    OpsOverwriteRenamingPrefix,
    OpsRenamedOverwritePrefix,
    OpsDeletingWaitPrefix,
    OpsDeletingWaitSuffix,
    OpsDeletingWaitNextSuffix,
    OpsDeletingPrefix,
    OpsDeletingLocalPrefix,
    OpsDeletedLocalPrefix,
    OpsSkippedNonUtf8,
    OpsUnknownReason,
    OpsMoreFailuresPrefix,
    OpsMoreFailuresSuffix,
    OpsDeleteThreadFailedPrefix,
    OpsCancellingDeletePrefix,
    OpsEllipsis,
    OpsOctalPlaceholder,
    OpsChmodRunningPrefix,
    OpsChmodFailedPrefix,
    OpsChmodDonePrefix,
    OpsChmodRecursivePrefix,
    OpsChmodRecursiveFailedPrefix,
    OpsSelectItemsToTransfer,
    OpsDownloadDirPlaceholder,
    OpsDownloadDirPlaceholderPosix,
    OpsChooseDownloadFolder,
    OpsNotADirectory,
    OpsDirUnusablePrefix,
    OpsServerBusy,
    OpsCompressFailedPrefix,
    OpsExtractCancelled,
    OpsNotExtractedPrefix,
    OpsExtractFailedPrefix,
    OpsLocalDirHint,
    OpsRemoteDirHint,
    OpsNoItemsDownload,
    OpsNoItemsUpload,
    OpsQueuedDownloadPrefix,
    OpsQueuedUploadPrefix,
    OpsQueuedSomeSuffix,
    OpsAutoOpNotStartedPrefix,
    OpsThreadEndedPrefix,
    OpsListSeparator,
    // ---- 复制 / 粘贴 ----
    ClipIntoSelfPrefix,
    ClipIntoSelfSuffix,
    ClipMethodCopyData,
    ClipMethodServerCp,
    ClipMethodRelay,
    ClipMethodLocal,
    ClipPreparingPrefix,
    ClipPreparingSuffix,
    ClipFileIndexPrefix,
    ClipFileIndexSuffix,
    ClipCopyingPrefix,
    ClipCopiedPrefix,
    ClipCopiedMiddle,
    ClipCopiedTookMiddle,
    ClipCopiedTookSuffix,
    ClipQueuedMiddle,
    ClipSkippedPrefix,
    ClipSkippedSuffix,
    ClipCancelledPrefix,
    ClipFailedMiddle,
    ClipFailedItemOpen,
    ClipFailedItemClose,
    ClipFailedEllipsis,
    ClipNothingToPaste,
    ClipSeparator,
    ClipLessThanOne,
    CopySelectionNotUtf8,
    OpsNoRuntime,
    OpsPasteFailedPrefix,
    // ---- 粘贴规则 ----
    PasteNoTargetFolder,
    PasteRenameEmpty,
    PasteAlreadyExistsPrefix,
    PasteAlreadyExistsSuffix,
    PasteSourceClosed,
    PasteTargetUnavailablePrefix,
    PasteNotAFolderSuffix,
    PasteIntoSelfLinkSuffix,
    PasteMergeNeedsRelay,
    PasteAsYouChose,
    PasteShellNotPosixPrefix,
    PasteRelaySuffix,
    PasteSkippedLinksPrefix,
    PasteSkippedLinksSuffix,
    PasteServerCopyFailedPrefix,
    PasteOverwriteSelf,
    PasteTargetIsFolder,
    PasteTargetIsFile,
    PasteKindsDiffer,
    // ---- 移动到… ----
    MoveSameFolder,
    MoveIntoSelf,
    MoveCrossDevice,
    MoveDestNeedAbsoluteRemote,
    MoveDestNeedAbsoluteLocal,
    MoveDestNeedAbsoluteLocalPosix,
    MoveSelectItems,
    MoveDialogOnePrefix,
    MoveDialogOneSuffix,
    MoveDialogManyPrefix,
    MoveDialogManySuffix,
    MoveSummaryMovedOnePrefix,
    MoveSummaryMovedToMiddle,
    MoveSummaryMovedToClose,
    MoveSummaryMovedManyPrefix,
    MoveSummaryMovedManyMiddle,
    MoveSummarySkippedPrefix,
    MoveSummarySkippedSuffix,
    MoveSummaryFailedPrefix,
    MoveSummaryFailedMiddle,
    MoveSummaryFailedMore,
    MoveNoItemsMoved,
    MoveDetailSkippedPrefix,
    MoveDetailFailedPrefix,
    MoveDetailMiddle,
    MoveSummarySeparator,
    MoveWhyFolderMerge,
    MoveWhyTargetFile,
    MoveWhyTargetFolder,
    MoveSkipTargetExists,
    MoveReplaceFailed,
    MoveSkipFolderOnly,
    MoveSkipCancelled,
    MoveBlockedEditingSuffix,
    MoveTargetAlreadyExistsDetail,
    MoveBusy,
    MoveTargetPlaceholder,
    MoveNewFolderPlaceholder,
    MoveBrowseFailedPrefix,
    MoveDestUnavailablePrefix,
    MoveEditingInsideMiddle,
    MoveTransferringSuffix,
    MoveInProgressPrefix,
    MoveWaitingChoiceSuffix,
    // ---- 传输动作 ----
    TransferDirCannotEditSuffix,
    TransferNotUtf8CannotEditSuffix,
    TransferOpeningPrefix,
    TransferDoneSuffix,
    TransferFailedSuffix,
    TransferCancelledSuffix,
    TransferSkippedSuffix,
    TransferDropMissingPrefix,
    TransferDropNone,
    TransferDropCountPrefix,
    TransferRemotePathEmpty,
    TransferCreateLocalDirFailedPrefix,
    TransferUploadFilePrompt,
    TransferIsDirectorySuffix,
    TransferLocalFileMissingPrefix,
    TransferUploadDirPrompt,
    TransferLocalDirMissingPrefix,
    TransferEngineNotReady,
    TransferQueuedPrefix,
    TransferQueuedMiddle,
    TransferCancelling,
    TransferPausing,
    TransferResuming,
    TransferRetrying,
    TransferAlreadyFinished,
    TransferCannotRetry,
    TransferClearedPrefix,
    TransferClearedSuffix,
    TransferNothingCleared,
    TransferCannotResume,
    TransferRenameInvalidPrefix,
    TransferConflictPrefix,
    TransferConflictMorePrefix,
    TransferConflictMoreSuffix,
    TransferConflictApplyAllMiddle,
    TransferConflictApplyAllSuffix,
    TransferAutoPausePrefix,
    TransferAutoResumePrefix,
    TransferAutoCountSuffix,
    // ---- 文件列表：链接与列 ----
    LinkToPrefix,
    LinkLabel,
    LinkBrokenSuffix,
    ColumnName,
    ColumnSize,
    ColumnMtime,
    ColumnKind,
    ColumnPerms,
    ColumnOwner,
    KindDir,
    KindLink,
    KindDrive,
    KindFile,
    // ---- 属性对话框 ----
    PropsKindFolder,
    PropsKindFile,
    PropsKindSymlinkDir,
    PropsKindSymlinkFile,
    PropsKindSymlinkBroken,
    PropsKindDrive,
    PropsKindOther,
    PropsLinkBrokenSuffix,
    PropsBytesSuffix,
    PropsBytesParenOpen,
    PropsBytesParenClose,
    PropsFilesCount,
    PropsFoldersCount,
    PropsSkippedLinksPrefix,
    PropsSkippedLinksSuffix,
    PropsUnreadableFoldersSuffix,
    PropsComma,
    PropsNotComputed,
    PropsCalcRunningPrefix,
    PropsCalcCancelledPrefix,
    PropsCalcCancelledSuffix,
    PropsCalcFailedPrefix,
    PropsUnknownError,
    PropsRowName,
    PropsRowLocation,
    PropsRowType,
    PropsRowLinkTo,
    PropsRowSize,
    PropsRowMtime,
    PropsRowPerms,
    PropsRowOwner,
    // ---- 传输队列 ----
    QueueActiveSuffix,
    EtaMinuteUnit,
    EtaMinuteTail,
    EtaSecondUnit,
    EtaHourUnit,
    QueueFilesSuffix,
    QueueFilesMismatchPrefix,
    QueueFilesMismatchMiddle,
    QueueBytesMatchSuffix,
    QueueBytesMismatchPrefix,
    QueueBytesMismatchMiddle,
    QueueTimeUnderOneSecond,
    QueueTimePrefix,
    QueueTimeAvgMiddle,
    QueueTimeSuffix,
    QueueReconciledPrefix,
    QueuePartSeparator,
    // ---- 压缩 / 解压 ----
    ArchiveElapsedPrefix,
    ArchiveElapsedSuffix,
    ArchiveCompressingWrittenPrefix,
    ArchiveCompressingMiddle,
    ArchiveCompressingSuffix,
    ArchiveCompressingPrefix,
    ArchiveScanningPrefix,
    ArchiveExtractingPrefix,
    ArchivePastingPrefix,
    ArchiveCompressCancelled,
    ArchiveCompressedPrefix,
    ArchiveCompressedSuffix,
    ArchiveExtractCancelledInPlacePrefix,
    ArchiveExtractCancelledInPlaceSuffix,
    ArchiveExtractCancelled,
    ArchiveExtractedPrefix,
    ArchiveExtractedMiddle,
    ArchiveExtractedSuffix,
    ArchiveSkippedPrefix,
    ArchiveSkippedSuffix,
    ArchiveConflictMorePrefix,
    ArchiveConflictMoreSuffix,
}

impl Msg for SftpMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::FilterPlaceholder => "过滤",
            Self::ConflictNamePlaceholder => "新名字（留空 = 自动起名）",
            Self::NotConnectedYet => "还没连上，稍等一下",
            Self::ItemCountSuffix => " 项",
            Self::ParenOpen => "（",
            Self::ParenClose => "）",
            Self::ConnectingPrefix => "正在连接 ",
            Self::ConnectFailedPrefix => "连接失败：",
            Self::CloseBusyPrefix => "还有 ",
            Self::CloseBusySuffix => " 项传输 / 删除没完成，关闭会取消它们",
            Self::WaitingConflict => "等待处理同名文件",
            Self::Deleting => "正在删除",
            Self::CancelDeleting => "正在取消删除",
            Self::DeleteProgressMiddle => "：已删 ",
            Self::DeleteProgressSuffix => " 项",
            Self::ChmodProgressPrefix => "正在递归改权限…已改 ",
            Self::ChmodProgressSuffix => " 项",
            Self::HostFallback => "主机 #",
            Self::PhaseConnecting => "连接中…",
            Self::PhaseReady => "已连接",
            Self::PhaseFailed => "连接失败",
            Self::PhaseDisconnected => "已断开",
            Self::DisconnectedStatus => "文件连接已断开。点刷新或右键标签「重新连接」可以恢复",
            Self::TransferInterrupted => "连接已断开，传输中断（重新连接后可重试）",
            Self::FollowOn => "跟随终端目录：已开启",
            Self::FollowOff => "跟随终端目录：已关闭",
            Self::FollowStopped => "已停止跟随终端目录（手动换了目录）",
            Self::CloneNotConnected => "还没连上，连上之后再复制",
            Self::CloneDisconnected => "连接已断开，不能复制",
            Self::LocalReadInternalError => "读取本地目录时程序内部出错",
            Self::LocalReadQueueFull => "本地目录读取排队太多，稍后再试",
            Self::LocalDiskPrefix => "本地磁盘 (",
            Self::LocalDiskSuffix => ":)",
            Self::DirNameEmpty => "目录名不能为空",
            Self::FileNameEmpty => "文件名不能为空",
            Self::PathEmpty => "路径不能为空",
            Self::AlreadyExists => "已经有同名的条目",
            Self::SameNameFile => "已经有同名的文件",
            Self::DragControlChars => "文件名里有控制字符，不能插进终端",
            Self::DragNotUtf8 => "拖动的条目现在不能传（文件名编码不是 UTF-8）",
            Self::ChmodCancelledPrefix => "已取消改权限：",
            Self::ChmodCancelledMiddle => "（已改 ",
            Self::ChmodCancelledSuffix => " 项，剩下的保持原样）",
            Self::ChmodDonePrefix => "已递归改权限：",
            Self::ChmodDoneMiddle => "（",
            Self::ChmodDoneSuffix => " 项）",
            Self::ChmodSkippedLinksPrefix => "跳过 ",
            Self::ChmodSkippedLinksSuffix => " 个符号链接",
            Self::ChmodSkippedNonUtf8Prefix => "跳过 ",
            Self::ChmodSkippedNonUtf8Suffix => " 个名字不是 UTF-8 的条目",
            Self::ChmodFailedPrefix => "",
            Self::ChmodFailedMiddle => " 项没改成，第一个是 ",
            Self::ChmodFailedSuffix => "：",
            Self::ChmodSymlinkRefused => "选中的是符号链接，递归改权限不会跟进链接里面",
            Self::NamePathSeparatorPrefix => "名字里不能带路径分隔符：",
            Self::ConflictNameNullChar => "名字里不能有空字符",
            Self::ConflictActionOverwrite => "覆盖",
            Self::ConflictActionSkip => "跳过",
            Self::ConflictActionRename => "重命名",
            Self::ConflictActionResume => "续传",
            Self::ConflictSource => "源",
            Self::ConflictTarget => "目标",
            Self::ConflictSideMiddle => "：",
            Self::ConflictUnknownSizeTime => "大小和时间未知",
            Self::ConflictSizeEqual => "两边大小相同",
            Self::ConflictTargetSmaller => "目标比源小",
            Self::ConflictTargetBigger => "目标比源大",
            Self::ConflictMtimeEqual => "修改时间相同",
            Self::ConflictSourceNewer => "源更新",
            Self::ConflictTargetNewer => "目标更新",
            Self::ConflictResumeHint => "可以从目标现有长度处续传",
            Self::DeleteDonePrefix => "已删除：",
            Self::DeleteCountOpen => "（",
            Self::DeleteCountClose => " 项）",
            Self::DeleteCancelledPrefix => "已取消删除 ",
            Self::DeleteCancelAtLeastMiddle => " 项（不回滚），还剩至少 ",
            Self::DeleteCancelExactMiddle => " 项（不回滚），还剩 ",
            Self::DeleteCancelExactSuffix => " 项没删",
            Self::DeleteCancelUnknownSuffix => " 项（不回滚），剩余项数没能数出来",
            Self::DeleteFailedPrefix => "删除失败：",
            Self::DeleteFailedCountOpen => "（已删 ",
            Self::DeleteFailedCountClose => " 项；",
            Self::DeleteNotRemovedMiddle => "；",
            Self::DeleteNotRemovedSuffix => " 没删掉",
            Self::OctalErrorPrefix => "权限要写成 3-4 位八进制（例如 644 或 0644），实际是 ",
            Self::DirectionUpload => "上传",
            Self::DirectionDownload => "下载",
            Self::DirectionRemoteCopy => "远端复制",
            Self::DirectionLocalCopy => "本地复制",
            Self::StateQueued => "排队中",
            Self::StatePlanning => "准备中",
            Self::StateRunning => "传输中",
            Self::StatePaused => "已暂停",
            Self::StateWaitingReconnect => "等待重连",
            Self::StateWaitingConflict => "等待处理同名文件",
            Self::StateDone => "已完成",
            Self::StateFailedPrefix => "失败：",
            Self::StateCancelled => "已取消",
            Self::StateSkipped => "已跳过",
            Self::NoUsableFileName => " 没有可用的文件名",
            Self::NoUsableDirName => " 没有可用的目录名",
            Self::NameEmpty => "名字是空的",
            Self::NameCannotBePrefix => "名字不能是 ",
            Self::RemoteDirNotReady => "远端目录还没读出来，稍等一下",
            Self::OnlyRelativeNamesPrefix => "只接受当前目录下的名字，不要给绝对路径：",
            Self::NotUnderCurrentDirSuffix => " 不在当前目录下面",
            Self::MkdirFailedPrefix => "新建目录失败：",
            Self::MkdirFailedMiddle => "（",
            Self::MkdirDonePrefix => "已新建目录：",
            Self::HookRenameFormatPrefix => "ZSSH_SFTP_RENAME 要写成 旧名:新名，实际是 ",
            Self::HookRenameNameEmptyPrefix => "ZSSH_SFTP_RENAME 的名字不能是空的：",
            Self::HookChmodFormatPrefix => "ZSSH_SFTP_CHMOD 要写成 名字:权限，实际是 ",
            Self::HookChmodNameEmptyPrefix => "ZSSH_SFTP_CHMOD 的名字不能是空的：",
            Self::HookChmodModeInvalidPrefix => "ZSSH_SFTP_CHMOD 的权限不对：",
            Self::HookConflictInvalidPrefix => {
                "ZSSH_SFTP_CONFLICT 只能是 overwrite / skip / rename / overwrite-all / skip-all，实际是 "
            }
            Self::MenuArchivePending => "正在检测服务器上的压缩工具…",
            Self::MenuNotUtf8 => "文件名编码不是 UTF-8，暂不支持操作",
            Self::MenuFeaturePending => "这个功能还在做",
            Self::MenuSelectOne => "先选中一个条目",
            Self::MenuSingleOnly => "只能对单个条目操作",
            Self::MenuDriveNotAllowed => "盘符不能这样操作",
            Self::MenuSelectFile => "先选中一个文件",
            Self::MenuEditSingle => "编辑只能对单个文件",
            Self::MenuDriveEdit => "盘符不能编辑",
            Self::MenuFolderEdit => "文件夹不能用编辑器打开",
            Self::MenuSpecialEdit => "这种特殊文件不能编辑",
            Self::MenuFileTooBigPrefix => "文件超过 ",
            Self::MenuFileTooBigSuffix => " MB，请下载后再看",
            Self::MenuClipboardEmpty => "剪贴板是空的",
            Self::MenuCrossSidePaste => "远端和本地之间请用上传 / 下载",
            Self::MenuEnterFolder => "先进入一个文件夹",
            Self::MenuEnterDir => "先进入一个目录",
            Self::MenuNoUnzip => "服务器上没有 unzip 命令",
            Self::MenuNoTar => "服务器上没有 tar 命令",
            Self::MenuNoZip => "服务器上没有 zip 命令",
            Self::MenuExtractToPrefix => "解压到「",
            Self::MenuExtractToSuffix => "」",
            Self::MenuExtractHere => "解压到当前目录",
            Self::MenuCompressAs => "压缩为",
            Self::MenuNew => "新建",
            Self::MenuFolder => "文件夹",
            Self::MenuFile => "文件",
            Self::MenuPaste => "粘贴",
            Self::MenuRefresh => "刷新",
            Self::MenuProperties => "属性",
            Self::MenuOpen => "打开",
            Self::MenuEditBuiltin => "用内置编辑器编辑",
            Self::MenuDownloadToLocal => "下载到本地",
            Self::MenuUploadToRemote => "上传到远端",
            Self::MenuDownloadTo => "下载到…",
            Self::MenuCopy => "复制",
            Self::MenuCopyPath => "复制路径",
            Self::MenuRename => "重命名",
            Self::MenuMoveTo => "移动到…",
            Self::MenuDelete => "删除",
            Self::MenuPermissions => "修改权限",
            Self::MenuTransferRetry => "重试",
            Self::MenuTransferPause => "暂停",
            Self::MenuTransferResume => "继续",
            Self::MenuTransferCancel => "取消",
            Self::DialogNameEmpty => "名字不能为空",
            Self::DialogNameSlash => "名字里不能有 /",
            Self::DialogNameBackslash => "名字里不能有 \\",
            Self::DialogNameBadCharPrefix => "名字里不能有 ",
            Self::DialogLocalDirRequired => "请输入一个本地目录",
            Self::DialogWindowsBadChars => "路径里有 Windows 不允许的字符",
            Self::DialogPlaceRemote => "远端",
            Self::DialogPlaceLocal => "本地",
            Self::DialogDeleteTitlePrefix => "删除",
            Self::DialogDeleteTitleMiddle => "的 ",
            Self::DialogDeleteTitleSuffix => " 项？",
            Self::DialogDeleteDescFolder => "选中的里有文件夹，将递归删除其中所有内容，不能恢复。",
            Self::DialogDeleteDescLinks => "选中的都是符号链接：只删链接本身，不动它指向的内容。",
            Self::DialogDeleteDescFiles => "选中的都是文件，删除后无法恢复。",
            Self::DialogDeleteMorePrefix => "…等另外 ",
            Self::DialogDeleteMoreSuffix => " 项",
            Self::NavReadingPrefix => "正在读取 ",
            Self::NavReadingThisComputer => "正在读取 此电脑",
            Self::NavLocalPrefix => "本地：",
            Self::ItemsVisibleJoin => "（共 ",
            Self::ItemsVisibleEnd => "）",
            Self::StartCustomMissing => "自定义的本地目录不存在，已改用下载文件夹：",
            Self::NavLocalCountPrefix => "本地 ",
            Self::NavNotADirectoryPrefix => "不是目录：",
            Self::NavLocalReadFailedPrefix => "读取本地目录失败：",
            Self::NavPathEmptyWait => "路径是空的",
            Self::NavLocalPathEmpty => "本地路径是空的",
            Self::NavPathPlaceholder => "路径",
            Self::NavLocalEditorNotReady => "本地文件的内置编辑器还没接入",
            Self::OpsShellNotPosix => "这台服务器的登录 shell 不是 POSIX，不支持远端压缩 / 解压",
            Self::OpsPathCopied => "路径已复制",
            Self::OpsCancelNoOverwritePrefix => "已取消：没有覆盖 ",
            Self::OpsCancelNoOverwriteSuffix => "，也没有重命名",
            Self::OpsNameFolder => "文件夹名",
            Self::OpsNameFile => "文件名",
            Self::OpsNameNewName => "新名字",
            Self::OpsStartNewFolder => "正在新建文件夹 ",
            Self::OpsDoneNewFolder => "已新建文件夹 ",
            Self::OpsFailNewFolder => "新建文件夹 ",
            Self::OpsStartNewFile => "正在新建文件 ",
            Self::OpsDoneNewFile => "已新建文件 ",
            Self::OpsFailNewFile => "新建文件 ",
            Self::OpsStartRename => "正在重命名 ",
            Self::OpsDoneRename => "已重命名 ",
            Self::OpsFailRename => "重命名 ",
            Self::OpsFailedSuffix => " 失败",
            Self::OpsErrorJoin => "：",
            Self::OpsRuntimeUnavailable => "后台运行时不可用，操作没有执行",
            Self::OpsCreatingFilePrefix => "正在新建文件 ",
            Self::OpsCreateFileFailedPrefix => "新建文件失败：",
            Self::OpsCreatedFilePrefix => "已新建文件：",
            Self::OpsCreatingDirPrefix => "正在新建目录 ",
            Self::OpsRenamingPrefix => "正在重命名 ",
            Self::OpsRenamedPrefix => "已重命名：",
            Self::OpsRenameFailedPrefix => "重命名失败：",
            Self::OpsOverwriteRenamingPrefix => "正在覆盖式重命名 ",
            Self::OpsRenamedOverwritePrefix => "已重命名（覆盖了同名文件）：",
            Self::OpsDeletingWaitPrefix => "正在删除 ",
            Self::OpsDeletingWaitSuffix => "，先等它结束或点「取消删除」",
            Self::OpsDeletingWaitNextSuffix => "，等它结束再删下一个",
            Self::OpsDeletingPrefix => "正在删除 ",
            Self::OpsDeletingLocalPrefix => "正在删除本地的 ",
            Self::OpsDeletedLocalPrefix => "已删除本地的 ",
            Self::OpsSkippedNonUtf8 => "名字不是 UTF-8 的条目已跳过",
            Self::OpsUnknownReason => "未知原因",
            Self::OpsMoreFailuresPrefix => "等 ",
            Self::OpsMoreFailuresSuffix => " 项",
            Self::OpsDeleteThreadFailedPrefix => "删除的线程异常结束：",
            Self::OpsCancellingDeletePrefix => "正在取消删除 ",
            Self::OpsEllipsis => "…",
            Self::OpsOctalPlaceholder => "如 644",
            Self::OpsChmodRunningPrefix => "正在改权限 ",
            Self::OpsChmodFailedPrefix => "改权限失败：",
            Self::OpsChmodDonePrefix => "已改权限：",
            Self::OpsChmodRecursivePrefix => "正在递归改权限 ",
            Self::OpsChmodRecursiveFailedPrefix => "递归改权限失败：",
            Self::OpsSelectItemsToTransfer => "先选中要传的条目",
            Self::OpsDownloadDirPlaceholder => "例如 D:\\下载",
            Self::OpsDownloadDirPlaceholderPosix => "例如 ~/Downloads",
            Self::OpsChooseDownloadFolder => "选择下载到的文件夹",
            Self::OpsNotADirectory => "不是一个目录",
            Self::OpsDirUnusablePrefix => "这个目录用不了：",
            Self::OpsServerBusy => "服务器上已有一个任务在进行，请等它结束再试",
            Self::OpsCompressFailedPrefix => "压缩失败：",
            Self::OpsExtractCancelled => "已取消解压",
            Self::OpsNotExtractedPrefix => "没有解压：",
            Self::OpsExtractFailedPrefix => "解压失败：",
            Self::OpsLocalDirHint => "先在本地栏进入一个文件夹，或者用“下载到…”选个目录",
            Self::OpsRemoteDirHint => "先在远端栏进入一个目录",
            Self::OpsNoItemsDownload => "没有可下载的条目",
            Self::OpsNoItemsUpload => "没有可上传的条目",
            Self::OpsQueuedDownloadPrefix => "已加入下载队列：",
            Self::OpsQueuedUploadPrefix => "已加入上传队列：",
            Self::OpsQueuedSomeSuffix => "，其余没有开始：",
            Self::OpsAutoOpNotStartedPrefix => "自动操作没有开始：",
            Self::OpsThreadEndedPrefix => "操作线程异常结束（",
            Self::OpsListSeparator => "、",
            Self::ClipIntoSelfPrefix => "不能把文件夹「",
            Self::ClipIntoSelfSuffix => "」粘贴到它自己或它的子文件夹里",
            Self::ClipMethodCopyData => "服务器端复制（copy-data）",
            Self::ClipMethodServerCp => "服务器端复制（cp）",
            Self::ClipMethodRelay => "经本机中转",
            Self::ClipMethodLocal => "本地复制",
            Self::ClipPreparingPrefix => "正在准备复制…（已 ",
            Self::ClipPreparingSuffix => " 秒）",
            Self::ClipFileIndexPrefix => "第 ",
            Self::ClipFileIndexSuffix => " 个文件 · ",
            Self::ClipCopyingPrefix => "服务器上复制中…（",
            Self::ClipCopiedPrefix => "已复制 ",
            Self::ClipCopiedMiddle => " 项（",
            Self::ClipCopiedTookMiddle => "，用时 ",
            Self::ClipCopiedTookSuffix => " 秒）",
            Self::ClipQueuedMiddle => " 项已加入传输队列（",
            Self::ClipSkippedPrefix => "跳过 ",
            Self::ClipSkippedSuffix => " 项",
            Self::ClipCancelledPrefix => "已取消 ",
            Self::ClipFailedMiddle => " 项失败：",
            Self::ClipFailedItemOpen => "「",
            Self::ClipFailedItemClose => "」",
            Self::ClipFailedEllipsis => "……",
            Self::ClipNothingToPaste => "没有可粘贴的内容",
            Self::ClipSeparator => "；",
            Self::ClipLessThanOne => "不到 1",
            Self::CopySelectionNotUtf8 => "选中的项目现在不能复制（文件名编码不是 UTF-8）",
            Self::OpsNoRuntime => "没有可用的运行时",
            Self::OpsPasteFailedPrefix => "粘贴失败：",
            Self::PasteNoTargetFolder => "没有目标文件夹",
            Self::PasteRenameEmpty => "新名字不能为空，也不能含有 /",
            Self::PasteAlreadyExistsPrefix => "「",
            Self::PasteAlreadyExistsSuffix => "」也已经存在",
            Self::PasteSourceClosed => "复制的源所在的连接已经关闭，请重新打开那个标签再复制",
            Self::PasteTargetUnavailablePrefix => "目标文件夹不可用：",
            Self::PasteNotAFolderSuffix => " 不是文件夹",
            Self::PasteIntoSelfLinkSuffix => {
                "」粘贴到它自己或它的子文件夹里（目标经符号链接指回了它）"
            }
            Self::PasteMergeNeedsRelay => {
                "合并要逐个文件确认，服务器不支持 copy-data，改经本机中转"
            }
            Self::PasteAsYouChose => "按你的选择",
            Self::PasteShellNotPosixPrefix => "远端的登录 shell 不是 POSIX（",
            Self::PasteRelaySuffix => "，改经本机中转",
            Self::PasteSkippedLinksPrefix => "跳过了 ",
            Self::PasteSkippedLinksSuffix => " 个符号链接",
            Self::PasteServerCopyFailedPrefix => "服务器端复制失败：",
            Self::PasteOverwriteSelf => "不能用它自己覆盖自己（可以选择改名）",
            Self::PasteTargetIsFolder => "目标是个文件夹，不能用文件覆盖它",
            Self::PasteTargetIsFile => "目标是个文件，不能用文件夹覆盖它",
            Self::PasteKindsDiffer => "目标和源一个是文件一个是文件夹，不能覆盖",
            Self::MoveSameFolder => "已经在这个文件夹里",
            Self::MoveIntoSelf => "不能把文件夹移到它自己里面",
            Self::MoveCrossDevice => "不能跨磁盘移动，请用复制 + 删除",
            Self::MoveDestNeedAbsoluteRemote => "目标要写成完整路径（以 / 开头）",
            Self::MoveDestNeedAbsoluteLocal => "目标要写成完整路径（例如 D:\\文件夹）",
            Self::MoveDestNeedAbsoluteLocalPosix => {
                "目标要写成完整路径（以 / 或 ~/ 开头，例如 /srv/data）"
            }
            Self::MoveSelectItems => "先选中要移动的条目",
            Self::MoveDialogOnePrefix => "移动「",
            Self::MoveDialogOneSuffix => "」到…",
            Self::MoveDialogManyPrefix => "移动 ",
            Self::MoveDialogManySuffix => " 项到…",
            Self::MoveSummaryMovedOnePrefix => "已移动 ",
            Self::MoveSummaryMovedToMiddle => " 到「",
            Self::MoveSummaryMovedToClose => "」",
            Self::MoveSummaryMovedManyPrefix => "已移动 ",
            Self::MoveSummaryMovedManyMiddle => " 项到「",
            Self::MoveSummarySkippedPrefix => "跳过 ",
            Self::MoveSummarySkippedSuffix => " 项",
            Self::MoveSummaryFailedPrefix => "失败 ",
            Self::MoveSummaryFailedMiddle => " 项：",
            Self::MoveSummaryFailedMore => " 等",
            Self::MoveNoItemsMoved => "没有移动任何项",
            Self::MoveDetailSkippedPrefix => "跳过 ",
            Self::MoveDetailFailedPrefix => "失败 ",
            Self::MoveDetailMiddle => "：",
            Self::MoveSummarySeparator => "，",
            Self::MoveWhyFolderMerge => "同名文件夹不会合并，只能跳过",
            Self::MoveWhyTargetFile => "同名的是文件，不能用文件夹替换，只能跳过",
            Self::MoveWhyTargetFolder => "同名的是文件夹，不能用文件替换，只能跳过",
            Self::MoveSkipTargetExists => "目标里已有同名项，已跳过",
            Self::MoveReplaceFailed => "替换没有成功：目标仍然存在",
            Self::MoveSkipFolderOnly => "同名项是文件夹或要移动的是文件夹，只能跳过",
            Self::MoveSkipCancelled => "已取消",
            Self::MoveBlockedEditingSuffix => " 正在编辑，先保存并关闭再移动",
            Self::MoveTargetAlreadyExistsDetail => "替换后目标里原来的内容就没有了。",
            Self::MoveBusy => "正在移动，等它结束再移动下一批",
            Self::MoveTargetPlaceholder => "目标文件夹的完整路径",
            Self::MoveNewFolderPlaceholder => "新文件夹的名字",
            Self::MoveBrowseFailedPrefix => "读不了这个文件夹：",
            Self::MoveDestUnavailablePrefix => "目标文件夹用不了：",
            Self::MoveEditingInsideMiddle => " 里的 ",
            Self::MoveTransferringSuffix => " 正在传输，等传输结束再移动",
            Self::MoveInProgressPrefix => "正在移动 ",
            Self::MoveWaitingChoiceSuffix => " 在目标里已有同名项，等你选择",
            Self::TransferDirCannotEditSuffix => " 是目录，不能编辑",
            Self::TransferNotUtf8CannotEditSuffix => " 的名字不是 UTF-8，不能编辑",
            Self::TransferOpeningPrefix => "正在打开 ",
            Self::TransferDoneSuffix => "完成：",
            Self::TransferFailedSuffix => "失败：",
            Self::TransferCancelledSuffix => "已取消：",
            Self::TransferSkippedSuffix => "已跳过：",
            Self::TransferDropMissingPrefix => "拖入的路径不存在或不是文件 / 目录：",
            Self::TransferDropNone => "拖入的内容里没有可以上传的文件",
            Self::TransferDropCountPrefix => "拖入上传 ",
            Self::TransferRemotePathEmpty => "远端路径是空的",
            Self::TransferCreateLocalDirFailedPrefix => "创建本地目录失败：",
            Self::TransferUploadFilePrompt => "先填要上传的本地文件路径",
            Self::TransferIsDirectorySuffix => " 是目录，只能上传单个文件",
            Self::TransferLocalFileMissingPrefix => "本地文件不存在：",
            Self::TransferUploadDirPrompt => "先填要上传的本地目录路径",
            Self::TransferLocalDirMissingPrefix => "本地目录不存在：",
            Self::TransferEngineNotReady => "传输引擎还没准备好（等连接建立）",
            Self::TransferQueuedPrefix => "已加入",
            Self::TransferQueuedMiddle => "队列：",
            Self::TransferCancelling => "正在取消…",
            Self::TransferPausing => "正在暂停…",
            Self::TransferResuming => "正在继续…",
            Self::TransferRetrying => "正在重试…",
            Self::TransferAlreadyFinished => "这个任务已经结束了",
            Self::TransferCannotRetry => "这个任务现在不能重试",
            Self::TransferClearedPrefix => "清除了 ",
            Self::TransferClearedSuffix => " 个已结束的任务",
            Self::TransferNothingCleared => "没有已结束的任务",
            Self::TransferCannotResume => "这一对文件不能续传（目标不比源小）",
            Self::TransferRenameInvalidPrefix => "重命名的新名字不能用：",
            Self::TransferConflictPrefix => "同名文件处理：",
            Self::TransferConflictMorePrefix => "（还有 ",
            Self::TransferConflictMoreSuffix => " 个）",
            Self::TransferConflictApplyAllMiddle => "（本次提交的 ",
            Self::TransferConflictApplyAllSuffix => " 个，后面的同样处理）",
            Self::TransferAutoPausePrefix => "自动测试：已请求暂停 ",
            Self::TransferAutoResumePrefix => "自动测试：已请求继续 ",
            Self::TransferAutoCountSuffix => " 个任务",
            Self::LinkToPrefix => "链接到：",
            Self::LinkLabel => "链接",
            Self::LinkBrokenSuffix => "\n目标不存在或读不了（断链）",
            Self::ColumnName => "名称",
            Self::ColumnSize => "大小",
            Self::ColumnMtime => "修改时间",
            Self::ColumnKind => "类型",
            Self::ColumnPerms => "权限",
            Self::ColumnOwner => "所有者",
            Self::KindDir => "目录",
            Self::KindLink => "链接",
            Self::KindDrive => "盘符",
            Self::KindFile => "文件",
            Self::PropsKindFolder => "文件夹",
            Self::PropsKindFile => "文件",
            Self::PropsKindSymlinkDir => "符号链接（指向文件夹）",
            Self::PropsKindSymlinkFile => "符号链接（指向文件）",
            Self::PropsKindSymlinkBroken => "符号链接（断链）",
            Self::PropsKindDrive => "磁盘",
            Self::PropsKindOther => "其他",
            Self::PropsLinkBrokenSuffix => "（目标不存在或读不了）",
            Self::PropsBytesSuffix => " 字节",
            Self::PropsBytesParenOpen => "（",
            Self::PropsBytesParenClose => " 字节）",
            Self::PropsFilesCount => " 个文件，",
            Self::PropsFoldersCount => " 个文件夹",
            Self::PropsSkippedLinksPrefix => "跳过 ",
            Self::PropsSkippedLinksSuffix => " 个符号链接",
            Self::PropsUnreadableFoldersSuffix => " 个文件夹读不了",
            Self::PropsComma => "，",
            Self::PropsNotComputed => "未计算",
            Self::PropsCalcRunningPrefix => "计算中…已数到 ",
            Self::PropsCalcCancelledPrefix => "已取消，取消前数到 ",
            Self::PropsCalcCancelledSuffix => "（不是完整结果）",
            Self::PropsCalcFailedPrefix => "计算失败：",
            Self::PropsUnknownError => "未知错误",
            Self::PropsRowName => "名称",
            Self::PropsRowLocation => "位置",
            Self::PropsRowType => "类型",
            Self::PropsRowLinkTo => "链接到",
            Self::PropsRowSize => "大小",
            Self::PropsRowMtime => "修改时间",
            Self::PropsRowPerms => "权限",
            Self::PropsRowOwner => "所有者",
            Self::QueueActiveSuffix => " 个任务进行中 ",
            Self::EtaMinuteUnit => " 分 ",
            Self::EtaMinuteTail => " 分",
            Self::EtaSecondUnit => " 秒",
            Self::EtaHourUnit => " 时 ",
            Self::QueueFilesSuffix => " 个文件",
            Self::QueueFilesMismatchPrefix => "文件数对不上：完成 ",
            Self::QueueFilesMismatchMiddle => " / 共 ",
            Self::QueueBytesMatchSuffix => "，字节数与源一致",
            Self::QueueBytesMismatchPrefix => "字节数对不上：已传 ",
            Self::QueueBytesMismatchMiddle => " / 源 ",
            Self::QueueTimeUnderOneSecond => "用时 不到 1 秒",
            Self::QueueTimePrefix => "用时 ",
            Self::QueueTimeAvgMiddle => " 秒，平均 ",
            Self::QueueTimeSuffix => " 秒",
            Self::QueueReconciledPrefix => "已核对：",
            Self::QueuePartSeparator => "；",
            Self::ArchiveElapsedPrefix => "已 ",
            Self::ArchiveElapsedSuffix => " 秒",
            Self::ArchiveCompressingWrittenPrefix => "压缩中…（已写入 ",
            Self::ArchiveCompressingMiddle => " · ",
            Self::ArchiveCompressingSuffix => "）",
            Self::ArchiveCompressingPrefix => "压缩中…（",
            Self::ArchiveScanningPrefix => "检查压缩包中…（",
            Self::ArchiveExtractingPrefix => "解压中…（",
            Self::ArchivePastingPrefix => "复制中…（",
            Self::ArchiveCompressCancelled => "已取消压缩，没有留下临时文件",
            Self::ArchiveCompressedPrefix => "已压缩为「",
            Self::ArchiveCompressedSuffix => "」",
            Self::ArchiveExtractCancelledInPlacePrefix => "已取消解压，已经解出的文件保留在「",
            Self::ArchiveExtractCancelledInPlaceSuffix => "」里",
            Self::ArchiveExtractCancelled => "已取消解压，没有留下半成品",
            Self::ArchiveExtractedPrefix => "已解压到「",
            Self::ArchiveExtractedMiddle => "」：解出 ",
            Self::ArchiveExtractedSuffix => " 个文件",
            Self::ArchiveSkippedPrefix => "，跳过 ",
            Self::ArchiveSkippedSuffix => " 个已有文件",
            Self::ArchiveConflictMorePrefix => "等 ",
            Self::ArchiveConflictMoreSuffix => " 个",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::FilterPlaceholder => "Filter",
            Self::ConflictNamePlaceholder => "New name (leave empty to generate one)",
            Self::NotConnectedYet => "Not connected yet — please wait a moment",
            Self::ItemCountSuffix => " items",
            Self::ParenOpen => " (",
            Self::ParenClose => ")",
            Self::ConnectingPrefix => "Connecting to ",
            Self::ConnectFailedPrefix => "Connection failed: ",
            Self::CloseBusyPrefix => "",
            Self::CloseBusySuffix => {
                " transfer / delete tasks are unfinished; closing will cancel them"
            }
            Self::WaitingConflict => "Waiting to resolve a name conflict",
            Self::Deleting => "Deleting ",
            Self::CancelDeleting => "Cancelling deletion of ",
            Self::DeleteProgressMiddle => ": ",
            Self::DeleteProgressSuffix => " deleted",
            Self::ChmodProgressPrefix => "Recursively changing permissions… ",
            Self::ChmodProgressSuffix => " changed",
            Self::HostFallback => "Host #",
            Self::PhaseConnecting => "Connecting…",
            Self::PhaseReady => "Connected",
            Self::PhaseFailed => "Connection failed",
            Self::PhaseDisconnected => "Disconnected",
            Self::DisconnectedStatus => {
                "The file connection was lost. Click Refresh or right-click the tab and choose Reconnect to recover"
            }
            Self::TransferInterrupted => {
                "The connection was lost and the transfer was interrupted (retry it after reconnecting)"
            }
            Self::FollowOn => "Follow terminal directory: on",
            Self::FollowOff => "Follow terminal directory: off",
            Self::FollowStopped => {
                "Stopped following the terminal directory (you changed it manually)"
            }
            Self::CloneNotConnected => "Not connected yet — copy after connecting",
            Self::CloneDisconnected => "Connection lost — cannot copy",
            Self::LocalReadInternalError => "Internal error while reading the local directory",
            Self::LocalReadQueueFull => "Too many local directory reads queued — try again later",
            Self::LocalDiskPrefix => "Local Disk (",
            Self::LocalDiskSuffix => ":)",
            Self::DirNameEmpty => "Directory name cannot be empty",
            Self::FileNameEmpty => "File name cannot be empty",
            Self::PathEmpty => "Path cannot be empty",
            Self::AlreadyExists => "An entry with that name already exists",
            Self::SameNameFile => "A file with that name already exists",
            Self::DragControlChars => {
                "The file name contains control characters and cannot be inserted into the terminal"
            }
            Self::DragNotUtf8 => "Dragged items cannot be transferred now (file name is not UTF-8)",
            Self::ChmodCancelledPrefix => "Permission change cancelled: ",
            Self::ChmodCancelledMiddle => " (",
            Self::ChmodCancelledSuffix => " changed; the rest were left unchanged)",
            Self::ChmodDonePrefix => "Permissions changed recursively: ",
            Self::ChmodDoneMiddle => " (",
            Self::ChmodDoneSuffix => " items)",
            Self::ChmodSkippedLinksPrefix => "skipped ",
            Self::ChmodSkippedLinksSuffix => " symbolic links",
            Self::ChmodSkippedNonUtf8Prefix => "skipped ",
            Self::ChmodSkippedNonUtf8Suffix => " entries whose names are not UTF-8",
            Self::ChmodFailedPrefix => "",
            Self::ChmodFailedMiddle => " items failed; first is ",
            Self::ChmodFailedSuffix => ": ",
            Self::ChmodSymlinkRefused => {
                "The selection is a symbolic link; recursive permission changes do not follow links"
            }
            Self::NamePathSeparatorPrefix => "Names cannot contain a path separator: ",
            Self::ConflictNameNullChar => "Names cannot contain a NUL character",
            Self::ConflictActionOverwrite => "Overwrite",
            Self::ConflictActionSkip => "Skip",
            Self::ConflictActionRename => "Rename",
            Self::ConflictActionResume => "Resume",
            Self::ConflictSource => "Source",
            Self::ConflictTarget => "Target",
            Self::ConflictSideMiddle => ": ",
            Self::ConflictUnknownSizeTime => "size and time unknown",
            Self::ConflictSizeEqual => "sizes are equal",
            Self::ConflictTargetSmaller => "target is smaller than source",
            Self::ConflictTargetBigger => "target is larger than source",
            Self::ConflictMtimeEqual => "modified times are equal",
            Self::ConflictSourceNewer => "source is newer",
            Self::ConflictTargetNewer => "target is newer",
            Self::ConflictResumeHint => "can resume from the target's current length",
            Self::DeleteDonePrefix => "Deleted: ",
            Self::DeleteCountOpen => " (",
            Self::DeleteCountClose => " items)",
            Self::DeleteCancelledPrefix => "Cancelled deleting ",
            Self::DeleteCancelAtLeastMiddle => " deleted (not rolled back); at least ",
            Self::DeleteCancelExactMiddle => " deleted (not rolled back); ",
            Self::DeleteCancelExactSuffix => " remaining",
            Self::DeleteCancelUnknownSuffix => {
                " deleted (not rolled back); remaining count unavailable"
            }
            Self::DeleteFailedPrefix => "Delete failed: ",
            Self::DeleteFailedCountOpen => " (deleted ",
            Self::DeleteFailedCountClose => " items; ",
            Self::DeleteNotRemovedMiddle => "; ",
            Self::DeleteNotRemovedSuffix => " was not deleted",
            Self::OctalErrorPrefix => {
                "Permissions must be written as 3-4 octal digits (for example 644 or 0644); got "
            }
            Self::DirectionUpload => "Upload",
            Self::DirectionDownload => "Download",
            Self::DirectionRemoteCopy => "Remote copy",
            Self::DirectionLocalCopy => "Local copy",
            Self::StateQueued => "Queued",
            Self::StatePlanning => "Preparing",
            Self::StateRunning => "Transferring",
            Self::StatePaused => "Paused",
            Self::StateWaitingReconnect => "Waiting to reconnect",
            Self::StateWaitingConflict => "Waiting to resolve a name conflict",
            Self::StateDone => "Done",
            Self::StateFailedPrefix => "Failed: ",
            Self::StateCancelled => "Cancelled",
            Self::StateSkipped => "Skipped",
            Self::NoUsableFileName => " has no usable file name",
            Self::NoUsableDirName => " has no usable directory name",
            Self::NameEmpty => "The name is empty",
            Self::NameCannotBePrefix => "The name cannot be ",
            Self::RemoteDirNotReady => {
                "The remote directory is not ready yet — please wait a moment"
            }
            Self::OnlyRelativeNamesPrefix => {
                "Only names under the current directory are accepted; do not pass an absolute path: "
            }
            Self::NotUnderCurrentDirSuffix => " is not under the current directory",
            Self::MkdirFailedPrefix => "Failed to create directory: ",
            Self::MkdirFailedMiddle => " (",
            Self::MkdirDonePrefix => "Directory created: ",
            Self::HookRenameFormatPrefix => {
                "ZSSH_SFTP_RENAME must be written as old-name:new-name; got "
            }
            Self::HookRenameNameEmptyPrefix => "ZSSH_SFTP_RENAME name cannot be empty: ",
            Self::HookChmodFormatPrefix => "ZSSH_SFTP_CHMOD must be written as name:mode; got ",
            Self::HookChmodNameEmptyPrefix => "ZSSH_SFTP_CHMOD name cannot be empty: ",
            Self::HookChmodModeInvalidPrefix => "ZSSH_SFTP_CHMOD has an invalid mode: ",
            Self::HookConflictInvalidPrefix => {
                "ZSSH_SFTP_CONFLICT must be one of overwrite / skip / rename / overwrite-all / skip-all; got "
            }
            Self::MenuArchivePending => "Checking the server's archive tools…",
            Self::MenuNotUtf8 => "The file name is not UTF-8; this action is not supported yet",
            Self::MenuFeaturePending => "This feature is not ready yet",
            Self::MenuSelectOne => "Select an item first",
            Self::MenuSingleOnly => "This action works on a single item only",
            Self::MenuDriveNotAllowed => "This action cannot be used on a drive",
            Self::MenuSelectFile => "Select a file first",
            Self::MenuEditSingle => "Editing works on a single file only",
            Self::MenuDriveEdit => "A drive cannot be edited",
            Self::MenuFolderEdit => "Folders cannot be opened in the editor",
            Self::MenuSpecialEdit => "This special file cannot be edited",
            Self::MenuFileTooBigPrefix => "The file exceeds ",
            Self::MenuFileTooBigSuffix => " MB; download it to view it",
            Self::MenuClipboardEmpty => "The clipboard is empty",
            Self::MenuCrossSidePaste => "Use upload / download between the remote and local panes",
            Self::MenuEnterFolder => "Enter a folder first",
            Self::MenuEnterDir => "Enter a directory first",
            Self::MenuNoUnzip => "The server has no unzip command",
            Self::MenuNoTar => "The server has no tar command",
            Self::MenuNoZip => "The server has no zip command",
            Self::MenuExtractToPrefix => "Extract to \"",
            Self::MenuExtractToSuffix => "\"",
            Self::MenuExtractHere => "Extract here",
            Self::MenuCompressAs => "Compress as",
            Self::MenuNew => "New",
            Self::MenuFolder => "Folder",
            Self::MenuFile => "File",
            Self::MenuPaste => "Paste",
            Self::MenuRefresh => "Refresh",
            Self::MenuProperties => "Properties",
            Self::MenuOpen => "Open",
            Self::MenuEditBuiltin => "Edit with Built-in Editor",
            Self::MenuDownloadToLocal => "Download to Local",
            Self::MenuUploadToRemote => "Upload to Remote",
            Self::MenuDownloadTo => "Download to…",
            Self::MenuCopy => "Copy",
            Self::MenuCopyPath => "Copy Path",
            Self::MenuRename => "Rename",
            Self::MenuMoveTo => "Move to…",
            Self::MenuDelete => "Delete",
            Self::MenuPermissions => "Permissions",
            Self::MenuTransferRetry => "Retry",
            Self::MenuTransferPause => "Pause",
            Self::MenuTransferResume => "Resume",
            Self::MenuTransferCancel => "Cancel",
            Self::DialogNameEmpty => "The name cannot be empty",
            Self::DialogNameSlash => "The name cannot contain /",
            Self::DialogNameBackslash => "The name cannot contain \\",
            Self::DialogNameBadCharPrefix => "The name cannot contain ",
            Self::DialogLocalDirRequired => "Enter a local directory",
            Self::DialogWindowsBadChars => {
                "The path contains characters that Windows does not allow"
            }
            Self::DialogPlaceRemote => "remote",
            Self::DialogPlaceLocal => "local",
            Self::DialogDeleteTitlePrefix => "Delete ",
            Self::DialogDeleteTitleMiddle => " ",
            Self::DialogDeleteTitleSuffix => " items?",
            Self::DialogDeleteDescFolder => {
                "The selection contains folders; everything inside will be deleted recursively and cannot be recovered."
            }
            Self::DialogDeleteDescLinks => {
                "The selection contains only symbolic links: only the links themselves are deleted, not what they point to."
            }
            Self::DialogDeleteDescFiles => {
                "The selection contains only files; they cannot be recovered after deletion."
            }
            Self::DialogDeleteMorePrefix => "…and ",
            Self::DialogDeleteMoreSuffix => " more",
            Self::NavReadingPrefix => "Reading ",
            Self::NavReadingThisComputer => "Reading This PC",
            Self::NavLocalPrefix => "Local: ",
            Self::ItemsVisibleJoin => " of ",
            Self::ItemsVisibleEnd => "",
            Self::StartCustomMissing => {
                "The custom local folder does not exist; using the Downloads folder instead: "
            }
            Self::NavLocalCountPrefix => "Local ",
            Self::NavNotADirectoryPrefix => "Not a directory: ",
            Self::NavLocalReadFailedPrefix => "Failed to read the local directory: ",
            Self::NavPathEmptyWait => "The path is empty",
            Self::NavLocalPathEmpty => "The local path is empty",
            Self::NavPathPlaceholder => "Path",
            Self::NavLocalEditorNotReady => {
                "The built-in editor is not available for local files yet"
            }
            Self::OpsShellNotPosix => {
                "This server's login shell is not POSIX; remote compress / extract is not supported"
            }
            Self::OpsPathCopied => "Path copied",
            Self::OpsCancelNoOverwritePrefix => "Cancelled: did not overwrite ",
            Self::OpsCancelNoOverwriteSuffix => ", and did not rename",
            Self::OpsNameFolder => "Folder name",
            Self::OpsNameFile => "File name",
            Self::OpsNameNewName => "New name",
            Self::OpsStartNewFolder => "Creating folder ",
            Self::OpsDoneNewFolder => "Created folder ",
            Self::OpsFailNewFolder => "Create folder ",
            Self::OpsStartNewFile => "Creating file ",
            Self::OpsDoneNewFile => "Created file ",
            Self::OpsFailNewFile => "Create file ",
            Self::OpsStartRename => "Renaming ",
            Self::OpsDoneRename => "Renamed ",
            Self::OpsFailRename => "Rename ",
            Self::OpsFailedSuffix => " failed",
            Self::OpsErrorJoin => ": ",
            Self::OpsRuntimeUnavailable => {
                "Background runtime unavailable; the operation was not run"
            }
            Self::OpsCreatingFilePrefix => "Creating file ",
            Self::OpsCreateFileFailedPrefix => "Failed to create file: ",
            Self::OpsCreatedFilePrefix => "File created: ",
            Self::OpsCreatingDirPrefix => "Creating directory ",
            Self::OpsRenamingPrefix => "Renaming ",
            Self::OpsRenamedPrefix => "Renamed: ",
            Self::OpsRenameFailedPrefix => "Rename failed: ",
            Self::OpsOverwriteRenamingPrefix => "Renaming with overwrite ",
            Self::OpsRenamedOverwritePrefix => "Renamed (overwrote the existing file): ",
            Self::OpsDeletingWaitPrefix => "Deleting ",
            Self::OpsDeletingWaitSuffix => "; wait for it to finish or press \"Cancel delete\"",
            Self::OpsDeletingWaitNextSuffix => "; wait for it to finish before deleting more",
            Self::OpsDeletingPrefix => "Deleting ",
            Self::OpsDeletingLocalPrefix => "Deleting local ",
            Self::OpsDeletedLocalPrefix => "Deleted local ",
            Self::OpsSkippedNonUtf8 => "Skipped entries whose names are not UTF-8",
            Self::OpsUnknownReason => "Unknown reason",
            Self::OpsMoreFailuresPrefix => "and ",
            Self::OpsMoreFailuresSuffix => " more",
            Self::OpsDeleteThreadFailedPrefix => "Delete thread ended abnormally: ",
            Self::OpsCancellingDeletePrefix => "Cancelling deletion of ",
            Self::OpsEllipsis => "…",
            Self::OpsOctalPlaceholder => "e.g. 644",
            Self::OpsChmodRunningPrefix => "Changing permissions ",
            Self::OpsChmodFailedPrefix => "Failed to change permissions: ",
            Self::OpsChmodDonePrefix => "Permissions changed: ",
            Self::OpsChmodRecursivePrefix => "Recursively changing permissions ",
            Self::OpsChmodRecursiveFailedPrefix => "Recursive permission change failed: ",
            Self::OpsSelectItemsToTransfer => "Select the items to transfer first",
            Self::OpsDownloadDirPlaceholder => "e.g. D:\\Downloads",
            Self::OpsDownloadDirPlaceholderPosix => "e.g. ~/Downloads",
            Self::OpsChooseDownloadFolder => "Choose a download folder",
            Self::OpsNotADirectory => "Not a directory",
            Self::OpsDirUnusablePrefix => "This directory cannot be used: ",
            Self::OpsServerBusy => {
                "Another task is already running on the server; wait for it to finish and try again"
            }
            Self::OpsCompressFailedPrefix => "Compression failed: ",
            Self::OpsExtractCancelled => "Extraction cancelled",
            Self::OpsNotExtractedPrefix => "Not extracted: ",
            Self::OpsExtractFailedPrefix => "Extraction failed: ",
            Self::OpsLocalDirHint => {
                "Enter a folder in the local pane first, or choose a directory with \"Download to…\""
            }
            Self::OpsRemoteDirHint => "Enter a directory in the remote pane first",
            Self::OpsNoItemsDownload => "No items to download",
            Self::OpsNoItemsUpload => "No items to upload",
            Self::OpsQueuedDownloadPrefix => "Added to download queue: ",
            Self::OpsQueuedUploadPrefix => "Added to upload queue: ",
            Self::OpsQueuedSomeSuffix => "; the rest did not start: ",
            Self::OpsAutoOpNotStartedPrefix => "Automated operation did not start: ",
            Self::OpsThreadEndedPrefix => "operation thread ended abnormally (",
            Self::OpsListSeparator => ", ",
            Self::ClipIntoSelfPrefix => "Cannot paste folder \"",
            Self::ClipIntoSelfSuffix => "\" into itself or one of its subfolders",
            Self::ClipMethodCopyData => "server-side copy (copy-data)",
            Self::ClipMethodServerCp => "server-side copy (cp)",
            Self::ClipMethodRelay => "relayed through this computer",
            Self::ClipMethodLocal => "local copy",
            Self::ClipPreparingPrefix => "Preparing to copy… (",
            Self::ClipPreparingSuffix => " s)",
            Self::ClipFileIndexPrefix => "file ",
            Self::ClipFileIndexSuffix => " · ",
            Self::ClipCopyingPrefix => "Copying on the server… (",
            Self::ClipCopiedPrefix => "Copied ",
            Self::ClipCopiedMiddle => " items (",
            Self::ClipCopiedTookMiddle => ", took ",
            Self::ClipCopiedTookSuffix => " s)",
            Self::ClipQueuedMiddle => " items added to the transfer queue (",
            Self::ClipSkippedPrefix => "Skipped ",
            Self::ClipSkippedSuffix => " items",
            Self::ClipCancelledPrefix => "Cancelled ",
            Self::ClipFailedMiddle => " items failed: ",
            Self::ClipFailedItemOpen => "\"",
            Self::ClipFailedItemClose => "\"",
            Self::ClipFailedEllipsis => "…",
            Self::ClipNothingToPaste => "Nothing to paste",
            Self::ClipSeparator => "; ",
            Self::ClipLessThanOne => "< 1",
            Self::CopySelectionNotUtf8 => {
                "The selected items cannot be copied now (file name is not UTF-8)"
            }
            Self::OpsNoRuntime => "No usable runtime",
            Self::OpsPasteFailedPrefix => "Paste failed: ",
            Self::PasteNoTargetFolder => "No target folder",
            Self::PasteRenameEmpty => "The new name cannot be empty or contain /",
            Self::PasteAlreadyExistsPrefix => "\"",
            Self::PasteAlreadyExistsSuffix => "\" already exists",
            Self::PasteSourceClosed => {
                "The connection for the copy source is closed; reopen that tab and copy again"
            }
            Self::PasteTargetUnavailablePrefix => "The target folder is unavailable: ",
            Self::PasteNotAFolderSuffix => " is not a folder",
            Self::PasteIntoSelfLinkSuffix => {
                "\" into itself or one of its subfolders (the target points back through a symbolic link)"
            }
            Self::PasteMergeNeedsRelay => {
                "Merging requires confirming each file, and the server does not support copy-data; relaying through this computer"
            }
            Self::PasteAsYouChose => "as you chose",
            Self::PasteShellNotPosixPrefix => "The remote login shell is not POSIX (",
            Self::PasteRelaySuffix => ", relaying through this computer",
            Self::PasteSkippedLinksPrefix => "Skipped ",
            Self::PasteSkippedLinksSuffix => " symbolic links",
            Self::PasteServerCopyFailedPrefix => "Server-side copy failed: ",
            Self::PasteOverwriteSelf => {
                "An item cannot overwrite itself (you can rename it instead)"
            }
            Self::PasteTargetIsFolder => {
                "The target is a folder and cannot be overwritten by a file"
            }
            Self::PasteTargetIsFile => "The target is a file and cannot be overwritten by a folder",
            Self::PasteKindsDiffer => {
                "One of the target and source is a file and the other is a folder; cannot overwrite"
            }
            Self::MoveSameFolder => "Already in this folder",
            Self::MoveIntoSelf => "A folder cannot be moved into itself",
            Self::MoveCrossDevice => "Cannot move across disks; use copy and delete instead",
            Self::MoveDestNeedAbsoluteRemote => "The target must be a full path (starting with /)",
            Self::MoveDestNeedAbsoluteLocal => {
                "The target must be a full path (for example D:\\folder)"
            }
            Self::MoveDestNeedAbsoluteLocalPosix => {
                "The target must be a full path (starting with / or ~/, for example /srv/data)"
            }
            Self::MoveSelectItems => "Select the items to move first",
            Self::MoveDialogOnePrefix => "Move \"",
            Self::MoveDialogOneSuffix => "\" to…",
            Self::MoveDialogManyPrefix => "Move ",
            Self::MoveDialogManySuffix => " items to…",
            Self::MoveSummaryMovedOnePrefix => "Moved ",
            Self::MoveSummaryMovedToMiddle => " to \"",
            Self::MoveSummaryMovedToClose => "\"",
            Self::MoveSummaryMovedManyPrefix => "Moved ",
            Self::MoveSummaryMovedManyMiddle => " items to \"",
            Self::MoveSummarySkippedPrefix => "skipped ",
            Self::MoveSummarySkippedSuffix => " items",
            Self::MoveSummaryFailedPrefix => "failed ",
            Self::MoveSummaryFailedMiddle => " items: ",
            Self::MoveSummaryFailedMore => " etc.",
            Self::MoveNoItemsMoved => "No items were moved",
            Self::MoveDetailSkippedPrefix => "Skipped ",
            Self::MoveDetailFailedPrefix => "Failed ",
            Self::MoveDetailMiddle => ": ",
            Self::MoveSummarySeparator => ", ",
            Self::MoveWhyFolderMerge => {
                "Folders with the same name are not merged; they can only be skipped"
            }
            Self::MoveWhyTargetFile => {
                "The existing item is a file and cannot be replaced by a folder; skipping"
            }
            Self::MoveWhyTargetFolder => {
                "The existing item is a folder and cannot be replaced by a file; skipping"
            }
            Self::MoveSkipTargetExists => {
                "An item with that name already exists in the target; skipped"
            }
            Self::MoveReplaceFailed => "Replacement did not succeed: the target still exists",
            Self::MoveSkipFolderOnly => {
                "The existing item is a folder, or the item being moved is a folder; skipping"
            }
            Self::MoveSkipCancelled => "Cancelled",
            Self::MoveBlockedEditingSuffix => " is being edited; save and close it before moving",
            Self::MoveTargetAlreadyExistsDetail => {
                "Replacing it removes the target's previous contents."
            }
            Self::MoveBusy => {
                "A move is already running; wait for it to finish before moving again"
            }
            Self::MoveTargetPlaceholder => "Full path of the target folder",
            Self::MoveNewFolderPlaceholder => "Name of the new folder",
            Self::MoveBrowseFailedPrefix => "Cannot read this folder: ",
            Self::MoveDestUnavailablePrefix => "The target folder cannot be used: ",
            Self::MoveEditingInsideMiddle => " in ",
            Self::MoveTransferringSuffix => {
                " is being transferred; wait for the transfer to finish before moving"
            }
            Self::MoveInProgressPrefix => "Moving ",
            Self::MoveWaitingChoiceSuffix => {
                " already exists in the target; waiting for your choice"
            }
            Self::TransferDirCannotEditSuffix => " is a directory and cannot be edited",
            Self::TransferNotUtf8CannotEditSuffix => {
                " has a name that is not UTF-8 and cannot be edited"
            }
            Self::TransferOpeningPrefix => "Opening ",
            Self::TransferDoneSuffix => " complete: ",
            Self::TransferFailedSuffix => " failed: ",
            Self::TransferCancelledSuffix => " cancelled: ",
            Self::TransferSkippedSuffix => " skipped: ",
            Self::TransferDropMissingPrefix => {
                "The dropped path does not exist or is not a file / directory: "
            }
            Self::TransferDropNone => "Nothing in the dropped items can be uploaded",
            Self::TransferDropCountPrefix => "Uploaded from drop: ",
            Self::TransferRemotePathEmpty => "The remote path is empty",
            Self::TransferCreateLocalDirFailedPrefix => "Failed to create the local directory: ",
            Self::TransferUploadFilePrompt => "Enter the local file path to upload first",
            Self::TransferIsDirectorySuffix => " is a directory; only single files can be uploaded",
            Self::TransferLocalFileMissingPrefix => "Local file not found: ",
            Self::TransferUploadDirPrompt => "Enter the local directory path to upload first",
            Self::TransferLocalDirMissingPrefix => "Local directory not found: ",
            Self::TransferEngineNotReady => {
                "The transfer engine is not ready yet (waiting for the connection)"
            }
            Self::TransferQueuedPrefix => "Added to ",
            Self::TransferQueuedMiddle => " queue: ",
            Self::TransferCancelling => "Cancelling…",
            Self::TransferPausing => "Pausing…",
            Self::TransferResuming => "Resuming…",
            Self::TransferRetrying => "Retrying…",
            Self::TransferAlreadyFinished => "This task has already finished",
            Self::TransferCannotRetry => "This task cannot be retried now",
            Self::TransferClearedPrefix => "Cleared ",
            Self::TransferClearedSuffix => " finished tasks",
            Self::TransferNothingCleared => "There are no finished tasks",
            Self::TransferCannotResume => {
                "This pair of files cannot be resumed (the target is not smaller than the source)"
            }
            Self::TransferRenameInvalidPrefix => "The new name cannot be used: ",
            Self::TransferConflictPrefix => "Name conflict: ",
            Self::TransferConflictMorePrefix => " (",
            Self::TransferConflictMoreSuffix => " more)",
            Self::TransferConflictApplyAllMiddle => " (",
            Self::TransferConflictApplyAllSuffix => {
                " in this batch; the rest will be handled the same way)"
            }
            Self::TransferAutoPausePrefix => "Automated test: pause requested for ",
            Self::TransferAutoResumePrefix => "Automated test: resume requested for ",
            Self::TransferAutoCountSuffix => " tasks",
            Self::LinkToPrefix => "Links to: ",
            Self::LinkLabel => "Link",
            Self::LinkBrokenSuffix => "\nThe target does not exist or cannot be read (broken link)",
            Self::ColumnName => "Name",
            Self::ColumnSize => "Size",
            Self::ColumnMtime => "Modified",
            Self::ColumnKind => "Type",
            Self::ColumnPerms => "Permissions",
            Self::ColumnOwner => "Owner",
            Self::KindDir => "Folder",
            Self::KindLink => "Link",
            Self::KindDrive => "Drive",
            Self::KindFile => "File",
            Self::PropsKindFolder => "Folder",
            Self::PropsKindFile => "File",
            Self::PropsKindSymlinkDir => "Symbolic link (to a folder)",
            Self::PropsKindSymlinkFile => "Symbolic link (to a file)",
            Self::PropsKindSymlinkBroken => "Symbolic link (broken)",
            Self::PropsKindDrive => "Disk",
            Self::PropsKindOther => "Other",
            Self::PropsLinkBrokenSuffix => " (target does not exist or cannot be read)",
            Self::PropsBytesSuffix => " bytes",
            Self::PropsBytesParenOpen => " (",
            Self::PropsBytesParenClose => " bytes)",
            Self::PropsFilesCount => " files, ",
            Self::PropsFoldersCount => " folders",
            Self::PropsSkippedLinksPrefix => "skipped ",
            Self::PropsSkippedLinksSuffix => " symbolic links",
            Self::PropsUnreadableFoldersSuffix => " folders unreadable",
            Self::PropsComma => ", ",
            Self::PropsNotComputed => "Not calculated",
            Self::PropsCalcRunningPrefix => "Calculating… counted ",
            Self::PropsCalcCancelledPrefix => "Cancelled; counted ",
            Self::PropsCalcCancelledSuffix => " (incomplete result)",
            Self::PropsCalcFailedPrefix => "Calculation failed: ",
            Self::PropsUnknownError => "Unknown error",
            Self::PropsRowName => "Name",
            Self::PropsRowLocation => "Location",
            Self::PropsRowType => "Type",
            Self::PropsRowLinkTo => "Links to",
            Self::PropsRowSize => "Size",
            Self::PropsRowMtime => "Modified",
            Self::PropsRowPerms => "Permissions",
            Self::PropsRowOwner => "Owner",
            Self::QueueActiveSuffix => " task(s) in progress ",
            Self::EtaMinuteUnit => " min ",
            Self::EtaMinuteTail => " min",
            Self::EtaSecondUnit => " s",
            Self::EtaHourUnit => " h ",
            Self::QueueFilesSuffix => " files",
            Self::QueueFilesMismatchPrefix => "File count mismatch: done ",
            Self::QueueFilesMismatchMiddle => " / total ",
            Self::QueueBytesMatchSuffix => ", bytes match the source",
            Self::QueueBytesMismatchPrefix => "Byte count mismatch: transferred ",
            Self::QueueBytesMismatchMiddle => " / source ",
            Self::QueueTimeUnderOneSecond => "took < 1 s",
            Self::QueueTimePrefix => "took ",
            Self::QueueTimeAvgMiddle => " s, average ",
            Self::QueueTimeSuffix => " s",
            Self::QueueReconciledPrefix => "Verified: ",
            Self::QueuePartSeparator => "; ",
            Self::ArchiveElapsedPrefix => "",
            Self::ArchiveElapsedSuffix => " s",
            Self::ArchiveCompressingWrittenPrefix => "Compressing… (written ",
            Self::ArchiveCompressingMiddle => " · ",
            Self::ArchiveCompressingSuffix => ")",
            Self::ArchiveCompressingPrefix => "Compressing… (",
            Self::ArchiveScanningPrefix => "Scanning archive… (",
            Self::ArchiveExtractingPrefix => "Extracting… (",
            Self::ArchivePastingPrefix => "Copying… (",
            Self::ArchiveCompressCancelled => {
                "Compression cancelled; no temporary files were left behind"
            }
            Self::ArchiveCompressedPrefix => "Compressed to \"",
            Self::ArchiveCompressedSuffix => "\"",
            Self::ArchiveExtractCancelledInPlacePrefix => {
                "Extraction cancelled; files already extracted remain in \""
            }
            Self::ArchiveExtractCancelledInPlaceSuffix => "\"",
            Self::ArchiveExtractCancelled => {
                "Extraction cancelled; nothing partial was left behind"
            }
            Self::ArchiveExtractedPrefix => "Extracted to \"",
            Self::ArchiveExtractedMiddle => "\": ",
            Self::ArchiveExtractedSuffix => " files",
            Self::ArchiveSkippedPrefix => ", skipped ",
            Self::ArchiveSkippedSuffix => " existing files",
            Self::ArchiveConflictMorePrefix => "and ",
            Self::ArchiveConflictMoreSuffix => " more",
        }
    }
}

/// 新 SFTP 窗口的编号（每台主机可以有多个窗口）。
static NEXT_SFTP: AtomicU64 = AtomicU64::new(1);

/// 文件视图的编号：复制的源认视图（同一个视图里粘贴不用登记对端）。
static NEXT_VIEW: AtomicU64 = AtomicU64::new(1);

/// 窗口生命周期。`Ready` 之后还要等后台任务把目录读回来。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Connecting,
    Ready,
    /// 连不上（首屏失败）。
    Failed,
    /// 连上之后断了（连接被关，或者 SFTP 通道死了）：旧目录还留在界面上，刷新 / 「重新连接」可以恢复。
    Disconnected,
}

impl Phase {
    /// 现在没有可用的连接：`Failed` 和 `Disconnected` 都可以重新连接。
    fn is_down(self) -> bool {
        matches!(self, Phase::Failed | Phase::Disconnected)
    }
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
    /// 本地目录在后台线程里读完了（V2-C3）。`reply.seq` 不是最新一次请求就丢弃。
    LocalListed {
        reply: LocalReply,
        /// 用户 / 历史记录给的原始路径（失败时从历史里拿掉用）。
        requested: String,
        mode: NavMode,
        notice: Option<String>,
        /// 视图刚创建时的首屏：读不出来也要让本地栏显示落点路径。
        initial: bool,
    },
    /// 「下载到…」后台确认本地目录读得出来的结果（`request` 是发起时的序号）。
    DownloadDirChecked {
        request: u64,
        dir: String,
        result: Result<(), String>,
    },
    /// 连接 / 开通道 / 首条目录失败：窗口停在失败态。
    ConnectFailed(String),
    /// 换目录失败：保留旧列表，只换状态栏。旧请求的失败不理会（见 `Listed`）。
    ListFailed {
        seq: u64,
        path: String,
        mode: NavMode,
        message: String,
        /// 这个错误是「通道 / 连接已经断了」一类：视图要转成断开状态，而不只是换状态栏的一句话。
        disconnected: bool,
    },
    /// 传输的进度或结束事件（后台转发任务送过来）。`generation` 是发出它的引擎代号：
    /// 断线重建引擎后，旧引擎排在队里的事件不能再落到界面上。
    Transfer {
        generation: u64,
        event: TransferEvent,
    },
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
        /// 发起粘贴时的引擎代号：做完时引擎已经换了，说明任务是排进旧引擎的。
        generation: u64,
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
    /// 跨主机复制的源端（其余任务是 `None`）。断线重建引擎后重试要靠它把源主机重新登记进新引擎，
    /// 绝不能退回「源 = 目标主机」。
    peer: Option<paste::PeerRef>,
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
            peer: None,
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
        self.notes = report
            .notes
            .iter()
            .map(crate::error_text::reason_text)
            .collect();
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
    /// 本地栏收起了。新开的视图按设置「显示本地栏」决定，面板上的「本地」按钮只改当前这个视图。
    local_hidden: bool,
    /// 本地栏和远端栏怎么排（设置「本地栏排列」；设置页改了，主窗口会推过来）。
    arrange: crate::settings::SftpLocalLayout,
    /// 画面上量到的整个视图的宽度：「自动」排列据此在左右 / 上下之间切换。
    view_width: std::rc::Rc<std::cell::Cell<f32>>,
    /// 设置选了「自定义」目录、且这次真的按它起步时，记着它：落点不是它就在状态栏提示一次。
    start_custom: Option<String>,
    /// 设置选的是「上次用过的」：本地栏换了目录就按主机记下来（其他来源不写库）。
    remember_last: bool,
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
    /// 本地目录读取：文件系统 + 有界线程池，界面线程只提交请求（V2-C3）。
    local_lister: LocalLister,
    /// 本地栏的请求序号：新请求发出后，更早的读取结果作废。
    local_gate: SeqGate,
    /// 「下载到…」确认目录的请求序号。
    probe_gate: SeqGate,
    /// 冲突面板里「重命名」用的名字。
    conflict_name_input: Entity<InputState>,
    /// 连上之后才有：传输引擎和浏览通道分开，互不拖累。
    engine: Option<Arc<TransferEngine>>,
    /// 引擎的代号：每建 / 停一次加一。旧引擎的事件转发任务发现代号变了就退出，旧任务号的事件
    /// 不会落到新引擎的行上（任务号每个引擎都从头数）。
    engine_gen: Arc<AtomicU64>,
    /// 上一次「检测到断线后自动重连」的时间：只自动试一次，间隔内不再自动试（见 `reconnect`）。
    last_auto_reconnect: Option<Instant>,
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
    /// 终端窗格眼下持有的连接（主窗口每个节拍递来，停靠面板用）：首次连接失败后终端重连成功，
    /// 靠它让文件面板用新连接重试。只存弱引用，不拖住连接池里的空闲回收。
    offered: Option<std::sync::Weak<SshSessionFactory>>,
    /// 最近一次连接尝试用的是哪条连接（`None` = 自己按主机拨号）：同一条连接失败了不自动再试。
    attempted: Option<std::sync::Weak<SshSessionFactory>>,
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
    orphan_reports: orphan::OrphanReports,
    /// 递归改权限的取消开关（关窗口时用）。
    chmod_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    archive_caps: ArchiveCapsState,
    /// 正在进行的「移动到…」（同一时间一个）。
    moving: Option<move_to::MoveJob>,
    next_move: u64,
    /// 「移动到…」对话框里目录读取的序号。
    move_gate: SeqGate,
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

        let remote_filter =
            cx.new(|cx| InputState::new(window, cx).placeholder(t(SftpMsg::FilterPlaceholder)));
        let local_filter =
            cx.new(|cx| InputState::new(window, cx).placeholder(t(SftpMsg::FilterPlaceholder)));
        let conflict_name_input = cx
            .new(|cx| InputState::new(window, cx).placeholder(t(SftpMsg::ConflictNamePlaceholder)));
        let remote = Pane::new(show_hidden, remote_filter, cx.focus_handle());
        let mut local = Pane::new(show_hidden, local_filter, cx.focus_handle());
        // 本地栏的落点和首屏列表都在后台线程里定、读（`is_dir`、建临时目录、`read_dir` 都可能很慢）：
        // 这里只先放一个暂定的路径，免得首屏回来之前本地栏的路径是空的。落点规则不变：复制标签带来的
        // 目录必须还是个目录才用，否则默认的临时目录，绝不回退到家目录。
        let wanted_local = start_local.filter(|dir| !dir.is_empty());
        // 起始目录的来源来自设置（下载文件夹 / 这台主机上次用过的 / 自定义）；`ZSSH_SFTP_LOCAL` 和
        // 复制标签带来的目录优先，这两种情况下不看设置。
        let panel = crate::settings_store::snapshot(cx);
        let plan = local_start::StartPlan {
            choice: panel.sftp_local_dir,
            custom: panel.sftp_local_dir_custom.clone(),
            last: (panel.sftp_local_dir == crate::settings::SftpLocalDir::Last)
                .then(|| nav::last_local_dir(host_id))
                .flatten(),
        };
        let start_custom = (wanted_local.is_none()
            && env_value("ZSSH_SFTP_LOCAL").is_none()
            && plan.choice == crate::settings::SftpLocalDir::Custom
            && !plan.custom.trim().is_empty())
        .then(|| plan.custom.trim().to_owned());
        let plan_last = plan.choice == crate::settings::SftpLocalDir::Last;
        local.path = wanted_local.clone().unwrap_or_else(|| {
            absolute_local(&initial_local_candidate(&plan))
                .display()
                .to_string()
        });
        local.loading = true;

        let pending_hooks = test_hooks();
        let (conflict_automation, conflict_automation_error) = conflict_automation();
        let focus = cx.focus_handle();

        let attempted = factory.as_ref().map(Arc::downgrade);
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
                    (
                        Phase::Connecting,
                        format!("{}{host_label}", t(SftpMsg::ConnectingPrefix)),
                    )
                }
                Err(err) => (
                    Phase::Failed,
                    format!("{}{err}", t(SftpMsg::ConnectFailedPrefix)),
                ),
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
            local_hidden: local_hidden_initial(&panel),
            arrange: panel.sftp_local_layout,
            view_width: std::rc::Rc::new(std::cell::Cell::new(0.0)),
            start_custom,
            remember_last: plan_last,
            resolver: None,
            conflict_automation,
            conflicts: VecDeque::new(),
            batch_scope: None,
            listing: ListingGate::default(),
            local_lister: LocalLister::real(plan),
            local_gate: SeqGate::default(),
            probe_gate: SeqGate::default(),
            conflict_name_input,
            engine: None,
            engine_gen: Arc::new(AtomicU64::new(0)),
            last_auto_reconnect: None,
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
            offered: None,
            attempted,
            channel: None,
            busy: None,
            provider: None,
            shell_kind: Arc::new(std::sync::Mutex::new(None)),
            paste_retry: None,
            follow: false,
            last_paste: None,
            orphan_reports: orphan::OrphanReports::new(),
            chmod_cancel: None,
            archive_caps: ArchiveCapsState::Idle,
            moving: None,
            next_move: 1,
            move_gate: SeqGate::default(),
            next_op: 1,
            next_calc: 1,
            focus,
            dialog_focus: cx.focus_handle(),
            _subscriptions: Vec::new(),
            _tick: Task::ready(()),
        };
        this._subscriptions = this.build_subscriptions(window, cx);
        this.request_local_start(wanted_local);
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
            // 窄栏里点开的过滤框：失去焦点、又没输入文字，就收回成放大镜按钮。
            let handle = input.read(cx).focus_handle(cx);
            subscriptions.push(cx.on_blur(&handle, window, move |this, _window, cx| {
                let pane = this.pane_mut(side);
                if pane.filter_open && pane.filter.text.is_empty() {
                    pane.filter_open = false;
                    cx.notify();
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
            let input =
                cx.new(|cx| InputState::new(window, cx).placeholder(t(SftpMsg::FilterPlaceholder)));
            input.update(cx, |state, cx| state.set_value(text, window, cx));
            let pane = self.pane_mut(side);
            pane.filter_input = input;
            pane.path_edit = None;
        }
        self.conflict_name_input = cx
            .new(|cx| InputState::new(window, cx).placeholder(t(SftpMsg::ConflictNamePlaceholder)));
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
            "{}{}{}",
            t(SftpMsg::CloseBusyPrefix),
            self.unfinished_work(),
            t(SftpMsg::CloseBusySuffix)
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
        // 连上之后连接被关了（对端断开 / keepalive 超时）：转成断开状态。通道死了但连接还活着的情形
        // 靠远端操作返回的「通道已关闭」错误发现（见 `SftpEvent::ListFailed`）。
        if reconnect::connection_lost(
            self.phase,
            self.fixture,
            self.factory
                .as_ref()
                .map(|factory| factory.connection().is_closed()),
        ) {
            self.mark_disconnected(cx);
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
            SftpEvent::LocalListed {
                reply,
                requested,
                mode,
                notice,
                initial,
            } => self.apply_local_listed(reply, requested, mode, notice, initial),
            SftpEvent::DownloadDirChecked {
                request,
                dir,
                result,
            } => self.apply_download_dir_checked(request, dir, result, window, cx),
            SftpEvent::ConnectFailed(message) => {
                self.phase = Phase::Failed;
                self.warn(format!("{}{message}", t(SftpMsg::ConnectFailedPrefix)));
            }
            // 换目录失败不动旧列表：用户还在原来的目录里。
            SftpEvent::ListFailed {
                seq,
                path,
                mode,
                message,
                disconnected,
            } => {
                if self.listing.is_latest(seq) {
                    self.remote.loading = false;
                    // 后退 / 前进去不了的目录从历史里拿掉，免得一直撞墙。
                    if mode == NavMode::History && !disconnected {
                        self.remote.history.forget(&path);
                    }
                    self.warn(message);
                    // 通道 / 连接断了：转成断开状态（会先停掉旧引擎、把没完成的传输标为失败，再自动试一次）。
                    if disconnected {
                        self.mark_disconnected(cx);
                    }
                }
            }
            SftpEvent::Transfer { generation, event } => {
                // 旧引擎排在队里的事件：它的行已经标成失败，不能再被盖回去。
                if generation != self.engine_gen.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                let report = match event {
                    TransferEvent::Progress(report) | TransferEvent::Finished(report) => report,
                };
                // 上传完成：远端列表里要马上看到新文件（下载完成刷新本地栏，见 `apply_report`）。
                let finished = matches!(report.state, TransferState::Done);
                let uploaded = report.direction == Direction::Upload && finished;
                let copied = report.direction == Direction::RemoteCopy && finished;
                if !self.transfers.iter().any(|row| row.id == report.id) {
                    // 行还没建（粘贴的任务是后台排进引擎的）：先记着，行建好时补上。
                    // 暂存满了也不丢终态，见 `orphan`。
                    self.orphan_reports.insert(report);
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
                self.warn(t(SftpMsg::WaitingConflict));
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
                        t(SftpMsg::CancelDeleting)
                    } else {
                        t(SftpMsg::Deleting)
                    };
                    let text = format!(
                        "{verb}{}{}{deleted}{}",
                        job.name,
                        t(SftpMsg::DeleteProgressMiddle),
                        t(SftpMsg::DeleteProgressSuffix)
                    );
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
                    self.say(format!(
                        "{}{changed}{}",
                        t(SftpMsg::ChmodProgressPrefix),
                        t(SftpMsg::ChmodProgressSuffix)
                    ));
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
            SftpEvent::PasteDone {
                op,
                generation,
                result,
            } => self.finish_paste(op, generation, result, cx),
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
            .ok_or_else(|| t(SftpMsg::NotConnectedYet).to_owned())
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
            .map_err(|err| error_text(&err))?;
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
        .map_err(|err| error_text(&err))?;
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
        ..crate::window_frame::app_options()
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
        // 「跟随终端目录」只有停靠面板里有开关也有轮询；初始状态取设置，之后只由这个视图自己的勾选改。
        this.follow = dock && crate::settings_store::snapshot(cx).sftp_follow_cwd;
        this
    }

    /// 设置页改了「本地栏排列」：主窗口把新值推给手里的视图。
    pub fn set_arrange(
        &mut self,
        arrange: crate::settings::SftpLocalLayout,
        cx: &mut Context<Self>,
    ) {
        if self.arrange != arrange {
            self.arrange = arrange;
            cx.notify();
        }
    }

    /// 停靠面板 → 标签（或反过来）：同一个实体换个布局，连接和两栏目录原样带过去。
    pub fn set_dock_layout(&mut self, dock: bool, cx: &mut Context<Self>) {
        self.dock = dock;
        cx.notify();
    }

    /// 只改这个视图（设置变了、别的入口已经写过设置时用）。用户点开关走 [`Self::toggle_local_pane`]。
    pub fn set_local_hidden(&mut self, hidden: bool, cx: &mut Context<Self>) {
        if self.local_hidden != hidden {
            self.local_hidden = hidden;
            cx.notify();
        }
    }

    /// 用户收起 / 展开本地栏（停靠面板标题栏的「本地」开关、页头的开关、本地栏工具栏的 ×）：
    /// 视图马上变，同时写设置 `sftp_show_local`（和设置页是同一个开关）。只改视图内存的话，视图被回收 /
    /// 重建后会按设置（默认显示）又冒出来。
    pub fn toggle_local_pane(&mut self, hidden: bool, cx: &mut Context<Self>) {
        self.set_local_hidden(hidden, cx);
        save_show_local(!hidden, cx);
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

    #[cfg(debug_assertions)]
    pub fn is_failed(&self) -> bool {
        self.phase == Phase::Failed
    }

    /// 现在没有可用的连接（连不上，或连上之后断了）：标签右键「重新连接」、面板标题栏的重连按钮据此显示。
    pub fn is_down(&self) -> bool {
        !self.fixture && self.phase.is_down()
    }

    /// 手动重新连接（标签右键、面板标题栏）：回到原来的远端目录。返回是否真的发起了。
    pub fn reconnect(&mut self, cx: &mut Context<Self>) -> bool {
        self.retry_connect(cx)
    }

    #[cfg(any(test, debug_assertions))]
    #[allow(dead_code)]
    pub fn is_disconnected(&self) -> bool {
        self.phase == Phase::Disconnected
    }

    #[cfg(debug_assertions)]
    pub fn remote_cwd(&self) -> &str {
        &self.remote.path
    }

    #[cfg(debug_assertions)]
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
    #[cfg(debug_assertions)]
    pub fn script_upload(&mut self, local: &str, cx: &mut Context<Self>) -> Result<(), String> {
        let result = self.in_new_batch(|this| this.start_upload(local));
        cx.notify();
        result
    }

    /// 驱动脚本用：把远端当前目录里的某个文件下载到本地栏的当前目录。
    #[cfg(debug_assertions)]
    pub fn script_download(&mut self, name: &str, cx: &mut Context<Self>) -> Result<(), String> {
        let remote = join(&self.remote.path, name);
        let dir = Path::new(&self.local.path).to_path_buf();
        let result = self.in_new_batch(|this| this.start_download_to(&dir, &remote));
        cx.notify();
        result
    }

    #[cfg(debug_assertions)]
    pub fn script_local_dir(&mut self, dir: &Path, cx: &mut Context<Self>) {
        self.navigate(Side::Local, &dir.display().to_string(), NavMode::Visit, cx);
    }

    #[cfg(debug_assertions)]
    pub fn script_remote_dir(&mut self, path: &str, cx: &mut Context<Self>) {
        self.navigate(Side::Remote, path, NavMode::Visit, cx);
    }

    /// (已完成, 失败, 还没结束)。
    #[cfg(debug_assertions)]
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
mod local_listing_tests;
#[cfg(test)]
mod move_e2e_tests;
#[cfg(test)]
mod paste_tests;
#[cfg(test)]
mod resubmit_tests;
/// 新建视图时本地栏是不是收起的：就是设置里「显示本地栏」的反面。
fn local_hidden_initial(settings: &crate::settings::UiSettings) -> bool {
    !settings.sftp_show_local
}

/// 把「显示本地栏」写进设置（和设置页同一个开关；设置实体会通知订阅它的窗口并落盘）。
fn save_show_local(show: bool, cx: &mut App) {
    let settings = crate::settings_store::AppSettings::global(cx);
    settings.update(cx, |model, mcx| {
        model.update_with(mcx, |s| s.sftp_show_local = show);
    });
}

#[cfg(test)]
mod tests;
