//! 文件栏的浏览和交互：换目录、后退前进、刷新、点击 / 双击 / 键盘、排序、路径栏编辑、
//! 右键点击时的选择处理。动作怎么执行（删除、改名、传输……）在 `ops` / `transfer`。

use std::path::PathBuf;

use ::gpui::{KeyDownEvent, Window};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::*;
use zssh_core::pathnorm::simplify_verbatim;
use zssh_core::sftp::DirEntry;
use zssh_core::sftp::path::{join, normalize};

use super::engine::resolve_local_path;
use super::local_listing::{LocalReply, LocalRequest, ScanError, accepts, request_listing};
use super::menu::{Side, not_utf8_reason};
use super::model::{Entry, NavMode, SortKey, local_parent, remote_parent};
use super::pane::PathEdit;
use super::{SftpDemo, SftpEvent, SftpMsg};
use crate::error_text::error_text;
use crate::i18n::t;
use crate::ssh_backend::runtime;

impl SftpDemo {
    // ---- 列表到了 ----

    /// 远端目录读回来了（首条、换目录、刷新都走这里）。
    pub(super) fn set_remote_listing(
        &mut self,
        path: String,
        entries: Vec<DirEntry>,
        mode: NavMode,
        notice: Option<String>,
    ) {
        let rows: Vec<Entry> = entries.iter().map(Entry::from_remote).collect();
        let count = rows.len();
        let same = self.remote.path == path;
        self.remote.set_listing(path.clone(), rows, same);
        if mode == NavMode::Visit {
            self.remote.history.visit(&path);
        }
        if !same {
            self.remote.scroll_to(0);
        }
        let items = crate::i18n::count(count as u64, t(SftpMsg::ItemCountSuffix), "item", "items");
        let text = match notice {
            Some(notice) => format!("{notice} · {items}"),
            None => items,
        };
        self.say(text);
    }

    // ---- 换目录 ----

    /// 换目录。远端的读取丢给 runtime，界面线程只改状态；本地是本地调用，当场读。
    pub(super) fn navigate(
        &mut self,
        side: Side,
        path: &str,
        mode: NavMode,
        cx: &mut Context<Self>,
    ) {
        match side {
            Side::Remote => {
                // 用户自己换了远端目录（刷新不算）：不再跟随终端目录。
                if mode != NavMode::Refresh {
                    self.manual_remote_navigation();
                }
                self.navigate_remote(path, mode, None, cx)
            }
            Side::Local => self.navigate_local(path, mode, None, cx),
        }
    }

    pub(super) fn navigate_remote(
        &mut self,
        path: &str,
        mode: NavMode,
        notice: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.fixture {
            self.fixture_navigate(path, mode, cx);
            return;
        }
        let Some(channel) = self.channel.clone() else {
            self.warn(t(SftpMsg::NotConnectedYet));
            cx.notify();
            return;
        };
        let trimmed = path.trim();
        if trimmed.is_empty() {
            self.warn(t(SftpMsg::NavPathEmptyWait));
            cx.notify();
            return;
        }
        let target = normalize(&join(&self.remote.path, trimmed));
        self.say(format!("{}{target}", t(SftpMsg::NavReadingPrefix)));
        self.remote.loading = true;
        // 每次读取取一个新序号：之前还没回来的读取从这一刻起作废。
        let seq = self.listing.next();
        match runtime() {
            Ok(runtime) => {
                let sender = self.sender.clone();
                runtime.spawn(async move {
                    let event = match channel.read_dir(&target).await {
                        Ok(entries) => SftpEvent::Listed {
                            seq,
                            path: target,
                            entries,
                            mode,
                            notice,
                        },
                        Err(err) => SftpEvent::ListFailed {
                            seq,
                            path: target,
                            mode,
                            message: error_text(&err),
                        },
                    };
                    let _ = sender.send(event);
                });
            }
            Err(err) => {
                self.remote.loading = false;
                self.warn(err);
            }
        }
        cx.notify();
    }

    /// 换本地目录。路径校验（`is_dir`）和目录读取都交给后台线程，这里只提交请求、挂上“正在读取”：
    /// 慢盘、大目录、失联的网络路径不会卡住窗口；用户又去了别处，之前那次的结果回来也会被丢弃。
    fn navigate_local(
        &mut self,
        path: &str,
        mode: NavMode,
        notice: Option<String>,
        cx: &mut Context<Self>,
    ) {
        // 给界面的路径不带 `\\?\` 前缀（纯字符串处理，不碰磁盘）。
        let target = if path.is_empty() {
            String::new()
        } else {
            simplify_verbatim(PathBuf::from(path)).display().to_string()
        };
        self.say(if target.is_empty() {
            t(SftpMsg::NavReadingThisComputer).to_owned()
        } else {
            format!("{}{target}", t(SftpMsg::NavReadingPrefix))
        });
        self.local.loading = true;
        let request = LocalRequest::Dir(target);
        if let Err(message) = self.submit_local(request, path.to_owned(), mode, notice, false) {
            self.local.loading = false;
            self.warn(message);
        }
        cx.notify();
    }

    /// 视图刚创建：后台定落点并读首屏（`wanted` 是复制标签带来的目录）。
    pub(super) fn request_local_start(&mut self, wanted: Option<String>) {
        let requested = wanted.clone().unwrap_or_default();
        if let Err(message) = self.submit_local(
            LocalRequest::Start(wanted),
            requested,
            NavMode::Visit,
            None,
            true,
        ) {
            self.local.loading = false;
            self.warn(format!("{}{message}", t(SftpMsg::NavLocalPrefix)));
        }
    }

    /// 把一次本地读取交给后台；读完以 [`SftpEvent::LocalListed`] 回到界面。
    fn submit_local(
        &mut self,
        request: LocalRequest,
        requested: String,
        mode: NavMode,
        notice: Option<String>,
        initial: bool,
    ) -> Result<(), String> {
        let sender = self.sender.clone();
        request_listing(
            &self.local_lister,
            &self.local_gate,
            request,
            move |reply| {
                let _ = sender.send(SftpEvent::LocalListed {
                    reply,
                    requested,
                    mode,
                    notice,
                    initial,
                });
            },
        )
        .map(|_| ())
    }

    /// 本地目录在后台读回来了。不是最新一次请求的结果（用户已经去了别处）直接丢掉。
    pub(super) fn apply_local_listed(
        &mut self,
        reply: LocalReply,
        requested: String,
        mode: NavMode,
        notice: Option<String>,
        initial: bool,
    ) {
        if !accepts(&self.local_gate, &reply) {
            return;
        }
        let target = reply.target;
        match reply.result {
            Ok(rows) => {
                let count = rows.len();
                let same = self.local.path == target;
                self.local.set_listing(target.clone(), rows, same);
                if mode == NavMode::Visit {
                    self.local.history.visit(&target);
                }
                if !same {
                    self.local.scroll_to(0);
                }
                match notice {
                    Some(notice) => {
                        self.say(format!("{notice} · {count}{}", t(SftpMsg::ItemCountSuffix)))
                    }
                    None => self.say(format!(
                        "{}{count}{}",
                        t(SftpMsg::NavLocalCountPrefix),
                        t(SftpMsg::ItemCountSuffix)
                    )),
                }
            }
            Err(error) => {
                self.local.loading = false;
                if mode == NavMode::History {
                    self.local.history.forget(&requested);
                }
                // 首屏读不出来：本地栏仍显示落点路径（列表是空的），不留一个看不出在哪的空栏。
                if initial {
                    self.local.path = target.clone();
                }
                match error {
                    ScanError::NotDir => {
                        self.warn(format!("{}{target}", t(SftpMsg::NavNotADirectoryPrefix)))
                    }
                    ScanError::Failed(err) => {
                        self.warn(format!("{}{err}", t(SftpMsg::NavLocalReadFailedPrefix)))
                    }
                }
            }
        }
    }

    pub(super) fn refresh(&mut self, side: Side, cx: &mut Context<Self>) {
        let path = self.pane(side).path.clone();
        match side {
            Side::Remote => {
                if !path.is_empty() {
                    self.navigate_remote(&path, NavMode::Refresh, None, cx);
                }
            }
            Side::Local => self.navigate_local(&path, NavMode::Refresh, None, cx),
        }
    }

    /// 刷新并把这句话留在刷新后的状态栏里（"N 项"会顶掉普通提示）。
    pub(super) fn refresh_with_notice(
        &mut self,
        side: Side,
        notice: String,
        cx: &mut Context<Self>,
    ) {
        let path = self.pane(side).path.clone();
        match side {
            Side::Remote => {
                if !path.is_empty() {
                    self.navigate_remote(&path, NavMode::Refresh, Some(notice), cx);
                }
            }
            Side::Local => self.navigate_local(&path, NavMode::Refresh, Some(notice), cx),
        }
    }

    /// 返回上级。远端根和本地“此电脑”没有上级，原地不动。
    pub(super) fn go_parent(&mut self, side: Side, cx: &mut Context<Self>) {
        let pane = self.pane(side);
        match side {
            Side::Remote => {
                if pane.path.is_empty() || pane.path == "/" {
                    return;
                }
                let target = remote_parent(&pane.path);
                self.navigate(side, &target, NavMode::Visit, cx);
            }
            Side::Local => {
                if pane.path.is_empty() {
                    return;
                }
                let target = local_parent(&pane.path);
                self.navigate(side, &target, NavMode::Visit, cx);
            }
        }
    }

    /// 后退（-1）/ 前进（+1）一步。
    pub(super) fn step_history(&mut self, side: Side, delta: isize, cx: &mut Context<Self>) {
        if let Some(path) = self.pane_mut(side).history.step(delta) {
            self.navigate(side, &path, NavMode::History, cx);
        }
    }

    /// 路径栏里敲出来的路径：远端相对当前目录，本地绝对路径直接用、相对的按当前目录拼。
    pub(super) fn browse(&mut self, side: Side, typed: &str, cx: &mut Context<Self>) {
        match side {
            Side::Remote => self.navigate(side, typed, NavMode::Visit, cx),
            Side::Local => {
                let base = PathBuf::from(&self.local.path);
                match resolve_local_path(&base, typed) {
                    Some(dir) => {
                        self.navigate(side, &dir.display().to_string(), NavMode::Visit, cx)
                    }
                    None => {
                        self.warn(t(SftpMsg::NavLocalPathEmpty));
                        cx.notify();
                    }
                }
            }
        }
    }

    // ---- 栏的外观开关 ----

    pub(super) fn toggle_sort(&mut self, side: Side, key: SortKey, cx: &mut Context<Self>) {
        let pane = self.pane_mut(side);
        pane.order = pane.order.toggled(key);
        pane.rebuild();
        cx.notify();
    }

    pub(super) fn toggle_hidden(&mut self, side: Side, cx: &mut Context<Self>) {
        let pane = self.pane_mut(side);
        pane.filter.show_hidden = !pane.filter.show_hidden;
        pane.rebuild();
        cx.notify();
    }

    /// 把键盘焦点交给这一栏的列表。
    pub(super) fn focus_pane(&mut self, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        self.focused_side = side;
        let handle = self.pane(side).focus.clone();
        window.focus(&handle, cx);
    }

    // ---- 路径栏编辑 ----

    pub(super) fn begin_path_edit(
        &mut self,
        side: Side,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pane(side).path_edit.is_some() {
            return;
        }
        let current = self.pane(side).path.clone();
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(t(SftpMsg::NavPathPlaceholder)));
        input.update(cx, |state, cx| state.set_value(current, window, cx));
        let subscriptions = vec![
            cx.observe(&input, |_this, _input, cx| cx.notify()),
            cx.subscribe_in(
                &input,
                window,
                move |this, input, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => {
                        let typed = input.read(cx).value().to_string();
                        this.finish_path_edit(side, Some(typed), window, cx);
                    }
                    // 点到别处：和回车一样去那个路径（0.2.14 的失焦行为）。
                    InputEvent::Blur if this.pane(side).path_edit.is_some() => {
                        let typed = input.read(cx).value().to_string();
                        this.finish_path_edit(side, Some(typed), window, cx);
                    }
                    _ => {}
                },
            ),
        ];
        self.pane_mut(side).path_edit = Some(PathEdit {
            input: input.clone(),
            _subscriptions: subscriptions,
        });
        input.update(cx, |state, cx| {
            state.focus(window, cx);
            state.select_all(window, cx);
        });
        cx.notify();
    }

    /// 结束路径栏编辑：`Some(路径)` = 去那里（和当前不同才去），`None` = 取消。
    pub(super) fn finish_path_edit(
        &mut self,
        side: Side,
        typed: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pane_mut(side).path_edit.take().is_none() {
            return;
        }
        self.focus_pane(side, window, cx);
        if let Some(typed) = typed {
            let target = typed.trim().to_owned();
            if !target.is_empty() && target != self.pane(side).path {
                self.browse(side, &target, cx);
            }
        }
        cx.notify();
    }

    // ---- 鼠标 ----

    /// 左键按下一行：选择（Ctrl / Shift 多选）；双击进入 / 打开。
    pub(super) fn row_mouse_down(
        &mut self,
        side: Side,
        index: usize,
        modifiers: ::gpui::Modifiers,
        click_count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_pane(side, window, cx);
        let pane = self.pane_mut(side);
        let Some(entry) = pane.rows.get(index).cloned() else {
            return;
        };
        let (ctrl, shift) = (modifiers.control || modifiers.platform, modifiers.shift);
        // 在多选里的一项上按下：先不动选择（可能是要拖整个选择），松手没拖再收成它。
        pane.pending_collapse = None;
        if click_count < 2
            && !ctrl
            && !shift
            && pane.selection.paths.len() > 1
            && pane.selection.contains(&entry)
        {
            pane.pending_collapse = Some(entry.path.clone());
            pane.selection.focused = Some(entry.path.clone());
            cx.notify();
            return;
        }
        // 双击：这一行先成为唯一选中的（第一下点已经选了），然后进入 / 打开。
        if click_count >= 2 && !ctrl && !shift {
            if !pane.selection.contains(&entry) || pane.selection.paths.len() != 1 {
                let rows = std::mem::take(&mut pane.rows);
                pane.selection.click(&rows, index, false, false);
                pane.rows = rows;
            }
            self.activate(side, &entry, window, cx);
            cx.notify();
            return;
        }
        let rows = std::mem::take(&mut pane.rows);
        pane.selection.click(&rows, index, ctrl, shift);
        pane.rows = rows;
        cx.notify();
    }

    /// 左键在一行上松开：按下时留着没动的多选，没拖动就收成这一行。
    pub(super) fn row_mouse_up(&mut self, side: Side, index: usize, cx: &mut Context<Self>) {
        let pane = self.pane_mut(side);
        let Some(pending) = pane.pending_collapse.take() else {
            return;
        };
        let rows = std::mem::take(&mut pane.rows);
        if rows.get(index).is_some_and(|row| row.path == pending) {
            pane.selection.click(&rows, index, false, false);
        }
        pane.rows = rows;
        cx.notify();
    }

    /// 右键按在一行上：没选中的换成它，已选中的保持（对整个选择操作）。
    pub(super) fn row_right_down(
        &mut self,
        side: Side,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_pane(side, window, cx);
        if side == Side::Remote {
            self.ensure_archive_caps(cx);
        }
        let pane = self.pane_mut(side);
        let rows = std::mem::take(&mut pane.rows);
        pane.selection.right_click(&rows, index);
        pane.rows = rows;
        pane.right_hit_row = true;
        cx.notify();
    }

    /// 右键按在列表空白处（行先处理过的不算）：清掉选择，菜单只给“粘贴 / 新建 / 刷新 / 属性”。
    pub(super) fn blank_right_down(
        &mut self,
        side: Side,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_pane(side, window, cx);
        let pane = self.pane_mut(side);
        if pane.right_hit_row {
            pane.right_hit_row = false;
            return;
        }
        if side == Side::Remote {
            self.ensure_archive_caps(cx);
        }
        let pane = self.pane_mut(side);
        pane.selection.clear();
        cx.notify();
    }

    // ---- 双击 / 回车 ----

    /// 进入目录、打开文件。
    pub(super) fn activate(
        &mut self,
        side: Side,
        entry: &Entry,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !entry.name_is_utf8 {
            self.warn(not_utf8_reason());
            return;
        }
        // 目录直接进；符号链接看它指向的类型（断链不动）。
        if entry.is_enterable() {
            // 进了新目录，上一个目录的过滤不带过去（0.2.14 也是换目录就重新列）。
            self.navigate(side, &entry.path, NavMode::Visit, cx);
            return;
        }
        if entry.is_file_like() {
            self.open_file(side, entry, cx);
        }
    }

    /// 双击文件：远端用内置编辑器打开；本地还没有内置编辑器标签，说清楚。
    fn open_file(&mut self, side: Side, entry: &Entry, cx: &mut Context<Self>) {
        match side {
            Side::Remote => {
                if let Err(err) = self.start_edit(entry.path.clone(), cx) {
                    self.warn(err);
                }
            }
            Side::Local => self.warn(t(SftpMsg::NavLocalEditorNotReady)),
        }
    }

    // ---- 键盘 ----

    /// 列表上的按键（0.2.14 `SftpFilePane` 的 `handleKeyDown`）。返回是否处理了这个键。
    pub(super) fn key_down(
        &mut self,
        side: Side,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.dialog.is_some() || self.pane(side).path_edit.is_some() {
            return false;
        }
        self.focused_side = side;
        let key = event.keystroke.key.to_ascii_lowercase();
        let m = event.keystroke.modifiers;
        let command = (m.control || m.platform) && !m.alt && !m.shift;
        if m.alt && !m.control && !m.shift && (key == "left" || key == "right") {
            self.step_history(side, if key == "left" { -1 } else { 1 }, cx);
            return true;
        }
        if command && key == "l" {
            self.begin_path_edit(side, window, cx);
            return true;
        }
        // 复制 / 粘贴：焦点在文件栏时的 Ctrl+C / Ctrl+V（系统剪贴板不动，用应用内剪贴板）。
        if command && key == "c" {
            // 没有选中任何东西时什么也不做（同 0.2.14）。
            let selected = self.pane(side).selected();
            if !selected.is_empty() {
                self.copy_selection(side, &selected);
                cx.notify();
            }
            return true;
        }
        if command && key == "v" {
            self.paste_here(side, cx);
            cx.notify();
            return true;
        }
        if m.alt && !m.control && key == "enter" {
            // Alt+Enter = 属性，不是“回车打开”。
            let selected = self.pane(side).selected();
            self.open_properties(side, selected, window, cx);
            return true;
        }
        match key.as_str() {
            "down" | "up" if !m.alt && !m.control => {
                let delta = if key == "down" { 1 } else { -1 };
                self.move_focus(side, delta, m.shift, cx);
                true
            }
            "pagedown" | "pageup" => {
                let delta = if key == "pagedown" { 10 } else { -10 };
                self.move_focus(side, delta, m.shift, cx);
                true
            }
            "home" | "end" if !m.alt && !m.control => {
                let pane = self.pane_mut(side);
                let target = if key == "home" {
                    0
                } else {
                    pane.rows.len().saturating_sub(1)
                };
                let rows = std::mem::take(&mut pane.rows);
                let moved = pane.selection.jump(&rows, target, m.shift);
                pane.rows = rows;
                if let Some(index) = moved {
                    pane.scroll_to(index);
                }
                cx.notify();
                true
            }
            "enter" if !m.alt && !m.control && !m.shift => {
                let focused = self.focused_entry(side);
                if let Some(entry) = focused {
                    self.activate(side, &entry, window, cx);
                }
                true
            }
            "backspace" if !m.alt && !m.control => {
                self.go_parent(side, cx);
                true
            }
            "delete" if !m.alt && !m.control => {
                let selected = self.pane(side).selected();
                if !selected.is_empty() {
                    self.ask_delete(side, selected, window, cx);
                }
                true
            }
            "f2" => {
                if let Some(entry) = self.focused_entry(side) {
                    self.open_rename(side, entry, window, cx);
                }
                true
            }
            "f5" => {
                self.refresh(side, cx);
                true
            }
            "a" if command => {
                let pane = self.pane_mut(side);
                let rows = std::mem::take(&mut pane.rows);
                pane.selection.select_all(&rows);
                pane.rows = rows;
                cx.notify();
                true
            }
            _ => false,
        }
    }

    fn move_focus(&mut self, side: Side, delta: isize, shift: bool, cx: &mut Context<Self>) {
        let pane = self.pane_mut(side);
        let rows = std::mem::take(&mut pane.rows);
        let moved = pane.selection.arrow(&rows, delta, shift);
        pane.rows = rows;
        if let Some(index) = moved {
            pane.scroll_to(index);
        }
        cx.notify();
    }

    /// 键盘焦点所在的行；没有就看是不是只选了一个。
    pub(super) fn focused_entry(&self, side: Side) -> Option<Entry> {
        let pane = self.pane(side);
        if let Some(index) = pane.focused_index() {
            return pane.rows.get(index).cloned();
        }
        let selected = pane.selected();
        (selected.len() == 1).then(|| selected[0].clone())
    }
}
