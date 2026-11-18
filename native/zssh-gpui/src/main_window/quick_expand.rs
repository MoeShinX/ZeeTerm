//! 快捷命令栏展开成面板（V2-U47）：左边一列分组（带条数），右边顶部搜索框 + 命令块网格。
//!
//! 收起时仍是 `bars.rs` 里原来的一行栏，一点没动；展开只是同一份命令数据换一种摆法：
//! - 当前分组和栏上的下拉共用设置键 `quick_commands_group`，两边永远选的是同一项；
//! - 命令块直接复用 `render_quick_button`（单击发送、Ctrl+单击 / 广播、右键菜单、悬停提示都是它的），
//!   管理 / 新建 / 广播按钮也和栏共用同一份代码，发送走 `run_quick_command` → `send_text` → `do_send`，没有第二条路；
//! - 展开状态（`quick_commands_expanded`）和面板高度（`layout.quick_panel_height`）存本机设置，不同步。
//!
//! 命令栏本身关着（状态栏开关）时 `render_bars` 根本不画命令栏，面板自然也不出现。

use ::gpui::{ClickEvent, MouseButton, MouseDownEvent, MouseMoveEvent, SharedString, Window, px};
use gpui_kit::component::input::Input;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::MainWindow;
use super::bar_height;
use super::bar_settings;
use super::bars::{BarDrag, BarsMsg};
use super::quick_commands::{self, GroupEntry, QuickCommand};
use crate::app_theme::Tokens;

/// 左边分组列的宽度。
const GROUP_COLUMN_WIDTH: f32 = 132.0;

impl MainWindow {
    // ---- 状态 ----

    /// 展开面板现在显示着没有：命令栏开着，并且上次是展开的。
    pub(super) fn quick_panel_shown(&self) -> bool {
        self.quick_bar_open() && self.bar_settings().quick_expanded
    }

    /// 展开面板现在的实际高度（拖动中用拖出来的值）。
    pub(super) fn quick_panel_height_now(&self) -> f32 {
        bar_height::clamp_panel(
            self.bars
                .panel_live
                .unwrap_or(self.bar_settings().quick_panel_height),
            self.area_height(),
        )
    }

    /// 展开 / 收起。收起时清掉搜索词（下次展开从头开始）；搜索框有焦点时把键盘还给终端。
    pub(crate) fn toggle_quick_expanded(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = !self.bar_settings().quick_expanded;
        // 要在写设置之前看：写完以后面板已经算「没显示」，焦点判断会落空。
        let had_focus = !next && self.quick_search_focused(window, cx);
        self.write_setting(
            |db| bar_settings::write_bool(db, bar_settings::QUICK_EXPANDED, next),
            window,
            cx,
        );
        if !next {
            self.bars
                .quick_search
                .update(cx, |input, cx| input.set_value("", window, cx));
            if had_focus {
                self.focus_content(window, cx);
            }
        }
    }

    // ---- 搜索框的键盘 ----

    pub(super) fn quick_search_focused(&self, window: &Window, cx: &App) -> bool {
        self.quick_panel_shown()
            && self
                .bars
                .quick_search
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
    }

    /// 面板搜索框里按 Esc：有搜索词先清空，再按一次把键盘还给终端。返回有没有处理。
    pub(super) fn quick_search_escape(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.quick_search_focused(window, cx) {
            return false;
        }
        if self.bars.quick_search.read(cx).value().is_empty() {
            self.focus_content(window, cx);
        } else {
            self.bars
                .quick_search
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        cx.notify();
        true
    }

    // ---- 拖动面板高度 ----

    fn quick_panel_drag_start(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        if event.click_count >= 2 {
            self.bars.panel_drag = None;
            self.bars.panel_live = Some(bar_height::PANEL_DEFAULT);
            self.commit_panel_height(cx);
            return;
        }
        self.bars.panel_drag = Some(BarDrag {
            start_y: f32::from(event.position.y),
            start_height: self.quick_panel_height_now(),
        });
        cx.notify();
    }

    /// 鼠标移动：拖着面板上沿时改高度。返回有没有在拖。
    pub(super) fn quick_panel_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(drag) = self.bars.panel_drag else {
            return false;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.commit_panel_height(cx);
            return true;
        }
        self.bars.panel_live = Some(bar_height::drag_panel(
            drag.start_height,
            drag.start_y,
            f32::from(event.position.y),
            self.area_height(),
        ));
        cx.notify();
        true
    }

    pub(super) fn quick_panel_mouse_up(&mut self, cx: &mut Context<Self>) {
        if self.bars.panel_drag.is_some() {
            self.commit_panel_height(cx);
        }
    }

    /// 松手（或双击恢复）：把拖出来的高度写进设置表。只点了一下没拖动时保持现在的高度。
    fn commit_panel_height(&mut self, cx: &mut Context<Self>) {
        let value = self
            .bars
            .panel_live
            .unwrap_or_else(|| self.quick_panel_height_now());
        self.bars.panel_drag = None;
        self.bars.panel_live = None;
        let value = bar_height::clamp_panel(value, self.area_height());
        if let Err(err) = self.store.write_bar_setting(|db| {
            bar_settings::write_number(db, bar_settings::QUICK_PANEL_HEIGHT, value)
        }) {
            tracing::warn!(%err, "保存命令面板高度失败");
        }
        cx.notify();
    }

    // ---- 画 ----

    /// 命令栏右端的「展开」按钮；面板右上角的「收起」按钮是同一个（`expanded` 决定图标和提示）。
    pub(super) fn quick_expand_button(
        entity: &Entity<Self>,
        expanded: bool,
        tokens: &Tokens,
    ) -> AnyElement {
        let (hover, text, muted) = (tokens.hover, tokens.text, tokens.text_muted);
        let entity = entity.clone();
        div()
            .id(if expanded {
                "quick-collapse"
            } else {
                "quick-expand"
            })
            .debug_selector(move || {
                if expanded {
                    "quick-collapse".to_owned()
                } else {
                    "quick-expand".to_owned()
                }
            })
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .size(px(24.0))
            .flex_none()
            .rounded_full()
            .cursor_pointer()
            .text_color(muted)
            .hover(move |style| style.bg(hover).text_color(text))
            .tooltip(move |window, cx| {
                Tooltip::new(crate::i18n::t(if expanded {
                    BarsMsg::CollapseTip
                } else {
                    BarsMsg::ExpandTip
                }))
                .build(window, cx)
            })
            .child(
                Icon::new(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronUp
                })
                .size_4(),
            )
            .on_click(move |_, window, cx| {
                entity.update(cx, |this, cx| this.toggle_quick_expanded(window, cx));
            })
            .into_any_element()
    }

    /// 左边一列里的一项：名称 + 条数，当前分组高亮。
    fn quick_group_row(
        &self,
        index: usize,
        entry: &GroupEntry,
        current: bool,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let (hover, text, muted) = (tokens.hover, tokens.text, tokens.text_muted);
        let label: SharedString = if entry.key.is_empty() {
            crate::i18n::t(BarsMsg::GroupAll).into()
        } else if entry.key == quick_commands::UNGROUPED_KEY {
            crate::i18n::t(BarsMsg::Ungrouped).into()
        } else {
            entry.key.clone().into()
        };
        let key = entry.key.clone();
        let count = entry.count;
        h_flex()
            .id(("quick-group-row", index))
            .debug_selector(move || format!("quick-group-row-{index}"))
            .occlude()
            .flex_none()
            .h(px(24.0))
            .px_2()
            .gap_1()
            .items_center()
            .rounded_full()
            .cursor_pointer()
            .text_color(text)
            .when(current, |d| d.bg(tokens.accent_soft))
            .when(!current, |d| d.hover(move |style| style.bg(hover)))
            .child(div().flex_1().min_w_0().truncate().child(label))
            .child(div().flex_none().text_color(muted).child(count.to_string()))
            .on_click(move |_: &ClickEvent, window, cx| {
                let key = key.clone();
                entity.update(cx, |this, cx| this.set_quick_group(&key, window, cx));
            })
            .into_any_element()
    }

    pub(super) fn render_quick_panel(
        &mut self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let height = self.quick_panel_height_now();
        let settings = self.bar_settings().clone();
        let broadcast = self.workspace.read(cx).broadcast_on();
        let pane_count = self.workspace.read(cx).pane_count();
        let managing = self.panels.visible() == Some(super::side_panel::QUICK_COMMANDS);
        let (hover, text, muted) = (tokens.hover, tokens.text, tokens.text_muted);

        let current = quick_commands::effective_group(&settings.commands, &settings.group);
        let query = self.bars.quick_search.read(cx).value().to_string();
        let shown: Vec<QuickCommand> =
            quick_commands::panel_shown(&settings.commands, &current, &query)
                .into_iter()
                .cloned()
                .collect();
        let entries = quick_commands::panel_entries(&settings.commands);

        let handle = {
            let entity = entity.clone();
            Self::resize_handle("quick-panel-resize", move |event, _window, cx| {
                entity.update(cx, |this, cx| this.quick_panel_drag_start(event, cx));
            })
        };

        let rows: Vec<AnyElement> = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                self.quick_group_row(index, entry, entry.key == current, tokens, cx)
            })
            .collect();
        let groups_column = v_flex()
            .id("quick-panel-groups")
            .flex_none()
            .w(px(GROUP_COLUMN_WIDTH))
            .h_full()
            .p_1()
            .gap_0p5()
            .border_r_1()
            .border_color(tokens.divider)
            .overflow_y_scroll()
            .children(rows);

        let header = h_flex()
            .flex_none()
            .gap_1()
            .px_1p5()
            .py_1()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .debug_selector(|| "quick-search".to_owned())
                    .child(
                        Input::new(&self.bars.quick_search)
                            .small()
                            .prefix(Icon::new(IconName::Search))
                            .cleanable(true),
                    ),
            )
            .child(Self::quick_manage_button(&entity, managing, tokens))
            .child(Self::quick_create_button(&entity, tokens))
            .child(Self::quick_broadcast_button(
                &entity, false, broadcast, pane_count, tokens,
            ))
            .child(Self::quick_expand_button(&entity, true, tokens));

        let buttons = shown
            .iter()
            .enumerate()
            .map(|(index, command)| self.render_quick_button(index, command, tokens, cx));
        // 一条命令都没有：和栏一样给一句说明，点一下新建；有命令但搜索没命中：只说没有匹配的。
        let hint = if settings.commands.is_empty() {
            let entity = entity.clone();
            Some(
                div()
                    .id("quick-empty")
                    .debug_selector(|| "quick-empty".to_owned())
                    .px_2()
                    .py_0p5()
                    .rounded_full()
                    .cursor_pointer()
                    .truncate()
                    .text_color(muted)
                    .hover(move |style| style.bg(hover).text_color(text))
                    .child(crate::i18n::t(BarsMsg::EmptyHint))
                    .on_click(move |_, window, cx| {
                        entity.update(cx, |this, cx| this.open_quick_new(window, cx));
                    })
                    .into_any_element(),
            )
        } else if shown.is_empty() {
            Some(
                div()
                    .id("quick-no-match")
                    .debug_selector(|| "quick-no-match".to_owned())
                    .px_2()
                    .py_0p5()
                    .text_color(muted)
                    .child(crate::i18n::t(BarsMsg::NoMatches))
                    .into_any_element(),
            )
        } else {
            None
        };
        let list = {
            let entity = entity.clone();
            div()
                .id("quick-panel-list")
                .debug_selector(|| "quick-panel-list".to_owned())
                .flex()
                .flex_wrap()
                .content_start()
                .flex_1()
                .min_h_0()
                .gap_x_0p5()
                .px_1p5()
                .overflow_y_scroll()
                .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                    // 双击命令块之间 / 后面的空白处：新建一条（命令块自己拦掉了点击）。
                    if event.click_count >= 2 {
                        entity.update(cx, |this, cx| this.open_quick_new(window, cx));
                    }
                })
                .children(buttons)
                .children(hint)
        };

        v_flex()
            .id("quick-panel")
            .debug_selector(|| "quick-panel".to_owned())
            .relative()
            .flex_none()
            .w_full()
            .h(px(height))
            .text_xs()
            .border_t_1()
            .border_color(tokens.divider)
            .bg(tokens.background)
            .child(handle)
            .child(
                h_flex().flex_1().min_h_0().child(groups_column).child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .child(header)
                        .child(list),
                ),
            )
            .into_any_element()
    }
}
