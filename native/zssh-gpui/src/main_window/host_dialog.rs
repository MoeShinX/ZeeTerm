//! 主机「属性」对话框（V2-U13 起是单栏表单）：在当前页面上居中弹出，一个可滚动的单栏表单从上到下一屏看完——
//! 名称、协议（分段按钮）、地址 + 端口、认证方式（分段按钮）、用户名 + 密码、分组、连接方式（分段按钮：
//! 直接连接 / SSH 跳板 / 代理访问）、可折叠的「高级设置」、可折叠的「AI 记忆」；底部固定：左边「测试连接」，
//! 右边「取消 / 连接 / 确定」。版式借自别家软件的主机表单，字段全是 ZeeTerm 原来表单里的，一个没加。
//!
//! 外壳（遮罩、卡片）在 `modal::render_modal`；表单状态、保存、滚动在 `host_panel`，有哪些行、错误落在哪一行
//! 在 `host_props`（纯函数有测试），连接方式三段和保存规则在 `host_form`，测试连接在 `host_test`。这里只管画，
//! 以及对话框自己的两层小浮层：
//! - 「新建代理…」：在属性对话框上再叠一层代理新建对话框，保存后回填到主机表单，主机表单里没保存的编辑原样留着；
//! - 有未保存修改时取消 / Esc：先问一句是否放弃。

use ::gpui::{AnyElement, Entity, SharedString, hsla, px};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState, Textarea};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::ai::host_memory::MAX_MEMORY_CHARS;
use zssh_core::model::{AuthMethod, Host, IdentityKind, Protocol, ProxyKind, ProxyMode};

use super::MainWindow;
use super::host_form::{
    AuthField, ConnMode, auth_fields, auth_label, can_reveal, password_hint, password_identity,
    pickable_identities,
};
use super::host_panel::HostForm;
use super::host_props::{self, AdvRow, Row};
use super::page_logic::{flatten_groups, ungrouped_option_label};
use super::protocol_form::{
    self, COMMON_BAUD_RATES, DATA_BITS, ENCODINGS, FLOW_CONTROLS, FormField, LOCAL_ECHOES,
    PARITIES, PROTOCOLS, SERIAL_NEWLINES, STOP_BITS, TELNET_NEWLINES,
};
use super::proxy_panel::{PROXY_KINDS, ProxyInput, build_proxy_draft};
use super::widgets::{
    ButtonLook, Choice, act, check_card, error_note, field, field_with_error, note, pick,
    required_label, segmented_bar, select, text_button,
};
use crate::app_theme::Tokens;
use crate::typography::{Role, Typo as _};

/// 对话框卡片的宽度（单栏表单：约 560–600）。
pub const DIALOG_WIDTH: f32 = 580.0;

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
    /// 刚打开时表单的快照：现在的快照和它不一样就是有没保存的修改。
    pub initial: String,
    pub overlay: Overlay,
}

impl HostPropsDialog {
    pub fn new(initial: String) -> Self {
        Self {
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

/// 「枚举值下拉」：按钮上写当前值，菜单里列全部，选中后交给 `set` 改表单。
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

/// 开关卡片：勾选框 + 标题 + 一行说明，勾选改表单里的一个布尔值。
fn toggle_card(
    entity: &Entity<MainWindow>,
    id: &'static str,
    title: &'static str,
    hint: Option<&'static str>,
    checked: bool,
    set: fn(&mut HostForm, bool),
    tokens: &Tokens,
) -> AnyElement {
    let entity = entity.clone();
    check_card(
        id,
        title,
        hint,
        checked,
        tokens,
        move |value, _window, cx| {
            entity.update(cx, |this, cx| {
                this.with_form(cx, |form| set(form, value));
            });
        },
    )
    .into_any_element()
}

/// 带标签的一行字段：标签（必填的带红星）+ 控件 + 控件下面的红字错误。
fn labelled(
    label: impl IntoElement,
    control: impl IntoElement,
    error: Option<&str>,
    tokens: &Tokens,
) -> AnyElement {
    v_flex()
        .w_full()
        .min_w_0()
        .gap_1p5()
        .child(label)
        .child(control)
        .children(error.map(|text| error_note(text.to_owned(), tokens.danger)))
        .into_any_element()
}

/// 可折叠区的标题行：箭头 + 文字，整行可点。
fn fold_header(
    id: &'static str,
    title: &str,
    detail: Option<String>,
    open: bool,
    tokens: &Tokens,
    on_toggle: impl Fn(&mut Window, &mut App) + 'static,
) -> AnyElement {
    let hover = tokens.hover;
    h_flex()
        .id(id)
        .w_full()
        .gap_2()
        .items_center()
        .py_1()
        .cursor_pointer()
        .rounded_md()
        .hover(move |style| style.bg(hover))
        .child(
            Icon::new(if open {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .size_4()
            .text_color(tokens.text_muted),
        )
        .child(
            div()
                .typo(Role::FieldLabel, tokens)
                .child(SharedString::from(title.to_owned())),
        )
        .children(detail.map(|text| {
            div()
                .min_w_0()
                .truncate()
                .typo(Role::Caption, tokens)
                .child(SharedString::from(text))
        }))
        .on_click(move |_, window, cx| on_toggle(window, cx))
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

    /// 对话框卡片里的内容：标题、可滚动的单栏表单、底部按钮栏（固定）。高度固定（不因协议不同而跳），
    /// 表单字段多时自己滚动。
    pub(super) fn host_props_body(
        &self,
        _dialog: &HostPropsDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(form) = self.host_form.as_ref() else {
            return div().into_any_element();
        };
        let entity = cx.entity();
        let title = host_props::dialog_title(form.existing.as_ref().map(|host| host.name.as_str()));
        let body_h = Self::host_props_height(self.window_height);
        let rows: Vec<AnyElement> = host_props::rows(form.layout_state())
            .into_iter()
            .map(|row| self.form_row(row, form, &entity, tokens, cx))
            .collect();
        // 表单滚动区：行就是它的直接子元素，出错时按行下标滚过去。
        let scroll = div()
            .flex_1()
            .min_h_0()
            .w_full()
            .border_t_1()
            .border_color(tokens.border)
            .child(
                v_flex()
                    .id("host-props-scroll")
                    .size_full()
                    .gap_3()
                    .pt_3()
                    .pr_3()
                    .pb_2()
                    .overflow_y_scroll()
                    .track_scroll(&form.scroll)
                    .children(rows),
            );

        // 结果栏：测试结果（绿 / 红，可复制详情）；找不到字段的保存错误也显示在这里。
        let result: Option<AnyElement> =
            match (&form.test.result, form.error_of(FormField::General)) {
                (Some(outcome), _) => {
                    let color = if outcome.ok {
                        tokens.dot_connected
                    } else if outcome.stage.is_some() {
                        tokens.danger
                    } else {
                        tokens.text_muted
                    };
                    let copy = entity.clone();
                    Some(
                        h_flex()
                            .id("host-test-result")
                            .w_full()
                            .flex_none()
                            .gap_2()
                            .items_start()
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .border_1()
                            .border_color(color)
                            .child(
                                Icon::new(if outcome.ok {
                                    IconName::CircleCheck
                                } else {
                                    IconName::CircleX
                                })
                                .size_4()
                                .text_color(color),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .typo(Role::Body, tokens)
                                    .text_color(color)
                                    .child(SharedString::from(outcome.headline.clone())),
                            )
                            .child(text_button(
                                "host-test-copy",
                                "复制详情",
                                ButtonLook::Ghost,
                                move |_, window, cx| {
                                    copy.update(cx, |this, cx| this.host_test_copy(window, cx));
                                },
                            ))
                            .into_any_element(),
                    )
                }
                (None, Some(text)) => {
                    Some(error_note(text.to_owned(), tokens.danger).into_any_element())
                }
                (None, None) => None,
            };

        let (cancel, connect, ok, test, stop) = (
            entity.clone(),
            entity.clone(),
            entity.clone(),
            entity.clone(),
            entity.clone(),
        );
        let running = form.test.is_running();
        // 右边留出和表单滚动区一样的 12 像素（那里给滚动条），按钮栏才和输入框的右边缘对齐。
        let footer = h_flex()
            .w_full()
            .flex_none()
            .gap_2()
            .items_center()
            .pr_3()
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        text_button(
                            "host-test",
                            if running {
                                "测试中…"
                            } else {
                                "测试连接"
                            },
                            ButtonLook::Outline,
                            move |_, window, cx| {
                                test.update(cx, |this, cx| this.host_test_connection(window, cx));
                            },
                        )
                        .disabled(running),
                    )
                    .when(running, |d| {
                        d.child(text_button(
                            "host-test-cancel",
                            "取消",
                            ButtonLook::Ghost,
                            move |_, _window, cx| {
                                stop.update(cx, |this, cx| this.host_test_cancel(cx));
                            },
                        ))
                    }),
            )
            .child(div().flex_1())
            .child(text_button(
                "host-cancel",
                "取消",
                ButtonLook::Outline,
                move |_, window, cx| {
                    cancel.update(cx, |this, cx| this.host_props_cancel(window, cx));
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
                "host-ok",
                "确定",
                ButtonLook::Primary,
                move |_, window, cx| {
                    ok.update(cx, |this, cx| this.host_form_commit(false, window, cx));
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
            .child(scroll)
            .children(result.map(|row| div().pr_3().child(row)))
            .child(footer)
            .into_any_element()
    }

    /// 对话框内容区的高度：最高 680，窗口矮时跟着缩（卡片的内边距和上下留白一并扣掉）。
    pub(crate) fn host_props_height(window_height: f32) -> f32 {
        (window_height - 48.0 - 40.0).clamp(300.0, 680.0)
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

    // ---- 画：表单的各行 ----

    fn form_row(
        &self,
        row: Row,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
        cx: &App,
    ) -> AnyElement {
        match row {
            Row::Name => labelled(
                required_label("名称", tokens),
                Input::new(&form.name),
                form.error_of(FormField::Name),
                tokens,
            ),
            Row::Protocol => self.protocol_row(form, entity, tokens),
            Row::Endpoint => self.endpoint_row(form, entity, tokens, cx),
            Row::AuthMethod => self.auth_method_row(form, entity, tokens),
            Row::Credentials => self.credentials_row(form, entity, tokens, cx),
            Row::AutoLogin => self.auto_login_row(form, entity, tokens),
            Row::AutoLoginPrompts => h_flex()
                .w_full()
                .gap_3()
                .items_start()
                .child(div().flex_1().min_w_0().child(field(
                    "用户名提示",
                    Some("逗号分隔，不区分大小写".to_owned()),
                    Input::new(&form.telnet.username_prompts),
                    tokens,
                )))
                .child(div().flex_1().min_w_0().child(field(
                    "密码提示",
                    Some("逗号分隔，不区分大小写".to_owned()),
                    Input::new(&form.telnet.password_prompts),
                    tokens,
                )))
                .into_any_element(),
            Row::Group => self.group_row(form, entity, tokens),
            Row::Routing => self.routing_row(form, entity, tokens),
            Row::RoutingDetail => self.routing_detail_row(form, entity, tokens),
            Row::AdvancedHeader => {
                let toggle = entity.clone();
                fold_header(
                    "host-advanced-toggle",
                    "高级设置",
                    None,
                    form.advanced_open,
                    tokens,
                    move |_window, cx| {
                        toggle.update(cx, |this, cx| this.host_toggle_advanced(cx));
                    },
                )
            }
            Row::Advanced(item) => self.advanced_row(item, form, entity, tokens),
            Row::MemoryHeader => {
                let toggle = entity.clone();
                let length = form
                    .memory
                    .as_ref()
                    .map_or(0, |memory| memory.input.read(cx).value().chars().count());
                fold_header(
                    "host-memory-toggle",
                    "AI 记忆",
                    (length > 0).then(|| format!("{length} 字")),
                    form.memory_open,
                    tokens,
                    move |_window, cx| {
                        toggle.update(cx, |this, cx| this.host_toggle_memory(cx));
                    },
                )
            }
            Row::MemoryBody => self
                .rows_memory(form, entity, tokens, cx)
                .into_iter()
                .next()
                .unwrap_or_else(|| div().into_any_element()),
        }
    }

    /// 协议：整行等分的分段按钮（串口主机和网络主机不能互相转换，对应的段置灰）。
    fn protocol_row(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
    ) -> AnyElement {
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
        field(
            "协议",
            None,
            segmented_bar(
                "host-protocol",
                options,
                tokens,
                pick(entity, move |this, index, window, cx| {
                    if let Some(protocol) = PROTOCOLS.get(index).copied() {
                        this.host_set_protocol(protocol, window, cx);
                    }
                }),
            ),
            tokens,
        )
        .into_any_element()
    }

    /// 地址 + 端口（同一行，地址占大头）；串口是设备（含刷新）+ 波特率。
    fn endpoint_row(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
        cx: &App,
    ) -> AnyElement {
        if form.protocol == Protocol::Serial {
            let mut parts = self
                .serial_device_rows(form, entity, tokens, cx)
                .into_iter();
            let (device, baud) = (parts.next(), parts.next());
            return h_flex()
                .w_full()
                .gap_3()
                .items_start()
                .child(div().flex_1().min_w_0().children(device))
                .child(div().w(px(210.0)).flex_none().children(baud))
                .into_any_element();
        }
        h_flex()
            .w_full()
            .gap_3()
            .items_start()
            .child(div().flex_1().min_w_0().child(labelled(
                required_label("地址", tokens),
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
            .into_any_element()
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

    /// 认证方式（SSH）：整行等分的分段按钮 密码 / 私钥 / ssh-agent。
    fn auth_method_row(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
    ) -> AnyElement {
        let methods = [AuthMethod::Password, AuthMethod::Key, AuthMethod::Agent];
        let options: Vec<(String, bool, Option<&'static str>)> = methods
            .iter()
            .map(|method| (auth_label(*method).to_owned(), *method == form.auth, None))
            .collect();
        field(
            "认证方式",
            None,
            segmented_bar(
                "host-auth",
                options,
                tokens,
                pick(entity, move |this, index, window, cx| {
                    if let Some(method) = methods.get(index).copied() {
                        this.host_set_auth(method, window, cx);
                    }
                }),
            ),
            tokens,
        )
        .into_any_element()
    }

    /// 用户名 + 密码（并排）；私钥认证是用户名 + 密钥下拉，ssh-agent 是用户名 + 可选的 agent 凭据。
    /// 字段由 `host_form::auth_fields` 定（V2-U11 的语义不变：密码归主机，密钥从凭据库选）。
    fn credentials_row(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
        cx: &App,
    ) -> AnyElement {
        let telnet = form.protocol == Protocol::Telnet;
        let effective = form.effective_auth();
        let second = auth_fields(form.protocol, form.auth)
            .into_iter()
            .find(|item| {
                matches!(
                    item,
                    AuthField::Password | AuthField::KeyPicker | AuthField::AgentPicker
                )
            });
        let right = match second {
            Some(AuthField::Password) => {
                Some(self.password_cell(form, entity, tokens, cx, effective))
            }
            Some(AuthField::KeyPicker | AuthField::AgentPicker) => {
                Some(self.identity_picker_cell(form, entity, tokens, effective))
            }
            _ => None,
        };
        h_flex()
            .w_full()
            .gap_3()
            .items_start()
            .child(div().flex_1().min_w_0().child(field_with_error(
                "用户名",
                telnet.then(|| "可以不填".to_owned()),
                Input::new(&form.username),
                form.error_of(FormField::Username),
                tokens,
            )))
            .child(div().flex_1().min_w_0().children(right))
            .into_any_element()
    }

    /// 密码框（带小眼睛）：存的是这台主机自己的密码，留空表示不改。
    fn password_cell(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
        cx: &App,
        effective: AuthMethod,
    ) -> AnyElement {
        let identity = password_identity(form.existing.as_ref(), self.store.identities());
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
            form.existing.is_some(),
            identity.is_some_and(|item| item.has_secret),
        );
        v_flex()
            .gap_1p5()
            .child(field_with_error(
                "密码",
                None,
                h_flex()
                    .w_full()
                    .gap_1()
                    .items_center()
                    .child(div().flex_1().min_w_0().child(Input::new(&form.password)))
                    .children(eye),
                form.error_of(FormField::Password),
                tokens,
            ))
            .child(note(hint, tokens))
            // 核心库报的凭据错误（密码认证下没有下拉可挂）显示在密码框下面。
            .children(
                form.error_of(FormField::Identity)
                    .map(|text| error_note(text.to_owned(), tokens.danger)),
            )
            .into_any_element()
    }

    /// 私钥认证：从凭据库选密钥（只列密钥类）；ssh-agent 认证：可选的 agent 凭据。
    fn identity_picker_cell(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
        effective: AuthMethod,
    ) -> AnyElement {
        let key = effective == AuthMethod::Key;
        let fitting = pickable_identities(self.store.identities(), effective);
        let describe = |item: &zssh_core::model::Identity| {
            if item.kind == IdentityKind::Agent || item.has_secret || item.key_path.is_some() {
                item.name.clone()
            } else {
                format!("{}（没有内容）", item.name)
            }
        };
        let empty_label = if key { "（请选择）" } else { "不用" };
        let mut choices = vec![Choice::new(empty_label, form.identity.is_none())];
        let mut targets: Vec<Option<i64>> = vec![None];
        for item in &fitting {
            choices.push(Choice::new(describe(item), form.identity == Some(item.id)));
            targets.push(Some(item.id));
        }
        let label = form
            .identity
            .and_then(|id| fitting.iter().find(|item| item.id == id))
            .map_or_else(|| empty_label.to_owned(), |item| describe(item));
        let hint = if key {
            fitting
                .is_empty()
                .then(|| "凭据库里还没有密钥：先到「管理 → 凭据」导入或生成一把".to_owned())
        } else {
            Some("可选".to_owned())
        };
        v_flex()
            .gap_1p5()
            .child(field_with_error(
                if key { "密钥" } else { "ssh-agent 凭据" },
                None,
                select(
                    "host-identity",
                    label,
                    choices,
                    pick(entity, move |this, index, window, cx| {
                        if let Some(target) = targets.get(index).copied() {
                            this.host_set_identity(target, window, cx);
                        }
                    }),
                ),
                form.error_of(FormField::Identity),
                tokens,
            ))
            .children(hint.map(|text| note(text, tokens)))
            .children(
                form.mismatch_note
                    .clone()
                    .map(|text| error_note(text, tokens.danger)),
            )
            .into_any_element()
    }

    /// Telnet 的自动登录开关卡片（开着时下面一行是用户名 / 密码提示词）。
    fn auto_login_row(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
    ) -> AnyElement {
        v_flex()
            .w_full()
            .gap_1p5()
            .child(toggle_card(
                entity,
                "telnet-auto-login",
                "自动登录",
                Some("看到用户名 / 密码提示就自动发送，用上面填的用户名和密码"),
                form.telnet.auto_login,
                |form, value| form.telnet.auto_login = value,
                tokens,
            ))
            .children(
                form.error_of(FormField::AutoLogin)
                    .map(|text| error_note(text.to_owned(), tokens.danger)),
            )
            .into_any_element()
    }

    /// 连接方式：直接连接 / SSH 跳板 / 代理访问（映射规则见 `host_form::ConnMode`）。
    fn routing_row(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
    ) -> AnyElement {
        let options: Vec<(String, bool, Option<&'static str>)> = ConnMode::ALL
            .iter()
            .map(|mode| (mode.label().to_owned(), *mode == form.conn_mode, None))
            .collect();
        field(
            "连接方式",
            None,
            segmented_bar(
                "host-routing",
                options,
                tokens,
                pick(entity, move |this, index, _window, cx| {
                    if let Some(mode) = ConnMode::ALL.get(index).copied() {
                        this.host_set_conn_mode(mode, cx);
                    }
                }),
            ),
            tokens,
        )
        .into_any_element()
    }

    /// 连接方式下面的细节：代理访问 = 跟随默认 / 指定代理 + 选择 + 新建代理…；SSH 跳板 = 选跳板机
    /// （旧数据里同时有代理的，下面也显示代理设置）。
    fn routing_detail_row(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
    ) -> AnyElement {
        match form.conn_mode {
            ConnMode::Direct => div().into_any_element(),
            ConnMode::Proxy => self.proxy_block(form, entity, tokens),
            ConnMode::Jump => {
                let others: Vec<&Host> = self
                    .store
                    .hosts()
                    .iter()
                    .filter(|item| {
                        Some(item.id) != form.host_id && item.protocol.protocol == Protocol::Ssh
                    })
                    .collect();
                let mut jump_choices = vec![Choice::new("（请选择）", form.jump.is_none())];
                let mut jump_targets: Vec<Option<i64>> = vec![None];
                for item in &others {
                    jump_choices.push(Choice::new(item.name.clone(), form.jump == Some(item.id)));
                    jump_targets.push(Some(item.id));
                }
                let jump_label = form
                    .jump
                    .and_then(|id| others.iter().find(|h| h.id == id))
                    .map_or_else(|| "（请选择）".to_owned(), |h| h.name.clone());
                v_flex()
                    .w_full()
                    .gap_3()
                    .child(field_with_error(
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
                    ))
                    .when(form.jump_keeps_proxy, |d| {
                        d.child(self.proxy_block(form, entity, tokens)).child(note(
                            "这台主机原来同时设了代理，保存时照旧保留；经跳板机连接时只有第一跳（跳板机自己）的代理生效。",
                            tokens,
                        ))
                    })
                    .into_any_element()
            }
        }
    }

    /// 代理设置：跟随默认代理 / 指定代理，指定时再选哪个并可以新建。
    fn proxy_block(
        &self,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
    ) -> AnyElement {
        let proxies = self.store.proxies();
        let modes = [ProxyMode::Default, ProxyMode::Custom];
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
        // 「跟随默认代理」下面说清现在到底走不走代理。
        let default_id = self
            .store
            .db()
            .ok()
            .and_then(|db| db.default_proxy_id().ok().flatten());
        let default_note = match default_id.and_then(|id| proxies.iter().find(|p| p.id == id)) {
            Some(proxy) => format!(
                "现在的默认代理：{}（在「管理 → 代理」里设置）。",
                proxy.name
            ),
            None => {
                "现在还没有设置默认代理，连接时等于直接连接（在「管理 → 代理」里设置）。".to_owned()
            }
        };
        let mut block = v_flex().w_full().gap_3().child(field_with_error(
            "代理",
            None,
            select(
                "host-proxy-mode",
                proxy_mode_label(if form.proxy_mode == ProxyMode::Custom {
                    ProxyMode::Custom
                } else {
                    ProxyMode::Default
                }),
                mode_choices,
                pick(entity, move |this, index, _window, cx| {
                    if let Some(mode) = modes.get(index).copied() {
                        this.with_form(cx, |form| form.proxy_mode = mode);
                    }
                }),
            ),
            form.error_of(FormField::Proxy),
            tokens,
        ));
        if form.proxy_mode == ProxyMode::Custom {
            block = block
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
                );
        } else {
            block = block.child(note(default_note, tokens));
        }
        block.into_any_element()
    }

    /// 高级设置里的一行（按协议）。
    fn advanced_row(
        &self,
        item: AdvRow,
        form: &HostForm,
        entity: &Entity<MainWindow>,
        tokens: &Tokens,
    ) -> AnyElement {
        let telnet = &form.telnet;
        let serial = &form.serial;
        let two = |left: AnyElement, right: AnyElement| {
            h_flex()
                .w_full()
                .gap_3()
                .items_start()
                .child(div().flex_1().min_w_0().child(left))
                .child(div().flex_1().min_w_0().child(right))
                .into_any_element()
        };
        match item {
            AdvRow::X11 => v_flex()
                .w_full()
                .gap_1p5()
                .child(toggle_card(
                    entity,
                    "host-x11",
                    "X11 转发",
                    Some("把远程图形程序显示在本机的 X 服务器上（如 VcXsrv）；本机没有时连接会提示安装。"),
                    form.x11,
                    |form, value| form.x11 = value,
                    tokens,
                ))
                .children(
                    form.error_of(FormField::X11)
                        .map(|text| error_note(text.to_owned(), tokens.danger)),
                )
                .into_any_element(),
            AdvRow::Legacy => toggle_card(
                entity,
                "host-legacy",
                "旧算法兼容",
                Some("仅用于老服务器：允许 SHA1 的 ssh-rsa、老的密钥交换和 CBC 加密。安全性较低，只在这台机器连不上、并且确认对方是老系统时才打开。"),
                form.legacy,
                |form, value| form.legacy = value,
                tokens,
            ),
            AdvRow::Fingerprint => self.fingerprint_box(form, entity, tokens),
            AdvRow::TelnetKeys => two(
                field(
                    "回车模式",
                    None,
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
            ),
            AdvRow::TelnetTerm => two(
                field_with_error(
                    "终端类型",
                    None,
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
            ),
            AdvRow::TelnetNaws => toggle_card(
                entity,
                "telnet-naws",
                "NAWS",
                Some("把窗口大小告诉服务器"),
                telnet.naws,
                |form, value| form.telnet.naws = value,
                tokens,
            ),
            AdvRow::TelnetRaw => toggle_card(
                entity,
                "telnet-raw-tcp",
                "纯 TCP 模式",
                Some("不做任何 Telnet 协商，连的是裸 TCP 服务（设备调试口、netcat）时用"),
                telnet.raw_tcp,
                |form, value| form.telnet.raw_tcp = value,
                tokens,
            ),
            AdvRow::SerialFraming => v_flex()
                .w_full()
                .gap_1p5()
                .child(
                    h_flex()
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
                        ))),
                )
                .children(
                    form.error_of(FormField::Framing)
                        .map(|text| error_note(text.to_owned(), tokens.danger)),
                )
                .into_any_element(),
            AdvRow::SerialFlow => two(
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
                    None,
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
            ),
            AdvRow::SerialEncoding => field(
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
            AdvRow::SerialEcho => toggle_card(
                entity,
                "serial-echo",
                "本地回显",
                Some("设备自己不回显键入的字符时打开"),
                serial.local_echo,
                |form, value| form.serial.local_echo = value,
                tokens,
            ),
        }
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
                None,
                baud_control,
                form.error_of(FormField::Baud),
                tokens,
            )
            .into_any_element(),
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
                                .child(SharedString::from(
                                    [
                                        stamp.clone(),
                                        if length > 0 {
                                            format!("{length}/{MAX_MEMORY_CHARS}")
                                        } else {
                                            String::new()
                                        },
                                    ]
                                    .into_iter()
                                    .filter(|part| !part.is_empty())
                                    .collect::<Vec<_>>()
                                    .join(" · "),
                                )),
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
    use super::{HostPropsDialog, MainWindow, Overlay};

    #[test]
    fn the_dialog_is_never_taller_than_the_window_allows() {
        // 窗口高 1000：最高 680；窗口矮：跟着缩，但不低于 300（再矮就靠表单区滚动）。
        assert_eq!(MainWindow::host_props_height(1000.0), 680.0);
        assert_eq!(MainWindow::host_props_height(600.0), 512.0);
        assert_eq!(MainWindow::host_props_height(300.0), 300.0);
    }

    #[test]
    fn a_new_dialog_starts_with_no_overlay_and_remembers_its_snapshot() {
        let dialog = HostPropsDialog::new("snap".to_owned());
        assert!(matches!(dialog.overlay, Overlay::None));
        assert_eq!(dialog.initial, "snap");
    }
}
