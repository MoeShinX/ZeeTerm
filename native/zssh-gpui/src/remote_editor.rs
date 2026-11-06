//! 远端文本编辑：主窗口里的编辑器标签（SFTP 菜单 / 双击打开），独立窗口只剩 debug 入口。
//!
//! 一个标签就是一个 [`RemoteEditor`] 实体，标签条、关闭确认、同一个文件不重复开，都在主窗口那边
//! （`main_window/editor_tabs.rs`）；这里管编辑器本身：
//!
//! - **打开**：读远端文件（上限 5 MiB），按 [`contract`] 的契约检查——UTF-8 / BOM / LF / CRLF 原样记下，
//!   二进制、非 UTF-8、换行混用的不打开；语言按 [`language`] 的 0.2.14 映射表认，文件名认不出再看 shebang。
//! - **保存**：保存前重读一遍当前内容比版本，远端被改过（或删了）一次都不写；一致才交给核心的
//!   `EditBackend::overwrite`（同目录临时文件 + 核对大小 + 改名，保留权限 / 属主，符号链接写真实文件）。
//!   同一服务器同一文件的保存在进程内串行（[`remote`] 的 `write_gate`），两个编辑器拿着同一个旧版本
//!   同时保存，只有一个写成，另一个报冲突。
//! - **冲突**：覆盖远端 / 放弃我的修改（换成远端现在的内容）/ 另存为（同目录新名字，之后编辑器改跟踪
//!   新文件）/ 稍后处理（收起，编辑内容留着，再保存会再比一次）。
//! - **属主**：保不住时什么都不替换，问 仍然保存（属主改变）/ 直接写入原文件 / 取消。
//! - **保存期间继续输入**：保存拿到的是点保存那一刻的文本；成功后基线只挪到那份文本，之后打的字仍然
//!   算未保存，草稿也只删「修订号不晚于那次保存」的（见 [`drafts`]）。每次保存带一个编号，过期的
//!   回调（已经被别的操作取代）一律忽略。
//! - **草稿**：编辑过程中每 2 秒把未保存的文本交给草稿写线程（`<数据目录>/edit-drafts/`），崩溃后
//!   启动时由主窗口的恢复提示列出来。
//! - **断线**：保存时发现连接没了，内容留在编辑器里，出现「重新连接」，连上后重试保存。
//!
//! 编辑器组件是 gpui-kit 0.8.1 的 Editor；它没有的几个命令（跳行、注释、行操作、括号配对高亮……）
//! 在 [`commands`] / [`lines`] / [`brackets`] 里补，能力对照见 `docs/reports/V2-G09.md`。

mod brackets;
mod commands;
mod contract;
mod drafts;
mod language;
mod lines;
#[cfg(test)]
mod live_tests;
pub(crate) mod open_files;
pub(crate) mod recovery;
mod remote;
mod view;

pub use commands::init as init_commands;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use ::gpui::{
    App, ClipboardItem, EventEmitter, FocusHandle, Subscription, Task, Window, WindowBounds,
    WindowOptions,
};
use gpui_kit::component::input::{
    EditorState, InputEvent, InputState, Position, RangeDecoration, RangeDecorationCollection,
    RangeDecorationStyle, RopeExt as _,
};
use gpui_kit::*;
use zssh_core::sftp::SftpChannel;
use zssh_core::sftp::edit::OwnerMode;
use zssh_core::sftp::edit::text::TextFormat;
use zssh_core::sftp::path::{file_name, normalize};

use crate::install_state::{Registration, WorkKind};
use crate::ssh_backend::runtime;
use contract::{MAX_BYTES, Opened, RemoteVersion};
use drafts::{
    DbDraftCipher, DraftJob, DraftNotice, DraftWriter, EditDraft, draft_age_label, draft_file_in,
    draft_notice, export_recovered, legacy_draft_file_in, now_unix, read_draft_with,
    read_legacy_draft_with, set_aside_locked,
};
// 恢复列表的「另存到本地」也要在界面上说明导出的是明文。
pub(crate) use drafts::export_plaintext_note;
use language::Lang;
use remote::{RemoteGlimpse, RemoteStamp, RemoteWatch};

/// 编辑器状态栏 / 关闭确认的文案。
#[derive(Clone, Copy, Debug)]
enum StatusMsg {
    ReadingInitial,
    Reconnecting,
    DisconnectedRead,
    Reconnected,
    RemoteDeletedRefused,
    RemoteChangedRefused,
    ConflictCloseSave,
    OwnerChangeCloseSave,
    DisconnectedKeepEditing,
    RemoteChangedConflict,
    ReloadConflictNote,
    ReadingWaitSave,
    ReadFailedNoSave,
    NoRemoteVersion,
    Saving,
    ConflictParked,
    DisconnectedCannotReadRemote,
    ReadingRemoteNow,
    DisconnectedNoSaveAs,
    SavingAs,
    ReadingWaitSaveAs,
    ReadFailedNoSaveAs,
    DraftSavingLocal,
    DraftCopied,
    OldDraftKept,
    DraftDiscarded,
    UnsavedKeptAsDraft,
    GotoInvalid,
    ConnectFailedPrefix,
    ReadFailedPrefix,
    SavedPrefix,
    /// `count()` 的中文量词后缀；英文界面下 `count()` 用 byte / bytes，这里只是占位。
    ByteSuffix,
    SavedClose,
    SavedOwnerMid,
    SavedOwnerTo,
    OwnerChangePrefix,
    OwnerChangeMid,
    OwnerChangeSuffix,
    DraftExportPrefix,
    DraftExportMid,
    DraftExportFailedPrefix,
    SavedAsPrefix,
    SavedAsEditingMid,
    LoadedPrefix,
    LoadedClose,
    DiscardReloadPrefix,
    DiscardReloadClose,
    ReloadFailedPrefix,
    KeptAsPrefix,
    MoveAsideFailedPrefix,
    MoveAsideFailedSuffix,
    TooLargePrefix,
    TooLargeMid,
    TooLargeSaveSuffix,
    TooLargeSaveAsSuffix,
    SavingAsTargetPrefix,
    DraftRestoredPrefix,
    DraftRestoredSuffix,
    GotoLinePrefix,
    GotoLineSuffix,
    SaveFailedPrefix,
    SaveAsFailedPrefix,
}

impl crate::i18n::Msg for StatusMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::ReadingInitial => "正在读取…",
            Self::Reconnecting => "正在重新连接…",
            Self::DisconnectedRead => "连接已经断开，读不了文件",
            Self::Reconnected => "已重新连接",
            Self::RemoteDeletedRefused => "远端文件已经被删掉了，保存被拒绝",
            Self::RemoteChangedRefused => "远端文件已被外部修改，保存被拒绝",
            Self::ConflictCloseSave => "远端文件已被改过，有冲突要处理",
            Self::OwnerChangeCloseSave => "保存会改变文件属主，要先决定",
            Self::DisconnectedKeepEditing => "连接已经断开，内容还留在编辑器里；重新连接后再保存",
            Self::RemoteChangedConflict => "远端文件已被外部修改；保存会冲突（内容不会被覆盖）",
            Self::ReloadConflictNote => {
                "重新载入期间内容有改动，没有覆盖；你输入的内容还在，冲突仍待处理"
            }
            Self::ReadingWaitSave => "文件还在读取，稍后再保存",
            Self::ReadFailedNoSave => "文件没有读取成功，不能保存",
            Self::NoRemoteVersion => "还没有读到远端版本，不能保存",
            Self::Saving => "正在保存…",
            Self::ConflictParked => "冲突先放着：编辑内容还在，再保存会重新核对远端",
            Self::DisconnectedCannotReadRemote => "连接已经断开，读不了远端内容",
            Self::ReadingRemoteNow => "正在读取远端现在的内容…",
            Self::DisconnectedNoSaveAs => "连接已经断开，不能另存为",
            Self::SavingAs => "正在另存为…",
            Self::ReadingWaitSaveAs => "文件还在读取，稍后再另存为",
            Self::ReadFailedNoSaveAs => "文件没有读取成功，不能另存为",
            Self::DraftSavingLocal => "正在把草稿另存为本地文件…",
            Self::DraftCopied => "草稿内容已复制到剪贴板",
            Self::OldDraftKept => "旧格式草稿已保留在草稿目录里，没有删除",
            Self::DraftDiscarded => "草稿已丢弃",
            Self::UnsavedKeptAsDraft => "有未保存的修改：已存为草稿，下次启动可以恢复",
            Self::GotoInvalid => "请输入行号（或 行:列）",
            Self::ConnectFailedPrefix => "连接失败：",
            Self::ReadFailedPrefix => "读取失败：",
            Self::SavedPrefix => "已保存（",
            Self::ByteSuffix => " 字节",
            Self::SavedClose => "）",
            Self::SavedOwnerMid => "），属主从 ",
            Self::SavedOwnerTo => " 变成了 ",
            Self::OwnerChangePrefix => "保存会让属主从 ",
            Self::OwnerChangeMid => " 变成 ",
            Self::OwnerChangeSuffix => "，没有替换远端文件",
            Self::DraftExportPrefix => "草稿已另存为本地文件（",
            Self::DraftExportMid => "）：",
            Self::DraftExportFailedPrefix => "草稿导出失败：",
            Self::SavedAsPrefix => "已另存为 ",
            Self::SavedAsEditingMid => "，现在编辑的是这个新文件",
            Self::LoadedPrefix => "已加载（",
            Self::LoadedClose => "）",
            Self::DiscardReloadPrefix => "已放弃本地修改，换成了远端现在的内容（",
            Self::DiscardReloadClose => "）",
            Self::ReloadFailedPrefix => "没能重新载入远端内容：",
            Self::KeptAsPrefix => "，已原样保留为 ",
            Self::MoveAsideFailedPrefix => "（挪到一边失败：",
            Self::MoveAsideFailedSuffix => "；继续编辑会覆盖它）",
            Self::TooLargePrefix => "内容太大（",
            Self::TooLargeMid => "，上限 ",
            Self::TooLargeSaveSuffix => "），拒绝保存",
            Self::TooLargeSaveAsSuffix => "），拒绝另存为",
            Self::SavingAsTargetPrefix => "正在另存为 ",
            Self::DraftRestoredPrefix => "已恢复草稿（",
            Self::DraftRestoredSuffix => "），尚未保存",
            Self::GotoLinePrefix => "已跳到第 ",
            Self::GotoLineSuffix => " 行",
            Self::SaveFailedPrefix => "保存失败：",
            Self::SaveAsFailedPrefix => "另存为失败：",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::ReadingInitial => "Reading…",
            Self::Reconnecting => "Reconnecting…",
            Self::DisconnectedRead => "Connection lost — cannot read the file",
            Self::Reconnected => "Reconnected",
            Self::RemoteDeletedRefused => "The remote file was deleted; the save was refused",
            Self::RemoteChangedRefused => {
                "The remote file changed externally; the save was refused"
            }
            Self::ConflictCloseSave => "The remote file changed; resolve the conflict first",
            Self::OwnerChangeCloseSave => "Saving would change the file owner; decide first",
            Self::DisconnectedKeepEditing => {
                "Connection lost; your content is still here — reconnect before saving"
            }
            Self::RemoteChangedConflict => {
                "The remote file changed externally; saving would conflict (nothing was overwritten)"
            }
            Self::ReloadConflictNote => {
                "Content changed during the reload; nothing was overwritten — your input is kept and the conflict remains"
            }
            Self::ReadingWaitSave => "The file is still loading — save later",
            Self::ReadFailedNoSave => "The file could not be read; saving is not possible",
            Self::NoRemoteVersion => "No remote version has been read yet; saving is not possible",
            Self::Saving => "Saving…",
            Self::ConflictParked => {
                "The conflict is left open: your edits are kept and saving again re-checks the remote file"
            }
            Self::DisconnectedCannotReadRemote => {
                "Connection lost — cannot read the remote content"
            }
            Self::ReadingRemoteNow => "Reading the current remote content…",
            Self::DisconnectedNoSaveAs => "Connection lost — cannot save as",
            Self::SavingAs => "Saving as…",
            Self::ReadingWaitSaveAs => "The file is still loading — save as later",
            Self::ReadFailedNoSaveAs => "The file could not be read; save as is not possible",
            Self::DraftSavingLocal => "Saving the draft to a local file…",
            Self::DraftCopied => "Draft content copied to the clipboard",
            Self::OldDraftKept => {
                "The old-format draft was kept in the drafts folder and not deleted"
            }
            Self::DraftDiscarded => "Draft discarded",
            Self::UnsavedKeptAsDraft => {
                "Unsaved changes were kept as a draft and can be recovered on the next launch"
            }
            Self::GotoInvalid => "Enter a line number (or line:column)",
            Self::ConnectFailedPrefix => "Connection failed: ",
            Self::ReadFailedPrefix => "Read failed: ",
            Self::SavedPrefix => "Saved (",
            Self::ByteSuffix => " bytes",
            Self::SavedClose => ")",
            Self::SavedOwnerMid => "); owner changed from ",
            Self::SavedOwnerTo => " to ",
            Self::OwnerChangePrefix => "Saving would change the owner from ",
            Self::OwnerChangeMid => " to ",
            Self::OwnerChangeSuffix => "; the remote file was not replaced",
            Self::DraftExportPrefix => "Draft saved to a local file (",
            Self::DraftExportMid => "): ",
            Self::DraftExportFailedPrefix => "Could not export the draft: ",
            Self::SavedAsPrefix => "Saved as ",
            Self::SavedAsEditingMid => " — you are now editing this new file",
            Self::LoadedPrefix => "Loaded (",
            Self::LoadedClose => ")",
            Self::DiscardReloadPrefix => {
                "Local changes discarded; the editor now shows the current remote content ("
            }
            Self::DiscardReloadClose => ")",
            Self::ReloadFailedPrefix => "Could not reload the remote content: ",
            Self::KeptAsPrefix => ", kept as ",
            Self::MoveAsideFailedPrefix => " (could not move it aside: ",
            Self::MoveAsideFailedSuffix => "; continuing to edit will overwrite it)",
            Self::TooLargePrefix => "The content is too large (",
            Self::TooLargeMid => ", limit ",
            Self::TooLargeSaveSuffix => "); saving refused",
            Self::TooLargeSaveAsSuffix => "); save as refused",
            Self::SavingAsTargetPrefix => "Saving as ",
            Self::DraftRestoredPrefix => "Restored draft (",
            Self::DraftRestoredSuffix => "), not saved yet",
            Self::GotoLinePrefix => "Jumped to line ",
            Self::GotoLineSuffix => "",
            Self::SaveFailedPrefix => "Save failed: ",
            Self::SaveAsFailedPrefix => "Save as failed: ",
        }
    }
}

/// 字节数的说法：中文「数字 + 字节」，英文 `1 byte` / `2 bytes`。
fn byte_count(n: u64) -> String {
    crate::i18n::count(n, crate::i18n::t(StatusMsg::ByteSuffix), "byte", "bytes")
}

/// 界面事件轮询间隔；草稿自动保存也挂在这个节拍上。
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// 草稿自动保存间隔，按 100ms 节拍折算成 20 次。
const DRAFT_TICKS: u32 = 20;
/// 退出前等草稿写线程落盘的最长时间。
const QUIT_FLUSH_WAIT: Duration = Duration::from_secs(3);

/// 标签生命周期：读回来之前不能编辑、不能保存。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Phase {
    /// 正在拨号 / 开 SFTP 通道（从恢复提示打开时没有现成的通道）。
    Connecting,
    Loading,
    Ready,
    Failed,
}

/// 通道从哪来：SFTP 视图已经连好的，或者编辑器自己连（连接池里有同配置的活连接就复用）。
pub(crate) enum ChannelSource {
    Ready(Arc<SftpChannel>),
    Dial,
}

/// 打开一个编辑器标签要的全部信息。
pub(crate) struct OpenSpec {
    pub host_id: i64,
    pub host_label: String,
    pub path: String,
    pub source: ChannelSource,
    /// 打开这个编辑器的资料库身份。由主窗口在已打开的 HostStore 上取，
    /// 避免编辑器在界面线程里再开一次库、再等一次 Keychain。
    pub vault_account: Option<String>,
}

/// 编辑器告诉宿主（主窗口 / 独立窗口）的事。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EditorEvent {
    /// 保存状态、未保存标记、路径之类变了（标签标题、关闭确认要跟着变）。
    Changed,
    /// 编辑器自己决定关掉（独立窗口的「保存并关闭」「不保存」）。
    CloseNow,
}

impl EventEmitter<EditorEvent> for RemoteEditor {}

/// 后台任务 → 界面的单向事件（`std::sync::mpsc`，由 100ms tick 取走）。
///
/// 带 `save_id` 的是保存相关：过期的（已经被新的保存取代的）一律忽略。
enum RemoteEditEvent {
    Connected(Arc<SftpChannel>),
    ConnectFailed(String),
    Loaded {
        opened: Opened,
        stamp: Option<RemoteStamp>,
    },
    LoadFailed(remote::LoadFailure),
    Saved {
        save_id: u64,
        version: RemoteVersion,
        size: usize,
        /// 写完之后重新 `stat` 一次得到的标记，避免监视器把这次写入当成外部修改。
        stamp: Option<RemoteStamp>,
        /// 用户同意改属主、而服务器又不让设回去：保存成功了，但要如实说。
        owner_changed: Option<(String, String)>,
    },
    /// 保存前重读的版本和打开时不一致：不写。
    Conflict {
        save_id: u64,
        remote: RemoteGlimpse,
    },
    /// 属主保持不了，**什么都没替换**：等用户决定要不要仍然保存。
    OwnerWouldChange {
        save_id: u64,
        from: String,
        to: String,
    },
    SaveFailed {
        save_id: u64,
        message: String,
        /// 是连接断了：给「重新连接」。
        disconnected: bool,
    },
    /// 草稿导出成本地文件的结果（成功是文件路径）。
    Exported(Result<PathBuf, String>),
    /// 只提醒，不改编辑器里的文字和版本；真正是否冲突仍由保存时的哈希比较决定。
    ExternalChange,
    /// 另存为（副本）成功：只报告副本路径，当前文件和版本都不变。
    SavedAs {
        path: String,
    },
    SaveAsFailed(String),
    /// 冲突里「放弃我的修改」读回来的远端内容；`revision` 是发起时的修订号。
    Reloaded {
        revision: u64,
        loaded: Box<remote::Loaded>,
    },
    ReloadFailed(String),
    /// 冲突里「另存为」成功：编辑器改跟踪新文件，`text` 是写进去的那份。
    Relocated {
        path: String,
        version: RemoteVersion,
        stamp: Option<RemoteStamp>,
        text: String,
    },
    RelocateFailed(String),
}

/// 一次正在进行的保存：只认这个编号的回调。
struct ActiveSave {
    id: u64,
    /// 点保存那一刻编辑器内容的修订号；成功后草稿只删到这个修订号。
    revision: u64,
    /// 点保存那一刻的文本；成功后它成为新基线。
    text: String,
}

/// 只接受当前保存的回调；先换基线再判断剩余修改，撤销回旧基线也不能漏掉。
fn apply_saved_snapshot(
    active_save: &mut Option<ActiveSave>,
    save_id: u64,
    baseline: &mut String,
    dirty_cache: &mut Option<(u64, bool)>,
) -> Option<u64> {
    let active = active_save.take_if(|active| active.id == save_id)?;
    *baseline = active.text;
    *dirty_cache = None;
    Some(active.revision)
}

/// 保存怎么写。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SaveKind {
    /// 先比版本，一致才写。
    Checked(OwnerMode),
    /// 用户在冲突提示里选了「覆盖远端」：不比版本。
    Overwrite(OwnerMode),
}

impl SaveKind {
    fn owner(self) -> OwnerMode {
        match self {
            Self::Checked(owner) | Self::Overwrite(owner) => owner,
        }
    }
}

/// 冲突提示。
struct ConflictState {
    remote: RemoteGlimpse,
    /// 点了「稍后处理」：收成一行，编辑内容留着。
    later: bool,
}

/// 「保存并关闭」这件事现在到哪一步（宿主的关闭确认框据此继续或报错）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CloseSave {
    Saving,
    Done,
    Failed(String),
}

#[derive(Debug, PartialEq, Eq)]
enum CloseSaveStep {
    Idle,
    Wait,
    Save,
    Done,
}

impl CloseSave {
    /// 失败或取消后不能被迟到的成功回调重新变成自动关闭。
    fn advance(state: &mut Option<Self>, active: bool, unsaved: bool) -> CloseSaveStep {
        if *state != Some(Self::Saving) {
            return CloseSaveStep::Idle;
        }
        if active {
            CloseSaveStep::Wait
        } else if unsaved {
            CloseSaveStep::Save
        } else {
            *state = Some(Self::Done);
            CloseSaveStep::Done
        }
    }

    fn fail(state: &mut Option<Self>, reason: &str) {
        if *state == Some(Self::Saving) {
            *state = Some(Self::Failed(reason.to_owned()));
        }
    }
}

/// 在途快照可能马上改变远端；即使当前文本退回旧基线，也需要草稿兜底。
fn needs_draft(text: &str, baseline: &str, active: Option<&ActiveSave>) -> bool {
    text != baseline || active.is_some_and(|save| save.text != text)
}

/// 状态栏用的光标信息（节拍里算好缓存，渲染时不再扫文本）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CursorInfo {
    pub line: usize,
    pub column: usize,
    /// 选中了多少个字符（没选 = 0）。
    pub selected: usize,
}

/// 编辑器自己的文案（输入框占位、窗口标题）。
#[derive(Clone, Copy, Debug)]
enum EditorMsg {
    PathPlaceholder,
    GotoPlaceholder,
    ConflictNamePlaceholder,
    TitlePrefix,
}

impl crate::i18n::Msg for EditorMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::PathPlaceholder => "远端路径（可相对当前目录）",
            Self::GotoPlaceholder => "行号，或 行:列",
            Self::ConflictNamePlaceholder => "新文件名",
            Self::TitlePrefix => "编辑 ",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::PathPlaceholder => "Remote path (relative to the current directory)",
            Self::GotoPlaceholder => "Line, or line:column",
            Self::ConflictNamePlaceholder => "New file name",
            Self::TitlePrefix => "Editing ",
        }
    }
}

pub struct RemoteEditor {
    editor: Entity<EditorState>,
    path: String,
    /// 主机库 id：草稿按它分开，恢复时也要对上。
    host_id: i64,
    host_label: String,
    phase: Phase,
    lang: Lang,
    /// 打开时文件的样子（BOM、换行）；保存时原样还原。
    format: TextFormat,
    version: Option<RemoteVersion>,
    status: String,
    /// 编辑器内容的修订号：每次用户改动 +1，只增不减。
    revision: u64,
    /// （算「是否有未保存修改」时的修订号，结果）：同一个修订号不重复扫文本。
    dirty_cache: Option<(u64, bool)>,
    /// 最近一次加载或保存成功时的文本：和编辑器内容不同才需要草稿。
    baseline: String,
    next_save_id: u64,
    active_save: Option<ActiveSave>,
    /// 保存失败 / 断线之后「重新连接」成功要不要自动再保存一次。
    retry_save: Option<SaveKind>,
    /// 另存为（副本）输入框：相对路径按当前文件的父目录解析。
    save_as_input: Entity<InputState>,
    save_as_open: bool,
    /// 另存为任务在跑：和保存分开计数，两个按钮互不影响。
    saving_as: bool,
    /// 跳行条的输入框。
    goto_input: Entity<InputState>,
    goto_open: bool,
    /// 窗口关掉时清空；后台任务手里的是另一份 `Arc`。
    channel: Option<Arc<SftpChannel>>,
    /// 连接没了（保存 / 读取时发现）：显示「重新连接」。
    disconnected: bool,
    reconnecting: bool,
    /// 外部修改监视：加载/保存成功时更新标记，关闭时置停。
    watch: Arc<RemoteWatch>,
    /// 数据目录：打开时取一次，草稿、旧草稿导出都用它，中途不跟着环境变量变。
    data_dir: PathBuf,
    /// 这台主机上这个远端路径对应的草稿文件。
    draft_path: PathBuf,
    /// 草稿加密绑定的资料库账户；拿不到时草稿功能关闭，绝不使用共享默认密钥。
    draft_account: Option<String>,
    /// 草稿写线程：自动保存、保存成功和丢弃都只往这里丢任务。
    drafts: DraftWriter,
    /// 加载完成时发现的草稿；用户没有动作之前一直显示提示条。
    draft: Option<DraftNotice>,
    /// 提示条里那份旧草稿已经另存成本地文件了：之后自动保存覆盖草稿文件也不会丢它。
    draft_preserved: bool,
    /// 用户刚丢弃的文本：内容没再变之前不再自动写回同一份草稿。
    discarded_text: Option<String>,
    /// 属主保不住、等用户决定要不要仍然保存：`(原属主, 新属主)`。
    owner_prompt: Option<(String, String)>,
    conflict: Option<ConflictState>,
    /// 冲突里「另存为」的新名字输入框。
    conflict_name: Entity<InputState>,
    /// 远端在我们不知道的时候变了（2 秒一次的 `stat` 发现的），只提醒。
    external_changed: bool,
    /// 读完文件后第一次画出来时把键盘焦点交给编辑器（标签先切过来、后读完，那时编辑区还没有）。
    want_focus: bool,
    wrap: bool,
    cursor: CursorInfo,
    /// 括号配对 / 选中内容其他出现处的高亮：上次算的时候的 (光标, 选区, 修订号)。
    marks_key: Option<(usize, std::ops::Range<usize>, u64)>,
    bracket_marks: Option<RangeDecorationCollection>,
    match_marks: Option<RangeDecorationCollection>,
    /// 「保存并关闭」的进度；`None` = 没在做。
    close_save: Option<CloseSave>,
    /// 独立窗口（debug 入口）：关窗口的确认条画在编辑器自己里面。
    standalone: bool,
    close_prompt: bool,
    close_confirmed: bool,
    /// 编辑器字号（打开时读的界面设置）。
    font_size: f32,
    /// 登记到进程级的「未保存修改」计数：退出 / 安装前检查用。
    unsaved_registration: Registration,
    /// 登记「这个远端文件正开在编辑器里」：文件视图的「移动到…」据此跳过它（V2-U23）。
    open_file: open_files::OpenFile,
    /// `ZSSH_EDIT_SAVE=<text>`：文件读完后替换成这段文本并保存一次（自动化测试用）。
    automation_save: Option<String>,
    /// `ZSSH_EDIT_SAVE_AS=<path>`：保存之后再另存一份（自动化测试用）。
    automation_save_as: Option<String>,
    /// `ZSSH_EDIT_DRAFT=<text>`：读完后换成这段文本但不保存，等自动保存写出草稿。
    automation_draft: Option<String>,
    /// `ZSSH_EDIT_DRAFT_ACTION=restore|discard`：草稿条出现后自动点一次（自动化测试用）。
    automation_draft_action: Option<String>,
    events: Receiver<RemoteEditEvent>,
    sender: Sender<RemoteEditEvent>,
    focus: FocusHandle,
    _subs: Vec<Subscription>,
    _tick: Task<()>,
}

impl RemoteEditor {
    pub(crate) fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        spec: OpenSpec,
        standalone: bool,
    ) -> Self {
        let OpenSpec {
            host_id,
            host_label,
            path,
            source,
            vault_account,
        } = spec;
        let (sender, events) = std::sync::mpsc::channel();
        let lang = language::language_for(&path);
        let ui = crate::settings_store::snapshot(cx);
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .line_number(true)
                .language(lang.syntax_token())
                // 0.2.14 默认不换行（有「自动换行」开关），组件默认是换行。
                .soft_wrap(false)
                // 0.2.14 的 CodeMirror 没开 closeBrackets：打 `(` 只出一个括号。
                .auto_close(false)
        });
        let save_as_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::i18n::t(EditorMsg::PathPlaceholder))
        });
        let goto_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::i18n::t(EditorMsg::GotoPlaceholder))
        });
        let conflict_name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::i18n::t(EditorMsg::ConflictNamePlaceholder))
        });
        let var = |name: &str| {
            crate::dev_hooks::var_os(name).map(|value| value.to_string_lossy().into_owned())
        };
        let focus = cx.focus_handle();
        let watch = Arc::new(RemoteWatch::default());
        // 数据目录只取一次：草稿路径和写线程用的是同一份，运行中改环境变量不会拆开它们。
        let data_dir = crate::settings::data_dir();
        // 资料库已经由主窗口打开；这里只接收身份，不在界面线程重新开库。
        let draft_account = vault_account;
        let draft_path = draft_file_in(&data_dir, host_id, &path);
        let open_file = open_files::OpenFile::register(host_id, &path);

        let mut subs = Vec::new();
        // 用户改了文字：修订号 +1。程序化替换（加载、放弃修改、恢复草稿）不发这个事件，那几处自己加。
        subs.push(cx.subscribe_in(
            &editor,
            window,
            |this, _editor, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.touch(cx);
                }
            },
        ));
        // 跳行条里按回车。
        subs.push(cx.subscribe_in(
            &goto_input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.submit_goto(window, cx);
                }
            },
        ));
        // 查找面板在编辑器里开关（Esc、面板上的 ×、快捷键）：工具栏「查找」「替换」的按下态跟着变。
        // 编辑器每次光标闪烁都会通知，只在面板状态真变了时才重画工具栏。
        let mut search_shown = (false, false);
        subs.push(cx.observe(&editor, move |_this, editor, cx| {
            let session = editor.read(cx).search_session();
            let now = (session.open, session.replace_mode);
            if now != search_shown {
                search_shown = now;
                cx.notify();
            }
        }));
        // 输入框自己的状态变了（文字、光标）：本视图跟着重画。
        for input in [&save_as_input, &goto_input, &conflict_name] {
            subs.push(cx.observe(input, |_this, _input, cx| cx.notify()));
        }

        let mut this = Self {
            editor,
            path,
            host_id,
            host_label,
            phase: Phase::Connecting,
            lang,
            format: TextFormat::default(),
            version: None,
            status: crate::i18n::t(StatusMsg::ReadingInitial).to_owned(),
            revision: 0,
            dirty_cache: None,
            baseline: String::new(),
            next_save_id: 1,
            active_save: None,
            retry_save: None,
            save_as_input,
            save_as_open: false,
            saving_as: false,
            goto_input,
            goto_open: false,
            channel: None,
            disconnected: false,
            reconnecting: false,
            watch,
            data_dir,
            draft_path,
            draft_account: draft_account.clone(),
            drafts: DraftWriter::spawn(draft_account),
            draft: None,
            draft_preserved: false,
            discarded_text: None,
            owner_prompt: None,
            conflict: None,
            conflict_name,
            external_changed: false,
            want_focus: false,
            wrap: false,
            cursor: CursorInfo {
                line: 1,
                column: 1,
                selected: 0,
            },
            marks_key: None,
            bracket_marks: None,
            match_marks: None,
            close_save: None,
            standalone,
            close_prompt: false,
            close_confirmed: false,
            font_size: ui.font_size,
            unsaved_registration: Registration::new(WorkKind::UnsavedEdits),
            open_file,
            automation_save: var("ZSSH_EDIT_SAVE"),
            automation_save_as: var("ZSSH_EDIT_SAVE_AS"),
            automation_draft: var("ZSSH_EDIT_DRAFT"),
            automation_draft_action: var("ZSSH_EDIT_DRAFT_ACTION"),
            events,
            sender,
            focus,
            _subs: subs,
            _tick: Task::ready(()),
        };
        match source {
            ChannelSource::Ready(channel) => this.attach_channel(channel, cx),
            ChannelSource::Dial => this.dial(cx),
        }
        // 100ms 轮询一次后台事件；每 20 次（约 2 秒）顺手写一次草稿。窗口没了
        // `update_in` 会报错，任务自己退出。
        this._tick = cx.spawn_in(window, async move |this, cx| {
            let mut ticks: u32 = 0;
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                ticks = ticks.wrapping_add(1);
                let draft_due = ticks.is_multiple_of(DRAFT_TICKS);
                let alive = this.update_in(cx, move |this, window, cx| {
                    this.poll_events(window, cx);
                    this.refresh_cursor(cx);
                    this.refresh_marks(cx);
                    this.refresh_registration(cx);
                    if draft_due {
                        this.autosave_draft(false, cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        this
    }

    // ---- 给宿主（主窗口标签 / 独立窗口）看的 ----

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// 编辑器里真正拿键盘的那个输入区（宿主切到这个标签时把焦点交给它）。
    pub(crate) fn editor_focus(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }

    pub(crate) fn host_label(&self) -> &str {
        &self.host_label
    }

    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    /// 标签标题：文件名。
    pub(crate) fn file_name(&self) -> String {
        file_name(&self.path)
    }

    /// 「同一个文件」的身份：主机 + 规范化后的远端路径。
    pub(crate) fn identity(&self) -> (i64, String) {
        (self.host_id, normalize(&self.path))
    }

    pub(crate) fn phase(&self) -> Phase {
        self.phase
    }

    #[cfg(debug_assertions)]
    pub(crate) fn status_text(&self) -> &str {
        &self.status
    }

    #[cfg(debug_assertions)]
    pub(crate) fn lang(&self) -> Lang {
        self.lang
    }

    #[cfg(debug_assertions)]
    pub(crate) fn format(&self) -> &TextFormat {
        &self.format
    }

    #[cfg(debug_assertions)]
    pub(crate) fn draft_bar_kind(&self) -> Option<&'static str> {
        self.draft.as_ref().map(|notice| match notice {
            DraftNotice::Restorable(_) => "restorable",
            DraftNotice::Stale(_) => "stale",
            DraftNotice::Legacy(_) => "legacy",
        })
    }

    #[cfg(debug_assertions)]
    pub(crate) fn current_text(&self, cx: &App) -> String {
        self.editor.read(cx).value().to_string()
    }

    #[cfg(debug_assertions)]
    pub(crate) fn cursor_info(&self) -> CursorInfo {
        self.cursor
    }

    pub(crate) fn close_save_state(&self) -> Option<&CloseSave> {
        self.close_save.as_ref()
    }
    /// 不动状态的读法（渲染标签标题用）：上一个节拍算好的结果，最多旧 100ms。
    pub(crate) fn unsaved_hint(&self) -> bool {
        self.active_save.is_some()
            || matches!(self.dirty_cache, Some((revision, true)) if revision == self.revision)
    }

    /// 编辑器里有没有没落到远端的内容（正在保存的也算：保存还没成功）。
    ///
    /// 同一个修订号只扫一次文本，闲着时是 O(1)。
    pub(crate) fn has_unsaved(&mut self, cx: &App) -> bool {
        if self.phase != Phase::Ready {
            return false;
        }
        if self.active_save.is_some() {
            return true;
        }
        if let Some((revision, dirty)) = self.dirty_cache
            && revision == self.revision
        {
            return dirty;
        }
        let dirty = self.editor.read(cx).value().as_ref() != self.baseline.as_str();
        self.dirty_cache = Some((self.revision, dirty));
        dirty
    }

    // ---- 连接 / 读取 ----

    /// 有了现成的通道：起监视器，开始读文件。
    fn attach_channel(&mut self, channel: Arc<SftpChannel>, cx: &mut Context<Self>) {
        self.restart_watcher(&channel);
        self.channel = Some(channel);
        self.phase = Phase::Loading;
        self.start_loading(cx);
    }

    /// 换一个新的外部修改监视（通道换了 / 路径换了）：旧的置停，新的从头开始。
    fn restart_watcher(&mut self, channel: &Arc<SftpChannel>) {
        self.watch.stop();
        self.watch = Arc::new(RemoteWatch::default());
        remote::start_watcher(
            Arc::downgrade(channel),
            self.path.clone(),
            self.sender.clone(),
            Arc::clone(&self.watch),
        );
    }

    /// 自己开通道：连接池里有同配置的活连接就复用，没有就重新握手。
    fn dial(&mut self, cx: &mut Context<Self>) {
        self.reconnecting = true;
        let host_id = self.host_id;
        let sender = self.sender.clone();
        match runtime() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    let event = match remote::dial_channel(host_id).await {
                        Ok(channel) => RemoteEditEvent::Connected(channel),
                        Err(message) => RemoteEditEvent::ConnectFailed(message),
                    };
                    let _ = sender.send(event);
                });
            }
            Err(err) => {
                self.reconnecting = false;
                self.phase = Phase::Failed;
                self.status = format!("{}{err}", crate::i18n::t(StatusMsg::ConnectFailedPrefix));
            }
        }
        cx.notify();
    }

    /// 「重新连接」按钮：开新通道，连上后（有待重试的保存就）再保存一次。
    pub(crate) fn reconnect(&mut self, cx: &mut Context<Self>) {
        if self.reconnecting {
            return;
        }
        self.status = crate::i18n::t(StatusMsg::Reconnecting).to_owned();
        self.dial(cx);
    }

    /// 第一次读取：整个等待过程都在 runtime 上，界面线程只改状态。
    fn start_loading(&mut self, cx: &mut Context<Self>) {
        let Some(channel) = self.channel.clone() else {
            self.phase = Phase::Failed;
            self.status = crate::i18n::t(StatusMsg::DisconnectedRead).to_owned();
            self.disconnected = true;
            cx.notify();
            return;
        };
        let path = self.path.clone();
        let sender = self.sender.clone();
        match runtime() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    let event = match remote::load_remote(&channel, &path).await {
                        Ok(loaded) => RemoteEditEvent::Loaded {
                            opened: loaded.opened,
                            stamp: loaded.stamp,
                        },
                        Err(failure) => RemoteEditEvent::LoadFailed(failure),
                    };
                    let _ = sender.send(event);
                });
            }
            Err(err) => {
                self.phase = Phase::Failed;
                self.status = format!("{}{err}", crate::i18n::t(StatusMsg::ReadFailedPrefix));
            }
        }
        cx.notify();
    }

    /// 取后台事件；有变化就重绘。没有事件时只醒来一次。
    fn poll_events(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut changed = false;
        while let Ok(event) = self.events.try_recv() {
            self.apply(event, window, cx);
            changed = true;
        }
        if changed {
            cx.emit(EditorEvent::Changed);
            cx.notify();
        }
    }

    /// 用户（或程序）改了文字。
    pub(crate) fn touch(&mut self, cx: &mut Context<Self>) {
        self.revision = self.revision.wrapping_add(1);
        cx.notify();
    }

    fn set_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |state, cx| state.set_value(text, window, cx));
        self.touch(cx);
    }

    fn apply(&mut self, event: RemoteEditEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            RemoteEditEvent::Connected(channel) => {
                self.reconnecting = false;
                self.disconnected = false;
                if self.phase == Phase::Connecting || self.phase == Phase::Failed {
                    self.attach_channel(channel, cx);
                } else {
                    // 断线后重连：换通道、换监视；有待重试的保存就再保存一次。
                    self.restart_watcher(&channel);
                    self.channel = Some(channel);
                    self.status = crate::i18n::t(StatusMsg::Reconnected).to_owned();
                    if let Some(kind) = self.retry_save.take() {
                        self.start_save(kind, cx);
                    }
                }
            }
            RemoteEditEvent::ConnectFailed(message) => {
                self.reconnecting = false;
                self.status = format!(
                    "{}{message}",
                    crate::i18n::t(StatusMsg::ConnectFailedPrefix)
                );
                self.disconnected = true;
                if self.phase == Phase::Connecting {
                    self.phase = Phase::Failed;
                }
            }
            RemoteEditEvent::Loaded { opened, stamp } => {
                self.apply_loaded(opened, stamp, window, cx);
            }
            RemoteEditEvent::LoadFailed(failure) => {
                self.phase = Phase::Failed;
                self.disconnected = failure.disconnected;
                self.status = format!(
                    "{}{}",
                    crate::i18n::t(StatusMsg::ReadFailedPrefix),
                    failure.message
                );
            }
            RemoteEditEvent::Saved {
                save_id,
                version,
                size,
                stamp,
                owner_changed,
            } => {
                let Some(saved_revision) = apply_saved_snapshot(
                    &mut self.active_save,
                    save_id,
                    &mut self.baseline,
                    &mut self.dirty_cache,
                ) else {
                    return;
                };
                self.watch.set_stamp(stamp);
                self.watch.set_saving(false);
                self.owner_prompt = None;
                self.conflict = None;
                self.disconnected = false;
                self.retry_save = None;
                self.external_changed = false;
                self.version = Some(version);
                let size_text = byte_count(size as u64);
                self.status = match owner_changed {
                    Some((from, to)) => format!(
                        "{}{size_text}{}{from}{}{to}",
                        crate::i18n::t(StatusMsg::SavedPrefix),
                        crate::i18n::t(StatusMsg::SavedOwnerMid),
                        crate::i18n::t(StatusMsg::SavedOwnerTo)
                    ),
                    None => format!(
                        "{}{size_text}{}",
                        crate::i18n::t(StatusMsg::SavedPrefix),
                        crate::i18n::t(StatusMsg::SavedClose)
                    ),
                };
                self.draft = None;
                // 只删「不晚于这次保存」的草稿：保存期间打字产生的更新草稿留着。
                self.drafts.send(DraftJob::DeleteUpTo(
                    self.draft_path.clone(),
                    saved_revision,
                ));
                self.advance_close_save(cx);
            }
            RemoteEditEvent::Conflict { save_id, remote } => {
                if self.active_save.take_if(|a| a.id == save_id).is_none() {
                    return;
                }
                self.watch.set_saving(false);
                self.status = if remote.gone {
                    crate::i18n::t(StatusMsg::RemoteDeletedRefused).to_owned()
                } else {
                    crate::i18n::t(StatusMsg::RemoteChangedRefused).to_owned()
                };
                self.conflict = Some(ConflictState {
                    remote,
                    later: false,
                });
                let suggested = contract::default_save_as_name(&self.path);
                self.conflict_name
                    .update(cx, |state, cx| state.set_value(suggested, window, cx));
                self.fail_close_save(crate::i18n::t(StatusMsg::ConflictCloseSave));
            }
            RemoteEditEvent::OwnerWouldChange { save_id, from, to } => {
                if self.active_save.take_if(|a| a.id == save_id).is_none() {
                    return;
                }
                self.watch.set_saving(false);
                self.status = format!(
                    "{}{from}{}{to}{}",
                    crate::i18n::t(StatusMsg::OwnerChangePrefix),
                    crate::i18n::t(StatusMsg::OwnerChangeMid),
                    crate::i18n::t(StatusMsg::OwnerChangeSuffix)
                );
                self.fail_close_save(crate::i18n::t(StatusMsg::OwnerChangeCloseSave));
                self.owner_prompt = Some((from, to));
            }
            RemoteEditEvent::SaveFailed {
                save_id,
                message,
                disconnected,
            } => {
                if self.active_save.take_if(|a| a.id == save_id).is_none() {
                    return;
                }
                self.watch.set_saving(false);
                if disconnected {
                    self.disconnected = true;
                    self.status = crate::i18n::t(StatusMsg::DisconnectedKeepEditing).to_owned();
                } else {
                    self.status = message.clone();
                }
                self.fail_close_save(&message);
            }
            RemoteEditEvent::Exported(result) => {
                self.status = match result {
                    Ok(path) => format!(
                        "{}{}{}{}",
                        crate::i18n::t(StatusMsg::DraftExportPrefix),
                        crate::remote_editor::export_plaintext_note(),
                        crate::i18n::t(StatusMsg::DraftExportMid),
                        path.display()
                    ),
                    Err(message) => format!(
                        "{}{message}",
                        crate::i18n::t(StatusMsg::DraftExportFailedPrefix)
                    ),
                };
            }
            RemoteEditEvent::ExternalChange => {
                // 只提醒；编辑内容和版本都不动，保存时仍按哈希决定是否冲突。
                self.external_changed = true;
                self.status = crate::i18n::t(StatusMsg::RemoteChangedConflict).to_owned();
            }
            RemoteEditEvent::SavedAs { path } => {
                self.saving_as = false;
                // 另存为只是复制：`self.path` / `self.version` 保持指向原来那个文件。
                self.status = format!("{}{path}", crate::i18n::t(StatusMsg::SavedAsPrefix));
            }
            RemoteEditEvent::SaveAsFailed(message) => {
                self.saving_as = false;
                self.status = message;
            }
            RemoteEditEvent::Reloaded { revision, loaded } => {
                if self.revision != revision {
                    // 读的时候用户又打了字：不覆盖（0.2.14 的「kept」）。
                    self.status = crate::i18n::t(StatusMsg::ReloadConflictNote).to_owned();
                    return;
                }
                let remote::Loaded { opened, stamp } = *loaded;
                self.format = opened.format;
                self.version = Some(opened.version);
                self.watch.set_stamp(stamp);
                self.baseline.clone_from(&opened.text);
                self.dirty_cache = None;
                self.conflict = None;
                self.owner_prompt = None;
                self.draft = None;
                self.external_changed = false;
                self.discarded_text = None;
                self.drafts.send(DraftJob::Delete(self.draft_path.clone()));
                self.status = format!(
                    "{}{}{}",
                    crate::i18n::t(StatusMsg::DiscardReloadPrefix),
                    byte_count(opened.size as u64),
                    crate::i18n::t(StatusMsg::DiscardReloadClose)
                );
                self.set_text(opened.text, window, cx);
            }
            RemoteEditEvent::ReloadFailed(message) => {
                self.status = format!("{}{message}", crate::i18n::t(StatusMsg::ReloadFailedPrefix));
            }
            RemoteEditEvent::Relocated {
                path,
                version,
                stamp,
                text,
            } => {
                // 旧路径的草稿不再属于这个编辑器。
                self.drafts.send(DraftJob::Delete(self.draft_path.clone()));
                self.path = path;
                self.open_file.relocate(&self.path);
                self.draft_path = draft_file_in(&self.data_dir, self.host_id, &self.path);
                self.version = Some(version);
                self.baseline = text;
                self.dirty_cache = None;
                self.conflict = None;
                self.owner_prompt = None;
                self.draft = None;
                self.external_changed = false;
                self.lang =
                    language::detect(&self.path, self.baseline.lines().next().unwrap_or(""));
                let token = self.lang.syntax_token();
                self.editor
                    .update(cx, |state, cx| state.set_highlighter(token, cx));
                if let Some(channel) = self.channel.clone() {
                    self.restart_watcher(&channel);
                    self.watch.set_stamp(stamp);
                }
                self.status = format!(
                    "{}{}{}",
                    crate::i18n::t(StatusMsg::SavedAsPrefix),
                    self.path,
                    crate::i18n::t(StatusMsg::SavedAsEditingMid)
                );
                self.advance_close_save(cx);
            }
            RemoteEditEvent::RelocateFailed(message) => {
                self.status = message;
            }
        }
    }

    fn apply_loaded(
        &mut self,
        opened: Opened,
        stamp: Option<RemoteStamp>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.want_focus = true;
        self.phase = Phase::Ready;
        self.disconnected = false;
        self.format = opened.format;
        self.version = Some(opened.version);
        self.watch.set_stamp(stamp);
        self.status = format!(
            "{}{}{}",
            crate::i18n::t(StatusMsg::LoadedPrefix),
            byte_count(opened.size as u64),
            crate::i18n::t(StatusMsg::LoadedClose)
        );
        self.baseline.clone_from(&opened.text);
        self.dirty_cache = None;
        // 文件名认不出语言时，第一行的 shebang 再补一刀。
        let first_line = opened.text.lines().next().unwrap_or("");
        self.lang = language::detect(&self.path, first_line);
        let token = self.lang.syntax_token();
        self.editor
            .update(cx, |state, cx| state.set_highlighter(token, cx));
        self.set_text(opened.text, window, cx);
        // 草稿要等加载完成才查：`base_version` 比的就是这次读到的版本。
        let own_draft = match self.draft_account.as_deref() {
            Some(account) => match read_draft_with(
                &DbDraftCipher::new(account),
                &self.draft_path,
                self.host_id,
                &self.path,
            ) {
                Ok(draft) => draft,
                // 解不开（别的 Windows 用户 / 别的电脑保存的）：文件挪到一边留着，不然下一次自动保存会把它盖掉。
                Err(locked) => {
                    let kept = match set_aside_locked(&locked) {
                        Ok(path) => path
                            .file_name()
                            .map(|name| {
                                format!(
                                    "{}{}",
                                    crate::i18n::t(StatusMsg::KeptAsPrefix),
                                    name.to_string_lossy()
                                )
                            })
                            .unwrap_or_default(),
                        Err(err) => format!(
                            "{}{err}{}",
                            crate::i18n::t(StatusMsg::MoveAsideFailedPrefix),
                            crate::i18n::t(StatusMsg::MoveAsideFailedSuffix)
                        ),
                    };
                    self.status = format!("{locked}{kept}");
                    None
                }
            },
            None => None,
        };
        self.draft = draft_notice(own_draft, opened.version).or_else(|| {
            let cipher = DbDraftCipher::new(self.draft_account.as_deref()?);
            // 认不出主人的旧格式草稿：只提示、不恢复、不删。
            read_legacy_draft_with(
                &cipher,
                &legacy_draft_file_in(&self.data_dir, &self.path),
                &self.path,
            )
            .map(DraftNotice::Legacy)
        });
        // 自动化：草稿条出现后按环境变量自动点一次，没草稿时什么也不做。
        if let Some(action) = self.automation_draft_action.take() {
            self.apply_draft_action(&action, window, cx);
        }
        // 自动化：换成指定文本但不保存，等下一轮自动保存写出草稿。
        if let Some(text) = self.automation_draft.take() {
            self.set_text(text, window, cx);
        }
        // 自动化只在文件读成功之后触发一次：先换成指定文本，再走正常保存链路。
        if let Some(text) = self.automation_save.take() {
            self.set_text(text, window, cx);
            self.save(cx);
        }
        // 先保存再另存为：两个都设置了就按这个顺序各跑一次。
        if let Some(target) = self.automation_save_as.take() {
            self.save_as_input
                .update(cx, |state, cx| state.set_value(target, window, cx));
            self.save_as(cx);
        }
    }

    // ---- 保存 ----

    /// 保存按钮和 Ctrl+S 共用：属主保不住就不替换，由 [`Self::owner_prompt`] 问用户。
    pub(crate) fn save(&mut self, cx: &mut Context<Self>) {
        self.start_save(SaveKind::Checked(OwnerMode::Require), cx);
    }

    /// 属主提示条上的「仍然保存」：用户同意属主变化，再走一遍同样的保存（含版本核对）。
    pub(crate) fn save_allowing_owner_change(&mut self, cx: &mut Context<Self>) {
        self.owner_prompt = None;
        self.start_save(SaveKind::Checked(OwnerMode::Allow), cx);
    }

    /// 属主提示条上的「直接写入原文件」：不换文件、截断后原地重写，属主和权限都不变，
    /// 代价是中途断线可能留下半截文件（提示条上写明了）。
    pub(crate) fn save_in_place(&mut self, cx: &mut Context<Self>) {
        self.owner_prompt = None;
        self.start_save(SaveKind::Checked(OwnerMode::InPlace), cx);
    }

    /// 冲突提示条上的「覆盖远端」。
    pub(crate) fn overwrite_remote(&mut self, cx: &mut Context<Self>) {
        self.conflict = None;
        self.start_save(SaveKind::Overwrite(OwnerMode::Require), cx);
    }

    /// 先把当前文本抓出来，剩下的等待交给 runtime。
    fn start_save(&mut self, kind: SaveKind, cx: &mut Context<Self>) {
        if self.active_save.is_some() {
            return;
        }
        if self.phase != Phase::Ready {
            self.status = match self.phase {
                Phase::Connecting | Phase::Loading => {
                    crate::i18n::t(StatusMsg::ReadingWaitSave).to_owned()
                }
                _ => crate::i18n::t(StatusMsg::ReadFailedNoSave).to_owned(),
            };
            cx.notify();
            return;
        }
        let Some(channel) = self.channel.clone() else {
            self.disconnected = true;
            self.retry_save = Some(kind);
            self.status = crate::i18n::t(StatusMsg::DisconnectedKeepEditing).to_owned();
            cx.notify();
            return;
        };
        let Some(expected) = self.version else {
            self.status = crate::i18n::t(StatusMsg::NoRemoteVersion).to_owned();
            cx.notify();
            return;
        };
        let text = self.editor.read(cx).value().to_string();
        let bytes = contract::encode(&text, &self.format);
        if bytes.len() > MAX_BYTES {
            self.status = format!(
                "{}{}{}{}{}",
                crate::i18n::t(StatusMsg::TooLargePrefix),
                byte_count(bytes.len() as u64),
                crate::i18n::t(StatusMsg::TooLargeMid),
                byte_count(MAX_BYTES as u64),
                crate::i18n::t(StatusMsg::TooLargeSaveSuffix)
            );
            cx.notify();
            return;
        }
        let id = self.next_save_id;
        self.next_save_id += 1;
        self.active_save = Some(ActiveSave {
            id,
            revision: self.revision,
            text,
        });
        self.watch.set_saving(true);
        self.status = crate::i18n::t(StatusMsg::Saving).to_owned();
        self.owner_prompt = None;
        self.retry_save = Some(kind);
        let expected = match kind {
            SaveKind::Checked(_) => Some(expected),
            SaveKind::Overwrite(_) => None,
        };
        let path = self.path.clone();
        let sender = self.sender.clone();
        // 调试钩子：让保存慢一点，窗口级场景才好验「保存途中继续输入」（release 构建里读不到）。
        let delay = crate::dev_hooks::var("ZSSH_EDIT_SAVE_DELAY_MS")
            .and_then(|raw| raw.parse::<u64>().ok())
            .map(Duration::from_millis);
        match runtime() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    if let Some(delay) = delay {
                        tokio::time::sleep(delay).await;
                    }
                    let event =
                        remote::save_event(&channel, &path, &bytes, expected, kind.owner(), id)
                            .await;
                    let _ = sender.send(event);
                });
            }
            Err(err) => {
                self.active_save = None;
                self.watch.set_saving(false);
                self.status = format!("{}{err}", crate::i18n::t(StatusMsg::SaveFailedPrefix));
            }
        }
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    /// 「保存并关闭」途中保存没成功：把原因交给宿主的确认框。
    fn fail_close_save(&mut self, reason: &str) {
        CloseSave::fail(&mut self.close_save, reason);
    }

    // ---- 冲突 ----

    /// 冲突条上的「稍后处理」：收成一行，编辑内容留着；再按保存会重新比一次。
    pub(crate) fn defer_conflict(&mut self, cx: &mut Context<Self>) {
        if let Some(conflict) = &mut self.conflict {
            conflict.later = true;
            self.status = crate::i18n::t(StatusMsg::ConflictParked).to_owned();
            cx.notify();
        }
    }

    pub(crate) fn reopen_conflict(&mut self, cx: &mut Context<Self>) {
        if let Some(conflict) = &mut self.conflict {
            conflict.later = false;
            cx.notify();
        }
    }

    /// 冲突条上的「放弃我的修改」：读回远端现在的内容，换掉编辑器里的文字。
    pub(crate) fn discard_my_changes(&mut self, cx: &mut Context<Self>) {
        let Some(channel) = self.channel.clone() else {
            self.disconnected = true;
            self.status = crate::i18n::t(StatusMsg::DisconnectedCannotReadRemote).to_owned();
            cx.notify();
            return;
        };
        if self.active_save.is_some() {
            return;
        }
        let revision = self.revision;
        let path = self.path.clone();
        let sender = self.sender.clone();
        self.status = crate::i18n::t(StatusMsg::ReadingRemoteNow).to_owned();
        match runtime() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    let event = match remote::load_remote(&channel, &path).await {
                        Ok(loaded) => RemoteEditEvent::Reloaded {
                            revision,
                            loaded: Box::new(loaded),
                        },
                        Err(failure) => RemoteEditEvent::ReloadFailed(failure.message),
                    };
                    let _ = sender.send(event);
                });
            }
            Err(err) => {
                self.status = format!("{}{err}", crate::i18n::t(StatusMsg::ReadFailedPrefix))
            }
        }
        cx.notify();
    }

    /// 冲突条上的「另存为」：同目录新名字，成功后编辑器改跟踪新文件。
    pub(crate) fn save_conflict_as(&mut self, cx: &mut Context<Self>) {
        let Some(channel) = self.channel.clone() else {
            self.disconnected = true;
            self.status = crate::i18n::t(StatusMsg::DisconnectedNoSaveAs).to_owned();
            cx.notify();
            return;
        };
        if self.active_save.is_some() || self.phase != Phase::Ready {
            return;
        }
        let name = self.conflict_name.read(cx).value().to_string();
        let text = self.editor.read(cx).value().to_string();
        let bytes = contract::encode(&text, &self.format);
        let original = self.path.clone();
        let sender = self.sender.clone();
        self.status = crate::i18n::t(StatusMsg::SavingAs).to_owned();
        match runtime() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    let event = match remote::save_beside(&*channel, &original, &name, &bytes).await
                    {
                        Ok(path) => RemoteEditEvent::Relocated {
                            stamp: remote::read_stamp(&channel, &path).await.ok(),
                            path,
                            version: contract::version_of(&bytes),
                            text,
                        },
                        Err(message) => RemoteEditEvent::RelocateFailed(message),
                    };
                    let _ = sender.send(event);
                });
            }
            Err(err) => {
                self.status = format!("{}{err}", crate::i18n::t(StatusMsg::SaveAsFailedPrefix))
            }
        }
        cx.notify();
    }

    // ---- 另存为（副本）----

    /// 另存为按钮和 `ZSSH_EDIT_SAVE_AS` 共用：目标路径先解析成绝对路径，再交给 runtime。
    pub(crate) fn save_as(&mut self, cx: &mut Context<Self>) {
        if self.saving_as {
            return;
        }
        if self.phase != Phase::Ready {
            self.status = match self.phase {
                Phase::Connecting | Phase::Loading => {
                    crate::i18n::t(StatusMsg::ReadingWaitSaveAs).to_owned()
                }
                _ => crate::i18n::t(StatusMsg::ReadFailedNoSaveAs).to_owned(),
            };
            cx.notify();
            return;
        }
        let Some(channel) = self.channel.clone() else {
            self.disconnected = true;
            self.status = crate::i18n::t(StatusMsg::DisconnectedNoSaveAs).to_owned();
            cx.notify();
            return;
        };
        let typed = self.save_as_input.read(cx).value().to_string();
        let target = match remote::resolve_save_as(&self.path, &typed) {
            Ok(target) => target,
            Err(message) => {
                self.status = message;
                cx.notify();
                return;
            }
        };
        let text = self.editor.read(cx).value().to_string();
        let bytes = contract::encode(&text, &self.format);
        if bytes.len() > MAX_BYTES {
            self.status = format!(
                "{}{}{}{}{}",
                crate::i18n::t(StatusMsg::TooLargePrefix),
                byte_count(bytes.len() as u64),
                crate::i18n::t(StatusMsg::TooLargeMid),
                byte_count(MAX_BYTES as u64),
                crate::i18n::t(StatusMsg::TooLargeSaveAsSuffix)
            );
            cx.notify();
            return;
        }

        self.saving_as = true;
        self.status = format!(
            "{}{target}…",
            crate::i18n::t(StatusMsg::SavingAsTargetPrefix)
        );
        let sender = self.sender.clone();
        match runtime() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    let event = remote::save_as_remote(&channel, &target, &bytes).await;
                    let _ = sender.send(event);
                });
            }
            Err(err) => {
                self.saving_as = false;
                self.status = format!("{}{err}", crate::i18n::t(StatusMsg::SaveAsFailedPrefix));
            }
        }
        cx.notify();
    }

    // ---- 草稿 ----

    /// 2 秒一次的草稿自动保存：只在就绪、并且文本和基线不同时把任务交给写线程。
    /// 关闭前 `force` 为真：不管用户之前点没点过「丢弃」，现在的内容都要落一份。
    ///
    /// 空文本是合法的未保存改动（用户清空了文件），照写不误。
    pub(crate) fn autosave_draft(&mut self, force: bool, cx: &mut Context<Self>) {
        if self.phase != Phase::Ready {
            return;
        }
        let Some(version) = self.version else {
            return;
        };
        let text = self.editor.read(cx).value().to_string();
        // 用户刚点过「丢弃」：这段文字不再自动写回草稿，等他再改一次。
        if !force && self.discarded_text.as_deref() == Some(text.as_str()) {
            return;
        }
        self.discarded_text = None;
        if !needs_draft(&text, &self.baseline, self.active_save.as_ref()) {
            // 没有未保存的改动。会被我们写进同一个文件的旧草稿还在提示条里时不能删——
            // 用户可能正要恢复它。
            let guards_same_file = matches!(
                self.draft,
                Some(DraftNotice::Restorable(_) | DraftNotice::Stale(_))
            );
            if !guards_same_file {
                self.drafts.send(DraftJob::Delete(self.draft_path.clone()));
            }
            return;
        }
        // 和保存同一个上限：超了既写不进草稿，保存时也会报错。
        if text.len() > MAX_BYTES {
            return;
        }
        // 提示条里那份旧草稿和马上要写的新草稿是同一个文件：先把旧的另存出来，
        // 不能因为用户接着打了几个字就把它悄悄覆盖掉。
        self.preserve_pending_notice();
        let draft = EditDraft {
            path: self.path.clone(),
            host_id: Some(self.host_id),
            base_version: version.0,
            text,
            saved_at_unix: now_unix(),
            revision: self.revision,
        };
        // 这里只入队：序列化、临时文件和 `sync_all` 都在写线程上做。
        self.drafts
            .send(DraftJob::Save(self.draft_path.clone(), draft));
    }

    /// 提示条里的旧草稿（可恢复的、或基于旧版本的）和新草稿共用一个文件：第一次要覆盖它之前
    /// 先导出成本地文件。导出结果走 `Exported` 事件告诉用户在哪。
    fn preserve_pending_notice(&mut self) {
        if self.draft_preserved {
            return;
        }
        let Some(DraftNotice::Restorable(draft) | DraftNotice::Stale(draft)) = &self.draft else {
            return;
        };
        self.draft_preserved = true;
        self.spawn_export(draft.clone());
    }

    /// 在 runtime 的阻塞线程上把一份草稿存成本地文件；界面线程不碰磁盘。
    fn spawn_export(&self, draft: EditDraft) {
        let sender = self.sender.clone();
        let base = self.data_dir.clone();
        match runtime() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    let result =
                        tokio::task::spawn_blocking(move || export_recovered(&base, &draft))
                            .await
                            .map_err(|err| err.to_string())
                            .and_then(|inner| inner.map_err(|err| err.to_string()));
                    let _ = sender.send(RemoteEditEvent::Exported(result));
                });
            }
            Err(err) => {
                let _ = sender.send(RemoteEditEvent::Exported(Err(err)));
            }
        }
    }

    /// 提示条上的「另存为本地文件」。
    pub(crate) fn export_notice(&mut self, cx: &mut Context<Self>) {
        let Some(notice) = &self.draft else {
            return;
        };
        self.spawn_export(notice.draft().clone());
        self.status = crate::i18n::t(StatusMsg::DraftSavingLocal).to_owned();
        cx.notify();
    }

    /// 提示条上的「复制到剪贴板」。
    pub(crate) fn copy_notice(&mut self, cx: &mut Context<Self>) {
        let Some(notice) = &self.draft else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(notice.draft().text.clone()));
        self.status = crate::i18n::t(StatusMsg::DraftCopied).to_owned();
        cx.notify();
    }

    /// 旧格式草稿的「关闭提示」：只收起提示条，文件原样留着。
    pub(crate) fn dismiss_legacy_notice(&mut self, cx: &mut Context<Self>) {
        if matches!(self.draft, Some(DraftNotice::Legacy(_))) {
            self.draft = None;
            self.status = crate::i18n::t(StatusMsg::OldDraftKept).to_owned();
            cx.notify();
        }
    }

    /// `ZSSH_EDIT_DRAFT_ACTION`：草稿条出现后自动点一次，服务器测试用。
    fn apply_draft_action(&mut self, action: &str, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            "restore" => self.restore_draft(window, cx),
            "discard" => self.discard_draft(cx),
            _ => {}
        }
    }

    /// 恢复草稿：只替换编辑器文字，不写远端；草稿文件留着，下一轮自动保存会刷新它。
    pub(crate) fn restore_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.draft, Some(DraftNotice::Restorable(_))) {
            return;
        }
        let Some(DraftNotice::Restorable(draft)) = self.draft.take() else {
            return;
        };
        // 用户自己决定恢复：这份草稿就是此刻的编辑内容，不用再单独另存。
        self.draft_preserved = true;
        let label = draft_age_label(draft.saved_at_unix, now_unix());
        self.set_text(draft.text, window, cx);
        self.status = format!(
            "{}{label}{}",
            crate::i18n::t(StatusMsg::DraftRestoredPrefix),
            crate::i18n::t(StatusMsg::DraftRestoredSuffix)
        );
        cx.notify();
    }

    /// 丢弃草稿：删文件，编辑器文字不动（加载完显示的是远端内容）。
    ///
    /// 记下当前文本，自动保存不会把这份被丢弃的内容又写回来。
    pub(crate) fn discard_draft(&mut self, cx: &mut Context<Self>) {
        // 旧格式草稿不在这里删：认不出主人，只能收起提示（见 `dismiss_legacy_notice`）。
        if matches!(self.draft, Some(DraftNotice::Legacy(_))) {
            self.dismiss_legacy_notice(cx);
            return;
        }
        if self.draft.take().is_none() {
            return;
        }
        self.drafts.send(DraftJob::Delete(self.draft_path.clone()));
        self.discarded_text = Some(self.editor.read(cx).value().to_string());
        self.status = crate::i18n::t(StatusMsg::DraftDiscarded).to_owned();
        cx.notify();
    }

    // ---- 关闭 ----

    /// 宿主要关这个标签了，而且用户选了「不保存」：草稿也一起删（同 0.2.14 的 `closeDoc(discard)`）。
    pub(crate) fn discard_unsaved_state(&mut self) {
        self.drafts.send(DraftJob::Delete(self.draft_path.clone()));
        self.dirty_cache = Some((self.revision, false));
    }

    /// 退出 / 关窗口前先把当前未保存的内容落一份草稿，并等写线程处理完（最多几秒）。
    /// 这样即使接下来进程被直接结束，下次启动也找得回来。
    pub(crate) fn persist_draft_now(&mut self, cx: &mut Context<Self>) {
        self.autosave_draft(true, cx);
        self.drafts.flush(QUIT_FLUSH_WAIT);
    }

    /// 「保存并关闭」：宿主的确认框选了保存。保存完成与否从 [`Self::close_save_state`] 看。
    pub(crate) fn begin_close_save(&mut self, cx: &mut Context<Self>) {
        self.close_save = Some(CloseSave::Saving);
        self.advance_close_save(cx);
    }

    /// 每轮成功后都以新基线核对当前内容，直到最新输入也已落到远端。
    fn advance_close_save(&mut self, cx: &mut Context<Self>) {
        if self.close_save != Some(CloseSave::Saving) {
            return;
        }
        if self.phase != Phase::Ready {
            self.save_for_close(cx);
            return;
        }
        let unsaved = self.has_unsaved(cx);
        match CloseSave::advance(&mut self.close_save, self.active_save.is_some(), unsaved) {
            CloseSaveStep::Save => self.save_for_close(cx),
            CloseSaveStep::Done if self.standalone => {
                self.close_confirmed = true;
                cx.emit(EditorEvent::CloseNow);
            }
            _ => {}
        }
    }

    fn save_for_close(&mut self, cx: &mut Context<Self>) {
        self.save(cx);
        if self.active_save.is_none() {
            // 续存也可能被大小上限、缺少通道或 runtime 错误拒绝，不能一直等。
            let reason = self.status.clone();
            self.fail_close_save(&reason);
        }
    }

    pub(crate) fn clear_close_save(&mut self) {
        self.close_save = None;
    }

    /// 标签 / 窗口关掉：后台任务的结果不再有人接，通道的最后一个引用在这里丢掉。
    ///
    /// 草稿故意不删（要不要删由调用方先发 [`Self::discard_unsaved_state`] 决定）：写线程丢掉发送端后，
    /// 会把已经排队的任务处理完再退出；这里不 join，界面不等磁盘。
    pub(crate) fn shutdown(&mut self) {
        self.watch.stop();
        self.watch.set_saving(false);
        self.drafts.close();
        self.channel = None;
        self.version = None;
        self.active_save = None;
        self.close_save = None;
        self.unsaved_registration.set(0);
        self.automation_save = None;
        self.automation_save_as = None;
        self.automation_draft = None;
        self.automation_draft_action = None;
    }

    // ---- 独立窗口（debug 入口）的关闭 ----

    /// 窗口关闭按钮：没有未保存修改直接关；有就先把当前内容落草稿，再显示确认条。
    /// 返回 `true` 表示这次可以关。
    fn should_close(&mut self, cx: &mut Context<Self>) -> bool {
        let unsaved = self.has_unsaved(cx);
        if close_decision(self.close_confirmed, unsaved) == CloseDecision::Close {
            self.shutdown();
            return true;
        }
        self.autosave_draft(true, cx);
        self.close_prompt = true;
        self.status = crate::i18n::t(StatusMsg::UnsavedKeptAsDraft).to_owned();
        cx.notify();
        false
    }

    /// 确认条上的「保存并关闭」：保存成功才关（`Saved` 事件里处理）。
    pub(crate) fn save_then_close(&mut self, cx: &mut Context<Self>) {
        self.close_prompt = false;
        self.begin_close_save(cx);
        cx.notify();
    }

    /// 确认条上的「不保存」：直接关；草稿一并删掉。
    pub(crate) fn close_without_saving(&mut self, cx: &mut Context<Self>) {
        self.close_prompt = false;
        self.close_confirmed = true;
        self.discard_unsaved_state();
        cx.emit(EditorEvent::CloseNow);
    }

    // ---- 节拍里的小活 ----

    /// 把「有没有未保存修改」同步到进程级登记（退出 / 安装前检查）。
    fn refresh_registration(&mut self, cx: &App) {
        let unsaved = usize::from(self.has_unsaved(cx));
        self.unsaved_registration.set(unsaved);
    }

    /// 光标位置和选中字数：状态栏用。选中很多时不数字符（数 5 MiB 的字符每个节拍太贵），按字节报。
    fn refresh_cursor(&mut self, cx: &mut Context<Self>) {
        let (position, selected) = {
            let state = self.editor.read(cx);
            let range = state.selected_range();
            let chars = if range.len() > 200_000 {
                range.len()
            } else {
                state.selected_value().chars().count()
            };
            (state.cursor_position(), chars)
        };
        let info = CursorInfo {
            line: position.line as usize + 1,
            column: position.character as usize + 1,
            selected,
        };
        if info != self.cursor {
            self.cursor = info;
            cx.notify();
        }
    }

    /// 括号配对 / 选中内容其他出现处的高亮：只在光标、选区或内容变了才重算。
    fn refresh_marks(&mut self, cx: &mut Context<Self>) {
        if self.phase != Phase::Ready {
            return;
        }
        let (cursor, selection) = {
            let state = self.editor.read(cx);
            (state.cursor(), state.selected_range())
        };
        let key = (cursor, selection.clone(), self.revision);
        if self.marks_key.as_ref() == Some(&key) {
            return;
        }
        self.marks_key = Some(key);
        let text = self.editor.read(cx).value().to_string();
        let tokens = crate::app_theme::Tokens::current(cx);
        let mut bracket_color = tokens.accent;
        bracket_color.a = 0.35;
        let mut match_color = tokens.accent;
        match_color.a = 0.18;
        let bracket_marks: Vec<RangeDecoration> = brackets::matching_pair(&text, cursor)
            .into_iter()
            .flat_map(|(a, b)| [a, b])
            .map(|range| {
                RangeDecoration::new(range)
                    .with_style(RangeDecorationStyle::Fill)
                    .with_color(bracket_color)
            })
            .collect();
        let match_marks: Vec<RangeDecoration> = brackets::selection_matches(&text, &selection)
            .into_iter()
            .map(|range| {
                RangeDecoration::new(range)
                    .with_style(RangeDecorationStyle::Fill)
                    .with_color(match_color)
            })
            .collect();
        let editor = self.editor.clone();
        if self.bracket_marks.is_none() {
            self.bracket_marks = Some(editor.update(cx, |state, cx| {
                state.create_range_decorations_collection(Vec::new(), cx)
            }));
        }
        if self.match_marks.is_none() {
            self.match_marks = Some(editor.update(cx, |state, cx| {
                state.create_range_decorations_collection(Vec::new(), cx)
            }));
        }
        if let Some(collection) = &self.bracket_marks {
            collection.set(bracket_marks, cx);
        }
        if let Some(collection) = &self.match_marks {
            collection.set(match_marks, cx);
        }
    }

    // ---- 编辑命令（快捷键 / 按钮共用）----

    /// 编辑器当前的文本和选区（字节偏移）。
    fn text_and_selection(&self, cx: &App) -> (String, std::ops::Range<usize>) {
        let state = self.editor.read(cx);
        (state.value().to_string(), state.selected_range())
    }

    /// 把一次整行编辑套到编辑器上：先选中要换掉的范围、替换（一步撤销）、再设新选区。
    fn apply_edit(&mut self, edit: lines::Edit, window: &mut Window, cx: &mut Context<Self>) {
        if self.phase != Phase::Ready {
            return;
        }
        self.editor.update(cx, |state, cx| {
            state.set_selected_range(edit.range.clone(), cx);
            state.replace(edit.insert.clone(), window, cx);
            state.set_selected_range(edit.select.clone(), cx);
        });
        // 程序化替换不发 Change 事件：修订号自己加。
        self.touch(cx);
    }

    pub(crate) fn toggle_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (text, selection) = self.text_and_selection(cx);
        if let Some(edit) = lines::toggle_comment(&text, &selection, self.lang.comment()) {
            self.apply_edit(edit, window, cx);
        }
    }

    pub(crate) fn move_lines(&mut self, up: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (text, selection) = self.text_and_selection(cx);
        if let Some(edit) = lines::move_lines(&text, &selection, up) {
            self.apply_edit(edit, window, cx);
        }
    }

    pub(crate) fn copy_lines(&mut self, up: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (text, selection) = self.text_and_selection(cx);
        let edit = lines::copy_lines(&text, &selection, up);
        self.apply_edit(edit, window, cx);
    }

    pub(crate) fn delete_lines(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (text, selection) = self.text_and_selection(cx);
        if let Some(edit) = lines::delete_lines(&text, &selection) {
            self.apply_edit(edit, window, cx);
        }
    }

    pub(crate) fn select_line(&mut self, cx: &mut Context<Self>) {
        let (text, selection) = self.text_and_selection(cx);
        let range = lines::select_line(&text, &selection);
        self.editor
            .update(cx, |state, cx| state.set_selected_range(range, cx));
    }

    pub(crate) fn insert_line_below(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (text, selection) = self.text_and_selection(cx);
        let edit = lines::insert_blank_line_below(&text, &selection);
        self.apply_edit(edit, window, cx);
    }

    /// Ctrl+Shift+\：跳到配对的括号（光标挨着括号时）。
    pub(crate) fn jump_to_bracket(&mut self, cx: &mut Context<Self>) {
        let (text, selection) = self.text_and_selection(cx);
        let cursor = selection.end;
        if let Some((open, close)) = brackets::matching_pair(&text, cursor) {
            // 光标在哪个括号旁边就去另一个的里面一侧。
            let target = if cursor == open.start || cursor == open.end {
                close.end
            } else {
                open.end
            };
            self.editor
                .update(cx, |state, cx| state.set_selected_range(target..target, cx));
        }
    }

    /// Ctrl+D：选下一个相同内容。没有选区先选光标下的词；有选区就把下一处也加进选区。
    pub(crate) fn select_next_occurrence(&mut self, cx: &mut Context<Self>) {
        let (text, selection) = self.text_and_selection(cx);
        if selection.is_empty() {
            if let Some(word) = word_at(&text, selection.start) {
                self.editor
                    .update(cx, |state, cx| state.set_selected_range(word, cx));
            }
            return;
        }
        let needle = &text[selection.clone()];
        if needle.is_empty() || needle.contains('\n') {
            return;
        }
        // 从当前选区之后找下一处，找不到就绕回开头（但不能是自己）。
        let found = text[selection.end..]
            .find(needle)
            .map(|at| selection.end + at)
            .or_else(|| text[..selection.start].find(needle));
        if let Some(at) = found {
            let range = at..at + needle.len();
            self.editor
                .update(cx, |state, cx| state.add_selection(range, cx));
        }
    }

    /// F3 / Shift+F3：查找下一个 / 上一个。查找条没开就先打开它。
    pub(crate) fn find_step(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.editor.read(cx).search_session().is_active();
        if !active {
            self.open_search(false, window, cx);
            return;
        }
        let range = self.editor.update(cx, |state, cx| {
            if forward {
                state.next_search_match(cx)
            } else {
                state.previous_search_match(cx)
            }
        });
        if let Some(range) = range {
            self.editor
                .update(cx, |state, cx| state.set_selected_range(range, cx));
        }
    }

    /// 工具栏「查找」「替换」按钮：面板已经以同一模式开着就关掉，否则打开（或切到）这个模式。
    /// 快捷键仍走 [`Self::open_search`]，再按一次是重新选中查找框，和常见编辑器一致。
    pub(crate) fn toggle_search(
        &mut self,
        replace: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session = self.editor.read(cx).search_session();
        if search_button_closes(session.open, session.replace_mode, replace) {
            self.editor.update(cx, |state, cx| state.close_search(cx));
            self.focus_editor(window, cx);
            cx.notify();
        } else {
            self.open_search(replace, window, cx);
        }
    }

    /// 工具栏按钮按下态：面板开着且正是这个模式。
    pub(crate) fn search_button_active(&self, replace: bool, cx: &App) -> bool {
        let session = self.editor.read(cx).search_session();
        session.open && session.replace_mode == replace
    }

    /// 打开编辑器内置的查找/替换面板，并让编辑器重新拿到焦点。
    pub(crate) fn open_search(
        &mut self,
        replace: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor
            .update(cx, |state, cx| state.open_search(replace, cx));
        let focus = self.editor.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
    }

    /// 自动换行开关。
    pub(crate) fn toggle_wrap(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.wrap = !self.wrap;
        let wrap = self.wrap;
        self.editor
            .update(cx, |state, cx| state.set_soft_wrap(wrap, window, cx));
        cx.notify();
    }

    /// Ctrl+G / 跳行按钮：打开（或关掉）跳行条。
    pub(crate) fn toggle_goto(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.goto_open = !self.goto_open;
        if self.goto_open {
            let focus = self.goto_input.read(cx).focus_handle(cx);
            self.goto_input
                .update(cx, |state, cx| state.set_value("", window, cx));
            window.focus(&focus, cx);
        } else {
            self.focus_editor(window, cx);
        }
        cx.notify();
    }

    pub(crate) fn close_goto(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.goto_open {
            self.goto_open = false;
            self.focus_editor(window, cx);
            cx.notify();
        }
    }

    /// 跳行条里按了回车：`行` 或 `行:列`，超出范围夹到最后一行 / 行尾。
    fn submit_goto(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.goto_input.read(cx).value().to_string();
        let total = self.editor.read(cx).text().lines_len();
        match parse_goto(&typed, total) {
            Some((line, column)) => {
                self.editor.update(cx, |state, cx| {
                    state.set_cursor_position(
                        Position {
                            line: line as u32,
                            character: column as u32,
                        },
                        window,
                        cx,
                    );
                });
                self.goto_open = false;
                self.status = format!(
                    "{}{}{}",
                    crate::i18n::t(StatusMsg::GotoLinePrefix),
                    line + 1,
                    crate::i18n::t(StatusMsg::GotoLineSuffix)
                );
                self.focus_editor(window, cx);
            }
            None => self.status = crate::i18n::t(StatusMsg::GotoInvalid).to_owned(),
        }
        cx.notify();
    }

    fn focus_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self.editor.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
    }

    // ---- 驱动脚本 / 测试用的读写口 ----

    /// 把编辑器内容整个换掉并算一次用户改动（驱动脚本用；和用户打字一样算未保存）。
    #[cfg(debug_assertions)]
    pub(crate) fn script_set_text(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |state, cx| {
            let len = state.value().len();
            state.set_selected_range(0..len, cx);
            state.replace(text.to_owned(), window, cx);
        });
        self.touch(cx);
    }

    /// 驱动脚本用：在光标处插入文字。
    #[cfg(debug_assertions)]
    pub(crate) fn script_insert(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor
            .update(cx, |state, cx| state.insert(text.to_owned(), window, cx));
        self.touch(cx);
    }

    #[cfg(debug_assertions)]
    pub(crate) fn script_select(&mut self, range: std::ops::Range<usize>, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |state, cx| state.set_selected_range(range, cx));
    }

    /// 驱动脚本用：先把状态栏数据和括号高亮刷新一次，再读。
    #[cfg(debug_assertions)]
    pub(crate) fn script_refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_cursor(cx);
        self.refresh_marks(cx);
    }

    /// 驱动脚本用：走跳行条的同一条路（打开条 → 填 → 回车）。
    #[cfg(debug_assertions)]
    pub(crate) fn script_goto(&mut self, typed: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.goto_open = true;
        self.goto_input.update(cx, |state, cx| {
            state.set_value(typed.to_owned(), window, cx)
        });
        self.submit_goto(window, cx);
    }

    /// 驱动脚本用：往冲突条的「另存为」名字框里填字。
    #[cfg(debug_assertions)]
    pub(crate) fn script_conflict_name(
        &mut self,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.conflict_name
            .update(cx, |state, cx| state.set_value(name.to_owned(), window, cx));
    }

    /// 属主提示条上的「取消」：什么都不改，这次修改仍然留在编辑器里算没保存。
    #[cfg(debug_assertions)]
    pub(crate) fn cancel_owner_prompt(&mut self, cx: &mut Context<Self>) {
        self.owner_prompt = None;
        cx.notify();
    }

    /// 跳行条开着没有（驱动脚本检查 Ctrl+G / Esc 用）。
    #[cfg(debug_assertions)]
    pub(crate) fn goto_visible(&self) -> bool {
        self.goto_open
    }

    /// 现在编辑器处在什么状态（驱动脚本和测试用，一个名字说清楚）。
    #[cfg(debug_assertions)]
    pub(crate) fn state_name(&self) -> &'static str {
        if self.active_save.is_some() {
            "saving"
        } else if let Some(conflict) = &self.conflict {
            if conflict.later {
                "conflict_later"
            } else {
                "conflict"
            }
        } else if self.owner_prompt.is_some() {
            "owner"
        } else if self.disconnected {
            "disconnected"
        } else {
            match self.draft_bar_kind() {
                Some("restorable") => "draft_restorable",
                Some("stale") => "draft_stale",
                Some(_) => "draft_legacy",
                None => "none",
            }
        }
    }
}

/// 光标所在的词（字母、数字、下划线，CJK 字符也算词字符）。
fn word_at(text: &str, offset: usize) -> Option<std::ops::Range<usize>> {
    if offset > text.len() || !text.is_char_boundary(offset) {
        return None;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let start = text[..offset]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map_or(offset, |(at, _)| at);
    let end = offset
        + text[offset..]
            .char_indices()
            .take_while(|(_, c)| is_word(*c))
            .last()
            .map_or(0, |(at, c)| at + c.len_utf8());
    (end > start).then_some(start..end)
}

/// 跳行输入：`12`、`12:5`、`12,5`（行列都是 1 起）。返回 0 起的 (行, 列)，行夹到最后一行；
/// 不是数字返回 `None`。列不在这里夹（编辑器自己会夹到行尾）。
fn parse_goto(typed: &str, total_lines: usize) -> Option<(usize, usize)> {
    let typed = typed.trim();
    // i18n-allow: 用户输入的分隔符（中文冒号也认），不是界面文案
    let (line, column) = match typed.split_once([':', ',', '：']) {
        Some((line, column)) => (line.trim(), column.trim()),
        None => (typed, ""),
    };
    let line: usize = line.parse().ok()?;
    let column: usize = if column.is_empty() {
        1
    } else {
        column.parse().ok()?
    };
    let last = total_lines.saturating_sub(1);
    Some(((line.max(1) - 1).min(last), column.max(1) - 1))
}

/// 关窗口怎么处理：用户已经确认过，或者没有未保存修改，就关；否则先问。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseDecision {
    Close,
    Ask,
}

fn close_decision(confirmed: bool, unsaved: bool) -> CloseDecision {
    if confirmed || !unsaved {
        CloseDecision::Close
    } else {
        CloseDecision::Ask
    }
}

/// 工具栏「查找」/「替换」按下去是关面板还是开面板：同一模式已经开着 → 关；
/// 没开，或开着的是另一个模式 → 打开（切到）按下的那个模式。
fn search_button_closes(open: bool, open_in_replace: bool, pressed_replace: bool) -> bool {
    open && open_in_replace == pressed_replace
}

/// debug 入口：把一个远端文件开在独立窗口里（`ZSSH_SFTP_EDIT` 钩子和没有主窗口时的退路）。
/// 正式界面里编辑器是主窗口的标签（`main_window/editor_tabs.rs`）。
pub fn open_remote_editor_window(
    cx: &mut App,
    channel: Arc<SftpChannel>,
    host_id: i64,
    path: String,
    host_label: String,
) {
    let name = file_name(&path);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(
            size(::gpui::px(900.), ::gpui::px(620.)),
            cx,
        )),
        ..Default::default()
    };
    let _ = cx.open_window(options, move |window, cx| {
        let spec = OpenSpec {
            host_id,
            host_label,
            path,
            source: ChannelSource::Ready(channel),
            vault_account: None,
        };
        let view = cx.new(|cx| RemoteEditor::new(window, cx, spec, true));
        window.set_window_title(&format!("{}{name}", crate::i18n::t(EditorMsg::TitlePrefix)));
        let close_view = view.clone();
        // 有未保存修改时这里返回 false，窗口里出现确认条；确认之后由编辑器发 `CloseNow`，下面关窗口。
        window.on_window_should_close(cx, move |_window, cx| {
            close_view.update(cx, |this, cx| this.should_close(cx))
        });
        let closing = view.clone();
        let subscription =
            window.subscribe(&view, cx, move |_view, event: &EditorEvent, window, cx| {
                if matches!(event, EditorEvent::CloseNow) {
                    closing.update(cx, |editor, _| editor.shutdown());
                    window.remove_window();
                }
            });
        // 订阅随窗口活着。
        std::mem::forget(subscription);
        window.focus(&view.read(cx).focus_handle(), cx);
        view
    });
}

#[cfg(test)]
mod tests {
    use super::{
        ActiveSave, CloseDecision, CloseSave, CloseSaveStep, apply_saved_snapshot, close_decision,
        needs_draft, parse_goto, search_button_closes, word_at,
    };

    fn snapshot(id: u64, revision: u64, text: &str) -> Option<ActiveSave> {
        Some(ActiveSave {
            id,
            revision,
            text: text.to_owned(),
        })
    }

    #[test]
    fn close_waits_for_old_save_then_saves_latest_revision() {
        let mut close = Some(CloseSave::Saving);
        let mut active = snapshot(1, 10, "first");
        let mut baseline = "original".to_owned();
        let mut cache = Some((11, true));
        assert_eq!(
            CloseSave::advance(&mut close, true, true),
            CloseSaveStep::Wait
        );
        assert_eq!(
            apply_saved_snapshot(&mut active, 1, &mut baseline, &mut cache),
            Some(10)
        );
        assert_eq!(baseline, "first");
        assert_eq!(cache, None);
        assert_eq!(
            CloseSave::advance(&mut close, false, "latest" != baseline),
            CloseSaveStep::Save
        );
        assert_eq!(close, Some(CloseSave::Saving));
        active = snapshot(2, 11, "latest");
        assert_eq!(
            apply_saved_snapshot(&mut active, 2, &mut baseline, &mut cache),
            Some(11)
        );
        assert_eq!(
            CloseSave::advance(&mut close, false, "latest" != baseline),
            CloseSaveStep::Done
        );
        assert_eq!(close, Some(CloseSave::Done));
    }

    #[test]
    fn reverting_to_old_baseline_still_needs_save_and_draft() {
        let mut active = snapshot(1, 10, "changed");
        let mut baseline = "original".to_owned();
        let current = "original";
        let mut cache = Some((11, false));
        let mut close = Some(CloseSave::Saving);
        assert!(needs_draft(current, &baseline, active.as_ref()));
        assert_eq!(
            apply_saved_snapshot(&mut active, 1, &mut baseline, &mut cache),
            Some(10)
        );
        assert_eq!(cache, None);
        assert!(needs_draft(current, &baseline, None));
        assert_eq!(
            CloseSave::advance(&mut close, false, current != baseline),
            CloseSaveStep::Save
        );
        active = snapshot(2, 11, current);
        apply_saved_snapshot(&mut active, 2, &mut baseline, &mut cache);
        assert_eq!(
            CloseSave::advance(&mut close, false, current != baseline),
            CloseSaveStep::Done
        );
        assert!(!needs_draft(current, &baseline, None));
    }

    #[test]
    fn reverting_to_saved_snapshot_can_finish_without_another_write() {
        let mut close = Some(CloseSave::Saving);
        let mut active = snapshot(1, 10, "saved");
        let mut baseline = "original".to_owned();
        let mut cache = Some((12, true));
        // revision 12 已改过又撤销，内容恰好等于 revision 10 的在途快照。
        apply_saved_snapshot(&mut active, 1, &mut baseline, &mut cache);
        assert_eq!(
            CloseSave::advance(&mut close, false, "saved" != baseline),
            CloseSaveStep::Done
        );
    }

    #[test]
    fn stale_success_cannot_finish_or_replace_current_save() {
        let mut active = snapshot(2, 11, "latest");
        let mut baseline = "first".to_owned();
        let mut cache = Some((11, true));
        assert_eq!(
            apply_saved_snapshot(&mut active, 1, &mut baseline, &mut cache),
            None
        );
        assert_eq!(active.as_ref().unwrap().id, 2);
        assert_eq!(baseline, "first");
        assert_eq!(cache, Some((11, true)));
    }

    #[test]
    fn ordinary_save_keeps_new_input_without_chaining_or_closing() {
        let mut close = None;
        let mut active = snapshot(1, 10, "first");
        let mut baseline = "original".to_owned();
        let mut cache = Some((11, true));
        apply_saved_snapshot(&mut active, 1, &mut baseline, &mut cache);
        assert!(needs_draft("latest", &baseline, None));
        assert_eq!(
            CloseSave::advance(&mut close, false, "latest" != baseline),
            CloseSaveStep::Idle
        );
        assert_eq!(close, None);
    }

    #[test]
    fn failed_followup_and_cancelled_close_stay_open() {
        for reason in [
            "too large",
            "disconnected",
            "conflict",
            "owner change",
            "runtime",
        ] {
            let mut close = Some(CloseSave::Saving);
            assert_eq!(
                CloseSave::advance(&mut close, false, true),
                CloseSaveStep::Save
            );
            CloseSave::fail(&mut close, reason);
            assert_eq!(
                CloseSave::advance(&mut close, false, false),
                CloseSaveStep::Idle
            );
            assert_eq!(close, Some(CloseSave::Failed(reason.to_owned())));
        }
        let mut cancelled = None;
        CloseSave::fail(&mut cancelled, "late failure");
        assert_eq!(
            CloseSave::advance(&mut cancelled, false, false),
            CloseSaveStep::Idle
        );
        assert_eq!(cancelled, None);
    }

    #[test]
    fn empty_content_is_a_valid_draft_and_save_target() {
        let mut close = Some(CloseSave::Saving);
        assert!(needs_draft("", "nonempty", None));
        assert_eq!(
            CloseSave::advance(&mut close, false, true),
            CloseSaveStep::Save
        );
        assert!(!needs_draft("", "", None));
        assert_eq!(
            CloseSave::advance(&mut close, false, false),
            CloseSaveStep::Done
        );
    }

    #[test]
    fn search_buttons_toggle_their_own_mode_and_switch_the_other() {
        // 没开：两个按钮都是打开。
        assert!(!search_button_closes(false, false, false));
        assert!(!search_button_closes(false, false, true));
        // 查找开着：再点查找关掉，点替换切到替换。
        assert!(search_button_closes(true, false, false));
        assert!(!search_button_closes(true, false, true));
        // 替换开着：再点替换关掉，点查找切回查找。
        assert!(search_button_closes(true, true, true));
        assert!(!search_button_closes(true, true, false));
    }

    #[test]
    fn closing_asks_only_when_something_is_unsaved() {
        assert_eq!(close_decision(false, false), CloseDecision::Close);
        assert_eq!(close_decision(false, true), CloseDecision::Ask);
        assert_eq!(close_decision(true, true), CloseDecision::Close);
    }

    #[test]
    fn goto_accepts_line_and_line_colon_column() {
        assert_eq!(parse_goto("12", 100), Some((11, 0)));
        assert_eq!(parse_goto(" 12:5 ", 100), Some((11, 4)));
        assert_eq!(parse_goto("3,7", 100), Some((2, 6)));
        assert_eq!(parse_goto("3：2", 100), Some((2, 1)));
    }

    #[test]
    fn goto_clamps_to_the_document() {
        assert_eq!(parse_goto("999", 10), Some((9, 0)));
        assert_eq!(parse_goto("0", 10), Some((0, 0)));
        assert_eq!(parse_goto("5", 0), Some((0, 0)));
    }

    #[test]
    fn goto_rejects_garbage() {
        assert_eq!(parse_goto("", 10), None);
        assert_eq!(parse_goto("abc", 10), None);
        assert_eq!(parse_goto("1:x", 10), None);
        assert_eq!(parse_goto("-3", 10), None);
    }

    #[test]
    fn word_under_the_cursor() {
        assert_eq!(word_at("foo bar", 1), Some(0..3));
        assert_eq!(word_at("foo bar", 3), Some(0..3));
        assert_eq!(word_at("foo bar", 4), Some(4..7));
        assert_eq!(word_at("foo  bar", 4), None);
        assert_eq!(word_at("你好 world", 3), Some(0..6));
        assert_eq!(word_at("", 0), None);
    }
}
