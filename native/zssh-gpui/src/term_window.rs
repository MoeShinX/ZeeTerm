//! 独立终端窗口（V2-U1 第 7 条）：把主窗口顶部的终端标签拖出窗口边界松手，标签就弹出成这样一个窗口。
//!
//! 里面是同一个 [`TerminalWorkspace`]（终端、分屏、选区、粘贴、鼠标、高亮都是主窗口那一份代码），
//! 窗口自己只多一个自绘标题栏：窗口里的标签（可以切换、关闭、再拖出去 / 拖回主窗口）和最小化 / 最大化 /
//! 关闭。主窗口的 AI 面板、快捷命令栏、撰写栏、状态栏、主机树不在这里（这是单纯的终端窗口）。
//!
//! 交接的是同一个 `Tab`：窗格里的会话驱动、已认证的连接、回滚内容原样过去，不重连。
//! 关窗口 = 关掉里面的标签（结束它们的会话），和关标签一样。最后一个标签走了（关掉或拖回主窗口）窗口自己关。

use std::sync::atomic::{AtomicU64, Ordering};

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
use crate::i18n::{Msg, t};
use crate::icons::Ico;
use crate::main_window::titlebar::{
    back_button, back_button_shows_text, back_button_visible, window_controls,
};
use crate::tab_drag::{self, DragKey, Dragger, Drop, Hover};
use crate::terminal_workspace::{Tab, TabStatus, TerminalWorkspace};
use crate::typography::UiFontStyled as _;

/// 标题栏高度（和文件窗口一样）。
pub const STRIP_HEIGHT: f32 = 40.0;
const TAB_HEIGHT: f32 = 32.0;
/// 新窗口的大小。
const WINDOW_SIZE: (f32, f32) = (1000.0, 680.0);

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
        crate::window_frame::on_should_close(window, cx, move |_window, cx| {
            close_view.update(cx, |this, cx| this.close_all(cx));
            true
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
    _observe: Subscription,
}

impl TermWindow {
    fn new(window: &mut Window, cx: &mut Context<Self>, ime_key: String, tab: Option<Tab>) -> Self {
        let workspace = cx.new(|cx| TerminalWorkspace::new(window, cx, ime_key.clone()));
        if let Some(tab) = tab {
            workspace.update(cx, |ws, _cx| ws.replace_with_tab(tab));
        }
        let focus = workspace.read(cx).focus_handle();
        let _observe = cx.observe(&workspace, |_this, _ws, cx| cx.notify());
        let ssh_prompt = cx.new(|cx| crate::ssh_prompt_ui::SshPromptLayer::new(window, cx, false));
        Self {
            workspace,
            focus,
            ime_key,
            dragger: Dragger::default(),
            lock_screen: None,
            ssh_prompt,
            _observe,
        }
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

    fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |ws, cx| ws.close_tab(index, cx));
        self.close_if_empty(window, cx);
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
        if self.dragger.moved(local, left, window, cx) {
            cx.notify();
        }
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
        self.dragger.cancel(cx);
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
            .gap_2()
            .h(px(TAB_HEIGHT))
            .min_w(px(112.0))
            .max_w(px(200.0))
            .pl_3()
            .pr_1p5()
            .flex_none()
            .rounded_t_md()
            .border_x_1()
            .border_t_1()
            .text_sm()
            .cursor_pointer()
            .when(dragging, |d| d.opacity(0.4))
            .when(active, |d| {
                d.border_color(tokens.border)
                    .bg(tokens.background)
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
            .child(div().size(px(6.0)).flex_none().rounded_full().bg(dot))
            .child(div().flex_1().min_w_0().truncate().child(title))
            .child(
                div()
                    .id(("term-window-tab-close", index))
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(24.0))
                    .flex_none()
                    .rounded_sm()
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
                    cx.notify();
                });
            })
            .on_click(move |_, window, cx| {
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
        // 锁屏（V2-U26）：锁着时整个窗口只画锁屏页，里面的内容这一帧不进元素树。
        let content = match crate::app_lock::cover(&mut self.lock_screen, true, window, cx) {
            Some(cover) => cover,
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
                            .overflow_hidden()
                            .map(crate::window_frame::drag_region)
                            .child(
                                h_flex()
                                    .mb_2()
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
            .children(ghost)
            .child(self.ssh_prompt.clone())
    }
}

impl TermWindow {
    /// 拖动中的提示：出了窗口 / 落在别的窗口标签栏上时写一句松手会怎样。
    fn drag_hint(&self, window: &Window, cx: &App, tokens: &Tokens) -> Option<::gpui::AnyElement> {
        tab_drag::hint_chip(window, cx, tokens)
    }
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
}
