//! 设置页的内容：把原来调试窗口里那条两行的设置面板（主题、字号、行高、光标、
//! 选中即复制、右键粘贴、多行确认、高亮、括号粘贴、启动隐藏、恢复默认）排成分组的页面。
//!
//! 主窗口把它当主视图之一显示（外面套标题和「返回」），调试窗口把它嵌在面板里。
//! 所有改动都走 `TerminalWorkspace` 的设置方法（保存、通知、套用主题都在那边），
//! 这里只负责画和接线。

use ::gpui::{App, Entity, Hsla, SharedString};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::slider::Slider;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app_theme::Tokens;
use crate::main_window::widgets::{Choice, select};
use crate::settings::{CursorShape, UiSettings};
use crate::terminal_workspace::TerminalWorkspace;

/// 设置页里带说明的一张卡（同步 / 备份这些大块用）：标题、一句说明，下面是若干行。
/// `id` 用来在驱动脚本里定位，也让每张卡有稳定的元素 ID。
pub(crate) fn card(
    id: &'static str,
    title: &'static str,
    description: Option<&'static str>,
    tokens: &Tokens,
    rows: Vec<AnyElement>,
) -> AnyElement {
    v_flex()
        .id(id)
        .w_full()
        .gap_2()
        .p_4()
        .rounded_lg()
        .border_1()
        .border_color(tokens.border)
        .bg(tokens.surface)
        .child(
            v_flex()
                .pb_1()
                .gap_0p5()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(tokens.text)
                        .child(title),
                )
                .when_some(description, |d, text| {
                    d.child(div().text_xs().text_color(tokens.text_muted).child(text))
                }),
        )
        .children(rows)
        .into_any_element()
}

/// 一行设置：左边名字和说明，右边是控件。
pub(crate) fn row(
    title: &'static str,
    description: Option<&'static str>,
    control: impl IntoElement,
    tokens: &Tokens,
) -> AnyElement {
    row_with(
        title,
        description.map(SharedString::new_static),
        control,
        tokens,
    )
}

/// 同 [`row`]，说明是运行时算出来的文字（比如当前目录）。
pub(crate) fn row_with(
    title: &'static str,
    description: Option<SharedString>,
    control: impl IntoElement,
    tokens: &Tokens,
) -> AnyElement {
    h_flex()
        .w_full()
        .min_h(::gpui::px(40.0))
        .py_1()
        .gap_4()
        .items_center()
        .justify_between()
        // 左边说明占剩下的宽度、写不下就换行；右边的控件不被它挤走。
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(div().text_sm().text_color(tokens.text).child(title))
                .when_some(description, |d, text| {
                    d.child(div().text_xs().text_color(tokens.text_muted).child(text))
                }),
        )
        .child(h_flex().flex_none().gap_1().items_center().child(control))
        .into_any_element()
}

/// 带开关的一行。
pub(crate) fn switch_row(
    id: &'static str,
    title: &'static str,
    description: &'static str,
    checked: bool,
    on_toggle: impl Fn(&mut TerminalWorkspace, &mut Context<TerminalWorkspace>) + 'static,
    ws: &Entity<TerminalWorkspace>,
    tokens: &Tokens,
) -> AnyElement {
    let ws = ws.clone();
    row(
        title,
        Some(description),
        Switch::new(id)
            .checked(checked)
            .accessibility_label(title)
            .on_click(move |_, _, cx| {
                ws.update(cx, |this, cx| on_toggle(this, cx));
            }),
        tokens,
    )
}

pub(crate) fn stepper(
    id: &'static str,
    value: String,
    on_minus: impl Fn(&mut App) + 'static,
    on_plus: impl Fn(&mut App) + 'static,
    tokens: &Tokens,
) -> impl IntoElement {
    h_flex()
        .gap_1()
        .items_center()
        .child(
            Button::new(SharedString::from(format!("{id}-minus")))
                .label("-")
                .compact()
                .on_click(move |_, _, cx| on_minus(cx)),
        )
        .child(
            div()
                .w(::gpui::px(44.0))
                .text_sm()
                .text_center()
                .text_color(tokens.text)
                .child(value),
        )
        .child(
            Button::new(SharedString::from(format!("{id}-plus")))
                .label("+")
                .compact()
                .on_click(move |_, _, cx| on_plus(cx)),
        )
}

/// 「外观」分类里由工作区驱动的几张卡：主题、应用皮肤、终端配色（同 0.2.14 的三张卡）。
/// 主机树、窗口两张卡读写数据库设置，在 `main_window::settings_page` 里。
///
/// 每一行、每一组都拆成单独的函数（`inline(never)`）：debug 构建里一个函数的栈帧会把所有临时
/// 元素都留着，写成一大坨时单个栈帧能到几百 KB，再叠上窗口绘制的调用链就把 1MB 的主线程栈
/// 撑爆了（调试窗口启动即展开设置面板时就这样溢出过）。拆开以后同一时刻只有一个小栈帧。
pub(crate) fn appearance_cards(ws: &Entity<TerminalWorkspace>, cx: &App) -> Vec<AnyElement> {
    let tokens = Tokens::current(cx);
    let s: UiSettings = ws.read(cx).settings().clone();
    vec![
        theme_card(ws, &s, &tokens),
        skin_card_group(ws, &s, &tokens),
        terminal_theme_card(ws, &s, &tokens),
    ]
}

/// 「恢复默认」：把外观和终端的设置（`ui-settings.json` 里那一份）全部还原。0.2.14 没有这个按钮。
pub(crate) fn reset_row(ws: &Entity<TerminalWorkspace>) -> AnyElement {
    let reset = ws.clone();
    h_flex()
        .id("settings-reset-row")
        .w_full()
        .justify_end()
        .child(
            Button::new("settings-reset")
                .label("恢复默认")
                .compact()
                .on_click(move |_, window, cx| {
                    reset.update(cx, |this, cx| this.reset_settings(window, cx));
                }),
        )
        .into_any_element()
}

/// 主题：系统 / 浅色 / 深色。
#[inline(never)]
fn theme_card(ws: &Entity<TerminalWorkspace>, s: &UiSettings, tokens: &Tokens) -> AnyElement {
    card(
        "settings-theme",
        "主题",
        Some("跟随系统时，Windows 切换浅色 / 深色模式，界面跟着变。"),
        tokens,
        vec![theme_row(ws, s, tokens)],
    )
}

/// 应用皮肤：浅色、深色各一排。
#[inline(never)]
fn skin_card_group(ws: &Entity<TerminalWorkspace>, s: &UiSettings, tokens: &Tokens) -> AnyElement {
    card(
        "settings-skins",
        "应用皮肤",
        Some("浅色和深色皮肤分别保存，随明暗模式生效。"),
        tokens,
        vec![
            skin_group("浅色皮肤", false, ws, s, tokens),
            skin_group("深色皮肤", true, ws, s, tokens),
        ],
    )
}

/// 终端配色：跟随皮肤或固定成某一套，固定成和界面明暗相反时给提示。
#[inline(never)]
fn terminal_theme_card(
    ws: &Entity<TerminalWorkspace>,
    s: &UiSettings,
    tokens: &Tokens,
) -> AnyElement {
    let mut rows = vec![terminal_theme_row(ws, s, tokens)];
    if let Some(hint) = terminal_mismatch_hint(ws, s, tokens) {
        rows.push(hint);
    }
    card("settings-terminal-theme", "终端配色", None, tokens, rows)
}

#[inline(never)]
fn theme_row(ws: &Entity<TerminalWorkspace>, s: &UiSettings, tokens: &Tokens) -> AnyElement {
    let (auto, dark, light) = (ws.clone(), ws.clone(), ws.clone());
    row(
        "主题",
        Some("「系统」跟随 Windows 的深色 / 浅色设置"),
        h_flex()
            .gap_1()
            .child(
                Button::new("theme-auto")
                    .label("系统")
                    .compact()
                    .when(s.follow_system, |b| b.primary())
                    .on_click(move |_, _, cx| {
                        auto.update(cx, |this, cx| this.set_theme_auto(cx));
                    }),
            )
            .child(
                Button::new("theme-light")
                    .label("浅色")
                    .compact()
                    .when(!s.follow_system && !s.dark, |b| b.primary())
                    .on_click(move |_, _, cx| {
                        light.update(cx, |this, cx| this.set_theme(false, cx));
                    }),
            )
            .child(
                Button::new("theme-dark")
                    .label("深色")
                    .compact()
                    .when(!s.follow_system && s.dark, |b| b.primary())
                    .on_click(move |_, _, cx| {
                        dark.update(cx, |this, cx| this.set_theme(true, cx));
                    }),
            ),
        tokens,
    )
}

/// 一排皮肤卡片：浅色或深色那一栏各自记住选了哪个，随明暗模式生效。
#[inline(never)]
fn skin_group(
    title: &'static str,
    dark: bool,
    ws: &Entity<TerminalWorkspace>,
    s: &UiSettings,
    tokens: &Tokens,
) -> AnyElement {
    let chosen =
        crate::app_theme::skin_for(dark, if dark { &s.skin_dark } else { &s.skin_light }).id;
    v_flex()
        .id(if dark { "skins-dark" } else { "skins-light" })
        .w_full()
        .gap_1p5()
        .py_1()
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(tokens.text_muted)
                .child(title),
        )
        .child(
            h_flex().flex_wrap().gap_2().children(
                crate::app_theme::skins_of(dark)
                    .map(|skin| skin_card(skin, skin.id == chosen, ws, tokens)),
            ),
        )
        .into_any_element()
}

/// 一张皮肤卡片：小窗口示意（侧栏 + 文字行 + 三个终端色点）加名字，选中的有强调色边框和勾。
#[inline(never)]
fn skin_card(
    skin: &'static crate::app_theme::Skin,
    selected: bool,
    ws: &Entity<TerminalWorkspace>,
    tokens: &Tokens,
) -> AnyElement {
    let color = |hex: u32| -> Hsla { ::gpui::rgb(hex).into() };
    let ws = ws.clone();
    let (dark, id) = (skin.dark, skin.id);
    let ui = skin.ui;
    let ansi = skin.terminal.ansi;
    let bar = |hex: u32, width: f32| {
        div()
            .h(::gpui::px(4.0))
            .w(::gpui::px(width))
            .rounded_full()
            .bg(color(hex))
    };
    let preview = h_flex()
        .h(::gpui::px(48.0))
        .w_full()
        .overflow_hidden()
        .rounded_md()
        .border_1()
        .border_color(color(ui.border))
        .bg(color(ui.background))
        .child(
            v_flex()
                .w(::gpui::px(26.0))
                .h_full()
                .gap_1()
                .p_1p5()
                .bg(color(ui.surface))
                .child(bar(ui.accent, 20.0))
                .child(bar(ui.border, 14.0))
                .child(bar(ui.border, 14.0)),
        )
        .child(
            v_flex()
                .flex_1()
                .h_full()
                .gap_1()
                .p_1p5()
                .child(bar(ui.text, 60.0))
                .child(bar(ui.text_muted, 86.0))
                .child(
                    h_flex().mt_auto().gap_1().children(
                        [ansi[4], ansi[2], ansi[3]]
                            .map(|hex| div().size(::gpui::px(6.0)).rounded_full().bg(color(hex))),
                    ),
                ),
        );
    v_flex()
        .id(SharedString::from(format!("skin-{id}")))
        .w(::gpui::px(150.0))
        .gap_1p5()
        .p_1p5()
        .rounded_lg()
        .border_1()
        .border_color(if selected {
            tokens.accent
        } else {
            tokens.border
        })
        .when(selected, |d| d.bg(tokens.accent_soft))
        .cursor_pointer()
        .hover(|d| d.border_color(tokens.text_muted))
        .on_click(move |_, _, cx| {
            ws.update(cx, |this, cx| this.set_skin(dark, id, cx));
        })
        .child(preview)
        .child(
            h_flex()
                .px_0p5()
                .justify_between()
                .text_xs()
                .text_color(tokens.text)
                .child(skin.name)
                .when(selected, |d| {
                    d.child(div().text_color(tokens.accent).child("\u{2713}"))
                }),
        )
        .into_any_element()
}

/// 终端配色：跟随皮肤，或固定成某一套皮肤的终端色板（同 `TerminalThemeSelect`）。
#[inline(never)]
fn terminal_theme_row(
    ws: &Entity<TerminalWorkspace>,
    s: &UiSettings,
    tokens: &Tokens,
) -> AnyElement {
    use crate::app_theme::{SKINS, TERMINAL_FOLLOW_SKIN, skin_for};
    let following = s.terminal_theme == TERMINAL_FOLLOW_SKIN;
    let current_skin = skin_for(
        tokens.dark,
        if tokens.dark {
            &s.skin_dark
        } else {
            &s.skin_light
        },
    );
    let mode_label = |dark: bool| if dark { "深色" } else { "浅色" };
    let mut choices = vec![Choice::new(
        format!("跟随皮肤（现在是 {}）", current_skin.name),
        following,
    )];
    let mut ids: Vec<&'static str> = vec![TERMINAL_FOLLOW_SKIN];
    for skin in SKINS {
        choices.push(Choice::new(
            format!("{}（{}）", skin.name, mode_label(skin.dark)),
            !following && skin.id == s.terminal_theme,
        ));
        ids.push(skin.id);
    }
    let label = if following {
        format!("跟随皮肤（现在是 {}）", current_skin.name)
    } else {
        crate::app_theme::skin(&s.terminal_theme).map_or_else(
            || s.terminal_theme.clone(),
            |skin| format!("{}（{}）", skin.name, mode_label(skin.dark)),
        )
    };
    let ws = ws.clone();
    row(
        "终端配色",
        Some("跟随皮肤时，换皮肤终端也跟着换"),
        div().w(::gpui::px(260.0)).child(select(
            "terminal-theme",
            label,
            choices,
            move |index, _, cx| {
                if let Some(id) = ids.get(index).copied() {
                    ws.update(cx, |this, cx| this.set_terminal_theme(id, cx));
                }
            },
        )),
        tokens,
    )
}

/// 终端固定成了和当前明暗相反的配色时的提示（浅色主题配深色终端之类），带一键改回跟随皮肤。
#[inline(never)]
fn terminal_mismatch_hint(
    ws: &Entity<TerminalWorkspace>,
    s: &UiSettings,
    tokens: &Tokens,
) -> Option<AnyElement> {
    let fixed = crate::app_theme::skin(&s.terminal_theme)?;
    if fixed.dark == tokens.dark {
        return None;
    }
    let ws = ws.clone();
    Some(
        h_flex()
            .id("terminal-mismatch")
            .flex_wrap()
            .gap_2()
            .items_center()
            .py_1()
            .text_xs()
            .text_color(tokens.dot_connecting)
            .child(format!(
                "终端配色固定为 {}（{}），现在是{}主题，所以终端和界面明暗不一样。",
                fixed.name,
                if fixed.dark { "深色" } else { "浅色" },
                if tokens.dark { "深色" } else { "浅色" },
            ))
            .child(
                Button::new("terminal-follow-skin")
                    .label("改回跟随皮肤")
                    .compact()
                    .on_click(move |_, _, cx| {
                        ws.update(cx, |this, cx| {
                            this.set_terminal_theme(crate::app_theme::TERMINAL_FOLLOW_SKIN, cx);
                        });
                    }),
            )
            .into_any_element(),
    )
}

/// 「终端」分类的卡：终端（字体 / 光标 / 输入法 / 复制粘贴）、关键词高亮、会话日志。
/// 后两张卡带自己的输入框，是各自的实体（`highlight_editor`、`session_log_card`），由设置页放进来。
pub(crate) fn terminal_cards(ws: &Entity<TerminalWorkspace>, cx: &App) -> Vec<AnyElement> {
    let tokens = Tokens::current(cx);
    let s: UiSettings = ws.read(cx).settings().clone();
    vec![terminal_section(ws, &s, &tokens, cx)]
}

/// 终端：字体、字号、行高、回滚行数、光标、输入法、复制 / 粘贴。已经打开的终端也会立刻跟着变。
#[inline(never)]
fn terminal_section(
    ws: &Entity<TerminalWorkspace>,
    s: &UiSettings,
    tokens: &Tokens,
    cx: &App,
) -> AnyElement {
    let mut rows = vec![font_family_row(ws, s, tokens, cx)];
    if ws.read(cx).font_custom_shown() {
        rows.push(
            div()
                .w_full()
                .child(Input::new(ws.read(cx).font_input()))
                .into_any_element(),
        );
    }
    if let Some(note) = font_fallback_note(s, cx) {
        rows.push(
            div()
                .id("font-note")
                .text_xs()
                .text_color(tokens.text_muted)
                .child(note)
                .into_any_element(),
        );
    }
    rows.extend([
        font_row(ws, s, tokens),
        line_height_row(ws, s, tokens),
        scrollback_row(ws, s, tokens, cx),
        cursor_row(ws, s, tokens),
        switch_row(
            "cursor-blink",
            "光标闪烁",
            "终端有焦点时光标一闪一闪；打字时先亮着。",
            s.cursor_blink,
            |this, cx| this.toggle_cursor_blink(cx),
            ws,
            tokens,
        ),
        switch_row(
            "ime-english",
            "终端里自动切到英文输入",
            "焦点进终端时关闭输入法（直接打英文），离开终端回到其他输入框（AI 对话框、搜索框、编辑器等）时重新打开；要在终端里打中文，按 Ctrl+空格 或点输入法图标打开。",
            s.ime_english,
            |this, cx| this.toggle_ime_english(cx),
            ws,
            tokens,
        ),
        switch_row(
            "copy-on-select",
            "选中即复制",
            "选中文字之后自动复制到剪贴板。",
            s.copy_on_select,
            |this, cx| this.toggle_copy_on_select(cx),
            ws,
            tokens,
        ),
        switch_row(
            "right-click-paste",
            "右键粘贴",
            "在终端里点右键直接粘贴剪贴板内容。",
            s.right_click_paste,
            |this, cx| this.toggle_right_click_paste(cx),
            ws,
            tokens,
        ),
        switch_row(
            "confirm-multiline",
            "多行粘贴前确认",
            "粘贴的内容带换行时先问一下：对方 shell 没开括号粘贴模式时，换行会让命令直接执行。",
            s.confirm_multiline,
            |this, cx| this.toggle_confirm_multiline(cx),
            ws,
            tokens,
        ),
        bracketed_paste_row(ws, s, tokens),
    ]);
    card(
        "settings-terminal",
        "终端",
        Some("已经打开的终端也会立刻跟着变。"),
        tokens,
        rows,
    )
}

/// 字体：只列本机装了的常见等宽字体，另有「自定义…」写一整条字体栈。
#[inline(never)]
fn font_family_row(
    ws: &Entity<TerminalWorkspace>,
    s: &UiSettings,
    tokens: &Tokens,
    cx: &App,
) -> AnyElement {
    let installed = crate::fonts::installed_fonts(cx);
    let names = crate::fonts::installed_common(installed);
    let custom = ws.read(cx).font_custom_shown();
    let leading = crate::fonts::leading_family(&s.font_family).unwrap_or_default();
    let mut choices: Vec<Choice> = names
        .iter()
        .map(|name| Choice::new(*name, !custom && name.eq_ignore_ascii_case(&leading)))
        .collect();
    choices.push(Choice::new("自定义…", custom));
    let label = if custom {
        "自定义…".to_owned()
    } else {
        leading
    };
    let ws = ws.clone();
    row(
        "字体",
        Some("从本机已装的常见等宽字体里选；需要别的字体栈就选「自定义」自己填"),
        div().w(::gpui::px(220.0)).child(select(
            "terminal-font",
            label,
            choices,
            move |index, window, cx| {
                ws.update(cx, |this, cx| match names.get(index) {
                    Some(name) => this.pick_font(name, window, cx),
                    None => this.pick_custom_font(cx),
                });
            },
        )),
        tokens,
    )
}

/// 设置的字体本机没有、改用了别的字体时给一句话。
fn font_fallback_note(s: &UiSettings, cx: &App) -> Option<String> {
    use gpui_kit::component::ActiveTheme as _;
    let installed = crate::fonts::installed_fonts(cx);
    let theme_mono = cx.theme().mono_font_family.to_string();
    crate::fonts::fallback_note(&s.font_family, installed, &theme_mono)
}

#[inline(never)]
fn font_row(ws: &Entity<TerminalWorkspace>, s: &UiSettings, tokens: &Tokens) -> AnyElement {
    let (minus, plus) = (ws.clone(), ws.clone());
    row(
        "字号",
        Some("10～24，也可以在终端里按 Ctrl+= / Ctrl+- / Ctrl+0"),
        stepper(
            "font",
            format!("{:.0}", s.font_size),
            move |cx| minus.update(cx, |this, cx| this.adjust_font(-1.0, cx)),
            move |cx| plus.update(cx, |this, cx| this.adjust_font(1.0, cx)),
            tokens,
        ),
        tokens,
    )
}

#[inline(never)]
fn line_height_row(ws: &Entity<TerminalWorkspace>, s: &UiSettings, tokens: &Tokens) -> AnyElement {
    let (minus, plus) = (ws.clone(), ws.clone());
    row(
        "行高",
        Some("1.0～1.6"),
        stepper(
            "line",
            format!("{:.2}", s.line_height),
            move |cx| minus.update(cx, |this, cx| this.adjust_line_height(-0.05, cx)),
            move |cx| plus.update(cx, |this, cx| this.adjust_line_height(0.05, cx)),
            tokens,
        ),
        tokens,
    )
}

/// 回滚行数：1000～100000，步长 1000。拖的过程中只是预览，松手才应用到所有已开的窗格。
#[inline(never)]
fn scrollback_row(
    ws: &Entity<TerminalWorkspace>,
    s: &UiSettings,
    tokens: &Tokens,
    cx: &App,
) -> AnyElement {
    let slider = ws.read(cx).scrollback_slider().clone();
    let shown = slider.read(cx).value().end().round() as usize;
    let pending = shown != s.scrollback;
    row(
        "回滚行数",
        Some("1000～100000，松手后对已开的和新开的终端都生效；调小会丢掉最旧的行"),
        h_flex()
            .gap_3()
            .items_center()
            .child(
                div().w(::gpui::px(150.0)).child(
                    Slider::new(&slider)
                        .horizontal()
                        .accessibility_label("回滚行数"),
                ),
            )
            .child(
                div()
                    .id("scrollback-value")
                    .w(::gpui::px(52.0))
                    .text_sm()
                    .text_right()
                    .text_color(if pending {
                        tokens.text_muted
                    } else {
                        tokens.text
                    })
                    .child(format!("{shown}")),
            ),
        tokens,
    )
}

#[inline(never)]
fn cursor_row(ws: &Entity<TerminalWorkspace>, s: &UiSettings, tokens: &Tokens) -> AnyElement {
    let button = |id: &'static str, label: &'static str, shape: CursorShape| {
        let ws = ws.clone();
        Button::new(id)
            .label(label)
            .compact()
            .when(s.cursor == shape, |b| b.primary())
            .on_click(move |_, _, cx| {
                ws.update(cx, |this, cx| this.set_cursor_shape(shape, cx));
            })
    };
    row(
        "光标样式",
        None,
        h_flex()
            .gap_1()
            .child(button("cursor-block", "方块", CursorShape::Block))
            .child(button("cursor-underline", "下划线", CursorShape::Underline))
            .child(button("cursor-bar", "竖线", CursorShape::Bar)),
        tokens,
    )
}

#[inline(never)]
fn bracketed_paste_row(
    ws: &Entity<TerminalWorkspace>,
    s: &UiSettings,
    tokens: &Tokens,
) -> AnyElement {
    let bracket = ws.clone();
    row(
        "括号粘贴",
        Some("自动：跟随远端终端的设置；开/关：强制"),
        Button::new("bracketed-paste")
            .label(s.bracketed_paste.label())
            .compact()
            .on_click(move |_, _, cx| {
                bracket.update(cx, |this, cx| this.cycle_bracketed_paste(cx));
            }),
        tokens,
    )
}

/// 调试窗口把外观和终端的卡摞在一起显示的页面体（产品里它们分在设置页的两个分类里）。
pub(crate) fn settings_body(ws: &Entity<TerminalWorkspace>, cx: &App) -> impl IntoElement {
    let mut cards = appearance_cards(ws, cx);
    cards.extend(terminal_cards(ws, cx));
    cards.push(reset_row(ws));
    v_flex().w_full().gap_3().children(cards)
}
