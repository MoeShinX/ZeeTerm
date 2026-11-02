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
use zssh_core::model::{Identity, IdentityKind};

use super::MainWindow;
use super::identity_form::{
    IdentityInput, KINDS, build_draft, can_keep_passphrase, kind_change_clears_secret, kind_label,
};
use super::modal::{Confirm, ConfirmAction, Modal};
use super::side_panel::PanelContent;
use super::widgets::{
    BadgeKind, ButtonLook, Choice, act, badge, empty_state, field, note, pick, section, select,
    switch_row, text_button,
};
use crate::app_theme::Tokens;

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
            key_path: cx.new(|cx| InputState::new(window, cx).placeholder("私钥文件的完整路径")),
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
            Ok(hosts) => crate::identities_demo::hosts_using_identity(&hosts, id),
            Err(err) => {
                self.modal = Some(Modal::Confirm(Confirm {
                    title: "不能确认有没有主机在用它".to_owned(),
                    message: format!("读取主机失败：{err}"),
                    confirm_label: None,
                    destructive: false,
                    action: ConfirmAction::DeleteIdentity(id),
                }));
                cx.notify();
                return;
            }
        };
        let (text, can_delete) =
            crate::identities_demo::identity_delete_prompt(&identity.name, &users);
        self.modal = Some(Modal::Confirm(Confirm {
            title: if can_delete {
                format!("删除凭据「{}」？", identity.name)
            } else {
                format!("不能删除凭据「{}」", identity.name)
            },
            message: if can_delete {
                "凭据里保存的密码或私钥会一起删掉，不能恢复。".to_owned()
            } else {
                text
            },
            confirm_label: can_delete.then(|| "删除".to_owned()),
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
        if let Err(message) = crate::identities_demo::clear_secret_draft(&identity) {
            self.modal = Some(Modal::Confirm(Confirm {
                title: "不能清除".to_owned(),
                message,
                confirm_label: None,
                destructive: false,
                action: ConfirmAction::ClearIdentitySecret(id),
            }));
        } else {
            self.modal = Some(Modal::Confirm(Confirm {
                title: format!("清除「{}」已存的秘密？", identity.name),
                message: "只清掉保存的密码 / 口令，名称、类型、路径不动；不能恢复。".to_owned(),
                confirm_label: Some("清除".to_owned()),
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
        let draft = match crate::identities_demo::clear_secret_draft(&identity) {
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
                &format!("已清除「{}」已存的秘密", identity.name),
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
                self.notice("已复制公钥", window, cx);
            }
            Err(err) => self.notice_error(&format!("读取公钥失败：{err}"), window, cx),
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
            let detail = format!(
                "{}{}",
                kind_label(identity.kind),
                identity
                    .key_path
                    .as_ref()
                    .map(|path| format!(" · {path}"))
                    .unwrap_or_default()
            );
            let hover = tokens.hover;
            h_flex()
                .id(("identity-row", id as usize))
                .w_full()
                .gap_2()
                .items_center()
                .px_3()
                .py_2()
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
                                .text_sm()
                                .child(SharedString::from(identity.name.clone())),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_xs()
                                .text_color(tokens.text_muted)
                                .child(SharedString::from(detail)),
                        ),
                )
                .child(badge(
                    if identity.has_secret {
                        "已保存"
                    } else {
                        "无密码"
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
            .gap_1()
            .when(count == 0 && !ui.editing(), |d| {
                d.child(empty_state(
                    "暂无凭据",
                    Some("在主机详情里填密码时勾选「存成凭据」，也会出现在这里。"),
                    Some(
                        text_button(
                            "identity-empty-new",
                            "新建密码",
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
                d.child(note("没有匹配的凭据。", tokens))
            })
            .children(rows);

        let form = ui.editing().then(|| {
            let kind_choices: Vec<Choice> = KINDS
                .iter()
                .map(|(kind, label)| Choice::new(*label, *kind == ui.kind))
                .collect();
            let can_keep = can_keep_passphrase(selected_identity, ui.kind);
            let secret_label = if ui.kind == IdentityKind::Password { "密码" } else { "私钥口令" };
            let mut items: Vec<AnyElement> = vec![
                field("名称", None, Input::new(&ui.name), tokens).into_any_element(),
                v_flex()
                    .gap_1p5()
                    .child(field(
                        "类型",
                        None,
                        select(
                            "identity-kind",
                            kind_label(ui.kind),
                            kind_choices,
                            pick(&entity, |this, index, _window, cx| {
                                if let Some((kind, _)) = KINDS.get(index) {
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
                                "改了类型：原来保存的{}内容不会带过来，保存时会清掉；需要的话在下面重新填写。",
                                selected_identity.map_or("", |i| kind_label(i.kind))
                            ),
                            tokens,
                        ))
                    })
                    .into_any_element(),
            ];
            if ui.kind == IdentityKind::KeyFile {
                items.push(
                    field(
                        "私钥文件路径",
                        None,
                        h_flex()
                            .w_full()
                            .gap_1p5()
                            .child(div().flex_1().min_w_0().child(Input::new(&ui.key_path)))
                            .child(
                                Button::new("identity-browse")
                                    .outline()
                                    .compact()
                                    .label("浏览…")
                                    .on_click(act(&entity, |this, window, cx| this.keychain_browse(window, cx))),
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
                        "私钥内容",
                        Some(if has { "留空表示不改" } else { "粘贴私钥文本" }.to_owned()),
                        Textarea::new(&ui.inline_key),
                        tokens,
                    )
                    .into_any_element(),
                );
                items.push(
                    v_flex()
                        .gap_1p5()
                        .child(field(
                            "私钥口令",
                            Some(if can_keep { "留空表示不改" } else { "私钥没有口令就留空" }.to_owned()),
                            Input::new(&ui.passphrase),
                            tokens,
                        ))
                        .when(can_keep, |d| {
                            d.child(switch_row(
                                "identity-clear-passphrase",
                                "清除已保存的口令（私钥没有口令时用）",
                                None,
                                ui.clear_passphrase,
                                {
                                    let entity = entity.clone();
                                    move |_window, cx| {
                                        entity.update(cx, |this, cx| {
                                            this.keychain.clear_passphrase = !this.keychain.clear_passphrase;
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
                        Some(if selected_identity.is_some() { "留空表示不改" } else { "" }.to_owned()).filter(|s| !s.is_empty()),
                        Input::new(&ui.secret),
                        tokens,
                    )
                    .into_any_element(),
                );
            }
            section(
                if ui.selected.is_some() { "编辑凭据" } else { "新建凭据" },
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
                        "删除",
                        ButtonLook::Ghost,
                        act(&entity, |this, _window, cx| this.keychain_ask_delete(cx)),
                    )))
                })
                .when(ui.selected.is_none(), |d| d.child(div().mr_auto()))
                .when(clearable, |d| {
                    d.child(text_button(
                        "identity-clear-secret",
                        "清除秘密",
                        ButtonLook::Outline,
                        act(&entity, |this, _window, cx| {
                            this.keychain_ask_clear_secret(cx)
                        }),
                    ))
                })
                .when(key_kind, |d| {
                    d.child(text_button(
                        "identity-copy-key",
                        "复制公钥",
                        ButtonLook::Outline,
                        act(&entity, |this, window, cx| {
                            this.keychain_copy_public_key(window, cx)
                        }),
                    ))
                })
                .child(text_button(
                    "identity-clear",
                    "清空",
                    ButtonLook::Outline,
                    act(&entity, |this, window, cx| this.keychain_reset(window, cx)),
                ))
                .child(text_button(
                    "identity-save",
                    "保存",
                    ButtonLook::Primary,
                    act(&entity, |this, window, cx| this.keychain_save(window, cx)),
                ))
                .into_any_element()
        });

        PanelContent {
            title: "凭据".into(),
            subtitle: Some(format!("{count} 条").into()),
            back: None,
            actions: vec![
                text_button(
                    "identity-new",
                    "新建密码",
                    ButtonLook::Outline,
                    act(&entity, |this, window, cx| this.keychain_new(window, cx)),
                )
                .icon(IconName::Plus)
                .into_any_element(),
                text_button(
                    "identity-new-keyfile",
                    "私钥文件",
                    ButtonLook::Outline,
                    act(&entity, |this, window, cx| {
                        this.keychain_new_kind(IdentityKind::KeyFile, window, cx)
                    }),
                )
                .icon(crate::icons::Ico::KeyRound)
                .into_any_element(),
                text_button(
                    "identity-new-inline",
                    "导入私钥",
                    ButtonLook::Outline,
                    act(&entity, |this, window, cx| {
                        this.keychain_new_kind(IdentityKind::KeyInline, window, cx)
                    }),
                )
                .icon(IconName::Plus)
                .into_any_element(),
                text_button(
                    "identity-generate",
                    "生成密钥",
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
    use super::identity_matches;
    use zssh_core::model::{Identity, IdentityKind};

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
}
