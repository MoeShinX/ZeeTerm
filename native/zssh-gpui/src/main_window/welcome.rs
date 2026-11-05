//! 欢迎页：没有任何终端标签时的默认主区域。居中的产品名、一行副标题、两个小按钮
//! （快速连接、新建主机）和一行浅灰提示，大面积留白，颜色跟随主题。
//!
//! 只接现有功能：「快速连接」弹出输入 `user@host[:port]` 的小对话框（解析规则同主机页搜索框里回车，
//! 见 `modal::quick_target_check`），「新建主机」打开现有的新建主机面板。ZeeTerm 没有本地终端，
//! 所以没有「新建本地终端」。

use ::gpui::{HighlightStyle, StyledText, px};
use gpui_kit::component::*;
use gpui_kit::*;

use super::MainWindow;
use super::widgets::{ButtonLook, act, text_button};
use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::typography::{Role, Typo as _};

/// 欢迎页上的文案（产品名 `ZeeTerm` 不翻译，不在表里）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WelcomeMsg {
    /// 大标题下面的一行副标题。
    Subtitle,
    /// 底部的浅灰提示。
    Hint,
    /// 第一个按钮：快速连接。
    QuickConnect,
    /// 第二个按钮：新建主机。
    NewHost,
}

impl Msg for WelcomeMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::Subtitle => "简洁的 SSH / Telnet / 串口终端",
            Self::Hint => "从左侧双击主机开始连接，或按 Ctrl+Shift+T 新建标签",
            Self::QuickConnect => "快速连接",
            Self::NewHost => "新建主机",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::Subtitle => "A clean SSH / Telnet / serial terminal",
            Self::Hint => {
                "Double-click a host on the left to connect, or press Ctrl+Shift+T for a new tab"
            }
            Self::QuickConnect => "Quick Connect",
            Self::NewHost => "New Host",
        }
    }
}

/// 大标题（产品名）。
const WELCOME_TITLE: &str = "ZeeTerm";
/// 标题渐变的两端：Logo 的紫（`#7950F2`）到浅紫（`#A263FF`），取自 `assets/brand/zeeterm-logo.svg`。
pub const TITLE_GRADIENT: (u32, u32) = (0x7950F2, 0xA263FF);

/// 标题第 `index` 个字（共 `count` 个）的颜色：从左到右在渐变两端之间线性插值（sRGB 各通道）。
///
/// GPUI 的文字颜色只能是单色（渐变只有背景支持，没法按字形裁剪），所以逐字取色：一个字一个颜色，
/// 7 个字母的步进很小，看上去就是连续的渐变。
pub fn title_color(index: usize, count: usize) -> u32 {
    let t = if count <= 1 {
        0.0
    } else {
        index as f32 / (count - 1) as f32
    };
    let (from, to) = TITLE_GRADIENT;
    let channel = |shift: u32| {
        let a = ((from >> shift) & 0xFF) as f32;
        let b = ((to >> shift) & 0xFF) as f32;
        ((a + (b - a) * t).round() as u32) & 0xFF
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

/// 大标题字的最低对比度（WCAG 大号字 3:1）。
const TITLE_MIN_CONTRAST: f32 = 3.0;

/// 同 [`title_color`]，但保证在 `background` 上读得清：对比度不够（深色底上的深紫、浅色底上的浅紫）
/// 就向白 / 黑推，直到够为止。够的字保持原色，所以浅色、深色皮肤上仍是同一条渐变，只是端点按底色微调。
pub fn title_color_on(index: usize, count: usize, background: u32) -> u32 {
    use crate::app_theme::{contrast, mix};
    let base = title_color(index, count);
    if contrast(base, background) >= TITLE_MIN_CONTRAST {
        return base;
    }
    let toward = if contrast(0xFFFFFF, background) >= contrast(0x000000, background) {
        0xFFFFFF
    } else {
        0x000000
    };
    (1..=50)
        .map(|step| mix(toward, base, step as f32 / 50.0))
        .find(|candidate| contrast(*candidate, background) >= TITLE_MIN_CONTRAST)
        .unwrap_or(toward)
}

/// 设置齿轮的颜色：首页标题渐变的中间那个紫（和 Logo 一个色系），对着当前皮肤底色校过对比度。
/// 图标是单色 SVG（按文字颜色着色），没法逐字渐变，所以取渐变正中间的一档。
pub(crate) fn brand_icon_color(cx: &::gpui::App) -> ::gpui::Hsla {
    let background = crate::app_theme::active_skin(cx).ui.background;
    gpui_kit::rgb(title_color_on(3, 7, background)).into()
}

/// 渐变标题：整段文字一次排版（保留字距），每个字母一段颜色。
fn gradient_title(background: u32) -> StyledText {
    let count = WELCOME_TITLE.chars().count();
    let ranges = WELCOME_TITLE
        .char_indices()
        .enumerate()
        .map(|(i, (at, ch))| {
            (
                at..at + ch.len_utf8(),
                HighlightStyle {
                    color: Some(gpui_kit::rgb(title_color_on(i, count, background)).into()),
                    ..Default::default()
                },
            )
        });
    StyledText::new(WELCOME_TITLE).with_highlights(ranges)
}

impl MainWindow {
    pub(super) fn render_welcome(
        &self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entity = cx.entity();
        // 标题的颜色要对着当前皮肤的底色校一次对比度（深色皮肤上最深的紫会发暗）。
        let skin_background = crate::app_theme::active_skin(cx).ui.background;
        // 本机有 Z-SSH 的数据、主机库还是空的：顶上多一条提示卡（V2-U19）。
        let tip = self
            .zssh_tip_visible()
            .then(|| self.render_zssh_tip(tokens, cx));
        v_flex()
            .id("welcome-page")
            .relative()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .bg(tokens.background)
            .children(tip)
            .child(
                div()
                    .text_3xl()
                    .font_weight(FontWeight::BOLD)
                    .child(gradient_title(skin_background)),
            )
            .child(
                div()
                    .typo(Role::Body, tokens)
                    .text_color(tokens.text_muted)
                    .child(t(WelcomeMsg::Subtitle)),
            )
            .child(
                h_flex()
                    .mt_2()
                    .gap_2()
                    .child(
                        text_button(
                            "welcome-quick-connect",
                            t(WelcomeMsg::QuickConnect),
                            ButtonLook::Primary,
                            act(&entity, |this, window, cx| {
                                this.open_quick_target_dialog(window, cx)
                            }),
                        )
                        .icon(IconName::Search),
                    )
                    .child(
                        text_button(
                            "welcome-new-host",
                            t(WelcomeMsg::NewHost),
                            ButtonLook::Outline,
                            act(&entity, |this, window, cx| this.new_host(window, cx)),
                        )
                        .icon(IconName::Plus),
                    ),
            )
            .child(self.render_zssh_link(tokens, cx))
            .child(
                div()
                    .mt_1()
                    .max_w(px(480.0))
                    .typo(Role::Caption, tokens)
                    .opacity(0.8)
                    .child(t(WelcomeMsg::Hint)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{TITLE_GRADIENT, WelcomeMsg, title_color, title_color_on};
    use crate::i18n::{Msg as _, t};

    #[test]
    fn the_title_runs_from_the_logo_purple_to_the_light_purple() {
        assert_eq!(
            TITLE_GRADIENT,
            (0x7950F2, 0xA263FF),
            "取自 zeeterm-logo.svg"
        );
        let colours: Vec<u32> = (0..7).map(|i| title_color(i, 7)).collect();
        assert_eq!(colours[0], 0x7950F2, "第一个字是深紫");
        assert_eq!(colours[6], 0xA263FF, "最后一个字是浅紫");
        // 中间 6 步均匀过渡：每个通道单调、相邻两步的差不超过一个步长。
        assert_eq!(colours[3], 0x8E5AF9, "正中间是两端的平均");
        for pair in colours.windows(2) {
            for shift in [16, 8, 0] {
                let (a, b) = ((pair[0] >> shift) & 0xFF, (pair[1] >> shift) & 0xFF);
                assert!(b >= a && b - a <= 8, "{:06X} -> {:06X}", pair[0], pair[1]);
            }
        }
        // 只有一个字时取起点，不除零。
        assert_eq!(title_color(0, 1), 0x7950F2);
        assert_eq!(title_color(0, 0), 0x7950F2);
    }

    #[test]
    fn the_gradient_title_reads_clearly_on_light_and_dark_skins() {
        use crate::app_theme::{SKINS, contrast};
        // 每个皮肤的底色上，7 个字都不低于大号字的 3:1。
        for skin in SKINS {
            let bg = skin.ui.background;
            for i in 0..7 {
                let c = contrast(title_color_on(i, 7, bg), bg);
                assert!(c >= 3.0, "{} 的第 {i} 个字对比度只有 {c:.2}", skin.name);
            }
        }
        // 纸白、Dracula 皮肤（不再是默认皮肤，但渐变的取色规则要对它们保持）：浅色底上完全保持原渐变，深色底上只调亮暗端。
        let paper = crate::app_theme::skin_for(false, "paper");
        assert_eq!(title_color_on(0, 7, paper.ui.background), 0x7950F2);
        assert_eq!(title_color_on(6, 7, paper.ui.background), 0xA263FF);
        let dracula = crate::app_theme::skin_for(true, "dracula");
        assert_eq!(title_color_on(6, 7, dracula.ui.background), 0xA263FF);
        assert_ne!(title_color_on(0, 7, dracula.ui.background), 0x7950F2);
    }

    #[test]
    fn the_welcome_page_has_two_entries_and_only_mentions_real_shortcuts() {
        assert_eq!(
            [t(WelcomeMsg::QuickConnect), t(WelcomeMsg::NewHost)],
            ["快速连接", "新建主机"]
        );
        assert!(t(WelcomeMsg::Hint).contains("Ctrl+Shift+T"));
        // 快捷键表里确实有这一条，提示不会说谎。
        assert!(
            crate::settings_nav::SHORTCUTS
                .iter()
                .any(|s| s.keys == "Ctrl+Shift+T")
        );
        assert!(!t(WelcomeMsg::Subtitle).is_empty());
    }

    #[test]
    fn every_welcome_message_keeps_its_chinese_and_has_english() {
        // 中文是原来那句，一字不改。
        assert_eq!(WelcomeMsg::Subtitle.zh(), "简洁的 SSH / Telnet / 串口终端");
        assert_eq!(
            WelcomeMsg::Hint.zh(),
            "从左侧双击主机开始连接，或按 Ctrl+Shift+T 新建标签"
        );
        assert_eq!(WelcomeMsg::QuickConnect.zh(), "快速连接");
        assert_eq!(WelcomeMsg::NewHost.zh(), "新建主机");
        for key in [
            WelcomeMsg::Subtitle,
            WelcomeMsg::Hint,
            WelcomeMsg::QuickConnect,
            WelcomeMsg::NewHost,
        ] {
            assert!(!key.en().is_empty(), "{key:?} 缺英文");
        }
    }
}
