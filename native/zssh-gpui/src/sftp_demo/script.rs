//! 调试驱动（`ZSSH_MAIN_SCRIPT` 的 `act:sftp_ui:...`）用的入口：按名字选中、按动作名触发菜单项、
//! 操作对话框、检查结果。它们调的是和菜单 / 对话框按钮同一批方法，所以验收脚本走的是真实的动作路径；
//! 真实鼠标和键盘事件另用 `click:` / `key:` 步骤。
//!
//! 返回 `Ok(true)` = 做完了 / 检查通过，`Ok(false)` = 还没到（驱动稍后再问一次），`Err` = 失败。

use ::gpui::Window;
use gpui_kit::*;
use zssh_core::sftp::archive::ConflictPolicy;

use super::dialogs::{Dialog, NameKind};
use super::menu::{MenuAction, MenuNode, Side, build_menu};
use super::model::SortKey;
use super::move_to::Choice;
use super::props::{CalcPhase, PermBit, aggregate};
use super::{SftpDemo, props};

fn side_of(raw: &str) -> Result<Side, String> {
    match raw {
        "remote" => Ok(Side::Remote),
        "local" => Ok(Side::Local),
        other => Err(format!("栏只能是 remote 或 local，不是 {other:?}")),
    }
}

fn action_of(raw: &str) -> Result<MenuAction, String> {
    Ok(match raw {
        "open" => MenuAction::Open,
        "edit" => MenuAction::EditBuiltin,
        "transfer" => MenuAction::Transfer,
        "download_to" => MenuAction::DownloadTo,
        "copy" => MenuAction::Copy,
        "paste" => MenuAction::Paste,
        "copy_path" => MenuAction::CopyPath,
        "rename" => MenuAction::Rename,
        "move" => MenuAction::MoveTo,
        "delete" => MenuAction::Delete,
        "mkdir" => MenuAction::Mkdir,
        "create" => MenuAction::Create,
        "perms" => MenuAction::Permissions,
        "refresh" => MenuAction::Refresh,
        "props" => MenuAction::Properties,
        "tgz" => MenuAction::CompressTarGz,
        "zip" => MenuAction::CompressZip,
        "extract_folder" => MenuAction::ExtractFolder,
        "extract_here" => MenuAction::ExtractHere,
        other => return Err(format!("不认识的菜单动作 {other:?}")),
    })
}

fn bit_of(raw: &str) -> Result<PermBit, String> {
    Ok(match raw {
        "ur" => PermBit::OwnerRead,
        "uw" => PermBit::OwnerWrite,
        "ux" => PermBit::OwnerExecute,
        "gr" => PermBit::GroupRead,
        "gw" => PermBit::GroupWrite,
        "gx" => PermBit::GroupExecute,
        "or" => PermBit::OthersRead,
        "ow" => PermBit::OthersWrite,
        "ox" => PermBit::OthersExecute,
        other => return Err(format!("不认识的权限位 {other:?}")),
    })
}

fn names(raw: &str) -> Vec<&str> {
    raw.split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .collect()
}

impl SftpDemo {
    fn script_rows(&self, side: Side) -> Vec<String> {
        self.pane(side)
            .rows
            .iter()
            .map(|e| e.name.clone())
            .collect()
    }

    fn dialog_kind(&self) -> &'static str {
        match &self.dialog {
            None => "none",
            Some(Dialog::Name(_)) => "name",
            Some(Dialog::Delete(_)) => "delete",
            Some(Dialog::Permissions(_)) => "perm",
            Some(Dialog::Properties(_)) => "props",
            Some(Dialog::DownloadTo(_)) => "download",
            Some(Dialog::ExtractConflict(_)) => "extract",
            Some(Dialog::OverwriteRename(_)) => "overwrite",
            Some(Dialog::MoveTo(_)) => "move",
            Some(Dialog::MoveConflict(_)) => "move_conflict",
            Some(Dialog::MoveSummary(_)) => "move_summary",
        }
    }

    /// 驱动脚本的统一入口，见文件头。
    pub fn script_ui(
        &mut self,
        command: &str,
        args: &[&str],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        let arg = |index: usize| args.get(index).copied().unwrap_or("");
        let rest = |from: usize| args.get(from..).unwrap_or(&[]).join(":");
        match command {
            "focus" => {
                let side = side_of(arg(0))?;
                self.focus_pane(side, window, cx);
            }
            // select:<栏>:<名字,名字>
            "select" => {
                let side = side_of(arg(0))?;
                let wanted: Vec<String> = names(&rest(1))
                    .into_iter()
                    .filter_map(|name| {
                        self.pane(side)
                            .rows
                            .iter()
                            .find(|e| e.name == name)
                            .map(|e| e.path.clone())
                    })
                    .collect();
                if wanted.len() != names(&rest(1)).len() {
                    return Ok(false);
                }
                let pane = self.pane_mut(side);
                let rows = std::mem::take(&mut pane.rows);
                pane.selection.select_paths(&rows, &wanted);
                pane.rows = rows;
            }
            // select_index:<栏>:<行号>（从 0 数；名字没法写进脚本时用，比如非 UTF-8 的名字）
            "select_index" => {
                let side = side_of(arg(0))?;
                let index: usize = arg(1).parse().map_err(|_| "行号不是数字")?;
                let Some(path) = self.pane(side).rows.get(index).map(|e| e.path.clone()) else {
                    return Ok(false);
                };
                let pane = self.pane_mut(side);
                let rows = std::mem::take(&mut pane.rows);
                pane.selection.select_paths(&rows, &[path]);
                pane.rows = rows;
            }
            "select_all" => {
                let pane = self.pane_mut(side_of(arg(0))?);
                let rows = std::mem::take(&mut pane.rows);
                pane.selection.select_all(&rows);
                pane.rows = rows;
            }
            "clear_select" => self.pane_mut(side_of(arg(0))?).selection.clear(),
            // assert_selected:<栏>:<名字,名字>（顺序按显示顺序）
            "assert_selected" => {
                let side = side_of(arg(0))?;
                let have: Vec<String> = self
                    .pane(side)
                    .selected()
                    .into_iter()
                    .map(|e| e.name)
                    .collect();
                let want: Vec<String> = names(&rest(1)).into_iter().map(str::to_owned).collect();
                if have != want {
                    return Err(format!("选中的是 {} 项，和预期不同", have.len()));
                }
            }
            "assert_selected_count" => {
                let have = self.pane(side_of(arg(0))?).selected().len();
                let want: usize = arg(1).parse().map_err(|_| "个数不是数字")?;
                if have != want {
                    return Err(format!("选中 {have} 项，预期 {want} 项"));
                }
            }
            "assert_rows" => {
                let side = side_of(arg(0))?;
                let want: usize = arg(1).parse().map_err(|_| "个数不是数字")?;
                let have = self.pane(side).rows.len();
                if have != want {
                    return Ok(false);
                }
            }
            // assert_has:<栏>:<名字>[:absent]
            "assert_has" => {
                let side = side_of(arg(0))?;
                let present = self.script_rows(side).iter().any(|n| n == arg(1));
                if present == (arg(2) == "absent") {
                    return Ok(false);
                }
            }
            // assert_contains:<栏>:<名字里的一段>：有一行的名字包含它（名字里有冒号时 assert_has 不好写）
            "assert_contains" => {
                let side = side_of(arg(0))?;
                let needle = arg(1);
                if !self.script_rows(side).iter().any(|n| n.contains(needle)) {
                    return Ok(false);
                }
            }
            // sort:<栏>:<name|size|mtime|kind>
            "sort" => {
                let key = match arg(1) {
                    "name" => SortKey::Name,
                    "size" => SortKey::Size,
                    "mtime" => SortKey::Mtime,
                    "kind" => SortKey::Kind,
                    other => return Err(format!("不认识的排序列 {other:?}")),
                };
                self.toggle_sort(side_of(arg(0))?, key, cx);
            }
            // assert_order:<栏>:<名字,名字>（显示顺序的开头几个）
            "assert_order" => {
                let rows = self.script_rows(side_of(arg(0))?);
                let joined = rest(1);
                let want = names(&joined);
                let have: Vec<&str> = rows.iter().take(want.len()).map(String::as_str).collect();
                if have != want {
                    return Err(format!("显示顺序的开头是 {have:?}，预期 {want:?}"));
                }
            }
            // assert_files_order:<栏>:<名字,名字>（不算文件夹，只看文件的显示顺序开头）
            "assert_files_order" => {
                let side = side_of(arg(0))?;
                let rows: Vec<String> = self
                    .pane(side)
                    .rows
                    .iter()
                    .filter(|e| e.kind != super::model::EntryKind::Dir)
                    .map(|e| e.name.clone())
                    .collect();
                let joined = rest(1);
                let want = names(&joined);
                let have: Vec<&str> = rows.iter().take(want.len()).map(String::as_str).collect();
                if have != want {
                    return Err(format!("文件的显示顺序开头是 {have:?}，预期 {want:?}"));
                }
            }
            // filter:<栏>:<文字>
            "filter" => {
                let side = side_of(arg(0))?;
                let text = rest(1);
                let input = self.pane(side).filter_input.clone();
                // `set_value` 不发 Change 事件：和输入框的 Change 处理走同一步，直接把过滤文字交给列表。
                input.update(cx, |state, cx| state.set_value(text.clone(), window, cx));
                let pane = self.pane_mut(side);
                pane.filter.text = text;
                pane.rebuild();
            }
            "hidden" => {
                let side = side_of(arg(0))?;
                let want = arg(1) == "on";
                if self.pane(side).filter.show_hidden != want {
                    self.toggle_hidden(side, cx);
                }
            }
            // goto:<栏>:<路径>
            "goto" => {
                let side = side_of(arg(0))?;
                let path = rest(1);
                self.browse(side, &path, cx);
            }
            "back" => self.step_history(side_of(arg(0))?, -1, cx),
            "forward" => self.step_history(side_of(arg(0))?, 1, cx),
            "up" => self.go_parent(side_of(arg(0))?, cx),
            "refresh" => self.refresh(side_of(arg(0))?, cx),
            // assert_mode:<栏>:<名字>:<四位八进制>：列表里这一项的权限
            "assert_mode" => {
                let side = side_of(arg(0))?;
                let mode = self
                    .pane(side)
                    .raw
                    .iter()
                    .find(|e| e.name == arg(1))
                    .and_then(|e| e.permissions)
                    .map(|m| format!("{:04o}", m & 0o7777));
                if mode.as_deref() != Some(arg(2)) {
                    return Ok(false);
                }
            }
            // assert_path:<栏>:<路径>
            "assert_path" => {
                let side = side_of(arg(0))?;
                if self.pane(side).path != rest(1) {
                    return Ok(false);
                }
            }
            // menu:<栏>:<动作>：对当前选择（没有选择就是空白处）触发菜单项，和点菜单一样。
            "menu" => {
                let side = side_of(arg(0))?;
                let action = action_of(arg(1))?;
                if side == Side::Remote {
                    self.ensure_archive_caps(cx);
                }
                let context = self.menu_context(side);
                let wanted = build_menu(&context);
                let enabled = find_enabled(&wanted, action);
                match enabled {
                    None => return Err(format!("菜单里没有 {:?}", arg(1))),
                    Some(false) => return Ok(false),
                    Some(true) => {}
                }
                self.run_menu_action(side, action, context.entries, context.blank, window, cx);
            }
            // assert_menu:<栏>:<动作>:<enabled|disabled|absent>
            "assert_menu" => {
                let side = side_of(arg(0))?;
                let action = action_of(arg(1))?;
                let context = self.menu_context(side);
                let state = match find_enabled(&build_menu(&context), action) {
                    None => "absent",
                    Some(true) => "enabled",
                    Some(false) => "disabled",
                };
                if state != arg(2) {
                    return Ok(false);
                }
            }
            "assert_dialog" => {
                if self.dialog_kind() != arg(0) {
                    return Ok(false);
                }
            }
            "dialog_text" => {
                let text = rest(0);
                let input = match &self.dialog {
                    Some(Dialog::Name(d)) => d.input.clone(),
                    Some(Dialog::Permissions(d)) => d.octal.clone(),
                    Some(Dialog::DownloadTo(d)) => d.input.clone(),
                    Some(Dialog::MoveTo(d)) => d.input.clone(),
                    _ => return Err("当前对话框没有输入框".to_owned()),
                };
                input.update(cx, |state, cx| state.set_value(text, window, cx));
            }
            "dialog_ok" => match &self.dialog {
                Some(Dialog::Delete(_)) => self.confirm_delete(window, cx),
                Some(Dialog::OverwriteRename(_)) => self.confirm_overwrite_rename(window, cx),
                Some(Dialog::MoveTo(_)) => self.confirm_move_to(window, cx),
                Some(Dialog::MoveConflict(_)) => {
                    return Err("同名询问用 move_choice 回答".to_owned());
                }
                Some(_) => self.dialog_enter(window, cx),
                None => return Err("没有对话框".to_owned()),
            },
            "dialog_cancel" => self.close_dialog(window, cx),
            // ---- 「移动到…」对话框（V2-U23）----
            // move_browse:<路径>：浏览到这个文件夹（输入框跟着变）
            "move_browse" => self.move_browse_to(rest(0), window, cx),
            "move_up" => self.move_up(window, cx),
            // move_select:<名字> / move_open:<名字>：单击 / 双击浏览列表里的子文件夹（列表还没读回来返回“还没到”）
            "move_select" | "move_open" => {
                let Some(Dialog::MoveTo(d)) = &self.dialog else {
                    return Err("当前不是「移动到」对话框".to_owned());
                };
                let name = rest(0);
                let Some(path) = d
                    .browse
                    .folders
                    .iter()
                    .find(|f| f.name == name)
                    .map(|f| f.path.clone())
                else {
                    return Ok(false);
                };
                if command == "move_select" {
                    self.move_select_folder(path, window, cx);
                } else {
                    self.move_browse_to(path, window, cx);
                }
            }
            // move_enter：在目标输入框里按 Enter
            "move_enter" => self.move_enter(window, cx),
            // move_new_folder_open：展开「新建文件夹」的名字行；move_new_folder:<名字>：填名字并创建
            "move_new_folder_open" => {
                if matches!(&self.dialog, Some(Dialog::MoveTo(d)) if d.new_folder.is_none()) {
                    self.toggle_new_folder(window, cx);
                }
            }
            "move_new_folder" => {
                if matches!(&self.dialog, Some(Dialog::MoveTo(d)) if d.new_folder.is_none()) {
                    self.toggle_new_folder(window, cx);
                }
                let Some(Dialog::MoveTo(d)) = &self.dialog else {
                    return Err("当前不是「移动到」对话框".to_owned());
                };
                let input = d.new_folder.clone().ok_or("新建文件夹的名字行没展开")?;
                let name = rest(0);
                input.update(cx, |state, cx| state.set_value(name, window, cx));
                self.confirm_new_folder(window, cx);
            }
            // assert_move_dir:<路径>：浏览列表现在显示的是这个文件夹（读完了）
            "assert_move_dir" => {
                let Some(Dialog::MoveTo(d)) = &self.dialog else {
                    return Err("当前不是「移动到」对话框".to_owned());
                };
                if d.browse.loading || d.browse.dir != rest(0) {
                    return Ok(false);
                }
            }
            // assert_move_folders:<名字,名字>：浏览列表里的子文件夹（按显示顺序，必须完全一致）
            "assert_move_folders" => {
                let Some(Dialog::MoveTo(d)) = &self.dialog else {
                    return Err("当前不是「移动到」对话框".to_owned());
                };
                if d.browse.loading {
                    return Ok(false);
                }
                let have: Vec<&str> = d.browse.folders.iter().map(|f| f.name.as_str()).collect();
                let joined = rest(0);
                let want = names(&joined);
                if have != want {
                    return Err(format!("浏览列表里是 {have:?}，预期 {want:?}"));
                }
            }
            // assert_move_check:<ok|same|self|invalid>：按钮置灰的原因 / 可以移动
            "assert_move_check" => {
                use super::move_to::{DestCheck, check_destination};
                let Some(Dialog::MoveTo(d)) = &self.dialog else {
                    return Err("当前不是「移动到」对话框".to_owned());
                };
                let text = d.input.read(cx).value().to_string();
                let state = match check_destination(d.side, &d.entries, &text) {
                    DestCheck::Ok(_) => "ok",
                    DestCheck::SameFolder => "same",
                    DestCheck::IntoSelf => "self",
                    DestCheck::Invalid(_) => "invalid",
                };
                if state != arg(0) {
                    return Err(format!("目标检查结果是 {state}，预期 {}", arg(0)));
                }
            }
            // assert_move_input:<路径>
            "assert_move_input" => {
                let Some(Dialog::MoveTo(d)) = &self.dialog else {
                    return Err("当前不是「移动到」对话框".to_owned());
                };
                let text = d.input.read(cx).value().to_string();
                if text != rest(0) {
                    return Err(format!("输入框里是 {text:?}"));
                }
            }
            // move_choice:<replace|skip|abort>[:all]：回答同名询问
            "move_choice" => {
                if !matches!(self.dialog, Some(Dialog::MoveConflict(_))) {
                    return Ok(false);
                }
                if arg(1) == "all" {
                    self.set_move_apply_rest(true, cx);
                }
                match arg(0) {
                    "replace" => self.answer_move_conflict(Choice::Replace, window, cx),
                    "skip" => self.answer_move_conflict(Choice::Skip, window, cx),
                    "abort" => self.abort_move(window, cx),
                    other => return Err(format!("不认识的选择 {other:?}")),
                }
            }
            // assert_moving:none|some
            "assert_moving" => {
                if self.moving.is_some() != (arg(0) == "some") {
                    return Ok(false);
                }
            }
            // assert_name_error:<yes|no>
            "assert_name_ok" => {
                let Some(Dialog::Name(d)) = &self.dialog else {
                    return Err("当前不是新建 / 重命名对话框".to_owned());
                };
                let text = d.input.read(cx).value().to_string();
                let ok =
                    super::dialogs::validate_name(&text, d.side) == super::dialogs::NameCheck::Ok;
                if ok != (arg(0) == "yes") {
                    return Err("名字校验结果和预期不同".to_owned());
                }
            }
            "assert_name_kind" => {
                let Some(Dialog::Name(d)) = &self.dialog else {
                    return Ok(false);
                };
                let kind = match d.kind {
                    NameKind::Mkdir => "mkdir",
                    NameKind::Create => "create",
                    NameKind::Rename => "rename",
                };
                if kind != arg(0) {
                    return Err("名字对话框的种类和预期不同".to_owned());
                }
            }
            "perm_bit" => self.toggle_perm_bit(bit_of(arg(0))?, window, cx),
            "perm_recursive" => {
                if let Some(Dialog::Permissions(d)) = &mut self.dialog {
                    d.recursive = arg(0) == "on";
                } else {
                    return Err("当前不是权限对话框".to_owned());
                }
            }
            // assert_perm:<八进制，3 位>
            "assert_perm" => {
                let Some(Dialog::Permissions(d)) = &self.dialog else {
                    return Err("当前不是权限对话框".to_owned());
                };
                let want = props::octal_text(d.matrix.to_mode());
                if want != arg(0) {
                    return Err(format!("勾选框对应的权限是 {want}，预期 {}", arg(0)));
                }
            }
            "calc" => match arg(0) {
                "start" => self.start_calcs(cx),
                "cancel" => self.cancel_calcs(cx),
                other => return Err(format!("calc 只有 start / cancel，不是 {other:?}")),
            },
            // assert_calc:<running|done|cancelled|failed|idle>
            "assert_calc" => {
                let Some(Dialog::Properties(d)) = &self.dialog else {
                    return Err("当前不是属性对话框".to_owned());
                };
                let phase = aggregate(&d.calcs).phase;
                let name = match phase {
                    CalcPhase::Idle => "idle",
                    CalcPhase::Running => "running",
                    CalcPhase::Done => "done",
                    CalcPhase::Cancelled => "cancelled",
                    CalcPhase::Failed => "failed",
                };
                if name != arg(0) {
                    return Ok(false);
                }
            }
            // assert_calc_text:<子串>
            "assert_calc_text" => {
                let Some(Dialog::Properties(d)) = &self.dialog else {
                    return Err("当前不是属性对话框".to_owned());
                };
                let text = props::calc_text(&aggregate(&d.calcs));
                if !text.contains(&rest(0)) {
                    return Err(format!("计算结果是「{text}」，里面没有预期的文字"));
                }
            }
            "props_perms" => self.properties_to_permissions(window, cx),
            "extract_choice" => match arg(0) {
                "skip" => self.choose_extract(ConflictPolicy::SkipExisting, window, cx),
                "overwrite" => self.choose_extract(ConflictPolicy::Overwrite, window, cx),
                "cancel" => self.cancel_extract_choice(window, cx),
                other => return Err(format!("不认识的选择 {other:?}")),
            },
            // assert_clip:<栏>:<个数>：应用内剪贴板里是哪一栏复制的、几项（none = 空的）
            "assert_clip" => {
                let have = super::clip::summary();
                if arg(0) == "none" {
                    if have.is_some() {
                        return Ok(false);
                    }
                } else if have
                    != Some((
                        side_of(arg(0))?,
                        arg(1).parse().map_err(|_| "个数不是数字")?,
                    ))
                {
                    return Ok(false);
                }
            }
            // assert_paste_retry:yes|no：有没有"改为经本机中转重试"的提示条
            "assert_paste_retry" => {
                if self.paste_retry.is_some() != (arg(0) == "yes") {
                    return Ok(false);
                }
            }
            // paste_retry_relay：点条上的"改为经本机中转重试"
            "paste_retry_relay" => {
                if self.paste_retry.is_none() {
                    return Err("没有可重试的粘贴".to_owned());
                }
                self.retry_paste_via_relay(cx);
            }
            // paste_into:<栏>:<文件夹路径>：粘贴进指定文件夹（右键在文件夹上粘贴走的就是它）
            "paste_into" => {
                let side = side_of(arg(0))?;
                self.paste_into(side, rest(1), cx);
            }
            // assert_busy:none|some
            "assert_busy" => {
                if self.busy.is_some() != (arg(0) == "some") {
                    return Ok(false);
                }
            }
            // assert_status:<子串>
            "assert_status" => {
                if !self.status.contains(&rest(0)) {
                    return Ok(false);
                }
            }
            "assert_no_error" => {
                if self.status_is_error {
                    return Err(format!("状态栏是出错提示：{}", self.status));
                }
            }
            "queue" => {
                self.queue_open = arg(0) == "open";
            }
            // retry_last：重试最后一个失败 / 取消 / 跳过的任务
            "retry_last" => {
                let id = self
                    .transfers
                    .iter()
                    .rev()
                    .find(|row| super::menu::transfer_can_retry(&row.state))
                    .map(|row| row.id)
                    .ok_or("没有能重试的任务")?;
                self.retry_transfer(id, cx);
            }
            // assert_path_edit:<栏>:<yes|no>：路径栏现在是不是输入框
            "assert_path_edit" => {
                let side = side_of(arg(0))?;
                if self.pane(side).path_edit.is_some() != (arg(1) == "yes") {
                    return Ok(false);
                }
            }
            // assert_transfer:<状态文字的一段>：队列里有一行的状态文字包含它（如 已暂停 / 已取消 / 已完成）
            "assert_transfer" => {
                let needle = rest(0);
                if !self
                    .transfers
                    .iter()
                    .any(|row| super::engine::state_label(&row.state).contains(&needle))
                {
                    return Ok(false);
                }
            }
            // assert_transfer_count:<个数>：队列里一共有几行（拖放取消 / 拖到不收的地方，不该多出任务）
            "assert_transfer_count" => {
                let want: usize = arg(0).parse().map_err(|_| "个数不是数字")?;
                if self.transfers.len() != want {
                    return Ok(false);
                }
            }
            // assert_reconcile:<子串>：队列里有一行的对账文字包含它
            "assert_reconcile" => {
                let found = self.transfers.iter().any(|row| {
                    super::queue::reconcile_summary(&row.facts(), row.elapsed())
                        .is_some_and(|text| text.contains(&rest(0)))
                });
                if !found {
                    return Ok(false);
                }
            }
            "assert_row_error" => {
                let found = self.transfers.iter().any(|row| {
                    matches!(&row.state, zssh_core::sftp::transfer::TransferState::Failed(reason) if reason.contains(&rest(0)))
                });
                if !found {
                    return Ok(false);
                }
            }
            // mkdir_local:<路径>：脚本用的本地准备（只在本地栏当前目录下建）
            "local_prepare" => {
                let base = std::path::PathBuf::from(&self.local.path);
                for spec in rest(0).split(',').filter(|s| !s.is_empty()) {
                    let (kind, name) =
                        spec.split_once('=').ok_or("格式是 d=名字 或 f=名字=内容")?;
                    let (name, content) = name.split_once('=').unwrap_or((name, "x"));
                    let path = base.join(name);
                    match kind {
                        "d" => std::fs::create_dir_all(&path).map_err(|e| e.to_string())?,
                        "f" => {
                            if let Some(parent) = path.parent() {
                                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                            }
                            std::fs::write(&path, content.as_bytes()).map_err(|e| e.to_string())?;
                        }
                        other => return Err(format!("不认识的准备类型 {other:?}")),
                    }
                }
                self.refresh(Side::Local, cx);
            }
            // assert_local_file:<相对本地栏当前目录的路径>:<内容>
            "assert_local_file" => {
                let path = std::path::PathBuf::from(&self.local.path).join(arg(0));
                let expected = rest(1).replace(r"\n", "\n");
                match std::fs::read(&path) {
                    Ok(bytes) if bytes == expected.as_bytes() => {}
                    Ok(_) => return Err("本地文件的内容和预期不同".to_owned()),
                    Err(_) => return Ok(false),
                }
            }
            // assert_local_size:<相对路径>:<字节数>
            "assert_local_size" => {
                let path = std::path::PathBuf::from(&self.local.path).join(arg(0));
                let want: u64 = arg(1).parse().map_err(|_| "大小不是数字")?;
                match std::fs::metadata(&path) {
                    Ok(meta) if meta.len() == want => {}
                    _ => return Ok(false),
                }
            }
            other => return Err(format!("sftp_ui 不认识 {other:?}")),
        }
        cx.notify();
        Ok(true)
    }
}

/// 菜单里这个动作能不能点：`None` = 菜单里没有，`Some(true)` = 可点，`Some(false)` = 置灰。
fn find_enabled(nodes: &[MenuNode], action: MenuAction) -> Option<bool> {
    for node in nodes {
        match node {
            MenuNode::Item {
                action: a, reason, ..
            } if *a == action => return Some(reason.is_none()),
            MenuNode::Submenu { children, .. } => {
                if let Some(found) = find_enabled(children, action) {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    None
}
