//! 文件栏上的动作：菜单项分派、对话框的打开和确认、新建 / 改名 / 删除 / 改权限、
//! 属性里的“计算大小”、远端压缩 / 解压。
//!
//! 每个动作最后都落到和自动化钩子共用的 `start_*` 函数上（后台任务 + `SftpEvent` 回界面），
//! 所以按钮、快捷键、菜单、钩子走的是同一条路。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ::gpui::{ClipboardItem, PathPromptOptions, Window};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::*;
use zssh_core::sftp::SftpError;
use zssh_core::sftp::archive::{
    ArchiveCaps, ArchiveError, ArchiveFormat, ArchiveProgress, CompressFormat, CompressOutcome,
    CompressRequest, ConflictPolicy, ExtractOutcome, ExtractPlan, ExtractRequest, compress,
    extract, extract_folder_name, plan_extract, try_probe_caps,
};
use zssh_core::sftp::fs::RecursiveDelete;
use zssh_core::sftp::path::file_name;
use zssh_core::sftp::size::{SizeTotals, local_size, remote_size};
use zssh_core::sftp::transfer::Direction;

use super::archive_ui::{self, BusyKind};
use super::chmod::recursive_chmod;
use super::dialogs::{
    DeleteDialog, Dialog, DownloadDialog, ExtractAsk, ExtractTarget, NameCheck, NameDialog,
    NameKind, OverwriteAsk, PermDialog, PropDialog, delete_subject, validate_local_dir,
    validate_name,
};
use super::engine::{DeleteJob, parse_octal_mode, run_delete_many};
use super::local_listing::{ScanError, scan};
use super::menu::{ArchiveMenuState, MenuAction, MenuContext, MenuFeatures, Side};
use super::model::{Entry, EntryKind};
use super::props::{
    CalcPhase, CalcState, PermMatrix, can_calculate, initial_permission_bits, octal_text,
    properties_targets,
};
use super::{ArchiveCapsState, Busy, RemoteOp, SftpDemo, SftpEvent, SftpMsg, local_ops};
use crate::error_text::error_text;
use crate::i18n::t;
use crate::ssh_backend::runtime;

/// 进度事件最多这么久发一次。
const REPORT_INTERVAL: Duration = Duration::from_millis(150);

/// 本地新建 / 改名的状态栏动词（开始 / 完成 / 失败三种形式）。
#[derive(Clone, Copy)]
enum LocalOpVerb {
    NewFolder,
    NewFile,
    Rename,
}

impl LocalOpVerb {
    fn start(self) -> &'static str {
        match self {
            Self::NewFolder => t(SftpMsg::OpsStartNewFolder),
            Self::NewFile => t(SftpMsg::OpsStartNewFile),
            Self::Rename => t(SftpMsg::OpsStartRename),
        }
    }

    fn done(self) -> &'static str {
        match self {
            Self::NewFolder => t(SftpMsg::OpsDoneNewFolder),
            Self::NewFile => t(SftpMsg::OpsDoneNewFile),
            Self::Rename => t(SftpMsg::OpsDoneRename),
        }
    }

    fn fail(self) -> &'static str {
        match self {
            Self::NewFolder => t(SftpMsg::OpsFailNewFolder),
            Self::NewFile => t(SftpMsg::OpsFailNewFile),
            Self::Rename => t(SftpMsg::OpsFailRename),
        }
    }
}

/// 临时文件名里的任务标识：一次操作一个，不会和别的任务撞。
fn make_owner() -> String {
    format!(
        "a{:x}{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    )
}

/// 能不能用这些命令压缩 / 解压：不是 POSIX shell 或缺命令就给出要告诉用户的原因。
fn require_tools(caps: &ArchiveCaps, tar: bool, zip: bool, unzip: bool) -> Result<(), String> {
    if !caps.posix {
        return Err(t(SftpMsg::OpsShellNotPosix).to_owned());
    }
    if tar && !caps.tar {
        return Err(t(SftpMsg::MenuNoTar).to_owned());
    }
    if zip && !caps.zip {
        return Err(t(SftpMsg::MenuNoZip).to_owned());
    }
    if unzip && !caps.unzip {
        return Err(t(SftpMsg::MenuNoUnzip).to_owned());
    }
    Ok(())
}

impl SftpDemo {
    // ---- 右键菜单 ----

    /// 服务器上的压缩能力在菜单里的样子：没探测过（探测失败）不显示这一组。
    fn archive_menu_state(&self) -> Option<ArchiveMenuState> {
        match self.archive_caps {
            ArchiveCapsState::Idle => None,
            ArchiveCapsState::Loading => Some(ArchiveMenuState::Pending),
            ArchiveCapsState::Ready(caps) => Some(ArchiveMenuState::Ready(caps)),
        }
    }

    /// 右键菜单打开的那一刻，这一栏的状态（点在条目上 / 空白处、选了哪些）。
    pub(super) fn menu_context(&self, side: Side) -> MenuContext {
        let pane = self.pane(side);
        // 右键点在空白处会清掉选择，点在行上会选中那一行：没有选择就是“空白处”的菜单。
        let entries = pane.selected();
        let blank = entries.is_empty();
        MenuContext {
            side,
            blank,
            entries,
            current_dir: pane.path.clone(),
            clipboard: super::clip::summary(),
            features: MenuFeatures {
                builtin_editor: side == Side::Remote,
                clipboard: true,
                properties: true,
            },
            archive: if side == Side::Remote {
                self.archive_menu_state()
            } else {
                None
            },
        }
    }

    /// 第一次右键远端时问服务器有没有 tar / zip / unzip（之后用缓存；没问成下次再试）。
    pub(super) fn ensure_archive_caps(&mut self, _cx: &mut Context<Self>) {
        if self.archive_caps != ArchiveCapsState::Idle {
            return;
        }
        let (Some(factory), Ok(runtime)) = (self.factory.clone(), runtime()) else {
            return;
        };
        self.archive_caps = ArchiveCapsState::Loading;
        let sender = self.sender.clone();
        runtime.spawn(async move {
            let caps = try_probe_caps(factory.connection().as_ref()).await;
            let _ = sender.send(SftpEvent::ArchiveCaps(caps));
        });
    }

    /// 菜单项被点了：`entries` / `blank` 是菜单打开那一刻的选择。
    pub(super) fn run_menu_action(
        &mut self,
        side: Side,
        action: MenuAction,
        entries: Vec<Entry>,
        blank: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            MenuAction::Open => {
                if let [entry] = entries.as_slice() {
                    self.activate(side, entry, window, cx);
                }
            }
            MenuAction::EditBuiltin => {
                if let ([entry], Side::Remote) = (entries.as_slice(), side)
                    && let Err(err) = self.start_edit(entry.path.clone(), cx)
                {
                    self.warn(err);
                }
            }
            MenuAction::Transfer => self.transfer_entries(side, &entries, None),
            MenuAction::DownloadTo => self.open_download_to(entries, window, cx),
            MenuAction::Copy => self.copy_selection(side, &entries),
            MenuAction::Paste => {
                // 右键在文件夹上粘贴 = 粘贴进那个文件夹（和资源管理器一致）；其余都是当前目录。
                match entries.as_slice() {
                    [entry] if !blank && entry.kind == EntryKind::Dir => {
                        self.paste_into(side, entry.path.clone(), cx)
                    }
                    _ => self.paste_here(side, cx),
                }
            }
            MenuAction::CopyPath => {
                let text = entries
                    .iter()
                    .map(|entry| entry.path.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                self.say(t(SftpMsg::OpsPathCopied));
            }
            MenuAction::Rename => {
                if let [entry] = entries.as_slice() {
                    self.open_rename(side, entry.clone(), window, cx);
                }
            }
            MenuAction::MoveTo => self.open_move_to(side, entries, window, cx),
            MenuAction::Delete => self.ask_delete(side, entries, window, cx),
            MenuAction::Mkdir => self.open_name_dialog(NameKind::Mkdir, side, None, window, cx),
            MenuAction::Create => self.open_name_dialog(NameKind::Create, side, None, window, cx),
            MenuAction::Permissions => {
                if let ([entry], Side::Remote) = (entries.as_slice(), side) {
                    self.open_permissions(entry.clone(), window, cx);
                }
            }
            MenuAction::Refresh => self.refresh(side, cx),
            MenuAction::Properties => {
                let entries = if blank { Vec::new() } else { entries };
                self.open_properties(side, entries, window, cx);
            }
            MenuAction::CompressTarGz if side == Side::Remote => {
                self.start_compress(&entries, CompressFormat::TarGz)
            }
            MenuAction::CompressZip if side == Side::Remote => {
                self.start_compress(&entries, CompressFormat::Zip)
            }
            MenuAction::ExtractFolder if side == Side::Remote => {
                if let [entry] = entries.as_slice() {
                    self.start_extract_check(entry, true);
                }
            }
            MenuAction::ExtractHere if side == Side::Remote => {
                if let [entry] = entries.as_slice() {
                    self.start_extract_check(entry, false);
                }
            }
            // 传输队列里一行的菜单在 `transfer.rs`；别的栏不会有压缩 / 解压。
            _ => {}
        }
        cx.notify();
    }

    // ---- 对话框：打开 / 关闭 ----

    fn dialog_input(
        &mut self,
        initial: &str,
        placeholder: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        input.update(cx, |state, cx| {
            state.set_value(initial.to_owned(), window, cx)
        });
        input
    }

    /// 把键盘交给对话框里的输入框，全选（改名时直接输入就是替换）。
    fn focus_dialog_input(input: &Entity<InputState>, window: &mut Window, cx: &mut Context<Self>) {
        input.update(cx, |state, cx| {
            state.focus(window, cx);
            state.select_all(window, cx);
        });
    }

    pub(super) fn close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_property_calcs();
        // 撤销类的对话框取消了：状态栏别还停在“正在……”上。
        match self.dialog.take() {
            Some(Dialog::OverwriteRename(ask)) => {
                self.say(format!(
                    "{}{}{}",
                    t(SftpMsg::OpsCancelNoOverwritePrefix),
                    ask.name,
                    t(SftpMsg::OpsCancelNoOverwriteSuffix)
                ));
            }
            Some(Dialog::Delete(dialog)) => {
                self.say(format!(
                    "{}{}",
                    t(SftpMsg::DeleteCancelledPrefix),
                    delete_subject(&dialog.entries)
                ));
            }
            // 同名询问上按 Esc / 点遮罩 = 取消移动：这一项和后面没处理的都不动。
            Some(Dialog::MoveConflict(conflict)) => {
                self.dialog = Some(Dialog::MoveConflict(conflict));
                self.abort_move(window, cx);
                return;
            }
            _ => {}
        }
        let side = self.focused_side;
        self.focus_pane(side, window, cx);
        cx.notify();
    }

    /// 对话框上按了 Enter。删除 / 覆盖这类不能撤销的不响应 Enter，必须点按钮。
    pub(super) fn dialog_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.dialog {
            Some(Dialog::Name(_)) => self.confirm_name(window, cx),
            Some(Dialog::Permissions(_)) => self.confirm_permissions(window, cx),
            Some(Dialog::DownloadTo(_)) => self.confirm_download_to(window, cx),
            Some(Dialog::Properties(_) | Dialog::MoveSummary(_)) => self.close_dialog(window, cx),
            Some(Dialog::MoveTo(_)) => self.move_enter(window, cx),
            Some(Dialog::ExtractConflict(_)) => {
                self.choose_extract(ConflictPolicy::SkipExisting, window, cx)
            }
            Some(Dialog::Delete(_) | Dialog::OverwriteRename(_) | Dialog::MoveConflict(_))
            | None => {}
        }
    }

    // ---- 新建 / 重命名 ----

    pub(super) fn open_rename(
        &mut self,
        side: Side,
        entry: Entry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !entry.name_is_utf8 {
            self.warn(super::menu::not_utf8_reason());
            return;
        }
        if entry.is_drive() {
            self.warn(t(SftpMsg::MenuDriveNotAllowed));
            return;
        }
        self.open_name_dialog(NameKind::Rename, side, Some(entry), window, cx);
    }

    pub(super) fn open_name_dialog(
        &mut self,
        kind: NameKind,
        side: Side,
        from: Option<Entry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self.pane(side);
        if kind != NameKind::Rename && pane.path.is_empty() {
            self.warn(match side {
                Side::Local => t(SftpMsg::MenuEnterFolder),
                Side::Remote => t(SftpMsg::MenuEnterDir),
            });
            return;
        }
        let existing: Vec<String> = pane
            .raw
            .iter()
            .filter(|entry| from.as_ref().is_none_or(|own| own.path != entry.path))
            .map(|entry| entry.name.clone())
            .collect();
        let dir = pane.path.clone();
        let initial = from.as_ref().map_or("", |entry| entry.name.as_str());
        let placeholder = match kind {
            NameKind::Mkdir => t(SftpMsg::OpsNameFolder),
            NameKind::Create => t(SftpMsg::OpsNameFile),
            NameKind::Rename => t(SftpMsg::OpsNameNewName),
        };
        let input = self.dialog_input(initial, placeholder, window, cx);
        let subscriptions = vec![
            cx.observe(&input, |_this, _input, cx| cx.notify()),
            cx.subscribe_in(
                &input,
                window,
                |this, _input, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.confirm_name(window, cx);
                    }
                },
            ),
        ];
        Self::focus_dialog_input(&input, window, cx);
        self.dialog = Some(Dialog::Name(NameDialog {
            kind,
            side,
            dir,
            from,
            input,
            existing,
            _subscriptions: subscriptions,
        }));
        cx.notify();
    }

    fn confirm_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::Name(dialog)) = &self.dialog else {
            return;
        };
        let name = dialog.input.read(cx).value().trim().to_owned();
        // 名字不合格：对话框里已经写着原因，确认键不响应。
        if validate_name(&name, dialog.side) != NameCheck::Ok {
            return;
        }
        let (kind, side, dir, from) = (
            dialog.kind,
            dialog.side,
            dialog.dir.clone(),
            dialog.from.clone(),
        );
        // 改成和原来一样的名字：什么也不用做。
        if kind == NameKind::Rename && from.as_ref().is_some_and(|f| f.name == name) {
            self.close_dialog(window, cx);
            return;
        }
        self.dialog = None;
        let result = match (side, kind) {
            (Side::Remote, NameKind::Mkdir) => self.start_mkdir(&name),
            (Side::Remote, NameKind::Create) => self.start_create(&name),
            (Side::Remote, NameKind::Rename) => match &from {
                Some(entry) => self.start_rename_interactive(&entry.path, &name),
                None => Ok(()),
            },
            (Side::Local, NameKind::Mkdir) => {
                let path = PathBuf::from(&dir).join(&name).display().to_string();
                self.start_local_op(LocalOpVerb::NewFolder, &name, move || {
                    local_ops::mkdir(&path)
                });
                Ok(())
            }
            (Side::Local, NameKind::Create) => {
                let path = PathBuf::from(&dir).join(&name).display().to_string();
                self.start_local_op(LocalOpVerb::NewFile, &name, move || {
                    local_ops::create_file(&path)
                });
                Ok(())
            }
            (Side::Local, NameKind::Rename) => match &from {
                Some(entry) => {
                    let to = PathBuf::from(&dir).join(&name).display().to_string();
                    let from_path = entry.path.clone();
                    let shown = format!("{} → {name}", entry.name);
                    self.start_local_op(LocalOpVerb::Rename, &shown, move || {
                        local_ops::rename(&from_path, &to)
                    });
                    Ok(())
                }
                None => Ok(()),
            },
        };
        if let Err(err) = result {
            self.warn(err);
        }
        self.focus_pane(side, window, cx);
        cx.notify();
    }

    /// 本地新建 / 改名：写盘可能卡在慢盘、断开的映射盘上，放到阻塞线程里做，
    /// 做完以事件回填（和本地删除同一条路径），界面线程不碰文件系统。
    fn start_local_op<F>(&mut self, verb: LocalOpVerb, subject: &str, work: F)
    where
        F: FnOnce() -> Result<(), String> + Send + 'static,
    {
        let Ok(runtime) = runtime() else {
            self.warn(t(SftpMsg::OpsRuntimeUnavailable).to_owned());
            return;
        };
        self.say(format!("{}{subject}", verb.start()));
        let sender = self.sender.clone();
        let done = format!("{}{subject}", verb.done());
        let failed = format!("{}{subject}{}", verb.fail(), t(SftpMsg::OpsFailedSuffix));
        runtime.spawn(async move {
            let event = match tokio::task::spawn_blocking(work).await {
                Ok(Ok(())) => SftpEvent::OpDone {
                    ok: true,
                    message: done,
                    refresh: Some(Side::Local),
                },
                Ok(Err(err)) => SftpEvent::OpDone {
                    ok: false,
                    message: format!("{failed}{}{err}", t(SftpMsg::OpsErrorJoin)),
                    refresh: Some(Side::Local),
                },
                Err(err) => SftpEvent::OpDone {
                    ok: false,
                    message: format!(
                        "{failed}{}{}{err}{}",
                        t(SftpMsg::OpsErrorJoin),
                        t(SftpMsg::OpsThreadEndedPrefix),
                        t(SftpMsg::ParenClose)
                    ),
                    refresh: Some(Side::Local),
                },
            };
            let _ = sender.send(event);
        });
    }

    /// 新建文件（对话框和钩子共用）。不覆盖：先看一眼，已经有同名的就报错（核心的 `create_file`
    /// 会截断已有文件，0.2.14 的对话框只写了一句“确定会覆盖 / 失败”）。
    pub(super) fn start_create(&mut self, raw_name: &str) -> Result<(), String> {
        let channel = self.connected_channel()?;
        let target = super::engine::mkdir_target(&self.remote.path, raw_name)?;
        self.say(format!("{}{target}", t(SftpMsg::OpsCreatingFilePrefix)));
        self.run_remote_op(
            async move {
                match channel.lstat(&target).await {
                    Ok(_) => {
                        return Err(format!(
                            "{}{target}{}{}{}",
                            t(SftpMsg::OpsCreateFileFailedPrefix),
                            t(SftpMsg::ParenOpen),
                            t(SftpMsg::AlreadyExists),
                            t(SftpMsg::ParenClose)
                        ));
                    }
                    Err(SftpError::NotFound { .. }) => {}
                    Err(err) => {
                        return Err(format!(
                            "{}{target}{}{}{}",
                            t(SftpMsg::OpsCreateFileFailedPrefix),
                            t(SftpMsg::ParenOpen),
                            error_text(&err),
                            t(SftpMsg::ParenClose)
                        ));
                    }
                }
                channel.create_file(&target).await.map_err(|err| {
                    format!(
                        "{}{target}{}{}{}",
                        t(SftpMsg::OpsCreateFileFailedPrefix),
                        t(SftpMsg::ParenOpen),
                        error_text(&err),
                        t(SftpMsg::ParenClose)
                    )
                })?;
                Ok(format!("{}{target}", t(SftpMsg::OpsCreatedFilePrefix)))
            },
            Some(Side::Remote),
        );
        Ok(())
    }

    /// "新建目录"（对话框和钩子共用这一条）。
    pub(super) fn start_mkdir(&mut self, raw_name: &str) -> Result<(), String> {
        let channel = self.connected_channel()?;
        let target = super::engine::mkdir_target(&self.remote.path, raw_name)?;
        self.say(format!("{}{target}", t(SftpMsg::OpsCreatingDirPrefix)));
        self.run_remote_op(
            async move { super::engine::create_remote_dir(&channel, &target).await },
            Some(Side::Remote),
        );
        Ok(())
    }

    /// 排一个改名（自动化钩子用）：不覆盖，新名字已经存在就让服务器报错。
    pub(super) fn start_rename(
        &mut self,
        old_path: &str,
        raw_new_name: &str,
    ) -> Result<(), String> {
        self.rename_remote(old_path, raw_new_name, false)
    }

    /// 排一个改名（对话框用）：不覆盖地试一次；服务器说“已存在”才问用户要不要覆盖。
    fn start_rename_interactive(
        &mut self,
        old_path: &str,
        raw_new_name: &str,
    ) -> Result<(), String> {
        self.rename_remote(old_path, raw_new_name, true)
    }

    fn rename_remote(
        &mut self,
        old_path: &str,
        raw_new_name: &str,
        ask_on_exists: bool,
    ) -> Result<(), String> {
        let channel = self.connected_channel()?;
        let target = super::engine::rename_target(old_path, raw_new_name)?;
        let runtime = runtime()?;
        let old_path = old_path.to_owned();
        let old_name = file_name(&old_path);
        let new_name = file_name(&target);
        self.say(format!(
            "{}{old_name} → {new_name}",
            t(SftpMsg::OpsRenamingPrefix)
        ));
        let sender = self.sender.clone();
        runtime.spawn(async move {
            let event = match channel.rename(&old_path, &target, false).await {
                Ok(_) => SftpEvent::OpDone {
                    ok: true,
                    message: format!("{}{old_name} → {new_name}", t(SftpMsg::OpsRenamedPrefix)),
                    refresh: Some(Side::Remote),
                },
                Err(SftpError::AlreadyExists { .. }) if ask_on_exists => SftpEvent::RenameExists {
                    from: old_path,
                    to: target,
                    name: new_name,
                },
                Err(err) => SftpEvent::OpDone {
                    ok: false,
                    message: format!(
                        "{}{old_name}{}{}{}",
                        t(SftpMsg::OpsRenameFailedPrefix),
                        t(SftpMsg::ParenOpen),
                        error_text(&err),
                        t(SftpMsg::ParenClose)
                    ),
                    refresh: None,
                },
            };
            let _ = sender.send(event);
        });
        Ok(())
    }

    /// 改名撞上同名文件：问“覆盖服务器上的同名文件？”。
    pub(super) fn ask_overwrite_rename(
        &mut self,
        from: String,
        to: String,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dialog = Some(Dialog::OverwriteRename(OverwriteAsk { from, to, name }));
        window.focus(&self.dialog_focus, cx);
        cx.notify();
    }

    pub(super) fn confirm_overwrite_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::OverwriteRename(ask)) = self.dialog.take() else {
            return;
        };
        let channel = match self.connected_channel() {
            Ok(channel) => channel,
            Err(err) => {
                self.warn(err);
                return;
            }
        };
        let old_name = file_name(&ask.from);
        self.say(format!(
            "{}{old_name} → {}",
            t(SftpMsg::OpsOverwriteRenamingPrefix),
            ask.name
        ));
        self.run_remote_op(
            async move {
                channel
                    .rename(&ask.from, &ask.to, true)
                    .await
                    .map_err(|err| {
                        format!(
                            "{}{old_name}{}{}{}",
                            t(SftpMsg::OpsRenameFailedPrefix),
                            t(SftpMsg::ParenOpen),
                            error_text(&err),
                            t(SftpMsg::ParenClose)
                        )
                    })?;
                Ok(format!(
                    "{}{old_name} → {}",
                    t(SftpMsg::OpsRenamedOverwritePrefix),
                    ask.name
                ))
            },
            Some(Side::Remote),
        );
        self.focus_pane(Side::Remote, window, cx);
        cx.notify();
    }

    // ---- 删除 ----

    /// 删除：弹确认对话框（写明名字和“将递归删除其中所有内容”），点“删除”才开始。
    pub(super) fn ask_delete(
        &mut self,
        side: Side,
        entries: Vec<Entry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let operable: Vec<Entry> = entries
            .iter()
            .filter(|entry| entry.name_is_utf8 && !entry.is_drive())
            .cloned()
            .collect();
        if operable.is_empty() {
            self.warn(if entries.is_empty() {
                t(SftpMsg::MenuSelectOne)
            } else {
                super::menu::not_utf8_reason()
            });
            cx.notify();
            return;
        }
        if side == Side::Remote
            && let Some(job) = &self.deleting
        {
            let text = format!(
                "{}{}{}",
                t(SftpMsg::OpsDeletingWaitPrefix),
                job.name,
                t(SftpMsg::OpsDeletingWaitSuffix)
            );
            self.warn(text);
            cx.notify();
            return;
        }
        if operable.len() < entries.len() {
            self.warn(t(SftpMsg::OpsSkippedNonUtf8));
        }
        self.dialog = Some(Dialog::Delete(DeleteDialog {
            side,
            entries: operable,
        }));
        window.focus(&self.dialog_focus, cx);
        cx.notify();
    }

    pub(super) fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::Delete(dialog)) = self.dialog.take() else {
            return;
        };
        let paths: Vec<String> = dialog.entries.iter().map(|e| e.path.clone()).collect();
        let subject = delete_subject(&dialog.entries);
        let result = match dialog.side {
            Side::Remote => self.start_delete_many(paths, subject),
            Side::Local => self.start_local_delete(paths, subject),
        };
        if let Err(err) = result {
            self.warn(err);
        }
        self.focus_pane(dialog.side, window, cx);
        cx.notify();
    }

    /// 排一个删除（自动化钩子用）。
    pub(super) fn start_delete(&mut self, path: &str) -> Result<(), String> {
        self.start_delete_many(vec![path.to_owned()], file_name(path))
    }

    /// 远端删除（可以一次多项）：核心的递归删除一项接一项走，后台逐项上报进度，
    /// 取消标志置位后在下一项之前停下。同一时间只跑一个删除。
    fn start_delete_many(&mut self, paths: Vec<String>, subject: String) -> Result<(), String> {
        if let Some(job) = &self.deleting {
            return Err(format!(
                "{}{}{}",
                t(SftpMsg::OpsDeletingWaitPrefix),
                job.name,
                t(SftpMsg::OpsDeletingWaitNextSuffix)
            ));
        }
        let channel = self.connected_channel()?;
        let runtime = runtime()?;
        let id = self.next_delete;
        self.next_delete += 1;
        let deleter = RecursiveDelete::new();
        self.deleting = Some(DeleteJob {
            id,
            name: subject.clone(),
            cancel: deleter.cancel_handle(),
            deleted: 0,
            current: String::new(),
            cancelling: false,
        });
        self.say(format!("{}{subject}", t(SftpMsg::OpsDeletingPrefix)));
        let sender = self.sender.clone();
        runtime.spawn(async move {
            let mut last_sent = Instant::now();
            let progress_sender = sender.clone();
            let outcome = run_delete_many(&channel, &paths, &deleter, move |progress| {
                // 每项都上报会淹没事件通道：至少隔 100ms 或者每 50 项发一次。
                if progress.deleted.is_multiple_of(50)
                    || last_sent.elapsed() >= Duration::from_millis(100)
                {
                    last_sent = Instant::now();
                    let _ = progress_sender.send(SftpEvent::DeleteProgress {
                        id,
                        deleted: progress.deleted,
                        current: progress.current,
                    });
                }
            })
            .await;
            let _ = sender.send(SftpEvent::DeleteDone { id, outcome });
        });
        Ok(())
    }

    /// 本地删除：目录可能很大，放到阻塞线程里做；链接只删链接。
    fn start_local_delete(&mut self, paths: Vec<String>, subject: String) -> Result<(), String> {
        let runtime = runtime()?;
        self.say(format!("{}{subject}", t(SftpMsg::OpsDeletingLocalPrefix)));
        let sender = self.sender.clone();
        runtime.spawn(async move {
            let results = tokio::task::spawn_blocking(move || local_ops::delete(&paths)).await;
            let event = match results {
                Ok(results) => {
                    let failed: Vec<&(String, Option<String>)> =
                        results.iter().filter(|(_, err)| err.is_some()).collect();
                    if failed.is_empty() {
                        SftpEvent::OpDone {
                            ok: true,
                            message: format!("{}{subject}", t(SftpMsg::OpsDeletedLocalPrefix)),
                            refresh: Some(Side::Local),
                        }
                    } else {
                        // 只报第一个会把"哪几项没删掉"丢掉：列前三个，剩下的说总数。
                        let mut shown: Vec<String> = failed
                            .iter()
                            .take(3)
                            .map(|(path, err)| {
                                format!(
                                    "{}{}{}{}",
                                    file_name(path),
                                    t(SftpMsg::ParenOpen),
                                    err.as_deref().unwrap_or(t(SftpMsg::OpsUnknownReason)),
                                    t(SftpMsg::ParenClose)
                                )
                            })
                            .collect();
                        if failed.len() > shown.len() {
                            shown.push(format!(
                                "{}{}{}",
                                t(SftpMsg::OpsMoreFailuresPrefix),
                                failed.len(),
                                t(SftpMsg::OpsMoreFailuresSuffix)
                            ));
                        }
                        SftpEvent::OpDone {
                            ok: false,
                            message: format!(
                                "{}{}",
                                t(SftpMsg::DeleteFailedPrefix),
                                shown.join(t(SftpMsg::OpsListSeparator))
                            ),
                            refresh: Some(Side::Local),
                        }
                    }
                }
                Err(err) => SftpEvent::OpDone {
                    ok: false,
                    message: format!("{}{err}", t(SftpMsg::OpsDeleteThreadFailedPrefix)),
                    refresh: Some(Side::Local),
                },
            };
            let _ = sender.send(event);
        });
        Ok(())
    }

    /// 「取消删除」：置取消标志，后台在下一项之前停下（已删的不回滚）。
    pub(super) fn cancel_delete(&mut self, cx: &mut Context<Self>) {
        if let Some(job) = self.deleting.as_mut() {
            job.cancel.cancel();
            job.cancelling = true;
            let text = format!(
                "{}{}{}",
                t(SftpMsg::OpsCancellingDeletePrefix),
                job.name,
                t(SftpMsg::OpsEllipsis)
            );
            self.say(text);
            cx.notify();
        }
    }

    // ---- 修改权限 ----

    pub(super) fn open_permissions(
        &mut self,
        entry: Entry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !entry.name_is_utf8 {
            self.warn(super::menu::not_utf8_reason());
            return;
        }
        let bits = initial_permission_bits(entry.permissions);
        let matrix = PermMatrix::from_mode(bits).unwrap_or_default();
        let octal = self.dialog_input(
            &octal_text(bits),
            t(SftpMsg::OpsOctalPlaceholder),
            window,
            cx,
        );
        let subscriptions = vec![
            cx.observe(&octal, |_this, _input, cx| cx.notify()),
            cx.subscribe_in(
                &octal,
                window,
                |this, input, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => {
                        // 八进制框里敲出合法的值：勾选框跟着变；敲到一半不合法就先不动。
                        let text = input.read(cx).value().to_string();
                        if let Ok(mode) = parse_octal_mode(&text)
                            && let Some(Dialog::Permissions(dialog)) = &mut this.dialog
                            && let Some(matrix) = PermMatrix::from_mode(mode)
                        {
                            dialog.matrix = matrix;
                            cx.notify();
                        }
                    }
                    InputEvent::PressEnter { .. } => this.confirm_permissions(window, cx),
                    _ => {}
                },
            ),
        ];
        Self::focus_dialog_input(&octal, window, cx);
        self.dialog = Some(Dialog::Permissions(PermDialog {
            entry,
            matrix,
            octal,
            recursive: false,
            _subscriptions: subscriptions,
        }));
        cx.notify();
    }

    /// 点九个勾选框之一：矩阵变了，八进制框跟着变。
    pub(super) fn toggle_perm_bit(
        &mut self,
        bit: super::props::PermBit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Dialog::Permissions(dialog)) = &mut self.dialog else {
            return;
        };
        dialog.matrix = dialog.matrix.toggled(bit);
        let text = octal_text(dialog.matrix.to_mode());
        let input = dialog.octal.clone();
        input.update(cx, |state, cx| state.set_value(text, window, cx));
        cx.notify();
    }

    fn confirm_permissions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::Permissions(dialog)) = &self.dialog else {
            return;
        };
        // 八进制框里是不合法的值：不提交，对话框里有提示。
        let text = dialog.octal.read(cx).value().to_string();
        if parse_octal_mode(&text).is_err() {
            return;
        }
        let (entry, mode, recursive) = (
            dialog.entry.clone(),
            dialog.matrix.to_mode(),
            dialog.recursive && dialog.entry.kind == EntryKind::Dir,
        );
        self.dialog = None;
        if let Err(err) = self.start_chmod_with(&entry.path, mode, recursive) {
            self.warn(err);
        }
        self.focus_pane(Side::Remote, window, cx);
        cx.notify();
    }

    /// 改权限（自动化钩子用，不递归）。
    pub(super) fn start_chmod(&mut self, path: &str, mode: u32) -> Result<(), String> {
        self.start_chmod_with(path, mode, false)
    }

    /// 排一个改权限。
    ///
    /// 核心的 `set_permissions` 用 `setstat`：SFTP 按路径处理，服务器会跟随符号链接，
    /// 改的是链接目标而不是链接本身。递归时跳过符号链接（见 `chmod.rs`）。
    pub(super) fn start_chmod_with(
        &mut self,
        path: &str,
        mode: u32,
        recursive: bool,
    ) -> Result<(), String> {
        let channel = self.connected_channel()?;
        let path = path.to_owned();
        let name = file_name(&path);
        let label = super::engine::format_mode(mode);
        if !recursive {
            self.say(format!(
                "{}{name} → {label}",
                t(SftpMsg::OpsChmodRunningPrefix)
            ));
            self.run_remote_op(
                async move {
                    channel.set_permissions(&path, mode).await.map_err(|err| {
                        format!(
                            "{}{name}{}{}{}",
                            t(SftpMsg::OpsChmodFailedPrefix),
                            t(SftpMsg::ParenOpen),
                            error_text(&err),
                            t(SftpMsg::ParenClose)
                        )
                    })?;
                    Ok(format!(
                        "{}{name} → {label}",
                        t(SftpMsg::OpsChmodDonePrefix)
                    ))
                },
                Some(Side::Remote),
            );
            return Ok(());
        }
        let runtime = runtime()?;
        let cancel = Arc::new(AtomicBool::new(false));
        self.chmod_cancel = Some(Arc::clone(&cancel));
        self.chmod_running = Some(0);
        self.say(format!(
            "{}{name} → {label}{}",
            t(SftpMsg::OpsChmodRecursivePrefix),
            t(SftpMsg::OpsEllipsis)
        ));
        let sender = self.sender.clone();
        runtime.spawn(async move {
            let mut last = Instant::now();
            let progress_sender = sender.clone();
            let result = recursive_chmod(channel.as_ref(), &path, mode, &cancel, move |changed| {
                if last.elapsed() >= REPORT_INTERVAL {
                    last = Instant::now();
                    let _ = progress_sender.send(SftpEvent::ChmodProgress { changed });
                }
            })
            .await;
            let event = match result {
                Ok(summary) => SftpEvent::OpDone {
                    ok: summary.is_clean(),
                    message: summary.message(&name, &label),
                    refresh: Some(Side::Remote),
                },
                Err(message) => SftpEvent::OpDone {
                    ok: false,
                    message: format!(
                        "{}{name}{}{message}{}",
                        t(SftpMsg::OpsChmodRecursiveFailedPrefix),
                        t(SftpMsg::ParenOpen),
                        t(SftpMsg::ParenClose)
                    ),
                    refresh: None,
                },
            };
            let _ = sender.send(event);
        });
        Ok(())
    }

    // ---- 属性 ----

    /// 打开属性对话框；没选中东西就是当前目录本身。
    pub(super) fn open_properties(
        &mut self,
        side: Side,
        entries: Vec<Entry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let targets = properties_targets(&entries, &self.pane(side).path);
        if targets.is_empty() {
            self.warn(match side {
                Side::Local => t(SftpMsg::MenuEnterFolder),
                Side::Remote => t(SftpMsg::MenuEnterDir),
            });
            cx.notify();
            return;
        }
        let count = targets.len();
        self.dialog = Some(Dialog::Properties(PropDialog {
            side,
            calcs: vec![CalcState::idle(); count],
            cancels: vec![None; count],
            targets,
        }));
        window.focus(&self.dialog_focus, cx);
        cx.notify();
    }

    /// 属性对话框里的“修改权限…”：换成权限对话框（只有远端单个真实条目）。
    pub(super) fn properties_to_permissions(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Dialog::Properties(dialog)) = &self.dialog else {
            return;
        };
        if dialog.side != Side::Remote || dialog.targets.len() != 1 {
            return;
        }
        let path = dialog.targets[0].path.clone();
        let Some(entry) = self.remote.raw.iter().find(|e| e.path == path).cloned() else {
            return;
        };
        self.cancel_property_calcs();
        self.open_permissions(entry, window, cx);
    }

    /// “计算大小”：给每个还没在算的文件夹排一个后台任务。
    pub(super) fn start_calcs(&mut self, cx: &mut Context<Self>) {
        let Some(Dialog::Properties(dialog)) = &mut self.dialog else {
            return;
        };
        let side = dialog.side;
        let mut jobs: Vec<(u64, String, Arc<AtomicBool>)> = Vec::new();
        for (index, target) in dialog.targets.iter().enumerate() {
            if !can_calculate(target) || dialog.calcs[index].phase == CalcPhase::Running {
                continue;
            }
            let request = self.next_calc;
            self.next_calc += 1;
            let cancel = Arc::new(AtomicBool::new(false));
            dialog.calcs[index] = CalcState::running(request);
            dialog.cancels[index] = Some(Arc::clone(&cancel));
            jobs.push((request, target.path.clone(), cancel));
        }
        let Ok(runtime) = runtime() else {
            return;
        };
        for (request, path, cancel) in jobs {
            let sender = self.sender.clone();
            let progress =
                move |totals: &SizeTotals,
                      last: &mut Instant,
                      sender: &std::sync::mpsc::Sender<SftpEvent>| {
                    if last.elapsed() >= REPORT_INTERVAL {
                        *last = Instant::now();
                        let _ = sender.send(SftpEvent::SizeProgress {
                            request,
                            phase: CalcPhase::Running,
                            totals: totals.clone(),
                            error: None,
                        });
                    }
                };
            let finish = move |sender: &std::sync::mpsc::Sender<SftpEvent>,
                               seen: SizeTotals,
                               result: Result<SizeTotals, SftpError>| {
                let event = match result {
                    Ok(totals) => SftpEvent::SizeProgress {
                        request,
                        phase: CalcPhase::Done,
                        totals,
                        error: None,
                    },
                    Err(SftpError::Cancelled) => SftpEvent::SizeProgress {
                        request,
                        phase: CalcPhase::Cancelled,
                        totals: seen,
                        error: None,
                    },
                    Err(err) => SftpEvent::SizeProgress {
                        request,
                        phase: CalcPhase::Failed,
                        totals: seen,
                        error: Some(error_text(&err)),
                    },
                };
                let _ = sender.send(event);
            };
            match side {
                Side::Remote => {
                    let Some(endpoints) = self.endpoints.clone() else {
                        continue;
                    };
                    runtime.spawn(async move {
                        let mut seen = SizeTotals::default();
                        let mut last = Instant::now();
                        let progress_sender = sender.clone();
                        let result = remote_size(endpoints.as_ref(), &path, &cancel, |totals| {
                            seen = totals.clone();
                            progress(totals, &mut last, &progress_sender);
                        })
                        .await;
                        finish(&sender, seen, result);
                    });
                }
                Side::Local => {
                    runtime.spawn_blocking(move || {
                        let mut seen = SizeTotals::default();
                        let mut last = Instant::now();
                        let progress_sender = sender.clone();
                        let result = local_size(Path::new(&path), &cancel, |totals| {
                            seen = totals.clone();
                            progress(totals, &mut last, &progress_sender);
                        });
                        finish(&sender, seen, result);
                    });
                }
            }
        }
        cx.notify();
    }

    /// “取消计算”：置取消标志，事件回来之后显示不完整的结果。
    pub(super) fn cancel_calcs(&mut self, cx: &mut Context<Self>) {
        self.cancel_property_calcs();
        cx.notify();
    }

    /// 关掉对话框 / 窗口 = 取消还在跑的计算。
    pub(super) fn cancel_property_calcs(&mut self) {
        if let Some(Dialog::Properties(dialog)) = &self.dialog {
            for cancel in dialog.cancels.iter().flatten() {
                cancel.store(true, Ordering::SeqCst);
            }
        }
    }

    pub(super) fn apply_size_progress(
        &mut self,
        request: u64,
        phase: CalcPhase,
        totals: SizeTotals,
        error: Option<String>,
    ) {
        let Some(Dialog::Properties(dialog)) = &mut self.dialog else {
            return;
        };
        for (index, calc) in dialog.calcs.iter_mut().enumerate() {
            if calc.request == request {
                calc.apply(request, phase, totals, error);
                if phase != CalcPhase::Running {
                    dialog.cancels[index] = None;
                }
                return;
            }
        }
    }

    // ---- 下载到… ----

    pub(super) fn open_download_to(
        &mut self,
        entries: Vec<Entry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if entries.is_empty() {
            self.warn(t(SftpMsg::OpsSelectItemsToTransfer));
            return;
        }
        let initial = if self.local.path.is_empty() {
            super::engine::default_download_dir().display().to_string()
        } else {
            self.local.path.clone()
        };
        let input = self.dialog_input(&initial, t(SftpMsg::OpsDownloadDirPlaceholder), window, cx);
        let subscriptions = vec![
            cx.observe(&input, |_this, _input, cx| cx.notify()),
            cx.subscribe_in(
                &input,
                window,
                |this, _input, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.confirm_download_to(window, cx),
                    InputEvent::Change => {
                        if let Some(Dialog::DownloadTo(dialog)) = &mut this.dialog {
                            dialog.error = None;
                        }
                    }
                    _ => {}
                },
            ),
        ];
        Self::focus_dialog_input(&input, window, cx);
        self.dialog = Some(Dialog::DownloadTo(DownloadDialog {
            entries,
            input,
            error: None,
            checking: false,
            check_request: 0,
            _subscriptions: subscriptions,
        }));
        cx.notify();
    }

    /// “选择文件夹…”：系统的选择文件夹对话框，选中的目录填进输入框。
    pub(super) fn pick_download_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t(SftpMsg::OpsChooseDownloadFolder).into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = receiver.await else {
                return;
            };
            let Some(picked) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(Dialog::DownloadTo(dialog)) = &mut this.dialog {
                    let input = dialog.input.clone();
                    dialog.error = None;
                    let text = picked.display().to_string();
                    input.update(cx, |state, cx| state.set_value(text, window, cx));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn confirm_download_to(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::DownloadTo(dialog)) = &mut self.dialog else {
            return;
        };
        let text = dialog.input.read(cx).value().trim().to_owned();
        if let NameCheck::Bad(reason) = validate_local_dir(&text) {
            dialog.error = Some(reason);
            cx.notify();
            return;
        }
        // 目录要存在、读得出来：在后台线程里确认（慢盘、失联的网络路径不能卡界面），
        // 不行就把原因留在对话框里，行了才开始下载（见 `apply_download_dir_checked`）。
        if dialog.checking {
            return;
        }
        dialog.checking = true;
        let sender = self.sender.clone();
        let dir = text.clone();
        let submitted = self.local_lister.spawn(
            &self.probe_gate,
            move |fs| scan(fs, &dir).map(|_| ()),
            move |seq, outcome| {
                let result = match outcome {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(ScanError::NotDir)) => Err(t(SftpMsg::OpsNotADirectory).to_owned()),
                    Ok(Err(ScanError::Failed(err))) | Err(err) => Err(err),
                };
                let _ = sender.send(SftpEvent::DownloadDirChecked {
                    request: seq,
                    dir: text,
                    result,
                });
            },
        );
        match submitted {
            Ok(seq) => dialog.check_request = seq,
            Err(_) => {
                dialog.checking = false;
                dialog.error = Some(t(SftpMsg::LocalReadQueueFull).to_owned());
            }
        }
        cx.notify();
    }

    /// 后台确认完这个下载目录：能用就开始下载，不能用把原因写在对话框里。
    pub(super) fn apply_download_dir_checked(
        &mut self,
        request: u64,
        dir: String,
        result: Result<(), String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Dialog::DownloadTo(dialog)) = &mut self.dialog else {
            return;
        };
        // 对话框已经重开 / 又点了一次：旧的确认结果不算数。
        if dialog.check_request != request || !self.probe_gate.is_latest(request) {
            return;
        }
        dialog.checking = false;
        if let Err(err) = result {
            dialog.error = Some(format!("{}{err}", t(SftpMsg::OpsDirUnusablePrefix)));
            cx.notify();
            return;
        }
        let entries = dialog.entries.clone();
        self.dialog = None;
        self.transfer_entries(Side::Remote, &entries, Some(PathBuf::from(dir)));
        self.focus_pane(Side::Remote, window, cx);
        cx.notify();
    }

    // ---- 压缩 / 解压 ----

    /// 同一时间只做一件（提示条只有一条）。返回任务号和取消开关；已有任务在跑返回 `None`。
    pub(super) fn begin_busy(&mut self, kind: BusyKind) -> Option<(u64, Arc<AtomicBool>)> {
        if self.busy.is_some() {
            self.say(t(SftpMsg::OpsServerBusy));
            return None;
        }
        let op = self.next_op;
        self.next_op += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        self.busy = Some(Busy {
            kind,
            op,
            started: Instant::now(),
            cancel: Arc::clone(&cancel),
            cancelling: false,
            bytes: None,
            files: None,
            local: false,
        });
        Some((op, cancel))
    }

    /// 提示条上的“取消”：关掉服务器上的执行通道，后台收尾（删临时文件）。
    pub(super) fn cancel_busy(&mut self, cx: &mut Context<Self>) {
        if let Some(busy) = self.busy.as_mut() {
            busy.cancel.store(true, Ordering::SeqCst);
            busy.cancelling = true;
            cx.notify();
        }
    }

    fn ready_caps(&self) -> Result<ArchiveCaps, String> {
        match self.archive_caps {
            ArchiveCapsState::Ready(caps) => Ok(caps),
            _ => Err(t(SftpMsg::MenuArchivePending).to_owned()),
        }
    }

    pub(super) fn start_compress(&mut self, entries: &[Entry], format: CompressFormat) {
        let names: Vec<String> = entries
            .iter()
            .filter(|entry| entry.name_is_utf8)
            .map(|entry| entry.name.clone())
            .collect();
        if names.is_empty() || self.remote.path.is_empty() {
            return;
        }
        let caps = match self.ready_caps() {
            Ok(caps) => caps,
            Err(err) => return self.warn(err),
        };
        if let Err(err) = require_tools(
            &caps,
            format == CompressFormat::TarGz,
            format == CompressFormat::Zip,
            false,
        ) {
            return self.warn(err);
        }
        let (Some(factory), Some(channel)) = (self.factory.clone(), self.channel.clone()) else {
            return self.warn(t(SftpMsg::NotConnectedYet));
        };
        let Ok(runtime) = runtime() else { return };
        let Some((op, cancel)) = self.begin_busy(BusyKind::Compress) else {
            return;
        };
        let dir = self.remote.path.clone();
        let sender = self.sender.clone();
        runtime.spawn(async move {
            let owner = make_owner();
            let progress_sender = sender.clone();
            let on_progress = move |progress: ArchiveProgress| {
                let _ = progress_sender.send(SftpEvent::ArchiveProgress {
                    op,
                    bytes: progress.bytes,
                });
            };
            let connection = factory.connection();
            let result = compress(
                connection.as_ref(),
                channel.as_ref(),
                &CompressRequest {
                    dir: &dir,
                    names: &names,
                    format,
                    owner: &owner,
                    server: channel.server_key(),
                },
                &cancel,
                Some(&on_progress),
            )
            .await;
            let result = match result {
                Ok(outcome) => Ok(Some(outcome)),
                Err(ArchiveError::Cancelled) => Ok(None),
                Err(err) => Err(err.to_string()),
            };
            let _ = sender.send(SftpEvent::CompressDone { op, result });
        });
    }

    pub(super) fn finish_compress(
        &mut self,
        op: u64,
        result: Result<Option<CompressOutcome>, String>,
        cx: &mut Context<Self>,
    ) {
        if self.busy.as_ref().is_some_and(|busy| busy.op == op) {
            self.busy = None;
        }
        match result {
            Ok(Some(outcome)) => {
                let notice =
                    archive_ui::compress_notice(false, &outcome.name, outcome.warning.as_deref());
                self.say(notice.text);
                self.remote.select_request = Some(vec![outcome.path]);
            }
            Ok(None) => self.say(archive_ui::compress_notice(true, "", None).text),
            Err(err) => self.warn(format!("{}{err}", t(SftpMsg::OpsCompressFailedPrefix))),
        }
        // 刷新回来时把这句结果留在状态栏里（不然马上被“N 项”顶掉）。
        let notice = self.status.clone();
        self.refresh_with_notice(Side::Remote, notice, cx);
    }

    /// 解压：先让服务器检查压缩包（拒绝指向目录外的路径、数同名文件），有同名文件再问用户。
    pub(super) fn start_extract_check(&mut self, entry: &Entry, into_folder: bool) {
        if self.remote.path.is_empty() {
            return;
        }
        let folder = extract_folder_name(&entry.name);
        if into_folder && folder.is_none() {
            return;
        }
        let caps = match self.ready_caps() {
            Ok(caps) => caps,
            Err(err) => return self.warn(err),
        };
        let format = ArchiveFormat::detect(&entry.name);
        if let Err(err) = require_tools(
            &caps,
            format.is_some_and(|f| !f.is_zip()),
            false,
            format.is_some_and(ArchiveFormat::is_zip),
        ) {
            return self.warn(err);
        }
        let (Some(factory), Some(channel)) = (self.factory.clone(), self.channel.clone()) else {
            return self.warn(t(SftpMsg::NotConnectedYet));
        };
        let Ok(runtime) = runtime() else { return };
        let base = self.remote.path.trim_end_matches('/').to_owned();
        let (dest, dest_name) = match &folder {
            Some(folder) if into_folder => (format!("{base}/{folder}"), folder.clone()),
            _ => (
                self.remote.path.clone(),
                base.rsplit('/')
                    .next()
                    .filter(|name| !name.is_empty())
                    .unwrap_or("/")
                    .to_owned(),
            ),
        };
        let target = ExtractTarget {
            archive: entry.path.clone(),
            archive_name: entry.name.clone(),
            dest,
            dest_name,
            select_dest: into_folder,
        };
        let Some((op, cancel)) = self.begin_busy(BusyKind::Scan) else {
            return;
        };
        let sender = self.sender.clone();
        runtime.spawn(async move {
            let connection = factory.connection();
            let result = plan_extract(
                connection.as_ref(),
                channel.as_ref(),
                &target.archive,
                &target.dest,
                &cancel,
            )
            .await;
            let result = match result {
                Ok(plan) => Ok(Some(plan)),
                Err(ArchiveError::Cancelled) => Ok(None),
                Err(err) => Err(err.to_string()),
            };
            let _ = sender.send(SftpEvent::ExtractPlanned { op, target, result });
        });
    }

    pub(super) fn finish_extract_plan(
        &mut self,
        op: u64,
        target: ExtractTarget,
        result: Result<Option<ExtractPlan>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.as_ref().is_some_and(|busy| busy.op == op) {
            self.busy = None;
        }
        match result {
            Ok(None) => self.say(t(SftpMsg::OpsExtractCancelled)),
            Err(err) => self.warn(format!("{}{err}", t(SftpMsg::OpsNotExtractedPrefix))),
            Ok(Some(plan)) => {
                let ask = ExtractAsk { target, plan };
                if ask.plan.conflict_count > 0 {
                    self.dialog = Some(Dialog::ExtractConflict(ask));
                    window.focus(&self.dialog_focus, cx);
                } else {
                    // 检查时没有同名文件：用“跳过已有文件”解压。检查和解压之间如果有别人新建了
                    // 同名文件，也不会被悄悄覆盖；只有用户在冲突对话框里明确选了“覆盖”才用 overwrite。
                    self.start_extract(ask, ConflictPolicy::SkipExisting);
                }
            }
        }
        cx.notify();
    }

    /// 冲突对话框上的选择。
    pub(super) fn choose_extract(
        &mut self,
        policy: ConflictPolicy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Dialog::ExtractConflict(ask)) = self.dialog.take() else {
            return;
        };
        self.start_extract(ask, policy);
        self.focus_pane(Side::Remote, window, cx);
        cx.notify();
    }

    pub(super) fn cancel_extract_choice(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.dialog, Some(Dialog::ExtractConflict(_))) {
            self.say(t(SftpMsg::OpsExtractCancelled));
            self.close_dialog(window, cx);
        }
    }

    /// 真正解压（检查过、冲突也问过了）。
    fn start_extract(&mut self, ask: ExtractAsk, policy: ConflictPolicy) {
        let (Some(factory), Some(channel)) = (self.factory.clone(), self.channel.clone()) else {
            return self.warn(t(SftpMsg::NotConnectedYet));
        };
        let Ok(runtime) = runtime() else { return };
        let Some((op, cancel)) = self.begin_busy(BusyKind::Extract) else {
            return;
        };
        let sender = self.sender.clone();
        runtime.spawn(async move {
            let owner = make_owner();
            let connection = factory.connection();
            let result = extract(
                connection.as_ref(),
                channel.as_ref(),
                &ExtractRequest {
                    archive: &ask.target.archive,
                    dest: &ask.target.dest,
                    policy,
                    plan_fingerprint: ask.plan.fingerprint,
                    plan_dest_exists: ask.plan.dest_exists,
                    plan_total_files: ask.plan.total_files,
                    plan_conflict_count: ask.plan.conflict_count,
                    owner: &owner,
                    server: channel.server_key(),
                },
                &cancel,
            )
            .await;
            let result = match result {
                Ok(outcome) => Ok(Some(outcome)),
                Err(ArchiveError::Cancelled) => Ok(None),
                Err(err) => Err(err.to_string()),
            };
            let _ = sender.send(SftpEvent::ExtractDone { op, ask, result });
        });
    }

    pub(super) fn finish_extract(
        &mut self,
        op: u64,
        ask: ExtractAsk,
        result: Result<Option<ExtractOutcome>, String>,
        cx: &mut Context<Self>,
    ) {
        if self.busy.as_ref().is_some_and(|busy| busy.op == op) {
            self.busy = None;
        }
        match result {
            Ok(Some(outcome)) => {
                let notice = archive_ui::extract_notice(
                    false,
                    outcome.extracted,
                    outcome.skipped,
                    outcome.warning.as_deref(),
                    &ask.target.dest_name,
                    ask.plan.dest_exists,
                );
                self.say(notice.text);
                if ask.target.select_dest {
                    self.remote.select_request = Some(vec![ask.target.dest.clone()]);
                }
            }
            Ok(None) => {
                let notice = archive_ui::extract_notice(
                    true,
                    0,
                    0,
                    None,
                    &ask.target.dest_name,
                    ask.plan.dest_exists,
                );
                self.say(notice.text);
            }
            Err(err) => self.warn(format!("{}{err}", t(SftpMsg::OpsExtractFailedPrefix))),
        }
        // 刷新回来时把这句结果留在状态栏里（不然马上被“N 项”顶掉）。
        let notice = self.status.clone();
        self.refresh_with_notice(Side::Remote, notice, cx);
    }

    // ---- 传输（菜单 / 对话框的入口） ----

    /// 下载（远端栏）/ 上传（本地栏）选中的条目：文件和整个文件夹都行，一次动作一个批次。
    /// 下载落点是 `dir_override`，没有就是本地栏当前目录；上传落点是远端栏当前目录。
    pub(super) fn transfer_entries(
        &mut self,
        side: Side,
        entries: &[Entry],
        dir_override: Option<PathBuf>,
    ) {
        match side {
            Side::Remote => {
                let dir = dir_override.unwrap_or_else(|| PathBuf::from(&self.local.path));
                self.download_entries(entries, dir);
            }
            Side::Local => {
                let remote_dir = self.remote.path.clone();
                self.upload_entries(entries, &remote_dir);
            }
        }
    }

    /// 把远端的条目下载进本地目录 `dir`（一个批次）。
    pub(super) fn download_entries(&mut self, entries: &[Entry], dir: PathBuf) {
        if entries.is_empty() {
            return self.warn(t(SftpMsg::OpsSelectItemsToTransfer));
        }
        if dir.as_os_str().is_empty() {
            return self.warn(t(SftpMsg::OpsLocalDirHint));
        }
        let (started, first_error) = self.in_new_batch(|this| {
            let mut started = 0usize;
            let mut first_error = None;
            for entry in entries.iter().filter(|e| e.name_is_utf8) {
                let result = if entry.is_enterable() {
                    this.start_dir_download_to(&dir, &entry.path)
                } else {
                    this.start_download_to(&dir, &entry.path)
                };
                match result {
                    Ok(()) => started += 1,
                    Err(err) => {
                        first_error.get_or_insert(err);
                    }
                }
            }
            (started, first_error)
        });
        self.report_started(started, first_error, Direction::Download);
        self.queue_open = true;
    }

    /// 把本地的条目上传进远端目录 `remote_dir`（一个批次）。
    pub(super) fn upload_entries(&mut self, entries: &[Entry], remote_dir: &str) {
        if entries.is_empty() {
            return self.warn(t(SftpMsg::OpsSelectItemsToTransfer));
        }
        if remote_dir.is_empty() {
            return self.warn(t(SftpMsg::OpsRemoteDirHint));
        }
        let (started, first_error) = self.in_new_batch(|this| {
            let mut started = 0usize;
            let mut first_error = None;
            for entry in entries.iter().filter(|e| e.name_is_utf8) {
                let result = if entry.is_enterable() {
                    this.start_dir_upload_to(&entry.path, remote_dir)
                } else {
                    this.start_upload_to(&entry.path, remote_dir)
                };
                match result {
                    Ok(()) => started += 1,
                    Err(err) => {
                        first_error.get_or_insert(err);
                    }
                }
            }
            (started, first_error)
        });
        self.report_started(started, first_error, Direction::Upload);
        self.queue_open = true;
    }

    fn report_started(
        &mut self,
        started: usize,
        first_error: Option<String>,
        direction: Direction,
    ) {
        let (queued, empty) = match direction {
            Direction::Upload => (SftpMsg::OpsQueuedUploadPrefix, SftpMsg::OpsNoItemsUpload),
            _ => (
                SftpMsg::OpsQueuedDownloadPrefix,
                SftpMsg::OpsNoItemsDownload,
            ),
        };
        match (started, first_error) {
            (0, Some(err)) => self.warn(err),
            (0, None) => self.warn(t(empty)),
            (count, None) => self.say(format!(
                "{}{count}{}",
                t(queued),
                t(SftpMsg::ItemCountSuffix)
            )),
            (count, Some(err)) => self.warn(format!(
                "{}{count}{}{}{err}",
                t(queued),
                t(SftpMsg::ItemCountSuffix),
                t(SftpMsg::OpsQueuedSomeSuffix)
            )),
        }
    }

    /// 自动化钩子排队的远端写操作：上一个结束（`OpDone`）之后才走到这里，
    /// 这样“建目录 → 改名 → 删除”在服务器上是按顺序发生的。
    pub(super) fn run_next_hook_op(&mut self, cx: &mut Context<Self>) {
        while let Some(op) = self.pending_ops.pop_front() {
            let cwd = self.remote.path.clone();
            let result = match op {
                RemoteOp::Mkdir(name) => self.start_mkdir(&name),
                RemoteOp::Rename { old, new } => super::engine::resolve_remote_child(&cwd, &old)
                    .and_then(|old_path| self.start_rename(&old_path, &new)),
                RemoteOp::Delete(name) => super::engine::resolve_remote_child(&cwd, &name)
                    .and_then(|path| self.start_delete(&path)),
                RemoteOp::Chmod { name, mode } => super::engine::resolve_remote_child(&cwd, &name)
                    .and_then(|path| self.start_chmod(&path, mode)),
            };
            match result {
                // 排上了，等它的结果回来再排下一个。
                Ok(()) => break,
                // 没排上（名字不合法等）：报出来，接着排下一个，别让一个坏钩子堵住后面。
                Err(err) => self.warn(format!("{}{err}", t(SftpMsg::OpsAutoOpNotStartedPrefix))),
            }
        }
        cx.notify();
    }
}
