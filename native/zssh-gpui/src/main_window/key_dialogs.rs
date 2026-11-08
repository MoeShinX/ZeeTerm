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
use super::modal::Modal;
use super::widgets::{
    ButtonLook, Choice, error_note, field, note, select, switch_row, text_button,
};
use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::typography::{Role, Typo as _};

/// 算法下拉里的三项：算法、名字、一句话说明。
///
/// 界面上显示的文字按语言取（见 [`key_dialogs::algorithm_info`]），这里的名字和说明是英文原文，
/// 只作为算法清单和测试的基准；中文在 `KeyDialogMsg::zh` 里。
pub const ALGORITHMS: [(KeyAlgorithm, &str, &str); 3] = [
    (
        KeyAlgorithm::Ed25519,
        "Ed25519 (recommended)",
        "Short and fast; supported by current servers",
    ),
    (
        KeyAlgorithm::EcdsaP256,
        "ECDSA P-256",
        "Some devices or compliance rules only accept ECDSA",
    ),
    (
        KeyAlgorithm::Rsa4096,
        "RSA 4096",
        "Works with very old servers; generation takes a few seconds",
    ),
];

/// 生成 / 安装密钥对话框的界面文案。
///
/// 密钥内容、指纹、主机名、核心库的错误原文一律原样显示；算法名（Ed25519、ECDSA P-256、
/// RSA 4096）是协议名，不翻译，只翻译括号里的注解和说明。
#[derive(Clone, Copy, Debug)]
enum KeyDialogMsg {
    AlgorithmEd25519,
    AlgorithmEd25519Hint,
    AlgorithmEcdsa,
    AlgorithmEcdsaHint,
    AlgorithmRsa,
    AlgorithmRsaHint,
    DefaultKeyName,
    NameRequired,
    PassphraseMismatch,
    InstallAddedPrefix,
    InstallPresentPrefix,
    InstallHostSuffix,
    InstallSwitchedSuffix,
    GeneratedTitle,
    GenerateTitle,
    GeneratedNote,
    GenerateNote,
    NameLabel,
    AlgorithmLabel,
    CommentLabel,
    CommentHint,
    PassphraseLabel,
    Optional,
    ConfirmLabel,
    Cancel,
    Done,
    Generating,
    Generate,
    PublicKey,
    CopyPublicKey,
    CopiedPublicKey,
    InstallTitlePrefix,
    InstallTitleSuffix,
    InstallNote,
    KeyPickLabel,
    NoKeys,
    ChooseKey,
    GenerateNew,
    SwitchRow,
    SwitchHint,
    Installing,
    Install,
    GenerateFailedPrefix,
    SwitchAuthFailedPrefix,
    InstallFailedPrefix,
}

impl Msg for KeyDialogMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::AlgorithmEd25519 => "Ed25519（推荐）",
            Self::AlgorithmEd25519Hint => "短、快，现在的服务器都支持",
            Self::AlgorithmEcdsa => "ECDSA P-256",
            Self::AlgorithmEcdsaHint => "部分设备或合规要求只认 ECDSA",
            Self::AlgorithmRsa => "RSA 4096",
            Self::AlgorithmRsaHint => "兼容很老的服务器，生成要几秒",
            Self::DefaultKeyName => "我的密钥",
            Self::NameRequired => "名称不能为空",
            Self::PassphraseMismatch => "两次输入的口令不一样",
            Self::InstallAddedPrefix => "公钥已装到「",
            Self::InstallPresentPrefix => "公钥本来就在「",
            Self::InstallHostSuffix => "」上",
            Self::InstallSwitchedSuffix => "，这台主机改用密钥登录",
            Self::GeneratedTitle => "密钥已生成",
            Self::GenerateTitle => "生成 SSH 密钥",
            Self::GeneratedNote => {
                "私钥已经加密存进凭据库。把下面的公钥装到服务器上就能用这把密钥登录（主机右键「安装公钥」可以一键装）。"
            }
            Self::GenerateNote if !cfg!(windows) => {
                "私钥存在本机凭据库里（用本机的资料库密钥加密，密钥在系统钥匙串里），不会离开这台电脑，除非开了配置同步。"
            }
            Self::GenerateNote => {
                "私钥存在本机凭据库里（Windows 数据保护加密），不会离开这台电脑，除非开了配置同步。"
            }
            Self::NameLabel => "名称",
            Self::AlgorithmLabel => "算法",
            Self::CommentLabel => "注释",
            Self::CommentHint => "写在公钥末尾，方便在服务器上认出是哪把",
            Self::PassphraseLabel => "口令",
            Self::Optional => "可选",
            Self::ConfirmLabel => "再输一次",
            Self::Cancel => "取消",
            Self::Done => "完成",
            Self::Generating => "生成中…",
            Self::Generate => "生成",
            Self::PublicKey => "公钥",
            Self::CopyPublicKey => "复制公钥",
            Self::CopiedPublicKey => "已复制公钥",
            Self::InstallTitlePrefix => "把公钥装到「",
            Self::InstallTitleSuffix => "」",
            Self::InstallNote => {
                "用这台主机现在的登录方式连上，把公钥追加到 ~/.ssh/authorized_keys（已经有就不重复加）。"
            }
            Self::KeyPickLabel => "用哪把密钥",
            Self::NoKeys => "凭据库里还没有私钥。",
            Self::ChooseKey => "选择一把密钥",
            Self::GenerateNew => "生成一把新密钥…",
            Self::SwitchRow => "装好后这台主机改用密钥登录",
            Self::SwitchHint => "认证方式改成私钥，凭据选这把。",
            Self::Installing => "安装中…",
            Self::Install => "安装",
            Self::GenerateFailedPrefix => "生成失败：",
            Self::SwitchAuthFailedPrefix => "公钥装好了，但没能改成密钥登录：",
            Self::InstallFailedPrefix => "安装公钥失败：",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::AlgorithmEd25519 => "Ed25519 (recommended)",
            Self::AlgorithmEd25519Hint => "Short and fast; current servers all support it",
            Self::AlgorithmEcdsa => "ECDSA P-256",
            Self::AlgorithmEcdsaHint => "Some devices or compliance policies only accept ECDSA",
            Self::AlgorithmRsa => "RSA 4096",
            Self::AlgorithmRsaHint => "Works with very old servers; generation takes a few seconds",
            Self::DefaultKeyName => "My key",
            Self::NameRequired => "Name cannot be empty",
            Self::PassphraseMismatch => "The two passphrases do not match",
            Self::InstallAddedPrefix => "Public key installed on \"",
            Self::InstallPresentPrefix => "The public key was already on \"",
            Self::InstallHostSuffix => "\"",
            Self::InstallSwitchedSuffix => "; this host now signs in with the key",
            Self::GeneratedTitle => "Key Generated",
            Self::GenerateTitle => "Generate SSH Key",
            Self::GeneratedNote => {
                "The private key has been encrypted and stored in the credential library. Install the public key below on the server to sign in with it (right-click the host and choose \"Install Public Key\" for one-click installation)."
            }
            Self::GenerateNote if !cfg!(windows) => {
                "The private key is stored in the local credential library (encrypted with this computer's data key, which is kept in the system keychain) and never leaves this computer unless configuration sync is enabled."
            }
            Self::GenerateNote => {
                "The private key is stored in the local credential library (encrypted with Windows Data Protection) and never leaves this computer unless configuration sync is enabled."
            }
            Self::NameLabel => "Name",
            Self::AlgorithmLabel => "Algorithm",
            Self::CommentLabel => "Comment",
            Self::CommentHint => "Appended to the public key so you can recognize it on the server",
            Self::PassphraseLabel => "Passphrase",
            Self::Optional => "Optional",
            Self::ConfirmLabel => "Enter it again",
            Self::Cancel => "Cancel",
            Self::Done => "Done",
            Self::Generating => "Generating…",
            Self::Generate => "Generate",
            Self::PublicKey => "Public key",
            Self::CopyPublicKey => "Copy Public Key",
            Self::CopiedPublicKey => "Public key copied",
            Self::InstallTitlePrefix => "Install the public key on \"",
            Self::InstallTitleSuffix => "\"",
            Self::InstallNote => {
                "Connect using this host's current sign-in method and append the public key to ~/.ssh/authorized_keys (it is not added twice)."
            }
            Self::KeyPickLabel => "Which key to use",
            Self::NoKeys => "There are no private keys in the credential library yet.",
            Self::ChooseKey => "Choose a key",
            Self::GenerateNew => "Generate a New Key…",
            Self::SwitchRow => "Switch this host to key sign-in after installing",
            Self::SwitchHint => "Set authentication to the private key and select this credential.",
            Self::Installing => "Installing…",
            Self::Install => "Install",
            Self::GenerateFailedPrefix => "Generation failed: ",
            Self::SwitchAuthFailedPrefix => {
                "The public key was installed, but switching to key sign-in failed: "
            }
            Self::InstallFailedPrefix => "Installing the public key failed: ",
        }
    }
}

pub fn algorithm_info(algorithm: KeyAlgorithm) -> (&'static str, &'static str) {
    match algorithm {
        KeyAlgorithm::Ed25519 => (
            t(KeyDialogMsg::AlgorithmEd25519),
            t(KeyDialogMsg::AlgorithmEd25519Hint),
        ),
        KeyAlgorithm::EcdsaP256 => (
            t(KeyDialogMsg::AlgorithmEcdsa),
            t(KeyDialogMsg::AlgorithmEcdsaHint),
        ),
        KeyAlgorithm::Rsa4096 => (
            t(KeyDialogMsg::AlgorithmRsa),
            t(KeyDialogMsg::AlgorithmRsaHint),
        ),
    }
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
            name: cx.new(|cx| {
                InputState::new(window, cx).default_value(t(KeyDialogMsg::DefaultKeyName))
            }),
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
        return Err(t(KeyDialogMsg::NameRequired).to_owned());
    }
    if passphrase != confirm {
        return Err(t(KeyDialogMsg::PassphraseMismatch).to_owned());
    }
    Ok(())
}

/// 安装结果的提示文字。
pub fn install_message(outcome: InstallOutcome, host_name: &str, switched: bool) -> String {
    format!(
        "{}{host_name}{}{}",
        match outcome {
            InstallOutcome::Added => t(KeyDialogMsg::InstallAddedPrefix),
            InstallOutcome::AlreadyPresent => t(KeyDialogMsg::InstallPresentPrefix),
        },
        t(KeyDialogMsg::InstallHostSuffix),
        if switched {
            t(KeyDialogMsg::InstallSwitchedSuffix)
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
            let db = crate::host_db::open_database()
                .map_err(|err| crate::error_text::error_text(&err))?;
            let passphrase = (!passphrase.is_empty()).then_some(passphrase.as_str());
            let identity = db
                .identity_generate(name.trim(), algorithm, &comment, passphrase)
                .map_err(|err| {
                    format!(
                        "{}{}",
                        t(KeyDialogMsg::GenerateFailedPrefix),
                        crate::error_text::error_text(&err)
                    )
                })?;
            let (public_key, fingerprint) = db
                .identity_public_key(identity.id)
                .map_err(|err| crate::error_text::error_text(&err))?;
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
            .map(|(algorithm, _, _)| {
                Choice::new(algorithm_info(*algorithm).0, *algorithm == dialog.algorithm)
            })
            .collect();
        v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_1p5()
                    .child(div().typo(Role::GroupTitle, tokens).child(if done {
                        t(KeyDialogMsg::GeneratedTitle)
                    } else {
                        t(KeyDialogMsg::GenerateTitle)
                    }))
                    .child(note(
                        if done {
                            t(KeyDialogMsg::GeneratedNote)
                        } else {
                            t(KeyDialogMsg::GenerateNote)
                        },
                        tokens,
                    )),
            )
            .child(match &dialog.result {
                Some(view) => self.public_key_box(&view.public_key, &view.fingerprint, tokens, cx),
                None => v_flex()
                    .gap_3()
                    .child(field(
                        t(KeyDialogMsg::NameLabel),
                        None,
                        Input::new(&dialog.name),
                        tokens,
                    ))
                    .child(field(
                        t(KeyDialogMsg::AlgorithmLabel),
                        Some(algo_hint.to_owned()),
                        select("keygen-algorithm", algo_label, choices, pick),
                        tokens,
                    ))
                    .child(field(
                        t(KeyDialogMsg::CommentLabel),
                        Some(t(KeyDialogMsg::CommentHint).to_owned()),
                        Input::new(&dialog.comment),
                        tokens,
                    ))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(field(
                                t(KeyDialogMsg::PassphraseLabel),
                                Some(t(KeyDialogMsg::Optional).to_owned()),
                                Input::new(&dialog.passphrase),
                                tokens,
                            )))
                            .child(div().flex_1().child(field(
                                t(KeyDialogMsg::ConfirmLabel),
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
                            t(KeyDialogMsg::Cancel),
                            ButtonLook::Outline,
                            move |_, window, cx| {
                                cancel.update(cx, |this, cx| this.cancel_generate(window, cx));
                            },
                        ))
                    })
                    .child(text_button(
                        "modal-ok",
                        if done {
                            t(KeyDialogMsg::Done)
                        } else if dialog.running {
                            t(KeyDialogMsg::Generating)
                        } else {
                            t(KeyDialogMsg::Generate)
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
            .child(
                div()
                    .typo(Role::FieldLabel, tokens)
                    .child(t(KeyDialogMsg::PublicKey)),
            )
            .child(
                div()
                    .w_full()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(tokens.border)
                    .bg(tokens.surface)
                    .text_xs()
                    .font_family(crate::fonts::mono_text_family())
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
                        t(KeyDialogMsg::CopyPublicKey),
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                            entity.update(cx, |this, cx| {
                                this.notice(t(KeyDialogMsg::CopiedPublicKey), window, cx)
                            });
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
        let outcome = self.store.db().and_then(|db| {
            db.identity_public_key(id)
                .map_err(|err| crate::error_text::error_text(&err))
        });
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
                .map_err(|err| crate::error_text::error_text(&err))
        }) {
            Ok((public_key, _)) => public_key,
            Err(err) => {
                self.on_installed(Err(err), host_name, key_id, switch_auth, window, cx);
                return;
            }
        };
        let work = super::bridge::on_ssh_runtime(async move {
            let command = zssh_core::ssh::keygen::install_command(&public_key)
                .map_err(|err| crate::error_text::error_text(&err))?;
            // 用这台主机现在的登录方式连上（池里有就复用；要密码 / 指纹确认照常弹提问）。
            let factory = crate::ssh_backend::factory_for_host(host_id).await?;
            // 装公钥的命令是 POSIX sh 写法：走 stdin 交给 `sh -s`，登录 shell 是 fish / csh 也能装。
            let output = factory
                .connection()
                .exec_script(&command, std::time::Duration::from_secs(30))
                .await
                .map_err(|err| crate::error_text::error_text(&err))?;
            zssh_core::ssh::keygen::parse_install_output(&output)
                .map_err(|err| crate::error_text::error_text(&err))
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
                if switch_auth {
                    // 只改认证两列、以库里最新的主机为准：界面缓存可能比库旧（同步、别的窗口改过）。
                    match self
                        .store
                        .set_host_auth(host_id, AuthMethod::Key, Some(key_id))
                    {
                        Ok(_) => switched = true,
                        Err(err) => {
                            self.notice_error(
                                &format!("{}{err}", t(KeyDialogMsg::SwitchAuthFailedPrefix)),
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
                    install.error = Some(format!("{}{err}", t(KeyDialogMsg::InstallFailedPrefix)));
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
            .map_or_else(
                || t(KeyDialogMsg::ChooseKey).to_owned(),
                |(_, name)| name.clone(),
            );
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
                            .child(SharedString::from(format!(
                                "{}{}{}",
                                t(KeyDialogMsg::InstallTitlePrefix),
                                dialog.host_name,
                                t(KeyDialogMsg::InstallTitleSuffix)
                            ))),
                    )
                    .child(note(t(KeyDialogMsg::InstallNote), tokens)),
            )
            .child(field(
                t(KeyDialogMsg::KeyPickLabel),
                None,
                v_flex()
                    .gap_2()
                    .child(if keys.is_empty() {
                        note(t(KeyDialogMsg::NoKeys), tokens).into_any_element()
                    } else {
                        select("install-key-pick", current, choices, pick).into_any_element()
                    })
                    .child(text_button(
                        "install-generate",
                        t(KeyDialogMsg::GenerateNew),
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
                t(KeyDialogMsg::SwitchRow),
                Some(t(KeyDialogMsg::SwitchHint).to_owned()),
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
                        t(KeyDialogMsg::Cancel),
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            cancel.update(cx, |this, cx| this.close_modal(window, cx));
                        },
                    ))
                    .child(text_button(
                        "modal-ok",
                        if busy {
                            t(KeyDialogMsg::Installing)
                        } else {
                            t(KeyDialogMsg::Install)
                        },
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
        // 清单里的名字 / 说明（英文原文）非空，当前语言下取到的也一样。
        for (algorithm, label, hint) in ALGORITHMS {
            assert!(!label.is_empty() && !hint.is_empty(), "清单里缺名字或说明");
            let (shown, shown_hint) = algorithm_info(algorithm);
            assert!(!shown.is_empty(), "当前语言下没有算法名");
            assert!(!shown_hint.is_empty(), "当前语言下没有算法说明");
        }
    }
}
