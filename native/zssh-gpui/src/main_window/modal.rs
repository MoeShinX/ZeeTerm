//! 主窗口里的对话框：确认、分组新建 / 重命名、快速连接的密码、生成密钥、安装公钥。
//!
//! 同一时间一个，盖在整个窗口上面（半透明遮罩 + 居中卡片）。对话框是主窗口自己画的，不走系统
//! 模态框：状态都在 [`Modal`] 里，自动化驱动能直接看、直接点，Esc 取消、Enter 确认。
//! 字段和流程对照 0.2.14 的 `GroupFormDialog.tsx` / `QuickConnectDialog.tsx` / `ConfirmDialog` /
//! `SshKeyDialogs.tsx`。

use ::gpui::{Entity, SharedString, hsla, px};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::model::{Group, GroupDraft};

use super::MainWindow;
use super::key_dialogs::{GenerateKeyDialog, InstallKeyDialog};
use super::page_logic::{delete_group_message, flatten_groups, forbidden_parents};
use super::widgets::{ButtonLook, Choice, error_note, field, note, select, text_button};
use crate::app_theme::Tokens;
use crate::typography::{Role, Typo as _};

/// 确认之后要做的事。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfirmAction {
    DeleteHost(i64),
    DeleteGroup(i64),
    DeleteIdentity(i64),
    DeleteProxy(i64),
    DeleteForward(i64),
    ClearIdentitySecret(i64),
    ClearProxySecret(i64),
    /// 多行命令发送前的确认（快捷命令栏和撰写栏共用）；`compose` 为真时发完要清空撰写栏。
    SendCommand {
        data: String,
        all: bool,
        compose: Option<String>,
    },
    DeleteQuickCommand(String),
    /// 有进行中的工作时真正退出。
    QuitApp,
    CloseSftpTab(u64),
}

pub struct Confirm {
    pub title: String,
    pub message: String,
    /// `None` = 只能关掉（比如还有主机在用这条凭据，不给删）。
    pub confirm_label: Option<String>,
    pub destructive: bool,
    pub action: ConfirmAction,
}

pub struct GroupDialog {
    pub group: Option<Group>,
    pub parent: Option<i64>,
    /// 「未分组」重命名：建成分组，并把现在所有未分组的主机收进去。
    pub adopt_ungrouped: bool,
    pub ungrouped_count: usize,
    pub name: Entity<InputState>,
    pub error: Option<String>,
}

/// 快速连接输入的检查：要写成 `user@host[:port]`，解析规则同主机页搜索框里回车（`parse_quick_target`）。
pub fn quick_target_check(text: &str) -> Result<(), String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("请输入 用户名@主机地址".to_owned());
    }
    if !text.contains('@') {
        return Err("要写成 用户名@主机地址，比如 user@host:22".to_owned());
    }
    zssh_core::model::parse_quick_target(text)
        .map(|_| ())
        .map_err(|err| err.to_string())
}

pub struct QuickDialog {
    pub target: String,
    pub password: Entity<InputState>,
}

/// 欢迎页的「快速连接」：先问 `user@host[:port]`，填对了再走原来的密码对话框。
pub struct QuickTargetDialog {
    pub error: Option<String>,
}

pub enum Modal {
    Confirm(Confirm),
    Group(GroupDialog),
    Quick(QuickDialog),
    QuickTarget(QuickTargetDialog),
    GenerateKey(GenerateKeyDialog),
    InstallKey(InstallKeyDialog),
    /// 编辑器有未保存修改：保存 / 不保存 / 取消。
    Unsaved(super::editor_tabs::UnsavedAsk),
    /// 启动时的恢复提示：上次没处理完的修改。
    Recovery(super::recovery_ui::RecoveryUi),
    /// 连开了 X11 的主机而本机没有可用的 X 服务器：一键安装 / 去设置 / 这次不用 X11 继续连接。
    X11Needed(super::x11_ui::X11Prompt),
}

impl Modal {
    /// 对话框里有没有后台任务在跑（跑的时候不能点遮罩 / Esc 关掉）。
    pub fn busy(&self) -> bool {
        match self {
            Self::GenerateKey(dialog) => dialog.busy(),
            Self::InstallKey(dialog) => dialog.busy(),
            Self::Unsaved(ask) => ask.busy,
            _ => false,
        }
    }
}

impl MainWindow {
    // ---- 打开 ----

    pub(crate) fn ask_delete_host(&mut self, host_id: i64, cx: &mut Context<Self>) {
        let Some(host) = self.store.host(host_id).cloned() else {
            return;
        };
        // 读库里最新的主机和转发规则，列出连带影响（F3 X3）。
        let hosts = self
            .store
            .db()
            .ok()
            .and_then(|db| db.host_list().ok())
            .unwrap_or_else(|| self.store.hosts().to_vec());
        let forwards = self
            .store
            .db()
            .ok()
            .and_then(|db| db.forward_list().ok())
            .unwrap_or_default();
        let impact = crate::hosts_demo::host_delete_impact(&hosts, &forwards, host_id);
        // 它的密码：独占的一起删，共用的保留，密钥不动（V2-U1 第 8 条，规则见 `host_delete`）。
        let identities = self
            .store
            .db()
            .ok()
            .and_then(|db| db.identity_list().ok())
            .unwrap_or_default();
        let credential_lines = super::host_delete::confirm_lines(&super::host_delete::plan(
            host_id,
            &hosts,
            &identities,
        ));
        let mut message = "主机的记录会被删掉。".to_owned();
        for line in credential_lines {
            message.push('\n');
            message.push_str(&line);
        }
        if !impact.via_jump.is_empty() {
            message.push_str(&format!(
                "\n{} 台经它跳转的主机（{}）会改成直连目标地址。",
                impact.via_jump.len(),
                impact.via_jump.join("、")
            ));
        }
        if impact.forwards > 0 {
            message.push_str(&format!(
                "\n它的 {} 条端口转发规则会一起删除。",
                impact.forwards
            ));
        }
        self.modal = Some(Modal::Confirm(Confirm {
            title: format!("删除主机「{}」？", host.name),
            message,
            confirm_label: Some("删除".to_owned()),
            destructive: true,
            action: ConfirmAction::DeleteHost(host_id),
        }));
        cx.notify();
    }

    pub(crate) fn ask_delete_group(&mut self, group_id: i64, cx: &mut Context<Self>) {
        let Some(group) = self
            .store
            .groups()
            .iter()
            .find(|g| g.id == group_id)
            .cloned()
        else {
            return;
        };
        self.modal = Some(Modal::Confirm(Confirm {
            title: format!("删除分组「{}」？", group.name),
            message: delete_group_message(self.store.flat()).to_owned(),
            confirm_label: Some("删除".to_owned()),
            destructive: true,
            action: ConfirmAction::DeleteGroup(group_id),
        }));
        cx.notify();
    }

    /// 新建（`group` 为空）或重命名分组；`parent` 是新建时默认挂在哪个分组下。
    pub(crate) fn open_group_dialog(
        &mut self,
        group: Option<Group>,
        parent: Option<i64>,
        adopt_ungrouped: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let initial = group.as_ref().map(|g| g.name.clone()).unwrap_or_default();
        let name = cx.new(|cx| InputState::new(window, cx).default_value(initial));
        let focus = name.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        let ungrouped_count = self
            .store
            .hosts()
            .iter()
            .filter(|host| host.group_id.is_none())
            .count();
        let parent = group.as_ref().map_or(parent, |g| g.parent_id);
        self.modal = Some(Modal::Group(GroupDialog {
            group,
            parent,
            adopt_ungrouped,
            ungrouped_count,
            name,
            error: None,
        }));
        cx.notify();
    }

    /// 欢迎页的「快速连接」：弹出输入 `user@host[:port]` 的小对话框（输入框是主窗口的 `quick_target`）。
    pub(crate) fn open_quick_target_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.quick_target.clone();
        input.update(cx, |state, cx| state.set_value("", window, cx));
        window.focus(&input.read(cx).focus_handle(cx), cx);
        self.modal = Some(Modal::QuickTarget(QuickTargetDialog { error: None }));
        cx.notify();
    }

    pub(crate) fn open_quick_dialog(
        &mut self,
        target: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let password = cx.new(|cx| InputState::new(window, cx).masked(true));
        let focus = password.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        self.modal = Some(Modal::Quick(QuickDialog { target, password }));
        cx.notify();
    }

    // ---- 关闭 / 确认 ----

    pub(crate) fn close_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal.as_ref().is_some_and(Modal::busy) {
            return;
        }
        self.modal = None;
        self.focus_content(window, cx);
        cx.notify();
    }

    /// Enter / 主按钮。
    pub(crate) fn confirm_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.modal {
            Some(Modal::Confirm(_)) => self.run_confirm(window, cx),
            Some(Modal::Group(_)) => self.submit_group_dialog(window, cx),
            Some(Modal::Quick(_)) => self.submit_quick_dialog(window, cx),
            Some(Modal::QuickTarget(_)) => self.submit_quick_target(window, cx),
            Some(Modal::GenerateKey(_)) => self.submit_generate_key(window, cx),
            Some(Modal::InstallKey(_)) => self.submit_install_key(window, cx),
            Some(Modal::Unsaved(_)) => self.answer_unsaved(true, window, cx),
            // 恢复提示每一项有自己的按钮，回车不替用户选。
            Some(Modal::Recovery(_)) => {}
            // 三个选项各有按钮：回车不替用户选（尤其不能一回车就开始安装）。
            Some(Modal::X11Needed(_)) => {}
            None => {}
        }
    }

    fn run_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::Confirm(confirm)) = self.modal.take() else {
            return;
        };
        if confirm.confirm_label.is_none() {
            self.focus_content(window, cx);
            cx.notify();
            return;
        }
        match confirm.action {
            ConfirmAction::DeleteHost(id) => self.do_delete_host(id, window, cx),
            ConfirmAction::DeleteGroup(id) => self.do_delete_group(id, window, cx),
            ConfirmAction::DeleteIdentity(id) => self.do_delete_identity(id, window, cx),
            ConfirmAction::DeleteProxy(id) => self.do_delete_proxy(id, window, cx),
            ConfirmAction::DeleteForward(id) => self.do_delete_forward(id, window, cx),
            ConfirmAction::ClearIdentitySecret(id) => self.do_clear_identity_secret(id, window, cx),
            ConfirmAction::ClearProxySecret(id) => self.do_clear_proxy_secret(id, window, cx),
            ConfirmAction::SendCommand { data, all, compose } => {
                self.do_send(&data, all, compose.as_deref(), window, cx);
            }
            ConfirmAction::DeleteQuickCommand(id) => self.do_delete_quick_command(&id, window, cx),
            ConfirmAction::QuitApp => {
                self.do_quit(window, cx);
                return;
            }
            ConfirmAction::CloseSftpTab(id) => self.do_close_sftp_tab(id, window, cx),
        }
        self.focus_content(window, cx);
        cx.notify();
    }

    fn submit_group_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::Group(dialog)) = &self.modal else {
            return;
        };
        let name = dialog.name.read(cx).value().trim().to_owned();
        if name.is_empty() {
            self.set_group_error("分组名称不能为空", cx);
            return;
        }
        let (existing, parent, adopt) =
            (dialog.group.clone(), dialog.parent, dialog.adopt_ungrouped);
        let draft = GroupDraft {
            id: existing.as_ref().map(|g| g.id),
            name: name.clone(),
            parent_id: parent,
            sort: existing.as_ref().map_or(0, |g| g.sort),
        };
        let saved = match self.store.save_group(&draft) {
            Ok(saved) => saved,
            Err(err) => {
                self.set_group_error(&err, cx);
                return;
            }
        };
        if adopt {
            let loose: Vec<i64> = self
                .store
                .hosts()
                .iter()
                .filter(|host| host.group_id.is_none())
                .map(|host| host.id)
                .collect();
            if let Err(err) = self.store.move_hosts(&loose, Some(saved.id)) {
                self.set_group_error(&err, cx);
                return;
            }
            // 「未分组」改名成功：树里不再有「未分组」这一行，之后不分组的主机直接排在树的根层。
            // 分组已经建好了，写开关失败只提示，不影响关窗口。
            if let Err(err) = self.store.set_flat(true) {
                self.notice_error(&format!("没能记住「平铺」设置：{err}"), window, cx);
            }
        }
        self.modal = None;
        self.focus_content(window, cx);
        cx.notify();
    }

    fn set_group_error(&mut self, message: &str, cx: &mut Context<Self>) {
        if let Some(Modal::Group(dialog)) = &mut self.modal {
            dialog.error = Some(message.to_owned());
        }
        cx.notify();
    }

    /// 欢迎页快速连接第一步：检查写法（同主机页搜索框里回车的解析），对了才进密码对话框，
    /// 不对就把原因留在对话框里。
    fn submit_quick_target(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.quick_target.read(cx).value().trim().to_owned();
        match quick_target_check(&text) {
            Ok(()) => {
                self.modal = None;
                self.open_quick_dialog(text, window, cx);
            }
            Err(message) => {
                if let Some(Modal::QuickTarget(dialog)) = &mut self.modal {
                    dialog.error = Some(message);
                }
                cx.notify();
            }
        }
    }

    fn submit_quick_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::Quick(dialog)) = self.modal.take() else {
            return;
        };
        let password = dialog.password.read(cx).value().to_string();
        self.quick_connect(&dialog.target, password, window, cx);
    }

    // ---- 画 ----

    pub(super) fn render_modal(
        &mut self,
        tokens: &Tokens,
        window: &mut ::gpui::Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let modal = self.modal.as_ref()?;
        let entity = cx.entity();
        let (width, content): (f32, AnyElement) = match modal {
            Modal::Confirm(confirm) => (420.0, self.confirm_body(confirm, tokens, cx)),
            Modal::Group(dialog) => (400.0, self.group_body(dialog, tokens, cx)),
            Modal::Quick(dialog) => (440.0, self.quick_body(dialog, tokens, cx)),
            Modal::QuickTarget(dialog) => (440.0, self.quick_target_body(dialog, tokens, cx)),
            Modal::GenerateKey(dialog) => (480.0, self.generate_key_body(dialog, tokens, cx)),
            Modal::InstallKey(dialog) => (480.0, self.install_key_body(dialog, tokens, cx)),
            Modal::Unsaved(ask) => (440.0, self.unsaved_body(ask, tokens, cx)),
            Modal::Recovery(ui) => (600.0, self.recovery_body(ui, tokens, cx)),
            Modal::X11Needed(prompt) => (460.0, self.x11_prompt_body(prompt, tokens, cx)),
        };
        let _ = window;
        let scrim = {
            let entity = entity.clone();
            div()
                .id("modal-scrim")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(hsla(0.0, 0.0, 0.0, 0.45))
                .on_mouse_down(::gpui::MouseButton::Left, move |_, window, cx| {
                    entity.update(cx, |this, cx| this.close_modal(window, cx));
                })
        };
        Some(
            scrim
                .child(
                    v_flex()
                        .id("modal-card")
                        .w(px(width))
                        .max_h(px(640.0))
                        .p_5()
                        .gap_4()
                        .rounded_lg()
                        .bg(tokens.background)
                        .border_1()
                        .border_color(tokens.border)
                        .shadow_lg()
                        .on_mouse_down(::gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(content),
                )
                .into_any_element(),
        )
    }

    fn confirm_body(
        &self,
        confirm: &Confirm,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity_cancel = cx.entity();
        let entity_ok = entity_cancel.clone();
        let look = if confirm.destructive {
            ButtonLook::Danger
        } else {
            ButtonLook::Primary
        };
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .typo(Role::GroupTitle, tokens)
                            .child(SharedString::from(confirm.title.clone())),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(tokens.text_muted)
                            .child(SharedString::from(confirm.message.clone())),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .when(confirm.confirm_label.is_some(), |d| {
                        d.child(text_button(
                            "modal-cancel",
                            "取消",
                            ButtonLook::Outline,
                            move |_, window, cx| {
                                entity_cancel.update(cx, |this, cx| this.close_modal(window, cx));
                            },
                        ))
                    })
                    .child(text_button(
                        "modal-ok",
                        confirm
                            .confirm_label
                            .clone()
                            .unwrap_or_else(|| "知道了".to_owned()),
                        look,
                        move |_, window, cx| {
                            entity_ok.update(cx, |this, cx| this.confirm_modal(window, cx));
                        },
                    )),
            )
            .into_any_element()
    }

    fn group_body(
        &self,
        dialog: &GroupDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let title = if dialog.group.is_some() {
            "重命名分组"
        } else if dialog.adopt_ungrouped {
            "重命名「未分组」"
        } else {
            "新建分组"
        };
        let groups = self.store.groups();
        let forbidden = dialog
            .group
            .as_ref()
            .map(|g| forbidden_parents(groups, g.id))
            .unwrap_or_default();
        let ordered = flatten_groups(groups);
        let mut choices = vec![Choice::new("（顶层）", dialog.parent.is_none())];
        let mut targets: Vec<Option<i64>> = vec![None];
        for (group, depth) in &ordered {
            if forbidden.contains(&group.id) {
                continue;
            }
            choices.push(
                Choice::new(group.name.clone(), dialog.parent == Some(group.id)).depth(*depth),
            );
            targets.push(Some(group.id));
        }
        let current = match dialog.parent {
            None => "（顶层）".to_owned(),
            Some(id) => groups
                .iter()
                .find(|g| g.id == id)
                .map_or_else(|| "（顶层）".to_owned(), |g| g.name.clone()),
        };
        let pick = {
            let entity = entity.clone();
            move |index: usize, _window: &mut Window, cx: &mut App| {
                let Some(target) = targets.get(index).copied() else {
                    return;
                };
                entity.update(cx, |this, cx| {
                    if let Some(Modal::Group(dialog)) = &mut this.modal {
                        dialog.parent = target;
                    }
                    cx.notify();
                });
            }
        };
        let cancel = entity.clone();
        let save = entity.clone();
        v_flex()
            .gap_4()
            .child(
                div()
                    .typo(Role::GroupTitle, tokens)
                    .child(title),
            )
            .when(dialog.adopt_ungrouped, |d| {
                d.child(note(
                    format!(
                        "「未分组」不是一个真的分组，只是没分组的主机放在一起。这里会建一个这个名字的分组，把现在 {} 台未分组的主机都移进去。",
                        dialog.ungrouped_count
                    ),
                    tokens,
                ))
            })
            .child(field("名称", None, Input::new(&dialog.name), tokens))
            .child(field(
                "上级分组",
                None,
                select("group-parent", current, choices, pick),
                tokens,
            ))
            .children(
                dialog
                    .error
                    .clone()
                    .map(|error| error_note(error, tokens.danger)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(text_button(
                        "modal-cancel",
                        "取消",
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            cancel.update(cx, |this, cx| this.close_modal(window, cx));
                        },
                    ))
                    .child(text_button(
                        "modal-ok",
                        "保存",
                        ButtonLook::Primary,
                        move |_, window, cx| {
                            save.update(cx, |this, cx| this.confirm_modal(window, cx));
                        },
                    )),
            )
            .into_any_element()
    }

    fn quick_target_body(
        &self,
        dialog: &QuickTargetDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cancel = cx.entity();
        let next = cancel.clone();
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_1p5()
                    .child(div().typo(Role::GroupTitle, tokens).child("快速连接"))
                    .child(note(
                        "输入 用户名@主机地址（可以带 :端口），直接连，不会保存到主机库。",
                        tokens,
                    )),
            )
            .child(field(
                "连接到",
                None,
                Input::new(&self.quick_target),
                tokens,
            ))
            .children(
                dialog
                    .error
                    .clone()
                    .map(|error| error_note(error, tokens.danger)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(text_button(
                        "modal-cancel",
                        "取消",
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            cancel.update(cx, |this, cx| this.close_modal(window, cx));
                        },
                    ))
                    .child(text_button(
                        "modal-ok",
                        "下一步",
                        ButtonLook::Primary,
                        move |_, window, cx| {
                            next.update(cx, |this, cx| this.confirm_modal(window, cx));
                        },
                    )),
            )
            .into_any_element()
    }

    fn quick_body(
        &self,
        dialog: &QuickDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cancel = cx.entity();
        let connect = cancel.clone();
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_1p5()
                    .child(div().typo(Role::GroupTitle, tokens).child("快速连接"))
                    .child(note(
                        format!("直接连 {}，不会保存到主机库。", dialog.target),
                        tokens,
                    )),
            )
            .child(field("密码", None, Input::new(&dialog.password), tokens))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(text_button(
                        "modal-cancel",
                        "取消",
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            cancel.update(cx, |this, cx| this.close_modal(window, cx));
                        },
                    ))
                    .child(text_button(
                        "modal-ok",
                        "连接",
                        ButtonLook::Primary,
                        move |_, window, cx| {
                            connect.update(cx, |this, cx| this.confirm_modal(window, cx));
                        },
                    )),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::quick_target_check;

    #[test]
    fn the_welcome_quick_connect_accepts_what_the_search_box_accepts() {
        assert!(quick_target_check("ops@192.0.2.10").is_ok());
        assert!(quick_target_check("  ops@192.0.2.10:2222 ").is_ok());
        assert!(quick_target_check("ops@[2001:db8::1]:22").is_ok());
    }

    #[test]
    fn the_welcome_quick_connect_explains_what_is_wrong() {
        assert!(quick_target_check("").is_err());
        let missing_user = quick_target_check("192.0.2.10").unwrap_err();
        assert!(missing_user.contains("用户名@主机地址"));
        assert!(quick_target_check("@192.0.2.10").is_err(), "没有用户名");
        assert!(quick_target_check("ops@").is_err(), "没有地址");
        assert!(quick_target_check("ops@host:99999").is_err(), "端口越界");
        assert!(quick_target_check("ops @host").is_err(), "有空白");
    }
}
