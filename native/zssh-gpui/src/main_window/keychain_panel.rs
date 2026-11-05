//! 凭据（0.2.14 `KeychainPanel.tsx`）：列表（新建的在最上面）+ 选中后展开的编辑表单；
//! 类型有密码、私钥文件、导入的私钥、ssh-agent；私钥文件路径可以从文件管理器选；可以生成密钥、
//! 复制公钥、删除（V2-F3：还有主机在用就不给删、清除已存秘密要确认）。
//!
//! V2-U3 起它是「管理 + 设置」页的「凭据」分类（不再是右侧面板）：同样交出 [`PanelContent`]，
//! 由 `settings_page` 画在整页里，页面上方有类型筛选和搜索框（只过滤列表，不动数据）。

use ::gpui::{ClipboardItem, Entity, PathPromptOptions, SharedString};
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::model::{Host, Identity, IdentityKind};

use super::MainWindow;
use super::identity_form::{
    IdentityInput, build_draft, can_keep_passphrase, kind_change_clears_secret, kind_label, kinds,
};
use super::modal::{Confirm, ConfirmAction, Modal};
use super::side_panel::PanelContent;
use super::widgets::{
    BadgeKind, ButtonLook, Choice, act, badge, empty_state, field, note, pick, section, select,
    switch_row, text_button,
};
use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::typography::{Role, Typo as _};

/// 「凭据」管理页自己的界面文案。
///
/// 只放应用自己的固定文案；凭据名、路径、主机名、核心库的错误原文一律原样显示。
#[derive(Clone, Copy, Debug)]
enum KeychainMsg {
    HostsPrefix,
    ListSeparator,
    OthersMiddle,
    OthersSuffix,
    BadgeStored,
    BadgeNoSecret,
    EmptyListTitle,
    EmptyListDetail,
    EmptyNewPassword,
    NoMatches,
    NameLabel,
    KindLabel,
    KeyPathLabel,
    KeyBodyLabel,
    PassphraseLabel,
    PasswordLabel,
    KeepExisting,
    PasteKey,
    EmptyPassphrase,
    KindClearedPrefix,
    KindClearedSuffix,
    ClearPassphraseRow,
    Browse,
    EditTitle,
    NewTitle,
    DeleteButton,
    ClearSecretButton,
    CopyPublicKey,
    ResetForm,
    Save,
    PanelTitle,
    ItemCountSuffix,
    NewPassword,
    KeyFile,
    ImportKey,
    GenerateKey,
    CantCheckHostsTitle,
    ReadHostsFailedPrefix,
    DeleteTitlePrefix,
    DeleteTitleSuffix,
    CantDeleteTitlePrefix,
    CantDeleteTitleSuffix,
    DeleteConfirmMessage,
    Delete,
    CantClearTitle,
    ClearTitlePrefix,
    ClearTitleSuffix,
    ClearConfirmMessage,
    Clear,
    ClearedSecretPrefix,
    ClearedSecretSuffix,
    CopiedPublicKey,
    ReadPublicKeyFailedPrefix,
    KeyPathPlaceholder,
}

impl Msg for KeychainMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::HostsPrefix => " · 主机：",
            Self::ListSeparator => "、",
            Self::OthersMiddle => " 等 ",
            Self::OthersSuffix => " 台",
            Self::BadgeStored => "已保存",
            Self::BadgeNoSecret => "无密码",
            Self::EmptyListTitle => "暂无凭据",
            Self::EmptyListDetail => "在主机属性里填密码时勾选「存成凭据」，也会出现在这里。",
            Self::EmptyNewPassword => "新建密码",
            Self::NoMatches => "没有匹配的凭据。",
            Self::NameLabel => "名称",
            Self::KindLabel => "类型",
            Self::KeyPathLabel => "私钥文件路径",
            Self::KeyBodyLabel => "私钥内容",
            Self::PassphraseLabel => "私钥口令",
            Self::PasswordLabel => "密码",
            Self::KeepExisting => "留空表示不改",
            Self::PasteKey => "粘贴私钥文本",
            Self::EmptyPassphrase => "私钥没有口令就留空",
            Self::KindClearedPrefix => "改了类型：原来保存的",
            Self::KindClearedSuffix => "内容不会带过来，保存时会清掉；需要的话在下面重新填写。",
            Self::ClearPassphraseRow => "清除已保存的口令（私钥没有口令时用）",
            Self::Browse => "浏览…",
            Self::EditTitle => "编辑凭据",
            Self::NewTitle => "新建凭据",
            Self::DeleteButton => "删除",
            Self::ClearSecretButton => "清除秘密",
            Self::CopyPublicKey => "复制公钥",
            Self::ResetForm => "清空",
            Self::Save => "保存",
            Self::PanelTitle => "凭据",
            Self::ItemCountSuffix => " 条",
            Self::NewPassword => "新建密码",
            Self::KeyFile => "私钥文件",
            Self::ImportKey => "导入私钥",
            Self::GenerateKey => "生成密钥",
            Self::CantCheckHostsTitle => "不能确认有没有主机在用它",
            Self::ReadHostsFailedPrefix => "读取主机失败：",
            Self::DeleteTitlePrefix => "删除凭据「",
            Self::DeleteTitleSuffix => "」？",
            Self::CantDeleteTitlePrefix => "不能删除凭据「",
            Self::CantDeleteTitleSuffix => "」",
            Self::DeleteConfirmMessage => "凭据里保存的密码或私钥会一起删掉，不能恢复。",
            Self::Delete => "删除",
            Self::CantClearTitle => "不能清除",
            Self::ClearTitlePrefix => "清除「",
            Self::ClearTitleSuffix => "」已存的秘密？",
            Self::ClearConfirmMessage => {
                "只清掉保存的密码 / 口令，名称、类型、路径不动；不能恢复。"
            }
            Self::Clear => "清除",
            Self::ClearedSecretPrefix => "已清除「",
            Self::ClearedSecretSuffix => "」已存的秘密",
            Self::CopiedPublicKey => "已复制公钥",
            Self::ReadPublicKeyFailedPrefix => "读取公钥失败：",
            Self::KeyPathPlaceholder => "私钥文件的完整路径",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::HostsPrefix => " · Hosts: ",
            Self::ListSeparator => ", ",
            Self::OthersMiddle => " and ",
            Self::OthersSuffix => " in total",
            Self::BadgeStored => "Saved",
            Self::BadgeNoSecret => "No secret",
            Self::EmptyListTitle => "No credentials yet",
            Self::EmptyListDetail => {
                "Passwords saved from host properties with \"Save as credential\" also appear here."
            }
            Self::EmptyNewPassword => "New Password",
            Self::NoMatches => "No matching credentials.",
            Self::NameLabel => "Name",
            Self::KindLabel => "Type",
            Self::KeyPathLabel => "Key file path",
            Self::KeyBodyLabel => "Key contents",
            Self::PassphraseLabel => "Key passphrase",
            Self::PasswordLabel => "Password",
            Self::KeepExisting => "Leave blank to keep",
            Self::PasteKey => "Paste the private key",
            Self::EmptyPassphrase => "Leave blank if the key has no passphrase",
            Self::KindClearedPrefix => "Type changed: the previously stored ",
            Self::KindClearedSuffix => {
                " content will not be carried over and is cleared on save; enter it again below if needed."
            }
            Self::ClearPassphraseRow => "Clear the saved passphrase (for keys without one)",
            Self::Browse => "Browse…",
            Self::EditTitle => "Edit Credential",
            Self::NewTitle => "New Credential",
            Self::DeleteButton => "Delete",
            Self::ClearSecretButton => "Clear Secret",
            Self::CopyPublicKey => "Copy Public Key",
            Self::ResetForm => "Clear",
            Self::Save => "Save",
            Self::PanelTitle => "Credentials",
            Self::ItemCountSuffix => " items",
            Self::NewPassword => "New Password",
            Self::KeyFile => "Key File",
            Self::ImportKey => "Import Key",
            Self::GenerateKey => "Generate Key",
            Self::CantCheckHostsTitle => "Cannot check whether any host uses it",
            Self::ReadHostsFailedPrefix => "Could not read hosts: ",
            Self::DeleteTitlePrefix => "Delete credential \"",
            Self::DeleteTitleSuffix => "\"?",
            Self::CantDeleteTitlePrefix => "Cannot delete credential \"",
            Self::CantDeleteTitleSuffix => "\"",
            Self::DeleteConfirmMessage => {
                "The password or private key stored in the credential is deleted too and cannot be recovered."
            }
            Self::Delete => "Delete",
            Self::CantClearTitle => "Cannot clear",
            Self::ClearTitlePrefix => "Clear the secret stored in \"",
            Self::ClearTitleSuffix => "\"?",
            Self::ClearConfirmMessage => {
                "Only the saved password / passphrase is cleared; name, type, and path stay. This cannot be undone."
            }
            Self::Clear => "Clear",
            Self::ClearedSecretPrefix => "Cleared the secret stored in \"",
            Self::ClearedSecretSuffix => "\"",
            Self::CopiedPublicKey => "Public key copied",
            Self::ReadPublicKeyFailedPrefix => "Could not read the public key: ",
            Self::KeyPathPlaceholder => "Full path to the private key file",
        }
    }
}

/// 凭据面板的界面状态。
pub struct KeychainUi {
    /// 正在编辑的凭据。
    pub selected: Option<i64>,
    /// 点了「新建」、正在填一条新凭据（这时既没有选中项，名称也还是空的）。
    pub creating: bool,
    pub name: Entity<InputState>,
    pub kind: IdentityKind,
    pub key_path: Entity<InputState>,
    pub secret: Entity<InputState>,
    pub passphrase: Entity<InputState>,
    pub inline_key: Entity<TextareaState>,
    pub clear_passphrase: bool,
    pub error: Option<String>,
    /// 列表只看这一种类型；`None` 看全部（页面上的筛选下拉）。
    pub kind_filter: Option<IdentityKind>,
}

impl KeychainUi {
    pub fn new(window: &mut Window, cx: &mut Context<MainWindow>) -> Self {
        Self {
            selected: None,
            creating: false,
            name: cx.new(|cx| InputState::new(window, cx)),
            kind: IdentityKind::Password,
            key_path: cx.new(|cx| {
                InputState::new(window, cx).placeholder(t(KeychainMsg::KeyPathPlaceholder))
            }),
            secret: cx.new(|cx| InputState::new(window, cx).masked(true)),
            passphrase: cx.new(|cx| InputState::new(window, cx).masked(true)),
            inline_key: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder("-----BEGIN OPENSSH PRIVATE KEY-----")
                    .auto_grow(4, 8)
            }),
            clear_passphrase: false,
            error: None,
            kind_filter: None,
        }
    }

    pub fn editing(&self) -> bool {
        self.creating || self.selected.is_some()
    }
}

/// 页面上的筛选 + 搜索：类型对得上，并且名称 / 类型名 / 私钥路径里含搜索词（不分大小写）。
pub fn identity_matches(identity: &Identity, kind: Option<IdentityKind>, query: &str) -> bool {
    if kind.is_some_and(|wanted| wanted != identity.kind) {
        return false;
    }
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    identity.name.to_lowercase().contains(&query)
        || kind_label(identity.kind).to_lowercase().contains(&query)
        || identity
            .key_path
            .as_deref()
            .is_some_and(|path| path.to_lowercase().contains(&query))
}

/// 列表里一条凭据的副标题：类型（私钥带路径）；密码身份再带上用它的主机（V2-U11：主机的密码归主机，
/// 在这里能认出是哪台的）。只读显示，三台以上写前两台和总数。
pub fn identity_detail(identity: &Identity, hosts: &[Host]) -> String {
    let mut detail = kind_label(identity.kind).to_owned();
    if let Some(path) = &identity.key_path {
        detail.push_str(&format!(" · {path}"));
    }
    if identity.kind == IdentityKind::Password {
        let users: Vec<&str> = hosts
            .iter()
            .filter(|host| host.identity_id == Some(identity.id))
            .map(|host| host.name.as_str())
            .collect();
        match users.as_slice() {
            [] => {}
            [one] => detail.push_str(&format!("{}{one}", t(KeychainMsg::HostsPrefix))),
            [one, two] => detail.push_str(&format!(
                "{}{one}{}{two}",
                t(KeychainMsg::HostsPrefix),
                t(KeychainMsg::ListSeparator)
            )),
            [first, second, ..] => detail.push_str(&format!(
                "{}{first}{}{second}{}{}{}",
                t(KeychainMsg::HostsPrefix),
                t(KeychainMsg::ListSeparator),
                t(KeychainMsg::OthersMiddle),
                users.len(),
                t(KeychainMsg::OthersSuffix)
            )),
        }
    }
    detail
}

impl MainWindow {
    /// 清掉表单并收起（保存、删除之后，或者点「清空」）。
    pub(crate) fn keychain_reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ui = &mut self.keychain;
        ui.creating = false;
        ui.selected = None;
        ui.kind = IdentityKind::Password;
        ui.clear_passphrase = false;
        ui.error = None;
        let (name, path, secret, passphrase, inline) = (
            ui.name.clone(),
            ui.key_path.clone(),
            ui.secret.clone(),
            ui.passphrase.clone(),
            ui.inline_key.clone(),
        );
        name.update(cx, |s, cx| s.set_value("", window, cx));
        path.update(cx, |s, cx| s.set_value("", window, cx));
        secret.update(cx, |s, cx| s.set_value("", window, cx));
        passphrase.update(cx, |s, cx| s.set_value("", window, cx));
        inline.update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    /// 「新建」：空表单，并且显示出来。
    pub(crate) fn keychain_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.keychain_new_kind(IdentityKind::Password, window, cx);
    }

    /// 页面右上角的「新建密码 / 私钥文件 / 导入私钥」：空表单，类型先选好。
    pub(crate) fn keychain_new_kind(
        &mut self,
        kind: IdentityKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.keychain_reset(window, cx);
        self.keychain.kind = kind;
        self.keychain.creating = true;
        let focus = self.keychain.name.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// 选中列表里的一条：表单填上它的名称、类型、路径；秘密框一律留空（不回显）。
    pub(crate) fn keychain_pick(
        &mut self,
        identity: &Identity,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.keychain_reset(window, cx);
        let ui = &mut self.keychain;
        ui.selected = Some(identity.id);
        ui.kind = identity.kind;
        let (name, path) = (ui.name.clone(), ui.key_path.clone());
        let (name_text, path_text) = (
            identity.name.clone(),
            identity.key_path.clone().unwrap_or_default(),
        );
        name.update(cx, |s, cx| s.set_value(name_text, window, cx));
        path.update(cx, |s, cx| s.set_value(path_text, window, cx));
        cx.notify();
    }

    /// 生成完密钥后在列表里选中它（并让「管理 + 设置」页停在「凭据」）。
    pub(crate) fn keychain_select(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        self.store.reload();
        if let Some(identity) = self
            .store
            .identities()
            .iter()
            .find(|item| item.id == id)
            .cloned()
        {
            self.open_settings_section(
                crate::settings_nav::SettingsSection::Credentials,
                window,
                cx,
            );
            self.keychain_pick(&identity, window, cx);
        }
    }

    pub(crate) fn keychain_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editing = self.keychain.selected.and_then(|id| {
            self.store
                .identities()
                .iter()
                .find(|item| item.id == id)
                .cloned()
        });
        let name = self.keychain.name.read(cx).value().to_string();
        let key_path = self.keychain.key_path.read(cx).value().to_string();
        let secret = self.keychain.secret.read(cx).value().to_string();
        let passphrase = self.keychain.passphrase.read(cx).value().to_string();
        let inline_key = self.keychain.inline_key.read(cx).value().to_string();
        let input = IdentityInput {
            name: &name,
            kind: self.keychain.kind,
            key_path: &key_path,
            secret: &secret,
            passphrase: &passphrase,
            clear_passphrase: self.keychain.clear_passphrase,
            inline_key: &inline_key,
        };
        let draft = match build_draft(&input, editing.as_ref()) {
            Ok(draft) => draft,
            Err(message) => {
                self.notice_error(&message, window, cx);
                return;
            }
        };
        let saved = self
            .store
            .db()
            .and_then(|db| db.identity_save(&draft).map_err(|err| err.to_string()));
        self.store.reload();
        match saved {
            Ok(_) => self.keychain_reset(window, cx),
            Err(err) => self.notice_error(&err, window, cx),
        }
    }

    pub(crate) fn keychain_ask_delete(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.keychain.selected else {
            return;
        };
        let Some(identity) = self
            .store
            .identities()
            .iter()
            .find(|item| item.id == id)
            .cloned()
        else {
            return;
        };
        let users = match self
            .store
            .db()
            .and_then(|db| db.host_list().map_err(|e| e.to_string()))
        {
            Ok(hosts) => crate::identities::hosts_using_identity(&hosts, id),
            Err(err) => {
                self.modal = Some(Modal::Confirm(Confirm {
                    title: t(KeychainMsg::CantCheckHostsTitle).to_owned(),
                    message: format!("{}{err}", t(KeychainMsg::ReadHostsFailedPrefix)),
                    confirm_label: None,
                    destructive: false,
                    action: ConfirmAction::DeleteIdentity(id),
                }));
                cx.notify();
                return;
            }
        };
        let (text, can_delete) = crate::identities::identity_delete_prompt(&identity.name, &users);
        self.modal = Some(Modal::Confirm(Confirm {
            title: if can_delete {
                format!(
                    "{}{}{}",
                    t(KeychainMsg::DeleteTitlePrefix),
                    identity.name,
                    t(KeychainMsg::DeleteTitleSuffix)
                )
            } else {
                format!(
                    "{}{}{}",
                    t(KeychainMsg::CantDeleteTitlePrefix),
                    identity.name,
                    t(KeychainMsg::CantDeleteTitleSuffix)
                )
            },
            message: if can_delete {
                t(KeychainMsg::DeleteConfirmMessage).to_owned()
            } else {
                text
            },
            confirm_label: can_delete.then(|| t(KeychainMsg::Delete).to_owned()),
            destructive: true,
            action: ConfirmAction::DeleteIdentity(id),
        }));
        cx.notify();
    }

    pub(crate) fn do_delete_identity(
        &mut self,
        id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result = self
            .store
            .db()
            .and_then(|db| db.identity_delete(id).map_err(|err| err.to_string()));
        self.store.reload();
        match result {
            Ok(()) => self.keychain_reset(window, cx),
            Err(err) => self.notice_error(&err, window, cx),
        }
    }

    pub(crate) fn keychain_ask_clear_secret(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.keychain.selected else {
            return;
        };
        let Some(identity) = self
            .store
            .identities()
            .iter()
            .find(|item| item.id == id)
            .cloned()
        else {
            return;
        };
        if let Err(message) = crate::identities::clear_secret_draft(&identity) {
            self.modal = Some(Modal::Confirm(Confirm {
                title: t(KeychainMsg::CantClearTitle).to_owned(),
                message,
                confirm_label: None,
                destructive: false,
                action: ConfirmAction::ClearIdentitySecret(id),
            }));
        } else {
            self.modal = Some(Modal::Confirm(Confirm {
                title: format!(
                    "{}{}{}",
                    t(KeychainMsg::ClearTitlePrefix),
                    identity.name,
                    t(KeychainMsg::ClearTitleSuffix)
                ),
                message: t(KeychainMsg::ClearConfirmMessage).to_owned(),
                confirm_label: Some(t(KeychainMsg::Clear).to_owned()),
                destructive: true,
                action: ConfirmAction::ClearIdentitySecret(id),
            }));
        }
        cx.notify();
    }

    pub(crate) fn do_clear_identity_secret(
        &mut self,
        id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(identity) = self
            .store
            .identities()
            .iter()
            .find(|item| item.id == id)
            .cloned()
        else {
            return;
        };
        let draft = match crate::identities::clear_secret_draft(&identity) {
            Ok(draft) => draft,
            Err(message) => {
                self.notice_error(&message, window, cx);
                return;
            }
        };
        let result = self
            .store
            .db()
            .and_then(|db| db.identity_save(&draft).map_err(|err| err.to_string()));
        self.store.reload();
        match result {
            Ok(_) => self.notice_success(
                &format!(
                    "{}{}{}",
                    t(KeychainMsg::ClearedSecretPrefix),
                    identity.name,
                    t(KeychainMsg::ClearedSecretSuffix)
                ),
                window,
                cx,
            ),
            Err(err) => self.notice_error(&err, window, cx),
        }
    }

    fn keychain_copy_public_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.keychain.selected else {
            return;
        };
        match self
            .store
            .db()
            .and_then(|db| db.identity_public_key(id).map_err(|err| err.to_string()))
        {
            Ok((public_key, _)) => {
                cx.write_to_clipboard(ClipboardItem::new_string(public_key));
                self.notice(t(KeychainMsg::CopiedPublicKey), window, cx);
            }
            Err(err) => self.notice_error(
                &format!("{}{err}", t(KeychainMsg::ReadPublicKeyFailedPrefix)),
                window,
                cx,
            ),
        }
    }

    /// 「浏览…」：从系统文件管理器选私钥文件。选好了填进路径框。
    fn keychain_browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await
                && let Some(path) = paths.into_iter().next()
            {
                let text = path.to_string_lossy().into_owned();
                let _ = this.update_in(cx, |this, window, cx| {
                    let input = this.keychain.key_path.clone();
                    input.update(cx, |state, cx| state.set_value(text, window, cx));
                });
            }
        })
        .detach();
    }

    // ---- 画 ----

    pub(super) fn keychain_panel_content(
        &mut self,
        tokens: &Tokens,
        _window: &mut ::gpui::Window,
        cx: &mut Context<Self>,
    ) -> PanelContent {
        let entity = cx.entity();
        // 新建的在最上面（同 0.2.14：按 id 倒序）。
        let mut identities: Vec<&Identity> = self.store.identities().iter().collect();
        identities.sort_by_key(|item| std::cmp::Reverse(item.id));
        let count = identities.len();
        // 页面上的类型筛选和搜索框：只缩小列表，不影响「共 n 条」。
        let query = self.manage_query(cx);
        identities.retain(|item| identity_matches(item, self.keychain.kind_filter, &query));
        let shown = identities.len();
        let ui = &self.keychain;
        let selected_identity = ui
            .selected
            .and_then(|id| self.store.identities().iter().find(|item| item.id == id));

        let rows = identities.iter().map(|identity| {
            let id = identity.id;
            let selected = ui.selected == Some(id);
            let snapshot = (*identity).clone();
            let detail = identity_detail(identity, self.store.hosts());
            let hover = tokens.hover;
            h_flex()
                .id(("identity-row", id as usize))
                .w_full()
                .gap_2()
                .items_center()
                .px_3()
                .py_3()
                .rounded_md()
                .border_1()
                .border_color(if selected {
                    tokens.accent
                } else {
                    tokens.border
                })
                .when(selected, |d| d.bg(tokens.accent_soft))
                .when(!selected, |d| d.hover(move |style| style.bg(hover)))
                .cursor_pointer()
                .child(
                    v_flex()
                        .min_w_0()
                        .flex_1()
                        .child(
                            div()
                                .truncate()
                                .typo(Role::ItemName, tokens)
                                .child(SharedString::from(identity.name.clone())),
                        )
                        .child(
                            div()
                                .truncate()
                                .typo(Role::Caption, tokens)
                                .child(SharedString::from(detail)),
                        ),
                )
                .child(badge(
                    if identity.has_secret {
                        t(KeychainMsg::BadgeStored)
                    } else {
                        t(KeychainMsg::BadgeNoSecret)
                    },
                    if identity.has_secret {
                        BadgeKind::Neutral
                    } else {
                        BadgeKind::Outline
                    },
                    tokens,
                ))
                .on_click({
                    let entity = entity.clone();
                    move |_, window, cx| {
                        let snapshot = snapshot.clone();
                        entity.update(cx, |this, cx| this.keychain_pick(&snapshot, window, cx));
                    }
                })
                .into_any_element()
        });
        let list = v_flex()
            .w_full()
            .gap_2()
            .when(count == 0 && !ui.editing(), |d| {
                d.child(empty_state(
                    t(KeychainMsg::EmptyListTitle),
                    Some(t(KeychainMsg::EmptyListDetail)),
                    Some(
                        text_button(
                            "identity-empty-new",
                            t(KeychainMsg::EmptyNewPassword),
                            ButtonLook::Primary,
                            act(&entity, |this, window, cx| this.keychain_new(window, cx)),
                        )
                        .icon(IconName::Plus)
                        .into_any_element(),
                    ),
                    tokens,
                ))
            })
            .when(count > 0 && shown == 0, |d| {
                d.child(note(t(KeychainMsg::NoMatches), tokens))
            })
            .children(rows);

        let form = ui.editing().then(|| {
            let kind_choices: Vec<Choice> = kinds()
                .iter()
                .map(|(kind, _)| Choice::new(kind_label(*kind), *kind == ui.kind))
                .collect();
            let can_keep = can_keep_passphrase(selected_identity, ui.kind);
            let secret_label = if ui.kind == IdentityKind::Password {
                t(KeychainMsg::PasswordLabel)
            } else {
                t(KeychainMsg::PassphraseLabel)
            };
            let mut items: Vec<AnyElement> = vec![
                field(
                    t(KeychainMsg::NameLabel),
                    None,
                    Input::new(&ui.name),
                    tokens,
                )
                .into_any_element(),
                v_flex()
                    .gap_1p5()
                    .child(field(
                        t(KeychainMsg::KindLabel),
                        None,
                        select(
                            "identity-kind",
                            kind_label(ui.kind),
                            kind_choices,
                            pick(&entity, |this, index, _window, cx| {
                                if let Some((kind, _)) = kinds().get(index) {
                                    this.keychain.kind = *kind;
                                    cx.notify();
                                }
                            }),
                        ),
                        tokens,
                    ))
                    .when(kind_change_clears_secret(selected_identity, ui.kind), |d| {
                        d.child(note(
                            format!(
                                "{}{}{}",
                                t(KeychainMsg::KindClearedPrefix),
                                selected_identity.map_or("", |i| kind_label(i.kind)),
                                t(KeychainMsg::KindClearedSuffix)
                            ),
                            tokens,
                        ))
                    })
                    .into_any_element(),
            ];
            if ui.kind == IdentityKind::KeyFile {
                items.push(
                    field(
                        t(KeychainMsg::KeyPathLabel),
                        None,
                        h_flex()
                            .w_full()
                            .gap_1p5()
                            .child(div().flex_1().min_w_0().child(Input::new(&ui.key_path)))
                            .child(
                                Button::new("identity-browse")
                                    .outline()
                                    .compact()
                                    .label(t(KeychainMsg::Browse))
                                    .on_click(act(&entity, |this, window, cx| {
                                        this.keychain_browse(window, cx)
                                    })),
                            ),
                        tokens,
                    )
                    .into_any_element(),
                );
            }
            if ui.kind == IdentityKind::KeyInline {
                let has = selected_identity.is_some_and(|i| i.has_secret);
                items.push(
                    field(
                        t(KeychainMsg::KeyBodyLabel),
                        Some(
                            if has {
                                t(KeychainMsg::KeepExisting)
                            } else {
                                t(KeychainMsg::PasteKey)
                            }
                            .to_owned(),
                        ),
                        Textarea::new(&ui.inline_key),
                        tokens,
                    )
                    .into_any_element(),
                );
                items.push(
                    v_flex()
                        .gap_1p5()
                        .child(field(
                            t(KeychainMsg::PassphraseLabel),
                            Some(
                                if can_keep {
                                    t(KeychainMsg::KeepExisting)
                                } else {
                                    t(KeychainMsg::EmptyPassphrase)
                                }
                                .to_owned(),
                            ),
                            Input::new(&ui.passphrase),
                            tokens,
                        ))
                        .when(can_keep, |d| {
                            d.child(switch_row(
                                "identity-clear-passphrase",
                                t(KeychainMsg::ClearPassphraseRow),
                                None,
                                ui.clear_passphrase,
                                {
                                    let entity = entity.clone();
                                    move |_window, cx| {
                                        entity.update(cx, |this, cx| {
                                            this.keychain.clear_passphrase =
                                                !this.keychain.clear_passphrase;
                                            cx.notify();
                                        });
                                    }
                                },
                                tokens,
                            ))
                        })
                        .into_any_element(),
                );
            }
            if matches!(ui.kind, IdentityKind::Password | IdentityKind::KeyFile) {
                items.push(
                    field(
                        secret_label,
                        Some(
                            if selected_identity.is_some() {
                                t(KeychainMsg::KeepExisting)
                            } else {
                                ""
                            }
                            .to_owned(),
                        )
                        .filter(|s| !s.is_empty()),
                        Input::new(&ui.secret),
                        tokens,
                    )
                    .into_any_element(),
                );
            }
            section(
                if ui.selected.is_some() {
                    t(KeychainMsg::EditTitle)
                } else {
                    t(KeychainMsg::NewTitle)
                },
                items,
                tokens,
            )
        });

        let footer = ui.editing().then(|| {
            let key_kind =
                selected_identity.is_some_and(|i| super::MainWindow::host_kind_is_key(i.kind));
            let clearable = selected_identity.is_some_and(|i| {
                i.has_secret && matches!(i.kind, IdentityKind::Password | IdentityKind::KeyFile)
            });
            h_flex()
                .gap_2()
                .items_center()
                .when(ui.selected.is_some(), |d| {
                    d.child(div().mr_auto().child(text_button(
                        "identity-delete",
                        t(KeychainMsg::DeleteButton),
                        ButtonLook::Ghost,
                        act(&entity, |this, _window, cx| this.keychain_ask_delete(cx)),
                    )))
                })
                .when(ui.selected.is_none(), |d| d.child(div().mr_auto()))
                .when(clearable, |d| {
                    d.child(text_button(
                        "identity-clear-secret",
                        t(KeychainMsg::ClearSecretButton),
                        ButtonLook::Outline,
                        act(&entity, |this, _window, cx| {
                            this.keychain_ask_clear_secret(cx)
                        }),
                    ))
                })
                .when(key_kind, |d| {
                    d.child(text_button(
                        "identity-copy-key",
                        t(KeychainMsg::CopyPublicKey),
                        ButtonLook::Outline,
                        act(&entity, |this, window, cx| {
                            this.keychain_copy_public_key(window, cx)
                        }),
                    ))
                })
                .child(text_button(
                    "identity-clear",
                    t(KeychainMsg::ResetForm),
                    ButtonLook::Outline,
                    act(&entity, |this, window, cx| this.keychain_reset(window, cx)),
                ))
                .child(text_button(
                    "identity-save",
                    t(KeychainMsg::Save),
                    ButtonLook::Primary,
                    act(&entity, |this, window, cx| this.keychain_save(window, cx)),
                ))
                .into_any_element()
        });

        PanelContent {
            title: t(KeychainMsg::PanelTitle).into(),
            subtitle: Some(
                crate::i18n::count(
                    count as u64,
                    t(KeychainMsg::ItemCountSuffix),
                    "item",
                    "items",
                )
                .into(),
            ),
            back: None,
            actions: vec![
                text_button(
                    "identity-new",
                    t(KeychainMsg::NewPassword),
                    ButtonLook::Outline,
                    act(&entity, |this, window, cx| this.keychain_new(window, cx)),
                )
                .icon(IconName::Plus)
                .into_any_element(),
                text_button(
                    "identity-new-keyfile",
                    t(KeychainMsg::KeyFile),
                    ButtonLook::Outline,
                    act(&entity, |this, window, cx| {
                        this.keychain_new_kind(IdentityKind::KeyFile, window, cx)
                    }),
                )
                .icon(crate::icons::Ico::KeyRound)
                .into_any_element(),
                text_button(
                    "identity-new-inline",
                    t(KeychainMsg::ImportKey),
                    ButtonLook::Outline,
                    act(&entity, |this, window, cx| {
                        this.keychain_new_kind(IdentityKind::KeyInline, window, cx)
                    }),
                )
                .icon(IconName::Plus)
                .into_any_element(),
                text_button(
                    "identity-generate",
                    t(KeychainMsg::GenerateKey),
                    ButtonLook::Outline,
                    act(&entity, |this, window, cx| {
                        this.open_generate_key_dialog(window, cx)
                    }),
                )
                .icon(crate::icons::Ico::KeyRound)
                .into_any_element(),
            ],
            body: v_flex()
                .w_full()
                .gap_4()
                .child(list)
                .children(form)
                .into_any_element(),
            footer,
        }
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{identity_detail, identity_matches};
    use zssh_core::model::{Host, Identity, IdentityKind};

    fn identity(name: &str, kind: IdentityKind, path: Option<&str>) -> Identity {
        Identity {
            id: 1,
            name: name.to_owned(),
            kind,
            key_path: path.map(str::to_owned),
            has_secret: false,
        }
    }

    #[test]
    fn the_page_filter_and_search_only_narrow_the_list() {
        let pw = identity("Deploy", IdentityKind::Password, None);
        let key = identity("laptop", IdentityKind::KeyFile, Some("keys/id_demo"));
        assert!(identity_matches(&pw, None, ""), "没筛选没搜索：全显示");
        assert!(identity_matches(&pw, Some(IdentityKind::Password), ""));
        assert!(!identity_matches(&pw, Some(IdentityKind::KeyFile), ""));
        assert!(
            identity_matches(&pw, None, " deploy "),
            "不分大小写、去空白"
        );
        assert!(identity_matches(&key, None, "id_demo"), "路径也能搜");
        assert!(identity_matches(&key, None, "私钥"), "类型名也能搜");
        assert!(!identity_matches(&key, None, "nothing"));
        assert!(!identity_matches(
            &key,
            Some(IdentityKind::Password),
            "laptop"
        ));
    }

    fn host_using(id: i64, name: &str, identity_id: i64) -> Host {
        Host {
            protocol: zssh_core::model::ProtocolSettings::default(),
            id,
            group_id: None,
            name: name.to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 22,
            username: "ops".to_owned(),
            auth_method: zssh_core::model::AuthMethod::Password,
            identity_id: Some(identity_id),
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: zssh_core::model::ProxyMode::None,
            tags: Vec::new(),
            sort: 0,
            legacy_algorithms: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn a_password_shows_which_host_uses_it() {
        let pw = identity("web-1 的密码", IdentityKind::Password, None);
        assert_eq!(identity_detail(&pw, &[]), "密码", "没有主机在用：只写类型");
        let hosts = [host_using(10, "web-1", 1), host_using(11, "db-1", 2)];
        assert_eq!(identity_detail(&pw, &hosts), "密码 · 主机：web-1");
        let shared = [
            host_using(10, "web-1", 1),
            host_using(11, "web-2", 1),
            host_using(12, "web-3", 1),
        ];
        assert_eq!(
            identity_detail(&pw, &shared[..2]),
            "密码 · 主机：web-1、web-2"
        );
        assert_eq!(
            identity_detail(&pw, &shared),
            "密码 · 主机：web-1、web-2 等 3 台"
        );
    }

    #[test]
    fn keys_do_not_list_hosts() {
        let key = identity("laptop", IdentityKind::KeyFile, Some("keys/id_demo"));
        let hosts = [host_using(10, "web-1", 1)];
        assert_eq!(identity_detail(&key, &hosts), "私钥文件 · keys/id_demo");
    }
}
