//! 侧栏「属性」面板里和密码、复制、就地编辑有关的状态与动作（V2-U34）。
//!
//! 密码行默认是圆点；用户点小眼睛才从保险库取出明文放进 [`InfoSecret`]，隐藏就丢掉。
//! 明文能留多久只由一处决定：[`MainWindow::sync_info_secret`] 每帧开头检查「面板还在显示的是同一台主机吗」，
//! 切主机、收起面板、收起侧栏、编辑这台主机、关掉显示开关都会让它丢掉明文；锁屏那一帧走
//! [`MainWindow::forget_info_secret`]（主窗口只画锁屏页，面板根本不进元素树）。
//!
//! 单击值是复制：因为双击要进就地编辑，复制延迟一小会儿，双击的第二击把没执行的复制作废
//! （[`MainWindow::info_copy_gen`]）。双击值进入 [`InfoEdit`] 的行内输入框，回车 / 失焦保存，
//! 面板这一帧显示的不是同一台主机时由 [`MainWindow::sync_info_edit`] 收起。

use std::time::Duration;

use ::gpui::{AppContext as _, Context, Entity, Focusable as _, Window};
use gpui_kit::base::TextSelection;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::*;
use zeroize::Zeroizing;
use zssh_core::model::HostDraft;

use super::MainWindow;
use super::page_logic::info_password_identity;
use super::reveal_ui::RevealAction;
use crate::i18n::{Msg, t};

/// 本文件（属性面板的复制 / 显示密码 / 就地编辑提示）的界面文案。
#[derive(Clone, Copy, Debug)]
enum InfoPanelMsg {
    CopiedPrefix,
    RevealFirst,
    PasswordCopied,
    PasswordCopyFailedPrefix,
    Saved,
    FieldEmpty,
    PortBad,
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
            Self::Saved => "已保存",
            Self::FieldEmpty => "内容不能为空",
            Self::PortBad => "端口要是 1～65535 的数字",
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
            Self::Saved => "Saved",
            Self::FieldEmpty => "The value cannot be empty",
            Self::PortBad => "The port must be a number between 1 and 65535",
        }
    }
}

/// 属性面板里可以就地编辑的行。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InfoField {
    Name,
    Address,
    Username,
    Port,
}

/// 一行正在就地编辑的主机信息；回车 / 失焦保存，面板换主机就收起。
pub(crate) struct InfoEdit {
    pub(crate) host_id: i64,
    pub(crate) field: InfoField,
    pub(crate) input: Entity<InputState>,
    /// 输入框的事件订阅；Drop 时自动退订。
    #[allow(dead_code)]
    subscription: ::gpui::Subscription,
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

    /// 单击值 = 复制：双击的第二击会把没执行的复制作废，所以延迟一小会儿再写剪贴板。
    /// `text` 是 `None` 的密码行：只有明文显示着才复制（和右键菜单同一套逻辑）。
    pub(crate) fn info_copy_schedule(
        &mut self,
        host_id: i64,
        text: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.info_copy_gen += 1;
        let generation = self.info_copy_gen;
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(260))
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.info_copy_gen != generation {
                    return;
                }
                this.info_copy_gen += 1;
                match text {
                    Some(text) => this.copy_info_value(text, window, cx),
                    None => this.copy_info_password(host_id, window, cx),
                }
            });
        })
        .detach();
    }

    /// 双击值：进入这一行的就地编辑（行内输入框，回车 / 失焦保存）。
    pub(crate) fn info_edit_open(
        &mut self,
        host_id: i64,
        field: InfoField,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.info_copy_gen += 1;
        let input = cx.new(|cx| InputState::new(window, cx).default_value(value));
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    this.info_edit_commit(window, cx);
                }
            },
        );
        self.info_edit = Some(InfoEdit {
            host_id,
            field,
            input,
            subscription,
        });
        let focus = self
            .info_edit
            .as_ref()
            .map(|edit| edit.input.read(cx).focus_handle(cx));
        if let Some(focus) = focus {
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    /// 提交就地编辑：校验一个字段、其余字段照库里原样，保存落库。
    pub(crate) fn info_edit_commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = &self.info_edit else {
            return;
        };
        let (host_id, field) = (edit.host_id, edit.field);
        let value = edit.input.read(cx).value().trim().to_owned();
        let value = match field {
            InfoField::Port => match value.parse::<u16>() {
                Ok(port) if port > 0 => port.to_string(),
                _ => {
                    window.push_notification(t(InfoPanelMsg::PortBad).to_owned(), cx);
                    return;
                }
            },
            _ if value.is_empty() => {
                window.push_notification(t(InfoPanelMsg::FieldEmpty).to_owned(), cx);
                return;
            }
            _ => value,
        };
        let Some(host) = self.store.host(host_id).cloned() else {
            // 主机已经没了（别处删的）：收起编辑，面板随后自己消失。
            self.info_edit = None;
            cx.notify();
            return;
        };
        let mut draft = info_edit_draft(&host);
        match field {
            InfoField::Name => draft.name = value,
            InfoField::Address => draft.host = value,
            InfoField::Username => draft.username = value,
            InfoField::Port => draft.port = value.parse::<u16>().unwrap_or(host.port),
        }
        let saved = self
            .store
            .db()
            .and_then(|db| db.host_save(&draft).map_err(|err| err.to_string()));
        match saved {
            Ok(_) => {
                self.store.reload();
                self.info_edit = None;
                window.push_notification(t(InfoPanelMsg::Saved).to_owned(), cx);
                cx.notify();
            }
            Err(err) => {
                // 保存失败（比如改成了和别的字段冲突的组合）：留在编辑态，原样把原因给用户。
                window.push_notification(err, cx);
            }
        }
    }

    /// 面板这一帧显示的不是正在编辑的那台主机（切主机 / 收起面板 / 主机没了）就收起编辑。
    pub(super) fn sync_info_edit(&mut self, panel_host: Option<i64>) {
        let stale = match (&self.info_edit, panel_host) {
            (Some(edit), Some(host)) => edit.host_id != host,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if stale {
            self.info_edit = None;
        }
    }

    /// 正在就地编辑的这一行（主机对得上时）。
    pub(super) fn info_edit_row(&self, host_id: i64) -> Option<(&InfoField, &Entity<InputState>)> {
        let edit = self.info_edit.as_ref()?;
        (edit.host_id == host_id).then_some((&edit.field, &edit.input))
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

/// 就地编辑保存用的草稿：只改目标字段，其余照库里这台主机原样（`protocol: None` = 协议设置不动）。
fn info_edit_draft(host: &zssh_core::model::Host) -> HostDraft {
    HostDraft {
        protocol: None,
        id: Some(host.id),
        group_id: host.group_id,
        name: host.name.clone(),
        host: host.host.clone(),
        port: host.port,
        username: host.username.clone(),
        auth_method: host.auth_method,
        identity_id: host.identity_id,
        proxy_id: host.proxy_id,
        jump_host_id: host.jump_host_id,
        proxy_mode: host.proxy_mode,
        tags: host.tags.clone(),
        sort: host.sort,
        legacy_algorithms: host.legacy_algorithms,
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
