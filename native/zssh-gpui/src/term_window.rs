//! 独立终端窗口（V2-U1 第 7 条）：把主窗口顶部的终端标签拖出窗口边界松手，标签就弹出成这样一个窗口。
//!
//! 里面是同一个 [`TerminalWorkspace`]（终端、分屏、选区、粘贴、鼠标、高亮都是主窗口那一份代码），
//! 窗口自己只多一个自绘标题栏：窗口里的标签（可以切换、关闭、再拖出去 / 拖回主窗口）和最小化 / 最大化 /
//! 关闭。主窗口的 AI 面板、快捷命令栏、撰写栏、状态栏、主机树不在这里（这是单纯的终端窗口）。
//!
//! 交接的是同一个 `Tab`：窗格里的会话驱动、已认证的连接、回滚内容原样过去，不重连。
//! 关窗口 = 关掉里面的标签（结束它们的会话），和关标签一样。最后一个标签走了（关掉或拖回主窗口）窗口自己关。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ::gpui::{
    App, Entity, FocusHandle, Global, Hsla, MouseButton, SharedString, TitlebarOptions, Window,
    WindowBounds, WindowHandle, WindowOptions, point, px,
};
use gpui_kit::component::TitleBar;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app_theme::Tokens;
use crate::close_confirm::{CloseRequest, CloseText};
use crate::i18n::{Msg, t};
use crate::icons::Ico;
use crate::main_window::tab_strip;
use crate::main_window::titlebar::{
    back_button, back_button_shows_text, back_button_visible, window_controls,
};
use crate::tab_drag::{self, DragKey, Dragger, Drop, Hover};
use crate::terminal_workspace::{StatusLevel, StatusNotice, Tab, TabStatus, TerminalWorkspace};
use crate::typography::UiFontStyled as _;

/// 标题栏高度（和文件窗口一样）。
pub const STRIP_HEIGHT: f32 = 40.0;
/// 新窗口的大小。
const WINDOW_SIZE: (f32, f32) = (1000.0, 680.0);
/// 工作区提示在这个窗口里停留多久（和主窗口通知的自动收起时长一样）。
const NOTICE_DURATION: Duration = Duration::from_secs(5);

/// 窗口编号（输入法按窗口 key 管理）。
static NEXT_WINDOW: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct Registry {
    windows: Vec<WindowHandle<TermWindow>>,
}

impl Global for Registry {}

fn registry(cx: &mut App) -> &mut Registry {
    if !cx.has_global::<Registry>() {
        cx.set_global(Registry::default());
    }
    cx.global_mut::<Registry>()
}

/// 现在开着的终端窗口（已经关掉的不算）。
pub fn handles(cx: &mut App) -> Vec<WindowHandle<TermWindow>> {
    let alive: Vec<_> = cx
        .windows()
        .iter()
        .map(|window| window.window_id())
        .collect();
    let registry = registry(cx);
    registry
        .windows
        .retain(|handle| alive.contains(&handle.window_id()));
    registry.windows.clone()
}

/// 独立终端窗口自己的文案（窗口标题）。
#[derive(Clone, Copy, Debug)]
enum TermWindowMsg {
    TitleSuffix,
    MenuBackToMain,
    MenuClose,
}

impl Msg for TermWindowMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::TitleSuffix => " 终端",
            Self::MenuBackToMain => "移回主窗口",
            Self::MenuClose => "关闭",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::TitleSuffix => " Terminal",
            Self::MenuBackToMain => "Move back to main window",
            Self::MenuClose => "Close",
        }
    }
}

/// 窗口标题。
pub fn window_title() -> String {
    format!("{}{}", crate::product::NAME, t(TermWindowMsg::TitleSuffix))
}

/// 用一个已经存在的标签开一个终端窗口；`at` 是松手时指针在屏幕上的位置（窗口放在它附近，标签标题
/// 大致落在指针下面）。窗口没建起来返回 `Err(tab)`，调用方把标签放回原处。
pub fn open(
    cx: &mut App,
    tab: Tab,
    at: Option<(f32, f32)>,
) -> Result<WindowHandle<TermWindow>, Tab> {
    let slot = std::rc::Rc::new(std::cell::RefCell::new(Some(tab)));
    let taken = std::rc::Rc::clone(&slot);
    let n = NEXT_WINDOW.fetch_add(1, Ordering::Relaxed);
    let key = format!("term-window-{n}");
    let bounds = match at {
        Some((x, y)) => WindowBounds::Windowed(::gpui::Bounds {
            origin: point(px((x - 60.0).max(0.0)), px((y - 20.0).max(0.0))),
            size: ::gpui::size(px(WINDOW_SIZE.0), px(WINDOW_SIZE.1)),
        }),
        None => WindowBounds::centered(::gpui::size(px(WINDOW_SIZE.0), px(WINDOW_SIZE.1)), cx),
    };
    let options = WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some(window_title().into()),
            ..TitleBar::title_bar_options()
        }),
        window_bounds: Some(bounds),
        window_min_size: Some(::gpui::size(px(480.0), px(320.0))),
        ..crate::window_frame::client_options()
    };
    let opened = cx.open_window(options, move |window, cx| {
        window.set_window_title(&window_title());
        let tab = taken.borrow_mut().take();
        let view = cx.new(|cx| TermWindow::new(window, cx, key.clone(), tab));
        let close_view = view.clone();
        // 关闭按钮 / 系统关闭请求：窗口里还有正在使用的连接时先问（V2-U44），确认后才真的关。
        crate::window_frame::on_should_close(window, cx, move |window, cx| {
            close_view.update(cx, |this, cx| this.on_window_close(window, cx))
        });
        let focus = view.read(cx).focus.clone();
        window.focus(&focus, cx);
        window.activate_window();
        view
    });
    match opened {
        Ok(handle) => {
            registry(cx).windows.push(handle);
            Ok(handle)
        }
        Err(_) => slot.borrow_mut().take().map_or_else(
            || unreachable!("tab is still in the handoff slot when the window failed to open"),
            Err,
        ),
    }
}

pub struct TermWindow {
    workspace: Entity<TerminalWorkspace>,
    focus: FocusHandle,
    ime_key: String,
    dragger: Dragger,
    /// 锁屏页（V2-U26）：锁着时整个窗口只画它。
    lock_screen: Option<Entity<crate::app_lock::LockScreen>>,
    /// SSH 提问（指纹确认、密码输入）的应用内对话框层（V2-U29）。
    ssh_prompt: Entity<crate::ssh_prompt_ui::SshPromptLayer>,
    /// 工作区要用户看见的提示（粘贴已取消、拖来的路径不能插……）。这个窗口没有组件库的通知层，
    /// 主窗口那套 `push_notification` 用不了，所以自己取走、自己画一条，到时收起。
    notice: Option<StatusNotice>,
    /// 每来一条提示加一：旧提示的收起计时到点时，发现已经有新的就不动它。
    notice_seq: u64,
    /// 关闭前的确认（V2-U44）：这个窗口没有组件库的 `Root`，对话框层自己画；开着时挡住下面的输入。
    close_ask: Option<CloseAsk>,
    /// 确认框开着时键盘焦点放在它的卡片上（终端没有焦点，敲的字不会进还连着的会话）。
    confirm_focus: FocusHandle,
    /// 用户在确认框里点了「关闭」且是关窗口：下一次关闭请求不再问，直接放行。
    close_confirmed: bool,
    _observe: Subscription,
}

/// 确认框的状态：打开时选中的范围、文字、「不再询问」勾选、确认后要不要连窗口一起关。
struct CloseAsk {
    request: CloseRequest,
    text: CloseText,
    dont_ask: bool,
    close_window: bool,
}

impl TermWindow {
    fn new(window: &mut Window, cx: &mut Context<Self>, ime_key: String, tab: Option<Tab>) -> Self {
        let workspace = cx.new(|cx| TerminalWorkspace::new(window, cx, ime_key.clone()));
        if let Some(tab) = tab {
            workspace.update(cx, |ws, _cx| ws.replace_with_tab(tab));
        }
        let focus = workspace.read(cx).focus_handle();
        let _observe = cx.observe_in(&workspace, window, |this, ws, window, cx| {
            if let Some(notice) = ws.update(cx, |ws, _cx| ws.take_status_notice()) {
                this.show_notice(notice, cx);
            }
            // 右键「关闭窗格」要先问用户：本窗口自己画确认层。
            if let Some(request) = ws.update(cx, |ws, _cx| ws.take_close_request()) {
                this.request_close(request, false, window, cx);
            }
            cx.notify();
        });
        let ssh_prompt = cx.new(|cx| crate::ssh_prompt_ui::SshPromptLayer::new(window, cx, false));
        Self {
            workspace,
            focus,
            ime_key,
            dragger: Dragger::new(STRIP_HEIGHT),
            lock_screen: None,
            ssh_prompt,
            notice: None,
            notice_seq: 0,
            close_ask: None,
            confirm_focus: cx.focus_handle(),
            close_confirmed: false,
            _observe,
        }
    }

    /// 显示一条工作区提示，`NOTICE_DURATION` 后收起；后来的提示顶掉先来的，并重新计时。
    fn show_notice(&mut self, notice: StatusNotice, cx: &mut Context<Self>) {
        self.notice_seq += 1;
        let seq = self.notice_seq;
        self.notice = Some(notice);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTICE_DURATION).await;
            let _ = this.update(cx, |this, cx| {
                if this.notice_seq == seq {
                    this.notice = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// 接管另一个窗口合并过来的标签：同一个 `Tab`，会话不断；选中它。
    pub fn adopt(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |ws, cx| ws.adopt_tab(tab, cx));
        let last = self.workspace.read(cx).tab_count().saturating_sub(1);
        self.workspace.update(cx, |ws, cx| ws.select_tab(last, cx));
        window.activate_window();
        cx.notify();
    }

    /// 关窗口：结束里面所有标签的会话。
    fn close_all(&mut self, cx: &mut Context<Self>) {
        self.workspace.update(cx, |ws, _cx| ws.close_all());
        crate::ime::window_destroyed(&self.ime_key);
    }

    #[cfg(debug_assertions)]
    pub fn tab_count(&self, cx: &App) -> usize {
        self.workspace.read(cx).tab_count()
    }

    /// 这个窗口的工作区（驱动脚本改设置、读设置用）。
    #[cfg(debug_assertions)]
    pub fn dev_workspace(&self) -> Entity<TerminalWorkspace> {
        self.workspace.clone()
    }

    /// 关闭确认层现在开着没有（驱动脚本用）。
    #[cfg(debug_assertions)]
    pub fn dev_close_ask_open(&self) -> bool {
        self.close_ask.is_some()
    }

    /// 往当前标签打字（驱动脚本用，和主窗口的 `type:` 步骤同一个方法）。
    #[cfg(debug_assertions)]
    pub fn dev_type(&mut self, text: &str, cx: &mut Context<Self>) {
        self.workspace.update(cx, |ws, cx| ws.type_text(text, cx));
    }

    /// 当前标签屏幕上有没有这段文字（驱动脚本用）。
    #[cfg(debug_assertions)]
    pub fn dev_has_text(&self, text: &str, cx: &App) -> bool {
        self.workspace
            .read(cx)
            .visible_text()
            .iter()
            .any(|line| line.contains(text))
    }

    /// 每个标签的连接状态（驱动脚本用）。
    #[cfg(debug_assertions)]
    pub fn dev_statuses(&self, cx: &App) -> Vec<TabStatus> {
        self.workspace
            .read(cx)
            .tab_infos()
            .into_iter()
            .map(|info| info.status)
            .collect()
    }

    fn select(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |ws, cx| ws.select_tab(index, cx));
        let focus = self.workspace.read(cx).focus_handle();
        window.focus(&focus, cx);
    }

    /// 标签上的 ✕、右键「关闭」：关第 `index` 个标签；有正在使用的连接先问（V2-U44）。
    fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(request) = self.workspace.read(cx).close_request_for_tab(index) {
            self.request_close(request, false, window, cx);
        }
    }

    /// 用户手动关闭：要问（设置开着、范围里有正在使用的连接）就弹本窗口的确认层并先不关，否则直接关。
    /// `close_window`：确认之后连窗口一起关（窗口关闭按钮）。已经有一个确认层开着时不叠。
    fn request_close(
        &mut self,
        request: CloseRequest,
        close_window: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.close_ask.is_some() {
            return;
        }
        let Some(summary) = self.workspace.read(cx).close_confirmation(&request) else {
            self.workspace
                .update(cx, |ws, cx| ws.perform_close(&request, cx));
            self.close_if_empty(window, cx);
            return;
        };
        self.close_ask = Some(CloseAsk {
            request,
            text: summary.text(),
            dont_ask: false,
            close_window,
        });
        // 键盘离开终端：确认层开着时敲的字不能落进下面还连着的会话。
        window.focus(&self.confirm_focus, cx);
        cx.notify();
    }

    /// 窗口关闭按钮 / 系统关闭请求（`on_should_close`）：返回 `true` 才真的关。
    /// 里面还有正在使用的连接时先弹确认层、这次不关（`false`）；用户确认后会再发起一次关闭，那一次放行。
    /// 应用锁着时窗口只画锁屏页、没有地方放确认层，这时不拦（和没有这个确认之前一样直接关）。
    fn on_window_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.close_confirmed || crate::app_lock::is_locked() {
            self.close_all(cx);
            return true;
        }
        if self.close_ask.is_some() {
            return false;
        }
        match self.workspace.read(cx).close_request_for_window() {
            Some(request)
                if self
                    .workspace
                    .read(cx)
                    .close_confirmation(&request)
                    .is_some() =>
            {
                self.request_close(request, true, window, cx);
                false
            }
            _ => {
                self.close_all(cx);
                true
            }
        }
    }

    /// 确认层点了「关闭」（或回车）：按打开时记下的标签 / 窗格编号关；已经不在的跳过。勾了「不再询问」就关掉设置。
    fn confirm_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ask) = self.close_ask.take() else {
            return;
        };
        window.focus(&self.workspace.read(cx).focus_handle(), cx);
        if ask.dont_ask {
            self.workspace
                .update(cx, |ws, cx| ws.disable_confirm_close_tab(cx));
        }
        if ask.close_window {
            // 放到下一轮再关：关闭请求会回到 `on_window_close`，不能在自己的更新里重入。
            self.close_confirmed = true;
            let handle = window.window_handle();
            cx.defer(move |cx| {
                let _ = handle.update(cx, |_, window, cx| {
                    crate::window_frame::request_close(window, cx)
                });
            });
        } else {
            self.workspace
                .update(cx, |ws, cx| ws.perform_close(&ask.request, cx));
            self.close_if_empty(window, cx);
        }
        cx.notify();
    }

    /// 确认层点了「取消」（或 Esc、点遮罩）：什么都不关。
    fn cancel_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.close_ask.take().is_some() {
            window.focus(&self.workspace.read(cx).focus_handle(), cx);
            cx.notify();
        }
    }

    fn set_dont_ask(&mut self, ticked: bool, cx: &mut Context<Self>) {
        if let Some(ask) = &mut self.close_ask {
            ask.dont_ask = ticked;
            cx.notify();
        }
    }

    /// 确认层开着时的按键：Esc 取消、Enter 关闭，其余一律吞掉（不进终端）。
    /// SSH 提问对话框开着时让路（它自己收键，焦点也在它那里）。
    fn on_close_ask_key(
        &mut self,
        event: &::gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.close_ask.is_none() || self.ssh_prompt.read(cx).is_open() {
            return;
        }
        let keystroke = &event.keystroke;
        if !keystroke.modifiers.modified() {
            match keystroke.key.as_str() {
                "escape" => self.cancel_close(window, cx),
                "enter" => self.confirm_close(window, cx),
                _ => {}
            }
        }
        cx.stop_propagation();
    }

    fn close_if_empty(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspace.read(cx).tab_count() == 0 {
            self.dragger.cancel(cx);
            // 窗口里没有标签了：关窗口。放到下一轮再关，免得在自己的事件处理里销毁自己。
            let handle = window.window_handle();
            cx.defer(move |cx| {
                let _ = handle.update(cx, |_, window, _| window.remove_window());
            });
        }
    }

    /// 标题栏上的「移回主窗口」按钮：对当前选中的标签，和标签右键「移回主窗口」是同一个 `move_to_main`。
    fn move_active_to_main(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let infos = self.workspace.read(cx).tab_infos();
        let active = self.workspace.read(cx).active_index();
        if let Some(info) = active.and_then(|index| infos.get(index)) {
            self.move_to_main(info.id, window, cx);
        }
    }

    /// 标签右键「移回主窗口」。
    fn move_to_main(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some((main, _)) = crate::file_window::main_window(cx) else {
            return;
        };
        let Some((index, tab)) = self
            .workspace
            .update(cx, |ws, cx| ws.take_tab_by_id(tab_id, cx))
        else {
            return;
        };
        match merge_into(cx, main.window_id(), tab) {
            Ok(()) => self.close_if_empty(window, cx),
            Err(tab) => self
                .workspace
                .update(cx, |ws, cx| ws.restore_tab(index, tab, cx)),
        }
    }

    // ---- 拖动（和主窗口同一套状态机，见 `tab_drag`） ----

    fn on_drag_move(
        &mut self,
        event: &::gpui::MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let local = (f32::from(event.position.x), f32::from(event.position.y));
        let left = event.pressed_button == Some(MouseButton::Left);
        let moved = self.dragger.moved(local, left, window, cx);
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

    /// 标签现在画出来的左右边界（驱动脚本算拖动坐标用）；还没画全返回 `None`。
    #[cfg(debug_assertions)]
    pub fn dev_tab_spans(&self, cx: &App) -> Option<Vec<tab_drag::Span>> {
        self.dragger.spans().collect(&self.tab_keys(cx))
    }

    /// 标签条上现在的顺序（这个窗口里只有终端标签，一整段）。
    fn tab_keys(&self, cx: &App) -> Vec<DragKey> {
        self.workspace
            .read(cx)
            .tab_ids()
            .into_iter()
            .map(DragKey::Terminal)
            .collect()
    }

    /// 拖动中指针在标签栏里：被拖的标签越过相邻标签的中线就换位。
    fn reorder_tab_under_pointer(
        &mut self,
        local: (f32, f32),
        size: (f32, f32),
        cx: &mut Context<Self>,
    ) {
        let keys = self.tab_keys(cx);
        if let Some((from, to)) = self.dragger.reorder_move(local, size, &keys) {
            self.workspace
                .update(cx, |ws, cx| ws.move_tab(from, to, cx));
            cx.notify();
        }
    }

    /// Esc：还原成拖动开始前的顺序。
    fn restore_tab_order(&mut self, original: &[DragKey], cx: &mut Context<Self>) {
        let current = self.tab_keys(cx);
        for (from, to) in tab_drag::restore_moves(&current, original) {
            self.workspace
                .update(cx, |ws, cx| ws.move_tab(from, to, cx));
        }
        cx.notify();
    }

    fn on_drag_up(
        &mut self,
        event: &::gpui::MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let local = (f32::from(event.position.x), f32::from(event.position.y));
        if let Some((key, drop)) = self.dragger.release(cx) {
            self.finish_drag(key, drop, local, window, cx);
        }
        cx.notify();
    }

    fn on_drag_cancel(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(original) = self.dragger.cancel(cx) {
            self.restore_tab_order(&original, cx);
        }
        cx.notify();
    }

    /// 松手之后：弹出（窗口外空地）、合并（别的窗口标签栏上）、什么也不做。
    pub fn finish_drag(
        &mut self,
        key: DragKey,
        drop: Drop,
        local: (f32, f32),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let DragKey::Terminal(tab_id) = key else {
            return;
        };
        // 按下时 AI 还没在工作、拖动中才开始的：松手这时再挡一次。合并回主窗口照常（对话面板在那边）。
        // 这个窗口没有通知层，标签留在原处就是反馈。
        if self.workspace.read(cx).tab_ai_active(tab_id)
            && tab_drag::blocked_for_ai(drop, tab_drag::main_window_id(cx))
        {
            return;
        }
        match drop {
            Drop::Nothing => {}
            Drop::TearOff => {
                // 窗口里只剩这一个标签：它已经单独在一个窗口里了，再弹出没有意义。
                if self.workspace.read(cx).tab_count() <= 1 {
                    return;
                }
                let screen = tab_drag::screen_point(window, local);
                let Some((index, tab)) = self
                    .workspace
                    .update(cx, |ws, cx| ws.take_tab_by_id(tab_id, cx))
                else {
                    return;
                };
                if let Err(tab) = open(cx, tab, Some(screen)) {
                    self.workspace
                        .update(cx, |ws, cx| ws.restore_tab(index, tab, cx));
                }
            }
            Drop::Merge(target) => {
                let Some((index, tab)) = self
                    .workspace
                    .update(cx, |ws, cx| ws.take_tab_by_id(tab_id, cx))
                else {
                    return;
                };
                match merge_into(cx, target, tab) {
                    Ok(()) => self.close_if_empty(window, cx),
                    Err(tab) => self
                        .workspace
                        .update(cx, |ws, cx| ws.restore_tab(index, tab, cx)),
                }
            }
        }
    }

    fn render_tab(
        &self,
        index: usize,
        info: &crate::terminal_workspace::TabInfo,
        active: bool,
        dragging: bool,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> ::gpui::AnyElement {
        let entity = cx.entity();
        let id = info.id;
        let hover = tokens.hover;
        let text = tokens.text;
        let muted = tokens.text_muted;
        let dot: Hsla = match info.status {
            TabStatus::Connecting => tokens.dot_connecting,
            TabStatus::Connected => tokens.dot_connected,
            TabStatus::Disconnected => tokens.dot_disconnected,
            TabStatus::Empty => tokens.dot_idle,
        };
        let title: SharedString = info.title.clone().into();
        let select = entity.clone();
        let close = entity.clone();
        let press = entity.clone();
        let menu_back = entity.clone();
        let menu_close = entity.clone();
        let press_title = info.title.clone();
        div()
            .id(("term-window-tab", index))
            .occlude()
            .relative()
            .flex()
            .items_center()
            .gap(px(tab_strip::TAB_GAP))
            .h(px(tab_strip::TAB_HEIGHT))
            .min_w(px(tab_strip::TAB_MIN_WIDTH))
            .max_w(px(tab_strip::TAB_MAX_WIDTH))
            .pl(px(tab_strip::TAB_PAD_LEFT))
            .pr(px(tab_strip::TAB_PAD_RIGHT))
            .flex_none()
            .mb(px(tab_strip::tab_margin_bottom(STRIP_HEIGHT - 1.0)))
            .rounded_full()
            .border_1()
            .text_size(crate::typography::ui_text_px(tab_strip::TAB_TEXT_SIZE))
            .cursor_pointer()
            .when(dragging, |d| d.opacity(0.4))
            .when(active, |d| {
                d.border_color(tokens.accent.opacity(0.45))
                    .bg(tokens.accent_soft)
                    .text_color(text)
            })
            .when(!active, |d| {
                d.border_color(Hsla::transparent_black())
                    .text_color(muted)
                    .hover(move |style| style.bg(hover))
            })
            .tooltip({
                let title = title.clone();
                move |window, cx| Tooltip::new(title.clone()).build(window, cx)
            })
            // 记下这个标签画出来的真实位置，拖动换位用（见 `tab_drag::TabSpans`）。
            .child(self.dragger.spans().track(DragKey::Terminal(id)))
            .child(div().size(px(5.0)).flex_none().rounded_full().bg(dot))
            .child(div().flex_1().min_w_0().truncate().child(title))
            .child(
                div()
                    .id(("term-window-tab-close", index))
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(tab_strip::TAB_CLOSE_SIZE))
                    .flex_none()
                    .rounded_full()
                    .text_color(muted)
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover).text_color(text))
                    .child(Icon::new(IconName::Close).size_3())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        close.update(cx, |this, cx| this.close_tab(index, window, cx));
                    }),
            )
            .on_mouse_down(MouseButton::Left, move |event, _window, cx| {
                let pos = (f32::from(event.position.x), f32::from(event.position.y));
                let title = press_title.clone();
                press.update(cx, |this, cx| {
                    this.dragger.press(DragKey::Terminal(id), title, pos);
                    if this.workspace.read(cx).tab_ai_active(id) {
                        this.dragger.pin();
                    }
                    cx.notify();
                });
            })
            .on_click(move |_, window, cx| {
                // 拖完松手那一下不算点击，别把落在鼠标下的标签选中。
                if select.read(cx).dragger.just_dragged() {
                    return;
                }
                select.update(cx, |this, cx| this.select(index, window, cx));
            })
            .context_menu(move |menu, _window, cx| {
                let tokens = Tokens::current(cx);
                let back = menu_back.clone();
                let close = menu_close.clone();
                menu.item(
                    crate::menu_kit::row(
                        &tokens,
                        crate::i18n::t(TermWindowMsg::MenuBackToMain),
                        Ico::SquareArrowDownLeft,
                    )
                    .on_click(move |_, window, cx| {
                        back.update(cx, |this, cx| this.move_to_main(id, window, cx));
                    }),
                )
                .item(
                    crate::menu_kit::row(&tokens, crate::i18n::t(TermWindowMsg::MenuClose), Ico::X)
                        .on_click(move |_, window, cx| {
                            close.update(cx, |this, cx| this.close_tab(index, window, cx));
                        }),
                )
            })
            .into_any_element()
    }
}

impl Render for TermWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 没有组件库 `Root` 的窗口自己对齐 rem 基准（界面字号）；`Root` 管的窗口同一个值。
        crate::typography::sync_window_rem(window);
        // 锁屏（V2-U26）：锁着时整个窗口只画锁屏页，里面的内容这一帧不进元素树。
        let content = match crate::app_lock::cover(&mut self.lock_screen, true, window, cx) {
            Some(cover) => {
                // 提示里可能有主机名、文件路径：锁着时不留着，解锁后也不再冒出来（同主窗口锁定时清通知）。
                self.notice = None;
                cover
            }
            None => self.render_unlocked(window, cx).into_any_element(),
        };
        // Linux 客户端装饰：细边框、阴影和边缘缩放（其他平台原样）。
        crate::window_frame::frame(content)
    }
}

impl TermWindow {
    fn render_unlocked(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = Tokens::current(cx);
        let infos = self.workspace.read(cx).tab_infos();
        let active = self.workspace.read(cx).active_index();
        let dragging_key = self.dragger.key();
        let me = crate::tab_drag::id_of(window);
        // 别的窗口正在拖标签、指针落在这个窗口的标签栏上：标签栏亮起来，告诉用户松手会合并进来。
        let drop_target = tab_drag::current(cx)
            .is_some_and(|drag| drag.hover == Hover::Over(me) && drag.source != me);
        let tabs: Vec<::gpui::AnyElement> = infos
            .iter()
            .enumerate()
            .map(|(index, info)| {
                self.render_tab(
                    index,
                    info,
                    active == Some(index),
                    dragging_key == Some(DragKey::Terminal(info.id)),
                    &tokens,
                    cx,
                )
            })
            .collect();
        let me_entity = cx.entity();
        let sensor = tab_drag::sensor(
            &me_entity,
            self.dragger.pending(),
            |this, event, window, cx| this.on_drag_move(event, window, cx),
            |this, event, window, cx| this.on_drag_up(event, window, cx),
        );
        let hint = self.drag_hint(window, cx, &tokens);
        let ghost = tab_drag::ghost_chip(window, cx, &tokens);
        let back = back_button_visible(infos.len()).then(|| {
            let me = cx.entity();
            back_button(
                &tokens,
                back_button_shows_text(f32::from(window.viewport_size().width)),
                move |window, cx| me.update(cx, |this, cx| this.move_active_to_main(window, cx)),
            )
        });
        v_flex()
            .id("term-window")
            .relative()
            .size_full()
            .track_focus(&self.focus)
            .capture_key_down(
                cx.listener(|this, event: &::gpui::KeyDownEvent, _window, cx| {
                    // 关闭确认层开着：按键都归它，不进终端（V2-U44）。
                    this.on_close_ask_key(event, _window, cx);
                    // 正在拖标签：Esc 取消。
                    if this.dragger.pending() && event.keystroke.key == "escape" {
                        this.on_drag_cancel(_window, cx);
                        cx.stop_propagation();
                    }
                }),
            )
            .bg(tokens.background)
            .text_color(tokens.text)
            .ui_font_fallbacks(cx)
            .child(sensor)
            .child(
                h_flex()
                    .id("term-window-titlebar")
                    .relative()
                    .flex_none()
                    .h(px(STRIP_HEIGHT))
                    .w_full()
                    .items_end()
                    .bg(if drop_target {
                        tokens.accent_soft
                    } else {
                        tokens.background
                    })
                    .border_b_1()
                    .border_color(if drop_target {
                        tokens.accent
                    } else {
                        tokens.divider
                    })
                    .child(
                        h_flex()
                            .id("term-window-drag")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .items_end()
                            .gap_0p5()
                            .pl_3()
                            // macOS 的红黄绿在左上角：和主窗口一样让出它们的宽度。
                            .when(cfg!(target_os = "macos"), |row| {
                                row.pl(px(crate::main_window::titlebar::MAC_TRAFFIC_LIGHT_INSET))
                            })
                            .overflow_hidden()
                            .map(crate::window_frame::drag_region)
                            .child(
                                h_flex()
                                    // 和标签垂直对齐：标签中心离底边 6 + 13 = 19，一行 12px 字约 18 高，下边距取 10（标签还是 32 高时是 8）。
                                    .mb(px(10.0))
                                    .mr_2()
                                    .flex_none()
                                    .gap_1p5()
                                    .items_center()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(tokens.text_muted)
                                    .child(
                                        Icon::new(Ico::Server).size_3p5().text_color(tokens.accent),
                                    )
                                    .child(crate::product::NAME),
                            )
                            .children(tabs)
                            .children(hint),
                    )
                    // 最小化按钮左边：把当前标签移回主窗口（没有标签时不画）。
                    .children(back)
                    .child(
                        div()
                            .h_full()
                            .flex_none()
                            .child(window_controls(&tokens, window)),
                    ),
            )
            .child(div().flex_1().min_h_0().child(self.workspace.clone()))
            .children(
                self.notice
                    .as_ref()
                    .map(|notice| notice_chip(notice, &tokens)),
            )
            .children(ghost)
            .children(self.render_close_ask(&tokens, cx))
            .child(self.ssh_prompt.clone())
    }

    /// 关闭确认层：整个窗口盖一层半透明遮罩 + 居中卡片，样子和主窗口的确认框一致（同一份内容 `close_confirm::dialog_content`）。
    /// 这个窗口没有组件库的 `Root`，所以不用库的对话框接口，自己画。
    fn render_close_ask(
        &mut self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> Option<::gpui::AnyElement> {
        let ask = self.close_ask.as_ref()?;
        let (toggle, cancel, confirm, scrim) = (cx.entity(), cx.entity(), cx.entity(), cx.entity());
        let content = crate::close_confirm::dialog_content(
            &ask.text,
            ask.dont_ask,
            tokens,
            move |ticked, _, cx| {
                toggle.update(cx, |this, cx| this.set_dont_ask(ticked, cx));
            },
            move |_, window, cx| {
                cancel.update(cx, |this, cx| this.cancel_close(window, cx));
            },
            move |_, window, cx| {
                confirm.update(cx, |this, cx| this.confirm_close(window, cx));
            },
        );
        let card = crate::main_window::widgets::modal_card("term-close-card", 440.0, false, tokens)
            .track_focus(&self.confirm_focus)
            .child(content);
        Some(
            crate::main_window::widgets::modal_scrim(
                "term-close-scrim",
                false,
                move |window, cx| {
                    scrim.update(cx, |this, cx| this.cancel_close(window, cx));
                },
            )
            .child(card)
            .into_any_element(),
        )
    }
}

impl TermWindow {
    /// 拖动中的提示：出了窗口 / 落在别的窗口标签栏上时写一句松手会怎样。
    fn drag_hint(&self, window: &Window, cx: &App, tokens: &Tokens) -> Option<::gpui::AnyElement> {
        tab_drag::hint_chip(window, cx, tokens)
    }
}

/// 终端区右上角的一条提示：只是画出来，不接鼠标（下面的终端照常能点、能选）。错误用危险色描边和文字。
fn notice_chip(notice: &StatusNotice, tokens: &Tokens) -> ::gpui::AnyElement {
    let error = notice.level == StatusLevel::Error;
    div()
        .absolute()
        .top(px(STRIP_HEIGHT + 8.0))
        .right_3()
        .max_w(px(360.0))
        .px_3()
        .py_2()
        // 文字可能折成两三行：用内容块的圆角，不用单行控件的。
        .rounded(px(crate::app_theme::CARD_RADIUS))
        .border_1()
        .border_color(if error { tokens.danger } else { tokens.border })
        .bg(tokens.popover)
        .shadow_md()
        .text_sm()
        .text_color(if error { tokens.danger } else { tokens.text })
        .child(SharedString::from(notice.text.clone()))
        .into_any_element()
}

/// 把一个终端标签交给目标窗口（主窗口或另一个终端窗口）：同一个 `Tab`，会话不断、不重连。
/// 找不到目标窗口 / 交接失败时把标签还给调用方。
pub fn merge_into(cx: &mut App, target: ::gpui::WindowId, tab: Tab) -> Result<(), Tab> {
    let slot = std::rc::Rc::new(std::cell::RefCell::new(Some(tab)));
    if let Some((handle, view)) = crate::file_window::main_window(cx)
        && handle.window_id() == target
    {
        let give = std::rc::Rc::clone(&slot);
        let done = handle.update(cx, move |_, window, cx| {
            if let (Some(view), Some(tab)) = (view.upgrade(), give.borrow_mut().take()) {
                view.update(cx, |main, cx| main.adopt_terminal_tab(tab, window, cx));
            }
        });
        let _ = done;
        return slot.borrow_mut().take().map_or(Ok(()), Err);
    }
    for handle in handles(cx) {
        if handle.window_id() == target {
            let give = std::rc::Rc::clone(&slot);
            let done = handle.update(cx, move |this, window, cx| {
                if let Some(tab) = give.borrow_mut().take() {
                    this.adopt(tab, window, cx);
                }
            });
            let _ = done;
            return slot.borrow_mut().take().map_or(Ok(()), Err);
        }
    }
    slot.borrow_mut().take().map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ::gpui::{
        AppContext as _, Modifiers, MouseButton, TestAppContext, VisualTestContext, WindowHandle,
        point, px,
    };

    use super::{STRIP_HEIGHT, TermWindow};
    use crate::settings_store::AppSettings;
    use crate::tab_drag::DragKey;

    /// 开一个有三个标签的独立终端窗口，选中第 `active` 个；返回窗口和三个标签的编号。
    fn open(cx: &mut TestAppContext, active: usize) -> (WindowHandle<TermWindow>, [u64; 3]) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            AppSettings::init_for_test(cx);
        });
        let window =
            cx.add_window(|window, cx| TermWindow::new(window, cx, "term-test".into(), None));
        let ids = window
            .update(cx, |term, window, cx| {
                window.focus(&term.focus, cx);
                term.workspace.update(cx, |ws, cx| {
                    let ids = [
                        ws.push_test_tab("web-1"),
                        ws.push_test_tab("web-2"),
                        ws.push_test_tab("web-3"),
                    ];
                    ws.select_tab(active, cx);
                    ids
                })
            })
            .expect("term window");
        draw(cx, window);
        draw(cx, window);
        (window, ids)
    }

    fn draw(cx: &mut TestAppContext, window: WindowHandle<TermWindow>) {
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .expect("draw");
    }

    fn order(cx: &mut TestAppContext, window: WindowHandle<TermWindow>) -> Vec<u64> {
        window
            .read_with(cx, |term, cx| term.workspace.read(cx).tab_ids())
            .expect("order")
    }

    fn centres(cx: &mut TestAppContext, window: WindowHandle<TermWindow>) -> Vec<f32> {
        let spans = window
            .read_with(cx, |term, cx| {
                term.dragger.spans().collect(&term.tab_keys(cx))
            })
            .expect("spans")
            .expect("每个标签都画过一帧");
        spans.iter().map(|(l, r)| (l + r) / 2.0).collect()
    }

    fn at(x: f32) -> ::gpui::Point<::gpui::Pixels> {
        point(px(x), px(STRIP_HEIGHT / 2.0))
    }

    #[gpui::test]
    fn the_terminal_window_swaps_tabs_while_dragging_and_escape_restores_them(
        cx: &mut TestAppContext,
    ) {
        let (window, [a, b, c]) = open(cx, 2);
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        let at_start = centres(cx, window);

        vcx.simulate_mouse_down(at(at_start[0]), MouseButton::Left, Modifiers::none());
        draw(cx, window);
        vcx.simulate_mouse_move(at(at_start[0] + 10.0), MouseButton::Left, Modifiers::none());
        draw(cx, window);
        assert_eq!(order(cx, window), [a, b, c], "还没过邻居的中线");
        vcx.simulate_mouse_move(at(at_start[1] + 4.0), MouseButton::Left, Modifiers::none());
        draw(cx, window);
        assert_eq!(order(cx, window), [b, a, c]);
        let active = window
            .read_with(cx, |term, cx| term.workspace.read(cx).active_tab_id())
            .expect("active");
        assert_eq!(active, Some(c), "选中的标签不受影响");

        // Esc 恢复；之后再抬起鼠标也不会再动。
        vcx.simulate_keystrokes("escape");
        draw(cx, window);
        assert_eq!(order(cx, window), [a, b, c]);
        vcx.simulate_mouse_up(at(at_start[1] + 4.0), MouseButton::Left, Modifiers::none());
        draw(cx, window);
        assert_eq!(order(cx, window), [a, b, c]);

        // 再拖一次，这回松手：保留新顺序。
        vcx.simulate_mouse_down(at(at_start[0]), MouseButton::Left, Modifiers::none());
        draw(cx, window);
        vcx.simulate_mouse_move(at(at_start[0] + 10.0), MouseButton::Left, Modifiers::none());
        draw(cx, window);
        vcx.simulate_mouse_move(at(at_start[1] + 4.0), MouseButton::Left, Modifiers::none());
        draw(cx, window);
        vcx.simulate_mouse_up(at(at_start[1] + 4.0), MouseButton::Left, Modifiers::none());
        draw(cx, window);
        assert_eq!(order(cx, window), [b, a, c]);
        let keys = window
            .read_with(cx, |term, cx| term.tab_keys(cx))
            .expect("keys");
        assert_eq!(
            keys,
            [
                DragKey::Terminal(b),
                DragKey::Terminal(a),
                DragKey::Terminal(c)
            ]
        );
    }

    /// 在这个窗口里挂起一次多行粘贴确认，再切标签让它作废：工作区会要求外壳提示「粘贴已取消」。
    fn cancel_a_pending_paste(
        cx: &mut TestAppContext,
        window: WindowHandle<TermWindow>,
        to: usize,
    ) {
        window
            .update(cx, |term, window, cx| {
                term.workspace.update(cx, |ws, cx| {
                    ws.begin_paste("first\nsecond".into(), window, cx);
                    ws.select_tab(to, cx);
                });
            })
            .expect("cancel paste");
        cx.run_until_parked();
    }

    fn shown_notice(cx: &mut TestAppContext, window: WindowHandle<TermWindow>) -> Option<String> {
        window
            .read_with(cx, |term, _| term.notice.as_ref().map(|n| n.text.clone()))
            .expect("notice")
    }

    /// 这个窗口没有组件库的通知层：工作区的提示要由窗口自己取走、画出来，到时收起；
    /// 新提示来了重新计时，旧提示的计时不能把新提示提前收掉。
    #[gpui::test]
    fn the_terminal_window_shows_workspace_notices_and_hides_them_later(cx: &mut TestAppContext) {
        let (window, _) = open(cx, 0);
        let cancelled = crate::terminal_workspace::paste_cancelled();

        cancel_a_pending_paste(cx, window, 1);
        assert_eq!(shown_notice(cx, window).as_deref(), Some(cancelled));
        let left = window
            .update(cx, |term, _, cx| {
                term.workspace.update(cx, |ws, _| ws.take_status_notice())
            })
            .expect("workspace notice");
        assert_eq!(left, None, "窗口已经取走，不该还留在工作区里");
        draw(cx, window);

        cx.executor().advance_clock(super::NOTICE_DURATION / 2);
        cancel_a_pending_paste(cx, window, 2);
        cx.executor()
            .advance_clock(super::NOTICE_DURATION / 2 + Duration::from_millis(100));
        cx.run_until_parked();
        assert_eq!(
            shown_notice(cx, window).as_deref(),
            Some(cancelled),
            "第一条的计时到了，但第二条还没满时长"
        );

        cx.executor().advance_clock(super::NOTICE_DURATION / 2);
        cx.run_until_parked();
        assert_eq!(shown_notice(cx, window), None);
        draw(cx, window);
    }

    /// 独立终端窗口没有组件库的 `Root`，rem 基准要靠自己根视图渲染时对齐：标准 16px（和这个设置出现之前一样），
    /// 小 14px、大 18px；窗口开着时换档，下一帧立刻跟上，不用重开。
    #[gpui::test]
    fn the_terminal_window_rem_base_follows_the_interface_text_size(cx: &mut TestAppContext) {
        let (window, _) = open(cx, 0);
        let rem = |cx: &mut TestAppContext| {
            cx.update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                window.rem_size()
            })
            .expect("draw")
        };
        assert_eq!(rem(cx), px(16.0), "标准档和以前一样");
        let settings = cx.update(AppSettings::global);
        for level in [
            crate::typography::UiTextSize::Large,
            crate::typography::UiTextSize::Small,
            crate::typography::UiTextSize::Normal,
        ] {
            settings.update(cx, |model, mcx| {
                model.update_with(mcx, |s| s.ui_text_size = level);
            });
            assert_eq!(rem(cx), px(level.rem_px()), "{level:?}");
        }
        crate::typography::set_ui_text_size(crate::typography::UiTextSize::Normal);
    }

    #[test]
    fn the_titlebar_button_and_the_tab_menu_share_one_move_back_function() {
        // 标题栏按钮经 `move_active_to_main` 对当前标签调 `move_to_main`，右键菜单也直接调它。
        let code = include_str!("term_window.rs");
        let code = code.split("#[cfg(test)]").next().unwrap_or(code);
        assert!(code.contains("back.update(cx, |this, cx| this.move_to_main(id, window, cx))"));
        assert!(code.contains("this.move_active_to_main(window, cx)"));
        let body = code
            .split("fn move_active_to_main")
            .nth(1)
            .and_then(|rest| rest.split("fn move_to_main").next())
            .unwrap_or("");
        assert!(body.contains("self.move_to_main(info.id, window, cx)"));
    }

    // ---- V2-U44：关闭前确认（这个窗口没有组件库的 Root，确认层自己画；用 add_window 开的窗口同样没有 Root） ----

    /// 开一个独立终端窗口，里面是几个标签：`true` = 已连上的主机会话，`false` = 离线演示标签。
    fn open_with(
        cx: &mut TestAppContext,
        tabs: &[(&str, bool)],
    ) -> (WindowHandle<TermWindow>, Vec<u64>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            AppSettings::init_for_test(cx);
        });
        let window =
            cx.add_window(|window, cx| TermWindow::new(window, cx, "term-close-test".into(), None));
        let ids = window
            .update(cx, |term, window, cx| {
                window.focus(&term.focus, cx);
                // 和真窗口一样登记关闭回调（`open` 里做的那一步）。
                let view = cx.entity();
                crate::window_frame::on_should_close(window, cx, move |window, cx| {
                    view.update(cx, |this, cx| this.on_window_close(window, cx))
                });
                term.workspace.update(cx, |ws, cx| {
                    let ids = tabs
                        .iter()
                        .map(|(title, live)| {
                            if *live {
                                ws.push_test_host_tab(title, 7)
                            } else {
                                ws.push_test_tab(title)
                            }
                        })
                        .collect::<Vec<_>>();
                    ws.select_tab(0, cx);
                    ids
                })
            })
            .expect("term window");
        draw(cx, window);
        draw(cx, window);
        (window, ids)
    }

    fn asked(cx: &mut TestAppContext, window: WindowHandle<TermWindow>) -> Option<String> {
        window
            .read_with(cx, |term, _| {
                term.close_ask.as_ref().map(|ask| ask.text.title.clone())
            })
            .expect("ask")
    }

    #[gpui::test]
    fn closing_a_live_tab_asks_in_the_windows_own_layer_and_the_keys_stay_out_of_the_terminal(
        cx: &mut TestAppContext,
    ) {
        let (window, [a, b]) = {
            let (window, ids) = open_with(cx, &[("web-1", true), ("web-2", true)]);
            (window, [ids[0], ids[1]])
        };
        window
            .update(cx, |term, window, cx| term.close_tab(1, window, cx))
            .expect("close");
        draw(cx, window);
        let title = asked(cx, window).expect("有正在使用的连接要先问");
        assert!(title.contains("web-2"), "{title}");
        assert_eq!(order(cx, window), [a, b], "问的时候还没关");
        // 画出来没有 panic（这个窗口没有 Root，用了库的对话框层会炸），键盘离开了终端。
        let terminal_focused = window
            .update(cx, |term, window, cx| {
                term.workspace.read(cx).focus_handle().is_focused(window)
            })
            .expect("focus");
        assert!(!terminal_focused, "确认层开着时终端没有键盘焦点");

        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.simulate_keystrokes("escape");
        draw(cx, window);
        assert_eq!(asked(cx, window), None, "Esc 取消");
        assert_eq!(order(cx, window), [a, b], "取消后什么都没关");
        let terminal_focused = window
            .update(cx, |term, window, cx| {
                term.workspace.read(cx).focus_handle().is_focused(window)
            })
            .expect("focus");
        assert!(terminal_focused, "取消后键盘还给终端");

        window
            .update(cx, |term, window, cx| term.close_tab(1, window, cx))
            .expect("close");
        draw(cx, window);
        vcx.simulate_keystrokes("enter");
        draw(cx, window);
        assert_eq!(asked(cx, window), None);
        assert_eq!(order(cx, window), [a], "回车 = 关闭");
    }

    #[gpui::test]
    fn tabs_not_in_use_close_without_the_layer(cx: &mut TestAppContext) {
        let (window, ids) = open_with(cx, &[("demo", false), ("web-1", true)]);
        window
            .update(cx, |term, window, cx| term.close_tab(0, window, cx))
            .expect("close");
        draw(cx, window);
        assert_eq!(asked(cx, window), None, "离线演示标签直接关");
        assert_eq!(order(cx, window), [ids[1]]);
    }

    #[gpui::test]
    fn the_window_close_button_asks_first_and_cancel_closes_nothing(cx: &mut TestAppContext) {
        let (window, ids) = open_with(cx, &[("web-1", true), ("web-2", false)]);
        // 关闭按钮（和系统关闭请求同一个回调）：还有正在使用的连接，这次不关，先弹确认层。
        // 系统发起的关闭请求不在窗口自己视图的更新里（用不带视图类型的窗口句柄，才不会重复借用）。
        cx.update_window(window.into(), |_, window, cx| {
            crate::window_frame::request_close(window, cx);
        })
        .expect("close request");
        draw(cx, window);
        let title = asked(cx, window).expect("关窗口前要问");
        assert!(!title.is_empty());
        assert_eq!(order(cx, window), ids, "没确认前什么都没关");
        assert!(cx.update(|cx| cx.windows().len()) == 1, "窗口还开着");

        // 取消：窗口和标签都在。
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.simulate_keystrokes("escape");
        draw(cx, window);
        assert_eq!(asked(cx, window), None);
        assert_eq!(order(cx, window), ids);
        assert_eq!(cx.update(|cx| cx.windows().len()), 1, "取消后窗口还在");
        let not_confirmed = window
            .read_with(cx, |term, _| !term.close_confirmed)
            .expect("flag");
        assert!(not_confirmed);
    }

    #[gpui::test]
    fn confirming_the_window_close_lets_the_next_close_request_through(cx: &mut TestAppContext) {
        let (window, _) = open_with(cx, &[("web-1", true)]);
        cx.update_window(window.into(), |_, window, cx| {
            crate::window_frame::request_close(window, cx)
        })
        .expect("close");
        draw(cx, window);
        assert!(asked(cx, window).is_some());
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(
            cx.update(|cx| cx.windows().len()),
            0,
            "确认后窗口关了（确认之后发起的那次关闭请求不再问）"
        );
    }

    #[gpui::test]
    fn a_window_with_nothing_in_use_closes_straight_away(cx: &mut TestAppContext) {
        let (window, _) = open_with(cx, &[("demo", false)]);
        cx.update_window(window.into(), |_, window, cx| {
            crate::window_frame::request_close(window, cx)
        })
        .expect("close");
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| cx.windows().len()), 0, "不用问：直接关");
    }

    #[gpui::test]
    fn do_not_ask_again_in_this_window_turns_the_setting_off(cx: &mut TestAppContext) {
        let (window, ids) = open_with(cx, &[("web-1", true), ("web-2", true), ("web-3", true)]);
        window
            .update(cx, |term, window, cx| {
                term.close_tab(0, window, cx);
                term.set_dont_ask(true, cx);
                term.confirm_close(window, cx);
            })
            .expect("confirm");
        draw(cx, window);
        assert_eq!(order(cx, window), [ids[1], ids[2]]);
        let enabled = window
            .read_with(cx, |term, cx| {
                term.workspace.read(cx).settings().confirm_close_tab
            })
            .expect("settings");
        assert!(!enabled, "勾了不再询问");
        window
            .update(cx, |term, window, cx| term.close_tab(0, window, cx))
            .expect("close");
        assert_eq!(asked(cx, window), None, "以后直接关");
        assert_eq!(order(cx, window), [ids[2]]);
    }

    #[gpui::test]
    fn the_pane_menu_close_is_asked_in_this_window_too(cx: &mut TestAppContext) {
        let (window, _) = open_with(cx, &[("web-1", true)]);
        let pane = window
            .update(cx, |term, _, cx| {
                term.workspace.update(cx, |ws, cx| {
                    ws.split_active(crate::split_tree::Axis::X, cx);
                    ws.dev_focused_pane_id_for_test()
                })
            })
            .expect("split");
        window
            .update(cx, |term, _, cx| {
                term.workspace
                    .update(cx, |ws, cx| ws.request_close_pane_for_test(pane, cx))
            })
            .expect("menu");
        draw(cx, window);
        assert!(
            asked(cx, window).is_some(),
            "右键关闭窗格走这个窗口的确认层"
        );
        window
            .update(cx, |term, window, cx| term.confirm_close(window, cx))
            .expect("confirm");
        let panes = window
            .read_with(cx, |term, cx| {
                term.workspace.read(cx).dev_pane_count_for_test()
            })
            .expect("panes");
        assert_eq!(panes, 1);
    }
}
