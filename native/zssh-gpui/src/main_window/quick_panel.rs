//! 管理快捷命令的右侧面板（0.2.14 `QuickCommandsPanel.tsx`）：搜索、按分组归拢的列表（可折叠）、
//! 组内上移 / 下移、编辑 / 删除，选中一条或点「新建命令」后在下面编辑（名称、分组、颜色、命令、
//! 「发送后回车执行」）。命令存在设置表的 `quick_commands` 里，命令栏读的是同一份。

use std::collections::HashSet;

use ::gpui::{Entity, SharedString, Window, px};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::MainWindow;
use super::modal::{Confirm, ConfirmAction, Modal};
use super::quick_commands::{self, COMMANDS_MAX, QuickCommand};
use super::side_panel::{PanelContent, QUICK_COMMANDS};
use super::widgets::{ButtonLook, act, field, note, switch_row, text_button};
use crate::app_theme::Tokens;

type RowAction = Box<dyn Fn(&mut Window, &mut App)>;

/// 正在编辑的那一条（输入框里的文字在 [`QuickUi`] 的输入框里，这里是其余字段）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Draft {
    /// `None` = 新建。
    pub id: Option<String>,
    pub color: Option<String>,
    pub enter: bool,
}

pub struct QuickUi {
    pub search: Entity<InputState>,
    pub name: Entity<InputState>,
    pub group: Entity<InputState>,
    pub command: Entity<TextareaState>,
    pub draft: Option<Draft>,
    /// 折起来的分组（空串 = 未分组）；不持久化，每次打开面板都是全部展开。
    pub collapsed: HashSet<String>,
    pub error: Option<String>,
    next_id: u64,
}

impl QuickUi {
    pub fn new(window: &mut Window, cx: &mut Context<MainWindow>) -> Self {
        Self {
            search: cx.new(|cx| InputState::new(window, cx).placeholder("搜索名称、命令或分组")),
            name: cx.new(|cx| InputState::new(window, cx).placeholder("例如：端口查看")),
            group: cx.new(|cx| {
                InputState::new(window, cx).placeholder("可选，例如：Debian、面板安装、测试脚本")
            }),
            command: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder("ss -tlnp")
                    .rows(5)
            }),
            draft: None,
            collapsed: HashSet::new(),
            error: None,
            next_id: 1,
        }
    }

    pub fn input_entities(&self) -> [Entity<InputState>; 3] {
        [self.search.clone(), self.name.clone(), self.group.clone()]
    }
}

fn set_input(
    state: &Entity<InputState>,
    value: &str,
    window: &mut Window,
    cx: &mut Context<MainWindow>,
) {
    let value = value.to_owned();
    state.update(cx, |input, cx| input.set_value(value, window, cx));
}

impl MainWindow {
    // ---- 打开 / 关闭 ----

    /// 命令栏上的管理按钮和主机页上的快捷命令按钮：开着就关，没开就开
    /// （没有命令时直接进入新建）。
    pub(crate) fn toggle_quick_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.panels.visible() == Some(QUICK_COMMANDS) {
            self.panels.close_side();
            self.focus_content(window, cx);
            cx.notify();
            return;
        }
        self.panels.open_side(QUICK_COMMANDS);
        self.quick_ui.draft = None;
        if self.bar_settings().commands.is_empty() {
            self.quick_begin_new(window, cx);
        }
        cx.notify();
    }

    pub(crate) fn open_quick_panel_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.panels.open_side(QUICK_COMMANDS);
        self.quick_begin_new(window, cx);
        cx.notify();
    }

    pub(crate) fn open_quick_panel_edit(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.panels.open_side(QUICK_COMMANDS);
        if let Some(command) = self
            .bar_settings()
            .commands
            .iter()
            .find(|item| item.id == id)
            .cloned()
        {
            self.quick_begin_edit(&command, window, cx);
        }
        cx.notify();
    }

    pub(super) fn quick_begin_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.quick_ui.draft = Some(Draft {
            id: None,
            color: None,
            enter: false,
        });
        self.quick_ui.error = None;
        let ui = &self.quick_ui;
        let (name, group, command) = (ui.name.clone(), ui.group.clone(), ui.command.clone());
        set_input(&name, "", window, cx);
        set_input(&group, "", window, cx);
        command.update(cx, |state, cx| state.set_value("", window, cx));
    }

    fn quick_begin_edit(
        &mut self,
        command: &QuickCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.quick_ui.draft = Some(Draft {
            id: Some(command.id.clone()),
            color: command.color.clone(),
            enter: command.enter,
        });
        self.quick_ui.error = None;
        let ui = &self.quick_ui;
        let (name, group, body) = (ui.name.clone(), ui.group.clone(), ui.command.clone());
        set_input(&name, &command.name, window, cx);
        set_input(&group, command.group.as_deref().unwrap_or(""), window, cx);
        let text = command.command.clone();
        body.update(cx, |state, cx| state.set_value(text, window, cx));
    }

    // ---- 保存 / 删除 / 移动 ----

    pub(crate) fn quick_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = self.quick_ui.draft.clone() else {
            return;
        };
        let name = self.quick_ui.name.read(cx).value().to_string();
        let group = self.quick_ui.group.read(cx).value().to_string();
        let command = self.quick_ui.command.read(cx).value().to_string();
        if let Some(problem) = quick_commands::validate(&name, &command, &group) {
            self.quick_ui.error = Some(problem);
            cx.notify();
            return;
        }
        let commands = self.bar_settings().commands.clone();
        if draft.id.is_none() && commands.len() >= COMMANDS_MAX {
            self.quick_ui.error = Some(format!("快捷命令最多 {COMMANDS_MAX} 条"));
            cx.notify();
            return;
        }
        let id = draft.id.clone().unwrap_or_else(|| {
            self.quick_ui.next_id += 1;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis());
            quick_commands::new_id(now, self.quick_ui.next_id)
        });
        let group = group.trim();
        let item = QuickCommand {
            id,
            name: name.trim().to_owned(),
            command,
            group: (!group.is_empty()).then(|| group.to_owned()),
            enter: draft.enter,
            color: draft.color,
        };
        let next = quick_commands::upsert(&commands, item);
        self.save_quick_commands(&next, window, cx);
        self.quick_ui.draft = None;
        self.quick_ui.error = None;
        cx.notify();
    }

    pub(crate) fn ask_delete_quick_command(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(command) = self
            .bar_settings()
            .commands
            .iter()
            .find(|item| item.id == id)
        else {
            return;
        };
        self.modal = Some(Modal::Confirm(Confirm {
            title: format!("删除快捷命令「{}」？", command.name),
            message: command.command.lines().next().unwrap_or("").to_owned(),
            confirm_label: Some("删除".to_owned()),
            destructive: true,
            action: ConfirmAction::DeleteQuickCommand(id.to_owned()),
        }));
        cx.notify();
    }

    pub(crate) fn do_delete_quick_command(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let next: Vec<QuickCommand> = self
            .bar_settings()
            .commands
            .iter()
            .filter(|item| item.id != id)
            .cloned()
            .collect();
        self.save_quick_commands(&next, window, cx);
        if self
            .quick_ui
            .draft
            .as_ref()
            .is_some_and(|draft| draft.id.as_deref() == Some(id))
        {
            self.quick_ui.draft = None;
        }
        cx.notify();
    }

    pub(crate) fn quick_move(
        &mut self,
        id: &str,
        delta: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let commands = self.bar_settings().commands.clone();
        if let Some(next) = quick_commands::move_in_group(&commands, id, delta) {
            self.save_quick_commands(&next, window, cx);
        }
    }

    // ---- 画 ----

    pub(super) fn quick_panel_content(
        &mut self,
        tokens: &Tokens,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PanelContent {
        let entity = cx.entity();
        let commands = self.bar_settings().commands.clone();
        let query = self.quick_ui.search.read(cx).value().to_string();
        let searching = !query.trim().is_empty();
        let found = quick_commands::search(&commands, &query);
        let sections = quick_commands::sections(&found);
        let groups = quick_commands::groups_of(&commands);
        let grouped = !groups.is_empty();
        let draft = self.quick_ui.draft.clone();
        let selected_id = draft.as_ref().and_then(|d| d.id.clone());
        let hover = tokens.hover;

        let render_row = |command: &QuickCommand, index: usize| -> AnyElement {
            let selected = selected_id.as_deref() == Some(command.id.as_str());
            let (can_up, can_down) = quick_commands::can_move(&commands, &command.id);
            let color = command
                .color
                .as_deref()
                .and_then(quick_commands::color_style)
                .map(|(_, hex)| hex);
            let id = command.id.clone();
            let snapshot = command.clone();
            let pick = entity.clone();
            let first_line: SharedString = command
                .command
                .lines()
                .next()
                .unwrap_or("")
                .to_owned()
                .into();
            let name: SharedString = command.name.clone().into();
            let small_button =
                |key: &'static str, icon: Icon, tip: &'static str, enabled: bool, on: RowAction| {
                    let mut button = div()
                        .id((key, index))
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(24.0))
                        .flex_none()
                        .rounded_md()
                        .text_color(tokens.text_muted)
                        .when(!enabled, |d| d.opacity(0.35))
                        .when(enabled, |d| {
                            d.cursor_pointer()
                                .hover(move |style| style.bg(hover).text_color(tokens.text))
                        })
                        .tooltip(move |window, cx| {
                            gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx)
                        })
                        .child(icon.size_3());
                    if enabled {
                        button = button.on_click(move |_, window, cx| on(window, cx));
                    }
                    button
                };
            let (up_id, down_id, edit_id, del_id) =
                (id.clone(), id.clone(), id.clone(), id.clone());
            let (up_e, down_e, del_e) = (entity.clone(), entity.clone(), entity.clone());
            let edit_e = entity.clone();
            h_flex()
                .id(("quick-row", index))
                .w_full()
                .items_center()
                .gap_1()
                .pr_1()
                .rounded_md()
                .border_1()
                .border_color(if selected {
                    tokens.accent
                } else {
                    tokens.border
                })
                .when(selected, |d| d.bg(tokens.accent_soft))
                .child(
                    h_flex()
                        .id(("quick-row-pick", index))
                        .flex_1()
                        .min_w_0()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_1p5()
                        .cursor_pointer()
                        .child(
                            div()
                                .size(px(8.0))
                                .flex_none()
                                .rounded_full()
                                .when_some(color, |d, hex| {
                                    d.bg(::gpui::Hsla::from(::gpui::rgba((hex << 8) | 0xFF)))
                                }),
                        )
                        .child(
                            v_flex()
                                .min_w_0()
                                .child(div().truncate().text_sm().child(name))
                                .child(
                                    div()
                                        .truncate()
                                        .text_xs()
                                        .font_family("Consolas")
                                        .text_color(tokens.text_muted)
                                        .child(first_line),
                                ),
                        )
                        .on_click(move |_, window, cx| {
                            let snapshot = snapshot.clone();
                            pick.update(cx, |this, cx| {
                                this.quick_begin_edit(&snapshot, window, cx);
                                cx.notify();
                            });
                        }),
                )
                .child(small_button(
                    "quick-up",
                    Icon::new(IconName::ArrowUp),
                    if searching {
                        "上移（搜索时不可用）"
                    } else {
                        "上移"
                    },
                    can_up && !searching,
                    Box::new(move |window, cx| {
                        let id = up_id.clone();
                        up_e.update(cx, |this, cx| this.quick_move(&id, -1, window, cx));
                    }),
                ))
                .child(small_button(
                    "quick-down",
                    Icon::new(IconName::ArrowDown),
                    if searching {
                        "下移（搜索时不可用）"
                    } else {
                        "下移"
                    },
                    can_down && !searching,
                    Box::new(move |window, cx| {
                        let id = down_id.clone();
                        down_e.update(cx, |this, cx| this.quick_move(&id, 1, window, cx));
                    }),
                ))
                .child(small_button(
                    "quick-edit",
                    Icon::new(crate::icons::Ico::Pencil),
                    "编辑",
                    true,
                    Box::new(move |window, cx| {
                        let id = edit_id.clone();
                        edit_e.update(cx, |this, cx| {
                            this.open_quick_panel_edit(&id, window, cx);
                        });
                    }),
                ))
                .child(small_button(
                    "quick-delete",
                    Icon::new(crate::icons::Ico::Trash),
                    "删除",
                    true,
                    Box::new(move |_window, cx| {
                        let id = del_id.clone();
                        del_e.update(cx, |this, cx| this.ask_delete_quick_command(&id, cx));
                    }),
                ))
                .into_any_element()
        };

        let mut rows: Vec<AnyElement> = Vec::new();
        let mut counter = 0usize;
        for section in &sections {
            if !grouped {
                for command in &section.items {
                    rows.push(render_row(command, counter));
                    counter += 1;
                }
                continue;
            }
            // 搜索时有匹配的分组自动展开（不动用户折起来的状态，清掉搜索后还是原样）。
            let open = searching || !self.quick_ui.collapsed.contains(&section.group);
            let key = section.group.clone();
            let toggle = entity.clone();
            let title: SharedString = if section.group.is_empty() {
                "未分组".into()
            } else {
                section.group.clone().into()
            };
            rows.push(
                h_flex()
                    .id(("quick-group-head", counter))
                    .w_full()
                    .items_center()
                    .gap_1()
                    .px_1()
                    .py_1()
                    .rounded_md()
                    .text_xs()
                    .text_color(tokens.text_muted)
                    .when(!searching, |d| {
                        d.cursor_pointer().hover(move |style| style.bg(hover))
                    })
                    .child(
                        Icon::new(if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size_3(),
                    )
                    .child(div().truncate().child(title))
                    .child(div().ml_auto().child(format!("{}", section.items.len())))
                    .on_click(move |_, _window, cx| {
                        let key = key.clone();
                        toggle.update(cx, |this, cx| {
                            // 搜索时分组一律展开、不能折：点了也看不到变化。
                            if this.quick_ui.search.read(cx).value().trim().is_empty()
                                && !this.quick_ui.collapsed.remove(&key)
                            {
                                this.quick_ui.collapsed.insert(key);
                            }
                            cx.notify();
                        });
                    })
                    .into_any_element(),
            );
            counter += 1;
            if open {
                for command in &section.items {
                    rows.push(render_row(command, counter));
                    counter += 1;
                }
            }
        }

        let list = v_flex()
            .w_full()
            .gap_1()
            .child(note(
                "显示在终端底部的命令栏里，同一分组内的顺序就是命令栏里的顺序（上移 / 下移只在分组内移动）。\
                 单击发到当前窗格，Ctrl+单击发到这个标签的所有分屏。",
                tokens,
            ))
            .when(!commands.is_empty(), |d| {
                d.child(div().mt_1().child(Input::new(&self.quick_ui.search)))
            })
            .when(commands.is_empty(), |d| {
                d.child(note(
                    "还没有快捷命令，点右上角「新建命令」添加。",
                    tokens,
                ))
            })
            .when(!commands.is_empty() && sections.is_empty(), |d| {
                d.child(note("没有匹配的命令。", tokens))
            })
            .children(rows);

        let form = draft.as_ref().map(|draft| {
            let ui = &self.quick_ui;
            let color_row = {
                let mut options: Vec<Option<&'static str>> = vec![None];
                options.extend(quick_commands::COLORS.iter().copied().map(Some));
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .children(options.into_iter().enumerate().map(|(index, color)| {
                        let active = draft.color.as_deref() == color;
                        let hex = color.and_then(quick_commands::color_style).map(|(_, h)| h);
                        let pick = entity.clone();
                        let label = color
                            .and_then(quick_commands::color_style)
                            .map_or("默认", |(label, _)| label);
                        div()
                            .id(("quick-color", index))
                            .size(px(20.0))
                            .flex_none()
                            .rounded_full()
                            .cursor_pointer()
                            .border_2()
                            .border_color(if active {
                                tokens.text
                            } else {
                                ::gpui::Hsla::transparent_black()
                            })
                            .when_some(hex, |d, hex| {
                                d.bg(::gpui::Hsla::from(::gpui::rgba((hex << 8) | 0xFF)))
                            })
                            .when(hex.is_none(), |d| {
                                d.border_color(if active {
                                    tokens.text
                                } else {
                                    tokens.text_muted
                                })
                            })
                            .tooltip(move |window, cx| {
                                gpui_kit::component::tooltip::Tooltip::new(label).build(window, cx)
                            })
                            .on_click(move |_, _window, cx| {
                                pick.update(cx, |this, cx| {
                                    if let Some(draft) = this.quick_ui.draft.as_mut() {
                                        draft.color = color.map(str::to_owned);
                                    }
                                    cx.notify();
                                });
                            })
                    }))
            };
            let group_chips = (!groups.is_empty()).then(|| {
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .children(groups.iter().enumerate().map(|(index, name)| {
                        let pick = entity.clone();
                        let value = name.clone();
                        div()
                            .id(("quick-group-chip", index))
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .border_1()
                            .border_color(tokens.border)
                            .text_xs()
                            .text_color(tokens.text_muted)
                            .cursor_pointer()
                            .hover(move |style| style.bg(hover))
                            .child(SharedString::from(name.clone()))
                            .on_click(move |_, window, cx| {
                                let value = value.clone();
                                pick.update(cx, |this, cx| {
                                    let group = this.quick_ui.group.clone();
                                    set_input(&group, &value, window, cx);
                                });
                            })
                    }))
            });
            let items: Vec<AnyElement> = vec![
                field("名称", None, Input::new(&ui.name), tokens).into_any_element(),
                v_flex()
                    .gap_1p5()
                    .child(field(
                        "分组",
                        Some("可选".to_owned()),
                        Input::new(&ui.group),
                        tokens,
                    ))
                    .children(group_chips)
                    .into_any_element(),
                field("颜色", Some("可选".to_owned()), color_row, tokens).into_any_element(),
                field(
                    "命令",
                    None,
                    Textarea::new(&ui.command)
                        .h(px(110.0))
                        .font_family("Consolas"),
                    tokens,
                )
                .into_any_element(),
                note("可以写多行，每一行按回车执行一次。", tokens).into_any_element(),
                switch_row(
                    "quick-enter",
                    "发送后回车执行",
                    Some("关掉就只填进去，改完自己按回车。".to_owned()),
                    draft.enter,
                    {
                        let entity = entity.clone();
                        move |_window, cx| {
                            entity.update(cx, |this, cx| {
                                if let Some(draft) = this.quick_ui.draft.as_mut() {
                                    draft.enter = !draft.enter;
                                }
                                cx.notify();
                            });
                        }
                    },
                    tokens,
                )
                .into_any_element(),
            ];
            v_flex()
                .w_full()
                .gap_3()
                .child(super::widgets::section(
                    if draft.id.is_some() {
                        "编辑命令"
                    } else {
                        "新建命令"
                    },
                    items,
                    tokens,
                ))
                .children(
                    self.quick_ui
                        .error
                        .clone()
                        .map(|error| super::widgets::error_note(error, tokens.danger)),
                )
        });

        let footer = draft.as_ref().map(|_| {
            h_flex()
                .gap_2()
                .justify_end()
                .child(text_button(
                    "quick-cancel",
                    "取消",
                    ButtonLook::Outline,
                    act(&entity, |this, _window, cx| {
                        this.quick_ui.draft = None;
                        this.quick_ui.error = None;
                        cx.notify();
                    }),
                ))
                .child(text_button(
                    "quick-save",
                    "保存",
                    ButtonLook::Primary,
                    act(&entity, |this, window, cx| this.quick_save(window, cx)),
                ))
                .into_any_element()
        });

        PanelContent {
            title: "快捷命令".into(),
            subtitle: Some(format!("{} 条", commands.len()).into()),
            back: None,
            actions: vec![
                text_button(
                    "quick-new",
                    "新建命令",
                    ButtonLook::Outline,
                    act(&entity, |this, window, cx| {
                        this.open_quick_panel_new(window, cx);
                    }),
                )
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
