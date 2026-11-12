//! 主窗口这一侧的「拖标签出窗口 / 合并」（V2-U1 第 7 条；通用部分在 `crate::tab_drag`）：
//!
//! - 终端标签拖出窗口边界松手 = 弹出成独立终端窗口（`term_window`），交接的是同一个 `Tab`，会话不重连；
//!   拖到另一个终端窗口的标签栏上松手 = 合并过去；
//! - 文件（SFTP）标签拖出窗口边界松手 = 放进文件窗口（`file_window`，G08b 的交接：同一个视图实体，
//!   传输不中断）；拖到文件窗口的标签栏上 = 同样；
//! - 别的窗口的标签拖到主窗口的标签栏上松手 = 合并回主窗口（`adopt_terminal_tab`、`adopt_sftp_view`）；
//! - 拖动中：被拖的标签变淡、指针在窗口里时有跟着走的标签提示、出了窗口时标签栏上写松手会怎样；
//!   Esc 取消。AI 正在这个终端标签里工作时不允许拖走（`tab_drag::blocked_for_ai`：对话面板只在主窗口，
//!   终端移走后对话和终端分在两处），拖到窗口外时标签栏上的提示改成「不能移出」，松手标签留在原处。
//! - 本窗口标签栏里换位：指针在标题栏那一条里时，被拖的标签随指针越过相邻标签的中线实时换位（几何见
//!   `tab_drag::reorder_target`），松手保留、Esc 还原。标签条里终端标签和文件标签是固定的两段（终端在前、
//!   文件在后，编辑器标签不能拖），各自只在自己那一段里换位，不会跨段；换位不会中断 AI 对话或传输（挪的只是
//!   位置，会话、视图实体都没动），所以 AI 正在工作的标签也可以在本窗口里换位。

use ::gpui::{App, MouseMoveEvent, MouseUpEvent, Window};
use gpui_kit::*;

use super::MainWindow;
use crate::tab_drag::{self, DragKey, Drop, TabKind};

impl MainWindow {
    /// 在终端标签上按下左键（记下起点，还没算拖动）。
    pub(super) fn press_terminal_tab(
        &mut self,
        tab_id: u64,
        title: String,
        pos: (f32, f32),
        cx: &App,
    ) {
        self.tab_dragger
            .press(DragKey::Terminal(tab_id), title, pos);
        if self.workspace.read(cx).tab_ai_active(tab_id) {
            self.tab_dragger.pin();
        }
    }

    /// 在文件标签上按下左键。
    pub(super) fn press_sftp_tab(&mut self, id: u64, title: String, pos: (f32, f32)) {
        self.tab_dragger.press(DragKey::Sftp(id), title, pos);
    }

    /// 被拖的标签（拖动中才有；标签栏据此把它画淡）。
    pub(super) fn dragging_tab(&self) -> Option<DragKey> {
        self.tab_dragger.key()
    }

    pub(super) fn on_tab_drag_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let local = (f32::from(event.position.x), f32::from(event.position.y));
        let left = event.pressed_button == Some(::gpui::MouseButton::Left);
        let moved = self.tab_dragger.moved(local, left, window, cx);
        if let Some(original) = moved.restore {
            self.restore_tab_order(&original, cx);
        }
        if moved.dragging {
            self.reorder_tab_under_pointer(local, tab_drag::window_size(window), cx);
        }
        if moved.redraw {
            cx.notify();
        }
    }

    /// 这一种标签现在在标签条上的顺序（只含同一段）。
    fn tab_keys(&self, kind: TabKind, cx: &Context<Self>) -> Vec<DragKey> {
        match kind {
            TabKind::Terminal => self
                .workspace
                .read(cx)
                .tab_ids()
                .into_iter()
                .map(DragKey::Terminal)
                .collect(),
            TabKind::Sftp => self
                .docks
                .sftp_tabs
                .iter()
                .map(|tab| DragKey::Sftp(tab.id))
                .collect(),
        }
    }

    /// 把这一段里第 `from` 个挪到第 `to` 个。
    fn move_tab_in_strip(&mut self, kind: TabKind, from: usize, to: usize, cx: &mut Context<Self>) {
        match kind {
            TabKind::Terminal => {
                self.workspace
                    .update(cx, |ws, cx| ws.move_tab(from, to, cx));
            }
            TabKind::Sftp => {
                crate::tab_set::move_within(&mut self.docks.sftp_tabs, from, to);
            }
        }
        cx.notify();
    }

    /// 拖动中指针在标题栏上：被拖的标签越过相邻标签的中线就换位（只在它自己那一段里）。
    fn reorder_tab_under_pointer(
        &mut self,
        local: (f32, f32),
        size: (f32, f32),
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self.tab_dragger.key() else {
            return;
        };
        let kind = key.kind();
        let keys = self.tab_keys(kind, cx);
        if let Some((from, to)) = self.tab_dragger.reorder_move(local, size, &keys) {
            self.move_tab_in_strip(kind, from, to, cx);
        }
    }

    /// Esc：把这一段标签还原成拖动开始前的顺序。
    fn restore_tab_order(&mut self, original: &[DragKey], cx: &mut Context<Self>) {
        let Some(kind) = original.first().map(|key| key.kind()) else {
            return;
        };
        let current = self.tab_keys(kind, cx);
        for (from, to) in tab_drag::restore_moves(&current, original) {
            self.move_tab_in_strip(kind, from, to, cx);
        }
    }

    pub(super) fn on_tab_drag_up(
        &mut self,
        event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let local = (f32::from(event.position.x), f32::from(event.position.y));
        if let Some((key, drop)) = self.tab_dragger.release(cx) {
            self.finish_tab_drag(key, drop, local, window, cx);
        }
        cx.notify();
    }

    pub(super) fn cancel_tab_drag(&mut self, cx: &mut Context<Self>) {
        if let Some(original) = self.tab_dragger.cancel(cx) {
            self.restore_tab_order(&original, cx);
        }
        cx.notify();
    }

    /// 松手之后：弹出 / 合并 / 什么也不做。
    pub(crate) fn finish_tab_drag(
        &mut self,
        key: DragKey,
        drop: Drop,
        local: (f32, f32),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match key {
            DragKey::Terminal(tab_id) => self.drop_terminal_tab(tab_id, drop, local, window, cx),
            DragKey::Sftp(id) => {
                // 文件标签只能去文件窗口：松在窗口外空地、或松在文件窗口的标签栏上。
                let to_file_window = match drop {
                    Drop::TearOff => true,
                    Drop::Merge(target) => crate::file_window::window_handle(cx)
                        .is_some_and(|handle| handle.window_id() == target),
                    Drop::Nothing => false,
                };
                if to_file_window {
                    self.move_sftp_tab_to_file_window(id, window, cx);
                }
            }
        }
    }

    fn drop_terminal_tab(
        &mut self,
        tab_id: u64,
        drop: Drop,
        local: (f32, f32),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(drop, Drop::Nothing) {
            return;
        }
        // 文件窗口的标签栏不收终端标签。
        if let Drop::Merge(target) = drop
            && crate::file_window::window_handle(cx)
                .is_some_and(|handle| handle.window_id() == target)
        {
            return;
        }
        // 按下时 AI 还没在工作、拖动中才开始的：松手这时再挡一次（拖动中没提示过，补一条通知）。
        if self.workspace.read(cx).tab_ai_active(tab_id)
            && tab_drag::blocked_for_ai(drop, tab_drag::main_window_id(cx))
        {
            self.notice(tab_drag::ai_busy_text(), window, cx);
            return;
        }
        let Some((index, tab)) = self
            .workspace
            .update(cx, |ws, cx| ws.take_tab_by_id(tab_id, cx))
        else {
            return;
        };
        let outcome = match drop {
            Drop::Merge(target) => crate::term_window::merge_into(cx, target, tab),
            _ => crate::term_window::open(cx, tab, Some(tab_drag::screen_point(window, local)))
                .map(|_| ()),
        };
        match outcome {
            Ok(()) => self.after_tabs_closed(window, cx),
            // 目标没了 / 窗口没建起来：标签放回原来的位置，会话还在。
            Err(tab) => self
                .workspace
                .update(cx, |ws, cx| ws.restore_tab(index, tab, cx)),
        }
    }

    /// 别的窗口把一个终端标签合并过来：同一个 `Tab`，会话不断；选中它并切到终端页。
    pub(crate) fn adopt_terminal_tab(
        &mut self,
        tab: crate::terminal_workspace::Tab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace.update(cx, |ws, cx| ws.adopt_tab(tab, cx));
        let last = self.workspace.read(cx).tab_count().saturating_sub(1);
        self.workspace.update(cx, |ws, cx| ws.select_tab(last, cx));
        self.show_view(super::nav::MainView::Terminal, window, cx);
        window.activate_window();
    }
}
