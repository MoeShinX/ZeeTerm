//! 「移动到…」（V2-U23）：同一侧内把条目移到另一个文件夹。
//!
//! 这里是没有界面的部分：目标检查、一项一项移动的状态机（[`MoveJob`]）、同名处理的决策、结果汇总文案，
//! 以及真正动文件的两个函数（远端走 SFTP `rename`，本地走系统改名）。对话框和后台任务的接线在 `ops.rs`。
//!
//! 为什么是状态机：同名冲突要停下来问用户（替换 / 跳过 / 对其余都这样做），而移动本身是后台任务，
//! 所以流程是“发一项 → 等结果 → 决定下一步”。状态机只回答“下一步做什么”（[`Next`]），不碰界面和网络，
//! 每一条分支都能直接单测。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use zssh_core::sftp::SftpChannel;
use zssh_core::sftp::SftpError;
use zssh_core::sftp::fs::stat as remote_stat;
use zssh_core::sftp::path::{join, normalize, parent};
use zssh_core::sftp::transfer::Direction;

use super::dialogs::{NameCheck, validate_local_dir};
use super::menu::Side;
use super::model::{Entry, EntryKind, local_parent};
use crate::error_text::error_text;
use crate::i18n::t;

pub(super) fn same_folder_note() -> &'static str {
    t(super::SftpMsg::MoveSameFolder)
}
pub(super) fn into_self_note() -> &'static str {
    t(super::SftpMsg::MoveIntoSelf)
}
pub(super) fn cross_device_note() -> &'static str {
    t(super::SftpMsg::MoveCrossDevice)
}

// ---------------------------------------------------------------------------
// 路径比较
// ---------------------------------------------------------------------------

/// 比较用的路径键：远端按规范化的 POSIX 路径（区分大小写），本地按 Windows 规则
/// （`/` 和 `\` 一样、不分大小写、没有 `\\?\` 前缀、盘符根带 `\`、没有末尾的 `\`）。
fn path_key(side: Side, path: &str) -> String {
    match side {
        Side::Remote => normalize(path),
        Side::Local if !cfg!(windows) => zssh_core::pathnorm::normalize(Path::new(path))
            .to_string_lossy()
            .into_owned(),
        Side::Local => {
            let mut text = path.replace('/', "\\");
            if let Some(rest) = text.strip_prefix("\\\\?\\UNC\\") {
                text = format!("\\\\{rest}");
            } else if let Some(rest) = text.strip_prefix("\\\\?\\") {
                text = rest.to_owned();
            }
            let bytes = text.as_bytes();
            let is_bare_drive = bytes.len() == 2 && bytes[1] == b':';
            if is_bare_drive {
                text.push('\\');
            }
            while text.len() > 3 && text.ends_with('\\') {
                text.pop();
            }
            text.to_lowercase()
        }
    }
}

fn separator(side: Side) -> char {
    match side {
        Side::Remote => '/',
        Side::Local => std::path::MAIN_SEPARATOR,
    }
}

/// `inner` 就是 `outer`，或者在 `outer` 里面（任意深）。
pub(super) fn same_or_inside(side: Side, inner: &str, outer: &str) -> bool {
    let (inner, mut outer) = (path_key(side, inner), path_key(side, outer));
    if inner == outer {
        return true;
    }
    if !outer.ends_with(separator(side)) {
        outer.push(separator(side));
    }
    inner.starts_with(&outer)
}

/// 两个路径指同一个位置。
pub(super) fn same_path(side: Side, a: &str, b: &str) -> bool {
    path_key(side, a) == path_key(side, b)
}

/// 两个路径有上下级（或相同）关系：正在传输的东西和要移动的东西，谁包着谁都不能动。
pub(super) fn related(side: Side, a: &str, b: &str) -> bool {
    same_or_inside(side, a, b) || same_or_inside(side, b, a)
}

/// 一个没结束的传输任务会不会碰到要移动的条目（谁包着谁都算）。
pub(super) fn transfer_blocks(
    side: Side,
    item: &str,
    direction: Direction,
    source: &str,
    target: &str,
) -> bool {
    // 只看落在这一侧文件系统上的那一头：上传的目标在远端、源在本地，下载反过来。
    let paths: Vec<&str> = match (side, direction) {
        (Side::Remote, Direction::Upload) => vec![target],
        (Side::Remote, Direction::Download) => vec![source],
        (Side::Remote, Direction::RemoteCopy) => vec![source, target],
        (Side::Local, Direction::Upload) => vec![source],
        (Side::Local, Direction::Download) => vec![target],
        (Side::Local, Direction::LocalCopy) => vec![source, target],
        _ => Vec::new(),
    };
    paths.into_iter().any(|path| related(side, item, path))
}

/// 条目所在的文件夹。
fn parent_of(side: Side, path: &str) -> String {
    match side {
        Side::Remote => parent(path),
        Side::Local => local_parent(path),
    }
}

/// 把条目放进 `dest` 之后的完整路径。名字原样拼，不做转义（空格、中文、特殊字符都可以）。
pub(super) fn target_path(side: Side, dest: &str, name: &str) -> String {
    match side {
        Side::Remote => join(dest, name),
        Side::Local => PathBuf::from(dest).join(name).display().to_string(),
    }
}

// ---------------------------------------------------------------------------
// 目标检查
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DestCheck {
    /// 可以移动；带规范好的目标文件夹。
    Ok(String),
    /// 没填 / 写法不对：原因给用户看（空的不提示）。
    Invalid(String),
    /// 选中的条目已经都在这个文件夹里。
    SameFolder,
    /// 要把文件夹移到它自己（或它的子目录）里。
    IntoSelf,
}

impl DestCheck {
    /// 给用户看的一句话；`Ok` 没有。
    pub(super) fn note(&self) -> Option<String> {
        match self {
            Self::Ok(_) => None,
            Self::Invalid(reason) => Some(reason.clone()).filter(|r| !r.is_empty()),
            Self::SameFolder => Some(same_folder_note().to_owned()),
            Self::IntoSelf => Some(into_self_note().to_owned()),
        }
    }

    pub(super) fn is_ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }
}

/// 目标文件夹的写法检查（不查目录存不存在，那一步在“移动”时去问）。
fn dest_shape(side: Side, raw: &str) -> Result<String, String> {
    let text = raw.trim();
    if text.is_empty() {
        return Err(String::new());
    }
    match side {
        Side::Remote => {
            if !text.starts_with('/') {
                return Err(t(super::SftpMsg::MoveDestNeedAbsoluteRemote).to_owned());
            }
            Ok(normalize(text))
        }
        Side::Local => {
            if let NameCheck::Bad(reason) = validate_local_dir(text) {
                return Err(reason);
            }
            if !cfg!(windows) {
                let path = Path::new(text);
                return if path.is_absolute() {
                    Ok(zssh_core::pathnorm::normalize(path).display().to_string())
                } else {
                    Err(t(super::SftpMsg::MoveDestNeedAbsoluteRemote).to_owned())
                };
            }
            let normalized = text.replace('/', "\\");
            let drive = normalized.len() >= 2
                && normalized.as_bytes()[1] == b':'
                && normalized.as_bytes()[0].is_ascii_alphabetic();
            if !(drive || normalized.starts_with("\\\\")) {
                return Err(t(super::SftpMsg::MoveDestNeedAbsoluteLocal).to_owned());
            }
            // 盘符根写成 `D:` 时补上 `\`（不补的话 `D:` 指的是那个盘的当前目录）。
            if normalized.len() == 2 {
                return Ok(format!("{normalized}\\"));
            }
            let trimmed = normalized.trim_end_matches('\\');
            Ok(if trimmed.len() == 2 {
                format!("{trimmed}\\")
            } else {
                trimmed.to_owned()
            })
        }
    }
}

/// 一个条目放进 `dest` 有没有问题：已经在里面 / 文件夹放进自己里面。
pub(super) fn entry_problem(side: Side, entry: &Entry, dest: &str) -> Option<&'static str> {
    if path_key(side, &parent_of(side, &entry.path)) == path_key(side, dest) {
        return Some(same_folder_note());
    }
    if entry.kind == EntryKind::Dir && same_or_inside(side, dest, &entry.path) {
        return Some(into_self_note());
    }
    None
}

/// 对话框里“移动”按钮能不能点，以及不能点时写什么。
pub(super) fn check_destination(side: Side, entries: &[Entry], raw: &str) -> DestCheck {
    let dest = match dest_shape(side, raw) {
        Ok(dest) => dest,
        Err(reason) => return DestCheck::Invalid(reason),
    };
    if entries.is_empty() {
        return DestCheck::Invalid(t(super::SftpMsg::MoveSelectItems).to_owned());
    }
    let problems: Vec<&'static str> = entries
        .iter()
        .map(|entry| entry_problem(side, entry, &dest))
        .collect::<Option<_>>()
        .unwrap_or_default();
    // 每一项都有问题才置灰；只有部分有问题的，那几项在执行时跳过并写进汇总。
    if problems.len() == entries.len() {
        if problems.contains(&into_self_note()) {
            return DestCheck::IntoSelf;
        }
        return DestCheck::SameFolder;
    }
    DestCheck::Ok(dest)
}

/// 对话框标题。
pub(super) fn dialog_title(entries: &[Entry]) -> String {
    match entries {
        [only] => format!(
            "{}{}{}",
            t(super::SftpMsg::MoveDialogOnePrefix),
            only.name,
            t(super::SftpMsg::MoveDialogOneSuffix)
        ),
        _ => format!(
            "{}{}{}",
            t(super::SftpMsg::MoveDialogManyPrefix),
            entries.len(),
            t(super::SftpMsg::MoveDialogManySuffix)
        ),
    }
}

/// 文件夹浏览列表：只要能进去的目录（含指向目录的链接）、名字合法的，排好序。
pub(super) fn folders_only(entries: Vec<Entry>) -> Vec<Entry> {
    let mut folders: Vec<Entry> = entries
        .into_iter()
        .filter(|entry| entry.is_enterable() && entry.name_is_utf8)
        .collect();
    // 链接指向的文件夹和普通文件夹混在一起按名字排（列表里的“目录在前”规则在这里没有意义）。
    folders.sort_by(|a, b| crate::main_window::collation::compare(&a.name, &b.name));
    folders
}

// ---------------------------------------------------------------------------
// 一项一项移动的状态机
// ---------------------------------------------------------------------------

/// 一次移动里一项的结果（后台任务回报给状态机）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum StepResult {
    Moved,
    /// 目标位置已经有同名的东西；`target_is_dir` = 它是文件夹。
    Exists {
        target_is_dir: bool,
    },
    Failed(String),
}

/// 同名时用户的选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Choice {
    Replace,
    Skip,
}

/// 等用户回答的同名冲突。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MoveAsk {
    pub(super) name: String,
    pub(super) target: String,
    /// 能替换吗；不能（文件夹相关）时只有“跳过”，原因在 `why_not`。
    pub(super) can_replace: bool,
    pub(super) why_not: Option<String>,
    /// 后面还有几项没处理（>0 才出现“对其余同名项都这样做”）。
    pub(super) remaining: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Note {
    pub(super) name: String,
    pub(super) reason: String,
}

/// 一次移动的账：成功几项、跳过哪些（为什么）、失败哪些（为什么）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Tally {
    pub(super) moved: Vec<String>,
    pub(super) skipped: Vec<Note>,
    pub(super) failed: Vec<Note>,
}

impl Tally {
    pub(super) fn has_problems(&self) -> bool {
        !self.skipped.is_empty() || !self.failed.is_empty()
    }

    /// 状态栏上的汇总：`已移动 3 项到「docs」，跳过 1 项，失败 1 项：a.txt（原因）`。
    pub(super) fn summary(&self, dest_name: &str) -> String {
        let mut parts: Vec<String> = Vec::new();
        match self.moved.as_slice() {
            [] => {}
            [only] => parts.push(format!(
                "{}{only}{}{dest_name}{}",
                t(super::SftpMsg::MoveSummaryMovedOnePrefix),
                t(super::SftpMsg::MoveSummaryMovedToMiddle),
                t(super::SftpMsg::MoveSummaryMovedToClose)
            )),
            many => parts.push(format!(
                "{}{}{}{dest_name}{}",
                t(super::SftpMsg::MoveSummaryMovedManyPrefix),
                many.len(),
                t(super::SftpMsg::MoveSummaryMovedManyMiddle),
                t(super::SftpMsg::MoveSummaryMovedToClose)
            )),
        }
        if !self.skipped.is_empty() {
            parts.push(format!(
                "{}{}{}",
                t(super::SftpMsg::MoveSummarySkippedPrefix),
                self.skipped.len(),
                t(super::SftpMsg::MoveSummarySkippedSuffix)
            ));
        }
        if let Some(first) = self.failed.first() {
            let more = if self.failed.len() > 1 {
                t(super::SftpMsg::MoveSummaryFailedMore)
            } else {
                ""
            };
            let mut text = format!(
                "{}{}{}{}{}",
                t(super::SftpMsg::MoveSummaryFailedPrefix),
                self.failed.len(),
                t(super::SftpMsg::MoveSummaryFailedMiddle),
                first.name,
                t(super::SftpMsg::ParenOpen)
            );
            text.push_str(&first.reason);
            text.push_str(t(super::SftpMsg::ParenClose));
            text.push_str(more);
            parts.push(text);
        }
        if parts.is_empty() {
            return t(super::SftpMsg::MoveNoItemsMoved).to_owned();
        }
        parts.join(t(super::SftpMsg::MoveSummarySeparator))
    }

    /// 汇总对话框里一行一项的明细（跳过的在前，失败的在后）。
    pub(super) fn detail_lines(&self) -> Vec<String> {
        self.skipped
            .iter()
            .map(|note| {
                format!(
                    "{}{}{}{}",
                    t(super::SftpMsg::MoveDetailSkippedPrefix),
                    note.name,
                    t(super::SftpMsg::MoveDetailMiddle),
                    note.reason
                )
            })
            .chain(self.failed.iter().map(|note| {
                format!(
                    "{}{}{}{}",
                    t(super::SftpMsg::MoveDetailFailedPrefix),
                    note.name,
                    t(super::SftpMsg::MoveDetailMiddle),
                    note.reason
                )
            }))
            .collect()
    }
}

/// 状态机对界面的指令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Next {
    /// 去移动这一项（`overwrite` = 用户已经同意替换同名文件）。
    Attempt {
        entry: Entry,
        target: String,
        overwrite: bool,
    },
    /// 停下来问用户。
    Ask(MoveAsk),
    /// 全部处理完。
    Done(Tally),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Idle,
    /// 正在移动（第一次尝试，不覆盖）。
    Trying {
        entry: Entry,
        target: String,
    },
    /// 同名了，等用户。
    Asking {
        entry: Entry,
        target: String,
        can_replace: bool,
    },
    /// 用户同意替换，正在覆盖式移动。
    Replacing {
        entry: Entry,
    },
}

pub(super) struct MoveJob {
    pub(super) id: u64,
    pub(super) side: Side,
    /// 目标文件夹（规范好的）。
    pub(super) dest: String,
    queue: VecDeque<Entry>,
    phase: Phase,
    /// “对其余同名项都这样做”选了什么。
    apply_rest: Option<Choice>,
    tally: Tally,
}

/// 为什么这一项不能替换同名的东西；`None` = 可以替换。
fn replace_blocker(source_is_dir: bool, target_is_dir: bool) -> Option<&'static str> {
    match (source_is_dir, target_is_dir) {
        (true, true) => Some(t(super::SftpMsg::MoveWhyFolderMerge)),
        (true, false) => Some(t(super::SftpMsg::MoveWhyTargetFile)),
        (false, true) => Some(t(super::SftpMsg::MoveWhyTargetFolder)),
        (false, false) => None,
    }
}

impl MoveJob {
    pub(super) fn new(id: u64, side: Side, dest: String, entries: Vec<Entry>) -> Self {
        Self {
            id,
            side,
            dest,
            queue: entries.into(),
            phase: Phase::Idle,
            apply_rest: None,
            tally: Tally::default(),
        }
    }

    fn skip(&mut self, name: &str, reason: impl Into<String>) {
        self.tally.skipped.push(Note {
            name: name.to_owned(),
            reason: reason.into(),
        });
    }

    fn fail(&mut self, name: &str, reason: impl Into<String>) {
        self.tally.failed.push(Note {
            name: name.to_owned(),
            reason: reason.into(),
        });
    }

    /// 取下一项。`blocked` 回答“这一项现在能不能动”（正在编辑 / 传输的返回原因，跳过）。
    pub(super) fn advance(&mut self, blocked: &dyn Fn(&Entry) -> Option<String>) -> Next {
        self.phase = Phase::Idle;
        while let Some(entry) = self.queue.pop_front() {
            if let Some(reason) = blocked(&entry) {
                self.skip(&entry.name, reason);
                continue;
            }
            if let Some(reason) = entry_problem(self.side, &entry, &self.dest) {
                self.skip(&entry.name, reason);
                continue;
            }
            let target = target_path(self.side, &self.dest, &entry.name);
            self.phase = Phase::Trying {
                entry: entry.clone(),
                target: target.clone(),
            };
            return Next::Attempt {
                entry,
                target,
                overwrite: false,
            };
        }
        Next::Done(std::mem::take(&mut self.tally))
    }

    /// 后台任务报回一项的结果。
    pub(super) fn on_step(
        &mut self,
        result: StepResult,
        blocked: &dyn Fn(&Entry) -> Option<String>,
    ) -> Next {
        match std::mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Trying { entry, target } => match result {
                StepResult::Moved => {
                    self.tally.moved.push(entry.name);
                    self.advance(blocked)
                }
                StepResult::Failed(reason) => {
                    self.fail(&entry.name, reason);
                    self.advance(blocked)
                }
                StepResult::Exists { target_is_dir } => {
                    let source_is_dir = entry.kind == EntryKind::Dir;
                    let blocker = replace_blocker(source_is_dir, target_is_dir);
                    match (self.apply_rest, blocker) {
                        (Some(Choice::Replace), None) => {
                            self.phase = Phase::Replacing {
                                entry: entry.clone(),
                            };
                            Next::Attempt {
                                entry,
                                target,
                                overwrite: true,
                            }
                        }
                        (Some(Choice::Replace), Some(why)) => {
                            self.skip(&entry.name, why);
                            self.advance(blocked)
                        }
                        (Some(Choice::Skip), _) => {
                            self.skip(&entry.name, t(super::SftpMsg::MoveSkipTargetExists));
                            self.advance(blocked)
                        }
                        (None, blocker) => {
                            self.phase = Phase::Asking {
                                entry: entry.clone(),
                                target: target.clone(),
                                can_replace: blocker.is_none(),
                            };
                            Next::Ask(MoveAsk {
                                name: entry.name,
                                target,
                                can_replace: blocker.is_none(),
                                why_not: blocker.map(str::to_owned),
                                remaining: self.queue.len(),
                            })
                        }
                    }
                }
            },
            Phase::Replacing { entry } => {
                match result {
                    StepResult::Moved => self.tally.moved.push(entry.name),
                    StepResult::Failed(reason) => self.fail(&entry.name, reason),
                    StepResult::Exists { .. } => {
                        self.fail(&entry.name, t(super::SftpMsg::MoveReplaceFailed))
                    }
                }
                self.advance(blocked)
            }
            // 没有在等结果时来了结果（用户取消之后迟到的回报）：忽略，保持当前状态。
            other => {
                self.phase = other;
                self.pending_next()
            }
        }
    }

    /// 迟到的回报不改变任何东西；返回当前该给界面的指令（正在问就继续问）。
    fn pending_next(&self) -> Next {
        match &self.phase {
            Phase::Asking {
                entry,
                target,
                can_replace,
            } => Next::Ask(MoveAsk {
                name: entry.name.clone(),
                target: target.clone(),
                can_replace: *can_replace,
                why_not: (!can_replace).then(|| t(super::SftpMsg::MoveSkipFolderOnly).to_owned()),
                remaining: self.queue.len(),
            }),
            _ => Next::Done(Tally::default()),
        }
    }

    /// 用户在同名对话框里点了按钮。`apply_rest` = 勾了“对其余同名项都这样做”。
    pub(super) fn answer(
        &mut self,
        choice: Choice,
        apply_rest: bool,
        blocked: &dyn Fn(&Entry) -> Option<String>,
    ) -> Next {
        let Phase::Asking {
            entry,
            target,
            can_replace,
        } = std::mem::replace(&mut self.phase, Phase::Idle)
        else {
            return self.advance(blocked);
        };
        let choice = if can_replace { choice } else { Choice::Skip };
        if apply_rest {
            self.apply_rest = Some(choice);
        }
        match choice {
            Choice::Replace => {
                self.phase = Phase::Replacing {
                    entry: entry.clone(),
                };
                Next::Attempt {
                    entry,
                    target,
                    overwrite: true,
                }
            }
            Choice::Skip => {
                self.skip(&entry.name, t(super::SftpMsg::MoveSkipTargetExists));
                self.advance(blocked)
            }
        }
    }

    /// 用户关掉了同名对话框 / 取消：这一项和后面没处理的都跳过，不再动任何东西。
    pub(super) fn abort(&mut self) -> Next {
        if let Phase::Asking { entry, .. } = std::mem::replace(&mut self.phase, Phase::Idle) {
            self.skip(&entry.name, t(super::SftpMsg::MoveSkipCancelled));
        }
        while let Some(entry) = self.queue.pop_front() {
            self.skip(&entry.name, t(super::SftpMsg::MoveSkipCancelled));
        }
        Next::Done(std::mem::take(&mut self.tally))
    }
}

// ---------------------------------------------------------------------------
// 真正动文件的两个函数
// ---------------------------------------------------------------------------

/// 远端：移动一项。先不覆盖地试；服务器说目标存在，就看一眼目标是不是文件夹，交给状态机决定。
pub(super) async fn remote_step(
    channel: &SftpChannel,
    from: &str,
    to: &str,
    overwrite: bool,
) -> StepResult {
    match channel.rename(from, to, overwrite).await {
        Ok(_) => StepResult::Moved,
        Err(SftpError::AlreadyExists { .. }) if !overwrite => {
            let target_is_dir = remote_stat(channel, to, false)
                .await
                .is_ok_and(|stat| stat.is_dir());
            StepResult::Exists { target_is_dir }
        }
        Err(err) => StepResult::Failed(error_text(&err)),
    }
}

/// 远端：列出一个目录里能进去的子文件夹。
pub(super) async fn remote_folders(channel: &SftpChannel, dir: &str) -> Result<Vec<Entry>, String> {
    let entries = channel
        .read_dir(dir)
        .await
        .map_err(|err| error_text(&err))?;
    Ok(folders_only(
        entries.iter().map(Entry::from_remote).collect(),
    ))
}

/// 本地：列出一个目录里的子文件夹（空路径 = “此电脑”，列出盘符）。只能在后台线程里调
/// （见 [`super::local_listing`]），`fs` 由线程池传进来。
pub(super) fn local_folders(
    fs: &dyn super::local_listing::LocalFs,
    dir: &str,
) -> Result<Vec<Entry>, String> {
    fs.list_dir(dir).map(folders_only)
}

/// 本地：移动一项（见 [`super::local_ops::move_item`]）。
pub(super) fn local_step(from: &str, to: &str, overwrite: bool) -> StepResult {
    super::local_ops::move_item(Path::new(from), Path::new(to), overwrite)
}

#[cfg(test)]
mod tests {
    use super::super::model::{Entry, EntryKind};
    use super::{
        Choice, DestCheck, MoveJob, Next, Side, StepResult, Tally, check_destination, dialog_title,
        folders_only, into_self_note, related, same_folder_note, same_or_inside, target_path,
    };

    fn entry(side: Side, dir: &str, name: &str, kind: EntryKind) -> Entry {
        let path = match side {
            Side::Remote => format!("{}/{name}", dir.trim_end_matches('/')),
            Side::Local => std::path::Path::new(dir).join(name).display().to_string(),
        };
        Entry {
            name: name.to_owned(),
            path,
            kind,
            target_kind: None,
            link_target: None,
            size: None,
            mtime: None,
            permissions: None,
            owner: None,
            hidden: false,
            name_is_utf8: true,
        }
    }

    fn remote(name: &str, kind: EntryKind) -> Entry {
        entry(Side::Remote, "/srv/app", name, kind)
    }

    #[cfg(windows)]
    fn local(name: &str, kind: EntryKind) -> Entry {
        entry(Side::Local, "D:\\work", name, kind)
    }

    fn none(_: &Entry) -> Option<String> {
        None
    }

    // ---- 路径 ----

    #[test]
    fn inside_means_the_same_folder_or_any_depth_below_it() {
        assert!(same_or_inside(Side::Remote, "/a/b", "/a/b"));
        assert!(same_or_inside(Side::Remote, "/a/b/c/d", "/a/b"));
        assert!(same_or_inside(Side::Remote, "/a/b/", "/a/b"));
        assert!(
            !same_or_inside(Side::Remote, "/a/bc", "/a/b"),
            "只是字符串前缀"
        );
        assert!(!same_or_inside(Side::Remote, "/a", "/a/b"));
        // 远端区分大小写。
        assert!(!same_or_inside(Side::Remote, "/a/B", "/a/b"));
        assert!(same_or_inside(Side::Remote, "/anything", "/"));
    }

    #[cfg(windows)]
    #[test]
    fn local_paths_ignore_case_slashes_prefixes_and_trailing_separators() {
        assert!(same_or_inside(Side::Local, "d:\\Work\\Sub", "D:\\work"));
        assert!(same_or_inside(Side::Local, "D:/work/sub", "D:\\work\\"));
        assert!(same_or_inside(
            Side::Local,
            "\\\\?\\D:\\work\\sub",
            "D:\\work"
        ));
        assert!(!same_or_inside(Side::Local, "D:\\workshop", "D:\\work"));
        assert!(
            same_or_inside(Side::Local, "D:\\work", "D:"),
            "D: 当作 D:\\"
        );
        assert!(same_or_inside(Side::Local, "D:\\", "D:\\"));
        assert!(same_or_inside(
            Side::Local,
            "\\\\?\\UNC\\srv\\share\\x",
            "\\\\srv\\share"
        ));
        assert!(!same_or_inside(Side::Local, "E:\\work", "D:\\work"));
    }

    #[cfg(windows)]
    #[test]
    fn live_transfers_block_the_items_they_touch_in_either_direction() {
        use super::transfer_blocks as blocks;
        use zssh_core::sftp::transfer::Direction::{Download, LocalCopy, RemoteCopy, Upload};
        let (remote, local) = (Side::Remote, Side::Local);
        // 上传的目标在远端：远端的 /srv/app 包着它，所以不能动；本地的源和远端无关。
        assert!(blocks(
            remote,
            "/srv/app",
            Upload,
            r"D:\src\f.txt",
            "/srv/app/f.txt"
        ));
        assert!(!blocks(
            local,
            r"D:\other",
            Upload,
            r"D:\src\f.txt",
            "/srv/app/f.txt"
        ));
        assert!(blocks(
            local,
            r"D:\src",
            Upload,
            r"D:\src\f.txt",
            "/srv/app/f.txt"
        ));
        // 下载的源在远端，目标在本地。
        assert!(blocks(
            remote,
            "/srv/app/big.iso",
            Download,
            "/srv/app/big.iso",
            r"D:\dl\big.iso"
        ));
        assert!(blocks(local, r"D:\dl", Download, "/srv/app", r"D:\dl\app"));
        // 传整个文件夹时，里面的东西也不能动。
        assert!(blocks(
            remote,
            "/srv/app/sub/x",
            Download,
            "/srv/app",
            r"D:\dl\app"
        ));
        // 两头都在同一侧的复制。
        assert!(blocks(remote, "/b", RemoteCopy, "/a", "/b/a"));
        assert!(blocks(local, r"D:\a", LocalCopy, r"D:\a\x", r"D:\b\x"));
        assert!(!blocks(local, r"D:\c", LocalCopy, r"D:\a\x", r"D:\b\x"));
        // 不相干的兄弟路径。
        assert!(!blocks(
            remote,
            "/srv/other",
            Download,
            "/srv/app",
            r"D:\dl"
        ));
    }

    #[test]
    fn related_is_symmetric() {
        assert!(related(Side::Remote, "/a/b", "/a"));
        assert!(related(Side::Remote, "/a", "/a/b"));
        assert!(!related(Side::Remote, "/a/b", "/a/c"));
    }

    #[cfg(windows)]
    #[test]
    fn target_paths_are_joined_with_the_path_tools_not_by_hand() {
        assert_eq!(target_path(Side::Remote, "/", "a.txt"), "/a.txt");
        assert_eq!(
            target_path(Side::Remote, "/srv/x/", "报告 最终.txt"),
            "/srv/x/报告 最终.txt"
        );
        assert_eq!(
            target_path(Side::Remote, "/srv", "a'b $c;.txt"),
            "/srv/a'b $c;.txt"
        );
        assert_eq!(
            target_path(Side::Local, "D:\\out", "报告 1.txt"),
            "D:\\out\\报告 1.txt"
        );
        assert_eq!(target_path(Side::Local, "D:\\", "a.txt"), "D:\\a.txt");
    }

    // ---- 目标检查 ----

    #[cfg(windows)]
    #[test]
    fn a_destination_equal_to_the_source_folder_is_refused_with_the_card_wording() {
        let entries = [
            remote("a.txt", EntryKind::File),
            remote("b", EntryKind::Dir),
        ];
        assert_eq!(
            check_destination(Side::Remote, &entries, "/srv/app"),
            DestCheck::SameFolder
        );
        // 末尾斜杠、重复斜杠不影响判断。
        assert_eq!(
            check_destination(Side::Remote, &entries, " /srv//app/ "),
            DestCheck::SameFolder
        );
        assert_eq!(
            DestCheck::SameFolder.note().as_deref(),
            Some(same_folder_note())
        );
        assert_eq!(same_folder_note(), "已经在这个文件夹里");
        let local_entries = [local("a.txt", EntryKind::File)];
        assert_eq!(
            check_destination(Side::Local, &local_entries, "d:/WORK/"),
            DestCheck::SameFolder
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_folder_cannot_go_into_itself_or_below_itself() {
        let entries = [remote("data", EntryKind::Dir)];
        for dest in [
            "/srv/app/data",
            "/srv/app/data/",
            "/srv/app/data/inner/deeper",
        ] {
            assert_eq!(
                check_destination(Side::Remote, &entries, dest),
                DestCheck::IntoSelf,
                "{dest}"
            );
        }
        assert_eq!(into_self_note(), "不能把文件夹移到它自己里面");
        // 兄弟目录和上级目录都可以（上级就是“已经在里面”以外的别的目录）。
        assert!(check_destination(Side::Remote, &entries, "/srv/app/data2").is_ok());
        assert!(check_destination(Side::Remote, &entries, "/srv").is_ok());
        // 文件移到“自己的路径”下面没有意义但不是这条规则管的事：文件不受限。
        let file = [remote("f", EntryKind::File)];
        assert!(check_destination(Side::Remote, &file, "/srv/app/f").is_ok());
        let local_dirs = [local("data", EntryKind::Dir)];
        assert_eq!(
            check_destination(Side::Local, &local_dirs, "D:\\work\\Data\\x"),
            DestCheck::IntoSelf
        );
    }

    #[test]
    fn a_mixed_selection_is_only_refused_when_every_item_has_a_problem() {
        // 文件夹 data 放不进自己，但文件 a.txt 放进 data 没问题：整体可以点，data 在执行时跳过。
        let entries = [
            remote("data", EntryKind::Dir),
            remote("a.txt", EntryKind::File),
        ];
        assert_eq!(
            check_destination(Side::Remote, &entries, "/srv/app/data"),
            DestCheck::Ok("/srv/app/data".to_owned())
        );
    }

    #[cfg(windows)]
    #[test]
    fn bad_destinations_say_why_or_stay_silent_when_empty() {
        let entries = [remote("a", EntryKind::File)];
        assert_eq!(
            check_destination(Side::Remote, &entries, "   "),
            DestCheck::Invalid(String::new())
        );
        assert_eq!(DestCheck::Invalid(String::new()).note(), None);
        assert_eq!(
            check_destination(Side::Remote, &entries, "relative/dir")
                .note()
                .as_deref(),
            Some("目标要写成完整路径（以 / 开头）")
        );
        let local_entries = [local("a", EntryKind::File)];
        assert!(
            check_destination(Side::Local, &local_entries, "docs")
                .note()
                .is_some()
        );
        assert!(
            check_destination(Side::Local, &local_entries, "D:\\a<b")
                .note()
                .is_some()
        );
        assert!(
            check_destination(Side::Local, &[], "D:\\x")
                .note()
                .is_some()
        );
        // 盘符根：`E:` 补成 `E:\`，不当成“E 盘的当前目录”。
        assert_eq!(
            check_destination(Side::Local, &local_entries, "E:"),
            DestCheck::Ok("E:\\".to_owned())
        );
        assert_eq!(
            check_destination(Side::Local, &local_entries, "E:\\sub\\"),
            DestCheck::Ok("E:\\sub".to_owned())
        );
    }

    #[test]
    fn the_dialog_title_names_a_single_item_and_counts_several() {
        assert_eq!(
            dialog_title(&[remote("报告.txt", EntryKind::File)]),
            "移动「报告.txt」到…"
        );
        assert_eq!(
            dialog_title(&[remote("a", EntryKind::File), remote("b", EntryKind::Dir)]),
            "移动 2 项到…"
        );
    }

    #[test]
    fn the_browser_lists_only_enterable_folders_in_name_order() {
        let mut link_dir = remote("link", EntryKind::Symlink);
        link_dir.target_kind = Some(super::super::model::TargetKind::Dir);
        let mut bad = remote("bad", EntryKind::Dir);
        bad.name_is_utf8 = false;
        let listed = folders_only(vec![
            remote("zeta", EntryKind::Dir),
            remote("file.txt", EntryKind::File),
            link_dir,
            remote("Alpha", EntryKind::Dir),
            remote("sock", EntryKind::Other),
            bad,
        ]);
        let names: Vec<&str> = listed.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "link", "zeta"]);
    }

    // ---- 状态机 ----

    fn job(entries: Vec<Entry>) -> MoveJob {
        MoveJob::new(1, Side::Remote, "/srv/dest".to_owned(), entries)
    }

    fn attempt(next: &Next) -> (&str, &str, bool) {
        match next {
            Next::Attempt {
                entry,
                target,
                overwrite,
            } => (entry.name.as_str(), target.as_str(), *overwrite),
            other => panic!("预期去移动一项，实际是 {other:?}"),
        }
    }

    #[test]
    fn a_clean_run_moves_every_item_in_order_and_reports_the_total() {
        let mut job = job(vec![
            remote("a", EntryKind::File),
            remote("b c", EntryKind::Dir),
        ]);
        let first = job.advance(&none);
        assert_eq!(attempt(&first), ("a", "/srv/dest/a", false));
        let second = job.on_step(StepResult::Moved, &none);
        assert_eq!(attempt(&second), ("b c", "/srv/dest/b c", false));
        let Next::Done(tally) = job.on_step(StepResult::Moved, &none) else {
            panic!("应该做完了");
        };
        assert_eq!(tally.moved, ["a", "b c"]);
        assert!(!tally.has_problems());
        assert_eq!(tally.summary("dest"), "已移动 2 项到「dest」");
    }

    #[test]
    fn an_empty_job_finishes_at_once() {
        let mut job = job(vec![]);
        assert_eq!(job.advance(&none), Next::Done(Tally::default()));
        assert_eq!(Tally::default().summary("x"), "没有移动任何项");
    }

    #[test]
    fn items_that_are_being_edited_or_transferred_are_skipped_with_the_reason() {
        let mut job = job(vec![
            remote("busy.txt", EntryKind::File),
            remote("free.txt", EntryKind::File),
        ]);
        let blocked = |entry: &Entry| {
            (entry.name == "busy.txt").then(|| "busy.txt 正在编辑，先保存并关闭再移动".to_owned())
        };
        let first = job.advance(&blocked);
        assert_eq!(attempt(&first).0, "free.txt");
        let Next::Done(tally) = job.on_step(StepResult::Moved, &blocked) else {
            panic!("应该做完了");
        };
        assert_eq!(tally.moved, ["free.txt"]);
        assert_eq!(tally.skipped.len(), 1);
        assert_eq!(
            tally.skipped[0].reason,
            "busy.txt 正在编辑，先保存并关闭再移动"
        );
        assert_eq!(
            tally.summary("dest"),
            "已移动 free.txt 到「dest」，跳过 1 项"
        );
        assert_eq!(
            tally.detail_lines(),
            ["跳过 busy.txt：busy.txt 正在编辑，先保存并关闭再移动"]
        );
    }

    #[test]
    fn an_item_already_in_the_destination_or_a_folder_into_itself_is_skipped_not_tried() {
        let mut job = MoveJob::new(
            1,
            Side::Remote,
            "/srv/app/data".to_owned(),
            vec![
                remote("data", EntryKind::Dir),
                entry(Side::Remote, "/srv/app/data", "inside.txt", EntryKind::File),
                remote("ok.txt", EntryKind::File),
            ],
        );
        let first = job.advance(&none);
        assert_eq!(attempt(&first).0, "ok.txt");
        let Next::Done(tally) = job.on_step(StepResult::Moved, &none) else {
            panic!();
        };
        assert_eq!(tally.moved, ["ok.txt"]);
        let reasons: Vec<&str> = tally.skipped.iter().map(|n| n.reason.as_str()).collect();
        assert_eq!(reasons, [into_self_note(), same_folder_note()]);
    }

    #[test]
    fn one_failure_does_not_stop_the_others_and_the_summary_names_the_first() {
        let mut job = job(vec![
            remote("a", EntryKind::File),
            remote("b", EntryKind::File),
            remote("c", EntryKind::File),
            remote("d", EntryKind::File),
        ]);
        job.advance(&none);
        job.on_step(StepResult::Failed("权限不够".to_owned()), &none);
        job.on_step(StepResult::Moved, &none);
        job.on_step(StepResult::Failed("磁盘已满".to_owned()), &none);
        let Next::Done(tally) = job.on_step(StepResult::Moved, &none) else {
            panic!();
        };
        assert_eq!(tally.moved, ["b", "d"]);
        assert_eq!(tally.failed.len(), 2);
        assert_eq!(
            tally.summary("dest"),
            "已移动 2 项到「dest」，失败 2 项：a（权限不够） 等"
        );
        assert_eq!(
            tally.detail_lines(),
            ["失败 a：权限不够", "失败 c：磁盘已满"]
        );
    }

    #[test]
    fn the_summary_matches_the_card_example_shape() {
        let tally = Tally {
            moved: vec!["a".into(), "b".into(), "c".into()],
            skipped: vec![super::Note {
                name: "d".into(),
                reason: "x".into(),
            }],
            failed: vec![super::Note {
                name: "e.txt".into(),
                reason: "权限不够".into(),
            }],
        };
        assert_eq!(
            tally.summary("docs"),
            "已移动 3 项到「docs」，跳过 1 项，失败 1 项：e.txt（权限不够）"
        );
    }

    #[test]
    fn a_name_clash_asks_and_replace_retries_with_overwrite() {
        let mut job = job(vec![
            remote("a.txt", EntryKind::File),
            remote("b.txt", EntryKind::File),
        ]);
        job.advance(&none);
        let ask = job.on_step(
            StepResult::Exists {
                target_is_dir: false,
            },
            &none,
        );
        let Next::Ask(ask) = ask else {
            panic!("应该问用户");
        };
        assert_eq!(ask.name, "a.txt");
        assert_eq!(ask.target, "/srv/dest/a.txt");
        assert!(ask.can_replace && ask.why_not.is_none());
        assert_eq!(ask.remaining, 1);
        let retry = job.answer(Choice::Replace, false, &none);
        assert_eq!(attempt(&retry), ("a.txt", "/srv/dest/a.txt", true));
        let next = job.on_step(StepResult::Moved, &none);
        assert_eq!(attempt(&next), ("b.txt", "/srv/dest/b.txt", false));
        let Next::Done(tally) = job.on_step(StepResult::Moved, &none) else {
            panic!();
        };
        assert_eq!(tally.moved, ["a.txt", "b.txt"]);
    }

    #[test]
    fn skipping_a_clash_records_it_and_moves_on() {
        let mut job = job(vec![
            remote("a.txt", EntryKind::File),
            remote("b.txt", EntryKind::File),
        ]);
        job.advance(&none);
        job.on_step(
            StepResult::Exists {
                target_is_dir: false,
            },
            &none,
        );
        let next = job.answer(Choice::Skip, false, &none);
        assert_eq!(attempt(&next).0, "b.txt");
        // 没勾“都这样做”：下一个同名还会问。
        assert!(matches!(
            job.on_step(
                StepResult::Exists {
                    target_is_dir: false
                },
                &none
            ),
            Next::Ask(_)
        ));
    }

    #[test]
    fn apply_to_the_rest_answers_later_clashes_without_asking() {
        let mut job = job(vec![
            remote("a", EntryKind::File),
            remote("b", EntryKind::File),
            remote("c", EntryKind::File),
        ]);
        job.advance(&none);
        let Next::Ask(ask) = job.on_step(
            StepResult::Exists {
                target_is_dir: false,
            },
            &none,
        ) else {
            panic!();
        };
        assert_eq!(ask.remaining, 2);
        // 对其余都“替换”。
        let b = job.answer(Choice::Replace, true, &none);
        assert_eq!(attempt(&b), ("a", "/srv/dest/a", true));
        let b = job.on_step(StepResult::Moved, &none);
        assert_eq!(attempt(&b), ("b", "/srv/dest/b", false));
        let c_overwrite = job.on_step(
            StepResult::Exists {
                target_is_dir: false,
            },
            &none,
        );
        assert_eq!(
            attempt(&c_overwrite),
            ("b", "/srv/dest/b", true),
            "不再问，直接替换"
        );
        let c = job.on_step(StepResult::Moved, &none);
        assert_eq!(attempt(&c).0, "c");
    }

    #[test]
    fn skip_all_records_each_clash_as_skipped() {
        let mut job = job(vec![
            remote("a", EntryKind::File),
            remote("b", EntryKind::File),
        ]);
        job.advance(&none);
        job.on_step(
            StepResult::Exists {
                target_is_dir: false,
            },
            &none,
        );
        job.answer(Choice::Skip, true, &none);
        let Next::Done(tally) = job.on_step(
            StepResult::Exists {
                target_is_dir: false,
            },
            &none,
        ) else {
            panic!();
        };
        assert_eq!(tally.skipped.len(), 2);
        assert!(tally.moved.is_empty());
    }

    #[test]
    fn folders_are_never_merged_or_replaced_only_skipped() {
        // 文件夹 ↔ 同名文件夹。
        let mut j = job(vec![remote("d", EntryKind::Dir)]);
        j.advance(&none);
        let Next::Ask(ask) = j.on_step(
            StepResult::Exists {
                target_is_dir: true,
            },
            &none,
        ) else {
            panic!();
        };
        assert!(!ask.can_replace);
        assert_eq!(ask.why_not.as_deref(), Some("同名文件夹不会合并，只能跳过"));
        // 用户硬点“替换”也只会跳过，不会发出覆盖式移动。
        let Next::Done(tally) = j.answer(Choice::Replace, false, &none) else {
            panic!("不能替换文件夹，应该直接跳过并做完");
        };
        assert_eq!(tally.skipped.len(), 1);
        assert!(tally.moved.is_empty());

        // 文件 → 同名文件夹、文件夹 → 同名文件：都不能替换。
        for (source, target_is_dir, why) in [
            (
                EntryKind::File,
                true,
                "同名的是文件夹，不能用文件替换，只能跳过",
            ),
            (
                EntryKind::Dir,
                false,
                "同名的是文件，不能用文件夹替换，只能跳过",
            ),
        ] {
            let mut j = job(vec![remote("x", source)]);
            j.advance(&none);
            let Next::Ask(ask) = j.on_step(StepResult::Exists { target_is_dir }, &none) else {
                panic!();
            };
            assert!(!ask.can_replace);
            assert_eq!(ask.why_not.as_deref(), Some(why));
        }
    }

    #[test]
    fn replace_all_still_skips_folder_clashes() {
        let mut job = job(vec![
            remote("a", EntryKind::File),
            remote("dir", EntryKind::Dir),
            remote("b", EntryKind::File),
        ]);
        job.advance(&none);
        job.on_step(
            StepResult::Exists {
                target_is_dir: false,
            },
            &none,
        );
        job.answer(Choice::Replace, true, &none);
        job.on_step(StepResult::Moved, &none); // a 替换完成，轮到 dir
        let next = job.on_step(
            StepResult::Exists {
                target_is_dir: true,
            },
            &none,
        );
        assert_eq!(attempt(&next).0, "b", "文件夹冲突被跳过，继续 b");
        let Next::Done(tally) = job.on_step(StepResult::Moved, &none) else {
            panic!();
        };
        assert_eq!(tally.moved, ["a", "b"]);
        assert_eq!(tally.skipped[0].name, "dir");
    }

    #[test]
    fn a_failed_replace_is_a_failure_and_the_run_goes_on() {
        let mut job = job(vec![
            remote("a", EntryKind::File),
            remote("b", EntryKind::File),
        ]);
        job.advance(&none);
        job.on_step(
            StepResult::Exists {
                target_is_dir: false,
            },
            &none,
        );
        job.answer(Choice::Replace, false, &none);
        let next = job.on_step(StepResult::Failed("没有权限".to_owned()), &none);
        assert_eq!(attempt(&next).0, "b");
        let Next::Done(tally) = job.on_step(StepResult::Moved, &none) else {
            panic!();
        };
        assert_eq!(tally.failed[0].name, "a");
        assert_eq!(tally.moved, ["b"]);
    }

    #[test]
    fn aborting_skips_the_pending_item_and_everything_after_it() {
        let mut job = job(vec![
            remote("a", EntryKind::File),
            remote("b", EntryKind::File),
            remote("c", EntryKind::File),
        ]);
        job.advance(&none);
        job.on_step(StepResult::Moved, &none);
        job.on_step(
            StepResult::Exists {
                target_is_dir: false,
            },
            &none,
        );
        let Next::Done(tally) = job.abort() else {
            panic!();
        };
        assert_eq!(tally.moved, ["a"]);
        let skipped: Vec<&str> = tally.skipped.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(skipped, ["b", "c"]);
        assert!(tally.skipped.iter().all(|n| n.reason == "已取消"));
    }

    #[test]
    fn a_stray_result_while_asking_changes_nothing() {
        let mut job = job(vec![remote("a", EntryKind::File)]);
        job.advance(&none);
        job.on_step(
            StepResult::Exists {
                target_is_dir: false,
            },
            &none,
        );
        // 问着的时候又来一个结果（不该发生）：仍然是同一个问题。
        assert!(matches!(
            job.on_step(StepResult::Moved, &none),
            Next::Ask(_)
        ));
        assert!(matches!(
            job.answer(Choice::Replace, false, &none),
            Next::Attempt {
                overwrite: true,
                ..
            }
        ));
    }

    #[cfg(windows)]
    #[test]
    fn local_jobs_build_windows_targets() {
        let mut job = MoveJob::new(
            2,
            Side::Local,
            "D:\\out".to_owned(),
            vec![local("报告 1.txt", EntryKind::File)],
        );
        let next = job.advance(&none);
        assert_eq!(attempt(&next), ("报告 1.txt", "D:\\out\\报告 1.txt", false));
    }
    #[cfg(not(windows))]
    #[test]
    fn local_posix_paths_validate_compare_and_build_jobs() {
        let files = [entry(Side::Local, "/work", "报告 1.txt", EntryKind::File)];
        assert_eq!(
            check_destination(Side::Local, &files, "/work/"),
            DestCheck::SameFolder
        );
        assert_eq!(
            check_destination(Side::Local, &files, "/out/"),
            DestCheck::Ok("/out".into())
        );
        assert!(matches!(
            check_destination(Side::Local, &files, "relative"),
            DestCheck::Invalid(_)
        ));
        let dirs = [entry(Side::Local, "/work", "data", EntryKind::Dir)];
        for dest in ["/work/data", "/work/data/sub", "/work/data/./sub/.."] {
            assert_eq!(
                check_destination(Side::Local, &dirs, dest),
                DestCheck::IntoSelf
            );
        }
        assert!(check_destination(Side::Local, &dirs, "/work/data2").is_ok());
        assert!(!super::same_path(Side::Local, "/work/Data", "/work/data"));
        assert!(same_or_inside(Side::Local, "/work/data/sub", "/work/data"));
        assert!(!same_or_inside(Side::Local, "/work/data2", "/work/data"));
        assert!(super::transfer_blocks(
            Side::Local,
            "/work/data",
            super::Direction::Upload,
            "/work/data/a",
            "/remote"
        ));
        assert_eq!(target_path(Side::Local, "/", "a.txt"), "/a.txt");
        assert_eq!(target_path(Side::Remote, "/srv/", "a.txt"), "/srv/a.txt");
        let mut job = MoveJob::new(2, Side::Local, "/out".into(), files.to_vec());
        assert_eq!(
            attempt(&job.advance(&none)),
            ("报告 1.txt", "/out/报告 1.txt", false)
        );
    }
}
