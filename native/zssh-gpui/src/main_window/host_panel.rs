//! 右侧的主机详情 / 新建面板（0.2.14 `HostDetailsPanel.tsx`）：基本、连接、认证、网络、高级
//! （旧算法兼容、已保存的主机指纹）和 AI 记忆。
//!
//! 表单状态放在 [`HostForm`] 里，换一台主机就整个换掉（同 0.2.14 的 `key` 重挂载）；开着凭据 /
//! 代理面板时它只是被藏在下面，里面没保存的编辑还在。保存逻辑在 `host_form`（纯函数，有测试），
//! 这里只管界面和调用。

use ::gpui::{App, Entity, SharedString};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::ai::host_memory::MAX_MEMORY_CHARS;
use zssh_core::model::{AuthMethod, Host, IdentityKind, ProxyMode};

use super::MainWindow;
use super::host_form::{
    FormInput, IdentityAction, auth_label, can_reveal, identity_fits_auth, mismatch_note,
    password_hint, plan_save,
};
use super::page_logic::{flatten_groups, ungrouped_option_label};
use super::side_panel::{PROXIES, PanelContent};
use super::widgets::{
    ButtonLook, Choice, act, error_note, field, note, pick, section, select, switch_row,
    text_button,
};
use crate::app_theme::Tokens;

/// AI 记忆小节的状态。
pub struct MemoryForm {
    pub input: Entity<TextareaState>,
    /// 库里现在的正文（判断改过没有）。
    pub saved: String,
    pub updated_at: Option<i64>,
    pub error: Option<String>,
}

pub struct HostForm {
    pub host_id: Option<i64>,
    pub existing: Option<Host>,
    pub name: Entity<InputState>,
    pub address: Entity<InputState>,
    pub port: Entity<InputState>,
    pub username: Entity<InputState>,
    pub password: Entity<InputState>,
    pub group: Option<i64>,
    pub auth: AuthMethod,
    pub identity: Option<i64>,
    pub proxy_mode: ProxyMode,
    pub proxy: Option<i64>,
    pub jump: Option<i64>,
    pub remember: bool,
    pub legacy: bool,
    /// 点小眼睛从保险库取出来的密码：（凭据 id，明文）。保存时和它一样就不算改密码。
    pub revealed: Option<(i64, String)>,
    pub password_shown: bool,
    pub mismatch_note: Option<String>,
    pub fingerprint: Option<String>,
    pub memory: Option<MemoryForm>,
}

impl HostForm {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<MainWindow>,
        host: Option<&Host>,
        group: Option<i64>,
        memory: Option<(String, Option<i64>)>,
    ) -> Self {
        let text = |value: String, cx: &mut Context<MainWindow>, window: &mut Window| {
            cx.new(|cx| InputState::new(window, cx).default_value(value))
        };
        let name = text(host.map(|h| h.name.clone()).unwrap_or_default(), cx, window);
        let address = text(host.map(|h| h.host.clone()).unwrap_or_default(), cx, window);
        let port = text(host.map_or(22, |h| h.port).to_string(), cx, window);
        // 新建时默认 root（和端口默认 22 一样，可以直接改）；编辑已有主机显示它保存的用户名。
        let username = text(
            host.map_or_else(|| "root".to_owned(), |h| h.username.clone()),
            cx,
            window,
        );
        let password = cx.new(|cx| InputState::new(window, cx).masked(true));
        let memory = memory.map(|(content, updated_at)| MemoryForm {
            input: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder("还没有记忆。和 AI 聊这台主机时，它会把有用的事实记在这里。")
                    .auto_grow(4, 10)
                    .default_value(content.clone())
            }),
            saved: content,
            updated_at,
            error: None,
        });
        Self {
            host_id: host.map(|h| h.id),
            existing: host.cloned(),
            name,
            address,
            port,
            username,
            password,
            group: host.map_or(group, |h| h.group_id),
            auth: host.map_or(AuthMethod::Password, |h| h.auth_method),
            identity: host.and_then(|h| h.identity_id),
            // 新建主机默认不使用代理（不跟随全局默认代理）；编辑已有主机显示它保存的值。
            proxy_mode: host.map_or(ProxyMode::None, |h| h.proxy_mode),
            proxy: host.and_then(|h| h.proxy_id),
            jump: host.and_then(|h| h.jump_host_id),
            remember: true,
            legacy: host.is_some_and(|h| h.legacy_algorithms),
            revealed: None,
            password_shown: false,
            mismatch_note: None,
            fingerprint: None,
            memory,
        }
    }
}

fn proxy_mode_label(mode: ProxyMode) -> &'static str {
    match mode {
        ProxyMode::Default => "跟随默认代理",
        ProxyMode::None => "不用代理",
        ProxyMode::Custom => "指定代理",
    }
}

impl MainWindow {
    // ---- 打开 / 关闭 ----

    /// 打开主机详情；`host_id` 为空是新建（`group` 是默认分组）。已经开着同一台就只是显示它。
    pub(crate) fn open_host_details(
        &mut self,
        host_id: Option<i64>,
        group: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let same = host_id.is_some()
            && self
                .host_form
                .as_ref()
                .is_some_and(|form| form.host_id == host_id);
        self.proxy_ui.from_host_form = false;
        if !same {
            let host = host_id.and_then(|id| self.store.host(id).cloned());
            if host_id.is_some() && host.is_none() {
                return;
            }
            let memory = host.as_ref().and_then(|h| self.read_memory(h.id));
            let mut form = HostForm::new(window, cx, host.as_ref(), group, memory);
            if let Some(host) = &host {
                form.fingerprint =
                    crate::ssh_backend::known_hosts().saved_fingerprint(&host.host, host.port);
            }
            let focus = form.name.read(cx).focus_handle(cx);
            self.host_form = Some(form);
            window.focus(&focus, cx);
        }
        self.panels.open_host_details();
        cx.notify();
    }

    fn read_memory(&self, host_id: i64) -> Option<(String, Option<i64>)> {
        let db = self.store.db().ok()?;
        match db.host_memory_get(host_id) {
            Ok(Some(memory)) => Some((memory.content, Some(memory.updated_at))),
            Ok(None) => Some((String::new(), None)),
            Err(err) => {
                tracing::warn!(%err, host_id, "读取主机记忆失败");
                Some((String::new(), None))
            }
        }
    }

    // ---- 表单上的动作 ----

    fn with_form(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut HostForm)) {
        if let Some(form) = &mut self.host_form {
            f(form);
        }
        cx.notify();
    }

    /// 认证方式或凭据变了：取回来的明文作废，密码框回到隐藏，不配套的凭据取消选择并说明原因。
    fn after_auth_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &mut self.host_form else {
            return;
        };
        let identity = form.identity.and_then(|id| {
            self.store
                .identities()
                .iter()
                .find(|item| item.id == id)
                .cloned()
        });
        if let Some(item) = &identity
            && let Some(note) = mismatch_note(item, form.auth)
        {
            form.mismatch_note = Some(note);
            form.identity = None;
        }
        let still_valid = matches!(
            (&form.revealed, form.identity),
            (Some((id, _)), Some(current)) if *id == current
        ) && form.auth == AuthMethod::Password;
        if !still_valid && let Some((_, value)) = form.revealed.take() {
            form.password_shown = false;
            let password = form.password.clone();
            let typed = password.read(cx).value().to_string();
            password.update(cx, |input, cx| {
                input.set_masked(true, window, cx);
                if typed == value {
                    input.set_value("", window, cx);
                }
            });
        }
        cx.notify();
    }

    pub(crate) fn host_set_auth(
        &mut self,
        auth: AuthMethod,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_form(cx, |form| form.auth = auth);
        self.after_auth_changed(window, cx);
    }

    pub(crate) fn host_set_identity(
        &mut self,
        identity: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_form(cx, |form| {
            form.identity = identity;
            form.mismatch_note = None;
        });
        self.after_auth_changed(window, cx);
    }

    /// 密码框右边的小眼睛：框里还空着、选了存过密码的凭据时，先从保险库取出来。
    pub(crate) fn host_toggle_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &mut self.host_form else {
            return;
        };
        let password = form.password.clone();
        if form.password_shown {
            form.password_shown = false;
            password.update(cx, |input, cx| input.set_masked(true, window, cx));
            cx.notify();
            return;
        }
        let empty = password.read(cx).value().is_empty();
        let identity = form.identity.and_then(|id| {
            self.store
                .identities()
                .iter()
                .find(|item| item.id == id)
                .cloned()
        });
        if empty
            && can_reveal(identity.as_ref(), form.auth)
            && let Some(identity) = identity
        {
            match self.store.db().and_then(|db| {
                db.identity_saved_password(identity.id)
                    .map_err(|err| err.to_string())
            }) {
                Ok(value) => {
                    let text = value.as_str().to_owned();
                    password.update(cx, |input, cx| input.set_value(text.clone(), window, cx));
                    if let Some(form) = &mut self.host_form {
                        form.revealed = Some((identity.id, text));
                    }
                }
                Err(err) => {
                    self.notice_error(&err, window, cx);
                    return;
                }
            }
        }
        if let Some(form) = &mut self.host_form {
            form.password_shown = true;
        }
        password.update(cx, |input, cx| input.set_masked(false, window, cx));
        cx.notify();
    }

    fn host_forget_fingerprint(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.host_form.as_ref().and_then(|f| f.existing.clone()) else {
            return;
        };
        match crate::ssh_backend::known_hosts().forget(&host.host, host.port) {
            Ok(removed) => {
                self.with_form(cx, |form| form.fingerprint = None);
                let message = if removed > 0 {
                    format!("已清除 {removed} 条指纹记录，下次连接会重新询问")
                } else {
                    "本来就没有保存过这台主机的指纹".to_owned()
                };
                self.notice(&message, window, cx);
            }
            Err(err) => self.notice_error(&err.to_string(), window, cx),
        }
    }

    // ---- 保存 / 删除 ----

    pub(crate) fn host_form_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.host_form else {
            return;
        };
        let name = form.name.read(cx).value().to_string();
        let address = form.address.read(cx).value().to_string();
        let port = form.port.read(cx).value().to_string();
        let username = form.username.read(cx).value().to_string();
        let password = form.password.read(cx).value().to_string();
        let input = FormInput {
            name: &name,
            address: &address,
            port: &port,
            username: &username,
            password: &password,
            group: form.group,
            auth: form.auth,
            identity: form.identity,
            proxy_mode: form.proxy_mode,
            proxy: form.proxy,
            jump: form.jump,
            remember: form.remember,
            legacy: form.legacy,
        };
        // 已有主机：读库里现在的样子，表单里没有的字段（标签、排序）带原值（F3 X1）。
        let existing = match form.host_id {
            Some(id) => match self.store.host_fresh(id) {
                Ok(host) => Some(host),
                Err(err) => {
                    self.notice_error(&format!("读取主机失败，没有保存：{err}"), window, cx);
                    return;
                }
            },
            None => None,
        };
        let revealed = form
            .revealed
            .as_ref()
            .map(|(id, value)| (*id, value.as_str()));
        let plan = match plan_save(&input, existing.as_ref(), self.store.identities(), revealed) {
            Ok(plan) => plan,
            Err(message) => {
                self.notice_error(&message, window, cx);
                return;
            }
        };
        let mut draft = plan.draft;
        // 先动凭据库（填了新密码时），再存主机。
        let identity_result: Result<(), String> = match plan.identity {
            IdentityAction::None => Ok(()),
            IdentityAction::Overwrite { id, draft: lite } => self
                .store
                .db()
                .and_then(|db| {
                    db.identity_save(&lite.into_draft(Some(id)))
                        .map_err(|e| e.to_string())
                })
                .map(|_| ()),
            IdentityAction::Create { draft: lite } => self
                .store
                .db()
                .and_then(|db| {
                    db.identity_save(&lite.into_draft(None))
                        .map_err(|e| e.to_string())
                })
                .map(|created| draft.identity_id = Some(created.id)),
        };
        if let Err(err) = identity_result {
            self.notice_error(&err, window, cx);
            return;
        }
        match self.store.save_host(&draft) {
            Ok(saved) => {
                self.panels.close_host_details();
                self.host_form = None;
                self.focus_content(window, cx);
                self.tree.selected_host = Some(saved.id);
                self.notice_success(&format!("已保存主机「{}」", saved.name), window, cx);
                cx.notify();
            }
            Err(err) => self.notice_error(&err, window, cx),
        }
    }

    pub(crate) fn do_delete_host(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let name = self
            .store
            .host(id)
            .map(|h| h.name.clone())
            .unwrap_or_default();
        match self.store.delete_host_with_passwords(id) {
            Ok(deleted) => {
                // 它的转发规则跟着删了：正开着的转发停掉。
                self.reconcile_forwards();
                if self
                    .host_form
                    .as_ref()
                    .is_some_and(|f| f.host_id == Some(id))
                {
                    self.panels.close_host_details();
                    self.host_form = None;
                }
                if self.tree.selected_host == Some(id) {
                    self.tree.selected_host = None;
                }
                if self.info_host == Some(id) {
                    self.info_host = None;
                }
                let mut text = format!("已删除「{name}」");
                if !deleted.passwords.is_empty() {
                    let names: Vec<String> = deleted
                        .passwords
                        .iter()
                        .map(|name| format!("「{name}」"))
                        .collect();
                    text.push_str(&format!("，并删除了它的密码{}", names.join("、")));
                }
                self.notice_success(&text, window, cx);
                if let Some(problem) = deleted.problems.first() {
                    self.notice_error(problem, window, cx);
                }
            }
            Err(err) => self.notice_error(&format!("删除失败：{err}"), window, cx),
        }
        cx.notify();
    }

    // ---- AI 记忆 ----

    pub(super) fn host_memory_save(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        clear: bool,
    ) {
        let Some(form) = &self.host_form else {
            return;
        };
        let (Some(host_id), Some(memory)) = (form.host_id, &form.memory) else {
            return;
        };
        let text = if clear {
            String::new()
        } else {
            memory.input.read(cx).value().to_string()
        };
        let input = memory.input.clone();
        let action = match crate::hosts_demo::memory_action(&text) {
            Ok(action) => action,
            Err(err) => {
                self.with_form(cx, |form| {
                    if let Some(memory) = &mut form.memory {
                        memory.error = Some(err);
                    }
                });
                return;
            }
        };
        let content = match &action {
            crate::hosts_demo::MemoryAction::Save(content) => content.clone(),
            crate::hosts_demo::MemoryAction::Clear => String::new(),
        };
        let written = self.store.db().and_then(|db| {
            db.host_memory_set(host_id, &content)
                .map_err(|e| e.to_string())
        });
        if let Err(err) = written {
            self.with_form(cx, |form| {
                if let Some(memory) = &mut form.memory {
                    memory.error = Some(err);
                }
            });
            return;
        }
        let memory_now = self.read_memory(host_id);
        // 文本框换成清理后的正文：藏掉的密码不会再留在界面上，也避免下次照原样写回去。
        input.update(cx, |state, cx| state.set_value(content.clone(), window, cx));
        self.with_form(cx, |form| {
            if let Some(memory) = &mut form.memory {
                memory.saved = content;
                memory.updated_at = memory_now.and_then(|(_, at)| at);
                memory.error = None;
            }
        });
    }

    /// AI 在对话里改了这些主机的记忆（0.2.14 的 `host-memory-changed`）：主机详情里开着的记忆框跟着刷新。
    /// 用户正在改的内容（框里和已保存的不一样）不覆盖，只提示「AI 刚更新了记忆」。
    pub(super) fn refresh_memory_from_ai(
        &mut self,
        changed: &[i64],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = &self.host_form else {
            return;
        };
        let (Some(host_id), Some(memory)) = (form.host_id, &form.memory) else {
            return;
        };
        if !changed.contains(&host_id) {
            return;
        }
        let dirty = memory.input.read(cx).value() != memory.saved;
        if dirty {
            self.with_form(cx, |form| {
                if let Some(memory) = &mut form.memory {
                    memory.error = Some(
                        "AI 刚刚更新了这台主机的记忆；你正在编辑的内容没有被覆盖，保存会替换 AI 写的。"
                            .to_owned(),
                    );
                }
            });
            return;
        }
        let input = memory.input.clone();
        let now = self.read_memory(host_id);
        let content = now
            .as_ref()
            .map(|(content, _)| content.clone())
            .unwrap_or_default();
        input.update(cx, |state, cx| state.set_value(content.clone(), window, cx));
        self.with_form(cx, |form| {
            if let Some(memory) = &mut form.memory {
                memory.saved = content;
                memory.updated_at = now.and_then(|(_, at)| at);
                memory.error = None;
            }
        });
    }

    // ---- 画 ----

    pub(super) fn host_panel_content(
        &mut self,
        tokens: &Tokens,
        _window: &mut ::gpui::Window,
        cx: &mut Context<Self>,
    ) -> PanelContent {
        let entity = cx.entity();
        let Some(form) = &self.host_form else {
            return PanelContent {
                title: "主机详情".into(),
                subtitle: None,
                back: None,
                actions: Vec::new(),
                body: div().into_any_element(),
                footer: None,
            };
        };
        let flat = self.store.flat();
        let existing = form.existing.as_ref();
        let title: SharedString = if existing.is_some() {
            "主机详情"
        } else {
            "新建主机"
        }
        .into();
        let subtitle: SharedString = match existing {
            Some(host) => format!("{}@{}:{}", host.username, host.host, host.port).into(),
            None => "填好之后保存到主机库".into(),
        };

        // ---- 基本 ----
        let groups = self.store.groups();
        let ordered = flatten_groups(groups);
        let mut group_choices = vec![Choice::new(
            ungrouped_option_label(flat),
            form.group.is_none(),
        )];
        let mut group_targets: Vec<Option<i64>> = vec![None];
        for (group, depth) in &ordered {
            group_choices
                .push(Choice::new(group.name.clone(), form.group == Some(group.id)).depth(*depth));
            group_targets.push(Some(group.id));
        }
        let group_label = form
            .group
            .and_then(|id| groups.iter().find(|g| g.id == id))
            .map_or_else(
                || ungrouped_option_label(flat).to_owned(),
                |g| g.name.clone(),
            );
        let basic = section(
            "基本",
            vec![
                field("名称", None, Input::new(&form.name), tokens).into_any_element(),
                field(
                    "分组",
                    None,
                    select(
                        "host-group",
                        group_label,
                        group_choices,
                        pick(&entity, move |this, index, _window, cx| {
                            if let Some(target) = group_targets.get(index).copied() {
                                this.with_form(cx, |form| form.group = target);
                            }
                        }),
                    ),
                    tokens,
                )
                .into_any_element(),
            ],
            tokens,
        );

        // ---- 连接 ----
        let identity = form
            .identity
            .and_then(|id| self.store.identities().iter().find(|item| item.id == id));
        let revealable = can_reveal(identity, form.auth);
        let has_text = !form.password.read(cx).value().is_empty();
        let eye = (revealable || has_text).then(|| {
            Button::new("host-password-eye")
                .ghost()
                .compact()
                .icon(if form.password_shown {
                    IconName::EyeOff
                } else {
                    IconName::Eye
                })
                .tooltip(if form.password_shown {
                    "隐藏密码"
                } else {
                    "显示密码"
                })
                .on_click(act(&entity, |this, window, cx| {
                    this.host_toggle_password(window, cx)
                }))
        });
        let hint = password_hint(
            existing.is_some(),
            identity.is_some_and(|item| item.has_secret),
        );
        let connection = section(
            "连接",
            vec![
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(div().flex_1().min_w_0().child(field(
                        "地址",
                        None,
                        Input::new(&form.address),
                        tokens,
                    )))
                    .child(div().w(::gpui::px(88.0)).flex_none().child(field(
                        "端口",
                        None,
                        Input::new(&form.port),
                        tokens,
                    )))
                    .into_any_element(),
                field("用户名", None, Input::new(&form.username), tokens).into_any_element(),
                field(
                    "密码",
                    Some(hint.to_owned()),
                    h_flex()
                        .w_full()
                        .gap_1()
                        .items_center()
                        .child(div().flex_1().min_w_0().child(Input::new(&form.password)))
                        .children(eye),
                    tokens,
                )
                .into_any_element(),
                switch_row(
                    "host-remember",
                    "没有选凭据时，把密码存成一条新凭据",
                    None,
                    form.remember,
                    {
                        let entity = entity.clone();
                        move |_window, cx| {
                            entity.update(cx, |this, cx| {
                                this.with_form(cx, |form| form.remember = !form.remember);
                            });
                        }
                    },
                    tokens,
                )
                .into_any_element(),
            ],
            tokens,
        );

        // ---- 认证 ----
        let auth_methods = [AuthMethod::Password, AuthMethod::Key, AuthMethod::Agent];
        let auth_choices: Vec<Choice> = auth_methods
            .iter()
            .map(|m| Choice::new(auth_label(*m), *m == form.auth))
            .collect();
        let fitting: Vec<&zssh_core::model::Identity> = self
            .store
            .identities()
            .iter()
            .filter(|item| identity_fits_auth(item.kind, form.auth))
            .collect();
        let mut identity_choices = vec![Choice::new("不用", form.identity.is_none())];
        let mut identity_targets: Vec<Option<i64>> = vec![None];
        for item in &fitting {
            identity_choices.push(Choice::new(
                format!(
                    "{}{}",
                    item.name,
                    if item.has_secret {
                        ""
                    } else {
                        "（没有密码）"
                    }
                ),
                form.identity == Some(item.id),
            ));
            identity_targets.push(Some(item.id));
        }
        let identity_label = identity.map_or_else(
            || "不用".to_owned(),
            |item| {
                format!(
                    "{}{}",
                    item.name,
                    if item.has_secret {
                        ""
                    } else {
                        "（没有密码）"
                    }
                )
            },
        );
        let auth = section(
            "认证",
            vec![
                field(
                    "认证方式",
                    None,
                    select(
                        "host-auth",
                        auth_label(form.auth),
                        auth_choices,
                        pick(&entity, move |this, index, window, cx| {
                            if let Some(method) = auth_methods.get(index).copied() {
                                this.host_set_auth(method, window, cx);
                            }
                        }),
                    ),
                    tokens,
                )
                .into_any_element(),
                v_flex()
                    .gap_1p5()
                    .child(field(
                        "保存的凭据",
                        None,
                        select(
                            "host-identity",
                            identity_label,
                            identity_choices,
                            pick(&entity, move |this, index, window, cx| {
                                if let Some(target) = identity_targets.get(index).copied() {
                                    this.host_set_identity(target, window, cx);
                                }
                            }),
                        ),
                        tokens,
                    ))
                    .children(
                        form.mismatch_note
                            .clone()
                            .map(|text| error_note(text, tokens.danger)),
                    )
                    .into_any_element(),
            ],
            tokens,
        );

        // ---- 网络 ----
        let proxies = self.store.proxies();
        let modes = [ProxyMode::Default, ProxyMode::None, ProxyMode::Custom];
        let mode_choices: Vec<Choice> = modes
            .iter()
            .map(|m| Choice::new(proxy_mode_label(*m), *m == form.proxy_mode))
            .collect();
        let mut proxy_choices = vec![Choice::new("（请选择）", form.proxy.is_none())];
        let mut proxy_targets: Vec<Option<i64>> = vec![None];
        for item in proxies {
            proxy_choices.push(Choice::new(
                format!(
                    "{}（{}）",
                    item.name,
                    crate::proxies_demo::kind_label(item.kind)
                ),
                form.proxy == Some(item.id),
            ));
            proxy_targets.push(Some(item.id));
        }
        let proxy_label = form
            .proxy
            .and_then(|id| proxies.iter().find(|p| p.id == id))
            .map_or_else(
                || "（请选择）".to_owned(),
                |p| format!("{}（{}）", p.name, crate::proxies_demo::kind_label(p.kind)),
            );
        let others: Vec<&Host> = self
            .store
            .hosts()
            .iter()
            .filter(|item| Some(item.id) != form.host_id)
            .collect();
        let mut jump_choices = vec![Choice::new("不使用", form.jump.is_none())];
        let mut jump_targets: Vec<Option<i64>> = vec![None];
        for item in &others {
            jump_choices.push(Choice::new(item.name.clone(), form.jump == Some(item.id)));
            jump_targets.push(Some(item.id));
        }
        let jump_label = form
            .jump
            .and_then(|id| others.iter().find(|h| h.id == id))
            .map_or_else(|| "不使用".to_owned(), |h| h.name.clone());
        let mut network = vec![
            field(
                "代理",
                None,
                select(
                    "host-proxy-mode",
                    proxy_mode_label(form.proxy_mode),
                    mode_choices,
                    pick(&entity, move |this, index, _window, cx| {
                        if let Some(mode) = modes.get(index).copied() {
                            this.with_form(cx, |form| form.proxy_mode = mode);
                        }
                    }),
                ),
                tokens,
            )
            .into_any_element(),
        ];
        if form.proxy_mode == ProxyMode::Custom {
            network.push(
                v_flex()
                    .gap_1p5()
                    .child(field(
                        "用哪个代理",
                        None,
                        select(
                            "host-proxy",
                            proxy_label,
                            proxy_choices,
                            pick(&entity, move |this, index, _window, cx| {
                                if let Some(target) = proxy_targets.get(index).copied() {
                                    this.with_form(cx, |form| form.proxy = target);
                                }
                            }),
                        ),
                        tokens,
                    ))
                    .when(proxies.is_empty(), |d| {
                        d.child(note(
                            "还没有代理，点下面的「新建 / 管理代理」添加一个。",
                            tokens,
                        ))
                    })
                    .child(
                        Button::new("host-manage-proxies")
                            .outline()
                            .compact()
                            .icon(IconName::Network)
                            .label("新建 / 管理代理")
                            .on_click(act(&entity, |this, window, cx| {
                                this.manage_proxies_from_host_form(window, cx);
                            })),
                    )
                    .into_any_element(),
            );
        }
        network.push(
            field(
                "跳板机",
                None,
                select(
                    "host-jump",
                    jump_label,
                    jump_choices,
                    pick(&entity, move |this, index, _window, cx| {
                        if let Some(target) = jump_targets.get(index).copied() {
                            this.with_form(cx, |form| form.jump = target);
                        }
                    }),
                ),
                tokens,
            )
            .into_any_element(),
        );
        let network = section("网络", network, tokens);

        // ---- 高级 ----
        let fingerprint_box = v_flex()
            .w_full()
            .gap_2()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(tokens.border)
            .child(note("已保存的主机指纹", tokens))
            .child(match existing {
                Some(_) => v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .font_family("Consolas")
                            .text_color(if form.fingerprint.is_some() {
                                tokens.text
                            } else {
                                tokens.text_muted
                            })
                            .child(SharedString::from(
                                form.fingerprint
                                    .clone()
                                    .unwrap_or_else(|| "（没有保存，下次连接会问）".to_owned()),
                            )),
                    )
                    .child(
                        Button::new("host-forget-fingerprint")
                            .outline()
                            .compact()
                            .label("清除已保存的指纹")
                            .on_click(act(&entity, |this, window, cx| {
                                this.host_forget_fingerprint(window, cx);
                            })),
                    )
                    .child(note(
                        "服务器换过密钥（比如重装系统）时用；如果是被人冒充，先查清楚原因再清。",
                        tokens,
                    ))
                    .into_any_element(),
                None => note("保存之后才能看到。", tokens).into_any_element(),
            });
        let advanced = section(
            "高级",
            vec![
                v_flex()
                    .w_full()
                    .gap_2()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(tokens.border)
                    .child(switch_row(
                        "host-legacy",
                        "旧算法兼容",
                        None,
                        form.legacy,
                        {
                            let entity = entity.clone();
                            move |_window, cx| {
                                entity.update(cx, |this, cx| {
                                    this.with_form(cx, |form| form.legacy = !form.legacy);
                                });
                            }
                        },
                        tokens,
                    ))
                    .child(error_note(
                        "仅用于老服务器：允许 SHA1 签名的 ssh-rsa、老的密钥交换和 CBC 加密。安全性较低，只在这台机器连不上、并且确认对方是老系统时才打开。",
                        tokens.danger,
                    ))
                    .into_any_element(),
                fingerprint_box.into_any_element(),
            ],
            tokens,
        );

        // ---- AI 记忆 ----
        let memory = form.memory.as_ref().map(|memory| {
            let current = memory.input.read(cx).value().to_string();
            let dirty = current != memory.saved;
            let length = current.chars().count();
            let stamp = memory
                .updated_at
                .map(|at| crate::hosts_demo::memory_summary(Some(at), crate::hosts_demo::now_unix()))
                .unwrap_or_default();
            section(
                "AI 记忆",
                vec![v_flex()
                    .gap_2()
                    .child(note(
                        "AI 排查这台主机时记下的笔记（服务、配置位置、处理过的问题、待办），下次对话会带上。「全部放行」模式下不带。只存在这台电脑上。",
                        tokens,
                    ))
                    .child(Textarea::new(&memory.input))
                    .children(memory.error.clone().map(|e| error_note(e, tokens.danger)))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .text_xs()
                                    .text_color(tokens.text_muted)
                                    .child(SharedString::from(format!(
                                        "{stamp}{}",
                                        if length > 0 {
                                            format!(" · {length}/{MAX_MEMORY_CHARS}")
                                        } else {
                                            String::new()
                                        }
                                    ))),
                            )
                            .when(!memory.saved.is_empty(), |d| {
                                d.child(text_button(
                                    "host-memory-clear",
                                    "清空",
                                    ButtonLook::Ghost,
                                    act(&entity, |this, window, cx| this.host_memory_save(window, cx, true)),
                                ))
                            })
                            .child(
                                text_button(
                                    "host-memory-save",
                                    "保存记忆",
                                    ButtonLook::Outline,
                                    act(&entity, |this, window, cx| this.host_memory_save(window, cx, false)),
                                )
                                .disabled(!dirty),
                            ),
                    )
                    .into_any_element()],
                tokens,
            )
        });

        let body = v_flex()
            .w_full()
            .gap_4()
            .child(basic)
            .child(connection)
            .child(auth)
            .child(network)
            .child(advanced)
            .children(memory)
            .into_any_element();

        let host_id = form.host_id;
        let footer = h_flex()
            .gap_2()
            .items_center()
            .when_some(host_id, |d, id| {
                d.child(div().mr_auto().child(text_button(
                    "host-delete",
                    "删除",
                    ButtonLook::Ghost,
                    act(&entity, move |this, _window, cx| {
                        this.ask_delete_host(id, cx)
                    }),
                )))
            })
            .when(host_id.is_none(), |d| d.child(div().mr_auto()))
            .child(text_button(
                "host-cancel",
                "取消",
                ButtonLook::Outline,
                act(&entity, |this, window, cx| {
                    this.cancel_host_form(window, cx)
                }),
            ))
            .child(text_button(
                "host-save",
                "保存",
                ButtonLook::Primary,
                act(&entity, |this, window, cx| this.host_form_save(window, cx)),
            ))
            .into_any_element();

        PanelContent {
            title,
            subtitle: Some(subtitle),
            back: None,
            actions: Vec::new(),
            body,
            footer: Some(footer),
        }
    }

    /// 「取消」：丢掉编辑，收起面板。
    pub(crate) fn cancel_host_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.panels.close_host_details();
        self.host_form = None;
        self.focus_content(window, cx);
        cx.notify();
    }

    /// 主机详情里「新建 / 管理代理」：盖上代理面板，返回键回到这里。
    fn manage_proxies_from_host_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let none_yet = self.store.proxies().is_empty();
        self.proxy_ui.from_host_form = true;
        self.panels.open_side(PROXIES);
        self.proxy_start_creating_if(none_yet, window, cx);
        cx.notify();
    }

    /// 代理面板的返回键 / ✕（从主机详情进来的）：回到主机详情，没保存的编辑还在。
    pub(crate) fn back_to_host_details(&mut self, cx: &mut Context<Self>) {
        self.proxy_ui.from_host_form = false;
        self.panels.close_side();
        cx.notify();
    }

    /// 代理面板里新建了代理：主机详情自动选中它（不用再去下拉框里找）。
    pub(crate) fn host_form_adopt_proxy(&mut self, proxy_id: i64) {
        if let Some(form) = &mut self.host_form {
            form.proxy_mode = ProxyMode::Custom;
            form.proxy = Some(proxy_id);
        }
    }

    /// 主机详情的某个字段在被调试脚本 / 测试读：名称输入框当前的文字。
    #[allow(dead_code)]
    pub(crate) fn host_form_name(&self, cx: &App) -> Option<String> {
        self.host_form
            .as_ref()
            .map(|form| form.name.read(cx).value().to_string())
    }

    pub(crate) fn host_kind_is_key(kind: IdentityKind) -> bool {
        matches!(kind, IdentityKind::KeyFile | IdentityKind::KeyInline)
    }
}
