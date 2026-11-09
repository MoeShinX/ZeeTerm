//! 管理快捷命令的右侧面板（0.2.14 `QuickCommandsPanel.tsx`）：搜索、按分组归拢的列表（可折叠）、
//! 组内上移 / 下移、编辑 / 删除。命令存在设置表的 `quick_commands` 里，命令栏读的是同一份。
//!
//! 新建和编辑一律是居中的对话框（`Modal::QuickEdit`：名称、分组、颜色、命令、「发送后回车执行」），
//! 所有入口（右侧面板、管理页、命令栏的新建 / 右键编辑）走同一个方法；列表里不再内嵌表单，
//! 始终占满。双击列表里的一条 = 编辑，单击只是选中；保存后那一条短暂高亮，必要时滚进可见范围。

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use ::gpui::{Entity, ScrollHandle, SharedString, Window, canvas, px};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::MainWindow;
use super::modal::{Confirm, ConfirmAction, Modal, QuickEditDialog};
use super::nav::MainView;
use super::quick_commands::{self, Field, Filled, Focus, QuickCommand, Seed};
use super::side_panel::{PanelContent, QUICK_COMMANDS};
use super::widgets::{
    ButtonLook, act, empty_state, error_note, field_with_error, note, switch_row, text_button,
};
use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::typography::{Role, Typo as _};

type RowAction = Box<dyn Fn(&mut Window, &mut App)>;

/// 保存后高亮多久。
const FLASH_FOR: Duration = Duration::from_millis(1800);

/// 快捷命令面板自己的界面文案。
///
/// 命令正文、分组名、颜色名和 `quick_commands` 里的校验提示属于用户数据或别的模块，原样显示。
#[derive(Clone, Copy, Debug)]
enum QuickPanelMsg {
    SearchPlaceholder,
    NamePlaceholder,
    GroupPlaceholder,
    DeleteTitlePrefix,
    DeleteTitleSuffix,
    Delete,
    UpSearching,
    Up,
    DownSearching,
    Down,
    EditTip,
    Ungrouped,
    ListNote,
    EmptyTitle,
    NewCommand,
    NoMatches,
    PanelTitle,
    CountSuffix,
    NameLabel,
    GroupLabel,
    Optional,
    ColorLabel,
    CommandLabel,
    MultiLineHint,
    EnterRow,
    EnterHint,
    SuggestRow,
    SuggestHint,
    ShortcutHint,
    Cancel,
    Save,
    DefaultColor,
}

impl Msg for QuickPanelMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::SearchPlaceholder => "搜索名称、命令或分组",
            Self::NamePlaceholder => "例如：端口查看",
            Self::GroupPlaceholder => "可选，例如：Debian、面板安装、测试脚本",
            Self::DeleteTitlePrefix => "删除快捷命令「",
            Self::DeleteTitleSuffix => "」？",
            Self::Delete => "删除",
            Self::UpSearching => "上移（搜索时不可用）",
            Self::Up => "上移",
            Self::DownSearching => "下移（搜索时不可用）",
            Self::Down => "下移",
            Self::EditTip => "编辑（双击这一行也可以）",
            Self::Ungrouped => "未分组",
            Self::ListNote => {
                "显示在终端底部的命令栏里，同一分组内的顺序就是命令栏里的顺序（上移 / 下移只在分组内移动）。单击发到当前窗格，Ctrl+单击发到这个标签的所有分屏。双击这里的一条可以编辑。"
            }
            Self::EmptyTitle => "暂无快捷命令",
            Self::NewCommand => "新建命令",
            Self::NoMatches => "没有匹配的命令。",
            Self::PanelTitle => "快捷命令",
            Self::CountSuffix => " 条",
            Self::NameLabel => "名称",
            Self::GroupLabel => "分组",
            Self::Optional => "可选",
            Self::ColorLabel => "颜色",
            Self::CommandLabel => "命令",
            Self::MultiLineHint => "可以写多行，每一行按回车执行一次。",
            Self::EnterRow => "发送后回车执行",
            Self::EnterHint => "关掉就只填进去，改完自己按回车。",
            Self::SuggestRow => "排除自动建议",
            Self::SuggestHint => {
                "勾上后，这条命令不会出现在终端输入时的自动候选里（含密码、令牌的命令即使不勾也不会出现）。"
            }
            Self::ShortcutHint => "Ctrl+Enter 保存，Esc 取消",
            Self::Cancel => "取消",
            Self::Save => "保存",
            Self::DefaultColor => "默认",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::SearchPlaceholder => "Search name, command, or group",
            Self::NamePlaceholder => "e.g. Port check",
            Self::GroupPlaceholder => "Optional, e.g. Debian, panel setup, test scripts",
            Self::DeleteTitlePrefix => "Delete snippet \"",
            Self::DeleteTitleSuffix => "\"?",
            Self::Delete => "Delete",
            Self::UpSearching => "Move up (unavailable while searching)",
            Self::Up => "Move up",
            Self::DownSearching => "Move down (unavailable while searching)",
            Self::Down => "Move down",
            Self::EditTip => "Edit (or double-click this row)",
            Self::Ungrouped => "Ungrouped",
            Self::ListNote => {
                "Shown in the command bar at the bottom of the terminal; the order within a group is the order in the bar (move up / down only within a group). Click to send to the current pane; Ctrl+click to send to all splits in this tab. Double-click a row here to edit it."
            }
            Self::EmptyTitle => "No snippets yet",
            Self::NewCommand => "New Command",
            Self::NoMatches => "No matching commands.",
            Self::PanelTitle => "Snippets",
            Self::CountSuffix => " items",
            Self::NameLabel => "Name",
            Self::GroupLabel => "Group",
            Self::Optional => "Optional",
            Self::ColorLabel => "Color",
            Self::CommandLabel => "Command",
            Self::MultiLineHint => "Multiple lines are allowed; each line runs when Enter is sent.",
            Self::EnterRow => "Send Enter after the command",
            Self::EnterHint => {
                "When off, the command is only typed in; press Enter yourself after editing."
            }
            Self::SuggestRow => "Exclude from auto-suggestions",
            Self::SuggestHint => {
                "When on, this command never shows up in the suggestions while typing in the terminal (commands that look like they contain a password or token are left out even when this is off)."
            }
            Self::ShortcutHint => "Ctrl+Enter to save, Esc to cancel",
            Self::Cancel => "Cancel",
            Self::Save => "Save",
            Self::DefaultColor => "Default",
        }
    }
}

pub struct QuickUi {
    pub search: Entity<InputState>,
    pub name: Entity<InputState>,
    pub group: Entity<InputState>,
    pub command: Entity<TextareaState>,
    /// 折起来的分组（空串 = 未分组）；不持久化，每次打开面板都是全部展开。
    pub collapsed: HashSet<String>,
    /// 单击选中的一条（只描边）。
    pub selected: Option<String>,
    /// 刚保存的一条：整行高亮一小会儿（`flash_gen` 防止旧的计时器把新的高亮提前清掉）。
    pub flash: Option<String>,
    flash_gen: u64,
    /// 刚保存的那一条要滚进可见范围：画那一行时看一眼，已经看得见就清掉。
    reveal: Rc<RefCell<Option<String>>>,
    next_id: u64,
}

impl QuickUi {
    pub fn new(window: &mut Window, cx: &mut Context<MainWindow>) -> Self {
        Self {
            search: cx.new(|cx| {
                InputState::new(window, cx).placeholder(t(QuickPanelMsg::SearchPlaceholder))
            }),
            name: cx.new(|cx| {
                InputState::new(window, cx).placeholder(t(QuickPanelMsg::NamePlaceholder))
            }),
            group: cx.new(|cx| {
                InputState::new(window, cx).placeholder(t(QuickPanelMsg::GroupPlaceholder))
            }),
            command: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder("ss -tlnp")
                    .rows(9)
            }),
            collapsed: HashSet::new(),
            selected: None,
            flash: None,
            flash_gen: 0,
            reveal: Rc::new(RefCell::new(None)),
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

    /// 命令栏上的管理按钮和主机页上的快捷命令按钮：开着就关，没开就开右侧面板。
    pub(crate) fn toggle_quick_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.panels.visible() == Some(QUICK_COMMANDS) {
            self.panels.close_side();
            self.focus_content(window, cx);
        } else {
            self.panels.open_side(QUICK_COMMANDS);
        }
        cx.notify();
    }

    /// 新建：居中弹出对话框，哪个入口都一样（命令栏的 +、面板和管理页的「新建命令」、空状态按钮）。
    pub(crate) fn open_quick_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_quick_dialog_with(Seed::blank(), window, cx);
    }

    /// 编辑：同一个对话框，字段预填这一条现在的内容（命令栏右键「编辑」、列表里的铅笔、双击）。
    pub(crate) fn open_quick_edit(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(command) = self
            .bar_settings()
            .commands
            .iter()
            .find(|item| item.id == id)
            .cloned()
        else {
            return;
        };
        self.open_quick_dialog_with(Seed::of(&command), window, cx);
    }

    fn open_quick_dialog_with(&mut self, seed: Seed, window: &mut Window, cx: &mut Context<Self>) {
        let ui = &self.quick_ui;
        let (name, group, command) = (ui.name.clone(), ui.group.clone(), ui.command.clone());
        set_input(&name, &seed.name, window, cx);
        set_input(&group, &seed.group, window, cx);
        let text = seed.command.clone();
        command.update(cx, |state, cx| state.set_value(text, window, cx));
        self.modal = Some(Modal::QuickEdit(QuickEditDialog {
            id: seed.id.clone(),
            color: seed.color.clone(),
            enter: seed.enter,
            no_suggest: seed.no_suggest,
            error: None,
        }));
        let handle = match seed.focus() {
            Focus::Name => name.read(cx).focus_handle(cx),
            Focus::Command => command.read(cx).focus_handle(cx),
        };
        window.focus(&handle, cx);
        cx.notify();
    }

    // ---- 保存 / 删除 / 移动 ----

    /// 对话框的「保存」和 Ctrl+Enter。校验不过就把原因留在对话框里（对话框不关，输入不丢）。
    pub(crate) fn quick_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Modal::QuickEdit(dialog)) = &self.modal else {
            return;
        };
        let (id, color, enter, no_suggest) = (
            dialog.id.clone(),
            dialog.color.clone(),
            dialog.enter,
            dialog.no_suggest,
        );
        let name = self.quick_ui.name.read(cx).value().to_string();
        let group = self.quick_ui.group.read(cx).value().to_string();
        let command = self.quick_ui.command.read(cx).value().to_string();
        let commands = self.bar_settings().commands.clone();
        let next_id = self.quick_ui.next_id + 1;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let built = quick_commands::build_item(
            &Filled {
                id: id.as_deref(),
                name: &name,
                group: &group,
                command: &command,
                color: color.as_deref(),
                enter,
                no_suggest,
            },
            &commands,
            || quick_commands::new_id(now, next_id),
        );
        let item = match built {
            Ok(item) => item,
            Err(problem) => {
                if let Some(Modal::QuickEdit(dialog)) = &mut self.modal {
                    dialog.error = Some(problem);
                }
                cx.notify();
                return;
            }
        };
        self.quick_ui.next_id = next_id;
        let saved_id = item.id.clone();
        let next = quick_commands::upsert(&commands, item);
        self.save_quick_commands(&next, window, cx);
        self.modal = None;
        self.focus_content(window, cx);
        self.flash_quick_command(&saved_id, cx);
        cx.notify();
    }

    /// 让保存的那一条高亮一小会儿，并滚到看得见的地方。
    fn flash_quick_command(&mut self, id: &str, cx: &mut Context<Self>) {
        self.quick_ui.flash = Some(id.to_owned());
        self.quick_ui.flash_gen += 1;
        *self.quick_ui.reveal.borrow_mut() = Some(id.to_owned());
        let token = self.quick_ui.flash_gen;
        cx.spawn(async move |this, cx| {
            // 对话框刚关、列表刚多了一条：头几帧里滚动容器的尺寸可能还没量好，隔一会儿再重画几次，
            // 让「滚到可见」一定有机会跑完（重画只是让画那一行的探针再看一眼，已经看得见就什么也不做）。
            for wait in [40u64, 120, 300] {
                cx.background_executor()
                    .timer(Duration::from_millis(wait))
                    .await;
                let _ = this.update(cx, |_, cx| cx.notify());
            }
            cx.background_executor().timer(FLASH_FOR).await;
            let _ = this.update(cx, |this, cx| {
                if this.quick_ui.flash_gen == token {
                    this.quick_ui.flash = None;
                    *this.quick_ui.reveal.borrow_mut() = None;
                    cx.notify();
                }
            });
        })
        .detach();
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
            title: format!(
                "{}{}{}",
                t(QuickPanelMsg::DeleteTitlePrefix),
                command.name,
                t(QuickPanelMsg::DeleteTitleSuffix)
            ),
            message: command.command.lines().next().unwrap_or("").to_owned(),
            confirm_label: Some(t(QuickPanelMsg::Delete).to_owned()),
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
        if self.quick_ui.selected.as_deref() == Some(id) {
            self.quick_ui.selected = None;
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

    // ---- 画：列表 ----

    pub(super) fn quick_panel_content(
        &mut self,
        tokens: &Tokens,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PanelContent {
        let entity = cx.entity();
        let commands = self.bar_settings().commands.clone();
        // 「管理 + 设置」页的搜索框在页面顶部（同一个输入框），列表里不再画第二个。
        let on_page = self.nav.view == MainView::Settings;
        let query = self.quick_ui.search.read(cx).value().to_string();
        let searching = !query.trim().is_empty();
        let found = quick_commands::search(&commands, &query);
        let sections = quick_commands::sections(&found);
        let groups = quick_commands::groups_of(&commands);
        let grouped = !groups.is_empty();
        let selected_id = self.quick_ui.selected.clone();
        let flash_id = self.quick_ui.flash.clone();
        // 对话框正在编辑的那一条：列表里描边，一眼看出改的是哪条。
        let editing_id = match &self.modal {
            Some(Modal::QuickEdit(dialog)) => dialog.id.clone(),
            _ => None,
        };
        let reveal = self.quick_ui.reveal.clone();
        let scroller = if on_page {
            self.settings_scroll.clone()
        } else {
            self.panel_scroll.clone()
        };
        let hover = tokens.hover;

        let render_row = |command: &QuickCommand, index: usize| -> AnyElement {
            let flashing = flash_id.as_deref() == Some(command.id.as_str());
            let marked = flashing
                || selected_id.as_deref() == Some(command.id.as_str())
                || editing_id.as_deref() == Some(command.id.as_str());
            let (can_up, can_down) = quick_commands::can_move(&commands, &command.id);
            let color = command
                .color
                .as_deref()
                .and_then(quick_commands::color_style)
                .map(|(_, hex)| hex);
            let id = command.id.clone();
            let pick_id = command.id.clone();
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
            // 保存后要滚到的那一条：贴着整行铺一块看不见的画布，量出这一行此刻的位置，不在可见
            // 范围里就把滚动偏移挪过去（量到了才清掉待办，量不到下一帧再来）。
            let reveal_probe = (flashing && reveal.borrow().is_some()).then(|| {
                let (reveal, scroller) = (reveal.clone(), scroller.clone());
                canvas(
                    move |bounds, window, _cx| {
                        reveal_into_view(&scroller, &reveal, bounds, window);
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full()
            });
            h_flex()
                .id(("quick-row", index))
                .relative()
                .w_full()
                .items_center()
                .gap_1()
                .pr_1()
                .rounded_md()
                .border_1()
                .border_color(if marked { tokens.accent } else { tokens.border })
                .when(
                    flashing || editing_id.as_deref() == Some(command.id.as_str()),
                    |d| d.bg(tokens.accent_soft),
                )
                .children(reveal_probe)
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
                                .child(div().truncate().typo(Role::ItemName, tokens).child(name))
                                .child(
                                    div()
                                        .truncate()
                                        .typo(Role::Caption, tokens)
                                        .font_family(crate::fonts::mono_text_family())
                                        .child(first_line),
                                ),
                        )
                        .on_click(move |event, window, cx| {
                            let id = pick_id.clone();
                            let double = event.click_count() >= 2;
                            pick.update(cx, |this, cx| {
                                // 双击 = 编辑；单击只选中（不弹对话框，否则双击的第二下就点到遮罩上了）。
                                if double {
                                    this.open_quick_edit(&id, window, cx);
                                } else {
                                    this.quick_ui.selected = Some(id);
                                    cx.notify();
                                }
                            });
                        }),
                )
                .child(small_button(
                    "quick-up",
                    Icon::new(IconName::ArrowUp),
                    if searching {
                        t(QuickPanelMsg::UpSearching)
                    } else {
                        t(QuickPanelMsg::Up)
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
                        t(QuickPanelMsg::DownSearching)
                    } else {
                        t(QuickPanelMsg::Down)
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
                    t(QuickPanelMsg::EditTip),
                    true,
                    Box::new(move |window, cx| {
                        let id = edit_id.clone();
                        edit_e.update(cx, |this, cx| this.open_quick_edit(&id, window, cx));
                    }),
                ))
                .child(small_button(
                    "quick-delete",
                    Icon::new(crate::icons::Ico::Trash),
                    t(QuickPanelMsg::Delete),
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
            // 刚保存的那一条所在的分组也要展开，不然高亮和滚动都落空。
            let holds_flash = flash_id
                .as_deref()
                .is_some_and(|id| section.items.iter().any(|item| item.id == id));
            let open =
                searching || holds_flash || !self.quick_ui.collapsed.contains(&section.group);
            let key = section.group.clone();
            let toggle = entity.clone();
            let title: SharedString = if section.group.is_empty() {
                t(QuickPanelMsg::Ungrouped).into()
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
                    .typo(Role::Caption, tokens)
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
            .child(note(t(QuickPanelMsg::ListNote), tokens))
            .when(!commands.is_empty() && !on_page, |d| {
                d.child(div().mt_1().child(Input::new(&self.quick_ui.search)))
            })
            .when(commands.is_empty(), |d| {
                d.child(empty_state(
                    t(QuickPanelMsg::EmptyTitle),
                    None,
                    Some(
                        text_button(
                            "quick-empty-new",
                            t(QuickPanelMsg::NewCommand),
                            ButtonLook::Primary,
                            act(&entity, |this, window, cx| {
                                this.open_quick_new(window, cx);
                            }),
                        )
                        .icon(IconName::Plus)
                        .into_any_element(),
                    ),
                    tokens,
                ))
            })
            .when(!commands.is_empty() && sections.is_empty(), |d| {
                d.child(note(t(QuickPanelMsg::NoMatches), tokens))
            })
            .children(rows);

        PanelContent {
            title: t(QuickPanelMsg::PanelTitle).into(),
            subtitle: Some(format!("{}{}", commands.len(), t(QuickPanelMsg::CountSuffix)).into()),
            back: None,
            actions: vec![
                text_button(
                    "quick-new",
                    t(QuickPanelMsg::NewCommand),
                    ButtonLook::Outline,
                    act(&entity, |this, window, cx| {
                        this.open_quick_new(window, cx);
                    }),
                )
                .icon(IconName::Plus)
                .into_any_element(),
            ],
            body: list.into_any_element(),
            footer: None,
        }
    }

    // ---- 画：新建 / 编辑对话框 ----

    /// 居中对话框里的内容：标题、字段（内容多时中间这一段自己滚动）、底部的提示和按钮。
    /// 外壳、遮罩、Esc 关闭在 `modal::render_modal`。
    pub(super) fn quick_edit_body(
        &self,
        dialog: &QuickEditDialog,
        tokens: &Tokens,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let ui = &self.quick_ui;
        let hover = tokens.hover;
        let groups = quick_commands::groups_of(&self.bar_settings().commands);
        let error_of = |field: Field| {
            dialog
                .error
                .as_ref()
                .filter(|(at, _)| *at == field)
                .map(|(_, reason)| reason.clone())
        };
        let title = quick_commands::dialog_title(dialog.id.is_some());

        let color_row = {
            let mut options: Vec<Option<&'static str>> = vec![None];
            options.extend(quick_commands::COLORS.iter().copied().map(Some));
            h_flex()
                .gap_2()
                .flex_wrap()
                .children(options.into_iter().enumerate().map(|(index, color)| {
                    let active = dialog.color.as_deref() == color;
                    let hex = color.and_then(quick_commands::color_style).map(|(_, h)| h);
                    let pick = entity.clone();
                    let label = color
                        .and_then(quick_commands::color_style)
                        .map_or(t(QuickPanelMsg::DefaultColor), |(label, _)| label);
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
                                if let Some(Modal::QuickEdit(dialog)) = &mut this.modal {
                                    dialog.color = color.map(str::to_owned);
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
                        .typo(Role::Caption, tokens)
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

        let fields = v_flex()
            .id("quick-edit-fields")
            .w_full()
            .flex_1()
            .min_h_0()
            .gap_3()
            .overflow_y_scroll()
            .child(field_with_error(
                t(QuickPanelMsg::NameLabel),
                None,
                Input::new(&ui.name),
                error_of(Field::Name).as_deref(),
                tokens,
            ))
            .child(
                v_flex()
                    .gap_1p5()
                    .child(field_with_error(
                        t(QuickPanelMsg::GroupLabel),
                        Some(t(QuickPanelMsg::Optional).to_owned()),
                        Input::new(&ui.group),
                        error_of(Field::Group).as_deref(),
                        tokens,
                    ))
                    .children(group_chips),
            )
            .child(super::widgets::field(
                t(QuickPanelMsg::ColorLabel),
                Some(t(QuickPanelMsg::Optional).to_owned()),
                color_row,
                tokens,
            ))
            .child(
                v_flex()
                    .gap_1p5()
                    .child(field_with_error(
                        t(QuickPanelMsg::CommandLabel),
                        None,
                        Textarea::new(&ui.command)
                            .h(px(200.0))
                            .font_family(crate::fonts::mono_text_family()),
                        error_of(Field::Command).as_deref(),
                        tokens,
                    ))
                    .child(
                        div()
                            .typo(Role::Caption, tokens)
                            .text_color(tokens.text_faint)
                            .child(t(QuickPanelMsg::MultiLineHint)),
                    ),
            )
            .child(switch_row(
                "quick-enter",
                t(QuickPanelMsg::EnterRow),
                Some(t(QuickPanelMsg::EnterHint).to_owned()),
                dialog.enter,
                {
                    let entity = entity.clone();
                    move |_window, cx| {
                        entity.update(cx, |this, cx| {
                            if let Some(Modal::QuickEdit(dialog)) = &mut this.modal {
                                dialog.enter = !dialog.enter;
                            }
                            cx.notify();
                        });
                    }
                },
                tokens,
            ))
            .child(switch_row(
                "quick-no-suggest",
                t(QuickPanelMsg::SuggestRow),
                Some(t(QuickPanelMsg::SuggestHint).to_owned()),
                dialog.no_suggest,
                {
                    let entity = entity.clone();
                    move |_window, cx| {
                        entity.update(cx, |this, cx| {
                            if let Some(Modal::QuickEdit(dialog)) = &mut this.modal {
                                dialog.no_suggest = !dialog.no_suggest;
                            }
                            cx.notify();
                        });
                    }
                },
                tokens,
            ))
            .children(error_of(Field::Form).map(|reason| error_note(reason, tokens.danger)));

        let cancel = entity.clone();
        let save = entity.clone();
        v_flex()
            .w_full()
            .flex_1()
            .min_h_0()
            .gap_4()
            .child(div().typo(Role::GroupTitle, tokens).child(title))
            .child(fields)
            .child(
                h_flex()
                    .w_full()
                    .flex_none()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .typo(Role::Caption, tokens)
                            .text_color(tokens.text_faint)
                            .child(t(QuickPanelMsg::ShortcutHint)),
                    )
                    .child(text_button(
                        "modal-cancel",
                        t(QuickPanelMsg::Cancel),
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            cancel.update(cx, |this, cx| this.close_modal(window, cx));
                        },
                    ))
                    .child(text_button(
                        "modal-ok",
                        t(QuickPanelMsg::Save),
                        ButtonLook::Primary,
                        move |_, window, cx| {
                            save.update(cx, |this, cx| this.confirm_modal(window, cx));
                        },
                    )),
            )
            .into_any_element()
    }
}

/// 把 `bounds`（这一行在窗口里此刻的位置）滚进 `scroller` 的可见范围，上下各留一点边。
/// 待办要等到这一行确实看得见（或者已经滚到头、没法再动）才清掉：滚动容器还没量出大小、
/// 或者这一帧只挪了偏移，下一帧还会再来看一眼。
fn reveal_into_view(
    scroller: &ScrollHandle,
    pending: &RefCell<Option<String>>,
    bounds: ::gpui::Bounds<::gpui::Pixels>,
    window: &mut Window,
) {
    let view = scroller.bounds();
    if view.size.height <= px(0.0) {
        window.refresh();
        return;
    }
    let margin = px(8.0);
    let before = scroller.offset();
    let mut offset = before;
    if bounds.top() < view.top() + margin {
        offset.y += view.top() + margin - bounds.top();
    } else if bounds.bottom() > view.bottom() - margin {
        offset.y -= bounds.bottom() - (view.bottom() - margin);
    }
    let floor = -scroller.max_offset().y.abs();
    offset.y = offset.y.min(px(0.0)).max(floor);
    if offset.y == before.y {
        pending.borrow_mut().take();
        return;
    }
    scroller.set_offset(offset);
    window.refresh();
}
