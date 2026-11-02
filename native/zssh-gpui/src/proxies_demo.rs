//! 代理库窗口：本地 SQLite 的增删改查。需要认证的代理可以在这里设密码（留空 = 不修改，
//! 另有「清除已存密码」点两次生效）；密码经 core 的 DPAPI 加密，窗口里不回显。
//! 删除代理前列出还在用它的主机（有就不给确认，核心也会拒绝），并提醒它是默认 SSH 代理。

use ::gpui::{FocusHandle, Subscription};
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::*;
use gpui_kit::*;
use zssh_core::model::{Host, Proxy, ProxyDraft, ProxyKind, ProxyMode};
use zssh_core::store::Database;

use crate::hosts_demo::open_database;

#[derive(Clone, Default, PartialEq, Eq)]
struct FormValues {
    name: String,
    host: String,
    port: String,
    user: String,
    /// 代理密码。留空 = 不修改；从库里选中一个代理时也是空（不回显已存的密码）。
    secret: String,
}

impl FormValues {
    fn new() -> Self {
        Self {
            port: "1080".to_owned(),
            ..Default::default()
        }
    }

    fn from_proxy(proxy: &Proxy) -> Self {
        Self {
            name: proxy.name.clone(),
            host: proxy.host.clone(),
            port: proxy.port.to_string(),
            user: proxy.username.clone().unwrap_or_default(),
            secret: String::new(),
        }
    }
}

fn draft_from(values: &FormValues, id: Option<i64>, kind: ProxyKind) -> ProxyDraft {
    let port = values.port.trim();
    let port = if port.is_empty() {
        1080
    } else {
        port.parse::<u16>().unwrap_or(0)
    };
    let user = values.user.trim();
    ProxyDraft {
        id,
        name: values.name.trim().to_owned(),
        kind,
        host: values.host.trim().to_owned(),
        port,
        username: (!user.is_empty()).then(|| user.to_owned()),
        // 空白当"没填"：`None` = 不修改已存的密码。密码本身不 trim（可能以空格结尾）。
        secret: (!values.secret.trim().is_empty()).then(|| values.secret.clone()),
    }
}

/// 「清除已存密码」要提交的草稿：别的字段原样，密码给空串（= 清掉）。
pub(crate) fn clear_secret_draft(proxy: &Proxy) -> Result<ProxyDraft, String> {
    if !proxy.has_secret {
        return Err(format!("「{}」没有存过密码", proxy.name));
    }
    Ok(ProxyDraft {
        id: Some(proxy.id),
        name: proxy.name.clone(),
        kind: proxy.kind,
        host: proxy.host.clone(),
        port: proxy.port,
        username: proxy.username.clone(),
        secret: Some(String::new()),
    })
}

/// 还在用这个代理（指定了它）的主机名。
pub(crate) fn hosts_using_proxy(hosts: &[Host], proxy_id: i64) -> Vec<String> {
    hosts
        .iter()
        .filter(|host| host.proxy_mode == ProxyMode::Custom && host.proxy_id == Some(proxy_id))
        .map(|host| host.name.clone())
        .collect()
}

/// 第一次点「删除」时状态栏上的话，以及能不能进入"再点一次确认"。
/// 有主机在用：不给确认（核心也会拒绝），列出主机名。是默认 SSH 代理：照常给确认，但写明
/// 删除后默认改成不用代理。
pub(crate) fn proxy_delete_prompt(
    name: &str,
    users: &[String],
    is_default: bool,
) -> (String, bool) {
    if !users.is_empty() {
        return (
            format!(
                "还有 {} 台主机在用代理「{name}」：{}。先把它们改成别的代理或直连，才能删除",
                users.len(),
                users.join("、")
            ),
            false,
        );
    }
    let mut text = format!("再点一次「确认删除」才会删除 {name}");
    if is_default {
        text.push_str("；它是默认 SSH 代理，删除后默认改成不用代理");
    }
    (text, true)
}

pub(crate) fn kind_label(kind: ProxyKind) -> &'static str {
    match kind {
        ProxyKind::Socks5 => "SOCKS5",
        ProxyKind::Http => "HTTP",
    }
}

pub struct ProxiesDemo {
    db: Option<Database>,
    proxies: Vec<Proxy>,
    selected: Option<i64>,
    kind: ProxyKind,
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    /// 代理密码（masked）。
    secret: Entity<InputState>,
    pending_form: Option<FormValues>,
    pending_delete: Option<i64>,
    /// 「清除已存密码」点过一次，等第二次确认。
    pending_clear: Option<i64>,
    status: String,
    focus: FocusHandle,
    _ime_focus: Subscription,
    _ime_blur: Subscription,
}

impl ProxiesDemo {
    pub fn new(window: &mut Window, cx: &mut Context<Self>, ime_key: String) -> Self {
        let db = match open_database() {
            Ok(db) => Some(db),
            Err(err) => {
                tracing::warn!(%err, "打开代理库失败");
                None
            }
        };
        let focus = cx.focus_handle();
        let name = text_input(window, cx, "名称");
        let host = text_input(window, cx, "代理地址");
        let port = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value("1080")
                .placeholder("端口")
        });
        let user = text_input(window, cx, "用户名（可空）");
        let secret = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("密码（留空 = 不修改）")
        });
        let _ime_focus = cx.on_focus(&focus, window, {
            let key = ime_key.clone();
            move |_this, _window, _cx| {
                crate::ime::apply(&key, true);
            }
        });
        let _ime_blur = cx.on_blur(&focus, window, move |_this, _window, _cx| {
            crate::ime::apply(&ime_key, false);
        });
        let mut this = Self {
            db,
            proxies: Vec::new(),
            selected: None,
            kind: ProxyKind::Socks5,
            name,
            host,
            port,
            user,
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
            this.status = "打开代理库失败（数据目录不可写？）".to_owned();
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
        match db.proxy_list() {
            Ok(proxies) => {
                self.status = format!("{} 个代理", proxies.len());
                self.proxies = proxies;
            }
            Err(err) => self.status = format!("读取代理失败：{err}"),
        }
        cx.notify();
    }

    fn select(&mut self, id: i64, cx: &mut Context<Self>) {
        let Some(proxy) = self.proxies.iter().find(|proxy| proxy.id == id) else {
            return;
        };
        self.selected = Some(id);
        self.kind = proxy.kind;
        self.pending_form = Some(FormValues::from_proxy(proxy));
        self.pending_delete = None;
        self.pending_clear = None;
        cx.notify();
    }

    fn new_proxy(&mut self, cx: &mut Context<Self>) {
        self.selected = None;
        self.kind = ProxyKind::Socks5;
        self.pending_form = Some(FormValues::new());
        self.pending_delete = None;
        self.pending_clear = None;
        cx.notify();
    }

    fn set_kind(&mut self, kind: ProxyKind, cx: &mut Context<Self>) {
        self.kind = kind;
        cx.notify();
    }

    fn form_values(&self, cx: &App) -> FormValues {
        FormValues {
            name: self.name.read(cx).value().to_string(),
            host: self.host.read(cx).value().to_string(),
            port: self.port.read(cx).value().to_string(),
            user: self.user.read(cx).value().to_string(),
            secret: self.secret.read(cx).value().to_string(),
        }
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(db) = &self.db else {
            return;
        };
        let draft = draft_from(&self.form_values(cx), self.selected, self.kind);
        match db.proxy_save(&draft) {
            Ok(proxy) => {
                self.selected = Some(proxy.id);
                self.kind = proxy.kind;
                self.pending_delete = None;
                self.pending_clear = None;
                self.status = format!("已保存 {}", proxy.name);
                self.reload(cx);
                self.pending_form = Some(FormValues::from_proxy(&proxy));
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
            .proxies
            .iter()
            .find(|proxy| proxy.id == id)
            .map(|proxy| proxy.name.clone())
            .unwrap_or_default();
        if self.pending_delete != Some(id) {
            let users = match db.host_list() {
                Ok(hosts) => hosts_using_proxy(&hosts, id),
                Err(err) => {
                    self.status = format!("读取主机失败，没法确认有没有主机在用它：{err}");
                    cx.notify();
                    return;
                }
            };
            let is_default = db.default_proxy_id().ok().flatten() == Some(id);
            let (text, armed) = proxy_delete_prompt(&name, &users, is_default);
            self.pending_delete = armed.then_some(id);
            self.status = text;
            cx.notify();
            return;
        }
        match db.proxy_delete(id) {
            Ok(()) => {
                self.selected = None;
                self.pending_delete = None;
                self.pending_form = Some(FormValues::new());
                self.reload(cx);
                self.status = format!("已删除 {name}");
            }
            Err(err) => {
                self.status = err.to_string();
                cx.notify();
            }
        }
    }

    /// 「清除已存密码」：点两次才真清（只清密码，地址、用户名不动）。
    fn clear_secret(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected else {
            self.status = "先在列表里选一个代理".to_owned();
            cx.notify();
            return;
        };
        let Some(proxy) = self.proxies.iter().find(|proxy| proxy.id == id) else {
            return;
        };
        let draft = match clear_secret_draft(proxy) {
            Ok(draft) => draft,
            Err(message) => {
                self.status = message;
                cx.notify();
                return;
            }
        };
        let name = proxy.name.clone();
        if self.pending_clear != Some(id) {
            self.pending_clear = Some(id);
            self.status = format!("再点一次「确认清除」才会清掉「{name}」已存的密码（不能恢复）");
            cx.notify();
            return;
        }
        let Some(db) = &self.db else {
            return;
        };
        self.pending_clear = None;
        match db.proxy_save(&draft) {
            Ok(_) => {
                self.reload(cx);
                self.status = format!("已清除「{name}」已存的密码");
            }
            Err(err) => self.status = err.to_string(),
        }
        cx.notify();
    }

    fn list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        v_flex()
            .w(::gpui::px(300.0))
            .h_full()
            .flex_none()
            .border_r_1()
            .border_color(cx.theme().border)
            .children(self.proxies.iter().map(|proxy| {
                let select_entity = entity.clone();
                let id = proxy.id;
                let selected = self.selected == Some(id);
                h_flex()
                    .id(("proxy", id as usize))
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
                    .child(div().text_sm().child(proxy.name.clone()))
                    .child(div().text_xs().child(format!(
                        "{} {}:{}{}",
                        kind_label(proxy.kind),
                        proxy.host,
                        proxy.port,
                        if proxy.has_secret {
                            " · 已存密码"
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
        let socks_entity = cx.entity();
        let http_entity = cx.entity();
        let clear_entity = cx.entity();
        let clear_label = if self.pending_clear.is_some() && self.pending_clear == self.selected {
            "确认清除"
        } else {
            "清除已存密码"
        };
        v_flex()
            .flex_1()
            .p_4()
            .gap_2()
            .child(field("名称", &self.name))
            .child(field("地址", &self.host))
            .child(field("端口", &self.port))
            .child(field("用户名", &self.user))
            .child(field("密码", &self.secret))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().w(::gpui::px(56.0)).text_sm().child("类型"))
                    .child(
                        Button::new("proxy-kind-socks")
                            .label("SOCKS5")
                            .compact()
                            .toggled(self.kind == ProxyKind::Socks5)
                            .on_click(move |_, _, cx| {
                                socks_entity
                                    .update(cx, |this, cx| this.set_kind(ProxyKind::Socks5, cx));
                            }),
                    )
                    .child(
                        Button::new("proxy-kind-http")
                            .label("HTTP")
                            .compact()
                            .toggled(self.kind == ProxyKind::Http)
                            .on_click(move |_, _, cx| {
                                http_entity
                                    .update(cx, |this, cx| this.set_kind(ProxyKind::Http, cx));
                            }),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("密码用 DPAPI 加密保存，窗口里不回显；留空表示不修改，要删掉请点「清除已存密码」。"),
            )
            .child(
                h_flex().gap_2().items_center().child(
                    Button::new("proxy-clear-secret")
                        .label(clear_label)
                        .compact()
                        .on_click(move |_, _, cx| {
                            clear_entity.update(cx, |this, cx| this.clear_secret(cx));
                        }),
                ),
            )
    }
}

fn field(label: &'static str, input: &Entity<InputState>) -> impl IntoElement {
    h_flex()
        .gap_2()
        .items_center()
        .child(div().w(::gpui::px(56.0)).text_sm().child(label))
        .child(Input::new(input).w(::gpui::px(280.0)))
}

fn text_input(
    window: &mut Window,
    cx: &mut Context<ProxiesDemo>,
    placeholder: &'static str,
) -> Entity<InputState> {
    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
}

impl Render for ProxiesDemo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(values) = self.pending_form.take() {
            self.name = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(values.name)
                    .placeholder("名称")
            });
            self.host = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(values.host)
                    .placeholder("代理地址")
            });
            self.port = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(values.port)
                    .placeholder("端口")
            });
            self.user = cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(values.user)
                    .placeholder("用户名（可空）")
            });
            // 密码框每次都重建成空的：已存的密码不回显，也不会把上一个代理里敲的字带过来。
            self.secret = cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder("密码（留空 = 不修改）")
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
                    .child(div().text_lg().child("代理库"))
                    .child(
                        Button::new("proxy-new")
                            .label("新建")
                            .on_click(move |_, _, cx| {
                                new_entity.update(cx, |this, cx| this.new_proxy(cx));
                            }),
                    )
                    .child(
                        Button::new("proxy-save")
                            .label("保存")
                            .on_click(move |_, _, cx| {
                                save_entity.update(cx, |this, cx| this.save(cx));
                            }),
                    )
                    .child(
                        Button::new("proxy-delete")
                            .label(
                                if self.pending_delete.is_some()
                                    && self.pending_delete == self.selected
                                {
                                    "确认删除"
                                } else {
                                    "删除"
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
    use super::{FormValues, draft_from, kind_label};
    use zssh_core::model::ProxyKind;

    #[test]
    fn draft_defaults_and_username() {
        let values = FormValues::new();
        let draft = draft_from(&values, None, ProxyKind::Socks5);
        assert_eq!(draft.port, 1080);
        assert_eq!(draft.username, None);
        assert_eq!(draft.secret, None);

        let values = FormValues {
            user: "  ".to_owned(),
            ..FormValues::new()
        };
        assert_eq!(draft_from(&values, None, ProxyKind::Http).username, None);

        let values = FormValues {
            user: " user ".to_owned(),
            port: "8080".to_owned(),
            ..FormValues::new()
        };
        let draft = draft_from(&values, None, ProxyKind::Http);
        assert_eq!(draft.username.as_deref(), Some("user"));
        assert_eq!(draft.port, 8080);
        assert_eq!(draft.kind, ProxyKind::Http);
        assert_eq!(kind_label(ProxyKind::Http), "HTTP");
    }

    use super::{clear_secret_draft, hosts_using_proxy, proxy_delete_prompt};
    use zssh_core::model::{AuthMethod, Host, Proxy, ProxyMode};
    use zssh_core::store::Database;

    fn sample_proxy(has_secret: bool) -> Proxy {
        Proxy {
            id: 4,
            name: "corp".to_owned(),
            kind: ProxyKind::Http,
            host: "proxy.example".to_owned(),
            port: 3128,
            username: Some("alice".to_owned()),
            has_secret,
        }
    }

    /// 代理库现在能设密码：填了才写，留空 = 不修改，空白当没填。
    #[test]
    fn password_is_only_sent_when_typed() {
        let values = FormValues {
            secret: "  ".to_owned(),
            ..FormValues::new()
        };
        assert_eq!(draft_from(&values, None, ProxyKind::Http).secret, None);
        let values = FormValues {
            secret: " p w ".to_owned(),
            ..FormValues::new()
        };
        assert_eq!(
            draft_from(&values, None, ProxyKind::Http).secret.as_deref(),
            Some(" p w "),
            "密码原样，不 trim"
        );
    }

    #[test]
    fn clearing_the_password_keeps_the_rest_of_the_proxy() {
        let draft = clear_secret_draft(&sample_proxy(true)).expect("可以清");
        assert_eq!(draft.id, Some(4));
        assert_eq!(draft.host, "proxy.example");
        assert_eq!(draft.port, 3128);
        assert_eq!(draft.username.as_deref(), Some("alice"));
        assert_eq!(draft.secret.as_deref(), Some(""));
        assert!(clear_secret_draft(&sample_proxy(false)).is_err());
    }

    /// 走真实的库：设密码 → `has_secret` → 光改地址不动密码 → 清除。
    #[test]
    fn password_can_be_set_kept_and_cleared_in_the_database() {
        let db = Database::open_in_memory().expect("内存库");
        let values = FormValues {
            name: "corp".to_owned(),
            host: "proxy.example".to_owned(),
            port: "3128".to_owned(),
            user: "alice".to_owned(),
            secret: "hunter2".to_owned(),
        };
        let saved = db
            .proxy_save(&draft_from(&values, None, ProxyKind::Http))
            .expect("存代理");
        assert!(saved.has_secret, "能设置代理密码");

        let edited = FormValues {
            host: "proxy2.example".to_owned(),
            secret: String::new(),
            ..values
        };
        let kept = db
            .proxy_save(&draft_from(&edited, Some(saved.id), ProxyKind::Http))
            .expect("改地址");
        assert_eq!(kept.host, "proxy2.example");
        assert!(kept.has_secret, "留空不修改：密码还在");

        let cleared = db
            .proxy_save(&clear_secret_draft(&kept).expect("可以清"))
            .expect("清密码");
        assert!(!cleared.has_secret);
        assert_eq!(cleared.host, "proxy2.example");
    }

    fn host_with_proxy(id: i64, name: &str, mode: ProxyMode, proxy_id: Option<i64>) -> Host {
        Host {
            id,
            group_id: None,
            name: name.to_owned(),
            host: "192.0.2.1".to_owned(),
            port: 22,
            username: "root".to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: None,
            proxy_id,
            jump_host_id: None,
            proxy_mode: mode,
            tags: Vec::new(),
            sort: 0,
            legacy_algorithms: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn deleting_a_proxy_lists_hosts_and_warns_about_the_default() {
        let hosts = vec![
            host_with_proxy(1, "web-1", ProxyMode::Custom, Some(4)),
            host_with_proxy(2, "web-2", ProxyMode::Default, None),
            host_with_proxy(3, "web-3", ProxyMode::Custom, Some(9)),
        ];
        let users = hosts_using_proxy(&hosts, 4);
        assert_eq!(users, vec!["web-1".to_owned()]);
        let (text, armed) = proxy_delete_prompt("corp", &users, false);
        assert!(!armed && text.contains("web-1"), "{text}");

        let (text, armed) = proxy_delete_prompt("corp", &[], true);
        assert!(armed);
        assert!(
            text.contains("默认 SSH 代理") && text.contains("不用代理"),
            "{text}"
        );

        let (text, armed) = proxy_delete_prompt("corp", &[], false);
        assert!(armed && !text.contains("默认"), "{text}");
    }
}
