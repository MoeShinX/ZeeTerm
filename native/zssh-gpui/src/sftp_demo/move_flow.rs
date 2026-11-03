//! 「移动到…」在文件视图上的接线（V2-U23）：打开对话框、目录浏览、新建文件夹、开始移动、
//! 逐项执行、同名询问、结束汇总。规则和状态机在 `move_to.rs`，这里只管对话框状态和后台任务。
//!
//! 流程：菜单「移动到…」→ 对话框（路径输入框 + 目录浏览列表，双向同步）→ 点「移动」→ 先列一遍目标
//! 文件夹确认它存在 → 建 [`MoveJob`] → 一项一项发给后台（远端 SFTP `rename`，本地系统改名）→
//! 每项的结果回来交给状态机，它说“下一项 / 问用户 / 完了”。

use ::gpui::Window;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::*;
use zssh_core::sftp::path::file_name;

use super::dialogs::{
    BrowseState, Dialog, MoveConflict, MoveDialog, MoveReport, NameCheck, validate_name,
};
use super::engine::create_remote_dir;
use super::local_ops;
use super::menu::Side;
use super::model::{Entry, local_parent, remote_parent};
use super::move_to::{
    Choice, DestCheck, MoveJob, Next, StepResult, check_destination, local_folders, local_step,
    remote_folders, remote_step, same_path, target_path, transfer_blocks,
};
use super::{SftpDemo, SftpEvent};
use crate::remote_editor_demo::open_files;
use crate::ssh_backend::runtime;

impl SftpDemo {
    // ---- 打开对话框 ----

    /// 菜单「移动到…」。`entries` 是菜单打开那一刻选中的条目。
    pub(super) fn open_move_to(
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
                "先选中要移动的条目"
            } else {
                super::menu::NOT_UTF8_REASON
            });
            return;
        }
        if self.moving.is_some() {
            self.warn("正在移动，等它结束再移动下一批");
            return;
        }
        let start = self.pane(side).path.clone();
        if start.is_empty() {
            self.warn(match side {
                Side::Local => "先进入一个文件夹",
                Side::Remote => "先进入一个目录",
            });
            return;
        }
        if operable.len() < entries.len() {
            self.warn("名字不是 UTF-8 的条目已跳过");
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("目标文件夹的完整路径"));
        input.update(cx, |state, cx| state.set_value(start.clone(), window, cx));
        let subscriptions = vec![
            cx.observe(&input, |_this, _input, cx| cx.notify()),
            cx.subscribe_in(
                &input,
                window,
                |this, input, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.move_enter(window, cx),
                    InputEvent::Change => {
                        let text = input.read(cx).value().to_string();
                        this.move_input_changed(&text, cx);
                    }
                    _ => {}
                },
            ),
        ];
        input.update(cx, |state, cx| {
            state.focus(window, cx);
            state.select_all(window, cx);
        });
        self.dialog = Some(Dialog::MoveTo(MoveDialog {
            side,
            entries: operable,
            input,
            browse: BrowseState {
                dir: start.clone(),
                folders: Vec::new(),
                loading: false,
                error: None,
                selected: None,
                request: 0,
                pending_select: None,
            },
            confirming: false,
            error: None,
            new_folder: None,
            new_folder_error: None,
            creating: false,
            scroll: ::gpui::ScrollHandle::new(),
            _subscriptions: subscriptions,
        }));
        self.spawn_browse(start, false, None);
        cx.notify();
    }

    // ---- 目录浏览 ----

    /// 后台读 `dir` 里的子文件夹；结果以 `MoveBrowsed` 回来。`confirming` = 这次读是为了确认目标存在。
    fn spawn_browse(&mut self, dir: String, confirming: bool, select: Option<String>) {
        self.move_browse_seq += 1;
        let request = self.move_browse_seq;
        let side = {
            let Some(Dialog::MoveTo(d)) = &mut self.dialog else {
                return;
            };
            d.browse.request = request;
            d.browse.loading = true;
            d.browse.pending_select = select;
            d.confirming = confirming;
            d.side
        };
        let sender = self.sender.clone();
        let outcome = match side {
            Side::Remote => match (self.connected_channel(), runtime()) {
                (Ok(channel), Ok(runtime)) => {
                    let dir = dir.clone();
                    runtime.spawn(async move {
                        let result = remote_folders(&channel, &dir).await;
                        let _ = sender.send(SftpEvent::MoveBrowsed {
                            request,
                            dir,
                            result,
                        });
                    });
                    Ok(())
                }
                (Err(err), _) | (_, Err(err)) => Err(err),
            },
            Side::Local => match runtime() {
                Ok(runtime) => {
                    let dir = dir.clone();
                    runtime.spawn_blocking(move || {
                        let result = local_folders(&dir);
                        let _ = sender.send(SftpEvent::MoveBrowsed {
                            request,
                            dir,
                            result,
                        });
                    });
                    Ok(())
                }
                Err(err) => Err(err),
            },
        };
        if let Err(err) = outcome
            && let Some(Dialog::MoveTo(d)) = &mut self.dialog
        {
            d.browse.loading = false;
            d.browse.error = Some(err);
            d.confirming = false;
        }
    }

    pub(super) fn apply_move_browsed(
        &mut self,
        request: u64,
        dir: String,
        result: Result<Vec<Entry>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Dialog::MoveTo(d)) = &mut self.dialog else {
            return;
        };
        if d.browse.request != request {
            return;
        }
        d.browse.loading = false;
        let was_confirming = std::mem::take(&mut d.confirming);
        match result {
            Ok(folders) => {
                d.browse.dir = dir;
                d.browse.folders = folders;
                d.browse.error = None;
                d.browse.selected = None;
                if let Some(path) = d.browse.pending_select.take()
                    && d.browse.folders.iter().any(|f| f.path == path)
                {
                    d.browse.selected = Some(path.clone());
                    if let Some(index) = d.browse.folders.iter().position(|f| f.path == path) {
                        d.scroll.scroll_to_item(index);
                    }
                    let input = d.input.clone();
                    input.update(cx, |state, cx| state.set_value(path, window, cx));
                }
                if was_confirming {
                    self.confirm_move_to(window, cx);
                }
            }
            Err(err) => {
                d.browse.pending_select = None;
                d.browse.error = Some(format!("读不了这个文件夹：{err}"));
                if was_confirming {
                    d.error = Some(format!("目标文件夹用不了：{err}"));
                }
            }
        }
        cx.notify();
    }

    /// 浏览到 `dir`（面包屑、上一级、双击文件夹），输入框跟着变。
    pub(super) fn move_browse_to(
        &mut self,
        dir: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Dialog::MoveTo(d)) = &mut self.dialog else {
            return;
        };
        d.error = None;
        d.browse.selected = None;
        let input = d.input.clone();
        input.update(cx, |state, cx| state.set_value(dir.clone(), window, cx));
        self.spawn_browse(dir, false, None);
        cx.notify();
    }

    /// 单击一个子文件夹：选中，它就是目标（输入框跟着变），列表不动。
    pub(super) fn move_select_folder(
        &mut self,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Dialog::MoveTo(d)) = &mut self.dialog else {
            return;
        };
        d.error = None;
        d.browse.selected = Some(path.clone());
        let input = d.input.clone();
        input.update(cx, |state, cx| state.set_value(path, window, cx));
        cx.notify();
    }

    /// 「上一级」。
    pub(super) fn move_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::MoveTo(d)) = &self.dialog else {
            return;
        };
        let parent = match d.side {
            Side::Remote if d.browse.dir.is_empty() || d.browse.dir == "/" => return,
            Side::Remote => remote_parent(&d.browse.dir),
            Side::Local if d.browse.dir.is_empty() => return,
            Side::Local => local_parent(&d.browse.dir),
        };
        self.move_browse_to(parent, window, cx);
    }

    /// 输入框里的文字被改了：清掉旧的出错提示；写到以分隔符结尾，就把列表带到那个文件夹。
    fn move_input_changed(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(Dialog::MoveTo(d)) = &mut self.dialog else {
            return;
        };
        d.error = None;
        d.browse.selected = None;
        let trimmed = text.trim();
        let ends_with_separator =
            trimmed.len() > 1 && (trimmed.ends_with('/') || trimmed.ends_with('\\'));
        let side = d.side;
        let differs = !same_path(side, trimmed, &d.browse.dir);
        if ends_with_separator && differs {
            let dir = trimmed.to_owned();
            self.spawn_browse(dir, false, None);
        }
        cx.notify();
    }

    /// 输入框 / 对话框上按 Enter：写的是另一个文件夹就带列表过去，已经在那里就开始移动。
    pub(super) fn move_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::MoveTo(d)) = &self.dialog else {
            return;
        };
        if d.browse.loading || d.confirming {
            return;
        }
        // 新建文件夹的名字框有焦点时，Enter 是“创建”。
        if d.new_folder.is_some() {
            self.confirm_new_folder(window, cx);
            return;
        }
        let text = d.input.read(cx).value().to_string();
        let check = check_destination(d.side, &d.entries, &text);
        if let DestCheck::Ok(dest) = &check
            && !same_path(d.side, dest, &d.browse.dir)
            && d.browse.selected.is_none()
        {
            let dest = dest.clone();
            self.spawn_browse(dest, false, None);
            cx.notify();
            return;
        }
        self.confirm_move_to(window, cx);
    }

    // ---- 新建文件夹 ----

    pub(super) fn toggle_new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::MoveTo(d)) = &mut self.dialog else {
            return;
        };
        if d.new_folder.take().is_some() {
            d.new_folder_error = None;
            cx.notify();
            return;
        }
        if d.browse.dir.is_empty() {
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("新文件夹的名字"));
        input.update(cx, |state, cx| state.focus(window, cx));
        d.new_folder = Some(input.clone());
        d.new_folder_error = None;
        d._subscriptions
            .push(cx.observe(&input, |_this, _input, cx| cx.notify()));
        d._subscriptions.push(cx.subscribe_in(
            &input,
            window,
            |this, _input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.confirm_new_folder(window, cx),
                InputEvent::Change => {
                    if let Some(Dialog::MoveTo(d)) = &mut this.dialog {
                        d.new_folder_error = None;
                    }
                }
                _ => {}
            },
        ));
        cx.notify();
    }

    pub(super) fn confirm_new_folder(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::MoveTo(d)) = &mut self.dialog else {
            return;
        };
        let Some(input) = d.new_folder.clone() else {
            return;
        };
        if d.creating {
            return;
        }
        let name = input.read(cx).value().trim().to_owned();
        if let NameCheck::Bad(reason) = validate_name(&name, d.side) {
            d.new_folder_error = Some(reason);
            cx.notify();
            return;
        }
        let (side, dir) = (d.side, d.browse.dir.clone());
        let path = target_path(side, &dir, &name);
        let Ok(runtime) = runtime() else {
            return;
        };
        let sender = self.sender.clone();
        let channel = if side == Side::Remote {
            match self.connected_channel() {
                Ok(channel) => Some(channel),
                Err(err) => {
                    if let Some(Dialog::MoveTo(d)) = &mut self.dialog {
                        d.new_folder_error = Some(err);
                    }
                    return;
                }
            }
        } else {
            None
        };
        if let Some(Dialog::MoveTo(d)) = &mut self.dialog {
            d.creating = true;
        }
        let event_dir = dir;
        let event_path = path.clone();
        match channel {
            Some(channel) => {
                runtime.spawn(async move {
                    let result = create_remote_dir(&channel, &path).await.map(|_| ());
                    let _ = sender.send(SftpEvent::MoveFolderMade {
                        dir: event_dir,
                        path: event_path,
                        result,
                    });
                });
            }
            None => {
                runtime.spawn_blocking(move || {
                    let result = local_ops::mkdir(&path);
                    let _ = sender.send(SftpEvent::MoveFolderMade {
                        dir: event_dir,
                        path: event_path,
                        result,
                    });
                });
            }
        }
        cx.notify();
    }

    pub(super) fn apply_move_folder_made(
        &mut self,
        dir: String,
        path: String,
        result: Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        let Some(Dialog::MoveTo(d)) = &mut self.dialog else {
            return;
        };
        d.creating = false;
        match result {
            Ok(()) => {
                d.new_folder = None;
                d.new_folder_error = None;
                // 重新读一遍刚建文件夹的那个目录，读回来后选中新文件夹。
                if same_path(d.side, &dir, &d.browse.dir) {
                    self.spawn_browse(dir, false, Some(path));
                }
            }
            Err(err) => d.new_folder_error = Some(err),
        }
        cx.notify();
    }

    // ---- 开始移动 ----

    /// 「移动」按钮：目标写法没问题 → 先确认目标文件夹存在，再开始。
    pub(super) fn confirm_move_to(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::MoveTo(d)) = &mut self.dialog else {
            return;
        };
        if d.confirming || d.creating {
            return;
        }
        let text = d.input.read(cx).value().to_string();
        let DestCheck::Ok(dest) = check_destination(d.side, &d.entries, &text) else {
            return;
        };
        // 列表正好显示着这个文件夹（读成功了）：它存在，直接开始；否则列一遍它来确认。
        let verified = d.browse.error.is_none()
            && !d.browse.loading
            && same_path(d.side, &dest, &d.browse.dir);
        if !verified {
            self.spawn_browse(dest, true, None);
            cx.notify();
            return;
        }
        let (side, entries) = (d.side, d.entries.clone());
        self.dialog = None;
        self.start_move(side, entries, dest, window, cx);
    }

    fn start_move(
        &mut self,
        side: Side,
        entries: Vec<Entry>,
        dest: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_move;
        self.next_move += 1;
        self.moving = Some(MoveJob::new(id, side, dest, entries));
        self.focus_pane(side, window, cx);
        self.step_job(|job, blocked| job.advance(blocked), window, cx);
    }

    // ---- 逐项执行 ----

    /// 这一项现在能不能动：正在编辑器里开着、正在传输的跳过，返回原因。
    fn move_block_reason(&self, side: Side, entry: &Entry) -> Option<String> {
        if side == Side::Remote
            && let Some(open) = open_files::open_at_or_under(self.host_id, &entry.path)
        {
            return Some(if same_path(side, &open, &entry.path) {
                format!("{} 正在编辑，先保存并关闭再移动", entry.name)
            } else {
                format!(
                    "{} 里的 {} 正在编辑，先保存并关闭再移动",
                    entry.name,
                    file_name(&open)
                )
            });
        }
        self.transfers
            .iter()
            .any(|row| {
                !row.state.is_terminal()
                    && transfer_blocks(side, &entry.path, row.direction, &row.source, &row.target)
            })
            .then(|| format!("{} 正在传输，等传输结束再移动", entry.name))
    }

    /// 把任务取出来问状态机“下一步”，再执行它说的。
    fn step_job(
        &mut self,
        decide: impl FnOnce(&mut MoveJob, &dyn Fn(&Entry) -> Option<String>) -> Next,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mut job) = self.moving.take() else {
            return;
        };
        let side = job.side;
        let next = {
            let this = &*self;
            let blocked = |entry: &Entry| this.move_block_reason(side, entry);
            decide(&mut job, &blocked)
        };
        let id = job.id;
        self.run_next(job, id, next, window, cx);
    }

    fn run_next(
        &mut self,
        job: MoveJob,
        id: u64,
        next: Next,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match next {
            Next::Attempt {
                entry,
                target,
                overwrite,
            } => {
                let side = job.side;
                self.moving = Some(job);
                self.say(format!("正在移动 {}…", entry.name));
                if let Err(err) =
                    self.spawn_move_step(side, id, entry.path.clone(), target, overwrite)
                {
                    // 发不出去（没连上等）：这一项算失败，接着走。
                    self.step_job_after(id, StepResult::Failed(err), window, cx);
                }
            }
            Next::Ask(ask) => {
                self.moving = Some(job);
                self.say(format!("{} 在目标里已有同名项，等你选择", ask.name));
                self.dialog = Some(Dialog::MoveConflict(MoveConflict {
                    ask,
                    apply_rest: false,
                }));
                window.focus(&self.dialog_focus, cx);
            }
            Next::Done(tally) => {
                let side = job.side;
                let dest_name = display_name(side, &job.dest);
                let summary = tally.summary(&dest_name);
                if tally.has_problems() {
                    self.warn(summary.clone());
                    self.dialog = Some(Dialog::MoveSummary(MoveReport {
                        summary: summary.clone(),
                        lines: tally.detail_lines(),
                    }));
                    window.focus(&self.dialog_focus, cx);
                } else {
                    self.say(summary.clone());
                    self.focus_pane(side, window, cx);
                }
                // 源文件夹里的东西变了：刷新这一栏，并把汇总留在刷新后的状态栏里。
                self.refresh_with_notice(side, summary, cx);
            }
        }
        cx.notify();
    }

    fn spawn_move_step(
        &mut self,
        side: Side,
        job: u64,
        from: String,
        to: String,
        overwrite: bool,
    ) -> Result<(), String> {
        let runtime = runtime()?;
        let sender = self.sender.clone();
        match side {
            Side::Remote => {
                let channel = self.connected_channel()?;
                runtime.spawn(async move {
                    let result = remote_step(&channel, &from, &to, overwrite).await;
                    let _ = sender.send(SftpEvent::MoveStep { job, result });
                });
            }
            Side::Local => {
                runtime.spawn_blocking(move || {
                    let result = local_step(&from, &to, overwrite);
                    let _ = sender.send(SftpEvent::MoveStep { job, result });
                });
            }
        }
        Ok(())
    }

    /// 一项的结果回来了。
    pub(super) fn apply_move_step(
        &mut self,
        job: u64,
        result: StepResult,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_job_after(job, result, window, cx);
    }

    fn step_job_after(
        &mut self,
        job: u64,
        result: StepResult,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.moving.as_ref().is_none_or(|current| current.id != job) {
            return;
        }
        self.step_job(move |j, blocked| j.on_step(result, blocked), window, cx);
    }

    // ---- 同名询问 ----

    /// 同名对话框上的选择。
    pub(super) fn answer_move_conflict(
        &mut self,
        choice: Choice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Dialog::MoveConflict(conflict)) = self.dialog.take() else {
            return;
        };
        let apply_rest = conflict.apply_rest && conflict.ask.remaining > 0;
        self.step_job(
            move |j, blocked| j.answer(choice, apply_rest, blocked),
            window,
            cx,
        );
    }

    /// 「对其余同名项都这样做」勾上 / 取消。
    pub(super) fn set_move_apply_rest(&mut self, on: bool, cx: &mut Context<Self>) {
        if let Some(Dialog::MoveConflict(conflict)) = &mut self.dialog {
            conflict.apply_rest = on;
            cx.notify();
        }
    }

    /// 「取消移动」/ Esc：这一项和后面的都不动了。
    pub(super) fn abort_move(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.dialog, Some(Dialog::MoveConflict(_))) {
            self.dialog = None;
        }
        let Some(mut job) = self.moving.take() else {
            return;
        };
        let next = job.abort();
        let id = job.id;
        self.run_next(job, id, next, window, cx);
    }
}

/// 目标文件夹给人看的名字：路径最后一段，根目录就写路径本身。
fn display_name(side: Side, dest: &str) -> String {
    let name = match side {
        Side::Remote => file_name(dest),
        Side::Local => {
            let tail = dest.trim_end_matches('\\');
            tail.rsplit('\\').next().unwrap_or(tail).to_owned()
        }
    };
    if name.is_empty() {
        dest.to_owned()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::{Side, display_name};

    #[test]
    fn the_destination_is_named_by_its_last_segment() {
        assert_eq!(display_name(Side::Remote, "/srv/app/docs"), "docs");
        assert_eq!(display_name(Side::Remote, "/"), "/");
        assert_eq!(
            display_name(Side::Local, "D:\\work\\文档 归档"),
            "文档 归档"
        );
        assert_eq!(display_name(Side::Local, "D:\\"), "D:");
    }
}
