//! 侧栏「属性」面板里和密码、复制有关的状态与动作（V2-U34）。
//!
//! 密码行默认是圆点；用户点小眼睛才从保险库取出明文放进 [`InfoSecret`]，隐藏就丢掉。
//! 明文能留多久只由一处决定：[`MainWindow::sync_info_secret`] 每帧开头检查「面板还在显示的是同一台主机吗」，
//! 切主机、收起面板、收起侧栏、编辑这台主机、关掉显示开关都会让它丢掉明文；锁屏那一帧走
//! [`MainWindow::forget_info_secret`]（主窗口只画锁屏页，面板根本不进元素树）。

use ::gpui::{Context, Window};
use gpui_kit::base::TextSelection;
use zeroize::Zeroizing;

use super::MainWindow;
use super::page_logic::info_password_identity;
use super::reveal_ui::RevealAction;
use crate::i18n::{Msg, t};

/// 本文件（属性面板的复制 / 显示密码提示）的界面文案。
#[derive(Clone, Copy, Debug)]
enum InfoPanelMsg {
    CopiedPrefix,
    RevealFirst,
    PasswordCopied,
    PasswordCopyFailedPrefix,
}

impl Msg for InfoPanelMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::CopiedPrefix => "已复制：",
            Self::RevealFirst => "先点眼睛显示密码",
            Self::PasswordCopied if cfg!(windows) => "已复制密码，30 秒后自动清除",
            // Linux 的剪贴板管理器可能已经记下来了，自动清除清不掉历史（见 `chat_copy`）。
            Self::PasswordCopied => "已复制密码，30 秒后自动清除；剪贴板管理器可能会记下这条内容",
            Self::PasswordCopyFailedPrefix => "复制密码失败：",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::CopiedPrefix => "Copied: ",
            Self::RevealFirst => "Click the eye to show the password first",
            Self::PasswordCopied if cfg!(windows) => {
                "Password copied; it will be cleared after 30 seconds"
            }
            Self::PasswordCopied => {
                "Password copied; it will be cleared after 30 seconds. A clipboard manager may keep a copy"
            }
            Self::PasswordCopyFailedPrefix => "Failed to copy password: ",
        }
    }
}

/// 取回来的明文密码，只属于 `host_id` 这一台主机；丢弃时内存清零。
pub(crate) struct InfoSecret {
    host_id: i64,
    value: Zeroizing<String>,
}

/// 明文还能不能留着：面板这一帧显示的还是同一台主机、没在编辑它、它现在仍然是「存了密码的密码登录」。
/// 任何一条不满足就丢（切主机、收起面板 / 侧栏、关掉显示开关、改了认证方式或密码）。
fn secret_stays(
    secret_host: i64,
    panel_host: Option<i64>,
    editing: bool,
    has_saved_password: bool,
) -> bool {
    panel_host == Some(secret_host) && !editing && has_saved_password
}

impl MainWindow {
    /// `host_id` 的密码现在以明文显示的话，返回明文；隐藏时 `None`。
    pub(super) fn info_revealed(&self, host_id: i64) -> Option<&str> {
        self.info_secret
            .as_ref()
            .filter(|secret| secret.host_id == host_id)
            .map(|secret| secret.value.as_str())
    }

    /// 小眼睛：隐藏 ⇄ 显示。显示时才从保险库取，取不到（没存密码、库打不开）就提示原因、保持隐藏。
    /// 取值和主机编辑对话框的「显示密码」是同一个入口：`identity_saved_password`。
    pub(crate) fn toggle_info_password(
        &mut self,
        host_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.info_revealed(host_id).is_some() {
            self.forget_info_secret(window, cx);
            return;
        }
        let Some(identity) = self
            .store
            .host(host_id)
            .and_then(|host| info_password_identity(host, self.store.identities()))
            .map(|identity| identity.id)
        else {
            return;
        };
        // 要从保险库取明文了：启用了锁屏的话先验证锁屏密码（V2-U35）。
        if !self.reveal_allowed(RevealAction::InfoEye(host_id), window, cx) {
            return;
        }
        let fetched = self.store.db().and_then(|db| {
            db.identity_saved_password(identity)
                .map_err(|err| crate::error_text::error_text(&err))
        });
        match fetched {
            Ok(value) => {
                self.info_secret = Some(InfoSecret { host_id, value });
                cx.notify();
            }
            Err(err) => self.notice_error(&err, window, cx),
        }
    }

    /// 丢掉明文，并清掉窗口里的文字选区（选区里可能正好选着密码）。
    pub(super) fn forget_info_secret(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.info_secret.take().is_some() {
            TextSelection::clear(window, cx);
            cx.notify();
        }
    }

    /// 每帧画侧栏前调用：明文只在「同一台主机的属性面板还开着、这台主机还是密码登录、没在编辑它」时保留。
    pub(super) fn sync_info_secret(
        &mut self,
        panel_host: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(secret) = &self.info_secret else {
            return;
        };
        let has_saved_password = self
            .store
            .host(secret.host_id)
            .is_some_and(|host| info_password_identity(host, self.store.identities()).is_some());
        if !secret_stays(
            secret.host_id,
            panel_host,
            self.host_form.is_some(),
            has_saved_password,
        ) {
            self.forget_info_secret(window, cx);
        }
    }

    /// 右键「复制」：普通行复制整行的值，走聊天复制同一个可替换的写入口（测试里不碰系统剪贴板）。
    pub(crate) fn copy_info_value(
        &mut self,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::chat_copy::write(text.clone(), cx);
        self.notice(
            &format!("{}{text}", t(InfoPanelMsg::CopiedPrefix)),
            window,
            cx,
        );
    }

    /// 右键「复制」密码行：只有明文已显示才复制；走和「复制密码」一样的敏感写入，30 秒后自动清除。
    pub(crate) fn copy_info_password(
        &mut self,
        host_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(secret) = self
            .info_revealed(host_id)
            .map(|text| Zeroizing::new(text.to_owned()))
        else {
            self.notice(t(InfoPanelMsg::RevealFirst), window, cx);
            return;
        };
        // 明文可能早就显示着、5 分钟窗口期已过：复制前再问一次（V2-U35）。
        if !self.reveal_allowed(RevealAction::InfoCopyPassword(host_id), window, cx) {
            return;
        }
        match crate::chat_copy::write_sensitive(secret.as_str(), cx) {
            Ok(()) => self.notice(t(InfoPanelMsg::PasswordCopied), window, cx),
            Err(err) => self.notice_error(
                &format!("{}{err}", t(InfoPanelMsg::PasswordCopyFailedPrefix)),
                window,
                cx,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{InfoPanelMsg, secret_stays};
    use crate::i18n::Msg as _;

    #[test]
    fn the_password_copy_notice_warns_about_clipboard_managers_except_on_windows() {
        let msg = InfoPanelMsg::PasswordCopied;
        assert!(msg.zh().contains("30 秒后自动清除") && msg.en().contains("30 seconds"));
        assert_eq!(
            msg.zh().contains("剪贴板管理器可能会记下这条内容"),
            !cfg!(windows)
        );
        assert_eq!(
            msg.en().contains("clipboard manager may keep a copy"),
            !cfg!(windows)
        );
        if cfg!(windows) {
            assert_eq!(msg.zh(), "已复制密码，30 秒后自动清除");
        }
    }

    #[test]
    fn the_secret_only_stays_for_the_same_open_panel_of_a_saved_password_host() {
        assert!(secret_stays(7, Some(7), false, true));
        assert!(!secret_stays(7, Some(8), false, true), "切到别的主机");
        assert!(
            !secret_stays(7, None, false, true),
            "收起面板 / 侧栏 / 关了开关"
        );
        assert!(!secret_stays(7, Some(7), true, true), "正在编辑这台主机");
        assert!(
            !secret_stays(7, Some(7), false, false),
            "不再是存了密码的密码登录"
        );
    }
}
