//! 主窗口这一侧的「拖标签出窗口 / 合并」（V2-U1 第 7 条；通用部分在 `crate::tab_drag`）：
//!
//! - 终端标签拖出窗口边界松手 = 弹出成独立终端窗口（`term_window`），交接的是同一个 `Tab`，会话不重连；
//!   拖到另一个终端窗口的标签栏上松手 = 合并过去；
//! - 文件（SFTP）标签拖出窗口边界松手 = 放进文件窗口（`file_window`，G08b 的交接：同一个视图实体，
//!   传输不中断）；拖到文件窗口的标签栏上 = 同样；
//! - 别的窗口的标签拖到主窗口的标签栏上松手 = 合并回主窗口（`adopt_terminal_tab`、`adopt_sftp_view`）；
//! - 拖动中：被拖的标签变淡、指针在窗口里时有跟着走的标签提示、出了窗口时标签栏上写松手会怎样；
//!   Esc 取消。AI 正在这个终端标签里工作时不允许拖走（对话跟着标签，拖走会把它中断）。

use ::gpui::{MouseMoveEvent, MouseUpEvent, Window};
use gpui_kit::*;

use super::MainWindow;
use crate::tab_drag::{self, DragKey, Drop};

impl MainWindow {
    /// 在终端标签上按下左键（记下起点，还没算拖动）。
    pub(super) fn press_terminal_tab(&mut self, tab_id: u64, title: String, pos: (f32, f32)) {
        self.tab_dragger
            .press(DragKey::Terminal(tab_id), title, pos);
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
        if self.tab_dragger.moved(local, left, window, cx) {
            cx.notify();
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
        self.tab_dragger.cancel(cx);
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
        // AI 正在这个标签里跑：对话跟着标签，拖走会中断它。
        let ai_busy = self
            .docks
            .ai
            .get(&tab_id)
            .is_some_and(|chat| chat.read(cx).is_turn_running());
        if ai_busy {
            self.notice_error(
                "AI 正在这个终端里工作，先停止它或等它结束，再把标签拖出窗口",
                window,
                cx,
            );
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
