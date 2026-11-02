//! 右侧的代理面板（0.2.14 `ProxyPanel.tsx`）：顶部选全局默认代理，下面是可以增删改的代理列表。
//! 从主机详情的「新建 / 管理代理」进来时带返回键，新建出来的代理会被主机详情自动选中。
//! V2-F3：代理密码留空 = 不改、可以「清除已存密码」；删除前列出还在用它的主机，有就不给删。

use ::gpui::{Entity, SharedString};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::model::{Proxy, ProxyDraft, ProxyKind};

use super::MainWindow;
use super::modal::{Confirm, ConfirmAction, Modal};
use super::nav::MainView;
use super::side_panel::PanelContent;
use super::widgets::{
    BadgeKind, ButtonLook, Choice, act, badge, empty_state, field, icon_button, note, pick,
    section, select, text_button,
};
use crate::app_theme::Tokens;
use crate::typography::{Role, Typo as _};

pub const PROXY_KINDS: [(ProxyKind, &str); 2] = [
    (ProxyKind::Socks5, "SOCKS5"),
    (ProxyKind::Http, "HTTP CONNECT"),
];

/// 代理表单的值。
#[derive(Clone, Debug)]
pub struct ProxyInput<'a> {
    pub name: &'a str,
    pub kind: ProxyKind,
    pub host: &'a str,
    pub port: &'a str,
    pub username: &'a str,
    pub secret: &'a str,
}

/// 检查表单并拼出草稿（同 0.2.14 `submit`）：名称和地址不能为空，端口 1–65535；
/// 用户名留空 = 没有；密码留空 = 不改已存的密码。
pub fn build_proxy_draft(
    input: &ProxyInput<'_>,
    editing: Option<i64>,
) -> Result<ProxyDraft, String> {
    if input.name.trim().is_empty() || input.host.trim().is_empty() {
        return Err("代理名称和地址不能为空".to_owned());
    }
    let port = input
        .port
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|port| (1..=65535).contains(port))
        .ok_or_else(|| "端口要在 1-65535 之间".to_owned())? as u16;
    let username = input.username.trim();
    Ok(ProxyDraft {
        id: editing,
        name: input.name.trim().to_owned(),
        kind: input.kind,
        host: input.host.trim().to_owned(),
        port,
        username: (!username.is_empty()).then(|| username.to_owned()),
        secret: (!input.secret.is_empty()).then(|| input.secret.to_owned()),
    })
}

/// 页面上的搜索：名称 / 地址 / 类型名里含搜索词（不分大小写）。
pub fn proxy_matches(proxy: &Proxy, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty()
        || proxy.name.to_lowercase().contains(&query)
        || proxy.host.to_lowercase().contains(&query)
        || crate::proxies_demo::kind_label(proxy.kind)
            .to_lowercase()
            .contains(&query)
}

/// 删除确认的话：是默认代理就多说一句。
pub fn delete_message(is_default: bool) -> &'static str {
    if is_default {
        "它是现在的 SSH 默认代理，删除后跟随默认代理的主机改为直连。正在被主机单独使用的代理不能删除。"
    } else {
        "正在被主机单独使用的代理不能删除，要先在那些主机里换掉。"
    }
}

pub struct ProxyUi {
    pub selected: Option<i64>,
    pub creating: bool,
    pub name: Entity<InputState>,
    pub kind: ProxyKind,
    pub host: Entity<InputState>,
    pub port: Entity<InputState>,
    pub username: Entity<InputState>,
    pub secret: Entity<InputState>,
    /// 是从主机详情点进来的（带返回键，新建的代理回填给它）。
    pub from_host_form: bool,
}

impl ProxyUi {
    pub fn new(window: &mut Window, cx: &mut Context<MainWindow>) -> Self {
        Self {
            selected: None,
            creating: false,
            name: cx.new(|cx| InputState::new(window, cx)),
            kind: ProxyKind::Socks5,
            host: cx.new(|cx| InputState::new(window, cx).placeholder("127.0.0.1")),
            port: cx.new(|cx| InputState::new(window, cx).default_value("10808")),
            username: cx.new(|cx| InputState::new(window, cx)),
            secret: cx.new(|cx| InputState::new(window, cx).masked(true)),
            from_host_form: false,
        }
    }

    pub fn editing(&self) -> bool {
        self.creating || self.selected.is_some()
    }
}

impl MainWindow {
    fn proxy_reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ui = &mut self.proxy_ui;
        ui.selected = None;
        ui.creating = false;
        ui.kind = ProxyKind::Socks5;
        let (name, host, port, username, secret) = (
            ui.name.clone(),
            ui.host.clone(),
            ui.port.clone(),
            ui.username.clone(),
            ui.secret.clone(),
        );
        name.update(cx, |s, cx| s.set_value("", window, cx));
        host.update(cx, |s, cx| s.set_value("", window, cx));
        port.update(cx, |s, cx| s.set_value("10808", window, cx));
        username.update(cx, |s, cx| s.set_value("", window, cx));
        secret.update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    pub(crate) fn proxy_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.proxy_reset(window, cx);
        self.proxy_ui.creating = true;
        let focus = self.proxy_ui.name.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// 主机详情里没有代理时，点进来直接是「新建代理」的表单。
    pub(crate) fn proxy_start_creating_if(
        &mut self,
        wanted: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if wanted {
            self.proxy_new(window, cx);
        } else {
            self.proxy_reset(window, cx);
        }
    }

    pub(crate) fn proxy_pick(
        &mut self,
        proxy: &Proxy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.proxy_reset(window, cx);
        let ui = &mut self.proxy_ui;
        ui.selected = Some(proxy.id);
        ui.kind = proxy.kind;
        let (name, host, port, username) = (
            ui.name.clone(),
            ui.host.clone(),
            ui.port.clone(),
            ui.username.clone(),
        );
        let (n, h, p, u) = (
            proxy.name.clone(),
            proxy.host.clone(),
            proxy.port.to_string(),
            proxy.username.clone().unwrap_or_default(),
        );
        name.update(cx, |s, cx| s.set_value(n, window, cx));
        host.update(cx, |s, cx| s.set_value(h, window, cx));
        port.update(cx, |s, cx| s.set_value(p, window, cx));
        username.update(cx, |s, cx| s.set_value(u, window, cx));
        cx.notify();
    }

    pub(crate) fn proxy_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.proxy_ui.name.read(cx).value().to_string();
        let host = self.proxy_ui.host.read(cx).value().to_string();
        let port = self.proxy_ui.port.read(cx).value().to_string();
        let username = self.proxy_ui.username.read(cx).value().to_string();
        let secret = self.proxy_ui.secret.read(cx).value().to_string();
        let input = ProxyInput {
            name: &name,
            kind: self.proxy_ui.kind,
            host: &host,
            port: &port,
            username: &username,
            secret: &secret,
        };
        let draft = match build_proxy_draft(&input, self.proxy_ui.selected) {
            Ok(draft) => draft,
            Err(message) => {
                self.notice_error(&message, window, cx);
                return;
            }
        };
        let created = draft.id.is_none();
        let saved = self
            .store
            .db()
            .and_then(|db| db.proxy_save(&draft).map_err(|err| err.to_string()));
        self.store.reload();
        match saved {
            Ok(proxy) => {
                self.proxy_reset(window, cx);
                // 从主机详情进来新建的：主机详情自动选中它，回到主机详情。
                // 「管理 + 设置」页里的代理管理没有主机详情可回：只有侧面板那条路才回填。
                if created && self.proxy_ui.from_host_form && self.nav.view != MainView::Settings {
                    self.host_form_adopt_proxy(proxy.id);
                    self.back_to_host_details(cx);
                }
            }
            Err(err) => self.notice_error(&err, window, cx),
        }
    }

    pub(crate) fn proxy_ask_delete(&mut self, proxy_id: i64, cx: &mut Context<Self>) {
        let Some(proxy) = self
            .store
            .proxies()
            .iter()
            .find(|p| p.id == proxy_id)
            .cloned()
        else {
            return;
        };
        let users = match self
            .store
            .db()
            .and_then(|db| db.host_list().map_err(|e| e.to_string()))
        {
            Ok(hosts) => crate::proxies_demo::hosts_using_proxy(&hosts, proxy_id),
            Err(err) => {
                self.modal = Some(Modal::Confirm(Confirm {
                    title: "不能确认有没有主机在用它".to_owned(),
                    message: format!("读取主机失败：{err}"),
                    confirm_label: None,
                    destructive: false,
                    action: ConfirmAction::DeleteProxy(proxy_id),
                }));
                cx.notify();
                return;
            }
        };
        let is_default = self
            .store
            .db()
            .ok()
            .and_then(|db| db.default_proxy_id().ok().flatten())
            == Some(proxy_id);
        let (text, can_delete) =
            crate::proxies_demo::proxy_delete_prompt(&proxy.name, &users, is_default);
        self.modal = Some(Modal::Confirm(Confirm {
            title: if can_delete {
                format!("删除代理「{}」？", proxy.name)
            } else {
                format!("不能删除代理「{}」", proxy.name)
            },
            message: if can_delete {
                delete_message(is_default).to_owned()
            } else {
                text
            },
            confirm_label: can_delete.then(|| "删除".to_owned()),
            destructive: true,
            action: ConfirmAction::DeleteProxy(proxy_id),
        }));
        cx.notify();
    }

    pub(crate) fn do_delete_proxy(&mut self, id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let result = self
            .store
            .db()
            .and_then(|db| db.proxy_delete(id).map_err(|err| err.to_string()));
        self.store.reload();
        match result {
            Ok(()) => {
                if self.proxy_ui.selected == Some(id) {
                    self.proxy_reset(window, cx);
                }
                // 主机详情里正选着这个代理：选择作废。
                if let Some(form) = &mut self.host_form
                    && form.proxy == Some(id)
                {
                    form.proxy = None;
                }
            }
            Err(err) => self.notice_error(&err, window, cx),
        }
        cx.notify();
    }

    pub(crate) fn proxy_ask_clear_secret(&mut self, cx: &mut Context<Self>) {
        let Some(proxy) = self
            .proxy_ui
            .selected
            .and_then(|id| self.store.proxies().iter().find(|p| p.id == id).cloned())
        else {
            return;
        };
        self.modal = Some(Modal::Confirm(
            match crate::proxies_demo::clear_secret_draft(&proxy) {
                Ok(_) => Confirm {
                    title: format!("清除代理「{}」已存的密码？", proxy.name),
                    message: "只清掉保存的密码，其余不动；不能恢复。".to_owned(),
                    confirm_label: Some("清除".to_owned()),
                    destructive: true,
                    action: ConfirmAction::ClearProxySecret(proxy.id),
                },
                Err(message) => Confirm {
                    title: "不能清除".to_owned(),
                    message,
                    confirm_label: None,
                    destructive: false,
                    action: ConfirmAction::ClearProxySecret(proxy.id),
                },
            },
        ));
        cx.notify();
    }

    pub(crate) fn do_clear_proxy_secret(
        &mut self,
        id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(proxy) = self.store.proxies().iter().find(|p| p.id == id).cloned() else {
            return;
        };
        let draft = match crate::proxies_demo::clear_secret_draft(&proxy) {
            Ok(draft) => draft,
            Err(message) => {
                self.notice_error(&message, window, cx);
                return;
            }
        };
        let result = self
            .store
            .db()
            .and_then(|db| db.proxy_save(&draft).map_err(|err| err.to_string()));
        self.store.reload();
        match result {
            Ok(_) => {
                self.notice_success(&format!("已清除「{}」已存的密码", proxy.name), window, cx)
            }
            Err(err) => self.notice_error(&err, window, cx),
        }
    }

    pub(super) fn set_default_proxy(
        &mut self,
        id: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result = self
            .store
            .db()
            .and_then(|db| db.set_default_proxy_id(id).map_err(|err| err.to_string()));
        if let Err(err) = result {
            self.notice_error(&err, window, cx);
        }
        cx.notify();
    }

    // ---- 画 ----

    pub(super) fn proxy_panel_content(
        &mut self,
        tokens: &Tokens,
        _window: &mut ::gpui::Window,
        cx: &mut Context<Self>,
    ) -> PanelContent {
        let entity = cx.entity();
        let proxies = self.store.proxies();
        let on_page = self.nav.view == MainView::Settings;
        let query = self.manage_query(cx);
        let ui = &self.proxy_ui;
        let default_id = self
            .store
            .db()
            .ok()
            .and_then(|db| db.default_proxy_id().ok().flatten());
        let selected = ui
            .selected
            .and_then(|id| proxies.iter().find(|p| p.id == id));

        let mut default_choices = vec![Choice::new("不使用默认代理", default_id.is_none())];
        let mut default_targets: Vec<Option<i64>> = vec![None];
        for proxy in proxies {
            default_choices.push(Choice::new(
                format!(
                    "{}（{}）",
                    proxy.name,
                    crate::proxies_demo::kind_label(proxy.kind)
                ),
                default_id == Some(proxy.id),
            ));
            default_targets.push(Some(proxy.id));
        }
        let default_label = default_id
            .and_then(|id| proxies.iter().find(|p| p.id == id))
            .map_or_else(
                || "不使用默认代理".to_owned(),
                |p| format!("{}（{}）", p.name, crate::proxies_demo::kind_label(p.kind)),
            );
        let default_section = section(
            "默认代理",
            vec![
                select(
                    "proxy-default",
                    default_label,
                    default_choices,
                    pick(&entity, move |this, index, window, cx| {
                        if let Some(target) = default_targets.get(index).copied() {
                            this.set_default_proxy(target, window, cx);
                        }
                    }),
                )
                .into_any_element(),
                note("「跟随默认代理」的主机（包括快速连接）都走这一个。", tokens)
                    .into_any_element(),
            ]
            .into_iter()
            .chain((proxies.is_empty()).then(|| {
                note("还没有代理，先点右上角「新建代理」添加一个。", tokens).into_any_element()
            }))
            .collect(),
            tokens,
        );

        let rows: Vec<AnyElement> = proxies
            .iter()
            .filter(|proxy| proxy_matches(proxy, &query))
            .map(|proxy| {
                let id = proxy.id;
                let is_selected = ui.selected == Some(id);
                let (pick_snapshot, edit_snapshot) = (proxy.clone(), proxy.clone());
                let hover = tokens.hover;
                h_flex()
                    .id(("proxy-row", id as usize))
                    .w_full()
                    .gap_1()
                    .items_center()
                    .pr_1()
                    .rounded_md()
                    .border_1()
                    .border_color(if is_selected {
                        tokens.accent
                    } else {
                        tokens.border
                    })
                    .when(is_selected, |d| d.bg(tokens.accent_soft))
                    .when(!is_selected, |d| d.hover(move |style| style.bg(hover)))
                    .child(
                        h_flex()
                            .id(("proxy-pick", id as usize))
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .items_center()
                            .px_3()
                            .py_2()
                            .cursor_pointer()
                            .child(
                                v_flex()
                                    .min_w_0()
                                    .flex_1()
                                    .child(
                                        div()
                                            .truncate()
                                            .typo(Role::ItemName, tokens)
                                            .child(SharedString::from(proxy.name.clone())),
                                    )
                                    .child(
                                        div()
                                            .truncate()
                                            .typo(Role::Caption, tokens)
                                            .font_family("Consolas")
                                            .child(SharedString::from(format!(
                                                "{}:{}",
                                                proxy.host, proxy.port
                                            ))),
                                    ),
                            )
                            .child(badge(
                                crate::proxies_demo::kind_label(proxy.kind),
                                BadgeKind::Neutral,
                                tokens,
                            ))
                            .on_click({
                                let entity = entity.clone();
                                move |_, window, cx| {
                                    let snapshot = pick_snapshot.clone();
                                    entity.update(cx, |this, cx| {
                                        this.proxy_pick(&snapshot, window, cx)
                                    });
                                }
                            }),
                    )
                    .child(icon_button(
                        SharedString::from(format!("proxy-edit-{id}")),
                        crate::icons::Ico::Pencil,
                        "编辑",
                        {
                            let entity = entity.clone();
                            move |_, window, cx| {
                                let snapshot = edit_snapshot.clone();
                                entity
                                    .update(cx, |this, cx| this.proxy_pick(&snapshot, window, cx));
                            }
                        },
                    ))
                    .child(icon_button(
                        SharedString::from(format!("proxy-delete-{id}")),
                        crate::icons::Ico::Trash,
                        "删除",
                        act(&entity, move |this, _window, cx| {
                            this.proxy_ask_delete(id, cx)
                        }),
                    ))
                    .into_any_element()
            })
            .collect();
        let list = section(
            "代理列表",
            if rows.is_empty() && proxies.is_empty() {
                vec![
                    empty_state(
                        "暂无代理",
                        None,
                        Some(
                            text_button(
                                "proxy-empty-new",
                                "新建代理",
                                ButtonLook::Primary,
                                act(&entity, |this, window, cx| this.proxy_new(window, cx)),
                            )
                            .icon(IconName::Plus)
                            .into_any_element(),
                        ),
                        tokens,
                    )
                    .into_any_element(),
                ]
            } else if rows.is_empty() {
                vec![note("没有匹配的代理。", tokens).into_any_element()]
            } else {
                rows
            },
            tokens,
        );

        let form = ui.editing().then(|| {
            let kind_choices: Vec<Choice> = PROXY_KINDS
                .iter()
                .map(|(kind, label)| Choice::new(*label, *kind == ui.kind))
                .collect();
            let kind_label = PROXY_KINDS
                .iter()
                .find(|(kind, _)| *kind == ui.kind)
                .map_or("", |(_, label)| *label);
            let has_secret = selected.is_some_and(|p| p.has_secret);
            section(
                if ui.selected.is_some() {
                    "编辑代理"
                } else {
                    "新建代理"
                },
                vec![
                    field("名称", None, Input::new(&ui.name), tokens).into_any_element(),
                    field(
                        "类型",
                        None,
                        select(
                            "proxy-kind",
                            kind_label,
                            kind_choices,
                            pick(&entity, |this, index, _window, cx| {
                                if let Some((kind, _)) = PROXY_KINDS.get(index) {
                                    this.proxy_ui.kind = *kind;
                                    cx.notify();
                                }
                            }),
                        ),
                        tokens,
                    )
                    .into_any_element(),
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(div().flex_1().min_w_0().child(field(
                            "地址",
                            None,
                            Input::new(&ui.host),
                            tokens,
                        )))
                        .child(div().w(::gpui::px(88.0)).flex_none().child(field(
                            "端口",
                            None,
                            Input::new(&ui.port),
                            tokens,
                        )))
                        .into_any_element(),
                    field("用户名（可选）", None, Input::new(&ui.username), tokens)
                        .into_any_element(),
                    field(
                        "密码",
                        Some(if selected.is_some() {
                            if has_secret {
                                "已存密码 · 留空表示不改"
                            } else {
                                "留空表示不改"
                            }
                            .to_owned()
                        } else {
                            String::new()
                        })
                        .filter(|s| !s.is_empty()),
                        Input::new(&ui.secret),
                        tokens,
                    )
                    .into_any_element(),
                ],
                tokens,
            )
        });

        let footer = ui.editing().then(|| {
            let id = ui.selected;
            h_flex()
                .gap_2()
                .items_center()
                .when_some(id, |d, id| {
                    d.child(div().mr_auto().child(text_button(
                        "proxy-form-delete",
                        "删除",
                        ButtonLook::Ghost,
                        act(&entity, move |this, _window, cx| {
                            this.proxy_ask_delete(id, cx)
                        }),
                    )))
                })
                .when(id.is_none(), |d| d.child(div().mr_auto()))
                .when(selected.is_some_and(|p| p.has_secret), |d| {
                    d.child(text_button(
                        "proxy-clear-secret",
                        "清除密码",
                        ButtonLook::Outline,
                        act(&entity, |this, _window, cx| this.proxy_ask_clear_secret(cx)),
                    ))
                })
                .child(text_button(
                    "proxy-cancel",
                    "取消",
                    ButtonLook::Outline,
                    act(&entity, |this, window, cx| this.proxy_reset(window, cx)),
                ))
                .child(text_button(
                    "proxy-save",
                    "保存",
                    ButtonLook::Primary,
                    act(&entity, |this, window, cx| this.proxy_save(window, cx)),
                ))
                .into_any_element()
        });

        let back = (ui.from_host_form && !on_page).then(|| {
            icon_button(
                "proxy-back",
                IconName::ArrowLeft,
                "返回主机详情",
                act(&entity, |this, _window, cx| this.back_to_host_details(cx)),
            )
            .into_any_element()
        });

        PanelContent {
            title: "代理".into(),
            subtitle: Some(format!("{} 个", proxies.len()).into()),
            back,
            actions: vec![
                text_button(
                    "proxy-new",
                    "新建代理",
                    ButtonLook::Outline,
                    act(&entity, |this, window, cx| this.proxy_new(window, cx)),
                )
                .icon(IconName::Plus)
                .into_any_element(),
            ],
            body: v_flex()
                .w_full()
                .gap_4()
                .child(default_section)
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
    use super::{ProxyInput, build_proxy_draft, delete_message, proxy_matches};
    use zssh_core::model::{Proxy, ProxyKind};

    #[test]
    fn the_page_search_matches_name_address_and_kind() {
        let proxy = Proxy {
            id: 1,
            name: "Office".to_owned(),
            kind: ProxyKind::Socks5,
            host: "proxy.example.test".to_owned(),
            port: 1080,
            username: None,
            has_secret: false,
        };
        assert!(proxy_matches(&proxy, ""));
        assert!(proxy_matches(&proxy, "office"));
        assert!(proxy_matches(&proxy, "example.test"));
        assert!(proxy_matches(&proxy, "socks"));
        assert!(!proxy_matches(&proxy, "http connect"));
    }

    fn input<'a>() -> ProxyInput<'a> {
        ProxyInput {
            name: "office",
            kind: ProxyKind::Socks5,
            host: "198.51.100.7",
            port: "1080",
            username: "",
            secret: "",
        }
    }

    #[test]
    fn name_and_address_are_required_and_the_port_is_checked() {
        let mut form = input();
        form.name = " ";
        assert_eq!(
            build_proxy_draft(&form, None).unwrap_err(),
            "代理名称和地址不能为空"
        );
        let mut form = input();
        form.host = "";
        assert_eq!(
            build_proxy_draft(&form, None).unwrap_err(),
            "代理名称和地址不能为空"
        );
        for port in ["", "0", "70000", "x"] {
            let mut form = input();
            form.port = port;
            assert_eq!(
                build_proxy_draft(&form, None).unwrap_err(),
                "端口要在 1-65535 之间",
                "{port}"
            );
        }
    }

    #[test]
    fn a_blank_username_is_none_and_a_blank_password_means_keep_the_stored_one() {
        let draft = build_proxy_draft(&input(), Some(4)).expect("合法");
        assert_eq!(draft.id, Some(4));
        assert_eq!(draft.username, None);
        assert_eq!(draft.secret, None, "留空 = 不改");
        let mut form = input();
        form.username = " ops ";
        form.secret = "pw";
        let draft = build_proxy_draft(&form, None).expect("合法");
        assert_eq!(draft.username.as_deref(), Some("ops"));
        assert_eq!(draft.secret.as_deref(), Some("pw"));
        assert!(
            !format!("{draft:?}").contains("pw\""),
            "调试输出里不能有明文密码"
        );
    }

    #[test]
    fn the_delete_message_warns_when_it_is_the_default_proxy() {
        assert!(delete_message(true).contains("默认代理"));
        assert!(!delete_message(false).contains("默认代理的主机改为直连"));
    }

    #[test]
    fn a_blank_proxy_password_keeps_the_stored_one_once_the_database_applies_it() {
        let db = zssh_core::store::Database::open_in_memory().expect("内存库");
        let mut form = input();
        form.secret = "first";
        let created = db
            .proxy_save(&build_proxy_draft(&form, None).expect("合法"))
            .expect("新建");
        assert!(created.has_secret);
        let kept = db
            .proxy_save(&build_proxy_draft(&input(), Some(created.id)).expect("合法"))
            .expect("改");
        assert!(kept.has_secret, "留空 = 不改");
        let cleared = db
            .proxy_save(&crate::proxies_demo::clear_secret_draft(&kept).expect("能清"))
            .expect("清除");
        assert!(!cleared.has_secret, "「清除密码」才真的清掉");
    }
}
