//! 身份库窗口：本地 SQLite，秘密用 DPAPI 加密后只存 `secret_blob`。
//!
//! 支持密码、密钥文件、Agent 三类身份；内联密钥（导入的整把私钥）只支持改名，秘密输入框对它
//! 不起作用，免得误输入的一行字替换掉整把私钥。已存的秘密可以单独「清除」（点两次）。
//! 删除身份前列出还在用它的主机：有主机在用就不给确认，先把那些主机改掉。

use ::gpui::{FocusHandle, Subscription};
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::*;
use gpui_kit::*;
use zssh_core::model::{Host, Identity, IdentityDraft, IdentityKind};
use zssh_core::store::Database;

use crate::host_db::open_database;
use crate::i18n::{Msg, t};

/// 身份库窗口自己的界面文案。
///
/// 只放应用自己的固定文案；身份名、主机名、路径、错误原文一律原样显示。
#[derive(Clone, Copy, Debug)]
enum IdentitiesMsg {
    DbOpenFailed,
    IdentityCountSuffix,
    ReadIdentitiesFailedPrefix,
    SavedPrefix,
    ReadHostsCheckFailedPrefix,
    DeletedPrefix,
    SelectIdentityFirst,
    ClearAgainPrefix,
    ClearAgainSuffix,
    ClearedPrefix,
    ClearedSuffix,
    DeleteAgainPrefix,
    InUsePrefix,
    InUseNameStart,
    InUseListStart,
    InUseSuffix,
    ListSeparator,
    KindPassword,
    KindKeyFile,
    KindKeyInline,
    KindAgent,
    SecretStoredSuffix,
    NamePlaceholder,
    KeyPathPlaceholder,
    SecretPlaceholder,
    InlineNoReplace,
    InlineCannotClear,
    NoSecretStoredPrefix,
    NoSecretStoredSuffix,
    Title,
    New,
    Save,
    Delete,
    ConfirmDelete,
    NameLabel,
    KindLabel,
    KeyPathLabel,
    SecretLabel,
    ConfirmClear,
    ClearSecret,
    InlineHint,
    SecretHint,
}

impl Msg for IdentitiesMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::DbOpenFailed => "打开身份库失败（数据目录不可写？）",
            Self::IdentityCountSuffix => " 个身份",
            Self::ReadIdentitiesFailedPrefix => "读取身份失败：",
            Self::SavedPrefix => "已保存 ",
            Self::ReadHostsCheckFailedPrefix => "读取主机失败，没法确认有没有主机在用它：",
            Self::DeletedPrefix => "已删除 ",
            Self::SelectIdentityFirst => "先在列表里选一条身份",
            Self::ClearAgainPrefix => "再点一次「确认清除」才会清掉「",
            Self::ClearAgainSuffix => "」已存的秘密（不能恢复）",
            Self::ClearedPrefix => "已清除「",
            Self::ClearedSuffix => "」已存的秘密",
            Self::DeleteAgainPrefix => "再点一次「确认删除」才会删除 ",
            Self::InUsePrefix => "还有 ",
            Self::InUseNameStart => " 台主机在用「",
            Self::InUseListStart => "」：",
            Self::InUseSuffix => "。先把它们改成别的凭据，才能删除",
            Self::ListSeparator => "、",
            Self::KindPassword => "密码",
            Self::KindKeyFile => "密钥文件",
            Self::KindKeyInline => "内联密钥",
            Self::KindAgent => "Agent",
            Self::SecretStoredSuffix => " · 已存秘密",
            Self::NamePlaceholder => "名称",
            Self::KeyPathPlaceholder => "私钥路径（密钥文件需要）",
            Self::SecretPlaceholder => "密码 / 口令（留空 = 不修改）",
            Self::InlineNoReplace => {
                "内联私钥不能在这里替换（输入的内容没有保存）：只能改名；要换私钥请删掉这条凭据后重新导入"
            }
            Self::InlineCannotClear => {
                "内联私钥的秘密就是整把私钥，不能只清掉它；要丢掉请删除整条凭据"
            }
            Self::NoSecretStoredPrefix => "「",
            Self::NoSecretStoredSuffix => "」没有存过秘密",
            Self::Title => "身份库",
            Self::New => "新建",
            Self::Save => "保存",
            Self::Delete => "删除",
            Self::ConfirmDelete => "确认删除",
            Self::NameLabel => "名称",
            Self::KindLabel => "类型",
            Self::KeyPathLabel => "私钥",
            Self::SecretLabel => "秘密",
            Self::ConfirmClear => "确认清除",
            Self::ClearSecret => "清除已存秘密",
            Self::InlineHint => {
                "内联私钥只能改名：秘密输入框在这里不起作用，要换私钥请删除后重新导入。"
            }
            Self::SecretHint => {
                "秘密用 DPAPI 加密，只写入 secret_blob；留空表示不修改，要删掉请点「清除已存秘密」。"
            }
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::DbOpenFailed => {
                "Could not open the credential library (is the data directory writable?)"
            }
            Self::IdentityCountSuffix => " credentials",
            Self::ReadIdentitiesFailedPrefix => "Could not read credentials: ",
            Self::SavedPrefix => "Saved ",
            Self::ReadHostsCheckFailedPrefix => {
                "Could not read hosts to check whether any host uses it: "
            }
            Self::DeletedPrefix => "Deleted ",
            Self::SelectIdentityFirst => "Select a credential in the list first",
            Self::ClearAgainPrefix => {
                "Click \"Confirm Clear\" again to clear the stored secret in \""
            }
            Self::ClearAgainSuffix => "\" (cannot be undone)",
            Self::ClearedPrefix => "Cleared the stored secret in \"",
            Self::ClearedSuffix => "\"",
            Self::DeleteAgainPrefix => "Click \"Confirm Delete\" again to delete ",
            Self::InUsePrefix => "",
            Self::InUseNameStart => " host(s) still use \"",
            Self::InUseListStart => "\": ",
            Self::InUseSuffix => ". Change them to another credential before deleting",
            Self::ListSeparator => ", ",
            Self::KindPassword => "Password",
            Self::KindKeyFile => "Key file",
            Self::KindKeyInline => "Inline key",
            Self::KindAgent => "Agent",
            Self::SecretStoredSuffix => " · Secret stored",
            Self::NamePlaceholder => "Name",
            Self::KeyPathPlaceholder => "Private key path (required for a key file)",
            Self::SecretPlaceholder => "Password / passphrase (leave blank to keep)",
            Self::InlineNoReplace => {
                "An inline private key cannot be replaced here (what you typed was not saved); only the name can change. To switch keys, delete this credential and import it again"
            }
            Self::InlineCannotClear => {
                "The secret of an inline private key is the whole key; it cannot be cleared alone. Delete the credential to discard it"
            }
            Self::NoSecretStoredPrefix => "\"",
            Self::NoSecretStoredSuffix => "\" has no stored secret",
            Self::Title => "Credential Library",
            Self::New => "New",
            Self::Save => "Save",
            Self::Delete => "Delete",
            Self::ConfirmDelete => "Confirm Delete",
            Self::NameLabel => "Name",
            Self::KindLabel => "Type",
            Self::KeyPathLabel => "Private key",
            Self::SecretLabel => "Secret",
            Self::ConfirmClear => "Confirm Clear",
            Self::ClearSecret => "Clear Stored Secret",
            Self::InlineHint => {
                "An inline private key can only be renamed; the secret box has no effect here. To switch keys, delete and import again."
            }
            Self::SecretHint => {
                "Secrets are encrypted with DPAPI and written only to secret_blob; leave blank to keep the current one, or click \"Clear Stored Secret\" to remove it."
            }
        }
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
struct FormValues {
    name: String,
    key_path: String,
    secret: String,
}

impl FormValues {
    fn from_identity(identity: &Identity) -> Self {
        Self {
            name: identity.name.clone(),
            key_path: identity.key_path.clone().unwrap_or_default(),
            secret: String::new(),
        }
    }
}

fn draft_from(values: &FormValues, id: Option<i64>, kind: IdentityKind) -> IdentityDraft {
    let key_path = values.key_path.trim();
    let secret = values.secret.trim();
    IdentityDraft {
        id,
        name: values.name.trim().to_owned(),
        kind,
        key_path: (kind == IdentityKind::KeyFile && !key_path.is_empty())
            .then(|| key_path.to_owned()),
        // 内联私钥的「秘密」就是整把私钥：这里不允许用一个输入框替换它（见 `check_secret_input`），
        // 草稿里恒为 `None` = 保持不动，只能改名。
        secret: (!secret.is_empty() && kind != IdentityKind::KeyInline)
            .then(|| values.secret.clone()),
        passphrase: None,
    }
}

/// 保存前的检查：内联私钥身份不接受秘密输入，免得误输入的一行字替换整把私钥。
fn check_secret_input(kind: IdentityKind, typed_secret: &str) -> Result<(), String> {
    if kind == IdentityKind::KeyInline && !typed_secret.trim().is_empty() {
        return Err(t(IdentitiesMsg::InlineNoReplace).to_owned());
    }
    Ok(())
}

/// 「清除已存秘密」要提交的草稿：名字、类型、路径原样，秘密给空串（= 清掉）。
/// 内联私钥不给清（清了就等于丢掉整把私钥，该删整条凭据）；本来就没存秘密的也没得清。
pub(crate) fn clear_secret_draft(identity: &Identity) -> Result<IdentityDraft, String> {
    if identity.kind == IdentityKind::KeyInline {
        return Err(t(IdentitiesMsg::InlineCannotClear).to_owned());
    }
    if !identity.has_secret {
        return Err(format!(
            "{}{}{}",
            t(IdentitiesMsg::NoSecretStoredPrefix),
            identity.name,
            t(IdentitiesMsg::NoSecretStoredSuffix)
        ));
    }
    Ok(IdentityDraft {
        id: Some(identity.id),
        name: identity.name.clone(),
        kind: identity.kind,
        key_path: identity.key_path.clone(),
        secret: Some(String::new()),
        passphrase: None,
    })
}

/// 还在用这条身份的主机名。
pub(crate) fn hosts_using_identity(hosts: &[Host], identity_id: i64) -> Vec<String> {
    hosts
        .iter()
        .filter(|host| host.identity_id == Some(identity_id))
        .map(|host| host.name.clone())
        .collect()
}

/// 第一次点「删除」：有主机在用就不给确认（核心也会拒绝），把主机名列出来。
pub(crate) fn identity_delete_prompt(name: &str, users: &[String]) -> (String, bool) {
    if users.is_empty() {
        (
            format!("{}{name}", t(IdentitiesMsg::DeleteAgainPrefix)),
            true,
        )
    } else {
        (
            format!(
                "{}{}{}{name}{}{}{}",
                t(IdentitiesMsg::InUsePrefix),
                users.len(),
                t(IdentitiesMsg::InUseNameStart),
                t(IdentitiesMsg::InUseListStart),
                users.join(t(IdentitiesMsg::ListSeparator)),
                t(IdentitiesMsg::InUseSuffix)
            ),
            false,
        )
    }
}

pub(crate) fn kind_label(kind: IdentityKind) -> &'static str {
    match kind {
        IdentityKind::Password => t(IdentitiesMsg::KindPassword),
        IdentityKind::KeyFile => t(IdentitiesMsg::KindKeyFile),
        IdentityKind::KeyInline => t(IdentitiesMsg::KindKeyInline),
        IdentityKind::Agent => t(IdentitiesMsg::KindAgent),
    }
}

pub struct IdentitiesDemo {
    db: Option<Database>,
    identities: Vec<Identity>,
    selected: Option<i64>,
    kind: IdentityKind,
    name: Entity<InputState>,
    key_path: Entity<InputState>,
    secret: Entity<InputState>,
    pending_form: Option<FormValues>,
    pending_delete: Option<i64>,
    /// 「清除已存秘密」点过一次，等第二次确认。
    pending_clear: Option<i64>,
    status: String,
    focus: FocusHandle,
    _ime_focus: Subscription,
    _ime_blur: Subscription,
}

impl IdentitiesDemo {
    pub fn new(window: &mut Window, cx: &mut Context<Self>, ime_key: String) -> Self {
        let db = match open_database() {
            Ok(db) => Some(db),
            Err(err) => {
                tracing::warn!(%err, "打开身份库失败");
                None
            }
        };
        let focus = cx.focus_handle();
        let name = text_input(window, cx, t(IdentitiesMsg::NamePlaceholder));
        let key_path = text_input(window, cx, t(IdentitiesMsg::KeyPathPlaceholder));
        let secret = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t(IdentitiesMsg::SecretPlaceholder))
        });
        let (_ime_focus, _ime_blur) = crate::ime::attach(cx, &focus, window, &ime_key);
        let mut this = Self {
            db,
            identities: Vec::new(),
            selected: None,
            kind: IdentityKind::Password,
            name,
            key_path,
            secret,
            pending_form: None,
            pending_delete: None,
            pending_clear: None,
            status: String::new(),
            focus,
            _ime_focus,
            _ime_blur,
        };
        if this.db.is_none() {
            this.status = t(IdentitiesMsg::DbOpenFailed).to_owned();
        }
        this.reload(cx);
        this
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(db) = &self.db else {
            return;
        };
        match db.identity_list() {
            Ok(identities) => {
                self.status = format!(
                    "{}{}",
                    identities.len(),
                    t(IdentitiesMsg::IdentityCountSuffix)
                );
                self.identities = identities;
            }
            Err(err) => {
                self.status = format!("{}{err}", t(IdentitiesMsg::ReadIdentitiesFailedPrefix));
            }
        }
        cx.notify();
    }

    fn select(&mut self, id: i64, cx: &mut Context<Self>) {
        let Some(identity) = self.identities.iter().find(|identity| identity.id == id) else {
            return;
        };
        self.selected = Some(id);
        self.kind = identity.kind;
        self.pending_form = Some(FormValues::from_identity(identity));
        self.pending_delete = None;
        self.pending_clear = None;
        cx.notify();
    }

    fn new_identity(&mut self, cx: &mut Context<Self>) {
        self.selected = None;
        self.kind = IdentityKind::Password;
        self.pending_form = Some(FormValues::default());
        self.pending_delete = None;
        self.pending_clear = None;
        cx.notify();
    }

    fn set_kind(&mut self, kind: IdentityKind, cx: &mut Context<Self>) {
        self.kind = kind;
        cx.notify();
    }

    fn form_values(&self, cx: &App) -> FormValues {
        FormValues {
            name: self.name.read(cx).value().to_string(),
            key_path: self.key_path.read(cx).value().to_string(),
            secret: self.secret.read(cx).value().to_string(),
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(db) = &self.db else {
            return;
        };
        let values = self.form_values(cx);
        if let Err(message) = check_secret_input(self.kind, &values.secret) {
            self.status = message;
            cx.notify();
            return;
        }
        let draft = draft_from(&values, self.selected, self.kind);
        match db.identity_save(&draft) {
            Ok(identity) => {
                self.selected = Some(identity.id);
                self.kind = identity.kind;
                self.pending_delete = None;
                self.pending_clear = None;
                self.status = format!("{}{}", t(IdentitiesMsg::SavedPrefix), identity.name);
                self.reload(cx);
                self.pending_form = Some(FormValues::from_identity(&identity));
            }
            Err(err) => {
                self.status = err.to_string();
                cx.notify();
            }
        }
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
        let (Some(db), Some(id)) = (&self.db, self.selected) else {
            return;
        };
        let name = self
            .identities
            .iter()
            .find(|identity| identity.id == id)
            .map(|identity| identity.name.clone())
            .unwrap_or_default();
        if self.pending_delete != Some(id) {
            let users = match db.host_list() {
                Ok(hosts) => hosts_using_identity(&hosts, id),
                Err(err) => {
                    self.status = format!("{}{err}", t(IdentitiesMsg::ReadHostsCheckFailedPrefix));
                    cx.notify();
                    return;
                }
            };
            let (text, armed) = identity_delete_prompt(&name, &users);
            self.pending_delete = armed.then_some(id);
            self.status = text;
            cx.notify();
            return;
        }
        match db.identity_delete(id) {
            Ok(()) => {
                self.selected = None;
                self.pending_delete = None;
                self.pending_form = Some(FormValues::default());
                self.reload(cx);
                self.status = format!("{}{}", t(IdentitiesMsg::DeletedPrefix), name);
            }
            Err(err) => {
                self.status = err.to_string();
                cx.notify();
            }
        }
    }

    /// 「清除已存秘密」：点两次才真清（只清秘密，名字、类型、路径不动）。
    fn clear_secret(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected else {
            self.status = t(IdentitiesMsg::SelectIdentityFirst).to_owned();
            cx.notify();
            return;
        };
        let Some(identity) = self.identities.iter().find(|identity| identity.id == id) else {
            return;
        };
        let draft = match clear_secret_draft(identity) {
            Ok(draft) => draft,
            Err(message) => {
                self.status = message;
                cx.notify();
                return;
            }
        };
        let name = identity.name.clone();
        if self.pending_clear != Some(id) {
            self.pending_clear = Some(id);
            self.status = format!(
                "{}{name}{}",
                t(IdentitiesMsg::ClearAgainPrefix),
                t(IdentitiesMsg::ClearAgainSuffix)
            );
            cx.notify();
            return;
        }
        let Some(db) = &self.db else {
            return;
        };
        self.pending_clear = None;
        match db.identity_save(&draft) {
            Ok(_) => {
                self.reload(cx);
                self.status = format!(
                    "{}{name}{}",
                    t(IdentitiesMsg::ClearedPrefix),
                    t(IdentitiesMsg::ClearedSuffix)
                );
            }
            Err(err) => self.status = err.to_string(),
        }
        cx.notify();
    }

    fn list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        v_flex()
            .w(::gpui::px(320.0))
            .h_full()
            .flex_none()
            .border_r_1()
            .border_color(cx.theme().border)
            .children(self.identities.iter().map(|identity| {
                let select_entity = entity.clone();
                let id = identity.id;
                let selected = self.selected == Some(id);
                h_flex()
                    .id(("identity", id as usize))
                    .h(::gpui::px(30.0))
                    .px_2()
                    .gap_2()
                    .items_center()
                    .cursor_pointer()
                    .bg(if selected {
                        cx.theme().list_active
                    } else {
                        cx.theme().background
                    })
                    .child(div().text_sm().child(identity.name.clone()))
                    .child(div().text_xs().child(format!(
                        "{}{}",
                        kind_label(identity.kind),
                        if identity.has_secret {
                            t(IdentitiesMsg::SecretStoredSuffix)
                        } else {
                            ""
                        }
                    )))
                    .on_click(move |_, _, cx| {
                        select_entity.update(cx, |this, cx| this.select(id, cx));
                    })
            }))
    }

    fn form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let password_entity = cx.entity();
        let key_entity = cx.entity();
        let agent_entity = cx.entity();
        let clear_entity = cx.entity();
        let inline = self.kind == IdentityKind::KeyInline;
        let clear_label = if self.pending_clear.is_some() && self.pending_clear == self.selected {
            t(IdentitiesMsg::ConfirmClear)
        } else {
            t(IdentitiesMsg::ClearSecret)
        };
        v_flex()
            .flex_1()
            .p_4()
            .gap_2()
            .child(field(t(IdentitiesMsg::NameLabel), &self.name))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(::gpui::px(56.0))
                            .text_sm()
                            .child(t(IdentitiesMsg::KindLabel)),
                    )
                    .child(
                        Button::new("identity-password")
                            .label(t(IdentitiesMsg::KindPassword))
                            .compact()
                            .toggled(self.kind == IdentityKind::Password)
                            .on_click(move |_, _, cx| {
                                password_entity.update(cx, |this, cx| {
                                    this.set_kind(IdentityKind::Password, cx);
                                });
                            }),
                    )
                    .child(
                        Button::new("identity-key")
                            .label(t(IdentitiesMsg::KindKeyFile))
                            .compact()
                            .toggled(self.kind == IdentityKind::KeyFile)
                            .on_click(move |_, _, cx| {
                                key_entity.update(cx, |this, cx| {
                                    this.set_kind(IdentityKind::KeyFile, cx);
                                });
                            }),
                    )
                    .child(
                        Button::new("identity-agent")
                            .label("Agent")
                            .compact()
                            .toggled(self.kind == IdentityKind::Agent)
                            .on_click(move |_, _, cx| {
                                agent_entity.update(cx, |this, cx| {
                                    this.set_kind(IdentityKind::Agent, cx);
                                });
                            }),
                    ),
            )
            .child(field(t(IdentitiesMsg::KeyPathLabel), &self.key_path))
            .child(field(t(IdentitiesMsg::SecretLabel), &self.secret))
            .child(
                h_flex().gap_2().items_center().child(
                    Button::new("identity-clear-secret")
                        .label(clear_label)
                        .compact()
                        .on_click(move |_, _, cx| {
                            clear_entity.update(cx, |this, cx| this.clear_secret(cx));
                        }),
                ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(if inline {
                        t(IdentitiesMsg::InlineHint)
                    } else {
                        t(IdentitiesMsg::SecretHint)
                    }),
            )
    }
}

fn field(label: &'static str, input: &Entity<InputState>) -> impl IntoElement {
    h_flex()
        .gap_2()
        .items_center()
        .child(div().w(::gpui::px(56.0)).text_sm().child(label))
        .child(Input::new(input).w(::gpui::px(300.0)))
}

fn text_input(
    window: &mut Window,
    cx: &mut Context<IdentitiesDemo>,
    placeholder: &'static str,
) -> Entity<InputState> {
    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
}

impl Render for IdentitiesDemo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(values) = self.pending_form.take() {
            self.name = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(values.name)
                    .placeholder(t(IdentitiesMsg::NamePlaceholder))
            });
            self.key_path = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(values.key_path)
                    .placeholder(t(IdentitiesMsg::KeyPathPlaceholder))
            });
            self.secret = cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder(t(IdentitiesMsg::SecretPlaceholder))
            });
        }

        let entity = cx.entity();
        let new_entity = entity.clone();
        let save_entity = entity.clone();
        let delete_entity = entity.clone();
        v_flex()
            .size_full()
            .track_focus(&self.focus)
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .h(::gpui::px(44.0))
                    .px_2()
                    .gap_2()
                    .items_center()
                    .child(div().text_lg().child(t(IdentitiesMsg::Title)))
                    .child(
                        Button::new("identity-new")
                            .label(t(IdentitiesMsg::New))
                            .on_click(move |_, _, cx| {
                                new_entity.update(cx, |this, cx| this.new_identity(cx));
                            }),
                    )
                    .child(
                        Button::new("identity-save")
                            .label(t(IdentitiesMsg::Save))
                            .on_click(move |_, _, cx| {
                                save_entity.update(cx, |this, cx| this.save(cx));
                            }),
                    )
                    .child(
                        Button::new("identity-delete")
                            .label(
                                if self.pending_delete.is_some()
                                    && self.pending_delete == self.selected
                                {
                                    t(IdentitiesMsg::ConfirmDelete)
                                } else {
                                    t(IdentitiesMsg::Delete)
                                },
                            )
                            .on_click(move |_, _, cx| {
                                delete_entity.update(cx, |this, cx| this.delete(cx));
                            }),
                    )
                    .child(div().text_sm().child(self.status.clone())),
            )
            .child(
                h_flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(self.list(cx))
                    .child(self.form(cx)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{FormValues, draft_from};
    use zssh_core::model::IdentityKind;

    #[test]
    fn draft_keeps_secret_only_when_typed() {
        let values = FormValues {
            name: " work ".to_owned(),
            key_path: String::new(),
            secret: String::new(),
        };
        let draft = draft_from(&values, None, IdentityKind::Password);
        assert_eq!(draft.name, "work");
        assert_eq!(draft.secret, None);
        assert_eq!(draft.key_path, None);

        let values = FormValues {
            secret: " p@ss ".to_owned(),
            ..values
        };
        let draft = draft_from(&values, Some(7), IdentityKind::Password);
        assert_eq!(draft.id, Some(7));
        assert_eq!(draft.secret.as_deref(), Some(" p@ss "));
    }

    #[test]
    fn key_path_only_for_key_file() {
        let values = FormValues {
            key_path: " C:/keys/id_ed25519 ".to_owned(),
            ..FormValues::default()
        };
        assert_eq!(
            draft_from(&values, None, IdentityKind::KeyFile)
                .key_path
                .as_deref(),
            Some("C:/keys/id_ed25519")
        );
        assert_eq!(
            draft_from(&values, None, IdentityKind::Password).key_path,
            None
        );
    }

    use super::{
        check_secret_input, clear_secret_draft, hosts_using_identity, identity_delete_prompt,
    };
    use zssh_core::model::{AuthMethod, Host, Identity, ProxyMode};
    use zssh_core::store::Database;

    fn identity(kind: IdentityKind, has_secret: bool) -> Identity {
        Identity {
            id: 3,
            name: "deploy".to_owned(),
            kind,
            key_path: (kind == IdentityKind::KeyFile).then(|| "C:/keys/id".to_owned()),
            has_secret,
        }
    }

    /// 内联私钥：秘密输入框的内容不能替换整把私钥——草稿里不带它，保存前还会明确拒绝。
    #[test]
    fn inline_key_never_takes_a_secret_from_the_form() {
        let values = FormValues {
            name: "imported".to_owned(),
            secret: "oops typed here".to_owned(),
            ..FormValues::default()
        };
        let draft = draft_from(&values, Some(9), IdentityKind::KeyInline);
        assert_eq!(draft.secret, None, "内联私钥的草稿不能带秘密");
        let message = check_secret_input(IdentityKind::KeyInline, &values.secret)
            .expect_err("要明确拒绝，不是悄悄丢掉");
        assert!(message.contains("没有保存"), "{message}");
        assert!(check_secret_input(IdentityKind::KeyInline, "  ").is_ok());
        assert!(check_secret_input(IdentityKind::Password, "p@ss").is_ok());
    }

    #[test]
    fn clearing_a_secret_keeps_everything_else() {
        let draft = clear_secret_draft(&identity(IdentityKind::KeyFile, true)).expect("可以清");
        assert_eq!(draft.id, Some(3));
        assert_eq!(draft.name, "deploy");
        assert_eq!(draft.kind, IdentityKind::KeyFile);
        assert_eq!(draft.key_path.as_deref(), Some("C:/keys/id"));
        assert_eq!(draft.secret.as_deref(), Some(""), "空串 = 清掉");

        assert!(clear_secret_draft(&identity(IdentityKind::Password, false)).is_err());
        assert!(clear_secret_draft(&identity(IdentityKind::KeyInline, true)).is_err());
    }

    /// 走真实的库：存一个带秘密的密码身份 → 清除 → `has_secret` 变 false，名字还在。
    #[test]
    fn clearing_removes_the_stored_secret_in_the_database() {
        let db = Database::open_in_memory().expect("内存库");
        let saved = db
            .identity_save(&draft_from(
                &FormValues {
                    name: "work".to_owned(),
                    secret: "s3cret".to_owned(),
                    ..FormValues::default()
                },
                None,
                IdentityKind::Password,
            ))
            .expect("存身份");
        assert!(saved.has_secret);

        let cleared = db
            .identity_save(&clear_secret_draft(&saved).expect("可以清"))
            .expect("清秘密");
        assert!(!cleared.has_secret, "秘密应该被清掉");
        assert_eq!(cleared.name, "work");
        // 光改名（不带秘密）不会动已存的：对照"留空 = 不修改"。
        let again = db
            .identity_save(&draft_from(
                &FormValues {
                    name: "work2".to_owned(),
                    secret: "x".to_owned(),
                    ..FormValues::default()
                },
                Some(cleared.id),
                IdentityKind::Password,
            ))
            .expect("设新秘密");
        assert!(again.has_secret);
        let renamed = db
            .identity_save(&draft_from(
                &FormValues {
                    name: "work3".to_owned(),
                    ..FormValues::default()
                },
                Some(again.id),
                IdentityKind::Password,
            ))
            .expect("只改名");
        assert!(renamed.has_secret, "留空不修改");
    }

    fn host_using(id: i64, name: &str, identity_id: Option<i64>) -> Host {
        Host {
            protocol: zssh_core::model::ProtocolSettings::default(),
            id,
            group_id: None,
            name: name.to_owned(),
            host: "192.0.2.1".to_owned(),
            port: 22,
            username: "root".to_owned(),
            auth_method: AuthMethod::Password,
            identity_id,
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: ProxyMode::Default,
            tags: Vec::new(),
            sort: 0,
            legacy_algorithms: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn deleting_an_identity_lists_the_hosts_using_it() {
        let hosts = vec![
            host_using(1, "web-1", Some(3)),
            host_using(2, "db-1", None),
            host_using(3, "web-2", Some(3)),
        ];
        let users = hosts_using_identity(&hosts, 3);
        assert_eq!(users, vec!["web-1".to_owned(), "web-2".to_owned()]);
        let (text, armed) = identity_delete_prompt("deploy", &users);
        assert!(!armed, "有主机在用就不给确认");
        assert!(
            text.contains("2 台") && text.contains("web-1、web-2"),
            "{text}"
        );

        let (text, armed) = identity_delete_prompt("deploy", &[]);
        assert!(armed);
        assert!(text.contains("确认删除"), "{text}");
    }
}
