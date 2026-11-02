//! 左侧主机树：新建主机、搜索框、「全部主机 / 分组 / 主机 / 未分组」、新分组、属性面板、拖动调宽。
//!
//! 主机页和终端页共用同一棵树（0.2.14 也是）。单击主机选中并展开底部的只读属性，双击连接，右键菜单
//! 同主机页；分组行单击是选中、箭头才展开（双击行也展开 / 收起）；主机 / 分组可以拖动换分组、调顺序。

use std::ops::Range;

use ::gpui::{
    DragMoveEvent, HighlightStyle, MouseButton, SharedString, StyledText, WeakEntity, px,
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::model::Host;

use super::drag::{DragChip, DragGroup, DragHost, DragItem, DropHint, Scope};
use super::host_tree::{self, TreeRow};
use super::page_logic::host_info_rows;
use super::vault_menus::{GroupMenuSpec, HostMenuSpec, group_menu, host_menu, ungrouped_menu};
use super::widgets::{act, icon_button};
use super::{GroupSel, MainWindow};
use crate::app_theme::Tokens;

/// 缩进：每层 14px，再加行内左边距。
const INDENT: f32 = 14.0;
const ROW_HEIGHT: f32 = 28.0;
/// 属性面板固定高度（放得下 7 行：基本 5 行 + 跳板机 + 代理），切换主机时树不会跟着跳。
const INFO_HEIGHT: f32 = 184.0;

/// 名字里搜索词（小写）出现的位置（字节范围）；转小写后长度变了的少数字符就不标，宁可不标也别标错。
pub fn match_ranges(text: &str, term: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    if term.is_empty() {
        return out;
    }
    let lower = text.to_lowercase();
    if lower.len() != text.len() {
        return out;
    }
    let mut from = 0;
    while let Some(at) = lower[from..].find(term) {
        let start = from + at;
        let end = start + term.len();
        if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            break;
        }
        out.push(start..end);
        from = end;
    }
    out
}

/// 名字没匹配、是靠地址 / 用户名 / 标签匹配到的：返回匹配的那一项，让用户看得出为什么在结果里。
pub fn match_hint(host: &Host, term: &str) -> Option<String> {
    if host.name.to_lowercase().contains(term) {
        return None;
    }
    if host.host.to_lowercase().contains(term) {
        return Some(host.host.clone());
    }
    if host.username.to_lowercase().contains(term) {
        return Some(host.username.clone());
    }
    host.tags
        .iter()
        .find(|tag| tag.to_lowercase().contains(term))
        .cloned()
}

fn highlighted(text: &str, term: &str, tokens: &Tokens) -> AnyElement {
    let ranges = match_ranges(text, term);
    let text: SharedString = text.to_owned().into();
    if ranges.is_empty() {
        return div().truncate().child(text).into_any_element();
    }
    let style = HighlightStyle {
        background_color: Some(tokens.accent_soft),
        ..Default::default()
    };
    div()
        .truncate()
        .child(StyledText::new(text).with_highlights(ranges.into_iter().map(|r| (r, style))))
        .into_any_element()
}

impl MainWindow {
    pub(super) fn render_tree(
        &mut self,
        tokens: &Tokens,
        width: f32,
        _window: &mut ::gpui::Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entity = cx.entity();
        let query = self.tree_query.read(cx).value().to_string();
        let filter = host_tree::tree_filter(
            self.store.groups(),
            self.store.hosts(),
            &query,
            self.store.flat(),
        );
        let rows = host_tree::visible_rows(
            self.store.groups(),
            self.store.hosts(),
            filter.as_ref(),
            &self.tree.expanded,
            self.store.flat(),
        );
        let searching = filter.is_some();
        let term = filter.as_ref().map(|f| f.text.clone());
        let empty_result = filter
            .as_ref()
            .is_some_and(|f| f.total == 0 && f.group_ids.is_empty() && !f.ungrouped);
        let flat = self.store.flat();
        let cursor = self.tree.cursor_host.or_else(|| {
            // 搜索时回车会连第一台：把它也标出来，用户看得见回车会连哪台。
            filter.as_ref().and_then(|f| {
                host_tree::filtered_host_order(
                    self.store.groups(),
                    self.store.hosts(),
                    f,
                    self.store.flat(),
                )
                .first()
                .copied()
            })
        });

        let top = h_flex()
            .gap_1()
            .items_center()
            .p_3()
            .pb_2()
            .child(
                div().flex_1().child(
                    Button::new("tree-new-host")
                        .outline()
                        .label("新建主机")
                        .icon(IconName::Plus)
                        .compact()
                        .w_full()
                        .on_click(act(&entity, |this, window, cx| this.new_host(window, cx))),
                ),
            )
            .child(icon_button(
                "tree-collapse",
                IconName::PanelLeftClose,
                "收起侧边栏",
                act(&entity, |this, _window, cx| this.toggle_tree(cx)),
            ));

        let list = v_flex()
            .w_full()
            .px_2()
            .pb_2()
            .gap_0p5()
            .children(rows.iter().enumerate().map(|(at, row)| {
                self.render_tree_row(at, row, cursor, term.as_deref(), searching, tokens, cx)
            }));

        let resize = {
            let entity_down = entity.clone();
            let entity_reset = entity.clone();
            div()
                .id("tree-resize")
                .absolute()
                .top_0()
                .bottom_0()
                .right(px(-3.0))
                .w(px(6.0))
                .cursor_col_resize()
                .on_mouse_down(MouseButton::Left, move |event, _, cx| {
                    cx.stop_propagation();
                    // 双击恢复默认宽度，单击开始拖动。
                    if event.click_count >= 2 {
                        entity_reset.update(cx, |this, cx| this.reset_tree_width(cx));
                    } else {
                        entity_down.update(cx, |this, cx| {
                            this.tree.resizing = true;
                            cx.notify();
                        });
                    }
                })
        };

        let scroll_body = if empty_result {
            div()
                .px_2()
                .py_6()
                .text_center()
                .text_sm()
                .text_color(tokens.text_muted)
                .child("没有匹配的主机")
                .into_any_element()
        } else if self.store.hosts().is_empty() && self.store.groups().is_empty() {
            div()
                .px_2()
                .py_6()
                .text_center()
                .text_sm()
                .text_color(tokens.text_muted)
                .child(
                    self.store
                        .error()
                        .unwrap_or("主机库里还没有主机，点上面的「新建主机」添加")
                        .to_owned(),
                )
                .into_any_element()
        } else {
            list.into_any_element()
        };
        let blank_hot = matches!(self.drop_hint, Some(DropHint::IntoRoot));
        let info = self
            .info_host
            .and_then(|id| self.store.host(id))
            .map(|host| self.render_info_panel(host, tokens, cx));

        v_flex()
            .id("host-tree")
            .relative()
            .w(px(width))
            .h_full()
            .flex_none()
            .bg(tokens.background)
            .border_r_1()
            .border_color(tokens.border)
            .child(top)
            .child(
                div()
                    .px_3()
                    .pb_2()
                    .child(Input::new(&self.tree_query).cleanable(true)),
            )
            .child(
                div()
                    .id("tree-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .when(flat && blank_hot, |d| d.bg(tokens.accent_soft))
                    .on_drag_move::<DragHost>(cx.listener(
                        |this, event: &DragMoveEvent<DragHost>, _w, cx| {
                            this.drag_over_tree_blank(event, cx);
                        },
                    ))
                    .on_drop::<DragHost>(cx.listener(|this, dragged: &DragHost, window, cx| {
                        this.drop_host_on_tree_blank(dragged, window, cx);
                    }))
                    .child(scroll_body),
            )
            .child(
                h_flex()
                    .flex_none()
                    .p_2()
                    .border_t_1()
                    .border_color(tokens.border)
                    .child(
                        Button::new("tree-new-group")
                            .ghost()
                            .compact()
                            .w_full()
                            .icon(crate::icons::Ico::FolderPlus)
                            .label("新分组")
                            .on_click(act(&entity, |this, window, cx| {
                                this.new_group(None, window, cx)
                            })),
                    ),
            )
            .children(info)
            .child(resize)
    }

    /// 树收起后在主机页上留下的一列图标（终端页直接不占地方）。
    pub(super) fn render_tree_collapsed(
        &mut self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entity = cx.entity();
        v_flex()
            .id("host-tree-collapsed")
            .w(px(44.0))
            .h_full()
            .flex_none()
            .items_center()
            .gap_1()
            .py_3()
            .bg(tokens.background)
            .border_r_1()
            .border_color(tokens.border)
            .child(icon_button(
                "tree-expand",
                IconName::PanelLeftOpen,
                "展开侧边栏",
                act(&entity, |this, _window, cx| this.toggle_tree(cx)),
            ))
            .child(icon_button(
                "tree-collapsed-new-host",
                IconName::Plus,
                "新建主机",
                act(&entity, |this, window, cx| this.new_host(window, cx)),
            ))
            .child(icon_button(
                "tree-collapsed-new-group",
                crate::icons::Ico::FolderPlus,
                "新分组",
                act(&entity, |this, window, cx| this.new_group(None, window, cx)),
            ))
    }

    /// 树底部的只读属性表（名称、主机、用户名、协议、端口，有跳板机 / 指定代理再加一行）。
    fn render_info_panel(
        &self,
        host: &Host,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let rows = host_info_rows(host, self.store.hosts(), self.store.proxies());
        let id = host.id;
        v_flex()
            .id("host-info")
            .flex_none()
            .h(px(INFO_HEIGHT))
            .border_t_1()
            .border_color(tokens.border)
            .text_xs()
            .child(
                h_flex()
                    .h(px(24.0))
                    .flex_none()
                    .pl_3()
                    .pr_1()
                    .items_center()
                    .justify_between()
                    .text_color(tokens.text_muted)
                    .child(div().font_weight(FontWeight::MEDIUM).child("属性"))
                    .child(icon_button(
                        "host-info-close",
                        IconName::Close,
                        "收起",
                        act(&entity, |this, _window, cx| this.close_info_panel(cx)),
                    )),
            )
            .child(
                div()
                    .id("host-info-rows")
                    .mx_2()
                    .mb_2()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .rounded_sm()
                            .border_1()
                            .border_color(tokens.border)
                            .overflow_hidden()
                            .children(rows.into_iter().enumerate().map(|(at, row)| {
                                let copy = (row.key == "address").then(|| {
                                    icon_button(
                                        "host-info-copy",
                                        IconName::Copy,
                                        "复制地址",
                                        act(&entity, move |this, window, cx| {
                                            this.copy_address_of(id, window, cx);
                                        }),
                                    )
                                });
                                h_flex()
                                    .h(px(22.0))
                                    .items_stretch()
                                    .when(at > 0, |d| d.border_t_1().border_color(tokens.border))
                                    .child(
                                        div()
                                            .w(px(64.0))
                                            .flex_none()
                                            .flex()
                                            .items_center()
                                            .px_2()
                                            .border_r_1()
                                            .border_color(tokens.border)
                                            .bg(tokens.hover)
                                            .text_color(tokens.text_muted)
                                            .child(row.label),
                                    )
                                    .child(
                                        h_flex()
                                            .flex_1()
                                            .min_w_0()
                                            .items_center()
                                            .gap_1()
                                            .px_2()
                                            .child(
                                                div()
                                                    .truncate()
                                                    .child(SharedString::from(row.value)),
                                            )
                                            .children(copy),
                                    )
                            })),
                    ),
            )
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_tree_row(
        &self,
        at: usize,
        row: &TreeRow,
        cursor: Option<i64>,
        term: Option<&str>,
        searching: bool,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let weak: WeakEntity<MainWindow> = entity.downgrade();
        match row {
            TreeRow::All { count } => {
                let active = self.tree.selected_group == GroupSel::All;
                tree_line(("tree-row", at), 0.0, active, tokens)
                    .child(div().w(px(16.0)).flex_none())
                    .child(div().flex_1().min_w_0().truncate().child("全部主机"))
                    .child(count_label(*count, tokens))
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| this.click_group(GroupSel::All, cx));
                    })
                    .into_any_element()
            }
            TreeRow::Group {
                id,
                name,
                depth,
                expanded,
                count,
            } => {
                let group_id = *id;
                let sel = GroupSel::Group(group_id);
                let active = self.tree.selected_group == sel;
                let parent = self
                    .store
                    .groups()
                    .iter()
                    .find(|g| g.id == group_id)
                    .and_then(|g| g.parent_id);
                let hot =
                    matches!(self.drop_hint, Some(DropHint::IntoGroup(Some(g))) if g == group_id);
                let spec = GroupMenuSpec::new(self, group_id, searching);
                let menu_entity = entity.clone();
                let drag = DragGroup {
                    id: group_id,
                    parent,
                    name: name.clone(),
                };
                let chip_drag = drag.clone();
                let chip_weak = weak.clone();
                let toggle_entity = entity.clone();
                let dbl_entity = entity.clone();
                let el = tree_line(("tree-row", at), *depth as f32 * INDENT, active, tokens)
                    .relative()
                    .when(hot, |d| {
                        d.bg(tokens.accent_soft)
                            .border_1()
                            .border_color(tokens.accent)
                    })
                    .children(self.group_insert_line(group_id, tokens))
                    .child(
                        div()
                            .id(("tree-toggle", at))
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(16.0))
                            .h(px(16.0))
                            .flex_none()
                            .rounded_sm()
                            .text_color(tokens.text_muted)
                            .when(!searching, |d| {
                                d.hover(|style| style.bg(tokens.hover))
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        toggle_entity.update(cx, |this, cx| {
                                            this.toggle_group(
                                                host_tree::group_key(Some(group_id)),
                                                cx,
                                            );
                                        });
                                    })
                            })
                            .when(!searching, |d| {
                                d.child(
                                    Icon::new(if *expanded {
                                        IconName::ChevronDown
                                    } else {
                                        IconName::ChevronRight
                                    })
                                    .size_3p5(),
                                )
                            }),
                    )
                    .child(
                        Icon::new(if *expanded {
                            IconName::FolderOpen
                        } else {
                            IconName::Folder
                        })
                        .size_3p5()
                        .text_color(tokens.text_muted),
                    )
                    .child(match term {
                        Some(term) => highlighted(name, term, tokens),
                        None => div()
                            .truncate()
                            .child(SharedString::from(name.clone()))
                            .into_any_element(),
                    })
                    .child(div().flex_1())
                    .child(count_label(*count, tokens))
                    .on_click(move |event, _, cx| {
                        if event.click_count() >= 2 {
                            dbl_entity.update(cx, |this, cx| {
                                this.toggle_group(host_tree::group_key(Some(group_id)), cx);
                            });
                        } else {
                            entity.update(cx, |this, cx| {
                                this.tree.selected_group = sel;
                                cx.notify();
                            });
                        }
                    })
                    .on_drag(drag, move |_, _offset, _window, cx| {
                        let item = DragItem::Group(chip_drag.clone());
                        let weak = chip_weak.clone();
                        cx.new(|_| DragChip::new(weak, item))
                    })
                    .on_drag_move::<DragHost>(cx.listener(
                        move |this, event: &DragMoveEvent<DragHost>, _w, cx| {
                            this.drag_over_group_row_host(Some(group_id), event, cx);
                        },
                    ))
                    .on_drag_move::<DragGroup>(cx.listener(
                        move |this, event: &DragMoveEvent<DragGroup>, _w, cx| {
                            this.drag_over_group_row_group(group_id, parent, event, cx);
                        },
                    ))
                    .on_drop::<DragHost>(cx.listener(
                        move |this, dragged: &DragHost, window, cx| {
                            cx.stop_propagation();
                            this.drop_host_on_group(dragged, Some(group_id), window, cx);
                        },
                    ))
                    .on_drop::<DragGroup>(cx.listener(
                        move |this, dragged: &DragGroup, window, cx| {
                            cx.stop_propagation();
                            this.drop_group_on_group(dragged, group_id, window, cx);
                        },
                    ));
                el.context_menu(move |menu, _window, _cx| group_menu(menu, &spec, &menu_entity))
                    .into_any_element()
            }
            TreeRow::Ungrouped { expanded, count } => {
                let sel = GroupSel::Ungrouped;
                let active = self.tree.selected_group == sel;
                let hot = matches!(self.drop_hint, Some(DropHint::IntoGroup(None)));
                let menu_entity = entity.clone();
                let toggle_entity = entity.clone();
                let el = tree_line(("tree-row", at), 0.0, active, tokens)
                    .when(hot, |d| {
                        d.bg(tokens.accent_soft)
                            .border_1()
                            .border_color(tokens.accent)
                    })
                    .child(
                        div()
                            .id(("tree-toggle", at))
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(16.0))
                            .h(px(16.0))
                            .flex_none()
                            .text_color(tokens.text_muted)
                            .when(!searching, |d| {
                                d.hover(|style| style.bg(tokens.hover))
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        toggle_entity.update(cx, |this, cx| {
                                            this.toggle_group(
                                                host_tree::UNGROUPED_KEY.to_owned(),
                                                cx,
                                            );
                                        });
                                    })
                            })
                            .when(!searching, |d| {
                                d.child(
                                    Icon::new(if *expanded {
                                        IconName::ChevronDown
                                    } else {
                                        IconName::ChevronRight
                                    })
                                    .size_3p5(),
                                )
                            }),
                    )
                    .child(
                        Icon::new(if *expanded {
                            IconName::FolderOpen
                        } else {
                            IconName::Folder
                        })
                        .size_3p5()
                        .text_color(tokens.text_muted),
                    )
                    .child(match term {
                        Some(term) => highlighted(host_tree::UNGROUPED_LABEL, term, tokens),
                        None => div()
                            .truncate()
                            .child(host_tree::UNGROUPED_LABEL)
                            .into_any_element(),
                    })
                    .child(div().flex_1())
                    .child(count_label(*count, tokens))
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| this.click_group(sel, cx));
                    })
                    .on_drag_move::<DragHost>(cx.listener(
                        |this, event: &DragMoveEvent<DragHost>, _w, cx| {
                            this.drag_over_group_row_host(None, event, cx);
                        },
                    ))
                    .on_drop::<DragHost>(cx.listener(|this, dragged: &DragHost, window, cx| {
                        cx.stop_propagation();
                        this.drop_host_on_group(dragged, None, window, cx);
                    }));
                el.context_menu(move |menu, _window, _cx| ungrouped_menu(menu, &menu_entity))
                    .into_any_element()
            }
            TreeRow::Divider => div()
                .mx_1p5()
                .my_1()
                .h(px(1.0))
                .bg(tokens.border)
                .into_any_element(),
            TreeRow::Host { id, depth } => {
                let Some(host) = self.store.host(*id) else {
                    return div().into_any_element();
                };
                let host_id = *id;
                let selected = self.tree.selected_host == Some(host_id);
                let cursor_here = searching && cursor == Some(host_id);
                let hint = term.and_then(|term| match_hint(host, term));
                let menu = HostMenuSpec::new(
                    self,
                    host_id,
                    if searching {
                        super::page_logic::OrderState::Search
                    } else {
                        super::page_logic::OrderState::Manual
                    },
                );
                let menu_entity = entity.clone();
                let connect = entity.clone();
                let drag = DragHost {
                    id: host_id,
                    name: host.name.clone(),
                };
                let chip_drag = drag.clone();
                let el = tree_line(
                    ("tree-row", at),
                    *depth as f32 * INDENT,
                    selected || cursor_here,
                    tokens,
                )
                .relative()
                .children(self.tree_insert_line(host_id, tokens))
                .child(div().w(px(16.0)).flex_none())
                .child(
                    Icon::new(IconName::SquareTerminal)
                        .size_3p5()
                        .text_color(tokens.accent),
                )
                .child(
                    div().flex_1().min_w_0().child(match term {
                        Some(term) => highlighted(&host.name, term, tokens),
                        None => div()
                            .truncate()
                            .child(SharedString::from(host.name.clone()))
                            .into_any_element(),
                    }),
                )
                .children(hint.map(|hint| {
                    div()
                        .max_w(px(96.0))
                        .flex_none()
                        .truncate()
                        .text_xs()
                        .text_color(tokens.text_muted)
                        .child(SharedString::from(hint))
                }))
                .on_click(move |event, window, cx| {
                    if event.click_count() >= 2 {
                        // 双击会先后来两次单击：先取消待执行的「再点收起」，免得属性面板来回闪。
                        connect.update(cx, |this, cx| {
                            this.cancel_info_collapse();
                            this.open_host(host_id, window, cx);
                        });
                    } else {
                        connect.update(cx, |this, cx| this.tree_click_host(host_id, 1, cx));
                    }
                })
                .on_drag(drag, move |_, _offset, _window, cx| {
                    let item = DragItem::Host(chip_drag.clone());
                    let weak = weak.clone();
                    cx.new(|_| DragChip::new(weak, item))
                })
                .on_drag_move::<DragHost>(cx.listener(
                    move |this, event: &DragMoveEvent<DragHost>, _w, cx| {
                        this.drag_over_host_row(host_id, Scope::Tree, false, event, cx);
                    },
                ))
                .on_drop::<DragHost>(cx.listener(
                    move |this, dragged: &DragHost, window, cx| {
                        cx.stop_propagation();
                        this.drop_on_host_row(dragged, host_id, Scope::Tree, window, cx);
                    },
                ));
                el.context_menu(move |m, window, cx| host_menu(m, &menu, &menu_entity, window, cx))
                    .into_any_element()
            }
        }
    }

    fn tree_insert_line(&self, host_id: i64, tokens: &Tokens) -> Option<AnyElement> {
        match self.drop_hint {
            Some(DropHint::InsertHost {
                id,
                after,
                scope: Scope::Tree,
            }) if id == host_id => Some(line(after, tokens)),
            _ => None,
        }
    }

    fn group_insert_line(&self, group_id: i64, tokens: &Tokens) -> Option<AnyElement> {
        match self.drop_hint {
            Some(DropHint::InsertGroup { id, after }) if id == group_id => {
                Some(line(after, tokens))
            }
            _ => None,
        }
    }
}

/// 一条细的水平插入线，画在带 `relative` 的行里面。
fn line(after: bool, tokens: &Tokens) -> AnyElement {
    let line = div()
        .absolute()
        .left(px(4.0))
        .right(px(4.0))
        .h(px(2.0))
        .rounded_full()
        .bg(tokens.accent);
    if after { line.bottom_0() } else { line.top_0() }.into_any_element()
}

/// 树里的一行：缩进、悬停和选中底色。
fn tree_line(
    id: (&'static str, usize),
    indent: f32,
    active: bool,
    tokens: &Tokens,
) -> Stateful<Div> {
    let hover = tokens.hover;
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap_1p5()
        .h(px(ROW_HEIGHT))
        .w_full()
        .pl(px(6.0 + indent))
        .pr_2()
        .rounded_md()
        .text_sm()
        .cursor_pointer()
        .when(active, |d| d.bg(tokens.accent_soft))
        .when(!active, |d| d.hover(move |style| style.bg(hover)))
}

fn count_label(count: usize, tokens: &Tokens) -> impl IntoElement {
    div()
        .flex_none()
        .text_xs()
        .text_color(tokens.text_muted)
        .child(count.to_string())
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{match_hint, match_ranges};
    use std::ops::Range;
    use zssh_core::model::{AuthMethod, Host, ProxyMode};

    fn host(name: &str, address: &str, user: &str, tags: &[&str]) -> Host {
        Host {
            id: 1,
            group_id: None,
            name: name.to_owned(),
            host: address.to_owned(),
            port: 22,
            username: user.to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: None,
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: ProxyMode::Default,
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            sort: 0,
            legacy_algorithms: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn every_occurrence_of_the_term_is_marked_case_insensitively() {
        assert_eq!(match_ranges("Web-web", "web"), vec![0..3, 4..7]);
        assert_eq!(match_ranges("alpha", "x"), Vec::<Range<usize>>::new());
        assert!(match_ranges("alpha", "").is_empty());
        assert_eq!(
            match_ranges("测试 web", "web"),
            vec![7..10],
            "按字节范围，中文在前也对得上"
        );
    }

    #[test]
    fn a_match_hint_explains_why_a_host_with_a_different_name_is_listed() {
        let target = host("db", "192.0.2.9", "postgres", &["blue"]);
        assert_eq!(match_hint(&target, "db"), None, "名字匹配不需要解释");
        assert_eq!(match_hint(&target, "192.0").as_deref(), Some("192.0.2.9"));
        assert_eq!(match_hint(&target, "postg").as_deref(), Some("postgres"));
        assert_eq!(match_hint(&target, "blu").as_deref(), Some("blue"));
        assert_eq!(match_hint(&target, "zzz"), None);
    }
}
