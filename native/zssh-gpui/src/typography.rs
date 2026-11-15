//! 界面排版：几档「字号 + 字重 + 颜色 + 行高」的组合，整个界面统一从这里取，不在各处自己拼
//! `text_sm().font_weight(..)`（V2-U4）。
//!
//! 层级靠三样东西拉开：字重（标题 Bold / Semibold，名称 Medium，说明 Regular）、颜色（主 / 次 / 淡三级，
//! 取值见 `app_theme::ink_for`）、字号（24 / 15 / 14 / 12）。终端、代码、地址这些等宽字体不在这里管。
//!
//! 用法：`div().typo(Role::ItemName, &tokens).child("名称")`。
//!
//! 界面字号（「设置 → 外观 → 界面字号」小 / 标准 / 大）也集中在这里：[`UiTextSize`] 定义三档的缩放系数，
//! 其他代码一律从这里取，不各处写死。缩放有两条路：角色字号和 `text_size(px(..))` 字面值乘系数
//! （[`ui_text_px`]、[`Typo::typo`]），GPUI 的 rem 字号和间距靠窗口的 rem 基准（16px × 系数，
//! [`UiTextSize::rem_px`]）——主窗口由组件库的 `Root` 按 `Theme.font_size` 设，没有 `Root` 的窗口在各自
//! 根视图渲染时调 [`sync_window_rem`]。

use std::cell::Cell;

use ::gpui::{FontWeight, Hsla, Pixels, Styled, Window, px, relative};
use serde::{Deserialize, Serialize};

use crate::app_theme::Tokens;

/// 界面字号三档。只管界面文字；终端字号在「设置 → 终端」里单独设置，不受它影响。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UiTextSize {
    Small,
    /// 和引入这个设置之前的界面完全一样（系数 1.0）。
    #[default]
    Normal,
    Large,
}

/// GPUI 默认的 rem 基准（`Window` 默认值、组件库 `Theme.font_size` 默认值都是它）。
pub const BASE_REM_PX: f32 = 16.0;

impl UiTextSize {
    #[cfg(test)]
    pub const ALL: [UiTextSize; 3] = [UiTextSize::Small, UiTextSize::Normal, UiTextSize::Large];

    /// 缩放系数：小 0.875、标准 1.0、大 1.125（都是二进制小数，乘出来没有舍入误差）。
    pub const fn scale(self) -> f32 {
        match self {
            Self::Small => 0.875,
            Self::Normal => 1.0,
            Self::Large => 1.125,
        }
    }

    /// 这一档的 rem 基准（逻辑像素）：`text_sm()`、`px_2()`、`gap_1()` 这类 rem 单位都以它为准。
    pub const fn rem_px(self) -> f32 {
        BASE_REM_PX * self.scale()
    }
}

thread_local! {
    /// 当前生效的界面字号。排版函数（`typo`、`ui_text_px`）拿不到 `cx`，所以放在这里；
    /// 界面只在 UI 线程上渲染，设置变化也在 UI 线程上落到这里（`settings_store`），所以不会有读写竞争。
    /// 用线程局部而不是全局原子量：并行跑的测试各在自己的线程里，互不串档。
    static CURRENT: Cell<UiTextSize> = const { Cell::new(UiTextSize::Normal) };
}

/// 设置变了（或启动读到设置）时调用，只在 UI 线程上。
pub fn set_ui_text_size(size: UiTextSize) {
    CURRENT.with(|current| current.set(size));
}

/// 当前生效的界面字号档位。
pub fn ui_text_size() -> UiTextSize {
    CURRENT.with(Cell::get)
}

/// 界面文字的字号字面值（px）换算成当前档位。属于界面文字、又不走 [`Role`] 的 `text_size(px(..))`
/// 都经过它，不要各处手乘；终端字符格、绘图、按终端格子算出来的尺寸不要用它。
pub fn ui_text_px(base: f32) -> Pixels {
    px(base * ui_text_size().scale())
}

/// 把窗口的 rem 基准对齐到当前档位。没有组件库 `Root` 的窗口（独立终端窗口、文件窗口、更新窗口……）
/// 在根视图 `render` 开头调一次；`Root` 管的主窗口由它自己按 `Theme.font_size` 设，两边是同一个值。
pub fn sync_window_rem(window: &mut Window) {
    window.set_rem_size(px(ui_text_size().rem_px()));
}

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
    /// 右键 / 下拉菜单里一行的文字（V2-U17）：比正文小一号（13px），菜单更紧凑；颜色不在这里定，
    /// 菜单行要跟着悬停高亮变色，由行自己决定（见 `menu_kit`）。
    Menu,
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
    pub const ALL: [Role; 11] = [
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
        Role::Menu,
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
            Role::Menu => (13.0, FontWeight::NORMAL, Tone::Primary, 1.4),
        };
        Spec {
            size,
            weight,
            tone,
            leading,
        }
    }

    /// 这个角色在指定档位下的字号（px）。
    pub fn size_at(self, level: UiTextSize) -> f32 {
        self.spec().size * level.scale()
    }

    /// 这个角色在当前档位下的字号（px）。
    pub fn size(self) -> f32 {
        self.size_at(ui_text_size())
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
        self.text_size(px(role.size()))
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
        assert_eq!(spec(Role::Menu).size, 13.0);
        assert_eq!(spec(Role::Menu).weight, FontWeight::NORMAL);
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

    /// 测试里临时换档，离开作用域（含 panic）时还原，别的断言不受影响。
    struct LevelGuard(UiTextSize);

    impl LevelGuard {
        fn set(level: UiTextSize) -> Self {
            let previous = ui_text_size();
            set_ui_text_size(level);
            Self(previous)
        }
    }

    impl Drop for LevelGuard {
        fn drop(&mut self) {
            set_ui_text_size(self.0);
        }
    }

    /// 「标准」档必须和引入这个设置之前一模一样：角色字号逐个钉死，rem 基准是 GPUI / 组件库一直用的 16px。
    #[test]
    fn the_normal_level_is_exactly_what_the_interface_used_before() {
        let before = [
            (Role::PageTitle, 24.0),
            (Role::GroupTitle, 15.0),
            (Role::ItemName, 14.0),
            (Role::FieldLabel, 13.0),
            (Role::Caption, 12.0),
            (Role::Body, 14.0),
            (Role::Tree, 14.0),
            (Role::Nav, 14.0),
            (Role::NavSelected, 14.0),
            (Role::NavGroup, 12.0),
            (Role::Menu, 13.0),
        ];
        assert_eq!(
            before.len(),
            Role::ALL.len(),
            "新增角色要把它的旧字号补进这张表"
        );
        for (role, px_before) in before {
            assert_eq!(role.size_at(UiTextSize::Normal), px_before, "{role:?}");
        }
        assert_eq!(UiTextSize::default(), UiTextSize::Normal);
        assert_eq!(UiTextSize::Normal.scale(), 1.0);
        assert_eq!(UiTextSize::Normal.rem_px(), 16.0);
        assert_eq!(BASE_REM_PX, 16.0);
        // 字面值经过帮助函数也不变（标准档乘 1.0 没有舍入）。
        let _guard = LevelGuard::set(UiTextSize::Normal);
        for base in [10.0_f32, 11.0, 11.5, 12.0, 13.0, 14.0] {
            assert_eq!(f32::from(ui_text_px(base)), base);
        }
    }

    #[test]
    fn the_three_levels_use_the_agreed_factors() {
        assert_eq!(UiTextSize::Small.scale(), 0.875);
        assert_eq!(UiTextSize::Normal.scale(), 1.0);
        assert_eq!(UiTextSize::Large.scale(), 1.125);
        assert_eq!(UiTextSize::Small.rem_px(), 14.0);
        assert_eq!(UiTextSize::Normal.rem_px(), 16.0);
        assert_eq!(UiTextSize::Large.rem_px(), 18.0);
        // 档位越大字越大，三档都不是零。
        let [small, normal, large] = UiTextSize::ALL.map(UiTextSize::scale);
        assert!(0.0 < small && small < normal && normal < large);
    }

    #[test]
    fn every_role_scales_by_the_level_factor() {
        for role in Role::ALL {
            let base = role.spec().size;
            assert_eq!(role.size_at(UiTextSize::Small), base * 0.875, "{role:?}");
            assert_eq!(role.size_at(UiTextSize::Large), base * 1.125, "{role:?}");
        }
        assert_eq!(Role::PageTitle.size_at(UiTextSize::Large), 27.0);
        assert_eq!(Role::Caption.size_at(UiTextSize::Small), 10.5);
        assert_eq!(Role::Menu.size_at(UiTextSize::Large), 14.625);
    }

    /// `typo` 真的把当前档位写进样式；换档以后同一个角色拿到新的字号，行高系数不变。
    #[test]
    fn typo_writes_the_scaled_size_into_the_style() {
        use ::gpui::{AbsoluteLength, Styled as _, div};
        let tokens = Tokens::for_skin(&SKINS[0]);
        for level in UiTextSize::ALL {
            let _guard = LevelGuard::set(level);
            for role in Role::ALL {
                let mut element = div().typo(role, &tokens);
                let style = element.text_style().clone();
                assert_eq!(
                    style.font_size,
                    Some(AbsoluteLength::Pixels(px(role.spec().size * level.scale()))),
                    "{role:?} @ {level:?}"
                );
            }
        }
    }

    #[test]
    fn the_literal_helper_follows_the_current_level_and_the_guard_restores_it() {
        {
            let _guard = LevelGuard::set(UiTextSize::Large);
            assert_eq!(ui_text_size(), UiTextSize::Large);
            assert_eq!(f32::from(ui_text_px(13.0)), 14.625);
            let _inner = LevelGuard::set(UiTextSize::Small);
            assert_eq!(f32::from(ui_text_px(12.0)), 10.5);
        }
        assert_eq!(ui_text_size(), UiTextSize::Normal);
    }

    #[test]
    fn levels_are_stored_as_lowercase_words() {
        for (level, word) in [
            (UiTextSize::Small, "\"small\""),
            (UiTextSize::Normal, "\"normal\""),
            (UiTextSize::Large, "\"large\""),
        ] {
            assert_eq!(serde_json::to_string(&level).unwrap(), word);
            assert_eq!(serde_json::from_str::<UiTextSize>(word).unwrap(), level);
        }
        assert!(serde_json::from_str::<UiTextSize>("\"huge\"").is_err());
    }

    /// 窗口测试里每个窗口在渲染前都是 GPUI 的默认 rem 基准；标准档下我们写的值和它一样。
    #[gpui::test]
    fn a_fresh_window_and_the_theme_already_use_the_normal_rem_base(
        cx: &mut ::gpui::TestAppContext,
    ) {
        use gpui_kit::component::ActiveTheme as _;
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::settings_store::AppSettings::init_for_test(cx);
            assert_eq!(cx.theme().font_size, px(16.0), "标准档的主题 rem 基准");
        });
        let window = cx.add_empty_window();
        let rem = window.update(|window, _| window.rem_size());
        assert_eq!(rem, px(UiTextSize::Normal.rem_px()));
    }

    /// 带组件库 `Root` 的窗口：换档以后主题的 rem 基准跟着变，`Root` 每帧把它写进窗口。
    #[gpui::test]
    fn a_root_window_follows_the_theme_rem_base_in_all_three_levels(
        cx: &mut ::gpui::TestAppContext,
    ) {
        use ::gpui::{AppContext as _, Empty, WindowOptions};
        use gpui_kit::component::ActiveTheme as _;
        let _guard = LevelGuard::set(UiTextSize::Normal);
        let settings = cx.update(|cx| {
            gpui_kit::init(cx);
            crate::settings_store::AppSettings::init_for_test(cx)
        });
        let (handle, _view) = cx
            .update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| cx.new(|_| Empty))
            })
            .expect("开 Root 窗口");
        let rem_of = |cx: &mut ::gpui::TestAppContext| {
            cx.update_window(handle, |_, window, cx| {
                window.draw(cx).clear(cx);
                window.rem_size()
            })
            .expect("画一帧")
        };
        assert_eq!(rem_of(cx), px(16.0));
        for level in [UiTextSize::Small, UiTextSize::Large, UiTextSize::Normal] {
            settings.update(cx, |model, mcx| {
                model.update_with(mcx, |s| s.ui_text_size = level);
            });
            assert_eq!(ui_text_size(), level);
            assert_eq!(
                cx.update(|cx| cx.theme().font_size),
                px(level.rem_px()),
                "{level:?} 的主题 rem 基准"
            );
            assert_eq!(rem_of(cx), px(level.rem_px()), "{level:?} 的窗口 rem 基准");
        }
    }
}
