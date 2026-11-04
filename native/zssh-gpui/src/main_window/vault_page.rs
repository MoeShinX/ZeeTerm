//! 主机页（0.2.14 `VaultView.tsx` 的右半边）：顶部搜索 + 一排纯图标（视图 / 疏密 / 排序），下面是
//! 主机卡片（网格）或列表；搜索框里写 `user@host:port` 回车直接连；空库有引导页。
//!
//! 单击选中（左边的树跟着高亮并展开属性面板），双击连接，右键出菜单，悬停显示「编辑」「连接」；
//! 手动排序且没在搜索时可以拖动调整顺序（同组内和跨组）。

use ::gpui::{KeyDownEvent, SharedString, WeakEntity, px};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::model::{Host, Protocol};

use super::drag::{DragHost, DragItem, DropHint, Scope};
use super::drag_scroll::ScrollArea;
use super::page_logic::{
    Density, OrderState, PageOptions, SortMode, ViewMode, page_hosts, ungrouped_label,
};
use super::protocol_form;
use super::vault_menus::{HostMenuSpec, host_menu};
use super::widgets::{BadgeKind, act, badge, protocol_icon};
use super::{GroupSel, MainWindow};
use crate::app_theme::Tokens;
use crate::os_icons::{self, OsIconSize};
use crate::typography::{Role, Typo as _};

/// 网格里一张卡片的最小宽度和间距（紧凑 / 舒适，同 0.2.14 的 `minmax(210px | 240px, 1fr)`）。
fn grid_metrics(density: Density) -> (f32, f32) {
    match density {
        Density::Compact => (210.0, 10.0),
        Density::Comfortable => (240.0, 12.0),
    }
}

/// 一排放几张、每张多宽（撑满一行，同 CSS grid 的 `auto-fill` + `1fr`）。
pub fn grid_layout(available: f32, density: Density) -> (usize, f32) {
    let (min, gap) = grid_metrics(density);
    let available = available.max(min);
    let columns = (((available + gap) / (min + gap)).floor() as usize).max(1);
    let width = (available - gap * (columns as f32 - 1.0)) / columns as f32;
    (columns, width)
}

impl MainWindow {
    pub(super) fn render_vault(
        &mut self,
        tokens: &Tokens,
        page_width: f32,
        _window: &mut ::gpui::Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.clear_stale_hint(cx);
        let entity = cx.entity();
        let layout = self.store.layout().clone();
        let query = self.vault_query.read(cx).value().to_string();
        let flat = self.store.flat();
        let shown: Vec<Host> = page_hosts(
            self.store.groups(),
            self.store.hosts(),
            &PageOptions {
                query: &query,
                selected: self.tree.selected_group,
                mode: layout.sort_mode,
                recent: &layout.recent,
                flat,
            },
        )
        .into_iter()
        .cloned()
        .collect();
        let order = self.page_order(cx);
        let has_hosts = !self.store.hosts().is_empty();
        let title = match self.tree.selected_group {
            GroupSel::All => "全部主机".to_owned(),
            GroupSel::Ungrouped if flat => "全部主机".to_owned(),
            GroupSel::Ungrouped => ungrouped_label(false).to_owned(),
            GroupSel::Group(id) => self
                .store
                .groups()
                .iter()
                .find(|g| g.id == id)
                .map_or_else(|| "分组".to_owned(), |g| g.name.clone()),
        };

        let toggle_group = |left: AnyElement, right: AnyElement| -> AnyElement {
            h_flex()
                .gap_0p5()
                .p_0p5()
                .rounded_md()
                .border_1()
                .border_color(tokens.border)
                .child(left)
                .child(right)
                .into_any_element()
        };
        let mode_button =
            |id: &'static str, icon: Icon, tip: &'static str, active: bool, mode: ViewMode| {
                let el = div()
                    .id(id)
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(28.0))
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(if active {
                        tokens.text
                    } else {
                        tokens.text_muted
                    })
                    .when(active, |d| d.bg(tokens.accent_soft))
                    .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
                    .child(Icon::new(icon).size_4())
                    .on_click({
                        let entity = entity.clone();
                        move |_, window, cx| {
                            entity.update(cx, |this, cx| this.set_view_mode(mode, window, cx));
                        }
                    });
                el.into_any_element()
            };
        let density_button = {
            let compact = layout.density == Density::Compact;
            div()
                .id("vault-density")
                .flex()
                .items_center()
                .justify_center()
                .size(px(28.0))
                .rounded_md()
                .cursor_pointer()
                .bg(if compact {
                    tokens.accent_soft
                } else {
                    tokens.background
                })
                .text_color(if compact {
                    tokens.text
                } else {
                    tokens.text_muted
                })
                .tooltip(move |window, cx| {
                    Tooltip::new(if compact {
                        "紧凑（点击切换到舒适）"
                    } else {
                        "舒适（点击切换到紧凑）"
                    })
                    .build(window, cx)
                })
                .child(
                    Icon::new(if compact {
                        crate::icons::Ico::Rows3
                    } else {
                        crate::icons::Ico::Rows2
                    })
                    .size_4(),
                )
                .on_click(act_click(&entity, |this, window, cx| {
                    this.toggle_density(window, cx)
                }))
                .into_any_element()
        };
        // 排序：纯图标，当前方式写在悬停提示里（点一下按 手动 / 最近打开 / 名称 轮换）。
        let sort_button = icon_only_button(
            "vault-sort",
            Icon::new(crate::icons::Ico::ArrowUpDown),
            layout.sort_mode.tooltip(),
            false,
            tokens,
        )
        .on_click(act_click(&entity, |this, window, cx| {
            this.cycle_sort_mode(window, cx)
        }));

        let header = h_flex()
            .flex_none()
            .flex_wrap()
            .gap_2()
            .items_center()
            .px_4()
            .py_2()
            .bg(tokens.background)
            .border_b_1()
            .border_color(tokens.divider)
            .child(
                div().flex_1().min_w(px(160.0)).max_w(px(448.0)).child(
                    Input::new(&self.vault_query)
                        .prefix(
                            Icon::new(IconName::Search)
                                .size_4()
                                .text_color(tokens.text_muted),
                        )
                        .cleanable(true),
                ),
            )
            .child(
                h_flex()
                    .ml_auto()
                    .min_w_0()
                    .flex_shrink(1.0)
                    .flex_wrap()
                    .gap_1p5()
                    .items_center()
                    .child(toggle_group(
                        mode_button(
                            "vault-grid",
                            Icon::new(crate::icons::Ico::LayoutGrid),
                            "网格视图",
                            layout.view_mode == ViewMode::Grid,
                            ViewMode::Grid,
                        ),
                        mode_button(
                            "vault-list",
                            Icon::new(crate::icons::Ico::List),
                            "列表视图",
                            layout.view_mode == ViewMode::List,
                            ViewMode::List,
                        ),
                    ))
                    .child(
                        h_flex()
                            .p_0p5()
                            .rounded_md()
                            .border_1()
                            .border_color(tokens.border)
                            .child(density_button),
                    )
                    .child(
                        h_flex()
                            .p_0p5()
                            .rounded_md()
                            .border_1()
                            .border_color(tokens.border)
                            .child(sort_button),
                    ),
            );

        // 键盘导航：当前显示的次序和每排几张（列表是 1）；每台主机一个焦点句柄。
        let nav_columns = match layout.view_mode {
            ViewMode::Grid => grid_layout((page_width - 32.0).max(120.0), layout.density).0,
            ViewMode::List => 1,
        };
        self.sync_vault_nav(shown.iter().map(|host| host.id).collect(), nav_columns, cx);

        let body: AnyElement = if shown.is_empty() {
            self.vault_empty(has_hosts, &query, tokens, cx)
                .into_any_element()
        } else {
            let available = (page_width - 32.0).max(120.0);
            match layout.view_mode {
                ViewMode::Grid => {
                    let (columns, width) = grid_layout(available, layout.density);
                    let _ = columns;
                    let (_, gap) = grid_metrics(layout.density);
                    h_flex()
                        .w_full()
                        .flex_wrap()
                        .items_stretch()
                        .gap(px(gap))
                        .children(shown.iter().map(|host| {
                            self.host_card(host, width, layout.density, order, tokens, cx)
                        }))
                        .into_any_element()
                }
                ViewMode::List => {
                    let last = shown.len().saturating_sub(1);
                    v_flex()
                        .w_full()
                        .overflow_hidden()
                        .rounded_lg()
                        .border_1()
                        .border_color(tokens.border)
                        .children(shown.iter().enumerate().map(|(at, host)| {
                            self.host_row(
                                host,
                                at == last,
                                (layout.density, page_width < 640.0),
                                order,
                                tokens,
                                cx,
                            )
                        }))
                        .into_any_element()
                }
            }
        };

        let count = shown.len();
        let _ = SortMode::Manual;
        v_flex()
            .id("vault-page")
            .size_full()
            .min_w_0()
            .child(header)
            .child(
                div()
                    .id("vault-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.drag_scroll.page)
                    .on_drag_move::<DragHost>(cx.listener(
                        |this, event: &::gpui::DragMoveEvent<DragHost>, window, cx| {
                            this.drag_scroll_probe(
                                ScrollArea::Page,
                                event.event.position,
                                window,
                                cx,
                            );
                        },
                    ))
                    .p_4()
                    .child(
                        v_flex()
                            .w_full()
                            .gap_3()
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .typo(Role::GroupTitle, tokens)
                                            .child(SharedString::from(title)),
                                    )
                                    .child(
                                        div()
                                            .typo(Role::Caption, tokens)
                                            .child(SharedString::from(format!("{count} 台"))),
                                    ),
                            )
                            .child(body),
                    ),
            )
    }

    /// 空库的引导页 / 搜不到时的提示。
    fn vault_empty(
        &self,
        has_hosts: bool,
        query: &str,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entity = cx.entity();
        if let Some(error) = self.store.error() {
            return div()
                .py_10()
                .text_center()
                .typo(Role::Body, tokens)
                .text_color(tokens.danger)
                .child(SharedString::from(error.to_owned()))
                .into_any_element();
        }
        if has_hosts {
            return v_flex()
                .py_16()
                .gap_2()
                .items_center()
                .text_color(tokens.text_muted)
                .child(Icon::new(IconName::Search).size_8())
                .child(
                    div()
                        .typo(Role::Body, tokens)
                        .text_color(tokens.text_muted)
                        .child(SharedString::from(format!("没有匹配「{query}」的主机"))),
                )
                .into_any_element();
        }
        v_flex()
            .py_10()
            .items_center()
            .child(
                v_flex()
                    .max_w(px(448.0))
                    .gap_4()
                    .items_center()
                    .px_8()
                    .py_10()
                    .rounded_xl()
                    .border_1()
                    .border_color(tokens.border)
                    .bg(tokens.surface)
                    .child(
                        div()
                            .p_4()
                            .rounded_full()
                            .bg(tokens.accent_soft)
                            .text_color(tokens.accent)
                            .child(Icon::new(crate::icons::Ico::Server).size_8()),
                    )
                    .child(
                        v_flex()
                            .items_center()
                            .gap_1()
                            .child(div().typo(Role::GroupTitle, tokens).child("还没有主机"))
                            .child(
                                div()
                                    .typo(Role::Body, tokens)
                                    .text_color(tokens.text_muted)
                                    .child("添加一台主机，之后双击就能连接。"),
                            ),
                    )
                    .child(
                        Button::new("vault-empty-new")
                            .primary()
                            .icon(IconName::Plus)
                            .label("新建主机")
                            .on_click(act(&entity, |this, window, cx| this.new_host(window, cx))),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_center()
                            .text_color(tokens.text_muted)
                            .child("也可以直接在搜索框里输入 user@host:port 回车，不保存直接连。"),
                    ),
            )
            .into_any_element()
    }

    /// 卡片 / 行上的「编辑」「连接」按钮；点击不冒泡成卡片的单击 / 双击。
    fn card_buttons(
        &self,
        host: &Host,
        compact: bool,
        with_label: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entity = cx.entity();
        let id = host.id;
        h_flex()
            .flex_none()
            .gap(px(if compact { 6.0 } else { 8.0 }))
            .items_center()
            .child(
                Button::new(("host-edit", id as usize))
                    .compact()
                    .ghost()
                    .icon(crate::icons::Ico::Pencil)
                    .tooltip("属性…")
                    .on_click({
                        let entity = entity.clone();
                        move |_, window, cx| {
                            cx.stop_propagation();
                            entity.update(cx, |this, cx| this.edit_host(id, window, cx));
                        }
                    }),
            )
            .child({
                let button = Button::new(("host-connect", id as usize))
                    .compact()
                    .secondary()
                    .icon(IconName::Play)
                    .tooltip("连接");
                let button = if with_label {
                    button.label("连接")
                } else {
                    button
                };
                button.on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    entity.update(cx, |this, cx| this.open_host(id, window, cx));
                })
            })
    }

    fn host_drag_wiring<E: StatefulInteractiveElement + InteractiveElement>(
        &self,
        el: E,
        host: &Host,
        horizontal: bool,
        cx: &mut Context<Self>,
    ) -> E {
        let id = host.id;
        let drag = DragHost {
            id,
            name: host.name.clone(),
        };
        let weak: WeakEntity<MainWindow> = cx.entity().downgrade();
        let chip_drag = drag.clone();
        el.on_drag(drag, move |_, _offset, _window, cx| {
            let item = DragItem::Host(chip_drag.clone());
            let weak = weak.clone();
            cx.new(|_| super::drag::DragChip::new(weak, item))
        })
        .on_drag_move::<DragHost>(cx.listener(move |this, event, _window, cx| {
            this.drag_over_host_row(id, Scope::Page, horizontal, event, cx);
        }))
        .on_drop::<DragHost>(cx.listener(move |this, dragged: &DragHost, window, cx| {
            this.drop_on_host_row(dragged, id, Scope::Page, window, cx);
        }))
    }

    /// 插入线：前面 / 后面。
    fn insert_line(&self, host_id: i64, vertical: bool, tokens: &Tokens) -> Option<AnyElement> {
        let Some(DropHint::InsertHost {
            id,
            after,
            scope: Scope::Page,
        }) = self.drop_hint
        else {
            return None;
        };
        if id != host_id {
            return None;
        }
        let line = div().absolute().bg(tokens.accent).rounded_full();
        Some(
            if vertical {
                let line = line.top(px(4.0)).bottom(px(4.0)).w(px(2.0));
                if after { line.right_0() } else { line.left_0() }
            } else {
                let line = line.left(px(4.0)).right(px(4.0)).h(px(2.0));
                if after { line.bottom_0() } else { line.top_0() }
            }
            .into_any_element(),
        )
    }

    fn host_card(
        &self,
        host: &Host,
        width: f32,
        density: Density,
        order: OrderState,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let id = host.id;
        let compact = density == Density::Compact;
        let selected = self.tree.selected_host == Some(id);
        let group_name = host
            .group_id
            .and_then(|gid| self.store.groups().iter().find(|g| g.id == gid))
            .map(|g| g.name.clone());
        // 紧凑卡片没有别的徽标时只靠图标分辨 SSH；Telnet / 串口主机一定带一个协议标签。
        let has_badges = group_name.is_some()
            || !host.tags.is_empty()
            || host.legacy_algorithms
            || host.protocol.protocol != Protocol::Ssh;
        let hover = tokens.subtle;
        let spec = HostMenuSpec::new(self, id, order);
        let menu_entity = entity.clone();
        let detail: SharedString = protocol_form::host_detail(host).into();
        let badges = h_flex()
            .flex_1()
            .min_w_0()
            .flex_wrap()
            .gap_1()
            .items_center()
            .child(badge(
                protocol_form::protocol_tag(host.protocol.protocol),
                BadgeKind::Outline,
                tokens,
            ))
            .children(group_name.map(|name| badge(name, BadgeKind::Neutral, tokens)))
            .children(
                host.tags
                    .iter()
                    .map(|tag| badge(tag.clone(), BadgeKind::Outline, tokens)),
            )
            .when(host.legacy_algorithms, |d| {
                d.child(badge("旧算法", BadgeKind::Outline, tokens))
            });
        let top = h_flex()
            .gap_2p5()
            .items_center()
            .child(
                // 连上过、探测出系统的 SSH 主机：发行版图标；否则是协议图标。
                match self
                    .store
                    .os_icon_of(host)
                    .and_then(|os| os_icons::badge(os, card_icon_size(compact)))
                {
                    Some(badge) => badge,
                    None => div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(if compact { 28.0 } else { 36.0 }))
                        .flex_none()
                        .rounded_md()
                        .bg(if selected {
                            tokens.accent_soft
                        } else {
                            tokens.subtle
                        })
                        .child(protocol_icon(host.protocol.protocol).size_4().text_color(
                            if selected {
                                tokens.accent
                            } else {
                                tokens.text_muted
                            },
                        ))
                        .into_any_element(),
                },
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .truncate()
                            .typo(Role::ItemName, tokens)
                            .child(SharedString::from(host.name.clone())),
                    )
                    .child(
                        div()
                            .truncate()
                            .typo(Role::Caption, tokens)
                            .font_family("Consolas")
                            .child(detail),
                    ),
            )
            .when(compact && !has_badges, |d| {
                d.child(self.card_buttons(host, compact, false, cx))
            });
        let el = v_flex()
            .id(("host-card", id as usize))
            .relative()
            .w(px(width))
            .flex_none()
            .p(px(if compact { 10.0 } else { 16.0 }))
            .gap_2()
            .rounded_lg()
            .border_1()
            .border_color(if selected {
                tokens.accent
            } else {
                tokens.border
            })
            .bg(tokens.surface)
            .cursor_pointer()
            .when(!selected, |d| d.hover(move |style| style.bg(hover)))
            // 键盘：Tab / 方向键聚焦，聚焦时有焦点框；Enter 连接、F2 编辑（见 `vault_keys`）。
            .when_some(self.host_focus_handle(id), |d, handle| {
                d.track_focus(&handle)
            })
            .focus_visible(|style| style.border_2().border_color(tokens.accent))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if this.host_key_down(id, event, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .children(self.insert_line(id, true, tokens))
            .when(selected, |d| {
                d.child(
                    div()
                        .absolute()
                        .left_0()
                        .top(px(12.0))
                        .bottom(px(12.0))
                        .w(px(2.0))
                        .rounded_full()
                        .bg(tokens.accent),
                )
            })
            .child(top)
            .when(!compact || has_badges, |d| {
                d.child(
                    h_flex()
                        .items_end()
                        .gap_2()
                        .pt(px(if compact { 0.0 } else { 4.0 }))
                        .child(badges)
                        .child(self.card_buttons(host, compact, false, cx)),
                )
            })
            .on_click({
                let entity = entity.clone();
                move |event, window, cx| {
                    if event.click_count() >= 2 {
                        entity.update(cx, |this, cx| this.open_host(id, window, cx));
                    } else {
                        entity.update(cx, |this, cx| this.page_click_host(id, cx));
                    }
                }
            });
        let el = self.host_drag_wiring(el, host, true, cx);
        el.context_menu(move |menu, window, cx| host_menu(menu, &spec, &menu_entity, window, cx))
            .into_any_element()
    }

    fn host_row(
        &self,
        host: &Host,
        last: bool,
        layout: (Density, bool),
        order: OrderState,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let id = host.id;
        let (density, narrow) = layout;
        let compact = density == Density::Compact;
        let selected = self.tree.selected_host == Some(id);
        let group_label = host
            .group_id
            .and_then(|gid| self.store.groups().iter().find(|g| g.id == gid))
            .map_or_else(
                || ungrouped_label(self.store.flat()).to_owned(),
                |g| g.name.clone(),
            );
        let hover = tokens.hover;
        let spec = HostMenuSpec::new(self, id, order);
        let menu_entity = entity.clone();
        let el = h_flex()
            .id(("host-row", id as usize))
            .relative()
            .w_full()
            .gap_3()
            .items_center()
            .px_4()
            .py(px(if compact { 4.0 } else { 10.0 }))
            .typo(Role::Body, tokens)
            .cursor_pointer()
            .when(selected, |d| d.bg(tokens.accent_soft))
            .when(!selected, |d| d.hover(move |style| style.bg(hover)))
            .when(!last, |d| d.border_b_1().border_color(tokens.border))
            .when_some(self.host_focus_handle(id), |d, handle| {
                d.track_focus(&handle)
            })
            .focus_visible(|style| {
                style
                    .bg(tokens.accent_soft)
                    .border_l_2()
                    .border_color(tokens.accent)
            })
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if this.host_key_down(id, event, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .children(self.insert_line(id, false, tokens))
            .child(
                match self
                    .store
                    .os_icon_of(host)
                    .and_then(|os| os_icons::badge(os, OsIconSize::Row))
                {
                    Some(badge) => badge,
                    None => protocol_icon(host.protocol.protocol)
                        .size_4()
                        .text_color(if selected {
                            tokens.accent
                        } else {
                            tokens.text_muted
                        })
                        .into_any_element(),
                },
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .typo(Role::ItemName, tokens)
                    .child(SharedString::from(host.name.clone())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .typo(Role::Caption, tokens)
                    .font_family("Consolas")
                    .child(SharedString::from(protocol_form::host_detail(host))),
            )
            .child(div().w(px(56.0)).flex_none().child(badge(
                protocol_form::protocol_tag(host.protocol.protocol),
                BadgeKind::Outline,
                tokens,
            )))
            .when(!narrow, |d| {
                d.child(
                    div()
                        .w(px(120.0))
                        .flex_none()
                        .truncate()
                        .typo(Role::Caption, tokens)
                        .child(SharedString::from(group_label)),
                )
            })
            .child(self.card_buttons(host, compact, !narrow, cx))
            .on_click({
                let entity = entity.clone();
                move |event, window, cx| {
                    if event.click_count() >= 2 {
                        entity.update(cx, |this, cx| this.open_host(id, window, cx));
                    } else {
                        entity.update(cx, |this, cx| this.page_click_host(id, cx));
                    }
                }
            });
        let el = self.host_drag_wiring(el, host, false, cx);
        el.context_menu(move |menu, window, cx| host_menu(menu, &spec, &menu_entity, window, cx))
            .into_any_element()
    }
}

/// 主机页右上角一排图标里的一个：28px 方块，悬停有中文提示（带当前状态的提示由调用方拼好）。
fn icon_only_button(
    id: &'static str,
    icon: Icon,
    tooltip: String,
    active: bool,
    tokens: &Tokens,
) -> ::gpui::Stateful<Div> {
    let tooltip: SharedString = tooltip.into();
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(28.0))
        .rounded_md()
        .cursor_pointer()
        .text_color(if active {
            tokens.text
        } else {
            tokens.text_muted
        })
        .when(active, |d| d.bg(tokens.accent_soft))
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        .child(icon.size_4())
}

/// 点击回调（不带 `&mut Context` 的外壳，给 `div().on_click` 用）。
fn act_click(
    entity: &Entity<MainWindow>,
    f: impl Fn(&mut MainWindow, &mut Window, &mut Context<MainWindow>) + 'static,
) -> impl Fn(&::gpui::ClickEvent, &mut Window, &mut App) + 'static {
    let entity = entity.clone();
    move |_, window, cx| {
        entity.update(cx, |this, cx| f(this, window, cx));
    }
}

/// 主机页卡片上发行版图标的尺寸：和卡片左上角那个 36px（紧凑 28px）的图标位一样大。
fn card_icon_size(compact: bool) -> OsIconSize {
    if compact {
        OsIconSize::CardCompact
    } else {
        OsIconSize::Card
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{Density, grid_layout};

    #[test]
    fn the_grid_fills_the_row_like_css_auto_fill() {
        // 紧凑：最小 210、间距 10。宽 1000 放 4 张：(1000 - 30) / 4 = 242.5。
        let (columns, width) = grid_layout(1000.0, Density::Compact);
        assert_eq!(columns, 4);
        assert!((width - 242.5).abs() < 0.01, "{width}");
        // 舒适：最小 240、间距 12。宽 1000 放 4 张：(1000 - 36) / 4 = 241。
        let (columns, width) = grid_layout(1000.0, Density::Comfortable);
        assert_eq!(columns, 4);
        assert!((width - 241.0).abs() < 0.01, "{width}");
    }

    #[test]
    fn a_narrow_page_still_shows_one_card_not_zero() {
        assert_eq!(grid_layout(100.0, Density::Compact).0, 1);
        assert_eq!(grid_layout(0.0, Density::Comfortable).0, 1);
        let (_, width) = grid_layout(100.0, Density::Compact);
        assert!(width >= 210.0, "太窄时卡片保持最小宽度");
    }

    #[test]
    fn a_row_that_just_fits_two_cards_does_not_squeeze_in_a_third() {
        let (columns, _) = grid_layout(430.0, Density::Compact);
        assert_eq!(columns, 2, "2 * 210 + 10 = 430 正好两张");
        assert_eq!(grid_layout(429.0, Density::Compact).0, 1);
    }
}
