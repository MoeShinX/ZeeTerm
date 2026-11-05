//! 右键 / 弹出菜单的公共菜单项（V2-U14）：所有菜单的一行都从这里造，图标必填、尺寸和间距统一，
//! 置灰 / 危险的颜色只在一处定。
//!
//! 组件库的 `PopupMenu` 把行内图标写死成 12px（`Icon::xsmall`），而且行高、内边距都不开放。能改的只有
//! 图标自己的样式：`Icon` 的 `style` 里设最小宽高 16px，布局盒子就比 12px 大，SVG 按盒子大小重新栅格化
//! （不是放大位图，线条仍然清楚）；再给图标一点右外边距，把文字推到统一的位置。所以：
//! - 菜单里有图标的行，图标 16px、文字从同一列开始（图标 + 6px 间距）；
//! - 普通行直接用组件库的 `Item`（键盘导航、读屏标签都保留）；置灰 / 危险 / 带右侧说明的行用 `ElementItem`
//!   自己画文字，颜色取皮肤的 token；
//! - 子菜单行走组件库的 `Submenu`，图标同样的样式，箭头在最右边。
//!
//! 文字 13px（`Role::Menu`，V2-U17 起比正文小一号）：组件库给 `Item` / `Submenu` 的文字写死了 14px（`text_sm`），
//! 外面改不了，所以所有行（含勾选列表）都做成 `ElementItem`、自己画 13px 的文字；只有子菜单行的名字（组件库只收
//! 一段字符串）仍是 14px。
//!
//! 勾选 / 单选下拉（只有名字和勾选的列表）用 [`choice`]，勾选标记在最左一列。

use std::rc::Rc;

use ::gpui::{App, ClickEvent, Div, Hsla, SharedString, Window, px, relative};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::*;

use crate::app_theme::Tokens;
use crate::typography::{Role, Typo as _};

/// 菜单图标边长（逻辑像素）：配 13px 的文字，16px 显得大，取 15。
pub const ICON_SIZE: f32 = 15.0;
/// 图标右边到文字的空隙（组件库另外还有 4px 的列间距）。
pub const ICON_GAP: f32 = 4.0;

/// 一行的状态：决定文字和图标用哪个颜色。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowState {
    Normal,
    /// 删除、断开连接这类危险操作：文字和图标都用危险色。
    Danger,
    /// 置灰：文字和图标一起变淡；置灰压过危险（不能点的"断开连接"不红）。
    Disabled,
}

/// 一行用到的三种颜色。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowColors {
    pub text: Hsla,
    pub icon: Hsla,
    /// 右侧的快捷键 / 置灰原因。
    pub right: Hsla,
}

/// 这个状态在当前皮肤下的颜色。
pub fn row_colors(state: RowState, tokens: &Tokens) -> RowColors {
    match state {
        RowState::Normal => RowColors {
            text: tokens.text,
            icon: tokens.text,
            right: tokens.text_muted,
        },
        RowState::Danger => RowColors {
            text: tokens.danger,
            icon: tokens.danger,
            right: tokens.text_muted,
        },
        RowState::Disabled => RowColors {
            text: tokens.text_faint,
            icon: tokens.text_faint,
            right: tokens.text_muted,
        },
    }
}

/// 菜单里的图标：16px，右边留出到文字的空隙；`color` 为空时跟着行的文字色走。
pub fn menu_icon(icon: impl Into<Icon>, color: Option<Hsla>) -> Icon {
    let icon = icon
        .into()
        .min_w(px(ICON_SIZE))
        .min_h(px(ICON_SIZE))
        .mr(px(ICON_GAP));
    match color {
        Some(color) => icon.text_color(color),
        None => icon,
    }
}

type Handler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// 一行菜单。图标是必填的（没有合适语义的也要给一个，保证左侧图标列对齐）。
pub struct MenuRow {
    tokens: Tokens,
    label: SharedString,
    icon: Icon,
    state: RowState,
    right: Option<SharedString>,
    tooltip: Option<SharedString>,
}

/// 造一行：名字 + 图标。
pub fn row(tokens: &Tokens, label: impl Into<SharedString>, icon: impl Into<Icon>) -> MenuRow {
    MenuRow {
        tokens: *tokens,
        label: label.into(),
        icon: icon.into(),
        state: RowState::Normal,
        right: None,
        tooltip: None,
    }
}

impl MenuRow {
    /// 危险操作（已置灰的行不受影响，仍是灰色）。
    pub fn danger(mut self) -> Self {
        if self.state == RowState::Normal {
            self.state = RowState::Danger;
        }
        self
    }

    /// `true` 时置灰（不能点）。
    pub fn disabled(mut self, disabled: bool) -> Self {
        if disabled {
            self.state = RowState::Disabled;
        }
        self
    }

    /// 右边的小字：快捷键，或者置灰的原因。
    pub fn right(mut self, text: Option<impl Into<SharedString>>) -> Self {
        self.right = text.map(Into::into);
        self
    }

    /// 悬停说明（置灰时说明原因最有用）。
    pub fn tooltip(mut self, text: Option<impl Into<SharedString>>) -> Self {
        self.tooltip = text.map(Into::into);
        self
    }

    /// 当前状态（测试用）。
    #[cfg(test)]
    pub fn state(&self) -> RowState {
        self.state
    }

    /// 做成可点的一项。置灰的行不会收到点击，处理函数传进来也不会被调用。
    pub fn on_click(
        self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> PopupMenuItem {
        self.build(Some(Rc::new(handler)))
    }

    /// 没有动作的一项（置灰的说明行）。
    pub fn inert(self) -> PopupMenuItem {
        self.build(None)
    }

    fn build(self, handler: Option<Handler>) -> PopupMenuItem {
        let disabled = self.state == RowState::Disabled;
        let Self {
            tokens,
            label,
            icon,
            state,
            right,
            tooltip,
        } = self;
        let id = label.clone();
        let colors = row_colors(state, &tokens);
        // 普通行不写文字色：悬停 / 键盘选中时组件库给整行换高亮色，文字要跟着变，写死了就压住它。
        let colour = (state != RowState::Normal).then_some(colors.text);
        let item = PopupMenuItem::element(move |_window, _cx| {
            let text = menu_text(colour).child(label.clone());
            let content = h_flex()
                .id(id.clone())
                .w_full()
                .gap_6()
                .items_center()
                .justify_between()
                .child(text)
                .children(right.clone().map(|right| {
                    div()
                        .typo(Role::Caption, &tokens)
                        .text_color(colors.right)
                        .child(right)
                }));
            match tooltip.clone() {
                Some(hint) => content
                    .tooltip(move |window, cx| Tooltip::new(hint.clone()).build(window, cx))
                    .into_any_element(),
                None => content.into_any_element(),
            }
        })
        .icon(menu_icon(
            icon,
            (state != RowState::Normal).then_some(colors.icon),
        ))
        .disabled(disabled);
        match handler {
            Some(handler) if !disabled => {
                item.on_click(move |event, window, cx| handler(event, window, cx))
            }
            _ => item,
        }
    }
}

/// 菜单里的一段文字：13px（`Role::Menu`）。`colour` 为空时继承（跟着悬停高亮变色）。
fn menu_text(colour: Option<Hsla>) -> Div {
    let spec = Role::Menu.spec();
    let text = div()
        .text_size(px(spec.size))
        .font_weight(spec.weight)
        .line_height(relative(spec.leading));
    match colour {
        Some(colour) => text.text_color(colour),
        None => text,
    }
}

/// 勾选 / 单选列表里的一项（没有图标，勾在最左一列）：文字同样 13px。
/// 要置灰、点击处理，接着在返回值上链 `.disabled(..)` / `.on_click(..)`。
pub fn choice(label: impl Into<SharedString>, checked: bool) -> PopupMenuItem {
    let label: SharedString = label.into();
    PopupMenuItem::element(move |_window, _cx| menu_text(None).child(label.clone()))
        .checked(checked)
}

/// 子菜单行：图标和文字的位置与普通行一致，箭头在最右边。
pub fn submenu(
    menu: PopupMenu,
    label: impl Into<SharedString>,
    icon: impl Into<Icon>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
    f: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
) -> PopupMenu {
    menu.submenu_with_icon(Some(menu_icon(icon, None)), label, window, cx, f)
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{ICON_GAP, ICON_SIZE, RowState, row, row_colors};
    use crate::app_theme::{SKINS, Tokens};
    use crate::icons::Ico;
    use crate::typography::Role;

    #[test]
    fn a_row_starts_normal_and_danger_or_disabled_change_the_state() {
        let t = Tokens::for_mode(false);
        assert_eq!(row(&t, "复制", Ico::Copy).state(), RowState::Normal);
        assert_eq!(
            row(&t, "删除", Ico::Trash).danger().state(),
            RowState::Danger
        );
        assert_eq!(
            row(&t, "上移", Ico::ArrowUpDown).disabled(true).state(),
            RowState::Disabled
        );
        assert_eq!(
            row(&t, "上移", Ico::ArrowUpDown).disabled(false).state(),
            RowState::Normal
        );
    }

    #[test]
    fn disabled_beats_danger_in_either_order() {
        let t = Tokens::for_mode(true);
        assert_eq!(
            row(&t, "断开连接", Ico::Unplug)
                .danger()
                .disabled(true)
                .state(),
            RowState::Disabled
        );
        assert_eq!(
            row(&t, "断开连接", Ico::Unplug)
                .disabled(true)
                .danger()
                .state(),
            RowState::Disabled
        );
    }

    #[test]
    fn danger_colours_text_and_icon_and_disabled_dims_both_on_every_skin() {
        for skin in SKINS {
            let t = Tokens::for_skin(skin);
            let normal = row_colors(RowState::Normal, &t);
            let danger = row_colors(RowState::Danger, &t);
            let off = row_colors(RowState::Disabled, &t);
            assert_eq!(normal.text, t.text, "{} 普通项用主文字色", skin.id);
            assert_eq!(normal.icon, normal.text, "{} 图标和文字同色", skin.id);
            assert_eq!(danger.text, t.danger, "{} 危险项文字", skin.id);
            assert_eq!(danger.icon, t.danger, "{} 危险项图标", skin.id);
            assert_eq!(off.text, t.text_faint, "{} 置灰文字", skin.id);
            assert_eq!(off.icon, off.text, "{} 置灰时图标同步变淡", skin.id);
            assert_ne!(off.text, normal.text);
            assert_ne!(off.text, danger.text);
        }
    }

    #[test]
    fn the_icon_box_fits_the_thirteen_pixel_menu_text() {
        // 文字 13px，图标 15px（比文字略大一点才协调，不超过 16）；图标到文字的空隙不能小于 4px。
        assert_eq!(ICON_SIZE, 15.0);
        const { assert!(ICON_GAP >= 4.0) };
        assert!(ICON_SIZE > Role::Menu.spec().size && ICON_SIZE <= 16.0);
    }

    #[test]
    fn menu_text_is_one_step_smaller_than_body_and_not_smaller_than_captions() {
        let menu = Role::Menu.spec();
        assert_eq!(menu.size, 13.0);
        assert!(menu.size < Role::Body.spec().size);
        assert!(menu.size > Role::Caption.spec().size);
    }

    /// 递归读 `src` 下所有 .rs（调试驱动 `dev_acts*` 除外）。
    fn source_files() -> Vec<(String, String)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let name = path.to_string_lossy().replace('\\', "/");
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        out.push((name, text));
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut out,
        );
        out
    }

    #[test]
    fn action_menus_are_built_with_the_shared_row_not_by_hand() {
        // 动作菜单走 `menu_kit::row`、勾选 / 单选列表走 `menu_kit::choice`：组件库的 `Item` 文字写死 14px，
        // 手写 `PopupMenuItem::new` 会让这一行比别的行大一号（V2-U17）。
        let allowed = ["menu_kit.rs"];
        for (name, text) in source_files() {
            if text.contains("PopupMenuItem::new(") {
                assert!(
                    allowed.iter().any(|a| name.ends_with(a)),
                    "{name} 里手写了 PopupMenuItem::new，请用 menu_kit::row（动作）或 menu_kit::choice（勾选列表）"
                );
            }
        }
    }

    /// 一个文件（测试模块之前的部分）里写死颜色字面量的次数。
    fn hardcoded_colours(text: &str) -> usize {
        let body = text.split("#[cfg(test)]").next().unwrap_or(text);
        let mut count = 0;
        for needle in ["rgb(0x", "rgba(0x", "hsla(", "tint(0x"] {
            for (at, _) in body.match_indices(needle) {
                // `color_to_hsla(` 这类函数名不算：前一个字符是字母 / 数字 / 下划线就跳过。
                let before = body[..at].chars().next_back();
                if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                count += 1;
            }
        }
        count
    }

    #[test]
    fn colours_come_from_the_skin_tokens_except_a_short_reviewed_list() {
        // 皮肤色板只在 app_theme.rs；其余文件写死颜色要记在这里，新增的会让测试失败。
        // 都是不随皮肤变的语义色：遮罩黑、状态徽标上的深色字、传输 / 同步里的状态标记、快捷命令广播的琥珀底。
        let allowed: &[(&str, usize)] = &[
            ("main_window/titlebar.rs", 1),
            ("main_window/host_dialog.rs", 1),
            ("main_window/widgets.rs", 1),
            ("main_window/processes_ui.rs", 1),
            ("main_window/bars.rs", 2),
            ("sftp/view.rs", 4),
            ("sftp/view_dialogs.rs", 1),
            ("sync/view.rs", 4),
        ];
        for (name, text) in source_files() {
            if name.ends_with("app_theme.rs") || name.contains("/dev_") {
                continue;
            }
            let found = hardcoded_colours(&text);
            let ok = allowed
                .iter()
                .find(|(suffix, _)| name.ends_with(suffix))
                .map_or(0, |(_, n)| *n);
            assert!(
                found <= ok,
                "{name} 里有 {found} 处写死的颜色（允许 {ok}）：请改用 Tokens，确实需要就记进名单"
            );
        }
    }
}
