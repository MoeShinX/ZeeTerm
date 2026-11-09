//! 设置页的内容：把原来调试窗口里那条两行的设置面板（主题、字号、行高、光标、
//! 选中即复制、右键粘贴、多行确认、高亮、括号粘贴）排成分组的页面；外观页和终端页各有一个只还原自己字段的
//! 「恢复本页默认设置」。
//!
//! 主窗口把它当主视图之一显示（外面套标题和「返回」），调试窗口把它嵌在面板里。
//! 所有改动都走 `TerminalWorkspace` 的设置方法（保存、通知、套用主题都在那边），
//! 这里只负责画和接线。

use ::gpui::{App, Entity, Hsla, SharedString};
use gpui_kit::component::button::Button;
use gpui_kit::component::input::Input;
use gpui_kit::component::slider::Slider;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::icons::Ico;
use crate::main_window::widgets::{Choice, select};
use crate::settings::{CursorShape, UiSettings};
use crate::terminal_workspace::TerminalWorkspace;
use crate::typography::{Role, Typo as _};

/// 「外观」「终端」两个分类的界面文案（切语言即时生效）。
///
/// 只放应用自己的固定文案；皮肤名、字体名这些来自本机的数据照原样显示，不进这张表。
/// 带运行时数值的句子保留 `format!`，这里只提供固定片段（全角括号、标点也各算一段）。
#[derive(Clone, Copy, Debug)]
enum SettingsViewMsg {
    // ---- 外观 · 主题 ----
    Theme,
    Light,
    Dark,
    FollowSystem,
    AppearanceTitle,
    AppearanceDescription,
    // ---- 外观 · 应用皮肤 ----
    SkinsDescription,
    LightSkin,
    DarkSkin,
    // ---- 外观 · 终端配色 ----
    TerminalColorsTitle,
    FollowSkinNowPrefix,
    OpenParen,
    CloseParen,
    SchemeTitle,
    SchemeDescription,
    MismatchLead,
    MismatchMid,
    MismatchEnd,
    FollowSkinButton,
    ResetPageButton,
    ResetRowTitle,
    ResetAppearanceScope,
    ResetTerminalScope,
    ResetSftpScope,
    // ---- 终端 · 字体与显示 ----
    ShowLatencyTitle,
    ShowLatencyDescription,
    FontDisplayTitle,
    FontDisplayDescription,
    CustomOption,
    FontFamilyTitle,
    FontFamilyDescription,
    FontSizeTitle,
    FontSizeDescription,
    LineHeightTitle,
    LineHeightDescription,
    ScrollbackTitle,
    ScrollbackDescription,
    // ---- 终端 · 光标 ----
    CursorTitle,
    CursorBlinkTitle,
    CursorBlinkDescription,
    CursorShapeTitle,
    CursorBlock,
    CursorUnderline,
    CursorBar,
    // ---- 终端 · 输入与剪贴板 ----
    InputTitle,
    ImeTitle,
    ImeDescription,
    CopyOnSelectTitle,
    CopyOnSelectDescription,
    RightClickPasteTitle,
    RightClickPasteDescription,
    ConfirmMultilineTitle,
    ConfirmMultilineDescription,
    BracketedPasteTitle,
    BracketedPasteDescription,
}

impl Msg for SettingsViewMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::Theme => "主题与皮肤",
            Self::Light => "浅色",
            Self::Dark => "深色",
            Self::FollowSystem => "跟随系统",
            Self::AppearanceTitle => "应用外观",
            Self::AppearanceDescription if !cfg!(windows) => {
                "选「跟随系统」时，系统切换浅色 / 深色模式，界面跟着变。"
            }
            Self::AppearanceDescription => {
                "选「跟随系统」时，Windows 切换浅色 / 深色模式，界面跟着变。"
            }
            Self::SkinsDescription => "浅色和深色皮肤分别保存，随明暗模式生效。",
            Self::LightSkin => "浅色皮肤",
            Self::DarkSkin => "深色皮肤",
            Self::TerminalColorsTitle => "终端配色",
            Self::FollowSkinNowPrefix => "跟随皮肤（现在是 ",
            Self::OpenParen => "（",
            Self::CloseParen => "）",
            Self::SchemeTitle => "配色方案",
            Self::SchemeDescription => "跟随皮肤时，换皮肤终端也跟着换",
            Self::MismatchLead => "终端配色固定为 ",
            Self::MismatchMid => "，现在是",
            Self::MismatchEnd => "主题，所以终端和界面明暗不一样。",
            Self::FollowSkinButton => "改回跟随皮肤",
            Self::ResetPageButton => "恢复本页默认设置",
            Self::ResetRowTitle => "恢复默认",
            Self::ResetAppearanceScope => {
                "只恢复本页的主题、应用皮肤和终端配色；主机树选项、语言、终端字体等其他设置不变。"
            }
            Self::ResetTerminalScope => {
                "只恢复本页的字体与显示、光标、输入与剪贴板、括号粘贴和高亮开关；高亮规则、会话日志、主题和其他页的设置不变。"
            }
            Self::ResetSftpScope => {
                "只恢复本页「文件面板」里的打开位置、本地栏、排列、本地默认目录和跟随终端目录；同时传输数、保留属性、默认显示隐藏文件等其他设置不变。"
            }
            Self::ShowLatencyTitle => "在终端右上角显示延迟",
            Self::ShowLatencyDescription => {
                "SSH 标签显示到服务器的往返延迟（Telnet、串口不显示）。关掉后也不再发测量请求。"
            }
            Self::FontDisplayTitle => "字体与显示",
            Self::FontDisplayDescription => "已经打开的终端也会立刻跟着变。",
            Self::CustomOption => "自定义…",
            Self::FontFamilyTitle => "字体",
            Self::FontFamilyDescription => {
                "从本机已装的常见等宽字体里选；需要别的字体栈就选「自定义」自己填"
            }
            Self::FontSizeTitle => "字号",
            Self::FontSizeDescription => "10～24，也可以在终端里按 Ctrl+= / Ctrl+- / Ctrl+0",
            Self::LineHeightTitle => "行高",
            Self::LineHeightDescription => "1.0～1.6",
            Self::ScrollbackTitle => "回滚行数",
            Self::ScrollbackDescription => {
                "1000～100000，松手后对已开的和新开的终端都生效；调小会丢掉最旧的行"
            }
            Self::CursorTitle => "光标",
            Self::CursorBlinkTitle => "光标闪烁",
            Self::CursorBlinkDescription => "终端有焦点时光标一闪一闪；打字时先亮着。",
            Self::CursorShapeTitle => "光标样式",
            Self::CursorBlock => "方块",
            Self::CursorUnderline => "下划线",
            Self::CursorBar => "竖线",
            Self::InputTitle => "输入与剪贴板",
            Self::ImeTitle => "终端里自动切到英文输入",
            Self::ImeDescription => {
                "焦点进终端时关闭输入法（直接打英文），离开终端回到其他输入框（AI 对话框、搜索框、编辑器等）时重新打开；要在终端里打中文，按 Ctrl+空格 或点输入法图标打开。"
            }
            Self::CopyOnSelectTitle => "选中即复制",
            Self::CopyOnSelectDescription => "选中文字之后自动复制到剪贴板。",
            Self::RightClickPasteTitle => "右键粘贴",
            Self::RightClickPasteDescription => {
                "开着时右键直接粘贴（在链接上右键仍然弹出链接菜单）；关着时右键弹出菜单。"
            }
            Self::ConfirmMultilineTitle => "多行粘贴前确认",
            Self::ConfirmMultilineDescription => {
                "粘贴的内容带换行时先问一下：对方 shell 没开括号粘贴模式时，换行会让命令直接执行。"
            }
            Self::BracketedPasteTitle => "括号粘贴",
            Self::BracketedPasteDescription => "自动：跟随远端终端的设置；开/关：强制",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::Theme => "Theme & Skins",
            Self::Light => "Light",
            Self::Dark => "Dark",
            Self::FollowSystem => "Follow System",
            Self::AppearanceTitle => "App Appearance",
            Self::AppearanceDescription if !cfg!(windows) => {
                "When “Follow System” is selected, the interface follows the system as it switches between light and dark mode."
            }
            Self::AppearanceDescription => {
                "When “Follow System” is selected, the interface follows Windows as it switches between light and dark mode."
            }
            Self::SkinsDescription => {
                "Light and dark skins are saved separately and apply with the light/dark mode."
            }
            Self::LightSkin => "Light Skin",
            Self::DarkSkin => "Dark Skin",
            Self::TerminalColorsTitle => "Terminal Colors",
            Self::FollowSkinNowPrefix => "Follow skin (currently ",
            Self::OpenParen => " (",
            Self::CloseParen => ")",
            Self::SchemeTitle => "Color Scheme",
            Self::SchemeDescription => {
                "When following the skin, the terminal colors change with the skin"
            }
            Self::MismatchLead => "Terminal colors are fixed to ",
            Self::MismatchMid => ", but the app is currently in ",
            Self::MismatchEnd => " mode, so the terminal and the interface differ in light/dark.",
            Self::FollowSkinButton => "Follow Skin Again",
            Self::ResetPageButton => "Restore Page Defaults",
            Self::ResetRowTitle => "Restore Defaults",
            Self::ResetAppearanceScope => {
                "Restores only this page's theme, app skins, and terminal colors. Host tree options, language, terminal font, and all other settings stay as they are."
            }
            Self::ResetTerminalScope => {
                "Restores only this page's font & display, cursor, input & clipboard, bracketed paste, and highlight switch. Highlight rules, the session log, the theme, and settings on other pages stay as they are."
            }
            Self::ResetSftpScope => {
                "Restores only the File Panel settings on this page (open location, local pane, layout, default local folder, follow terminal directory). Concurrent transfers, preserved attributes, show hidden files and other settings stay as they are."
            }
            Self::ShowLatencyTitle => "Show latency in the top-right corner of the terminal",
            Self::ShowLatencyDescription => {
                "SSH tabs show the round-trip latency to the server (not shown for Telnet or serial). When off, no measurement requests are sent either."
            }
            Self::FontDisplayTitle => "Font & Display",
            Self::FontDisplayDescription => {
                "Terminals that are already open update immediately too."
            }
            Self::CustomOption => "Custom…",
            Self::FontFamilyTitle => "Font",
            Self::FontFamilyDescription => {
                "Choose from common monospace fonts installed on this computer; for another font stack, choose “Custom” and enter it yourself"
            }
            Self::FontSizeTitle => "Font Size",
            Self::FontSizeDescription => "10–24, or press Ctrl+= / Ctrl+- / Ctrl+0 in the terminal",
            Self::LineHeightTitle => "Line Height",
            Self::LineHeightDescription => "1.0–1.6",
            Self::ScrollbackTitle => "Scrollback Lines",
            Self::ScrollbackDescription => {
                "1000–100000; applies to open and new terminals when you release the slider; lowering it discards the oldest lines"
            }
            Self::CursorTitle => "Cursor",
            Self::CursorBlinkTitle => "Blinking cursor",
            Self::CursorBlinkDescription => {
                "The cursor blinks while the terminal has focus and stays lit while you type."
            }
            Self::CursorShapeTitle => "Cursor Style",
            Self::CursorBlock => "Block",
            Self::CursorUnderline => "Underline",
            Self::CursorBar => "Bar",
            Self::InputTitle => "Input & Clipboard",
            Self::ImeTitle => "Switch to English input automatically in the terminal",
            Self::ImeDescription => {
                "Turn the IME off when the terminal gains focus (so you type English directly) and back on when you leave the terminal for other input fields (AI chat box, search field, editor, and so on); to type Chinese in the terminal, press Ctrl+Space or turn the IME on from its icon."
            }
            Self::CopyOnSelectTitle => "Copy on select",
            Self::CopyOnSelectDescription => {
                "Copy the selected text to the clipboard automatically."
            }
            Self::RightClickPasteTitle => "Paste on right-click",
            Self::RightClickPasteDescription => {
                "When on, right-click pastes directly (right-clicking a link still opens the link menu); when off, right-click opens a menu."
            }
            Self::ConfirmMultilineTitle => "Confirm before pasting multiple lines",
            Self::ConfirmMultilineDescription => {
                "Ask first when the pasted text contains line breaks: when the remote shell has not enabled bracketed paste, a line break runs the command immediately."
            }
            Self::BracketedPasteTitle => "Bracketed paste",
            Self::BracketedPasteDescription => {
                "Auto: follow the remote terminal's setting; On/Off: force it"
            }
        }
    }
}

/// 明暗两色在句子里的说法（「深色」/「浅色」）。
fn mode_label(dark: bool) -> &'static str {
    if dark {
        t(SettingsViewMsg::Dark)
    } else {
        t(SettingsViewMsg::Light)
    }
}

/// 设置页里的一组（V2-U4 版式）：分组标题（可带一句说明）在卡片**外面**上方，卡片里一行一项，
/// 行与行之间一条细分隔线。每个 `rows` 元素占一行（通常是 [`row`] 做出来的「名称 + 说明 | 控件」）。
/// `id` 用来在驱动脚本里定位，也让每组有稳定的元素 ID。
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
        .gap_2p5()
        // 标题留空的组（只有一行的小块，比如「恢复默认」）不画标题。
        .when(!title.is_empty(), |d| {
            d.child(
                v_flex()
                    .px_1()
                    .gap_1()
                    .child(div().typo(Role::GroupTitle, tokens).child(title))
                    .when_some(description, |d, text| {
                        d.child(div().typo(Role::Caption, tokens).child(text))
                    }),
            )
        })
        .child(
            v_flex()
                .w_full()
                .rounded(::gpui::px(crate::app_theme::PANEL_RADIUS))
                .border_1()
                .border_color(tokens.border)
                .bg(tokens.background)
                .children(rows.into_iter().enumerate().map(|(index, content)| {
                    div()
                        .w_full()
                        .px_4()
                        .py_3()
                        .when(index > 0, |d| d.border_t_1().border_color(tokens.border))
                        .child(content)
                })),
        )
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
        .flex_wrap()
        .min_h(::gpui::px(28.0))
        .gap_3()
        .items_center()
        .justify_between()
        // 左边说明占剩下的宽度、写不下就换行；右边的控件不被它挤走。
        .child(
            v_flex()
                .flex_1()
                .min_w(::gpui::px(160.0))
                .gap_0p5()
                .child(div().typo(Role::ItemName, tokens).child(title))
                .when_some(description, |d, text| {
                    d.child(div().typo(Role::Caption, tokens).child(text))
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

/// 分段选择里的一项。
pub(crate) struct Segment {
    pub id: SharedString,
    pub label: &'static str,
    pub icon: Option<Icon>,
    pub selected: bool,
}

/// 一排互斥的选项（主题：浅色 / 深色 / 跟随系统；光标样式）：带小图标，选中项浅底加粗，
/// 不用整块高饱和主色。`on_pick` 收到被点的是第几项。
pub(crate) fn choice_group(
    segments: Vec<Segment>,
    tokens: &Tokens,
    on_pick: impl Fn(usize, &mut ::gpui::Window, &mut App) + 'static,
) -> AnyElement {
    let on_pick = std::rc::Rc::new(on_pick);
    let (hover, selected_bg, text, muted) =
        (tokens.hover, tokens.subtle, tokens.text, tokens.text_muted);
    h_flex()
        .gap_1()
        .children(segments.into_iter().enumerate().map(|(index, segment)| {
            let on_pick = on_pick.clone();
            let selected = segment.selected;
            h_flex()
                .id(segment.id)
                .gap_1p5()
                .items_center()
                .px_3()
                .py_1p5()
                .rounded_full()
                .cursor_pointer()
                .text_size(::gpui::px(14.0))
                .line_height(::gpui::relative(1.4))
                .when(selected, |d| {
                    d.bg(selected_bg)
                        .text_color(text)
                        .font_weight(FontWeight::SEMIBOLD)
                })
                .when(!selected, |d| {
                    d.text_color(muted)
                        .font_weight(FontWeight::NORMAL)
                        .hover(move |style| style.bg(hover))
                })
                .when_some(segment.icon, |d, icon| {
                    d.child(
                        icon.size_4()
                            .text_color(if selected { text } else { muted }),
                    )
                })
                .child(segment.label)
                .on_click(move |_, window, cx| on_pick(index, window, cx))
        }))
        .into_any_element()
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
        terminal_theme_card(ws, &s, &tokens),
    ]
}

/// 「恢复本页默认设置」那一小块的样子：一行，左边写清楚恢复哪些，右边是按钮。
fn reset_block(
    id: &'static str,
    button_id: &'static str,
    scope: &'static str,
    on_click: impl Fn(&mut ::gpui::Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let tokens = Tokens::current(cx);
    card(
        id,
        "",
        None,
        &tokens,
        vec![row(
            t(SettingsViewMsg::ResetRowTitle),
            Some(scope),
            Button::new(button_id)
                .label(t(SettingsViewMsg::ResetPageButton))
                .compact()
                .on_click(move |_, window, cx| on_click(window, cx)),
            &tokens,
        )],
    )
}

/// 外观页末尾的「恢复本页默认设置」：只还原外观页自己的字段（见 `UiSettings::reset_appearance_page`）。
pub(crate) fn reset_appearance_block(ws: &Entity<TerminalWorkspace>, cx: &App) -> AnyElement {
    let reset = ws.clone();
    reset_block(
        "settings-reset-appearance",
        "settings-reset-appearance-button",
        t(SettingsViewMsg::ResetAppearanceScope),
        move |_, cx| reset.update(cx, |this, cx| this.reset_appearance_settings(cx)),
        cx,
    )
}

/// 终端页末尾的「恢复本页默认设置」：只还原终端页自己的字段（见 `UiSettings::reset_terminal_page`）。
pub(crate) fn reset_terminal_block(ws: &Entity<TerminalWorkspace>, cx: &App) -> AnyElement {
    let reset = ws.clone();
    reset_block(
        "settings-reset-terminal",
        "settings-reset-terminal-button",
        t(SettingsViewMsg::ResetTerminalScope),
        move |window, cx| reset.update(cx, |this, cx| this.reset_terminal_settings(window, cx)),
        cx,
    )
}

/// 文件传输页末尾的「恢复本页默认设置」：只还原文件面板六项（见 `UiSettings::reset_sftp_page`）。
pub(crate) fn reset_sftp_block(ws: &Entity<TerminalWorkspace>, cx: &App) -> AnyElement {
    let reset = ws.clone();
    reset_block(
        "settings-reset-sftp",
        "settings-reset-sftp-button",
        t(SettingsViewMsg::ResetSftpScope),
        move |_, cx| reset.update(cx, |this, cx| this.reset_sftp_settings(cx)),
        cx,
    )
}

/// 主题与皮肤：明暗模式（系统 / 浅色 / 深色），下面是浅色、深色各一排皮肤。
#[inline(never)]
fn theme_card(ws: &Entity<TerminalWorkspace>, s: &UiSettings, tokens: &Tokens) -> AnyElement {
    card(
        "settings-theme",
        t(SettingsViewMsg::Theme),
        Some(t(SettingsViewMsg::SkinsDescription)),
        tokens,
        vec![
            theme_row(ws, s, tokens),
            skin_group(t(SettingsViewMsg::LightSkin), false, ws, s, tokens),
            skin_group(t(SettingsViewMsg::DarkSkin), true, ws, s, tokens),
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
    card(
        "settings-terminal-theme",
        t(SettingsViewMsg::TerminalColorsTitle),
        None,
        tokens,
        rows,
    )
}

#[inline(never)]
fn theme_row(ws: &Entity<TerminalWorkspace>, s: &UiSettings, tokens: &Tokens) -> AnyElement {
    let ws = ws.clone();
    let segments = vec![
        Segment {
            id: "theme-light".into(),
            label: t(SettingsViewMsg::Light),
            icon: Some(Icon::new(IconName::Sun)),
            selected: !s.follow_system && !s.dark,
        },
        Segment {
            id: "theme-dark".into(),
            label: t(SettingsViewMsg::Dark),
            icon: Some(Icon::new(IconName::Moon)),
            selected: !s.follow_system && s.dark,
        },
        Segment {
            id: "theme-auto".into(),
            label: t(SettingsViewMsg::FollowSystem),
            icon: Some(Icon::new(Ico::Monitor)),
            selected: s.follow_system,
        },
    ];
    row(
        t(SettingsViewMsg::AppearanceTitle),
        Some(t(SettingsViewMsg::AppearanceDescription)),
        choice_group(segments, tokens, move |index, _, cx| {
            ws.update(cx, |this, cx| match index {
                0 => this.set_theme(false, cx),
                1 => this.set_theme(true, cx),
                _ => this.set_theme_auto(cx),
            });
        }),
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
        .child(div().typo(Role::ItemName, tokens).child(title))
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
                // 真界面里侧栏和主区同一个底色，只隔一根淡线。
                .bg(color(ui.background))
                .border_r_1()
                .border_color(color(crate::app_theme::divider_hex(skin)))
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
        .rounded(::gpui::px(crate::app_theme::CARD_RADIUS))
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
                .typo(Role::Caption, tokens)
                .text_color(tokens.text)
                .child(crate::app_theme::skin_name(skin))
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
    let mut choices = vec![Choice::new(
        format!(
            "{}{}{}",
            t(SettingsViewMsg::FollowSkinNowPrefix),
            crate::app_theme::skin_name(current_skin),
            t(SettingsViewMsg::CloseParen)
        ),
        following,
    )];
    let mut ids: Vec<&'static str> = vec![TERMINAL_FOLLOW_SKIN];
    for skin in SKINS {
        choices.push(Choice::new(
            format!(
                "{}{}{}{}",
                crate::app_theme::skin_name(skin),
                t(SettingsViewMsg::OpenParen),
                mode_label(skin.dark),
                t(SettingsViewMsg::CloseParen)
            ),
            !following && skin.id == s.terminal_theme,
        ));
        ids.push(skin.id);
    }
    let label = if following {
        format!(
            "{}{}{}",
            t(SettingsViewMsg::FollowSkinNowPrefix),
            crate::app_theme::skin_name(current_skin),
            t(SettingsViewMsg::CloseParen)
        )
    } else {
        crate::app_theme::skin(&s.terminal_theme).map_or_else(
            || s.terminal_theme.clone(),
            |skin| {
                format!(
                    "{}{}{}{}",
                    crate::app_theme::skin_name(skin),
                    t(SettingsViewMsg::OpenParen),
                    mode_label(skin.dark),
                    t(SettingsViewMsg::CloseParen)
                )
            },
        )
    };
    let ws = ws.clone();
    row(
        t(SettingsViewMsg::SchemeTitle),
        Some(t(SettingsViewMsg::SchemeDescription)),
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
            .text_color(tokens.caution)
            .child(format!(
                "{}{}{}{}{}{}{}{}",
                t(SettingsViewMsg::MismatchLead),
                crate::app_theme::skin_name(fixed),
                t(SettingsViewMsg::OpenParen),
                mode_label(fixed.dark),
                t(SettingsViewMsg::CloseParen),
                t(SettingsViewMsg::MismatchMid),
                mode_label(tokens.dark),
                t(SettingsViewMsg::MismatchEnd),
            ))
            .child(
                Button::new("terminal-follow-skin")
                    .label(t(SettingsViewMsg::FollowSkinButton))
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
    terminal_sections(ws, &s, &tokens, cx)
}

/// 终端分类的三组：字体与显示（字体、字号、行高、回滚行数）、光标、输入与剪贴板。
/// 已经打开的终端也会立刻跟着变。
#[inline(never)]
fn terminal_sections(
    ws: &Entity<TerminalWorkspace>,
    s: &UiSettings,
    tokens: &Tokens,
    cx: &App,
) -> Vec<AnyElement> {
    vec![
        display_section(ws, s, tokens, cx),
        cursor_section(ws, s, tokens),
        input_section(ws, s, tokens),
    ]
}

/// 字体与显示：字体、字号、行高、回滚行数。
#[inline(never)]
fn display_section(
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
        switch_row(
            "show-latency",
            t(SettingsViewMsg::ShowLatencyTitle),
            t(SettingsViewMsg::ShowLatencyDescription),
            s.show_latency,
            |this, cx| this.toggle_show_latency(cx),
            ws,
            tokens,
        ),
    ]);
    card(
        "settings-terminal",
        t(SettingsViewMsg::FontDisplayTitle),
        Some(t(SettingsViewMsg::FontDisplayDescription)),
        tokens,
        rows,
    )
}

/// 光标：样式、闪烁。
#[inline(never)]
fn cursor_section(ws: &Entity<TerminalWorkspace>, s: &UiSettings, tokens: &Tokens) -> AnyElement {
    card(
        "settings-terminal-cursor",
        t(SettingsViewMsg::CursorTitle),
        None,
        tokens,
        vec![
            cursor_row(ws, s, tokens),
            switch_row(
                "cursor-blink",
                t(SettingsViewMsg::CursorBlinkTitle),
                t(SettingsViewMsg::CursorBlinkDescription),
                s.cursor_blink,
                |this, cx| this.toggle_cursor_blink(cx),
                ws,
                tokens,
            ),
        ],
    )
}

/// 输入与剪贴板：输入法、选中即复制、右键粘贴、多行粘贴确认、括号粘贴。
#[inline(never)]
fn input_section(ws: &Entity<TerminalWorkspace>, s: &UiSettings, tokens: &Tokens) -> AnyElement {
    let rows = vec![
        switch_row(
            "ime-english",
            t(SettingsViewMsg::ImeTitle),
            t(SettingsViewMsg::ImeDescription),
            s.ime_english,
            |this, cx| this.toggle_ime_english(cx),
            ws,
            tokens,
        ),
        switch_row(
            "copy-on-select",
            t(SettingsViewMsg::CopyOnSelectTitle),
            t(SettingsViewMsg::CopyOnSelectDescription),
            s.copy_on_select,
            |this, cx| this.toggle_copy_on_select(cx),
            ws,
            tokens,
        ),
        switch_row(
            "right-click-paste",
            t(SettingsViewMsg::RightClickPasteTitle),
            t(SettingsViewMsg::RightClickPasteDescription),
            s.right_click_paste,
            |this, cx| this.toggle_right_click_paste(cx),
            ws,
            tokens,
        ),
        switch_row(
            "confirm-multiline",
            t(SettingsViewMsg::ConfirmMultilineTitle),
            t(SettingsViewMsg::ConfirmMultilineDescription),
            s.confirm_multiline,
            |this, cx| this.toggle_confirm_multiline(cx),
            ws,
            tokens,
        ),
        bracketed_paste_row(ws, s, tokens),
    ];
    card(
        "settings-terminal-input",
        t(SettingsViewMsg::InputTitle),
        None,
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
    choices.push(Choice::new(t(SettingsViewMsg::CustomOption), custom));
    let label = if custom {
        t(SettingsViewMsg::CustomOption).to_owned()
    } else {
        leading
    };
    let ws = ws.clone();
    row(
        t(SettingsViewMsg::FontFamilyTitle),
        Some(t(SettingsViewMsg::FontFamilyDescription)),
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
        t(SettingsViewMsg::FontSizeTitle),
        Some(t(SettingsViewMsg::FontSizeDescription)),
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
        t(SettingsViewMsg::LineHeightTitle),
        Some(t(SettingsViewMsg::LineHeightDescription)),
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
        t(SettingsViewMsg::ScrollbackTitle),
        Some(t(SettingsViewMsg::ScrollbackDescription)),
        h_flex()
            .gap_3()
            .items_center()
            .child(
                div().w(::gpui::px(150.0)).child(
                    Slider::new(&slider)
                        .horizontal()
                        .accessibility_label(t(SettingsViewMsg::ScrollbackTitle)),
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
    const SHAPES: [(&str, SettingsViewMsg, CursorShape); 3] = [
        (
            "cursor-block",
            SettingsViewMsg::CursorBlock,
            CursorShape::Block,
        ),
        (
            "cursor-underline",
            SettingsViewMsg::CursorUnderline,
            CursorShape::Underline,
        ),
        ("cursor-bar", SettingsViewMsg::CursorBar, CursorShape::Bar),
    ];
    let segments = SHAPES
        .iter()
        .map(|(id, label, shape)| Segment {
            id: (*id).into(),
            label: t(*label),
            icon: None,
            selected: s.cursor == *shape,
        })
        .collect();
    let ws = ws.clone();
    row(
        t(SettingsViewMsg::CursorShapeTitle),
        None,
        choice_group(segments, tokens, move |index, _, cx| {
            if let Some((_, _, shape)) = SHAPES.get(index) {
                ws.update(cx, |this, cx| this.set_cursor_shape(*shape, cx));
            }
        }),
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
        t(SettingsViewMsg::BracketedPasteTitle),
        Some(t(SettingsViewMsg::BracketedPasteDescription)),
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
#[cfg(debug_assertions)]
pub(crate) fn settings_body(ws: &Entity<TerminalWorkspace>, cx: &App) -> impl IntoElement {
    let mut cards = appearance_cards(ws, cx);
    cards.push(reset_appearance_block(ws, cx));
    cards.extend(terminal_cards(ws, cx));
    cards.push(reset_terminal_block(ws, cx));
    v_flex().w_full().gap_3().children(cards)
}
