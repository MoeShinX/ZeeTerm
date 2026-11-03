//! 界面排版：几档「字号 + 字重 + 颜色 + 行高」的组合，整个界面统一从这里取，不在各处自己拼
//! `text_sm().font_weight(..)`（V2-U4）。
//!
//! 层级靠三样东西拉开：字重（标题 Bold / Semibold，名称 Medium，说明 Regular）、颜色（主 / 次 / 淡三级，
//! 取值见 `app_theme::ink_for`）、字号（24 / 15 / 14 / 12）。终端、代码、地址这些等宽字体不在这里管。
//!
//! 用法：`div().typo(Role::ItemName, &tokens).child("名称")`。

use ::gpui::{FontWeight, Hsla, Styled, px, relative};

use crate::app_theme::Tokens;

/// 文字角色：每个角色对应一档固定的排版（见 [`Role::spec`]）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// 页面大标题（「凭据」「外观」）。
    PageTitle,
    /// 分组标题（「主题」「应用皮肤」），放在卡片外面上方。
    GroupTitle,
    /// 设置项 / 列表项 / 卡片的名称。
    ItemName,
    /// 表单字段的标签（面板和对话框里输入框上面的那行字）：比设置项名称小一号，字重也轻一档。
    FieldLabel,
    /// 说明文字：名称下面的一行小字、提示、数量。
    Caption,
    /// 一般正文（对话框里的句子、表单里的输入内容）。
    Body,
    /// 侧栏主机树里的行（分组名、主机名）：14px（V2-U14 起和导航项同号），行高固定 30px，见 `sidebar::ROW_HEIGHT`。
    Tree,
    /// 导航项（未选中）。
    Nav,
    /// 导航项（选中）。
    NavSelected,
    /// 导航分组名（「管理」「偏好」）。
    NavGroup,
}

/// 三级文字色（和 `Tokens` 里的 `text` / `text_muted` / `text_faint` 对应），导航项的「略淡」单独一档。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Primary,
    Secondary,
    Faint,
    /// 介于主色和次色之间：未选中的导航项。
    Nav,
}

/// 一档排版。字号单位是逻辑像素，行高是字号的倍数。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spec {
    pub size: f32,
    pub weight: FontWeight,
    pub tone: Tone,
    pub leading: f32,
}

impl Role {
    #[cfg(test)]
    pub const ALL: [Role; 10] = [
        Role::PageTitle,
        Role::GroupTitle,
        Role::ItemName,
        Role::FieldLabel,
        Role::Caption,
        Role::Body,
        Role::Tree,
        Role::Nav,
        Role::NavSelected,
        Role::NavGroup,
    ];

    /// 这个角色的排版取值。改这里就是改整个界面的字号字重。
    pub const fn spec(self) -> Spec {
        let (size, weight, tone, leading) = match self {
            Role::PageTitle => (24.0, FontWeight::BOLD, Tone::Primary, 1.3),
            Role::GroupTitle => (15.0, FontWeight::SEMIBOLD, Tone::Primary, 1.4),
            Role::ItemName => (14.0, FontWeight::MEDIUM, Tone::Primary, 1.45),
            Role::FieldLabel => (13.0, FontWeight::MEDIUM, Tone::Primary, 1.45),
            Role::Caption => (12.0, FontWeight::NORMAL, Tone::Secondary, 1.5),
            Role::Body => (14.0, FontWeight::NORMAL, Tone::Primary, 1.5),
            Role::Tree => (14.0, FontWeight::NORMAL, Tone::Primary, 1.3),
            Role::Nav => (14.0, FontWeight::NORMAL, Tone::Nav, 1.4),
            Role::NavSelected => (14.0, FontWeight::MEDIUM, Tone::Primary, 1.4),
            Role::NavGroup => (12.0, FontWeight::NORMAL, Tone::Faint, 1.4),
        };
        Spec {
            size,
            weight,
            tone,
            leading,
        }
    }

    /// 这个角色在当前皮肤下的文字色。
    pub fn color(self, tokens: &Tokens) -> Hsla {
        tone_color(self.spec().tone, tokens)
    }
}

/// 一档颜色在当前皮肤下的取值。
pub fn tone_color(tone: Tone, tokens: &Tokens) -> Hsla {
    match tone {
        Tone::Primary => tokens.text,
        Tone::Secondary => tokens.text_muted,
        Tone::Faint => tokens.text_faint,
        Tone::Nav => tokens.text_nav,
    }
}

/// 给任何带样式的元素套一档排版。
pub trait Typo: Styled + Sized {
    fn typo(self, role: Role, tokens: &Tokens) -> Self {
        let spec = role.spec();
        self.text_size(px(spec.size))
            .font_weight(spec.weight)
            .line_height(relative(spec.leading))
            .text_color(role.color(tokens))
    }
}

impl<T: Styled + Sized> Typo for T {}

/// 给元素套上中文后备字体（见 `fonts::resolve_ui_font`）：主字体里没有的字，先去找雅黑 UI，
/// 而不是走系统的回退表。`Root` 只替整个窗口设了主字体族名，后备要每个窗口的顶层元素自己加一次。
pub trait UiFontStyled: Styled + Sized {
    fn ui_font_fallbacks(mut self, cx: &::gpui::App) -> Self {
        let font = crate::fonts::ui_font(cx);
        if !font.fallbacks.is_empty() {
            self.text_style().font_fallbacks =
                Some(::gpui::FontFallbacks::from_fonts(font.fallbacks.clone()));
        }
        self
    }
}

impl<T: Styled + Sized> UiFontStyled for T {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_theme::{SKINS, Tokens};

    #[test]
    fn the_scale_is_what_the_card_asked_for() {
        let spec = |role: Role| role.spec();
        assert_eq!(spec(Role::PageTitle).size, 24.0);
        assert_eq!(spec(Role::PageTitle).weight, FontWeight::BOLD);
        assert_eq!(spec(Role::GroupTitle).size, 15.0);
        assert_eq!(spec(Role::GroupTitle).weight, FontWeight::SEMIBOLD);
        assert_eq!(spec(Role::ItemName).size, 14.0);
        assert_eq!(spec(Role::ItemName).weight, FontWeight::MEDIUM);
        assert_eq!(spec(Role::Caption).size, 12.0);
        assert_eq!(spec(Role::Caption).weight, FontWeight::NORMAL);
        assert_eq!(spec(Role::Nav).size, 14.0);
        assert_eq!(spec(Role::Nav).weight, FontWeight::NORMAL);
        assert_eq!(spec(Role::NavSelected).weight, FontWeight::MEDIUM);
        assert_eq!(spec(Role::NavGroup).size, 12.0);
    }

    #[test]
    fn hierarchy_goes_bigger_and_bolder_towards_the_top() {
        let weight = |role: Role| role.spec().weight.0;
        let size = |role: Role| role.spec().size;
        // 标题 > 分组标题 > 名称 > 说明：字号不增，字重不减。
        assert!(size(Role::PageTitle) > size(Role::GroupTitle));
        assert!(size(Role::GroupTitle) > size(Role::ItemName));
        assert!(size(Role::ItemName) > size(Role::Caption));
        assert!(weight(Role::PageTitle) > weight(Role::GroupTitle));
        assert!(weight(Role::GroupTitle) >= weight(Role::ItemName));
        assert!(weight(Role::ItemName) > weight(Role::Caption));
        // 说明比名称小、轻、淡：三个维度都拉开。
        assert_ne!(Role::ItemName.spec().tone, Role::Caption.spec().tone);
    }

    #[test]
    fn names_use_the_primary_colour_and_captions_a_lighter_one() {
        for skin in SKINS {
            let t = Tokens::for_skin(skin);
            assert_eq!(Role::PageTitle.color(&t), t.text);
            assert_eq!(Role::GroupTitle.color(&t), t.text);
            assert_eq!(Role::ItemName.color(&t), t.text);
            assert_eq!(Role::Caption.color(&t), t.text_muted);
            assert_eq!(Role::NavSelected.color(&t), t.text);
            assert_eq!(Role::NavGroup.color(&t), t.text_faint);
            assert_ne!(
                Role::Nav.color(&t),
                t.text,
                "{} 未选中导航要比选中淡",
                skin.id
            );
        }
    }

    #[test]
    fn every_role_has_a_sane_line_height() {
        for role in Role::ALL {
            let spec = role.spec();
            assert!(
                (1.2..=1.6).contains(&spec.leading),
                "{role:?} 行高 {} 不合理",
                spec.leading
            );
            assert!(spec.size >= 12.0, "{role:?} 字号不能小于 12");
        }
    }
}
