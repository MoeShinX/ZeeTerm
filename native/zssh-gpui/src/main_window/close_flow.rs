//! 主窗口关闭标签 / 分屏窗格前的确认（V2-U44 A）：所有「用户手动关」的入口（标签上的 ✕、中键、右键菜单、
//! Ctrl+Shift+W、窗格右键「关闭窗格」、标签溢出菜单里的 ✕）都先走 [`MainWindow::request_close`]：
//! 设置开着并且范围里有正在使用的连接就弹确认框（[`super::modal::Modal::CloseTabs`]），否则直接关。
//!
//! 不问的路径（程序内部关闭、拖出 / 合并 / 移回、会话自己结束、退出应用）不经过这里，
//! 各自照旧（比如调试脚本的 `close:` 步骤走 `close_tab_now`）。

use ::gpui::Window;
use gpui_kit::*;

use super::MainWindow;
use super::modal::{CloseAsk, Modal};
use crate::close_confirm::{CloseRequest, CloseScope};

impl MainWindow {
    /// 用户手动关闭：要问就弹确认框（本窗口一次只有一个对话框，已经有别的对话框开着时不叠，什么也不做），
    /// 不用问就直接关。返回有没有「动」（弹了框或者关了东西）。
    pub(crate) fn request_close(
        &mut self,
        request: CloseRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(summary) = self.workspace.read(cx).close_confirmation(&request) else {
            return self.run_close(&request, window, cx) > 0;
        };
        if self.modal.is_some() {
            return false;
        }
        self.modal = Some(Modal::CloseTabs(CloseAsk {
            request,
            text: summary.text(),
            dont_ask: false,
        }));
        // 键盘离开终端：确认框开着时敲的字不能落进下面还连着的会话（Esc / Enter 由窗口级的对话框按键处理）。
        window.focus(&self.focus, cx);
        cx.notify();
        true
    }

    /// 真正关（不问）。返回关掉了几个标签（窗格请求关掉了算 1）。
    fn run_close(
        &mut self,
        request: &CloseRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let closed = self
            .workspace
            .update(cx, |ws, cx| ws.perform_close(request, cx));
        if closed > 0 {
            self.after_tabs_closed(window, cx);
        }
        closed
    }

    /// 确认框点了「关闭」（或回车）：按打开时记下的范围关；勾了「不再询问」就把设置关掉。
    /// 范围里的标签已经被别处关了 / 窗格没了就跳过，绝不改关别的。
    pub(super) fn run_close_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::CloseTabs(ask)) = self.modal.take() else {
            return;
        };
        if ask.dont_ask {
            self.workspace
                .update(cx, |ws, cx| ws.disable_confirm_close_tab(cx));
        }
        self.run_close(&ask.request, window, cx);
        self.focus_content(window, cx);
        cx.notify();
    }

    /// 确认框里勾 / 取消勾「不再询问」。
    pub(super) fn set_close_dont_ask(&mut self, ticked: bool, cx: &mut Context<Self>) {
        if let Some(Modal::CloseTabs(ask)) = &mut self.modal {
            ask.dont_ask = ticked;
            cx.notify();
        }
    }

    // ---- 各入口 ----

    /// 标签上的 ✕、中键、右键「关闭」、标签溢出菜单里的 ✕：关第 `index` 个标签。
    pub(crate) fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(request) = self.workspace.read(cx).close_request_for_tab(index) {
            self.request_close(request, window, cx);
        }
    }

    /// 右键「关闭其他 / 左侧 / 右侧 / 全部」。
    pub(crate) fn close_tabs(
        &mut self,
        index: usize,
        scope: CloseScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(request) = self
            .workspace
            .read(cx)
            .close_request_for_scope(index, scope)
        {
            self.request_close(request, window, cx);
        }
    }

    /// 程序内部关一个标签（调试脚本、自动化）：不问用户。
    #[cfg(debug_assertions)]
    pub(crate) fn close_tab_now(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let closed = self.workspace.update(cx, |ws, cx| ws.close_tab(index, cx));
        if closed {
            self.after_tabs_closed(window, cx);
        }
    }

    /// Ctrl+Shift+W：分屏时关聚焦窗格，否则关标签。
    /// 没有可关的（或者不在终端页）时返回 `false`，让调用方决定要不要退一步去关窗口
    /// （macOS 的 Cmd+W 需要这个区分）。弹了确认框也算「有动」。
    pub(crate) fn close_focused_pane_or_tab(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.nav.view != super::MainView::Terminal {
            return false;
        }
        let Some(request) = self.workspace.read(cx).focused_close_request() else {
            return false;
        };
        // 已经有别的对话框开着时 Ctrl+Shift+W 根本到不了这里（窗口级按键先给对话框）；万一到了也算「接手了」，
        // 不要让 macOS 的 Cmd+W 顺手退出应用。
        self.request_close(request, window, cx) || self.modal.is_some()
    }

    /// 工作区在右键「关闭窗格」时交来的请求（见 `TerminalWorkspace::take_close_request`）。
    pub(super) fn take_workspace_close_request(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = self.workspace.update(cx, |ws, _| ws.take_close_request());
        if let Some(request) = request {
            self.request_close(request, window, cx);
        }
    }
}
