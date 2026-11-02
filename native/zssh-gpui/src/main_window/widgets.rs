//! 主窗口里面板和对话框共用的小部件：字段（标签 + 控件）、分区标题、下拉选择、开关行、
//! 徽标、图标按钮。样子对照 0.2.14 的 `AsidePanelSection` / `Label` / `Select` / `Badge`。
//!
//! 只做外观，不持有状态；颜色一律从 `Tokens` 取。

use ::gpui::{App, ClickEvent, Hsla, SharedString, Window, px};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app_theme::Tokens;

/// 字段：上面一行小标签（可带灰色的补充说明），下面是控件。
pub fn field(label: &str, hint: Option<String>, control: impl IntoElement, tokens: &Tokens) -> Div {
    v_flex()
        .w_full()
        .min_w_0()
        .gap_1p5()
        .child(
            h_flex()
                .gap_2()
                .items_baseline()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(SharedString::from(label.to_owned())),
                )
                .children(hint.map(|hint| {
                    div()
                        .text_xs()
                        .text_color(tokens.text_muted)
                        .child(SharedString::from(hint))
                })),
        )
        .child(control)
}

/// 面板里的一组字段：全大写风格的小标题 + 内容。
pub fn section(title: &str, children: Vec<AnyElement>, tokens: &Tokens) -> Div {
    v_flex()
        .w_full()
        .min_w_0()
        .gap_2()
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(tokens.text_muted)
                .child(SharedString::from(title.to_owned())),
        )
        .children(children)
}

/// 灰色的小字说明。
pub fn note(text: impl Into<SharedString>, tokens: &Tokens) -> Div {
    div()
        .text_xs()
        .text_color(tokens.text_muted)
        .child(text.into())
}

/// 红色的错误 / 警告小字。
pub fn error_note(text: impl Into<SharedString>, color: Hsla) -> Div {
    div().text_xs().text_color(color).child(text.into())
}

/// 下拉里的一项。
pub struct Choice {
    pub label: String,
    pub selected: bool,
    pub disabled: bool,
    /// 缩进层级（分组下拉里子分组缩进）。
    pub depth: usize,
}

impl Choice {
    pub fn new(label: impl Into<String>, selected: bool) -> Self {
        Self {
            label: label.into(),
            selected,
            disabled: false,
            depth: 0,
        }
    }

    pub fn depth(mut self, depth: usize) -> Self {
        self.depth = depth;
        self
    }
}

/// 下拉选择：一个带小箭头的按钮，点开是菜单，当前项打勾；选中后回调选项的下标。
/// 菜单项多时可以滚动。
pub fn select(
    id: impl Into<SharedString>,
    current: impl Into<SharedString>,
    choices: Vec<Choice>,
    on_pick: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let on_pick = std::rc::Rc::new(on_pick);
    Button::new(id.into())
        .outline()
        .label(current.into())
        .dropdown_caret(true)
        .w_full()
        .dropdown_menu(move |menu, _window, _cx| {
            let mut menu = menu.min_w(px(220.0)).max_h(px(320.0)).scrollable(true);
            for (index, choice) in choices.iter().enumerate() {
                let on_pick = on_pick.clone();
                let label = format!("{}{}", "　".repeat(choice.depth), choice.label);
                menu = menu.item(
                    PopupMenuItem::new(label)
                        .checked(choice.selected)
                        .disabled(choice.disabled)
                        .on_click(move |_: &ClickEvent, window, cx| on_pick(index, window, cx)),
                );
            }
            menu
        })
}

/// 左边说明、右边开关的一行。
pub fn switch_row(
    id: impl Into<SharedString>,
    title: &str,
    hint: Option<String>,
    checked: bool,
    on_toggle: impl Fn(&mut Window, &mut App) + 'static,
    tokens: &Tokens,
) -> Div {
    let id: SharedString = id.into();
    h_flex()
        .w_full()
        .gap_3()
        .items_start()
        .justify_between()
        .child(
            v_flex()
                .min_w_0()
                .gap_0p5()
                .child(div().text_sm().child(SharedString::from(title.to_owned())))
                .children(hint.map(|hint| note(hint, tokens))),
        )
        .child(
            Switch::new(id)
                .checked(checked)
                .on_click(move |_, window, cx| on_toggle(window, cx)),
        )
}

/// 小徽标。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BadgeKind {
    Neutral,
    Outline,
    Good,
    Bad,
    Busy,
}

pub fn badge(text: impl Into<SharedString>, kind: BadgeKind, tokens: &Tokens) -> Div {
    let (fg, bg, border) = match kind {
        BadgeKind::Neutral => (tokens.text, tokens.hover, None),
        BadgeKind::Outline => (tokens.text_muted, tokens.background, Some(tokens.border)),
        BadgeKind::Good => (tokens.on_accent, tokens.dot_connected, None),
        BadgeKind::Bad => (tokens.danger, tokens.background, Some(tokens.danger)),
        BadgeKind::Busy => (tokens.text, tokens.hover, None),
    };
    div()
        .flex_none()
        .px_2()
        .py_0p5()
        .rounded_md()
        .text_xs()
        .text_color(fg)
        .bg(bg)
        .when_some(border, |d, color| d.border_1().border_color(color))
        .child(text.into())
}

/// 只有图标的小按钮（有悬停提示）。
pub fn icon_button(
    id: impl Into<SharedString>,
    icon: impl Into<Icon>,
    tooltip: &'static str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    Button::new(id.into())
        .ghost()
        .compact()
        .icon(icon.into())
        .tooltip(tooltip)
        .on_click(on_click)
}

/// 面板 / 对话框底部的按钮：`danger` 是红色的删除、`primary` 是保存。
pub fn text_button(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    variant: ButtonLook,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    let button = Button::new(id.into()).label(label.into()).compact();
    match variant {
        ButtonLook::Primary => button.primary(),
        ButtonLook::Outline => button.outline(),
        ButtonLook::Ghost => button.ghost(),
        ButtonLook::Danger => button.danger(),
    }
    .on_click(on_click)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonLook {
    Primary,
    Outline,
    Ghost,
    Danger,
}

/// 点击回调：把「在主窗口上做一件事」包成按钮要的闭包（省得每个按钮都克隆一遍实体）。
pub fn act(
    entity: &Entity<super::MainWindow>,
    f: impl Fn(&mut super::MainWindow, &mut Window, &mut Context<super::MainWindow>) + 'static,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let entity = entity.clone();
    move |_, window, cx| {
        entity.update(cx, |this, cx| f(this, window, cx));
    }
}

/// 下拉选中回调：选项下标 -> 在主窗口上做一件事。
pub fn pick(
    entity: &Entity<super::MainWindow>,
    f: impl Fn(&mut super::MainWindow, usize, &mut Window, &mut Context<super::MainWindow>) + 'static,
) -> impl Fn(usize, &mut Window, &mut App) + 'static {
    let entity = entity.clone();
    move |index, window, cx| {
        entity.update(cx, |this, cx| f(this, index, window, cx));
    }
}
