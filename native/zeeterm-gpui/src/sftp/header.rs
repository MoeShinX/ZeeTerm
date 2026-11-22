//! 文件视图顶部那一行的文字和小点：主机显示名、连接状态。
//!
//! 只放不依赖界面的规则（有单测）；怎么画在 `view.rs`，字号字重走 `typography::Role`。

use crate::app_theme::Tokens;
use crate::i18n::t;
use gpui_kit::Hsla;

use super::Phase;
use super::SftpMsg;

/// 主机的显示名：有名字用名字，名字是空的才退到地址，都没有就用编号。
/// 名字和地址两头的空白不算（用户可能留着空格）。
pub(super) fn display_name(name: &str, address: &str, host_id: i64) -> String {
    let name = name.trim();
    if !name.is_empty() {
        return name.to_owned();
    }
    let address = address.trim();
    if !address.is_empty() {
        return address.to_owned();
    }
    format!("{}{host_id}", t(SftpMsg::HostFallback))
}

/// 状态小点旁边的字。
pub(super) fn phase_label(phase: Phase) -> &'static str {
    match phase {
        Phase::Connecting => t(SftpMsg::PhaseConnecting),
        Phase::Ready => t(SftpMsg::PhaseReady),
        Phase::Failed => t(SftpMsg::PhaseFailed),
        Phase::Disconnected => t(SftpMsg::PhaseDisconnected),
    }
}

/// 状态小点的颜色（和主窗口标签上的连接状态点同一套）。
pub(super) fn phase_color(phase: Phase, tokens: &Tokens) -> Hsla {
    match phase {
        Phase::Connecting => tokens.dot_connecting,
        Phase::Ready => tokens.dot_connected,
        Phase::Failed | Phase::Disconnected => tokens.dot_disconnected,
    }
}

#[cfg(test)]
mod tests {
    use super::{Phase, display_name, phase_color, phase_label};
    use crate::app_theme::{SKINS, Tokens};

    #[test]
    fn a_named_host_shows_its_name_not_its_address() {
        assert_eq!(display_name("web-1", "192.0.2.11", 7), "web-1");
        assert_eq!(display_name("  web-1 ", "192.0.2.11", 7), "web-1");
    }

    #[test]
    fn an_unnamed_host_falls_back_to_the_address_then_the_id() {
        assert_eq!(display_name("", "192.0.2.11", 7), "192.0.2.11");
        assert_eq!(display_name("   ", " 192.0.2.11 ", 7), "192.0.2.11");
        assert_eq!(display_name("", "", 7), "主机 #7");
    }

    #[test]
    fn the_three_states_have_their_own_words_and_dots() {
        assert_eq!(phase_label(Phase::Connecting), "连接中…");
        assert_eq!(phase_label(Phase::Ready), "已连接");
        assert_eq!(phase_label(Phase::Failed), "连接失败");
        assert_eq!(phase_label(Phase::Disconnected), "已断开");
        for skin in SKINS {
            let t = Tokens::for_skin(skin);
            let dots = [
                phase_color(Phase::Connecting, &t),
                phase_color(Phase::Ready, &t),
                phase_color(Phase::Failed, &t),
            ];
            assert_ne!(dots[0], dots[1], "{} 连接中和已连接要能分开", skin.id);
            assert_ne!(dots[1], dots[2], "{} 已连接和失败要能分开", skin.id);
        }
    }
}
