//! 两个 SSH 密钥对话框：生成新密钥、把公钥装到服务器上（0.2.14 `SshKeyDialogs.tsx`）。
//!
//! 生成走 `Database::identity_generate`（RSA 4096 要几秒，放阻塞线程里，用独立的库连接）；
//! 安装走 `keygen::install_command` + 池里已认证的连接 `exec` + `parse_install_output`，
//! 和 Tauri 版 `host_install_public_key` 是同一套 core 调用。

use ::gpui::{ClipboardItem, Entity, SharedString};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::model::{AuthMethod, IdentityKind};
use zssh_core::ssh::keygen::{InstallOutcome, KeyAlgorithm};

use super::MainWindow;
use super::host_store::draft_of_host;
use super::modal::Modal;
use super::widgets::{
    ButtonLook, Choice, error_note, field, note, select, switch_row, text_button,
};
use crate::app_theme::Tokens;
use crate::typography::{Role, Typo as _};

/// 算法下拉里的三项：算法、名字、一句话说明。
pub const ALGORITHMS: [(KeyAlgorithm, &str, &str); 3] = [
    (
        KeyAlgorithm::Ed25519,
        "Ed25519（推荐）",
        "短、快，现在的服务器都支持",
    ),
    (
        KeyAlgorithm::EcdsaP256,
        "ECDSA P-256",
        "部分设备或合规要求只认 ECDSA",
    ),
    (
        KeyAlgorithm::Rsa4096,
        "RSA 4096",
        "兼容很老的服务器，生成要几秒",
    ),
];

pub fn algorithm_info(algorithm: KeyAlgorithm) -> (&'static str, &'static str) {
    ALGORITHMS
        .iter()
        .find(|(item, _, _)| *item == algorithm)
        .map_or(("", ""), |(_, label, hint)| (*label, *hint))
}

/// 生成好的密钥：给用户看公钥和指纹。
#[derive(Clone)]
pub struct GeneratedView {
    pub identity_id: i64,
    pub public_key: String,
    pub fingerprint: String,
}

pub struct GenerateKeyDialog {
    pub name: Entity<InputState>,
    pub comment: Entity<InputState>,
    pub passphrase: Entity<InputState>,
    pub confirm: Entity<InputState>,
    pub algorithm: KeyAlgorithm,
    pub running: bool,
    pub result: Option<GeneratedView>,
    pub error: Option<String>,
    /// 嵌在「安装公钥」里时为真：生成完回到安装，而不是在凭据面板里选中它。
    pub nested: bool,
}

impl GenerateKeyDialog {
    pub fn new(window: &mut Window, cx: &mut Context<MainWindow>, nested: bool) -> Self {
        Self {
            name: cx.new(|cx| InputState::new(window, cx).default_value("我的密钥")),
            comment: cx.new(|cx| InputState::new(window, cx).default_value("z-ssh")),
            passphrase: cx.new(|cx| InputState::new(window, cx).masked(true)),
            confirm: cx.new(|cx| InputState::new(window, cx).masked(true)),
            algorithm: KeyAlgorithm::Ed25519,
            running: false,
            result: None,
            error: None,
            nested,
        }
    }

    pub fn busy(&self) -> bool {
        self.running
    }
}

pub struct InstallKeyDialog {
    pub host_id: i64,
    pub host_name: String,
    pub key_id: Option<i64>,
    pub switch_auth: bool,
    pub running: bool,
    pub preview: Option<(String, String)>,
    pub error: Option<String>,
    pub generate: Option<GenerateKeyDialog>,
}

impl InstallKeyDialog {
    pub fn busy(&self) -> bool {
        self.running || self.generate.as_ref().is_some_and(GenerateKeyDialog::busy)
    }
}

/// 检查「生成密钥」表单：名称不能为空、两次口令要一样。
pub fn check_generate_input(name: &str, passphrase: &str, confirm: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("名称不能为空".to_owned());
    }
    if passphrase != confirm {
        return Err("两次输入的口令不一样".to_owned());
    }
    Ok(())
}

/// 安装结果的提示文字。
pub fn install_message(outcome: InstallOutcome, host_name: &str, switched: bool) -> String {
    format!(
        "{}「{host_name}」上{}",
        match outcome {
            InstallOutcome::Added => "公钥已装到",
            InstallOutcome::AlreadyPresent => "公钥本来就在",
        },
        if switched {
            "，这台主机改用密钥登录"
        } else {
            ""
        }
    )
}

impl MainWindow {
    // ---- 生成 ----

    pub(crate) fn open_generate_key_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dialog = GenerateKeyDialog::new(window, cx, false);
        let focus = dialog.name.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        self.modal = Some(Modal::GenerateKey(dialog));
        cx.notify();
    }

    /// 当前显示的生成对话框（独立的，或嵌在安装对话框里的）。
    fn generate_dialog_mut(&mut self) -> Option<&mut GenerateKeyDialog> {
        match self.modal.as_mut()? {
            Modal::GenerateKey(dialog) => Some(dialog),
            Modal::InstallKey(install) => install.generate.as_mut(),
            _ => None,
        }
    }

    pub(super) fn submit_generate_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (name, comment, passphrase, algorithm) = {
            let Some(dialog) = self.generate_dialog_mut() else {
                return;
            };
            if dialog.result.is_some() {
                // 已经生成好了：Enter / 「完成」就是关掉。
                self.finish_generate_dialog(window, cx);
                return;
            }
            if dialog.running {
                return;
            }
            let name = dialog.name.read(cx).value().to_string();
            let comment = dialog.comment.read(cx).value().to_string();
            let passphrase = dialog.passphrase.read(cx).value().to_string();
            let confirm = dialog.confirm.read(cx).value().to_string();
            if let Err(message) = check_generate_input(&name, &passphrase, &confirm) {
                dialog.error = Some(message);
                cx.notify();
                return;
            }
            dialog.error = None;
            dialog.running = true;
            (name, comment, passphrase, dialog.algorithm)
        };
        cx.notify();
        let work = super::bridge::on_blocking(move || -> Result<GeneratedView, String> {
            // 阻塞线程里单开一条库连接：连接对象不能跨线程。
            let db = crate::hosts_demo::open_database().map_err(|err| err.to_string())?;
            let passphrase = (!passphrase.is_empty()).then_some(passphrase.as_str());
            let identity = db
                .identity_generate(name.trim(), algorithm, &comment, passphrase)
                .map_err(|err| format!("生成失败：{err}"))?;
            let (public_key, fingerprint) = db
                .identity_public_key(identity.id)
                .map_err(|err| err.to_string())?;
            Ok(GeneratedView {
                identity_id: identity.id,
                public_key,
                fingerprint,
            })
        });
        match work {
            Ok(rx) => self.await_result(rx, window, cx, |this, result, _window, cx| {
                this.on_generated(result, cx);
            }),
            Err(err) => self.on_generated(Err(err), cx),
        }
    }

    fn on_generated(&mut self, result: Result<GeneratedView, String>, cx: &mut Context<Self>) {
        self.store.reload();
        let mut picked = None;
        if let Some(dialog) = self.generate_dialog_mut() {
            dialog.running = false;
            match result {
                Ok(view) => {
                    picked = Some((view.identity_id, dialog.nested));
                    dialog.result = Some(view);
                    dialog.error = None;
                }
                Err(err) => dialog.error = Some(err),
            }
        }
        // 嵌在安装对话框里：生成完直接选中这把新密钥。
        if let Some((id, true)) = picked
            && let Some(Modal::InstallKey(install)) = &mut self.modal
        {
            install.key_id = Some(id);
            self.refresh_install_preview();
        }
        cx.notify();
    }

    /// 「完成」：关掉生成对话框；嵌在安装里就回到安装，独立的就在凭据面板里选中新密钥。
    fn finish_generate_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (id, nested) = match self.generate_dialog_mut() {
            Some(dialog) => (
                dialog.result.as_ref().map(|view| view.identity_id),
                dialog.nested,
            ),
            None => return,
        };
        if nested {
            if let Some(Modal::InstallKey(install)) = &mut self.modal {
                install.generate = None;
            }
            cx.notify();
            return;
        }
        self.modal = None;
        if let Some(id) = id {
            self.keychain_select(id, window, cx);
        }
        self.focus_content(window, cx);
        cx.notify();
    }

    pub(super) fn generate_key_body(
        &self,
        dialog: &GenerateKeyDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let done = dialog.result.is_some();
        let cancel = entity.clone();
        let go = entity.clone();
        let (algo_label, algo_hint) = algorithm_info(dialog.algorithm);
        let pick = {
            let entity = entity.clone();
            move |index: usize, _window: &mut Window, cx: &mut App| {
                let Some((algorithm, _, _)) = ALGORITHMS.get(index) else {
                    return;
                };
                let algorithm = *algorithm;
                entity.update(cx, |this, cx| {
                    if let Some(dialog) = this.generate_dialog_mut() {
                        dialog.algorithm = algorithm;
                    }
                    cx.notify();
                });
            }
        };
        let choices: Vec<Choice> = ALGORITHMS
            .iter()
            .map(|(algorithm, label, _)| Choice::new(*label, *algorithm == dialog.algorithm))
            .collect();
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_1p5()
                    .child(
                        div()
                            .typo(Role::GroupTitle, tokens)
                            .child(if done { "密钥已生成" } else { "生成 SSH 密钥" }),
                    )
                    .child(note(
                        if done {
                            "私钥已经加密存进凭据库。把下面的公钥装到服务器上就能用这把密钥登录（主机右键「安装公钥」可以一键装）。"
                        } else {
                            "私钥存在本机凭据库里（Windows 数据保护加密），不会离开这台电脑，除非开了配置同步。"
                        },
                        tokens,
                    )),
            )
            .child(match &dialog.result {
                Some(view) => self.public_key_box(&view.public_key, &view.fingerprint, tokens, cx),
                None => v_flex()
                    .gap_3()
                    .child(field("名称", None, Input::new(&dialog.name), tokens))
                    .child(field(
                        "算法",
                        Some(algo_hint.to_owned()),
                        select("keygen-algorithm", algo_label, choices, pick),
                        tokens,
                    ))
                    .child(field(
                        "注释",
                        Some("写在公钥末尾，方便在服务器上认出是哪把".to_owned()),
                        Input::new(&dialog.comment),
                        tokens,
                    ))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(field(
                                "口令",
                                Some("可选".to_owned()),
                                Input::new(&dialog.passphrase),
                                tokens,
                            )))
                            .child(div().flex_1().child(field(
                                "再输一次",
                                None,
                                Input::new(&dialog.confirm),
                                tokens,
                            ))),
                    )
                    .into_any_element(),
            })
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
                    .when(!done, |d| {
                        d.child(text_button(
                            "modal-cancel",
                            "取消",
                            ButtonLook::Outline,
                            move |_, window, cx| {
                                cancel.update(cx, |this, cx| this.cancel_generate(window, cx));
                            },
                        ))
                    })
                    .child(text_button(
                        "modal-ok",
                        if done {
                            "完成"
                        } else if dialog.running {
                            "生成中…"
                        } else {
                            "生成"
                        },
                        ButtonLook::Primary,
                        move |_, window, cx| {
                            go.update(cx, |this, cx| this.submit_generate_key(window, cx));
                        },
                    )),
            )
            .into_any_element()
    }

    /// 生成对话框的「取消」：嵌在安装里就回到安装，独立的就关掉。
    fn cancel_generate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Modal::InstallKey(install)) = &mut self.modal
            && install.generate.is_some()
        {
            if !install
                .generate
                .as_ref()
                .is_some_and(GenerateKeyDialog::busy)
            {
                install.generate = None;
            }
            cx.notify();
            return;
        }
        self.close_modal(window, cx);
    }

    fn public_key_box(
        &self,
        public_key: &str,
        fingerprint: &str,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let text = public_key.to_owned();
        let entity = cx.entity();
        v_flex()
            .gap_1p5()
            .child(div().typo(Role::FieldLabel, tokens).child("公钥"))
            .child(
                div()
                    .w_full()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(tokens.border)
                    .bg(tokens.surface)
                    .text_xs()
                    .font_family("Consolas")
                    .child(SharedString::from(public_key.to_owned())),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .typo(Role::Caption, tokens)
                            .child(SharedString::from(fingerprint.to_owned())),
                    )
                    .child(text_button(
                        "copy-public-key",
                        "复制公钥",
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                            entity.update(cx, |this, cx| this.notice("已复制公钥", window, cx));
                        },
                    )),
            )
            .into_any_element()
    }

    // ---- 安装公钥 ----

    pub(crate) fn open_install_key_dialog(
        &mut self,
        host_id: i64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.store.host(host_id) else {
            return;
        };
        let key_id = self
            .store
            .identities()
            .iter()
            .find(|item| matches!(item.kind, IdentityKind::KeyInline | IdentityKind::KeyFile))
            .map(|item| item.id);
        self.modal = Some(Modal::InstallKey(InstallKeyDialog {
            host_id,
            host_name: host.name.clone(),
            key_id,
            switch_auth: true,
            running: false,
            preview: None,
            error: None,
            generate: None,
        }));
        self.refresh_install_preview();
        cx.notify();
    }

    /// 选中的密钥变了：重新读它的公钥和指纹。读不出来（口令不对、文件没了）就把原因显示出来。
    fn refresh_install_preview(&mut self) {
        let Some(Modal::InstallKey(install)) = &self.modal else {
            return;
        };
        let Some(id) = install.key_id else {
            if let Some(Modal::InstallKey(install)) = &mut self.modal {
                install.preview = None;
            }
            return;
        };
        let outcome = self
            .store
            .db()
            .and_then(|db| db.identity_public_key(id).map_err(|err| err.to_string()));
        if let Some(Modal::InstallKey(install)) = &mut self.modal {
            match outcome {
                Ok(view) => {
                    install.preview = Some(view);
                    install.error = None;
                }
                Err(err) => {
                    install.preview = None;
                    install.error = Some(err);
                }
            }
        }
    }

    pub(super) fn submit_install_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(&self.modal, Some(Modal::InstallKey(install)) if install.generate.is_some()) {
            self.submit_generate_key(window, cx);
            return;
        }
        let (host_id, host_name, key_id, switch_auth) = match &mut self.modal {
            Some(Modal::InstallKey(install)) if !install.running => {
                let Some(key_id) = install.key_id else {
                    return;
                };
                install.running = true;
                install.error = None;
                (
                    install.host_id,
                    install.host_name.clone(),
                    key_id,
                    install.switch_auth,
                )
            }
            _ => return,
        };
        cx.notify();
        let public_key = match self.store.db().and_then(|db| {
            db.identity_public_key(key_id)
                .map_err(|err| err.to_string())
        }) {
            Ok((public_key, _)) => public_key,
            Err(err) => {
                self.on_installed(Err(err), host_name, key_id, switch_auth, window, cx);
                return;
            }
        };
        let work = super::bridge::on_ssh_runtime(async move {
            let command = zssh_core::ssh::keygen::install_command(&public_key)
                .map_err(|err| err.to_string())?;
            // 用这台主机现在的登录方式连上（池里有就复用；要密码 / 指纹确认照常弹提问）。
            let factory = crate::ssh_backend::factory_for_host(host_id).await?;
            let output = factory
                .connection()
                .exec(&command)
                .await
                .map_err(|err| err.to_string())?;
            zssh_core::ssh::keygen::parse_install_output(&output).map_err(|err| err.to_string())
        });
        match work {
            Ok(rx) => self.await_result(rx, window, cx, move |this, result, window, cx| {
                this.on_installed(result, host_name, key_id, switch_auth, window, cx);
            }),
            Err(err) => self.on_installed(Err(err), host_name, key_id, switch_auth, window, cx),
        }
    }

    fn on_installed(
        &mut self,
        result: Result<InstallOutcome, String>,
        host_name: String,
        key_id: i64,
        switch_auth: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host_id = match &mut self.modal {
            Some(Modal::InstallKey(install)) => {
                install.running = false;
                install.host_id
            }
            _ => return,
        };
        match result {
            Ok(outcome) => {
                let mut switched = false;
                if switch_auth && let Some(host) = self.store.host(host_id).cloned() {
                    let mut draft = draft_of_host(&host);
                    draft.auth_method = AuthMethod::Key;
                    draft.identity_id = Some(key_id);
                    match self.store.save_host(&draft) {
                        Ok(_) => switched = true,
                        Err(err) => {
                            self.notice_error(
                                &format!("公钥装好了，但没能改成密钥登录：{err}"),
                                window,
                                cx,
                            );
                        }
                    }
                }
                self.modal = None;
                self.focus_content(window, cx);
                self.notice(&install_message(outcome, &host_name, switched), window, cx);
            }
            Err(err) => {
                if let Some(Modal::InstallKey(install)) = &mut self.modal {
                    install.error = Some(format!("安装公钥失败：{err}"));
                }
            }
        }
        cx.notify();
    }

    pub(super) fn install_key_body(
        &self,
        dialog: &InstallKeyDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(generate) = &dialog.generate {
            return self.generate_key_body(generate, tokens, cx);
        }
        let entity = cx.entity();
        let keys: Vec<(i64, String)> = self
            .store
            .identities()
            .iter()
            .filter(|item| matches!(item.kind, IdentityKind::KeyInline | IdentityKind::KeyFile))
            .map(|item| (item.id, item.name.clone()))
            .collect();
        let current = dialog
            .key_id
            .and_then(|id| keys.iter().find(|(key, _)| *key == id))
            .map_or_else(|| "选择一把密钥".to_owned(), |(_, name)| name.clone());
        let choices: Vec<Choice> = keys
            .iter()
            .map(|(id, name)| Choice::new(name.clone(), Some(*id) == dialog.key_id))
            .collect();
        let ids: Vec<i64> = keys.iter().map(|(id, _)| *id).collect();
        let pick = {
            let entity = entity.clone();
            move |index: usize, _window: &mut Window, cx: &mut App| {
                let Some(id) = ids.get(index).copied() else {
                    return;
                };
                entity.update(cx, |this, cx| {
                    if let Some(Modal::InstallKey(install)) = &mut this.modal {
                        install.key_id = Some(id);
                    }
                    this.refresh_install_preview();
                    cx.notify();
                });
            }
        };
        let cancel = entity.clone();
        let go = entity.clone();
        let new_key = entity.clone();
        let toggle = entity.clone();
        let busy = dialog.busy();
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_1p5()
                    .child(
                        div()
                            .typo(Role::GroupTitle, tokens)
                            .child(SharedString::from(format!("把公钥装到「{}」", dialog.host_name))),
                    )
                    .child(note(
                        "用这台主机现在的登录方式连上，把公钥追加到 ~/.ssh/authorized_keys（已经有就不重复加）。",
                        tokens,
                    )),
            )
            .child(field(
                "用哪把密钥",
                None,
                v_flex()
                    .gap_2()
                    .child(if keys.is_empty() {
                        note("凭据库里还没有私钥。", tokens).into_any_element()
                    } else {
                        select("install-key-pick", current, choices, pick).into_any_element()
                    })
                    .child(text_button(
                        "install-generate",
                        "生成一把新密钥…",
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            new_key.update(cx, |this, cx| this.open_nested_generate(window, cx));
                        },
                    )),
                tokens,
            ))
            .children(dialog.preview.as_ref().map(|(public_key, fingerprint)| {
                self.public_key_box(public_key, fingerprint, tokens, cx)
            }))
            .child(switch_row(
                "install-switch-auth",
                "装好后这台主机改用密钥登录",
                Some("认证方式改成私钥，凭据选这把。".to_owned()),
                dialog.switch_auth,
                move |_window, cx| {
                    toggle.update(cx, |this, cx| {
                        if let Some(Modal::InstallKey(install)) = &mut this.modal {
                            install.switch_auth = !install.switch_auth;
                        }
                        cx.notify();
                    });
                },
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
                        if busy { "安装中…" } else { "安装" },
                        ButtonLook::Primary,
                        move |_, window, cx| {
                            go.update(cx, |this, cx| this.submit_install_key(window, cx));
                        },
                    )),
            )
            .into_any_element()
    }

    fn open_nested_generate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dialog = GenerateKeyDialog::new(window, cx, true);
        let focus = dialog.name.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        if let Some(Modal::InstallKey(install)) = &mut self.modal {
            install.generate = Some(dialog);
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{ALGORITHMS, algorithm_info, check_generate_input, install_message};
    use zssh_core::ssh::keygen::InstallOutcome;

    #[test]
    fn the_generate_form_needs_a_name_and_matching_passphrases() {
        assert_eq!(
            check_generate_input("  ", "", ""),
            Err("名称不能为空".to_owned())
        );
        assert_eq!(
            check_generate_input("key", "a", "b"),
            Err("两次输入的口令不一样".to_owned())
        );
        assert_eq!(check_generate_input("key", "", ""), Ok(()), "口令可以不要");
        assert_eq!(check_generate_input("key", "pw", "pw"), Ok(()));
    }

    #[test]
    fn install_messages_say_whether_the_key_was_new_and_whether_auth_switched() {
        assert_eq!(
            install_message(InstallOutcome::Added, "web", true),
            "公钥已装到「web」上，这台主机改用密钥登录"
        );
        assert_eq!(
            install_message(InstallOutcome::AlreadyPresent, "web", false),
            "公钥本来就在「web」上"
        );
    }

    #[test]
    fn every_algorithm_has_a_label_and_a_hint() {
        for (algorithm, label, hint) in ALGORITHMS {
            assert_eq!(algorithm_info(algorithm), (label, hint));
            assert!(!label.is_empty() && !hint.is_empty());
        }
    }
}
