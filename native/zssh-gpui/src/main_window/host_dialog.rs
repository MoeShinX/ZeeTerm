//! 主机「属性」对话框（V2-U7）：在当前页面上居中弹出，左边是类别、右边是这个类别的字段，底部
//! 「确定 / 连接 / 取消」。交互形态借自 Xshell 的会话属性对话框，字段全是 ZeeTerm 原来的主机表单里的
//! （名称、协议、地址、端口、分组、认证、凭据、代理、跳板机、X11、Telnet / 串口选项、指纹、AI 记忆），
//! 一个没加；原来折叠的「高级设置」拆进了对应的类别，不再折叠。
//!
//! 外壳（遮罩、卡片）在 `modal::render_modal`；表单状态、保存、类别切换在 `host_panel`，类别列表和错误落点
//! 在 `host_props`（纯函数有测试）。这里只管画，以及对话框自己的两层小浮层：
//! - 「新建代理…」：在属性对话框上再叠一层代理新建对话框，保存后回填到主机表单，主机表单里没保存的编辑原样留着；
//! - 有未保存修改时取消 / Esc：先问一句是否放弃。

use ::gpui::{AnyElement, Entity, SharedString, hsla, px};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState, Textarea};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::ai::host_memory::MAX_MEMORY_CHARS;
use zssh_core::model::{AuthMethod, Host, Protocol, ProxyKind, ProxyMode};

use super::MainWindow;
use super::host_form::{auth_label, can_reveal, identity_fits_auth, password_hint};
use super::host_panel::HostForm;
use super::host_props::{self, Category};
use super::page_logic::{flatten_groups, ungrouped_option_label};
use super::protocol_form::{
    self, COMMON_BAUD_RATES, DATA_BITS, ENCODINGS, FLOW_CONTROLS, FormField, LOCAL_ECHOES,
    PARITIES, PROTOCOLS, SERIAL_NEWLINES, STOP_BITS, TELNET_NEWLINES,
};
use super::proxy_panel::{PROXY_KINDS, ProxyInput, build_proxy_draft};
use super::widgets::{
    ButtonLook, Choice, act, error_note, field, field_with_error, note, pick, segmented, select,
    switch_row, text_button,
};
use crate::app_theme::Tokens;
use crate::typography::{Role, Typo as _};

/// 对话框卡片的宽度。
pub const DIALOG_WIDTH: f32 = 780.0;
/// 左侧类别列表的宽度。
const NAV_WIDTH: f32 = 160.0;

/// 「新建代理…」小对话框的表单状态（自己的输入框，不和「设置 → 管理 → 代理」页共用，
/// 免得互相冲掉没保存的内容）。
pub struct NewProxyForm {
    pub name: Entity<InputState>,
    pub host: Entity<InputState>,
    pub port: Entity<InputState>,
    pub username: Entity<InputState>,
    pub secret: Entity<InputState>,
    pub kind: ProxyKind,
    pub error: Option<String>,
}

/// 属性对话框上面叠着的小浮层。
pub enum Overlay {
    None,
    NewProxy(NewProxyForm),
    /// 有未保存修改时点了取消 / 按了 Esc：问是否放弃。
    Discard,
}

/// 属性对话框的状态（表单本身在 `MainWindow::host_form` 里）。
pub struct HostPropsDialog {
    pub category: Category,
    /// 刚打开时表单的快照：现在的快照和它不一样就是有没保存的修改。
    pub initial: String,
    pub overlay: Overlay,
}

impl HostPropsDialog {
    pub fn new(initial: String) -> Self {
        Self {
            category: Category::General,
            initial,
            overlay: Overlay::None,
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

/// 一个"枚举值下拉"：按钮上写当前值，菜单里列全部，选中后交给 `set` 改表单。
fn enum_select<T: Copy + PartialEq + 'static>(
    id: &'static str,
    entity: &Entity<MainWindow>,
    current: T,
    all: &'static [T],
    label: fn(T) -> &'static str,
    set: fn(&mut HostForm, T),
) -> impl IntoElement {
    let choices: Vec<Choice> = all
        .iter()
        .map(|value| Choice::new(label(*value), *value == current))
        .collect();
    select(
        id,
        label(current),
        choices,
        pick(entity, move |this, index, _window, cx| {
            if let Some(value) = all.get(index).copied() {
                this.with_form(cx, |form| set(form, value));
            }
        }),
    )
}

/// 「标题 + 说明 + 开关」一行，开关改表单里的一个布尔值。
fn toggle_row(
    entity: &Entity<MainWindow>,
    id: &'static str,
    title: &'static str,
    hint: Option<&'static str>,
    checked: bool,
    set: fn(&mut HostForm, bool),
    tokens: &Tokens,
) -> AnyElement {
    let entity = entity.clone();
    switch_row(
        id,
        title,
        hint.map(str::to_owned),
        checked,
        move |_window, cx| {
            entity.update(cx, |this, cx| {
                this.with_form(cx, |form| set(form, !checked));
            });
        },
        tokens,
    )
    .into_any_element()
}

impl MainWindow {
    // ---- 状态 ----

    /// 表单里有没有没保存的修改。
    pub(crate) fn host_props_dirty(&self, cx: &App) -> bool {
        match (&self.modal, &self.host_form) {
            (Some(super::modal::Modal::HostProps(dialog)), Some(form)) => {
                form.snapshot(cx) != dialog.initial || form.memory_dirty(cx)
            }
            _ => false,
        }
    }

    fn set_host_overlay(&mut self, overlay: Overlay, cx: &mut Context<Self>) {
        if let Some(super::modal::Modal::HostProps(dialog)) = &mut self.modal {
            dialog.overlay = overlay;
        }
        cx.notify();
    }

    // ---- 取消 / 确定（Esc、Enter、按钮都走这里） ----

    /// 取消 / Esc：先收小浮层；对话框本身有没保存的修改时问一句，没有就直接关。
    pub(crate) fn host_props_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let overlay_open = match &self.modal {
            Some(super::modal::Modal::HostProps(dialog)) => {
                !matches!(dialog.overlay, Overlay::None)
            }
            _ => return,
        };
        if overlay_open {
            self.set_host_overlay(Overlay::None, cx);
            if let Some(form) = &self.host_form {
                let focus = form.name.read(cx).focus_handle(cx);
                window.focus(&focus, cx);
            }
            return;
        }
        if self.host_props_dirty(cx) {
            self.set_host_overlay(Overlay::Discard, cx);
            window.focus(&self.focus, cx);
            return;
        }
        self.host_props_discard(window, cx);
    }

    /// 丢掉编辑，关掉对话框，回到原来的页面。
    pub(crate) fn host_props_discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_host_props_now();
        self.focus_content(window, cx);
        cx.notify();
    }

    /// Enter / 主按钮：小浮层开着就是它的确认，否则是对话框的「确定」。
    pub(crate) fn host_props_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let which = match &self.modal {
            Some(super::modal::Modal::HostProps(dialog)) => match dialog.overlay {
                Overlay::None => 0,
                Overlay::NewProxy(_) => 1,
                Overlay::Discard => 2,
            },
            _ => return,
        };
        match which {
            0 => self.host_form_commit(false, window, cx),
            1 => self.host_save_new_proxy(window, cx),
            _ => self.host_props_discard(window, cx),
        }
    }

    // ---- 对话框里新建代理 ----

    /// 「新建代理…」：在属性对话框上叠一层代理新建对话框。
    pub(crate) fn host_open_new_proxy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = |value: &str, placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            let value = value.to_owned();
            let placeholder = placeholder.to_owned();
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let form = NewProxyForm {
            name: text("", "", window, cx),
            host: text("", "198.51.100.7", window, cx),
            port: text("10808", "", window, cx),
            username: text("", "", window, cx),
            secret: cx.new(|cx| InputState::new(window, cx).masked(true)),
            kind: ProxyKind::Socks5,
            error: None,
        };
        let focus = form.name.read(cx).focus_handle(cx);
        self.set_host_overlay(Overlay::NewProxy(form), cx);
        window.focus(&focus, cx);
    }

    /// 小对话框的「保存」：校验、存进代理库，成功就回填到主机表单（选成「指定代理」并选中它）并收起小浮层；
    /// 不过就把原因留在小对话框里。
    pub(crate) fn host_save_new_proxy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(super::modal::Modal::HostProps(dialog)) = &self.modal else {
            return;
        };
        let Overlay::NewProxy(form) = &dialog.overlay else {
            return;
        };
        let name = form.name.read(cx).value().to_string();
        let host = form.host.read(cx).value().to_string();
        let port = form.port.read(cx).value().to_string();
        let username = form.username.read(cx).value().to_string();
        let secret = form.secret.read(cx).value().to_string();
        let kind = form.kind;
        let input = ProxyInput {
            name: &name,
            kind,
            host: &host,
            port: &port,
            username: &username,
            secret: &secret,
        };
        let result = build_proxy_draft(&input, None).and_then(|draft| {
            self.store
                .db()
                .and_then(|db| db.proxy_save(&draft).map_err(|err| err.to_string()))
        });
        match result {
            Ok(proxy) => {
                self.store.reload();
                self.host_form_adopt_proxy(proxy.id);
                self.set_host_overlay(Overlay::None, cx);
                if let Some(form) = &self.host_form {
                    let focus = form.name.read(cx).focus_handle(cx);
                    window.focus(&focus, cx);
                }
            }
            Err(message) => {
                if let Some(super::modal::Modal::HostProps(dialog)) = &mut self.modal
                    && let Overlay::NewProxy(form) = &mut dialog.overlay
                {
                    form.error = Some(message);
                }
                cx.notify();
            }
        }
    }

    // ---- 画：整个对话框 ----

    /// 对话框卡片里的内容（标题、左侧类别、右侧字段、底部按钮）。高度固定（不因类别不同而跳），
    /// 右侧字段多时自己滚动。
    pub(super) fn host_props_body(
        &self,
        dialog: &HostPropsDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(form) = self.host_form.as_ref() else {
            return div().into_any_element();
        };
        let entity = cx.entity();
        let title = host_props::dialog_title(form.existing.as_ref().map(|host| host.name.as_str()));
        let categories = form.categories();
        let category = host_props::fit(dialog.category, form.protocol, form.has_memory());
        let body_h = Self::host_props_height(self.window_height);

        let nav = v_flex()
            .w(px(NAV_WIDTH))
            .flex_none()
            .h_full()
            .gap_0p5()
            .pr_3()
            .border_r_1()
            .border_color(tokens.border)
            .children(categories.iter().enumerate().map(|(index, item)| {
                let item = *item;
                let selected = item == category;
                let has_error = form
                    .errors
                    .keys()
                    .any(|field| host_props::category_of(*field) == Some(item));
                let pick = entity.clone();
                div()
                    .id(("host-category", index))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_3()
                    .py_1p5()
                    .rounded_md()
                    .cursor_pointer()
                    .typo(
                        if selected {
                            Role::NavSelected
                        } else {
                            Role::Nav
                        },
                        tokens,
                    )
                    .when(selected, |d| d.bg(tokens.accent_soft))
                    .when(!selected, |d| {
                        let hover = tokens.hover;
                        d.hover(move |style| style.bg(hover))
                    })
                    .child(div().truncate().child(item.label(form.protocol)))
                    .when(has_error, |d| {
                        d.child(
                            div()
                                .id(("host-category-error", index))
                                .size(px(7.0))
                                .flex_none()
                                .rounded_full()
                                .bg(tokens.danger),
                        )
                    })
                    .on_click(move |_, window, cx| {
                        pick.update(cx, |this, cx| {
                            this.host_select_category(item, window, cx);
                        });
                    })
            }));

        let rows = self.host_category_rows(form, category, &entity, tokens, cx);
        let content = v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .gap_3()
            .child(
                div()
                    .flex_none()
                    .typo(Role::GroupTitle, tokens)
                    .child(category.label(form.protocol)),
            )
            .child(
                div()
                    .id((
                        "host-props-scroll",
                        categories.iter().position(|c| *c == category).unwrap_or(0),
                    ))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(v_flex().w_full().gap_3().pr_3().pb_2().children(rows)),
            );

        let general_error = form
            .error_of(FormField::General)
            .map(|text| error_note(text.to_owned(), tokens.danger));
        let (cancel, connect, ok) = (entity.clone(), entity.clone(), entity.clone());
        let footer = h_flex()
            .w_full()
            .flex_none()
            .gap_2()
            .items_center()
            .child(div().flex_1().min_w_0().children(general_error))
            .child(text_button(
                "host-ok",
                "确定",
                ButtonLook::Primary,
                move |_, window, cx| {
                    ok.update(cx, |this, cx| this.host_form_commit(false, window, cx));
                },
            ))
            .child(text_button(
                "host-connect",
                "连接",
                ButtonLook::Outline,
                move |_, window, cx| {
                    connect.update(cx, |this, cx| this.host_form_commit(true, window, cx));
                },
            ))
            .child(text_button(
                "host-cancel",
                "取消",
                ButtonLook::Outline,
                move |_, window, cx| {
                    cancel.update(cx, |this, cx| this.host_props_cancel(window, cx));
                },
            ));

        v_flex()
            .id("host-props")
            .w_full()
            .h(px(body_h))
            .gap_3()
            .child(
                div()
                    .flex_none()
                    .typo(Role::GroupTitle, tokens)
                    .truncate()
                    .child(SharedString::from(title)),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .gap_4()
                    .child(nav)
                    .child(content),
            )
            .child(footer)
            .into_any_element()
    }

    /// 对话框内容区的高度：最高 520，窗口矮时跟着缩（卡片的内边距和上下留白一并扣掉）。
    pub(crate) fn host_props_height(window_height: f32) -> f32 {
        (window_height - 48.0 - 40.0).clamp(260.0, 520.0)
    }

    /// 对话框上面叠的小浮层（新建代理 / 放弃修改的确认）；没有就是 `None`。
    pub(super) fn host_props_overlay(
        &self,
        dialog: &HostPropsDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let entity = cx.entity();
        let card = |content: AnyElement| {
            div()
                .id("host-overlay-scrim")
                .occlude()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(hsla(0.0, 0.0, 0.0, 0.3))
                .child(
                    v_flex()
                        .id("host-overlay-card")
                        .w(px(460.0))
                        .max_h_full()
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
                .into_any_element()
        };
        match &dialog.overlay {
            Overlay::None => None,
            Overlay::Discard => {
                let (keep, drop) = (entity.clone(), entity);
                Some(card(
                    v_flex()
                        .gap_4()
                        .child(
                            v_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .typo(Role::GroupTitle, tokens)
                                        .child("放弃未保存的修改？"),
                                )
                                .child(
                                    div()
                                        .typo(Role::Body, tokens)
                                        .text_color(tokens.text_muted)
                                        .child("关掉以后这次的修改不会保存。"),
                                ),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .justify_end()
                                .child(text_button(
                                    "host-discard-keep",
                                    "继续编辑",
                                    ButtonLook::Outline,
                                    move |_, window, cx| {
                                        keep.update(cx, |this, cx| {
                                            this.host_props_cancel(window, cx)
                                        });
                                    },
                                ))
                                .child(text_button(
                                    "host-discard-drop",
                                    "放弃修改",
                                    ButtonLook::Danger,
                                    move |_, window, cx| {
                                        drop.update(cx, |this, cx| {
                                            this.host_props_discard(window, cx);
                                        });
                                    },
                                )),
                        )
                        .into_any_element(),
                ))
            }
            Overlay::NewProxy(form) => {
                let kind_choices: Vec<Choice> = PROXY_KINDS
                    .iter()
                    .map(|(kind, label)| Choice::new(*label, *kind == form.kind))
                    .collect();
                let kind_label = PROXY_KINDS
                    .iter()
                    .find(|(kind, _)| *kind == form.kind)
                    .map_or("", |(_, label)| *label);
                let (cancel, save, kind_entity) = (entity.clone(), entity.clone(), entity);
                Some(card(
                    v_flex()
                        .gap_3()
                        .child(div().typo(Role::GroupTitle, tokens).child("新建代理"))
                        .child(field("名称", None, Input::new(&form.name), tokens))
                        .child(field(
                            "类型",
                            None,
                            select(
                                "host-new-proxy-kind",
                                kind_label,
                                kind_choices,
                                pick(&kind_entity, |this, index, _window, cx| {
                                    if let Some((kind, _)) = PROXY_KINDS.get(index)
                                        && let Some(super::modal::Modal::HostProps(dialog)) =
                                            &mut this.modal
                                        && let Overlay::NewProxy(form) = &mut dialog.overlay
                                    {
                                        form.kind = *kind;
                                    }
                                    cx.notify();
                                }),
                            ),
                            tokens,
                        ))
                        .child(
                            h_flex()
                                .w_full()
                                .gap_2()
                                .items_start()
                                .child(div().flex_1().min_w_0().child(field(
                                    "地址",
                                    None,
                                    Input::new(&form.host),
                                    tokens,
                                )))
                                .child(div().w(px(88.0)).flex_none().child(field(
                                    "端口",
                                    None,
                                    Input::new(&form.port),
                                    tokens,
                                ))),
                        )
                        .child(field(
                            "用户名（可选）",
                            None,
                            Input::new(&form.username),
                            tokens,
                        ))
                        .child(field("密码", None, Input::new(&form.secret), tokens))
                        .children(
                            form.error
                                .clone()
                                .map(|text| error_note(text, tokens.danger)),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .justify_end()
                                .child(text_button(
                                    "host-new-proxy-cancel",
                                    "取消",
                                    ButtonLook::Outline,
                                    move |_, window, cx| {
                                        cancel.update(cx, |this, cx| {
                                            this.host_props_cancel(window, cx)
                                        });
                                    },
                                ))
                                .child(text_button(
                                    "host-new-proxy-save",
                                    "保存",
                                    ButtonLook::Primary,
                                    move |_, window, cx| {
                                        save.update(cx, |this, cx| {
                                            this.host_save_new_proxy(window, cx);
                                        });
                                    },
                                )),
                        )
                        .into_any_element(),
                ))
            }
        }
    }

    // ---- 画：各类别的字段 ----

    fn host_category_rows(
        &self,
        form: &HostForm,
        category: Category,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
        cx: &App,
    ) -> Vec<AnyElement> {
        match category {
            Category::General => self.rows_general(form, entity, tokens, cx),
            Category::Auth => self.rows_auth(form, entity, tokens, cx),
            Category::Connection => self.rows_connection(form, entity, tokens),
            Category::Terminal => self.rows_terminal(form, entity, tokens),
            Category::SerialParams => self.rows_serial_params(form, entity, tokens),
            Category::Security => self.rows_security(form, entity, tokens),
            Category::Memory => self.rows_memory(form, entity, tokens, cx),
        }
    }

    /// 常规：名称、协议、地址 + 端口（串口是设备 + 波特率）、分组。
    fn rows_general(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
        cx: &App,
    ) -> Vec<AnyElement> {
        let vis = protocol_form::visible(form.protocol);
        let editing = form.existing.as_ref().map(|host| host.protocol.protocol);
        let options: Vec<(String, bool, Option<&'static str>)> = PROTOCOLS
            .iter()
            .map(|protocol| {
                (
                    protocol.label().to_owned(),
                    *protocol == form.protocol,
                    (!protocol_form::can_switch_to(editing, *protocol))
                        .then_some("串口主机和网络主机不能互相转换，要换类型请新建一台主机"),
                )
            })
            .collect();
        let mut rows: Vec<AnyElement> = vec![
            field_with_error(
                "名称",
                None,
                Input::new(&form.name),
                form.error_of(FormField::Name),
                tokens,
            )
            .into_any_element(),
            field(
                "协议",
                None,
                div().w(px(320.0)).child(segmented(
                    "host-protocol",
                    options,
                    pick(entity, move |this, index, window, cx| {
                        if let Some(protocol) = PROTOCOLS.get(index).copied() {
                            this.host_set_protocol(protocol, window, cx);
                        }
                    }),
                )),
                tokens,
            )
            .into_any_element(),
        ];
        if vis.address {
            rows.push(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_start()
                    .child(div().flex_1().min_w_0().child(field_with_error(
                        "地址",
                        None,
                        Input::new(&form.address),
                        form.error_of(FormField::Address),
                        tokens,
                    )))
                    .child(div().w(px(110.0)).flex_none().child(field_with_error(
                        "端口",
                        None,
                        Input::new(&form.port),
                        form.error_of(FormField::Port),
                        tokens,
                    )))
                    .into_any_element(),
            );
        }
        if vis.serial {
            rows.extend(self.serial_device_rows(form, entity, tokens, cx));
        }
        rows.push(self.group_row(form, entity, tokens));
        rows
    }

    /// 分组下拉。
    fn group_row(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
    ) -> AnyElement {
        let flat = self.store.flat();
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
        field(
            "分组",
            None,
            select(
                "host-group",
                group_label,
                group_choices,
                pick(entity, move |this, index, _window, cx| {
                    if let Some(target) = group_targets.get(index).copied() {
                        this.with_form(cx, |form| form.group = target);
                    }
                }),
            ),
            tokens,
        )
        .into_any_element()
    }

    /// 认证：用户名、认证方式（SSH）、保存的凭据、密码；Telnet 还有自动登录。
    fn rows_auth(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
        cx: &App,
    ) -> Vec<AnyElement> {
        let vis = protocol_form::visible(form.protocol);
        let telnet = form.protocol == Protocol::Telnet;
        let existing = form.existing.as_ref();
        let effective = if vis.auth_method {
            form.auth
        } else {
            AuthMethod::Password
        };
        let identity = form
            .identity
            .and_then(|id| self.store.identities().iter().find(|item| item.id == id));
        let auth_methods = [AuthMethod::Password, AuthMethod::Key, AuthMethod::Agent];
        let auth_choices: Vec<Choice> = auth_methods
            .iter()
            .map(|m| Choice::new(auth_label(*m), *m == form.auth))
            .collect();
        let fitting: Vec<&zssh_core::model::Identity> = self
            .store
            .identities()
            .iter()
            .filter(|item| identity_fits_auth(item.kind, effective))
            .collect();
        let describe = |item: &zssh_core::model::Identity| {
            format!(
                "{}{}",
                item.name,
                if item.has_secret {
                    ""
                } else {
                    "（没有密码）"
                }
            )
        };
        let mut identity_choices = vec![Choice::new("不用", form.identity.is_none())];
        let mut identity_targets: Vec<Option<i64>> = vec![None];
        for item in &fitting {
            identity_choices.push(Choice::new(describe(item), form.identity == Some(item.id)));
            identity_targets.push(Some(item.id));
        }
        let identity_label = identity.map_or_else(|| "不用".to_owned(), describe);

        let revealable = can_reveal(identity, effective);
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
                .on_click(act(entity, |this, window, cx| {
                    this.host_toggle_password(window, cx)
                }))
        });
        let hint = password_hint(
            existing.is_some(),
            identity.is_some_and(|item| item.has_secret),
        );

        let mut rows: Vec<AnyElement> = vec![
            field_with_error(
                "用户名",
                telnet.then(|| "可以不填".to_owned()),
                Input::new(&form.username),
                form.error_of(FormField::Username),
                tokens,
            )
            .into_any_element(),
        ];
        if vis.auth_method {
            rows.push(
                field(
                    "认证方式",
                    None,
                    select(
                        "host-auth",
                        auth_label(form.auth),
                        auth_choices,
                        pick(entity, move |this, index, window, cx| {
                            if let Some(method) = auth_methods.get(index).copied() {
                                this.host_set_auth(method, window, cx);
                            }
                        }),
                    ),
                    tokens,
                )
                .into_any_element(),
            );
        }
        rows.push(
            v_flex()
                .gap_1p5()
                .child(field_with_error(
                    "保存的凭据",
                    telnet.then(|| "可选，只能选密码凭据".to_owned()),
                    select(
                        "host-identity",
                        identity_label,
                        identity_choices,
                        pick(entity, move |this, index, window, cx| {
                            if let Some(target) = identity_targets.get(index).copied() {
                                this.host_set_identity(target, window, cx);
                            }
                        }),
                    ),
                    form.error_of(FormField::Identity),
                    tokens,
                ))
                .children(
                    form.mismatch_note
                        .clone()
                        .map(|text| error_note(text, tokens.danger)),
                )
                .into_any_element(),
        );
        rows.push(
            field_with_error(
                "密码",
                Some(if telnet {
                    format!("{hint}；开了自动登录才会用到")
                } else {
                    hint.to_owned()
                }),
                h_flex()
                    .w_full()
                    .gap_1()
                    .items_center()
                    .child(div().flex_1().min_w_0().child(Input::new(&form.password)))
                    .children(eye),
                form.error_of(FormField::Password),
                tokens,
            )
            .into_any_element(),
        );
        rows.push(toggle_row(
            entity,
            "host-remember",
            "没有选凭据时，把密码存成一条新凭据",
            None,
            form.remember,
            |form, value| form.remember = value,
            tokens,
        ));
        if telnet {
            rows.extend(self.telnet_auto_login_rows(form, entity, tokens));
        }
        rows
    }

    /// Telnet 的自动登录：开关、用户名 / 密码提示词、错误。
    fn telnet_auto_login_rows(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
    ) -> Vec<AnyElement> {
        let telnet = &form.telnet;
        let mut rows: Vec<AnyElement> = vec![toggle_row(
            entity,
            "telnet-auto-login",
            "自动登录",
            Some(
                "看到用户名 / 密码提示就自动发送；用户名取上面的「用户名」，密码取选中的凭据或上面输入的密码。",
            ),
            telnet.auto_login,
            |form, value| form.telnet.auto_login = value,
            tokens,
        )];
        // 错误紧跟在开关下面（提示词那两个框可能把它挤出可见区域）。
        if let Some(text) = form.error_of(FormField::AutoLogin) {
            rows.push(error_note(text.to_owned(), tokens.danger).into_any_element());
        }
        if telnet.auto_login {
            rows.push(
                v_flex()
                    .gap_3()
                    .child(field(
                        "用户名提示",
                        Some("逗号分隔，不区分大小写".to_owned()),
                        Input::new(&telnet.username_prompts),
                        tokens,
                    ))
                    .child(field(
                        "密码提示",
                        Some("逗号分隔，不区分大小写".to_owned()),
                        Input::new(&telnet.password_prompts),
                        tokens,
                    ))
                    .into_any_element(),
            );
        }
        rows
    }

    /// 连接：代理、跳板机；SSH 还有 X11 转发。
    fn rows_connection(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
    ) -> Vec<AnyElement> {
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
        // 跳板机只能是 SSH 主机（Telnet / 串口主机不能当跳板）。
        let others: Vec<&Host> = self
            .store
            .hosts()
            .iter()
            .filter(|item| Some(item.id) != form.host_id && item.protocol.protocol == Protocol::Ssh)
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

        let mut rows: Vec<AnyElement> = vec![
            field_with_error(
                "代理",
                None,
                select(
                    "host-proxy-mode",
                    proxy_mode_label(form.proxy_mode),
                    mode_choices,
                    pick(entity, move |this, index, _window, cx| {
                        if let Some(mode) = modes.get(index).copied() {
                            this.with_form(cx, |form| form.proxy_mode = mode);
                        }
                    }),
                ),
                form.error_of(FormField::Proxy),
                tokens,
            )
            .into_any_element(),
        ];
        if form.proxy_mode == ProxyMode::Custom {
            rows.push(
                v_flex()
                    .gap_1p5()
                    .child(field(
                        "用哪个代理",
                        None,
                        select(
                            "host-proxy",
                            proxy_label,
                            proxy_choices,
                            pick(entity, move |this, index, _window, cx| {
                                if let Some(target) = proxy_targets.get(index).copied() {
                                    this.with_form(cx, |form| form.proxy = target);
                                }
                            }),
                        ),
                        tokens,
                    ))
                    .when(proxies.is_empty(), |d| {
                        d.child(note("还没有代理，点下面的「新建代理…」添加一个。", tokens))
                    })
                    .child(
                        div().flex().child(
                            Button::new("host-new-proxy")
                                .outline()
                                .compact()
                                .icon(IconName::Plus)
                                .label("新建代理…")
                                .on_click(act(entity, |this, window, cx| {
                                    this.host_open_new_proxy(window, cx);
                                })),
                        ),
                    )
                    .into_any_element(),
            );
        }
        rows.push(
            field_with_error(
                "跳板机",
                Some("只能选 SSH 主机".to_owned()),
                select(
                    "host-jump",
                    jump_label,
                    jump_choices,
                    pick(entity, move |this, index, _window, cx| {
                        if let Some(target) = jump_targets.get(index).copied() {
                            this.with_form(cx, |form| form.jump = target);
                        }
                    }),
                ),
                form.error_of(FormField::Jump),
                tokens,
            )
            .into_any_element(),
        );
        if protocol_form::visible(form.protocol).x11 {
            rows.push(toggle_row(
                entity,
                "host-x11",
                "X11 转发",
                Some("把远程图形程序显示在本机的 X 服务器上（如 VcXsrv）；本机没有时连接会提示安装。"),
                form.x11,
                |form, value| form.x11 = value,
                tokens,
            ));
            if let Some(text) = form.error_of(FormField::X11) {
                rows.push(error_note(text.to_owned(), tokens.danger).into_any_element());
            }
        }
        rows
    }

    /// Telnet 终端：回车模式、本地回显、NAWS、终端类型、编码、纯 TCP。
    fn rows_terminal(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
    ) -> Vec<AnyElement> {
        let telnet = &form.telnet;
        vec![
            field(
                "回车模式",
                Some("按回车键时发送什么".to_owned()),
                enum_select(
                    "telnet-newline",
                    entity,
                    telnet.newline,
                    &TELNET_NEWLINES,
                    protocol_form::telnet_newline_label,
                    |form, value| form.telnet.newline = value,
                ),
                tokens,
            )
            .into_any_element(),
            field(
                "本地回显",
                None,
                enum_select(
                    "telnet-echo",
                    entity,
                    telnet.local_echo,
                    &LOCAL_ECHOES,
                    protocol_form::local_echo_label,
                    |form, value| form.telnet.local_echo = value,
                ),
                tokens,
            )
            .into_any_element(),
            toggle_row(
                entity,
                "telnet-naws",
                "NAWS（把窗口大小告诉服务器）",
                None,
                telnet.naws,
                |form, value| form.telnet.naws = value,
                tokens,
            ),
            field_with_error(
                "终端类型",
                Some("回应服务器的 TTYPE 协商".to_owned()),
                Input::new(&telnet.terminal_type),
                form.error_of(FormField::TerminalType),
                tokens,
            )
            .into_any_element(),
            field(
                "编码",
                None,
                enum_select(
                    "telnet-encoding",
                    entity,
                    telnet.encoding,
                    &ENCODINGS,
                    protocol_form::encoding_label,
                    |form, value| form.telnet.encoding = value,
                ),
                tokens,
            )
            .into_any_element(),
            toggle_row(
                entity,
                "telnet-raw-tcp",
                "纯 TCP 模式",
                Some("不做任何 Telnet 协商，连的是裸 TCP 服务（设备调试口、netcat）时用"),
                telnet.raw_tcp,
                |form, value| form.telnet.raw_tcp = value,
                tokens,
            ),
        ]
    }

    /// 串口的设备（本机列表 + 刷新 / 手填）和波特率：放在「常规」里。
    fn serial_device_rows(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
        cx: &App,
    ) -> Vec<AnyElement> {
        let serial = &form.serial;
        let device_text = serial.device.read(cx).value().to_string();
        let listed = serial
            .ports
            .iter()
            .any(|port| port.device.eq_ignore_ascii_case(device_text.trim()));
        // 没有串口、选了「手动输入」、或框里的设备不在列表里（拔掉了 / 以前手填的）：显示文字框。
        let manual =
            serial.manual || serial.ports.is_empty() || (!device_text.trim().is_empty() && !listed);
        let ports = serial.ports.clone();
        let mut choices: Vec<Choice> = ports
            .iter()
            .map(|port| {
                Choice::new(
                    port.label.clone(),
                    port.device.eq_ignore_ascii_case(device_text.trim()),
                )
            })
            .collect();
        choices.push(Choice::new("手动输入…", manual && !ports.is_empty()));
        let picker: AnyElement = if ports.is_empty() {
            Button::new("serial-device-none")
                .outline()
                .label("未检测到串口")
                .w_full()
                .disabled(true)
                .into_any_element()
        } else {
            let devices: Vec<String> = ports.iter().map(|port| port.device.clone()).collect();
            select(
                "serial-device",
                protocol_form::device_button_label(&device_text, &ports),
                choices,
                pick(entity, move |this, index, window, cx| {
                    match devices.get(index) {
                        Some(device) => this.host_pick_port(device, window, cx),
                        None => this.with_form(cx, |form| form.serial.manual = true),
                    }
                }),
            )
            .into_any_element()
        };
        let device_control = v_flex()
            .w_full()
            .gap_1p5()
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .items_center()
                    .child(div().flex_1().min_w_0().child(picker))
                    .child(
                        Button::new("serial-refresh")
                            .outline()
                            .compact()
                            .icon(IconName::RefreshCw)
                            .tooltip("刷新串口列表")
                            .on_click(act(entity, |this, _window, cx| this.host_refresh_ports(cx))),
                    ),
            )
            .when(manual, |d| d.child(Input::new(&serial.device)))
            .when(ports.is_empty(), |d| {
                d.child(note(
                    "没有检测到串口，可以直接填设备名（例如 COM3）；插上设备后点右边的刷新。",
                    tokens,
                ))
            });
        let baud_choices: Vec<Choice> = COMMON_BAUD_RATES
            .iter()
            .map(|rate| Choice::new(rate.to_string(), false))
            .collect();
        let baud_control = h_flex()
            .w_full()
            .gap_1()
            .items_center()
            .child(div().flex_1().min_w_0().child(Input::new(&serial.baud)))
            .child(div().w(px(104.0)).flex_none().child(select(
                "serial-baud-common",
                "常用值",
                baud_choices,
                pick(entity, move |this, index, window, cx| {
                    if let Some(rate) = COMMON_BAUD_RATES.get(index).copied() {
                        this.host_set_baud(rate, window, cx);
                    }
                }),
            )));
        vec![
            field_with_error(
                "设备",
                None,
                device_control,
                form.error_of(FormField::Device),
                tokens,
            )
            .into_any_element(),
            field_with_error(
                "波特率",
                Some("可以直接填别的数值".to_owned()),
                baud_control,
                form.error_of(FormField::Baud),
                tokens,
            )
            .into_any_element(),
        ]
    }

    /// 串口参数：数据位 / 校验位 / 停止位、流控、换行、本地回显、编码。
    fn rows_serial_params(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
    ) -> Vec<AnyElement> {
        let serial = &form.serial;
        let framing = h_flex()
            .w_full()
            .gap_2()
            .items_start()
            .child(div().flex_1().min_w_0().child(field(
                "数据位",
                None,
                enum_select(
                    "serial-data-bits",
                    entity,
                    serial.data_bits,
                    &DATA_BITS,
                    protocol_form::data_bits_label,
                    |form, value| form.serial.data_bits = value,
                ),
                tokens,
            )))
            .child(div().flex_1().min_w_0().child(field(
                "校验位",
                None,
                enum_select(
                    "serial-parity",
                    entity,
                    serial.parity,
                    &PARITIES,
                    protocol_form::parity_label,
                    |form, value| form.serial.parity = value,
                ),
                tokens,
            )))
            .child(div().flex_1().min_w_0().child(field(
                "停止位",
                None,
                enum_select(
                    "serial-stop-bits",
                    entity,
                    serial.stop_bits,
                    &STOP_BITS,
                    protocol_form::stop_bits_label,
                    |form, value| form.serial.stop_bits = value,
                ),
                tokens,
            )));
        vec![
            v_flex()
                .gap_1p5()
                .child(framing)
                .children(
                    form.error_of(FormField::Framing)
                        .map(|text| error_note(text.to_owned(), tokens.danger)),
                )
                .into_any_element(),
            field(
                "流控",
                None,
                enum_select(
                    "serial-flow",
                    entity,
                    serial.flow_control,
                    &FLOW_CONTROLS,
                    protocol_form::flow_control_label,
                    |form, value| form.serial.flow_control = value,
                ),
                tokens,
            )
            .into_any_element(),
            field(
                "换行发送模式",
                Some("按回车键时发送什么".to_owned()),
                enum_select(
                    "serial-newline",
                    entity,
                    serial.newline,
                    &SERIAL_NEWLINES,
                    protocol_form::serial_newline_label,
                    |form, value| form.serial.newline = value,
                ),
                tokens,
            )
            .into_any_element(),
            toggle_row(
                entity,
                "serial-echo",
                "本地回显",
                Some("设备自己不回显键入的字符时打开"),
                serial.local_echo,
                |form, value| form.serial.local_echo = value,
                tokens,
            ),
            field(
                "编码",
                None,
                enum_select(
                    "serial-encoding",
                    entity,
                    serial.encoding,
                    &ENCODINGS,
                    protocol_form::encoding_label,
                    |form, value| form.serial.encoding = value,
                ),
                tokens,
            )
            .into_any_element(),
        ]
    }

    /// 安全（SSH）：旧算法兼容、已保存的主机指纹。
    fn rows_security(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
    ) -> Vec<AnyElement> {
        vec![
            v_flex()
                .w_full()
                .gap_2()
                .p_3()
                .rounded_md()
                .border_1()
                .border_color(tokens.border)
                .child(toggle_row(
                    entity,
                    "host-legacy",
                    "旧算法兼容",
                    None,
                    form.legacy,
                    |form, value| form.legacy = value,
                    tokens,
                ))
                .child(error_note(
                    "仅用于老服务器：允许 SHA1 签名的 ssh-rsa、老的密钥交换和 CBC 加密。安全性较低，只在这台机器连不上、并且确认对方是老系统时才打开。",
                    tokens.danger,
                ))
                .into_any_element(),
            self.fingerprint_box(form, entity, tokens),
        ]
    }

    /// 已保存的主机指纹和「清除」（这是对话框里唯一一处非编辑的操作，所以放在「安全」类别里）。
    fn fingerprint_box(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
    ) -> AnyElement {
        v_flex()
            .w_full()
            .gap_2()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(tokens.border)
            .child(
                div()
                    .typo(Role::FieldLabel, tokens)
                    .child("已保存的主机指纹"),
            )
            .child(match form.existing.as_ref() {
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
                        div().flex().child(
                            Button::new("host-forget-fingerprint")
                                .outline()
                                .compact()
                                .label("清除已保存的指纹")
                                .on_click(act(entity, |this, window, cx| {
                                    this.host_forget_fingerprint(window, cx);
                                })),
                        ),
                    )
                    .child(note(
                        "服务器换过密钥（比如重装系统）时用；如果是被人冒充，先查清楚原因再清。",
                        tokens,
                    ))
                    .into_any_element(),
                None => note("保存之后才能看到。", tokens).into_any_element(),
            })
            .into_any_element()
    }

    /// AI 记忆（只有已保存的 SSH 主机有）：「确定」会连同这里没保存的修改一起存。
    fn rows_memory(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
        cx: &App,
    ) -> Vec<AnyElement> {
        let Some(memory) = form.memory.as_ref() else {
            return Vec::new();
        };
        let current = memory.input.read(cx).value().to_string();
        let dirty = current != memory.saved;
        let length = current.chars().count();
        let stamp = memory
            .updated_at
            .map(|at| crate::hosts_demo::memory_summary(Some(at), crate::hosts_demo::now_unix()))
            .unwrap_or_default();
        vec![
            v_flex()
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
                                .typo(Role::Caption, tokens)
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
                                act(entity, |this, window, cx| {
                                    this.host_memory_save(window, cx, true)
                                }),
                            ))
                        })
                        .child(
                            text_button(
                                "host-memory-save",
                                "保存记忆",
                                ButtonLook::Outline,
                                act(entity, |this, window, cx| {
                                    this.host_memory_save(window, cx, false)
                                }),
                            )
                            .disabled(!dirty),
                        ),
                )
                .into_any_element(),
        ]
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{Category, HostPropsDialog, MainWindow, Overlay};

    #[test]
    fn the_dialog_is_never_taller_than_the_window_allows() {
        // 窗口高 1000：最高 520；窗口矮：跟着缩，但不低于 260（再矮就靠内容区滚动）。
        assert_eq!(MainWindow::host_props_height(1000.0), 520.0);
        assert_eq!(MainWindow::host_props_height(520.0), 432.0);
        assert_eq!(MainWindow::host_props_height(300.0), 260.0);
    }

    #[test]
    fn a_new_dialog_starts_on_general_with_no_overlay() {
        let dialog = HostPropsDialog::new("snap".to_owned());
        assert_eq!(dialog.category, Category::General);
        assert!(matches!(dialog.overlay, Overlay::None));
        assert_eq!(dialog.initial, "snap");
    }
}
