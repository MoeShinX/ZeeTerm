//! 消息区渲染：助手正文、工具卡片、确认细节、复制外壳和消息列表。
//!
//! 独立窗口和嵌在主窗口里的面板共用这一份；历史面板的只读视图在 `history_view`。

use super::*;

/// 点链接之后底部的提示。所有链接 handler 都过 [`link_action`]（core 的 `validate_external_url`）：
/// 和 0.2.14 一致，**不打开浏览器，只复制网址**，并把完整网址显示出来让用户自己看清。
pub(super) fn link_click_notice(raw: &str) -> (Notice, Option<String>) {
    match link_action(raw) {
        LinkAction::Copy(url) => (
            Notice {
                text: format!("{}{url}", t(ChatMsg::LinkCopiedPrefix)),
                error: false,
            },
            Some(url),
        ),
        LinkAction::Refuse(reason) => (
            Notice {
                text: format!("{}{reason}", t(ChatMsg::LinkCopyFailedPrefix)),
                error: true,
            },
            None,
        ),
    }
}

/// 只有左键/中键（以及键盘触发）才打开链接；右键和长按不打开，和 TextView 默认行为一致。
pub(crate) fn link_click_opens(event: &ClickEvent) -> bool {
    match event {
        ClickEvent::Mouse(click) => {
            matches!(click.up.button, MouseButton::Left | MouseButton::Middle)
        }
        ClickEvent::Keyboard(_) => true,
        ClickEvent::Touch(click) => !click.long_press,
    }
}

/// 代码块「复制」写进剪贴板的内容：行尾统一成 LF，去掉围栏自带的末尾换行。
pub(super) fn code_block_copy_text(code: &str) -> String {
    let normalized = code.replace("\r\n", "\n").replace('\r', "\n");
    normalized.trim_end_matches('\n').to_owned()
}

/// 表格「复制表格」写进剪贴板的内容：制表符分列、换行分行，和表格软件粘贴的 TSV 一致。
pub(super) fn table_tsv(headers: &[String], rows: &[Vec<String>]) -> String {
    let mut lines = Vec::with_capacity(rows.len() + usize::from(!headers.is_empty()));
    if !headers.is_empty() {
        lines.push(tsv_line(headers));
    }
    for row in rows {
        lines.push(tsv_line(row));
    }
    lines.join("\n")
}

/// 单元格里的制表符和换行会把 TSV 切错列/行，先换成空格。
pub(super) fn tsv_line(cells: &[String]) -> String {
    cells
        .iter()
        .map(|cell| cell.replace(['\t', '\r', '\n'], " "))
        .collect::<Vec<_>>()
        .join("\t")
}

impl AgentChatDemo {
    /// 复制按钮（V2-U31）：鼠标移进所在的组（[`copy_group`]）才显示，刚复制过则一直显示对勾和「已复制」，
    /// 到点（[`COPIED_FLASH`]）收回。`key` 同时是「谁刚被复制」的标记。
    pub(super) fn copy_controls(
        &self,
        key: &str,
        whole: &str,
        entity: &Entity<Self>,
        cx: &mut Context<Self>,
    ) -> Div {
        let copied = self.is_copied(key);
        let group = copy_group(key);
        let click_entity = entity.clone();
        let click_key = key.to_owned();
        let click_text = whole.to_owned();
        let button = Button::new(format!("{key}-copy"))
            .ghost()
            .compact()
            .icon(if copied {
                IconName::Check
            } else {
                IconName::Copy
            })
            .tooltip(if copied {
                t(ChatMsg::Copied)
            } else {
                t(ChatMsg::Copy)
            })
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                let text = click_text.clone();
                click_entity.update(cx, |this, cx| this.copy_message(&click_key, text, cx));
            });
        h_flex()
            .flex_none()
            .gap_1()
            .items_center()
            .rounded_full()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .when(copied, |row| {
                row.child(
                    div()
                        .pl_1()
                        .text_xs()
                        .text_color(cx.theme().success)
                        .child(t(ChatMsg::Copied)),
                )
            })
            .when(!copied, |row| {
                row.invisible().group_hover(group, |style| style.visible())
            })
            .child(button)
    }

    /// 给一块消息内容套上「悬停出复制按钮 + 右键菜单」的外壳（V2-U31），按钮浮在右上角。
    /// 按钮要占位的消息（用户气泡）自己用 [`Self::copy_controls`] 摆好再走 [`Self::copy_wrap`]。
    ///
    /// `menu` 为假时只有按钮（嵌在已经有菜单的消息里的小块，右键走外层那一个）。
    pub(super) fn copy_shell(
        &self,
        key: String,
        whole: String,
        body: AnyElement,
        menu: bool,
        entity: Entity<Self>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let controls = self
            .copy_controls(&key, &whole, &entity, cx)
            .absolute()
            .top_0()
            .right_0();
        let content = div()
            .relative()
            .w_full()
            .min_w_0()
            .child(body)
            .child(controls)
            .into_any_element();
        Self::copy_wrap(key, whole, content, menu, entity)
    }

    /// 外壳的另一半：悬停组 + 右键菜单「复制」。右键菜单：此刻有选中文字就复制选中的那部分，没有就复制
    /// 整块（[`copy_menu_choice`]）；选区要在菜单**建起来的那一刻**读，点菜单项的左键按下会先清掉选区。
    pub(super) fn copy_wrap(
        key: String,
        whole: String,
        content: AnyElement,
        menu: bool,
        entity: Entity<Self>,
    ) -> AnyElement {
        let shell = div()
            .id(key.clone())
            .group(copy_group(&key))
            .w_full()
            .min_w_0()
            .child(content);
        if !menu {
            return shell.into_any_element();
        }
        shell
            .context_menu(move |popup, window, cx| {
                let selected = gpui_kit::base::TextSelection::selected_text(window, cx);
                let (label, text) = copy_menu_choice(&selected, &whole);
                let tokens = Tokens::current(cx);
                let entity = entity.clone();
                let key = key.clone();
                popup.item(menu_kit::row(&tokens, label, IconName::Copy).on_click(
                    move |_, _, cx| {
                        let text = text.clone();
                        entity.update(cx, |this, cx| this.copy_message(&key, text, cx));
                    },
                ))
            })
            .into_any_element()
    }

    /// 助手正文：渲染模式保留 Markdown 视图；原文模式把原始 Markdown 放进代码块。
    ///
    /// 两种模式的链接都挂同一个 handler（[`Self::open_link`]：core 的 `validate_external_url`
    /// 校验，通过只复制网址、不打开浏览器，和 0.2.14 一致）。原文模式的围栏按正文里最长的
    /// 反引号串现算，正文逃不出代码块；handler 是第二道防线。
    pub(super) fn assistant_body(
        &self,
        entity: Entity<Self>,
        index: usize,
        message: &AssistantMessage,
        mode: BodyMode,
    ) -> Option<AnyElement> {
        let source = body_markdown(message, mode)?;
        let id = match mode {
            BodyMode::Rendered => ("agent-chat-markdown", index),
            BodyMode::Raw => ("agent-chat-raw", index),
        };
        let link_entity = entity.clone();
        let view = TextView::markdown(id, source)
            .text_sm()
            .selectable(true)
            .selection_format(SelectionFormat::Source)
            // 所有图片都换成不存在的内嵌资源：既不读远端，也不读本地文件。
            .image_source(|_| {
                ImageSource::Resource(Resource::Embedded("zssh-no-remote-images".into()))
            })
            .on_link_click(move |url, event, _, cx| {
                if !link_click_opens(event) {
                    return;
                }
                let url = url.to_string();
                link_entity.update(cx, |this, cx| this.open_link(&url, cx));
            })
            .code_block_actions(move |code_block, _, _| {
                let code = code_block_copy_text(&code_block.code());
                Button::new(format!("agent-chat-copy-code-{index}"))
                    .label(t(ChatMsg::Copy))
                    .compact()
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(code.clone()));
                    })
            })
            .table_actions(move |table, _, _| {
                let tsv = table_tsv(&table.headers, &table.rows);
                h_flex().justify_end().child(
                    Button::new(format!("agent-chat-copy-table-{index}"))
                        .label(t(ChatMsg::CopyTable))
                        .compact()
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(tsv.clone()));
                        }),
                )
            });
        let (_, clipped) = clipped_body(&message.text, message.body_expanded);
        let total_chars = message.text.chars().count();
        let expanded = message.body_expanded;
        if !clipped && !(expanded && total_chars > BODY_PREVIEW_CHARS) {
            return Some(view.into_any_element());
        }
        let toggle_entity = entity;
        Some(
            v_flex()
                .gap_1()
                .child(view)
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().text_xs().child(if expanded {
                            format!(
                                "{}{total_chars}{}",
                                t(ChatMsg::BodyLongPrefix),
                                t(ChatMsg::BodyLongAllSuffix)
                            )
                        } else {
                            format!(
                                "{}{total_chars}{}{BODY_PREVIEW_CHARS}{}",
                                t(ChatMsg::BodyLongPrefix),
                                t(ChatMsg::BodyLongPartialMiddle),
                                t(ChatMsg::BodyLongPartialSuffix)
                            )
                        }))
                        .child(
                            Button::new(format!("agent-chat-body-more-{index}"))
                                .label(if expanded {
                                    t(ChatMsg::Collapse)
                                } else {
                                    t(ChatMsg::ExpandAll)
                                })
                                .compact()
                                .on_click(move |_, _, cx| {
                                    toggle_entity.update(cx, |this, cx| {
                                        this.chat.toggle_body_expanded(index);
                                        cx.notify();
                                    });
                                }),
                        ),
                )
                .into_any_element(),
        )
    }

    /// 一个等宽的文本框：默认限高滚动，超长时旁边有「展开全文」；展开后不限高。
    /// 命令、文件路径都用它显示全文（换行保留，不可见控制字符已被转义成可见形式）。
    pub(super) fn text_box(
        id: String,
        text: String,
        expanded: bool,
        danger: bool,
        cx: &App,
    ) -> AnyElement {
        div()
            .id(id)
            .w_full()
            .p_2()
            .rounded(::gpui::px(crate::app_theme::CARD_RADIUS))
            .border_1()
            .border_color(if danger {
                cx.theme().danger
            } else {
                cx.theme().border
            })
            .bg(cx.theme().background)
            .text_xs()
            .when(!expanded, |boxed| {
                boxed.max_h(::gpui::px(160.0)).overflow_y_scroll()
            })
            .child(text)
            .into_any_element()
    }

    /// 命令框 + 「展开全文 / 收起」「复制」按钮。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn command_block(
        &self,
        id: &str,
        text: String,
        expanded: bool,
        danger: bool,
        entity: Entity<Self>,
        message_index: usize,
        tool_index: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let long = needs_expand(&text);
        let line_count = text.lines().count().max(1);
        let copy_text = text.clone();
        let toggle_entity = entity.clone();
        let copy_entity = entity;
        v_flex()
            .gap_1()
            .child(Self::text_box(
                format!("{id}-text"),
                text,
                expanded,
                danger,
                cx,
            ))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .when(long, |row| {
                        row.child(
                            Button::new(format!("{id}-expand"))
                                .label(if expanded {
                                    t(ChatMsg::Collapse).to_owned()
                                } else {
                                    format!(
                                        "{}{line_count}{}",
                                        t(ChatMsg::ExpandAllPrefix),
                                        t(ChatMsg::ExpandAllSuffix)
                                    )
                                })
                                .compact()
                                .on_click(move |_, _, cx| {
                                    toggle_entity.update(cx, |this, cx| {
                                        this.chat
                                            .toggle_command_expanded(message_index, tool_index);
                                        cx.notify();
                                    });
                                }),
                        )
                    })
                    .child(
                        Button::new(format!("{id}-copy"))
                            .label(t(ChatMsg::Copy))
                            .compact()
                            .on_click(move |_, _, cx| {
                                copy_entity.update(cx, |this, cx| {
                                    this.copy_text(copy_text.clone(), cx);
                                });
                            }),
                    ),
            )
            .into_any_element()
    }

    /// 确认请求的细节：主机、危险标记和原因、命令全文，或者文件路径和差异。
    /// 回答之后也留着，回看时能确认当时批准的是什么。
    pub(super) fn approval_detail_view(
        &self,
        message_index: usize,
        tool_index: usize,
        detail: &ApprovalDetail,
        entity: Entity<Self>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let request = &detail.request;
        let danger = detail.dangerous();
        let base = format!("agent-chat-approval-{message_index}-{tool_index}");
        let mut view = v_flex().gap_1();
        view = view.child(
            h_flex().gap_2().items_center().child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "{}{}",
                        t(ChatMsg::HostPrefix),
                        redact_paths(&request.host_label)
                    )),
            ),
        );
        if !request.reasons.is_empty() {
            view = view.child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().danger)
                    .child(
                        request
                            .reasons
                            .iter()
                            .map(super::reason::reason_text)
                            .collect::<Vec<_>>()
                            .join(t(ChatMsg::ReasonSeparator)),
                    ),
            );
        }
        if let Some(command) = detail.command_text() {
            view = view.child(self.command_block(
                &format!("{base}-command"),
                command,
                detail.command_expanded,
                danger,
                entity.clone(),
                message_index,
                tool_index,
                cx,
            ));
        }
        if let Some(file) = &request.file {
            let total = file.diff.lines.len();
            let (shown, hidden) = fold_diff(total, detail.diff_expanded);
            let diff_entity = entity.clone();
            let fold_entity = entity;
            view = view
                .child(Self::text_box(
                    format!("{base}-path"),
                    visible_command_text(&file.path),
                    true,
                    danger,
                    cx,
                ))
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(diff_summary(
                                    file.diff.added,
                                    file.diff.removed,
                                    file.diff.is_new,
                                )),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(if file.is_new {
                                    t(ChatMsg::NewFile)
                                } else {
                                    t(ChatMsg::AutoBackup)
                                }),
                        ),
                );
            let lines: Vec<AnyElement> = file
                .diff
                .lines
                .iter()
                .take(shown)
                .map(|line| {
                    let (prefix, color) = match line.kind.as_str() {
                        "add" => ("+ ", cx.theme().success),
                        "del" => ("- ", cx.theme().danger),
                        "hunk" => ("", cx.theme().muted_foreground),
                        _ => ("  ", cx.theme().foreground),
                    };
                    div()
                        .text_xs()
                        .text_color(color)
                        .child(format!("{prefix}{}", visible_command_text(&line.text)))
                        .into_any_element()
                })
                .collect();
            view = view.child(
                v_flex()
                    .id(format!("{base}-diff"))
                    .w_full()
                    .p_2()
                    .rounded(::gpui::px(crate::app_theme::CARD_RADIUS))
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .max_h(::gpui::px(260.0))
                    .overflow_y_scroll()
                    .children(lines),
            );
            if hidden > 0 {
                view = view.child(
                    Button::new(format!("{base}-diff-more"))
                        .label(format!(
                            "{}{hidden}{}",
                            t(ChatMsg::DiffMorePrefix),
                            t(ChatMsg::DiffMoreSuffix)
                        ))
                        .compact()
                        .on_click(move |_, _, cx| {
                            diff_entity.update(cx, |this, cx| {
                                this.chat.toggle_diff_expanded(message_index, tool_index);
                                cx.notify();
                            });
                        }),
                );
            } else if detail.diff_expanded && total > crate::agent_text::DIFF_FOLD_LIMIT {
                view = view.child(
                    Button::new(format!("{base}-diff-less"))
                        .label(t(ChatMsg::Collapse))
                        .compact()
                        .on_click(move |_, _, cx| {
                            fold_entity.update(cx, |this, cx| {
                                this.chat.toggle_diff_expanded(message_index, tool_index);
                                cx.notify();
                            });
                        }),
                );
            }
            if file.diff.omitted_lines > 0 {
                view = view.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "{}{}{}",
                            t(ChatMsg::DiffOmittedPrefix),
                            file.diff.omitted_lines,
                            t(ChatMsg::DiffOmittedSuffix)
                        )),
                );
            }
        }
        view.into_any_element()
    }

    /// 一张工具卡片：标题行、输入 / 确认细节、结果细节、确认状态、错误和输出。
    ///
    /// 拆成几个小函数只为读起来清楚；元素的先后顺序就是这里 `.child` 的顺序。
    pub(super) fn tool_card(
        &self,
        message_index: usize,
        tool_index: usize,
        card: &ToolCard,
        entity: Entity<Self>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let dangerous = card.dangerous();
        let mut element = v_flex()
            .gap_1()
            .p_2()
            .rounded(::gpui::px(crate::app_theme::CARD_RADIUS))
            .border_1()
            .border_color(if card.error.is_some() || dangerous {
                cx.theme().danger
            } else {
                cx.theme().border
            })
            .bg(cx.theme().tab_bar)
            .child(self.tool_card_header(message_index, tool_index, card, entity.clone(), cx));
        element = element.child(self.tool_card_input(
            message_index,
            tool_index,
            card,
            dangerous,
            entity.clone(),
            cx,
        ));
        for child in self.tool_card_detail_lines(card, cx) {
            element = element.child(child);
        }
        if let Some(approval) =
            self.tool_card_approval(message_index, tool_index, card, entity.clone(), cx)
        {
            element = element.child(approval);
        }
        if let Some(error) = self.tool_card_error(card, cx) {
            element = element.child(error);
        }
        for child in self.tool_card_output(message_index, tool_index, card, entity, cx) {
            element = element.child(child);
        }
        element.into_any_element()
    }

    /// 标题行：工具名、执行主机、目标未确定、查看终端、危险标记、状态和输出开关。
    fn tool_card_header(
        &self,
        message_index: usize,
        tool_index: usize,
        card: &ToolCard,
        entity: Entity<Self>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let output_entity = entity.clone();
        let status_color = match card.status {
            ToolStatus::Done | ToolStatus::Answered => cx.theme().success,
            ToolStatus::Failed | ToolStatus::Stopped => cx.theme().danger,
            ToolStatus::AwaitingApproval => Tokens::current(cx).caution,
            ToolStatus::Running => cx.theme().muted_foreground,
        };
        let output_id = format!("agent-chat-output-{message_index}-{tool_index}");
        let dangerous = card.dangerous();
        // 执行层回报的实际执行位置：对话里有多个目标（或当时没有「当前」）时必须标明在哪台主机执行。
        let exec_label = card
            .exec
            .as_ref()
            .filter(|_| card.exec_multi || self.selected_hosts.len() > 1)
            .map(|exec| redact_paths(&exec.label));
        let target_unknown = self.chat.target_unknown(card);
        let view_exec = card.exec.clone().filter(|_| self.embed.is_some());
        let view_entity = entity;
        let status_text = card_status_text(card);

        h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(agent_tools::friendly_tool_name(&card.tool)),
            )
            .when_some(exec_label, |row, label| {
                row.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(label),
                )
            })
            .when(target_unknown, |row| {
                row.child(
                    div()
                        .text_xs()
                        .text_color(Tokens::current(cx).caution)
                        .child(t(ChatMsg::TargetUnknown)),
                )
            })
            .when_some(view_exec, |row, exec| {
                row.child(
                    Button::new(format!(
                        "agent-chat-view-terminal-{message_index}-{tool_index}"
                    ))
                    .label(t(ChatMsg::ViewTerminal))
                    .compact()
                    .on_click(move |_, _, cx| {
                        view_entity.update(cx, |this, cx| this.view_terminal(&exec, cx));
                    }),
                )
            })
            .when(dangerous, |row| {
                row.child(
                    div()
                        .px_1()
                        .rounded_full()
                        .bg(cx.theme().danger)
                        .text_xs()
                        .text_color(cx.theme().background)
                        .child(t(ChatMsg::DangerBadge)),
                )
            })
            .child(div().text_xs().text_color(status_color).child(status_text))
            .child(div().flex_1())
            .when(card.output.is_some(), |row| {
                row.child(
                    div()
                        .id(output_id)
                        .cursor_pointer()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(if card.output_expanded {
                            t(ChatMsg::OutputExpanded)
                        } else {
                            t(ChatMsg::OutputCollapsed)
                        })
                        .on_click(move |_, _, cx| {
                            output_entity.update(cx, |this, cx| {
                                this.chat.toggle_tool_output(message_index, tool_index);
                                cx.notify();
                            });
                        }),
                )
            })
            .into_any_element()
    }

    /// 输入区：确认请求的细节、命令全文、文件路径，或者一行参数摘要。
    fn tool_card_input(
        &self,
        message_index: usize,
        tool_index: usize,
        card: &ToolCard,
        dangerous: bool,
        entity: Entity<Self>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if let Some(detail) = &card.detail {
            self.approval_detail_view(message_index, tool_index, detail, entity, cx)
        } else if let Some(command) = card.input.command() {
            // 没有确认请求的命令卡片（自动模式、驱动上报的调用）也显示命令全文，
            // 不再只给一行被截断的摘要。
            self.command_block(
                &format!("agent-chat-call-{message_index}-{tool_index}"),
                visible_command_text(&command),
                card.command_expanded,
                dangerous,
                entity,
                message_index,
                tool_index,
                cx,
            )
        } else if agent_tools::is_file_tool(&card.tool)
            && let Some(path) = agent_tools::path_of(&card.input_json())
        {
            // 文件工具：路径全文（服务器上的路径），不是一行被截断的 JSON 摘要。
            Self::text_box(
                format!("agent-chat-path-{message_index}-{tool_index}"),
                visible_command_text(&path),
                true,
                false,
                cx,
            )
        } else {
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(redact_paths(&card.input.summary()))
                .into_any_element()
        }
    }

    /// 结果里的细节（0.2.14 `ToolCard`）：写系统路径标红、原文件备份到哪儿、属主没能改回去、
    /// 命令在终端里还是后台执行的。
    fn tool_card_detail_lines(&self, card: &ToolCard, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut elements = Vec::new();
        for (line, danger) in card_detail_lines(card) {
            elements.push(
                div()
                    .text_xs()
                    .child(line)
                    .text_color(if danger {
                        cx.theme().danger
                    } else {
                        cx.theme().muted_foreground
                    })
                    .into_any_element(),
            );
        }
        elements
    }

    /// 确认状态：等确认时给「允许 / 拒绝」，回答之后显示结果；不需要确认的卡片没有这一段。
    fn tool_card_approval(
        &self,
        message_index: usize,
        tool_index: usize,
        card: &ToolCard,
        entity: Entity<Self>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if card.approval == ApprovalState::Pending {
            let allow_entity = entity.clone();
            let deny_entity = entity;
            let allow_call = card.call_id.clone();
            let deny_call = card.call_id.clone();
            Some(
                v_flex()
                    .gap_1()
                    .p_2()
                    .rounded(::gpui::px(crate::app_theme::CARD_RADIUS))
                    .border_1()
                    .border_color(Tokens::current(cx).caution)
                    .bg(cx.theme().muted)
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(Tokens::current(cx).caution)
                            .child(t(ChatMsg::NeedsConfirm)),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                Button::new(format!(
                                    "agent-chat-allow-{message_index}-{tool_index}"
                                ))
                                .label(t(ChatMsg::Allow))
                                .compact()
                                .on_click(move |_, _, cx| {
                                    allow_entity.update(cx, |this, cx| {
                                        this.answer_approval(&allow_call, true, cx);
                                    });
                                }),
                            )
                            .child(
                                Button::new(format!(
                                    "agent-chat-deny-{message_index}-{tool_index}"
                                ))
                                .label(t(ChatMsg::Deny))
                                .compact()
                                .on_click(move |_, _, cx| {
                                    deny_entity.update(cx, |this, cx| {
                                        this.answer_approval(&deny_call, false, cx);
                                    });
                                }),
                            ),
                    )
                    .into_any_element(),
            )
        } else if card.approval == ApprovalState::Allowed {
            Some(
                div()
                    .text_xs()
                    .text_color(cx.theme().success)
                    .child(t(ChatMsg::Allowed))
                    .into_any_element(),
            )
        } else if card.approval == ApprovalState::Denied {
            Some(
                div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(t(ChatMsg::Denied))
                    .into_any_element(),
            )
        } else {
            None
        }
    }

    /// 卡片上的错误行。
    fn tool_card_error(&self, card: &ToolCard, cx: &mut Context<Self>) -> Option<AnyElement> {
        let error = card.error.as_ref()?;
        Some(
            div()
                .text_xs()
                .text_color(cx.theme().danger)
                .child(format!(
                    "{}{}",
                    t(ChatMsg::ErrorPrefix),
                    redact_paths(error)
                ))
                .into_any_element(),
        )
    }

    /// 工具输出：默认只显示前几十行，超长时下面给「显示全部 / 收起」。
    fn tool_card_output(
        &self,
        message_index: usize,
        tool_index: usize,
        card: &ToolCard,
        entity: Entity<Self>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(output) = &card.output else {
            return Vec::new();
        };
        if !card.output_expanded {
            return Vec::new();
        }
        let more_entity = entity;
        let mut elements = Vec::new();
        // 有的智能体把 MCP 结果原样包成 JSON 字符串：拆开再显示。
        let output = &agent_tools::readable_output(output);
        let clip = clip_text(output, OUTPUT_PREVIEW_LINES, OUTPUT_PREVIEW_CHARS);
        let shown = if card.output_full {
            output.clone()
        } else {
            clip.text.clone()
        };
        let long = clip.clipped;
        elements.push(
            div()
                .id(format!(
                    "agent-chat-output-text-{message_index}-{tool_index}"
                ))
                .p_2()
                .rounded(::gpui::px(crate::app_theme::CARD_RADIUS))
                .border_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().background)
                .text_xs()
                .when(card.output_full, |boxed| {
                    boxed.max_h(::gpui::px(320.0)).overflow_y_scroll()
                })
                .child(shown)
                .into_any_element(),
        );
        if long {
            let full = card.output_full;
            elements.push(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(if full {
                                format!(
                                    "{}{}{}",
                                    t(ChatMsg::AllLinesPrefix),
                                    clip.total_lines,
                                    t(ChatMsg::AllLinesSuffix)
                                )
                            } else {
                                format!(
                                    "{}{}{}{}{}",
                                    t(ChatMsg::OutputLongPrefix),
                                    clip.total_lines,
                                    t(ChatMsg::OutputLongMiddle),
                                    clip.total_chars,
                                    t(ChatMsg::OutputLongSuffix)
                                )
                            }),
                    )
                    .child(
                        Button::new(format!(
                            "agent-chat-output-more-{message_index}-{tool_index}"
                        ))
                        .label(if full {
                            t(ChatMsg::Collapse)
                        } else {
                            t(ChatMsg::ShowAll)
                        })
                        .compact()
                        .on_click(move |_, _, cx| {
                            more_entity.update(cx, |this, cx| {
                                this.chat.toggle_output_full(message_index, tool_index);
                                cx.notify();
                            });
                        }),
                    )
                    .into_any_element(),
            );
        }
        elements
    }

    /// 一组已经执行完的工具调用：一行淡色小字，默认收起，点开显示完整卡片；有失败的用警示色并写明几条失败。
    pub(super) fn tool_group(
        &self,
        message_index: usize,
        message: &AssistantMessage,
        ids: &[usize],
        entity: Entity<Self>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let first = ids.first().copied().unwrap_or(0);
        let open = message.open_groups.contains(&first);
        // 确认卡片不计数：命令的结果在另一张工具卡片上，一条命令不能算两次。
        let counted: Vec<&ToolCard> = ids
            .iter()
            .map(|index| &message.tools[*index])
            .filter(|card| !card.is_approval())
            .collect();
        let tools: Vec<&str> = counted.iter().map(|card| card.tool.as_str()).collect();
        let failed = counted.iter().filter(|card| card.view().failed).count();
        let mut summary = if tools.is_empty() {
            format!("{}{}", ids.len(), t(ChatMsg::ProcessedApprovalsSuffix))
        } else {
            agent_tools::summarize_calls(&tools)
        };
        if failed > 0 {
            summary.push_str(&format!(
                "{}{failed}{}",
                t(ChatMsg::GroupFailedPrefix),
                t(ChatMsg::GroupFailedSuffix)
            ));
        }
        let toggle_entity = entity.clone();
        let mut group = v_flex().w_full().gap_1().child(
            h_flex()
                .id(("agent-chat-tool-group", message_index * 1000 + first))
                .w_full()
                .cursor_pointer()
                .gap_1()
                .items_center()
                .child(
                    div()
                        // 摘要比面板宽时折行，不被截断。
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .text_color(if failed > 0 {
                            Tokens::current(cx).caution
                        } else {
                            cx.theme().muted_foreground
                        })
                        .child(format!("{} {summary}", if open { "▾" } else { "▸" })),
                )
                .on_click(move |_, _, cx| {
                    toggle_entity.update(cx, |this, cx| {
                        this.chat.toggle_group(message_index, first);
                        cx.notify();
                    });
                }),
        );
        if open {
            for index in ids {
                group = group.child(self.tool_card(
                    message_index,
                    *index,
                    &message.tools[*index],
                    entity.clone(),
                    cx,
                ));
            }
        }
        group.into_any_element()
    }

    /// 卡片上的「查看终端」：切到这台主机的终端标签（还没连上、正在连也可以，能看到连接过程）。
    pub(super) fn view_terminal(&mut self, exec: &ExecInfo, cx: &mut Context<Self>) {
        agent_setup::request_view_terminal(exec.target_id.clone(), exec.session_id.clone());
        cx.notify();
    }

    /// 消息区底部的「· · · 12秒 · 思考中」，重试时下面带一行提示（0.2.14 `ThinkingIndicator`）。
    pub(super) fn thinking_row(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let retrying = self.chat.real_running && self.chat.retry.is_some();
        if !self.real || !(self.chat.thinking_visible() || retrying) {
            return None;
        }
        let since = self.chat.turn_started?;
        let elapsed = agent_tools::elapsed_text(since.elapsed().as_millis() as u64);
        let retry = self
            .chat
            .retry
            .as_ref()
            .filter(|_| self.chat.real_running)
            .map(|notice| redact_paths(&agent_tools::retry_text(notice)));
        Some(
            v_flex()
                .id("agent-chat-thinking")
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("· · ·  {elapsed} · {}", t(ChatMsg::Thinking))),
                )
                .when_some(retry, |column, text| {
                    column.child(
                        div()
                            .id("agent-chat-retrying")
                            .text_xs()
                            .text_color(Tokens::current(cx).caution)
                            .child(text),
                    )
                })
                .into_any_element(),
        )
    }
}

/// 助手消息下面带按钮的提示卡（需要充值 / 授权失效，V2-A5）。
pub(super) fn tokenway_card(
    entity: Entity<AgentChatDemo>,
    index: usize,
    card_index: usize,
    card: CardKind,
    cx: &App,
) -> AnyElement {
    let (text, label, id) = match card {
        CardKind::TopUp => (
            agent_setup::builtin::requires_topup_text(),
            t(ChatMsg::TopUp),
            "agent-chat-card-topup",
        ),
        CardKind::Reconnect => (
            agent_setup::builtin::authorization_invalid_text(),
            t(ChatMsg::Reconnect),
            "agent-chat-card-reconnect",
        ),
    };
    h_flex()
        .id(("agent-chat-card", index * 8 + card_index))
        .w_full()
        .min_w_0()
        .gap_2()
        .items_center()
        .p_2()
        .rounded(::gpui::px(crate::app_theme::CARD_RADIUS))
        .border_1()
        .border_color(Tokens::current(cx).caution)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_xs()
                .text_color(cx.theme().foreground)
                .child(text),
        )
        .child(
            Button::new((id, index * 8 + card_index))
                .label(label)
                .compact()
                .primary()
                .on_click(move |_, _window, cx| match card {
                    CardKind::TopUp => {
                        let opened = agent_setup::tokenway::open_topup();
                        entity.update(cx, |this, cx| {
                            this.link_notice = Some(match opened {
                                Ok(()) => Notice {
                                    text: t(ChatMsg::TopUpOpened).to_owned(),
                                    error: false,
                                },
                                Err(reason) => Notice {
                                    text: format!("{}{reason}", t(ChatMsg::TopUpOpenFailedPrefix)),
                                    error: true,
                                },
                            });
                            cx.notify();
                        });
                    }
                    CardKind::Reconnect => agent_setup::tokenway::request_reconnect(),
                }),
        )
        .into_any_element()
}
impl AgentChatDemo {
    /// 消息区（或历史面板，二选一）：独立窗口和嵌在主窗口里的面板共用同一份。
    pub(super) fn conversation_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let older_entity = entity.clone();
        let message_count = self.chat.messages.len();
        // 长对话只画最后一页：每帧为全部消息构建元素（含 Markdown 解析）会越用越卡。
        let first_shown = visible_start(message_count, self.shown_messages);

        let mut messages = Vec::new();
        for (index, message) in self.chat.messages.iter().enumerate().skip(first_shown) {
            messages.push(self.message_element(index, message, &entity, cx));
        }

        let empty = message_count == 0;
        // 真实模式下智能体没找到 / 没登录 / 还在检测：整块提示替代空状态（0.2.14 同）。
        let unavailable = if self.real && empty {
            self.unavailable_view(cx)
        } else {
            None
        };
        let message_area = v_flex()
            .id("agent-chat-messages")
            .flex_1()
            .min_h_0()
            .gap_2()
            .p_2()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .border_1()
            .border_color(cx.theme().border)
            .when(empty && unavailable.is_none(), |area| {
                area.child(
                    div()
                        .p_2()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(if self.real {
                            format!(
                                "{}{}{}",
                                t(ChatMsg::EmptyRealPrefix),
                                self.agent.label(),
                                t(ChatMsg::EmptyRealSuffix)
                            )
                        } else {
                            t(ChatMsg::EmptyDemo).to_owned()
                        }),
                )
            })
            .children(unavailable)
            .when(first_shown > 0, |area| {
                area.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!(
                                    "{}{first_shown}{}",
                                    t(ChatMsg::EarlierPrefix),
                                    t(ChatMsg::EarlierSuffix)
                                )),
                        )
                        .child(
                            Button::new("agent-chat-older")
                                .label(t(ChatMsg::ShowEarlier))
                                .compact()
                                .on_click(move |_, _, cx| {
                                    older_entity.update(cx, |this, cx| {
                                        this.shown_messages += MESSAGES_PER_PAGE;
                                        cx.notify();
                                    });
                                }),
                        ),
                )
            })
            .children(messages)
            .children(self.thinking_row(cx))
            .children(self.queue_block(cx));

        // 历史面板和消息区二选一：面板打开时替代消息区，输入框和底部提示照旧。
        let body_area: AnyElement = if self.history == HistoryPanel::Closed {
            message_area.into_any_element()
        } else {
            self.history_panel(cx)
        };
        body_area
    }

    /// 对话里的一条消息：用户气泡、系统提示行或助手消息块。
    fn message_element(
        &self,
        index: usize,
        message: &ChatMessage,
        entity: &Entity<Self>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match message {
            ChatMessage::User(text) => self.user_message_row(index, text, entity.clone(), cx),
            ChatMessage::System(text) => self.system_message_row(index, text, entity.clone(), cx),
            ChatMessage::Assistant(message) => {
                self.assistant_message_block(index, message, entity.clone(), cx)
            }
        }
    }

    /// 用户消息：气泡靠右；按钮摆在气泡左边（占位、不遮字）。
    fn user_message_row(
        &self,
        index: usize,
        text: &str,
        entity: Entity<Self>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = format!("agent-chat-msg-{index}");
        let controls = self.copy_controls(&key, text, &entity, cx);
        let row = h_flex()
            .w_full()
            .min_w_0()
            .justify_end()
            .items_start()
            .gap_1()
            .child(controls)
            .child(
                div()
                    .max_w(::gpui::px(680.0))
                    .min_w_0()
                    .p_2()
                    .rounded(::gpui::px(crate::app_theme::CARD_RADIUS))
                    .bg(cx.theme().primary)
                    .text_color(cx.theme().primary_foreground)
                    .text_sm()
                    .child(SelectableText::new(
                        ("agent-chat-user-text", index),
                        text.to_owned(),
                    )),
            )
            .into_any_element();
        Self::copy_wrap(key, text.to_owned(), row, true, entity)
    }

    /// 独立的系统提示行：居中、淡色，没有行头和按钮（V2-U14）。
    fn system_message_row(
        &self,
        index: usize,
        text: &str,
        entity: Entity<Self>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shown = redact_paths(text);
        let line = div()
            .id(("agent-chat-system", index))
            .w_full()
            .min_w_0()
            .py_1()
            .px_8()
            .text_center()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(SelectableText::new(
                ("agent-chat-system-text", index),
                shown.clone(),
            ))
            .into_any_element();
        self.copy_shell(
            format!("agent-chat-msg-{index}"),
            shown,
            line,
            true,
            entity,
            cx,
        )
    }

    /// 助手消息：行头、提示 / 提示卡、思考、正文和工具调用，最后套上复制外壳。
    fn assistant_message_block(
        &self,
        index: usize,
        message: &AssistantMessage,
        entity: Entity<Self>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mode = body_mode(message);
        let mut block = v_flex().w_full().gap_1().child(self.assistant_header(
            index,
            message,
            entity.clone(),
            mode,
            cx,
        ));

        for element in self.assistant_notices(index, message, entity.clone(), cx) {
            block = block.child(element);
        }
        if let Some(reasoning) = self.assistant_reasoning(index, message, entity.clone(), cx) {
            block = block.child(reasoning);
        }
        if let Some(body) = self.assistant_body(entity.clone(), index, message, mode) {
            block = block.child(body);
        }
        for element in self.assistant_tool_rows(index, message, entity.clone(), cx) {
            block = block.child(element);
        }

        let whole = assistant_copy_text(message);
        if whole.is_empty() {
            return block.into_any_element();
        }
        self.copy_shell(
            format!("agent-chat-msg-{index}"),
            whole,
            block.into_any_element(),
            true,
            entity,
            cx,
        )
    }

    /// 助手消息的行头：「助手」、已停止标记和「原文 / 渲染」按钮。
    fn assistant_header(
        &self,
        index: usize,
        message: &AssistantMessage,
        entity: Entity<Self>,
        mode: BodyMode,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let raw_entity = entity;
        h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(t(ChatMsg::AssistantRole)),
            )
            .when(message.stopped, |row| {
                row.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().danger)
                        .child(t(ChatMsg::StopBadge)),
                )
            })
            .child(
                Button::new(format!("agent-chat-raw-{index}"))
                    .label(mode.toggle_label())
                    .compact()
                    .toggled(mode == BodyMode::Raw)
                    .on_click(move |_, _, cx| {
                        raw_entity.update(cx, |this, cx| {
                            this.chat.toggle_raw(index);
                            cx.notify();
                        });
                    }),
            )
            .into_any_element()
    }

    /// 助手消息里的提示行和提示卡（按顺序，提示在前、提示卡在后）。
    fn assistant_notices(
        &self,
        index: usize,
        message: &AssistantMessage,
        entity: Entity<Self>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut elements = Vec::new();
        for (notice_index, notice) in message.notices.iter().enumerate() {
            let shown = notice_copy_text(notice);
            let line = div()
                .pr_8()
                .text_xs()
                .text_color(if notice.error {
                    cx.theme().danger
                } else {
                    Tokens::current(cx).caution
                })
                .child(SelectableText::new(
                    ("agent-chat-notice-text", index * 1000 + notice_index),
                    shown.clone(),
                ))
                .into_any_element();
            elements.push(self.copy_shell(
                format!("agent-chat-notice-{index}-{notice_index}"),
                shown,
                line,
                true,
                entity.clone(),
                cx,
            ));
        }
        for (card_index, card) in message.cards.iter().enumerate() {
            elements.push(tokenway_card(entity.clone(), index, card_index, *card, cx));
        }
        elements
    }

    /// 助手消息的思考区；没有思考时返回 `None`。
    fn assistant_reasoning(
        &self,
        index: usize,
        message: &AssistantMessage,
        entity: Entity<Self>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if message.reasoning.is_empty() {
            return None;
        }
        let expanded = message.reasoning_expanded;
        let reasoning = message.reasoning.clone();
        let toggle_entity = entity;
        Some(
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .id(("agent-chat-reasoning", index))
                        .cursor_pointer()
                        .items_center()
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(if expanded {
                                    t(ChatMsg::ReasoningExpanded)
                                } else {
                                    t(ChatMsg::ReasoningCollapsed)
                                }),
                        )
                        .on_click(move |_, _, cx| {
                            toggle_entity.update(cx, |this, cx| {
                                this.chat.toggle_reasoning(index);
                                cx.notify();
                            });
                        }),
                )
                .when(expanded, |section| {
                    section.child(
                        div()
                            .pl_2()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(reasoning),
                    )
                })
                .into_any_element(),
        )
    }

    /// 助手消息下面的工具调用：单张卡片、进度行或折叠的一组。
    fn assistant_tool_rows(
        &self,
        index: usize,
        message: &AssistantMessage,
        entity: Entity<Self>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut elements = Vec::new();
        let views: Vec<CallView> = message.tools.iter().map(ToolCard::view).collect();
        for row in agent_tools::group_calls(&views) {
            match row {
                ToolRow::Card(tool_index) => {
                    elements.push(self.tool_card(
                        index,
                        tool_index,
                        &message.tools[tool_index],
                        entity.clone(),
                        cx,
                    ));
                }
                ToolRow::Progress(tool_index) => {
                    let card = &message.tools[tool_index];
                    elements.push(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "◔ {}",
                                redact_paths(&agent_tools::progress_text(
                                    &card.tool,
                                    &card.input_json()
                                ))
                            ))
                            .into_any_element(),
                    );
                }
                ToolRow::Group(ids) => {
                    elements.push(self.tool_group(index, message, &ids, entity.clone(), cx));
                }
            }
        }
        elements
    }
}
