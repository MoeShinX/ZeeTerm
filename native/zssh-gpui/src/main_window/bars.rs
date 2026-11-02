//! 终端页底部的快捷命令栏和撰写栏（0.2.14 `QuickCommandBar.tsx`、`ComposeBar.tsx`、
//! `QuickBarResizeHandle.tsx`、`ComposeBarResizeHandle.tsx`）。
//!
//! 两条栏都画在终端下面、状态栏上面：终端自己量区域大小，栏出现 / 变高时它会重新排版。
//! 发送走同一条路径（[`MainWindow::send_text`]）：多行命令按设置「多行粘贴要确认」先问，
//! 一个连上的窗格都没有时提示，真正写进窗格用 [`crate::terminal_workspace::TerminalWorkspace::send_to_tab`]。

use ::gpui::{
    ClickEvent, Entity, MouseButton, MouseDownEvent, MouseMoveEvent, SharedString, Window, px,
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::MainWindow;
use super::bar_height::{self, COMPOSE_BAR_CHROME};
use super::bar_settings::{self, BarSettings};
use super::compose_history::{self, Nav};
use super::modal::{Confirm, ConfirmAction, Modal};
use super::nav::MainView;
use super::quick_commands::{self, QuickCommand};
use crate::app_theme::Tokens;
use crate::icons::Ico;

/// 拖动栏上沿时记下的起点。
#[derive(Clone, Copy, Debug)]
pub struct BarDrag {
    pub start_y: f32,
    pub start_height: f32,
}

pub struct BarUi {
    pub compose: Entity<TextareaState>,
    /// Enter 和发送按钮发到所有分屏（只在内存里，不保存；Ctrl+Enter 不看这个）。
    pub compose_all: bool,
    /// 发送历史（旧 → 新），所有标签共用，只在内存里。
    pub history: Vec<String>,
    pub nav: Nav,
    /// 拖动中的快捷命令栏 / 撰写栏。
    pub quick_drag: Option<BarDrag>,
    pub compose_drag: Option<BarDrag>,
    /// 拖动中显示的高度（松手才写进设置表）。
    pub quick_live: Option<f32>,
    pub compose_live: Option<f32>,
}

impl BarUi {
    pub fn new(window: &mut Window, cx: &mut Context<MainWindow>) -> Self {
        let compose = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(
                    "输入命令，Enter 发送到当前窗格，Ctrl+Enter 发到所有分屏，Shift+Enter 换行",
                )
                .submit_on_enter(true)
                .auto_grow(1, 5)
        });
        Self {
            compose,
            compose_all: false,
            history: Vec::new(),
            nav: Nav::default(),
            quick_drag: None,
            compose_drag: None,
            quick_live: None,
            compose_live: None,
        }
    }
}

/// 撰写栏输入框的事件：回车发送，内容变了就不再算「正在翻历史」。
pub fn on_compose_event(
    this: &mut MainWindow,
    event: &InputEvent,
    window: &mut Window,
    cx: &mut Context<MainWindow>,
) {
    match event {
        InputEvent::PressEnter { secondary, shift } if !*shift => {
            this.compose_submit(*secondary, window, cx);
        }
        InputEvent::Change => {
            this.bars.nav = Nav::default();
            cx.notify();
        }
        _ => {}
    }
}

fn tint(hex: u32, alpha: u8) -> ::gpui::Hsla {
    ::gpui::rgba((hex << 8) | u32::from(alpha)).into()
}

impl MainWindow {
    // ---- 设置 ----

    pub(super) fn bar_settings(&self) -> &BarSettings {
        self.store.bars()
    }

    pub(super) fn quick_bar_open(&self) -> bool {
        self.bar_settings().quick_open
    }

    pub(super) fn compose_bar_open(&self) -> bool {
        self.bar_settings().compose_open
    }

    /// 终端区域的大概高度（窗口高度扣掉标题栏和状态栏）：栏高的上限按它算。
    fn area_height(&self) -> f32 {
        (self.window_height - super::titlebar::BAR_HEIGHT - 26.0).max(0.0)
    }

    pub(super) fn quick_height_now(&self) -> f32 {
        let area = self.area_height();
        bar_height::clamp_quick(
            self.bars
                .quick_live
                .unwrap_or(self.bar_settings().quick_height),
            area,
        )
    }

    /// 撰写栏现在的实际高度：固定高度，或者按输入框里的行数自动算。
    pub(super) fn compose_height_now(&self, cx: &App) -> f32 {
        let area = self.area_height();
        let fixed = self
            .bars
            .compose_live
            .unwrap_or(self.bar_settings().compose_height);
        if fixed > 0.0 {
            return bar_height::clamp_compose(fixed, area);
        }
        let lines = self.bars.compose.read(cx).value().lines().count().max(1);
        bar_height::compose_auto_input_height(lines, area) + COMPOSE_BAR_CHROME
    }

    fn write_setting(
        &mut self,
        write: impl FnOnce(&zssh_core::store::Database) -> Result<(), String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(err) = self.store.write_bar_setting(write) {
            self.notice(&format!("保存设置失败：{err}"), window, cx);
        }
        cx.notify();
    }

    pub(crate) fn toggle_quick_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 欢迎页 / 主机页没有终端、看不到底部的命令栏：这个按钮改成打开「管理 + 设置」页的「快捷命令」。
        if matches!(self.nav.view, MainView::Vault | MainView::Welcome) {
            self.open_settings_section(
                crate::settings_nav::SettingsSection::QuickCommands,
                window,
                cx,
            );
            return;
        }
        let next = !self.quick_bar_open();
        self.write_setting(
            |db| bar_settings::write_bool(db, bar_settings::QUICK_BAR, next),
            window,
            cx,
        );
    }

    pub(crate) fn toggle_compose_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = !self.compose_bar_open();
        self.write_setting(
            |db| bar_settings::write_bool(db, bar_settings::COMPOSE_BAR, next),
            window,
            cx,
        );
        if next && self.nav.view == MainView::Terminal {
            let handle = self.bars.compose.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
    }

    pub(super) fn set_quick_group(
        &mut self,
        group: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let group = group.to_owned();
        self.write_setting(
            |db| bar_settings::write_text(db, bar_settings::QUICK_GROUP, &group),
            window,
            cx,
        );
    }

    pub(crate) fn save_quick_commands(
        &mut self,
        list: &[QuickCommand],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.write_setting(|db| bar_settings::write_commands(db, list), window, cx);
    }

    // ---- 发送 ----

    /// 点命令栏上的一条：`ctrl` 或这个标签开着输入广播就发到所有分屏。
    pub(crate) fn run_quick_command(
        &mut self,
        id: &str,
        all_requested: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(command) = self
            .bar_settings()
            .commands
            .iter()
            .find(|item| item.id == id)
            .cloned()
        else {
            return;
        };
        let all = all_requested || self.workspace.read(cx).broadcast_on();
        self.send_text(
            &command.name,
            &command.command,
            command.enter,
            all,
            None,
            window,
            cx,
        );
    }

    /// 撰写栏回车：`ctrl` 是 Ctrl+Enter（始终发所有分屏）。
    pub(crate) fn compose_submit(
        &mut self,
        ctrl: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = self.bars.compose.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        let all = ctrl || self.bars.compose_all || self.workspace.read(cx).broadcast_on();
        self.send_text("撰写栏", &text, true, all, Some(text.clone()), window, cx);
    }

    /// 快捷命令栏和撰写栏共用的发送流程（0.2.14 `sendCommandToTab`）。
    /// `compose` 有值表示是撰写栏发的：发出去之后要记历史、清空输入框。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn send_text(
        &mut self,
        name: &str,
        text: &str,
        enter: bool,
        all: bool,
        compose: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let data = quick_commands::payload(text, enter);
        let confirm = self.workspace.read(cx).settings().confirm_multiline;
        if confirm && quick_commands::is_multiline(text) {
            let (title, message) = quick_commands::multiline_confirm_text(name, text, all);
            self.modal = Some(Modal::Confirm(Confirm {
                title,
                message,
                confirm_label: Some("发送".to_owned()),
                destructive: false,
                action: ConfirmAction::SendCommand { data, all, compose },
            }));
            cx.notify();
            return;
        }
        self.do_send(&data, all, compose.as_deref(), window, cx);
    }

    pub(crate) fn do_send(
        &mut self,
        data: &str,
        all: bool,
        compose: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sent = self
            .workspace
            .update(cx, |ws, cx| ws.send_to_tab(data, all, cx));
        if sent == 0 {
            let message = if all {
                "这个标签里还没有连上的窗格"
            } else {
                "当前窗格还没连上"
            };
            self.notice(message, window, cx);
        }
        match compose {
            // 一个窗格都没发出去：内容留着（已经提示过了）。
            Some(_) if sent == 0 => {}
            Some(text) => {
                self.bars.history = compose_history::push(&self.bars.history, text);
                self.bars.nav = Nav::default();
                self.bars
                    .compose
                    .update(cx, |state, cx| state.clean(window, cx));
                let handle = self.bars.compose.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
            // 点完命令按钮把键盘还给终端。
            None => self.focus_content(window, cx),
        }
        cx.notify();
    }

    /// 撰写栏里按 ↑ / ↓：输入框是空的，或光标在第一行 / 最后一行时翻发送历史。
    /// 返回有没有处理（处理了就不让输入框再移光标）。
    pub(super) fn compose_history_key(&mut self, up: bool, cx: &mut Context<Self>) -> bool {
        let (value, start, end) = {
            let state = self.bars.compose.read(cx);
            let range = state.selected_range();
            (state.value().to_string(), range.start, range.end)
        };
        let at_edge = if up {
            compose_history::caret_on_first_line(&value, start, end)
        } else {
            compose_history::caret_on_last_line(&value, start, end)
        };
        if !at_edge {
            return false;
        }
        let Some(step) = compose_history::step(&self.bars.history, &self.bars.nav, up, &value)
        else {
            return false;
        };
        self.bars.nav = step.nav;
        let text = step.value;
        let handle = self.bars.compose.read(cx).focus_handle(cx);
        let _ = handle;
        self.pending_compose_text = Some(text);
        cx.notify();
        true
    }

    /// 渲染前把「翻历史」要填的内容写进输入框（需要 `Window`，按键处理里拿不到）。
    pub(super) fn apply_pending_compose_text(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(text) = self.pending_compose_text.take() {
            let nav = self.bars.nav.clone();
            self.bars
                .compose
                .update(cx, |state, cx| state.set_value(text, window, cx));
            // `set_value` 会触发 Change，它把翻历史的状态清了；这里放回去。
            self.bars.nav = nav;
        }
    }

    pub(super) fn compose_has_focus(&self, window: &Window, cx: &App) -> bool {
        self.bars
            .compose
            .read(cx)
            .focus_handle(cx)
            .is_focused(window)
    }

    // ---- 拖动栏高 ----

    pub(super) fn quick_drag_start(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        if event.click_count >= 2 {
            self.bars.quick_drag = None;
            self.bars.quick_live = Some(bar_height::QUICK_DEFAULT);
            self.commit_bar_height(true, cx);
            return;
        }
        self.bars.quick_drag = Some(BarDrag {
            start_y: f32::from(event.position.y),
            start_height: self.quick_height_now(),
        });
        cx.notify();
    }

    pub(super) fn compose_drag_start(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        if event.click_count >= 2 {
            self.bars.compose_drag = None;
            self.bars.compose_live = Some(bar_height::COMPOSE_AUTO);
            self.commit_bar_height(false, cx);
            return;
        }
        let start_height = self.compose_height_now(cx);
        self.bars.compose_drag = Some(BarDrag {
            start_y: f32::from(event.position.y),
            start_height,
        });
        cx.notify();
    }

    /// 鼠标移动：拖着栏的上沿时改高度。返回有没有在拖。
    pub(super) fn bars_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let y = f32::from(event.position.y);
        let area = self.area_height();
        if let Some(drag) = self.bars.quick_drag {
            if event.pressed_button != Some(MouseButton::Left) {
                self.commit_bar_height(true, cx);
                return true;
            }
            self.bars.quick_live = Some(bar_height::drag_quick(
                drag.start_height,
                drag.start_y,
                y,
                area,
            ));
            cx.notify();
            return true;
        }
        if let Some(drag) = self.bars.compose_drag {
            if event.pressed_button != Some(MouseButton::Left) {
                self.commit_bar_height(false, cx);
                return true;
            }
            self.bars.compose_live = Some(bar_height::drag_compose(
                drag.start_height,
                drag.start_y,
                y,
                area,
            ));
            cx.notify();
            return true;
        }
        false
    }

    pub(super) fn bars_mouse_up(&mut self, cx: &mut Context<Self>) {
        if self.bars.quick_drag.is_some() {
            self.commit_bar_height(true, cx);
        }
        if self.bars.compose_drag.is_some() {
            self.commit_bar_height(false, cx);
        }
    }

    /// 松手（或双击恢复）：把拖出来的高度写进设置表。
    fn commit_bar_height(&mut self, quick: bool, cx: &mut Context<Self>) {
        let (key, value) = if quick {
            self.bars.quick_drag = None;
            (
                bar_settings::QUICK_HEIGHT,
                self.bars
                    .quick_live
                    .take()
                    .unwrap_or(bar_height::QUICK_DEFAULT),
            )
        } else {
            self.bars.compose_drag = None;
            (
                bar_settings::COMPOSE_HEIGHT,
                self.bars
                    .compose_live
                    .take()
                    .unwrap_or(bar_height::COMPOSE_AUTO),
            )
        };
        if let Err(err) = self
            .store
            .write_bar_setting(|db| bar_settings::write_number(db, key, value))
        {
            tracing::warn!(%err, "保存栏高失败");
        }
        cx.notify();
    }

    // ---- 画 ----

    /// 终端页底部的两条栏；没开的不占位置。
    pub(super) fn render_bars(
        &mut self,
        tokens: &Tokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if self.quick_bar_open() {
            out.push(self.render_quick_bar(tokens, cx));
        }
        if self.compose_bar_open() {
            out.push(self.render_compose_bar(tokens, window, cx));
        }
        out
    }

    fn resize_handle(
        id: &'static str,
        on_down: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id)
            .absolute()
            .left_0()
            .right_0()
            .top(px(-3.0))
            .h(px(6.0))
            .cursor_row_resize()
            .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                cx.stop_propagation();
                on_down(event, window, cx);
            })
    }

    fn render_quick_bar(&mut self, tokens: &Tokens, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let height = self.quick_height_now();
        let tall = height > bar_height::QUICK_DEFAULT;
        let settings = self.bar_settings().clone();
        let broadcast = self.workspace.read(cx).broadcast_on();
        let pane_count = self.workspace.read(cx).pane_count();
        let managing = self.panels.visible() == Some(super::side_panel::QUICK_COMMANDS);
        let groups = quick_commands::groups_of(&settings.commands);
        let shown: Vec<QuickCommand> =
            quick_commands::commands_in_group(&settings.commands, &settings.group)
                .into_iter()
                .cloned()
                .collect();
        let group_label: SharedString =
            if settings.group.is_empty() || !groups.contains(&settings.group) {
                "全部".into()
            } else {
                settings.group.clone().into()
            };

        let hover = tokens.hover;
        let text = tokens.text;
        let muted = tokens.text_muted;
        let handle = {
            let entity = entity.clone();
            Self::resize_handle("quick-bar-resize", move |event, _window, cx| {
                entity.update(cx, |this, cx| this.quick_drag_start(event, cx));
            })
        };
        let manage = {
            let entity = entity.clone();
            div()
                .id("quick-manage")
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .size(px(24.0))
                .flex_none()
                .rounded_md()
                .cursor_pointer()
                .text_color(if managing { text } else { muted })
                .when(managing, |d| d.bg(hover))
                .hover(move |style| style.bg(hover).text_color(text))
                .tooltip(|window, cx| Tooltip::new("管理快捷命令").build(window, cx))
                .child(Icon::new(Ico::ListChecks).size_3p5())
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| this.toggle_quick_panel(window, cx));
                })
        };
        let create = {
            let entity = entity.clone();
            div()
                .id("quick-create")
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .size(px(24.0))
                .flex_none()
                .rounded_md()
                .cursor_pointer()
                .text_color(muted)
                .hover(move |style| style.bg(hover).text_color(text))
                .tooltip(|window, cx| Tooltip::new("新建快捷命令").build(window, cx))
                .child(Icon::new(IconName::Plus).size_3p5())
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| this.open_quick_panel_new(window, cx));
                })
        };
        let group_picker = (!groups.is_empty()).then(|| {
            let entity = entity.clone();
            let current = settings.group.clone();
            Button::new("quick-group")
                .ghost()
                .compact()
                .label(group_label)
                .dropdown_caret(true)
                .tooltip("分组")
                .dropdown_menu(move |menu, _window, _cx| {
                    let mut menu = menu.min_w(px(140.0)).max_h(px(280.0)).scrollable(true);
                    let all = entity.clone();
                    menu = menu.item(
                        PopupMenuItem::new("全部")
                            .checked(current.is_empty() || !groups.contains(&current))
                            .on_click(move |_: &ClickEvent, window, cx| {
                                all.update(cx, |this, cx| this.set_quick_group("", window, cx));
                            }),
                    );
                    for name in &groups {
                        let target = entity.clone();
                        let picked = name.clone();
                        menu = menu.item(
                            PopupMenuItem::new(name.clone())
                                .checked(&current == name)
                                .on_click(move |_: &ClickEvent, window, cx| {
                                    let picked = picked.clone();
                                    target.update(cx, |this, cx| {
                                        this.set_quick_group(&picked, window, cx);
                                    });
                                }),
                        );
                    }
                    menu
                })
        });

        let buttons = shown
            .iter()
            .enumerate()
            .map(|(index, command)| self.render_quick_button(index, command, tokens, cx));
        let empty_hint = shown.is_empty().then(|| {
            let entity = entity.clone();
            div()
                .id("quick-empty")
                .px_2()
                .py_0p5()
                .rounded_md()
                .cursor_pointer()
                .truncate()
                .text_color(muted)
                .hover(move |style| style.bg(hover).text_color(text))
                .child("还没有快捷命令，点这里添加常用命令（例如查看端口、系统更新）")
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| this.open_quick_panel_new(window, cx));
                })
        });
        let list = {
            let entity = entity.clone();
            div()
                .id("quick-list")
                .flex()
                .flex_1()
                .min_w_0()
                .gap_x_0p5()
                .when(tall, |d| {
                    d.flex_wrap().content_start().h_full().overflow_y_scroll()
                })
                .when(!tall, |d| d.items_center().overflow_x_scroll())
                .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                    // 双击按钮之间 / 后面的空白处：新建一条（按钮自己拦掉了点击）。
                    if event.click_count >= 2 {
                        entity.update(cx, |this, cx| this.open_quick_panel_new(window, cx));
                    }
                })
                .children(buttons)
                .children(empty_hint)
        };
        let broadcast_button = {
            let entity = entity.clone();
            let disabled = pane_count < 2 && !broadcast;
            let warn = tokens.dot_connecting;
            div()
                .id("quick-broadcast")
                .occlude()
                .flex()
                .items_center()
                .gap_1()
                .flex_none()
                .px_2()
                .py_0p5()
                .rounded_md()
                .when(tall, |d| d.mt_1())
                .when(disabled, |d| d.opacity(0.4))
                .when(!disabled, |d| d.cursor_pointer())
                .when(broadcast, |d| d.bg(tint(0xF59E0B, 0x33)).text_color(warn))
                .when(!broadcast, |d| {
                    d.text_color(muted).when(!disabled, |d| {
                        d.hover(move |style| style.bg(hover).text_color(text))
                    })
                })
                .tooltip(move |window, cx| {
                    Tooltip::new(if disabled {
                        "输入广播：先分屏（Ctrl+Shift+D / E）再开，键盘输入会同时发到所有分屏"
                    } else if broadcast {
                        "正在广播：键盘输入同时发到这个标签的所有分屏（点一下关闭，Ctrl+Shift+B）"
                    } else {
                        "输入广播：键盘输入同时发到这个标签的所有分屏（Ctrl+Shift+B）"
                    })
                    .build(window, cx)
                })
                .child(Icon::new(Ico::Radio).size_3())
                .child(if broadcast { "广播中" } else { "广播" })
                .on_click(move |_, _window, cx| {
                    if !disabled {
                        entity.update(cx, |this, cx| {
                            this.workspace.update(cx, |ws, cx| ws.toggle_broadcast(cx));
                            cx.notify();
                        });
                    }
                })
        };

        h_flex()
            .id("quick-bar")
            .relative()
            .flex_none()
            .w_full()
            .h(px(height))
            .px_1p5()
            .gap_1()
            .text_xs()
            .border_t_1()
            .border_color(tokens.border)
            .bg(tokens.background)
            .when(tall, |d| d.items_start().pt_0p5())
            .when(!tall, |d| d.items_center())
            .child(handle)
            .child(manage)
            .child(create)
            .children(group_picker)
            .child(list)
            .child(broadcast_button)
            .into_any_element()
    }

    fn render_quick_button(
        &self,
        index: usize,
        command: &QuickCommand,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let hover = tokens.hover;
        let text = tokens.text;
        let muted = tokens.text_muted;
        let color = command
            .color
            .as_deref()
            .and_then(quick_commands::color_style)
            .map(|(_, hex)| hex);
        let id = command.id.clone();
        let name: SharedString = command.name.clone().into();
        let tooltip: SharedString = format!(
            "{}{}\nCtrl+单击：发到所有分屏",
            command.command,
            if command.enter {
                ""
            } else {
                "\n（只填入，不回车）"
            }
        )
        .into();
        let run = entity.clone();
        let menu_entity = entity.clone();
        let run_id = id.clone();
        let menu_id = id.clone();
        div()
            .id(("quick-command", index))
            .occlude()
            .flex()
            .items_center()
            .gap_1()
            .flex_none()
            .max_w(px(160.0))
            .h(px(20.0))
            .my_0p5()
            .px_2()
            .rounded_md()
            .cursor_pointer()
            .when_some(color, |d, hex| {
                d.bg(tint(hex, 0x26))
                    .border_1()
                    .border_color(tint(hex, 0x66))
                    .hover(move |style| style.bg(tint(hex, 0x40)))
            })
            .when(color.is_none(), |d| d.hover(move |style| style.bg(hover)))
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            // 按钮自己吃掉按下：不让父级的「双击空白处新建」把它当成空白。
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                Icon::new(IconName::SquareTerminal)
                    .size_3()
                    .text_color(color.map_or(muted, |hex| tint(hex, 0xFF))),
            )
            .child(div().truncate().text_color(text).child(name))
            .on_click(move |event: &ClickEvent, window, cx| {
                let ctrl = event.modifiers().control || event.modifiers().platform;
                let id = run_id.clone();
                run.update(cx, |this, cx| this.run_quick_command(&id, ctrl, window, cx));
            })
            .context_menu(move |menu, _window, _cx| {
                let (send, send_all, edit, delete) = (
                    menu_entity.clone(),
                    menu_entity.clone(),
                    menu_entity.clone(),
                    menu_entity.clone(),
                );
                let (a, b, c, d) = (
                    menu_id.clone(),
                    menu_id.clone(),
                    menu_id.clone(),
                    menu_id.clone(),
                );
                menu.item(PopupMenuItem::new("发送").on_click(move |_, window, cx| {
                    let id = a.clone();
                    send.update(cx, |this, cx| {
                        this.run_quick_command(&id, false, window, cx);
                    });
                }))
                .item(PopupMenuItem::new("发到这个标签的所有分屏").on_click(
                    move |_, window, cx| {
                        let id = b.clone();
                        send_all.update(cx, |this, cx| {
                            this.run_quick_command(&id, true, window, cx);
                        });
                    },
                ))
                .item(PopupMenuItem::separator())
                .item(PopupMenuItem::new("编辑").on_click(move |_, window, cx| {
                    let id = c.clone();
                    edit.update(cx, |this, cx| this.open_quick_panel_edit(&id, window, cx));
                }))
                .item(PopupMenuItem::new("删除").on_click(move |_, _window, cx| {
                    let id = d.clone();
                    delete.update(cx, |this, cx| this.ask_delete_quick_command(&id, cx));
                }))
            })
            .into_any_element()
    }

    fn render_compose_bar(
        &mut self,
        tokens: &Tokens,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let height = self.compose_height_now(cx);
        let fixed = {
            let stored = self
                .bars
                .compose_live
                .unwrap_or(self.bar_settings().compose_height);
            stored > 0.0
        };
        let broadcast = self.workspace.read(cx).broadcast_on();
        let pane_count = self.workspace.read(cx).pane_count();
        let effective_all = self.bars.compose_all || broadcast;
        let can_send = !self.bars.compose.read(cx).value().trim().is_empty();
        let hover = tokens.hover;
        let text = tokens.text;
        let muted = tokens.text_muted;
        let warn = tokens.dot_connecting;
        let toggle_disabled = pane_count < 2 && !effective_all;

        let handle = {
            let entity = entity.clone();
            Self::resize_handle("compose-bar-resize", move |event, _window, cx| {
                entity.update(cx, |this, cx| this.compose_drag_start(event, cx));
            })
        };
        let target = {
            let entity = entity.clone();
            div()
                .id("compose-target")
                .occlude()
                .flex_none()
                .h(px(24.0))
                .px_2()
                .flex()
                .items_center()
                .rounded_md()
                .when(toggle_disabled, |d| d.opacity(0.4))
                .when(!toggle_disabled, |d| d.cursor_pointer())
                .when(effective_all, |d| d.bg(tint(0xF59E0B, 0x33)).text_color(warn))
                .when(!effective_all, |d| {
                    d.text_color(muted)
                        .when(!toggle_disabled, |d| {
                            d.hover(move |style| style.bg(hover).text_color(text))
                        })
                })
                .tooltip(move |window, cx| {
                    Tooltip::new(if broadcast {
                        "这个标签正在广播输入，Enter 和发送按钮实际发到所有分屏（先关广播才能只发当前窗格）"
                    } else if toggle_disabled {
                        "发送目标：先分屏（Ctrl+Shift+D / E）才能选「所有分屏」"
                    } else if effective_all {
                        "Enter 和发送按钮现在发到这个标签的所有分屏（点一下改回当前窗格）"
                    } else {
                        "Enter 和发送按钮现在发到当前窗格（点一下改成所有分屏）"
                    })
                    .build(window, cx)
                })
                .child(if effective_all { "所有分屏" } else { "当前窗格" })
                .on_click(move |_, _window, cx| {
                    if toggle_disabled {
                        return;
                    }
                    entity.update(cx, |this, cx| {
                        this.bars.compose_all = !this.bars.compose_all;
                        cx.notify();
                    });
                })
        };
        let send = {
            let entity = entity.clone();
            div()
                .id("compose-send")
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .size(px(24.0))
                .flex_none()
                .rounded_md()
                .when(!can_send, |d| d.opacity(0.4).text_color(muted))
                .when(can_send, |d| {
                    d.cursor_pointer()
                        .text_color(text)
                        .hover(move |style| style.bg(hover))
                })
                .tooltip(move |window, cx| {
                    Tooltip::new(if effective_all {
                        "发送到所有分屏（Enter）"
                    } else {
                        "发送到当前窗格（Enter）；Ctrl+Enter 发到所有分屏"
                    })
                    .build(window, cx)
                })
                .child(Icon::new(Ico::SendHorizontal).size_3p5())
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| this.compose_submit(false, window, cx));
                })
        };
        let input = Textarea::new(&self.bars.compose)
            .flex_1()
            .min_w_0()
            .when(fixed, |t| t.h(px((height - COMPOSE_BAR_CHROME).max(24.0))));

        h_flex()
            .id("compose-bar")
            .relative()
            .flex_none()
            .w_full()
            .px_1p5()
            .py_0p5()
            .gap_1()
            .items_end()
            .text_xs()
            .border_t_1()
            .border_color(tokens.border)
            .bg(tokens.background)
            .when(fixed, |d| d.h(px(height)))
            .child(handle)
            .child(input)
            .child(target)
            .child(send)
            .into_any_element()
    }
}
