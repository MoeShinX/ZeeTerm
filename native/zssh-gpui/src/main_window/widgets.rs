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

use zssh_core::model::Protocol;

use crate::app_theme::Tokens;
use crate::typography::{Role, Typo as _};

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
                        .typo(Role::FieldLabel, tokens)
                        .child(SharedString::from(label.to_owned())),
                )
                .children(hint.map(|hint| {
                    div()
                        .typo(Role::Caption, tokens)
                        .child(SharedString::from(hint))
                })),
        )
        .child(control)
}

/// 同 [`field`]，控件下面多一行红色的错误提示（保存校验的结果落在对应字段旁）。
pub fn field_with_error(
    label: &str,
    hint: Option<String>,
    control: impl IntoElement,
    error: Option<&str>,
    tokens: &Tokens,
) -> Div {
    let body = v_flex()
        .w_full()
        .min_w_0()
        .gap_1p5()
        .child(control)
        .children(error.map(|text| error_note(text.to_owned(), tokens.danger)));
    field(label, hint, body, tokens)
}

/// 分段按钮：一排互斥的选项，选中的高亮（协议选择、执行模式这类三五个选项的开关）。
/// `options` 里每项是（文字，是否选中，不可选时的悬停说明——`None` 表示可选）。
pub fn segmented(
    id: &'static str,
    options: Vec<(String, bool, Option<&'static str>)>,
    on_pick: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let on_pick = std::rc::Rc::new(on_pick);
    h_flex()
        .w_full()
        .gap_1()
        .children(
            options
                .into_iter()
                .enumerate()
                .map(|(index, (label, selected, locked))| {
                    let on_pick = on_pick.clone();
                    let button = Button::new((id, index))
                        .label(label)
                        .compact()
                        .flex_1()
                        .when(selected, |b| b.primary())
                        .when(!selected, |b| b.outline())
                        .disabled(locked.is_some());
                    let button = match locked {
                        Some(reason) => button.tooltip(reason),
                        None => button,
                    };
                    button.on_click(move |_, window, cx| on_pick(index, window, cx))
                }),
        )
}

/// 整行等分的分段控件（V2-U13 主机表单：协议 / 认证方式 / 连接方式）：外面一个带边框的圆角条，
/// 每个选项平分宽度，选中项实底（强调色）。`options` 同 [`segmented`]：（文字，是否选中，不可选时的悬停说明）。
pub fn segmented_bar(
    id: &'static str,
    options: Vec<(String, bool, Option<&'static str>)>,
    tokens: &Tokens,
    on_pick: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let on_pick = std::rc::Rc::new(on_pick);
    let (accent, on_accent, hover, muted) = (
        tokens.accent,
        tokens.on_accent,
        tokens.hover,
        tokens.text_muted,
    );
    h_flex()
        .w_full()
        .p_0p5()
        .gap_0p5()
        .rounded_md()
        .border_1()
        .border_color(tokens.border)
        .bg(tokens.subtle)
        .children(
            options
                .into_iter()
                .enumerate()
                .map(|(index, (label, selected, locked))| {
                    let on_pick = on_pick.clone();
                    let item = div()
                        .id((id, index))
                        .flex_1()
                        .min_w_0()
                        .h(px(28.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_md()
                        .text_sm()
                        .truncate()
                        .child(SharedString::from(label));
                    match (selected, locked) {
                        (true, _) => item
                            .bg(accent)
                            .text_color(on_accent)
                            .font_weight(FontWeight::MEDIUM),
                        (false, Some(reason)) => {
                            item.text_color(muted)
                                .opacity(0.5)
                                .tooltip(move |window, cx| {
                                    gpui_kit::component::tooltip::Tooltip::new(reason)
                                        .build(window, cx)
                                })
                        }
                        (false, None) => item
                            .cursor_pointer()
                            .hover(move |style| style.bg(hover))
                            .on_click(move |_, window, cx| on_pick(index, window, cx)),
                    }
                }),
        )
}

/// 开关卡片（V2-U13 高级设置里的开关）：带边框的一块，左边勾选框，右边标题 + 一行说明。
pub fn check_card(
    id: impl Into<SharedString>,
    title: &str,
    hint: Option<&str>,
    checked: bool,
    tokens: &Tokens,
    on_toggle: impl Fn(bool, &mut Window, &mut App) + 'static,
) -> Div {
    h_flex()
        .w_full()
        .gap_3()
        .items_start()
        .p_3()
        .rounded_md()
        .border_1()
        .border_color(tokens.border)
        .child(
            gpui_kit::component::checkbox::Checkbox::new(id.into())
                .checked(checked)
                .on_click(move |value, window, cx| on_toggle(*value, window, cx)),
        )
        .child(
            v_flex()
                .min_w_0()
                .gap_0p5()
                .child(
                    div()
                        .typo(Role::FieldLabel, tokens)
                        .child(SharedString::from(title.to_owned())),
                )
                .children(hint.map(|hint| note(SharedString::from(hint.to_owned()), tokens))),
        )
}

/// 带红色星号的字段标签（必填项）。
pub fn required_label(label: &str, tokens: &Tokens) -> Div {
    h_flex()
        .gap_1()
        .items_baseline()
        .child(
            div()
                .typo(Role::FieldLabel, tokens)
                .child(SharedString::from(label.to_owned())),
        )
        .child(
            div()
                .typo(Role::FieldLabel, tokens)
                .text_color(tokens.danger)
                .child("*"),
        )
}

/// 面板里的一组字段：全大写风格的小标题 + 内容。
pub fn section(title: &str, children: Vec<AnyElement>, tokens: &Tokens) -> Div {
    v_flex()
        .w_full()
        .min_w_0()
        .gap_2()
        .child(
            div()
                .typo(Role::GroupTitle, tokens)
                .child(SharedString::from(title.to_owned())),
        )
        .children(children)
}

/// 灰色的小字说明。
pub fn note(text: impl Into<SharedString>, tokens: &Tokens) -> Div {
    div().typo(Role::Caption, tokens).child(text.into())
}

/// 管理页列表为空时的空状态：居中一句话（可带一行灰色补充）+ 一个主按钮。
pub fn empty_state(
    text: &str,
    hint: Option<&str>,
    button: Option<AnyElement>,
    tokens: &Tokens,
) -> Div {
    v_flex()
        .w_full()
        .py_10()
        .gap_3()
        .items_center()
        .child(
            div()
                .typo(Role::Body, tokens)
                .text_color(tokens.text_muted)
                .child(SharedString::from(text.to_owned())),
        )
        .children(hint.map(|hint| note(SharedString::from(hint.to_owned()), tokens)))
        .children(button)
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
                .child(
                    div()
                        .typo(Role::FieldLabel, tokens)
                        .child(SharedString::from(title.to_owned())),
                )
                .children(hint.map(|hint| note(hint, tokens))),
        )
        .child(
            Switch::new(id)
                .checked(checked)
                .on_click(move |_, window, cx| on_toggle(window, cx)),
        )
}

/// 协议的图标：SSH 是方框终端，Telnet 是命令行，串口是线缆。
pub fn protocol_icon(protocol: Protocol) -> Icon {
    match protocol {
        Protocol::Ssh => Icon::new(IconName::SquareTerminal),
        Protocol::Telnet => Icon::new(crate::icons::Ico::Terminal),
        Protocol::Serial => Icon::new(crate::icons::Ico::Cable),
    }
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
