//! 独立窗口的外壳：工具栏、目标主机、输入行、底部提示和「全部放行」确认框。
//!
//! 嵌在主窗口右侧面板里的紧凑版在 `embed`。

use super::*;

/// 独立窗口里「全部放行」的确认框：外壳和主窗口的确认框同一套（遮罩 + 居中卡片）。
pub(super) fn allow_all_dialog(
    entity: &Entity<AgentChatDemo>,
    window: &Window,
    cx: &App,
) -> AnyElement {
    use crate::main_window::widgets::{ButtonLook, modal_card, modal_scrim, text_button};
    use crate::typography::{Role, Typo as _};

    let tokens = Tokens::current(cx);
    let viewport = window.viewport_size();
    let scrim_cancel = entity.clone();
    let cancel = entity.clone();
    let confirm = entity.clone();
    let dialog = modal_scrim("allow-all-scrim", false, move |_, cx| {
        scrim_cancel.update(cx, |this, cx| this.cancel_allow_all(cx));
    })
    .child(
        modal_card("allow-all-card", 420.0, false, &tokens)
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .typo(Role::GroupTitle, &tokens)
                            .child(t(ChatMsg::AllowAllTitle)),
                    )
                    .child(
                        div()
                            .typo(Role::Body, &tokens)
                            .child(t(ChatMsg::AllowAllMessage)),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(text_button(
                        "allow-all-cancel",
                        t(ChatMsg::Cancel),
                        ButtonLook::Outline,
                        move |_, _, cx| {
                            cancel.update(cx, |this, cx| this.cancel_allow_all(cx));
                        },
                    ))
                    .child(text_button(
                        "allow-all-ok",
                        t(ChatMsg::ModeAuto),
                        ButtonLook::Danger,
                        move |_, _, cx| {
                            confirm.update(cx, |this, cx| this.confirm_allow_all(cx));
                        },
                    )),
            ),
    );
    deferred(
        anchored()
            .position(::gpui::point(::gpui::px(0.0), ::gpui::px(0.0)))
            .anchor(::gpui::Anchor::TopLeft)
            .child(
                div()
                    .relative()
                    .w(viewport.width)
                    .h(viewport.height)
                    .child(dialog),
            ),
    )
    .with_priority(gpui_kit::base::POPUP_PRIORITY)
    .into_any_element()
}

/// 独立窗口的输入框 / 发送按钮要不要禁用：智能体不可用（`blocked`）时禁用；
/// 回合进行中，真实模式不禁用（和面板一样，Enter 排队），离线演示脚本没有队列，仍然禁用。
pub(super) fn input_disabled(real: bool, running: bool, blocked: bool) -> bool {
    blocked || (running && !real)
}

impl AgentChatDemo {
    pub(super) fn render_window(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.chat.is_running();
        let blocked = self.real && self.availability().blocks_input();
        let banners = if self.real {
            self.agent_banners(cx)
        } else {
            Vec::new()
        };
        let body_area = self.conversation_body(cx);

        v_flex()
            .size_full()
            .p_3()
            .gap_2()
            .track_focus(&self.focus)
            .bg(cx.theme().background)
            .child(self.window_toolbar(running, cx))
            .when(self.real, |column| {
                column.child(self.host_selection_row(cx))
            })
            .children(banners)
            .child(body_area)
            .child(self.input_row(running, blocked, cx))
            .child(self.footer_hints(cx))
    }

    /// 标题栏：窗口名、离线标记、智能体 / 模型、执行模式和「新对话 / 示例 / 历史 / 停止」。
    fn window_toolbar(&self, running: bool, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let mode_entity = entity.clone();
        let new_entity = entity.clone();
        let sample_entity = entity.clone();
        let stop_entity = entity.clone();
        let real_entity = entity.clone();
        let agent_entity = entity.clone();
        let model_entity = entity.clone();
        let history_entity = entity;

        h_flex()
            .gap_2()
            .items_center()
            .child(div().text_lg().child(t(ChatMsg::WindowTitle)))
            .when(!self.real, |row| {
                row.child(
                    div()
                        .px_2()
                        .py_0p5()
                        .rounded_full()
                        .border_1()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().muted)
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(t(ChatMsg::OfflineDemo)),
                )
            })
            .when(demo_toggle_available(), |row| {
                row.child(
                    Button::new("agent-chat-real")
                        .label(if self.real {
                            t(ChatMsg::OfflineDemo)
                        } else {
                            t(ChatMsg::RealAgent)
                        })
                        .compact()
                        .on_click(move |_, _, cx| {
                            real_entity.update(cx, |this, cx| this.toggle_real(cx));
                        }),
                )
            })
            .when(self.real, |row| {
                row.child(
                    Button::new("agent-chat-agent")
                        .label(format!(
                            "{}{}",
                            t(ChatMsg::AgentPrefix),
                            self.agent_label_now()
                        ))
                        .compact()
                        .on_click(move |_, _, cx| {
                            agent_entity.update(cx, |this, cx| this.cycle_agent(cx));
                        }),
                )
                .child(
                    Button::new("agent-chat-model")
                        .label(model_label(&self.models, self.model_index))
                        .compact()
                        .on_click(move |_, _, cx| {
                            model_entity.update(cx, |this, cx| this.cycle_model(cx));
                        }),
                )
            })
            .child(div().flex_1())
            .child({
                let current = self.current_mode();
                Button::new("agent-chat-mode")
                    .label(format!("{}{}", t(ChatMsg::ModePrefix), mode_label(current)))
                    .compact()
                    .dropdown_caret(true)
                    .dropdown_menu_with_anchor(
                        ::gpui::Anchor::BottomRight,
                        move |menu, _window, _cx| {
                            let mut menu = menu.min_w(px(200.0));
                            for mode in [ExecMode::Manual, ExecMode::ReadOnly, ExecMode::Auto] {
                                let entity = mode_entity.clone();
                                menu = menu.item(
                                    menu_kit::choice(mode_label(mode), current == mode).on_click(
                                        move |_: &ClickEvent, _window, cx| {
                                            entity.update(cx, |this, cx| {
                                                this.request_mode(mode, cx);
                                            });
                                        },
                                    ),
                                );
                            }
                            menu
                        },
                    )
            })
            .child(
                Button::new("agent-chat-new")
                    .label(t(ChatMsg::NewConversation))
                    .compact()
                    .on_click(move |_, _, cx| {
                        new_entity.update(cx, |this, cx| this.new_conversation(cx));
                    }),
            )
            .child(
                Button::new("agent-chat-markdown-sample")
                    .label(t(ChatMsg::MarkdownSample))
                    .compact()
                    .on_click(move |_, _, cx| {
                        sample_entity.update(cx, |this, cx| {
                            this.append_markdown_sample(cx);
                        });
                    }),
            )
            .child(
                Button::new("agent-chat-history")
                    .label(t(ChatMsg::HistoryTab))
                    .compact()
                    .toggled(self.history != HistoryPanel::Closed)
                    .on_click(move |_, _, cx| {
                        history_entity.update(cx, |this, cx| this.toggle_history(cx));
                    }),
            )
            .child(
                Button::new("agent-chat-stop")
                    .label(t(ChatMsg::Stop))
                    .compact()
                    .disabled(!running)
                    .on_click(move |_, _, cx| {
                        stop_entity.update(cx, |this, cx| this.stop(cx));
                    }),
            )
            .into_any_element()
    }

    /// 目标主机：每台一个开关，默认一台都不选，可以增也可以减。
    fn host_selection_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let host_chips: Vec<AnyElement> = self
            .hosts
            .iter()
            .map(|host| {
                let host_id = host.id;
                let chosen = self.selected_hosts.contains(&host_id);
                let chip_entity = entity.clone();
                Button::new(format!("agent-chat-host-{host_id}"))
                    .label(host.name.clone())
                    .compact()
                    .toggled(chosen)
                    .on_click(move |_, _, cx| {
                        chip_entity.update(cx, |this, cx| this.toggle_host(host_id, cx));
                    })
                    .into_any_element()
            })
            .collect();
        let no_hosts_selected = self.selected_hosts.is_empty();

        h_flex()
            .gap_2()
            .items_center()
            .flex_wrap()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(t(ChatMsg::TargetHostPrefix)),
            )
            .children(host_chips)
            .when(self.hosts.is_empty(), |row| {
                row.child(
                    div()
                        .text_xs()
                        .text_color(Tokens::current(cx).caution)
                        .child(t(ChatMsg::NoHostsInLibrary)),
                )
            })
            .when(!self.hosts.is_empty() && no_hosts_selected, |row| {
                row.child(
                    div()
                        .text_xs()
                        .text_color(Tokens::current(cx).caution)
                        .child(t(ChatMsg::NoHostHint)),
                )
            })
            .into_any_element()
    }

    /// 输入行：输入框 + 发送。真实回合进行中和面板一样可以继续写、Enter / 发送把话放进待发送队列
    /// （队列块画在对话区底部，见 `queue_view`）；智能体不可用时才禁用。
    fn input_row(&self, running: bool, blocked: bool, cx: &mut Context<Self>) -> AnyElement {
        let send_entity = cx.entity();
        let disabled = input_disabled(self.real, running, blocked);
        h_flex()
            .gap_2()
            .items_end()
            .child(
                Textarea::new(&self.input)
                    .flex_1()
                    .h(::gpui::px(72.0))
                    .disabled(disabled),
            )
            .child(
                Button::new("agent-chat-send")
                    .label(t(ChatMsg::Send))
                    .disabled(disabled)
                    .on_click(move |_, window, cx| {
                        send_entity.update(cx, |this, cx| this.send(window, cx));
                    }),
            )
            .into_any_element()
    }

    /// 底部说明和提示：模式说明、续接 / 链接 / 历史保存的提示、失败原因。
    fn footer_hints(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .gap_1()
            .when(self.real, |footer| {
                footer
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(t(ChatMsg::FooterRealIntro)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(t(ChatMsg::FooterRealModes)),
                    )
            })
            .when(!self.real, |footer| {
                footer
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(t(ChatMsg::FooterDemoIntro)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(t(ChatMsg::FooterDemoModes)),
                    )
            })
            .when_some(self.resume_notice.clone(), |footer, notice| {
                footer.child(div().text_xs().text_color(cx.theme().success).child(notice))
            })
            .when_some(self.link_notice.clone(), |footer, notice| {
                footer.child(
                    div()
                        .text_xs()
                        .text_color(if notice.error {
                            cx.theme().danger
                        } else {
                            cx.theme().success
                        })
                        .child(notice.text),
                )
            })
            .when_some(self.history_save.clone(), |footer, notice| {
                footer.child(
                    div()
                        .text_xs()
                        .text_color(if notice.error {
                            cx.theme().danger
                        } else {
                            cx.theme().success
                        })
                        .child(redact_paths(&notice.text)),
                )
            })
            .when_some(self.footer_error.clone(), |footer, err| {
                footer.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().danger)
                        .child(redact_paths(&err)),
                )
            })
            .when_some(self.exec.error.clone(), |footer, err| {
                footer.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().danger)
                        .child(redact_paths(&err)),
                )
            })
            .when_some(self.storage_error.clone(), |footer, err| {
                footer.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().danger)
                        .child(redact_paths(&format!(
                            "{}{err}{}",
                            t(ChatMsg::SettingsUnavailablePrefix),
                            t(ChatMsg::SettingsUnavailableNoSaveSuffix)
                        ))),
                )
            })
            .into_any_element()
    }
}
