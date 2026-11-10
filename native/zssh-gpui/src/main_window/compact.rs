//! 主机属性对话框（和它的「新建代理」小对话框）用的紧凑版控件。
//!
//! `widgets.rs` 里的 `field` / `segmented_bar` / `select` / `text_button` / `modal_card` 是全应用共用的
//! （设置页、别的对话框都在用），默认样子不能动；主机对话框字段多、一屏要放下很多行，所以另做一套
//! 更小的：输入框 / 下拉 / 按钮 28 高、13px，字段标签 12px，分段按钮项高 24。圆角规则不变（单行控件仍然是胶囊，
//! 高度变小后胶囊自然变细）。别的界面不要用这里的函数，要缩小自己的界面先和设计商量。
//!
//! 尺寸为什么这样定：gpui-component 的 `Size::Medium` 是 32 高、`Size::Small` 是 24 高，都不是目标的 28，
//! 所以保持组件库的 Medium 档（内边距、插图、光标样式都按它调过），只用样式覆盖把高度压到 28、文字压到 13px；
//! 按钮的文字字号由组件库按档位写死（14px），覆盖不到，所以按钮文字改成放进子元素里自己定字号。

use ::gpui::{App, ClickEvent, Hsla, SharedString, Styled as _, Window, px};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::DropdownMenu as _;
use gpui_kit::component::*;
use gpui_kit::*;

use super::widgets::{ButtonLook, Choice};
use crate::app_theme::Tokens;
use crate::menu_kit;
use crate::typography::{Role, Typo as _};

/// 输入框 / 下拉 / 按钮的高度。
pub const CONTROL_HEIGHT: f32 = 28.0;
/// 控件里的文字。
pub const CONTROL_TEXT: f32 = 13.0;
/// 字段标签。
pub const LABEL_TEXT: f32 = 12.0;
/// 提示 / 错误小字。
pub const HINT_TEXT: f32 = 11.5;
/// 分段按钮每一项的高度、文字。
pub const SEGMENT_HEIGHT: f32 = 24.0;
pub const SEGMENT_TEXT: f32 = 12.0;
/// 标签和控件之间的间距。
pub const LABEL_GAP: f32 = 4.0;
/// 表单行与行之间的间距。
pub const ROW_GAP: f32 = 10.0;
/// 卡片的内边距、区块间距（`modal_card` 的紧凑档用）。
pub const CARD_PADDING: f32 = 16.0;
pub const CARD_GAP: f32 = 12.0;

/// 单行输入框：28 高、13px，上下内边距清零（组件库的 8px 内边距是给 32 高留的，压到 28 高会把文字挤出框）。
pub fn input(state: &::gpui::Entity<InputState>) -> Input {
    ::gpui::Styled::h(Input::new(state), px(CONTROL_HEIGHT))
        .py_0()
        .text_size(px(CONTROL_TEXT))
}

/// 字段标签：12px、字重不变（FieldLabel 的半粗）。
pub fn label(text: &str, tokens: &Tokens) -> ::gpui::Div {
    div()
        .typo(Role::FieldLabel, tokens)
        .text_size(px(LABEL_TEXT))
        .child(SharedString::from(text.to_owned()))
}

/// 带红色星号的字段标签（必填项）。
pub fn required_label(text: &str, tokens: &Tokens) -> ::gpui::Div {
    h_flex()
        .gap_0p5()
        .items_baseline()
        .child(label(text, tokens))
        .child(label("*", tokens).text_color(tokens.danger))
}

/// 灰色的小字说明。
pub fn note(text: impl Into<SharedString>, tokens: &Tokens) -> ::gpui::Div {
    div()
        .typo(Role::Caption, tokens)
        .text_size(px(HINT_TEXT))
        .line_height(::gpui::relative(1.4))
        .child(text.into())
}

/// 红色的错误 / 警告小字。
pub fn error_note(text: impl Into<SharedString>, color: Hsla) -> ::gpui::Div {
    div()
        .text_size(px(HINT_TEXT))
        .line_height(::gpui::relative(1.4))
        .text_color(color)
        .child(text.into())
}

/// 字段：上面一行小标签（可带灰色补充说明），下面是控件。
pub fn field(
    text: &str,
    hint: Option<String>,
    control: impl IntoElement,
    tokens: &Tokens,
) -> ::gpui::Div {
    field_labelled(label(text, tokens), hint, control, tokens)
}

/// 标签已经画好（比如带星号的）的字段。
pub fn field_labelled(
    label: impl IntoElement,
    hint: Option<String>,
    control: impl IntoElement,
    tokens: &Tokens,
) -> ::gpui::Div {
    v_flex()
        .w_full()
        .min_w_0()
        .gap(px(LABEL_GAP))
        .child(
            h_flex()
                .gap_2()
                .items_baseline()
                .child(label)
                .children(hint.map(|hint| note(SharedString::from(hint), tokens))),
        )
        .child(control)
}

/// 同 [`field`]，控件下面多一行红色错误。
pub fn field_with_error(
    text: &str,
    hint: Option<String>,
    control: impl IntoElement,
    error: Option<&str>,
    tokens: &Tokens,
) -> ::gpui::Div {
    let body = v_flex()
        .w_full()
        .min_w_0()
        .gap(px(LABEL_GAP))
        .child(control)
        .children(error.map(|text| error_note(text.to_owned(), tokens.danger)));
    field(text, hint, body, tokens)
}

/// 整行等分的分段控件：外框内边距仍是 2，项高 24、文字 12px。
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
        .rounded_full()
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
                        .h(px(SEGMENT_HEIGHT))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .text_size(px(SEGMENT_TEXT))
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

/// 按钮里的文字：字号自己定（组件库按档位写死 14px，覆盖不到），超长省略。
fn button_text(text: impl Into<SharedString>) -> ::gpui::Div {
    div()
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .text_size(px(CONTROL_TEXT))
        .child(text.into())
}

/// 下拉选择：和 `widgets::select` 一样的行为（点开是菜单、当前项打勾），按钮压成 28 高、13px。
pub fn select(
    id: impl Into<SharedString>,
    current: impl Into<SharedString>,
    choices: Vec<Choice>,
    on_pick: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let on_pick = std::rc::Rc::new(on_pick);
    Button::new(id.into())
        .outline()
        .child(button_text(current))
        .dropdown_caret(true)
        .w_full()
        .h(px(CONTROL_HEIGHT))
        .dropdown_menu(move |menu, _window, _cx| {
            let mut menu = menu.min_w(px(200.0)).max_h(px(320.0)).scrollable(true);
            for (index, choice) in choices.iter().enumerate() {
                let on_pick = on_pick.clone();
                let label = format!("{}{}", "　".repeat(choice.depth), choice.label);
                menu = menu.item(
                    menu_kit::choice(label, choice.selected)
                        .disabled(choice.disabled)
                        .on_click(move |_: &ClickEvent, window, cx| on_pick(index, window, cx)),
                );
            }
            menu
        })
}

/// 底部 / 对话框里的文字按钮：28 高、13px，和输入框一样高。
pub fn text_button(
    id: impl Into<SharedString>,
    text: impl Into<SharedString>,
    variant: ButtonLook,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    let button = Button::new(id.into())
        .compact()
        .child(button_text(text))
        .h(px(CONTROL_HEIGHT))
        .px(px(10.0));
    match variant {
        ButtonLook::Primary => button.primary(),
        ButtonLook::Outline => button.outline(),
        ButtonLook::Ghost => button.ghost(),
        ButtonLook::Danger => button.danger(),
    }
    .on_click(on_click)
}

/// 图标 + 文字的描边按钮（「新建代理…」）：28 高、13px。
pub fn icon_text_button(
    id: impl Into<SharedString>,
    icon: IconName,
    text: impl Into<SharedString>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Button {
    Button::new(id.into())
        .outline()
        .compact()
        .child(Icon::new(icon).size_3p5())
        .child(button_text(text))
        .h(px(CONTROL_HEIGHT))
        .px(px(10.0))
        .on_click(on_click)
}

/// 只有图标的方形小按钮（密码的小眼睛、刷新串口列表）：和输入框同高的 28 见方。
pub fn icon_button(id: impl Into<SharedString>, icon: IconName, outline: bool) -> Button {
    let button = Button::new(id.into()).compact().icon(icon).small();
    let button = if outline {
        button.outline()
    } else {
        button.ghost()
    };
    button.size(px(CONTROL_HEIGHT))
}

/// 开关卡片（高级设置里的开关）：和 `widgets::check_card` 同一个版式，内边距、字号收紧。
pub fn check_card(
    id: impl Into<SharedString>,
    title: &str,
    hint: Option<&str>,
    checked: bool,
    tokens: &Tokens,
    on_toggle: impl Fn(bool, &mut Window, &mut App) + 'static,
) -> ::gpui::Div {
    h_flex()
        .w_full()
        .gap_2p5()
        .items_start()
        .px_2p5()
        .py_2()
        .rounded(px(crate::app_theme::CONTROL_RADIUS))
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
                .child(label(title, tokens))
                .children(hint.map(|hint| note(SharedString::from(hint.to_owned()), tokens))),
        )
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{
        CARD_GAP, CARD_PADDING, CONTROL_HEIGHT, CONTROL_TEXT, HINT_TEXT, LABEL_GAP, LABEL_TEXT,
        ROW_GAP, SEGMENT_HEIGHT, SEGMENT_TEXT,
    };

    #[test]
    fn the_compact_sizes_are_the_ones_the_card_asked_for() {
        // 输入框 / 下拉 / 按钮 28 高、13px；分段按钮项高 24、12px；标签 12px；标签与控件间距 4、行距 10。
        assert_eq!(CONTROL_HEIGHT, 28.0);
        assert_eq!(CONTROL_TEXT, 13.0);
        assert_eq!(SEGMENT_HEIGHT, 24.0);
        assert_eq!(SEGMENT_TEXT, 12.0);
        assert_eq!(LABEL_TEXT, 12.0);
        assert_eq!(LABEL_GAP, 4.0);
        assert_eq!(ROW_GAP, 10.0);
        assert_eq!((CARD_PADDING, CARD_GAP), (16.0, 12.0));
        assert!(
            (11.0..=12.0).contains(&HINT_TEXT),
            "提示 / 错误文字 11–12px"
        );
    }
}
