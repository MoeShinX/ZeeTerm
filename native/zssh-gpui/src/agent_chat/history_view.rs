//! 历史面板的渲染：会话列表、一行摘要和只读查看一条会话。
//!
//! 面板打开时替代消息区显示；数据来自 `history`，操作入口在 `actions`。

use super::*;

impl AgentChatDemo {
    /// 历史面板：列表 / 只读查看；替代消息区显示。
    pub(super) fn history_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.history {
            HistoryPanel::Closed => div().into_any_element(),
            HistoryPanel::List => self.history_list(cx),
            HistoryPanel::Detail(record) => self.history_detail(record.as_ref(), cx),
        }
    }

    /// 历史列表：标题、更新时间、主机、消息数，以及查看 / 继续 / 删除。
    pub(super) fn history_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let refresh_entity = entity.clone();
        let close_entity = entity.clone();
        let now = now_ms();
        let mut rows = Vec::with_capacity(self.history_rows.len());
        for summary in &self.history_rows {
            rows.push(self.history_row(summary, now, entity.clone(), cx));
        }
        let mut panel = v_flex()
            .id("agent-chat-history-list")
            .flex_1()
            .min_h_0()
            .gap_2()
            .p_2()
            .overflow_y_scroll()
            .border_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t(ChatMsg::HistoryPanelTitle)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(self.agent.label()),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("agent-chat-history-refresh")
                            .label(t(ChatMsg::Refresh))
                            .compact()
                            .on_click(move |_, _, cx| {
                                refresh_entity.update(cx, |this, cx| {
                                    this.refresh_history();
                                    cx.notify();
                                });
                            }),
                    )
                    .child(
                        Button::new("agent-chat-history-close")
                            .label(t(ChatMsg::Close))
                            .compact()
                            .on_click(move |_, _, cx| {
                                close_entity.update(cx, |this, cx| {
                                    this.close_history();
                                    cx.notify();
                                });
                            }),
                    ),
            );
        if let Some(error) = &self.history_error {
            panel = panel.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(redact_paths(error)),
            );
        }
        if self.history_rows.is_empty() && self.history_error.is_none() {
            panel = panel.child(
                div()
                    .p_2()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(t(ChatMsg::HistoryEmpty)),
            );
        }
        panel.children(rows).into_any_element()
    }

    /// 历史列表的一行。
    pub(super) fn history_row(
        &self,
        summary: &HistorySummary,
        now: u64,
        entity: Entity<Self>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let armed = self.history_armed.as_deref() == Some(summary.id.as_str());
        let view_entity = entity.clone();
        let continue_entity = entity.clone();
        let delete_entity = entity.clone();
        let cancel_entity = entity;
        let view_id = summary.id.clone();
        let continue_id = summary.id.clone();
        let delete_id = summary.id.clone();
        let button_suffix = summary.id.clone();
        let title = history_title(&summary.title).to_owned();
        // 锚定的主机单独成一个小标签（一眼看出这条对话是在哪台主机上聊的），其余是时间和消息数。
        let host = history_host(summary.anchor.as_ref(), summary.host_label.as_deref())
            .map(|host| host.trim().to_owned())
            .filter(|host| !host.is_empty());
        let meta = history_meta(summary.updated_at, None, summary.message_count, now);
        v_flex()
            .gap_1()
            .p_2()
            .rounded(::gpui::px(crate::app_theme::CARD_RADIUS))
            .border_1()
            .border_color(if armed {
                cx.theme().danger
            } else {
                cx.theme().border
            })
            .bg(cx.theme().tab_bar)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title),
                            )
                            .when(summary.resumable, |row| {
                                row.child(
                                    div()
                                        .px_1()
                                        .py_0p5()
                                        .rounded_full()
                                        .bg(cx.theme().muted)
                                        .text_xs()
                                        .text_color(cx.theme().success)
                                        .child(t(ChatMsg::Resumable)),
                                )
                            }),
                    )
                    .child(
                        Button::new(format!("agent-chat-history-view-{button_suffix}"))
                            .label(t(ChatMsg::View))
                            .compact()
                            .on_click(move |_, _, cx| {
                                view_entity.update(cx, |this, cx| {
                                    this.open_history(&view_id, cx);
                                });
                            }),
                    )
                    .child(
                        Button::new(format!("agent-chat-history-continue-{button_suffix}"))
                            .label(t(ChatMsg::Continue))
                            .compact()
                            .disabled(!summary.resumable)
                            .on_click(move |_, _, cx| {
                                continue_entity.update(cx, |this, cx| {
                                    this.continue_history(&continue_id, cx);
                                });
                            }),
                    )
                    .child(
                        Button::new(format!("agent-chat-history-delete-{button_suffix}"))
                            .label(if armed {
                                t(ChatMsg::ConfirmDelete)
                            } else {
                                t(ChatMsg::Delete)
                            })
                            .compact()
                            .on_click(move |_, _, cx| {
                                delete_entity.update(cx, |this, cx| {
                                    this.delete_history(&delete_id, cx);
                                });
                            }),
                    )
                    .when(armed, |row| {
                        row.child(
                            Button::new(format!("agent-chat-history-cancel-{button_suffix}"))
                                .label(t(ChatMsg::Cancel))
                                .compact()
                                .on_click(move |_, _, cx| {
                                    cancel_entity.update(cx, |this, cx| {
                                        this.disarm_history_delete(cx);
                                    });
                                }),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .children(host.map(|host| {
                        div()
                            .id(SharedString::from(format!("history-host-{button_suffix}")))
                            .max_w(px(140.0))
                            .truncate()
                            .px_1p5()
                            .rounded_full()
                            .border_1()
                            .border_color(cx.theme().border)
                            .text_color(cx.theme().foreground)
                            .child(host)
                    }))
                    .child(meta),
            )
            .into_any_element()
    }

    /// 只读查看一条历史会话；助手正文仍用 Markdown 渲染。
    pub(super) fn history_detail(
        &self,
        record: &HistoryRecord,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let back_entity = cx.entity();
        let now = now_ms();
        let meta = history_meta(
            record.updated_at,
            history_host(record.anchor.as_ref(), record.host_label.as_deref()).as_deref(),
            record.messages.len(),
            now,
        );
        let mut panel = v_flex()
            .id("agent-chat-history-detail")
            .flex_1()
            .min_h_0()
            .gap_2()
            .p_2()
            .overflow_y_scroll()
            .border_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("agent-chat-history-back")
                            .label(t(ChatMsg::BackToList))
                            .compact()
                            .on_click(move |_, _, cx| {
                                back_entity.update(cx, |this, cx| {
                                    this.history = HistoryPanel::List;
                                    this.history_armed = None;
                                    this.history_error = None;
                                    cx.notify();
                                });
                            }),
                    )
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(history_title(&record.title).to_owned()),
                            )
                            .when(record.agent_session_id.is_some(), |row| {
                                row.child(
                                    div()
                                        .px_1()
                                        .py_0p5()
                                        .rounded_full()
                                        .bg(cx.theme().muted)
                                        .text_xs()
                                        .text_color(cx.theme().success)
                                        .child(t(ChatMsg::Resumable)),
                                )
                            }),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(meta),
            );
        if record.messages.is_empty() {
            panel = panel.child(
                div()
                    .p_2()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(t(ChatMsg::NoMessages)),
            );
        }
        let start = visible_start(record.messages.len(), self.history_shown);
        if start > 0 {
            let more_entity = cx.entity();
            panel = panel.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "{}{start}{}",
                                t(ChatMsg::EarlierPrefix),
                                t(ChatMsg::EarlierSuffix)
                            )),
                    )
                    .child(
                        Button::new("agent-chat-history-older")
                            .label(t(ChatMsg::ShowEarlier))
                            .compact()
                            .on_click(move |_, _, cx| {
                                more_entity.update(cx, |this, cx| {
                                    this.history_shown += MESSAGES_PER_PAGE;
                                    cx.notify();
                                });
                            }),
                    ),
            );
        }
        for (index, message) in record.messages.iter().enumerate().skip(start) {
            let element = self.history_message(index, message, cx);
            // 用户 / 助手消息和对话里一样能复制（原文）；工具卡片没有。
            let whole = match message {
                HistoryMessage::User { text } | HistoryMessage::Assistant { text } => text.clone(),
                HistoryMessage::Tool { .. } => String::new(),
            };
            panel = panel.child(if whole.is_empty() {
                element
            } else {
                self.copy_shell(
                    format!("agent-chat-hist-{index}"),
                    whole,
                    element,
                    true,
                    cx.entity(),
                    cx,
                )
            });
        }
        panel.into_any_element()
    }

    /// 历史里的一条消息：用户 / 助手 / 工具，只读。
    pub(super) fn history_message(
        &self,
        index: usize,
        message: &HistoryMessage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match message {
            HistoryMessage::User { text } => h_flex()
                .w_full()
                .justify_end()
                .child(
                    div()
                        .max_w(::gpui::px(680.0))
                        .p_2()
                        .rounded(::gpui::px(crate::app_theme::CARD_RADIUS))
                        .bg(cx.theme().primary)
                        .text_color(cx.theme().primary_foreground)
                        .text_sm()
                        .child(text.clone()),
                )
                .into_any_element(),
            HistoryMessage::Assistant { text } => v_flex()
                .w_full()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(t(ChatMsg::AssistantRole)),
                )
                .child(
                    TextView::markdown(("agent-chat-history-markdown", index), text.clone())
                        .text_sm()
                        .selectable(true)
                        .selection_format(SelectionFormat::Source)
                        // 和历史对话一样：所有图片都换成不存在的内嵌资源。
                        .image_source(|_| {
                            ImageSource::Resource(Resource::Embedded(
                                "zssh-no-remote-images".into(),
                            ))
                        })
                        .on_link_click({
                            let link_entity = cx.entity();
                            move |url, event, _, cx| {
                                if !link_click_opens(event) {
                                    return;
                                }
                                let url = url.to_string();
                                link_entity.update(cx, |this, cx| this.open_link(&url, cx));
                            }
                        })
                        .code_block_actions(move |code_block, _, _| {
                            let code = code_block_copy_text(&code_block.code());
                            Button::new(format!("agent-chat-history-copy-code-{index}"))
                                .label(t(ChatMsg::Copy))
                                .compact()
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(code.clone()));
                                })
                        })
                        .table_actions(move |table, _, _| {
                            let tsv = table_tsv(&table.headers, &table.rows);
                            h_flex().justify_end().child(
                                Button::new(format!("agent-chat-history-copy-table-{index}"))
                                    .label(t(ChatMsg::CopyTable))
                                    .compact()
                                    .on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            tsv.clone(),
                                        ));
                                    }),
                            )
                        }),
                )
                .into_any_element(),
            HistoryMessage::Tool {
                tool,
                input,
                output,
                status,
            } => {
                let mut card = v_flex()
                    .gap_1()
                    .p_2()
                    .rounded(::gpui::px(crate::app_theme::CARD_RADIUS))
                    .border_1()
                    .border_color(
                        if matches!(status.as_str(), "error" | "rejected" | "stopped") {
                            cx.theme().danger
                        } else {
                            cx.theme().border
                        },
                    )
                    .bg(cx.theme().tab_bar)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(tool.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(history_tool_status(status)),
                            ),
                    );
                let summary = content_preview(&input.to_string());
                if !summary.is_empty() && summary != "null" {
                    card = card.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{}{summary}", t(ChatMsg::ParamsPrefix))),
                    );
                }
                if !output.trim().is_empty() {
                    let full = self.history_full.contains(&index);
                    let clip = clip_text(output, OUTPUT_PREVIEW_LINES, OUTPUT_PREVIEW_CHARS);
                    let shown = if full {
                        output.clone()
                    } else {
                        clip.text.clone()
                    };
                    card = card.child(
                        div()
                            .id(("agent-chat-history-output", index))
                            .p_2()
                            .rounded(::gpui::px(crate::app_theme::CARD_RADIUS))
                            .border_1()
                            .border_color(cx.theme().border)
                            .bg(cx.theme().background)
                            .text_xs()
                            .when(full, |boxed| {
                                boxed.max_h(::gpui::px(320.0)).overflow_y_scroll()
                            })
                            .child(shown),
                    );
                    if clip.clipped {
                        let toggle_entity = cx.entity();
                        card = card.child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(format!(
                                            "{}{}{}{}{}",
                                            t(ChatMsg::OutputLongShortPrefix),
                                            clip.total_lines,
                                            t(ChatMsg::OutputLongShortMiddle),
                                            clip.total_chars,
                                            t(ChatMsg::OutputLongShortSuffix)
                                        )),
                                )
                                .child(
                                    Button::new(format!("agent-chat-history-output-more-{index}"))
                                        .label(if full {
                                            t(ChatMsg::Collapse)
                                        } else {
                                            t(ChatMsg::ShowAll)
                                        })
                                        .compact()
                                        .on_click(move |_, _, cx| {
                                            toggle_entity.update(cx, |this, cx| {
                                                if !this.history_full.remove(&index) {
                                                    this.history_full.insert(index);
                                                }
                                                cx.notify();
                                            });
                                        }),
                                ),
                        );
                    }
                }
                card.into_any_element()
            }
        }
    }
}
