//! 欢迎页：没有任何终端标签时的默认主区域。居中的产品名、一行副标题、两个小按钮
//! （快速连接、新建主机）和一行浅灰提示，大面积留白，颜色跟随主题。
//!
//! 只接现有功能：「快速连接」弹出输入 `user@host[:port]` 的小对话框（解析规则同主机页搜索框里回车，
//! 见 `modal::quick_target_check`），「新建主机」打开现有的新建主机面板。ZeeTerm 没有本地终端，
//! 所以没有「新建本地终端」。

use ::gpui::px;
use gpui_kit::component::*;
use gpui_kit::*;

use super::MainWindow;
use super::widgets::{ButtonLook, act, text_button};
use crate::app_theme::Tokens;
use crate::typography::{Role, Typo as _};

/// 副标题（自己的话，不照搬别的软件）。
pub const WELCOME_SUBTITLE: &str = "简洁的 SSH / Telnet / 串口终端";
/// 浅灰提示：两条入口都是真实存在的（侧栏双击主机、Ctrl+Shift+T 新标签）。
pub const WELCOME_HINT: &str = "从左侧双击主机开始连接，或按 Ctrl+Shift+T 新建标签";
/// 两个按钮的文字，顺序即显示顺序。
pub const WELCOME_BUTTONS: [&str; 2] = ["快速连接", "新建主机"];

impl MainWindow {
    pub(super) fn render_welcome(
        &self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entity = cx.entity();
        v_flex()
            .id("welcome-page")
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .bg(tokens.background)
            .child(
                div()
                    .text_3xl()
                    .font_weight(FontWeight::BOLD)
                    .text_color(tokens.text)
                    .child("ZeeTerm"),
            )
            .child(
                div()
                    .typo(Role::Body, tokens)
                    .text_color(tokens.text_muted)
                    .child(WELCOME_SUBTITLE),
            )
            .child(
                h_flex()
                    .mt_2()
                    .gap_2()
                    .child(
                        text_button(
                            "welcome-quick-connect",
                            WELCOME_BUTTONS[0],
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
                            WELCOME_BUTTONS[1],
                            ButtonLook::Outline,
                            act(&entity, |this, window, cx| this.new_host(window, cx)),
                        )
                        .icon(IconName::Plus),
                    ),
            )
            .child(
                div()
                    .mt_1()
                    .max_w(px(480.0))
                    .typo(Role::Caption, tokens)
                    .opacity(0.8)
                    .child(WELCOME_HINT),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{WELCOME_BUTTONS, WELCOME_HINT, WELCOME_SUBTITLE};

    #[test]
    fn the_welcome_page_has_two_entries_and_only_mentions_real_shortcuts() {
        assert_eq!(WELCOME_BUTTONS, ["快速连接", "新建主机"]);
        assert!(WELCOME_HINT.contains("Ctrl+Shift+T"));
        // 快捷键表里确实有这一条，提示不会说谎。
        assert!(
            crate::settings_nav::SHORTCUTS
                .iter()
                .any(|s| s.keys == "Ctrl+Shift+T")
        );
        assert!(!WELCOME_SUBTITLE.is_empty());
    }
}
