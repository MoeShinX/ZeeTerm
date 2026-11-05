//! 主机树 / 主机页里的拖放：拖主机换分组、拖主机 / 分组调整上下顺序（0.2.14 `hostDrag.tsx`）。
//!
//! 用 gpui 自带的拖放（`on_drag` / `on_drag_move` / `on_drop`），规则和 0.2.14 一样：
//! - 换分组：落点是分组行（那个分组）、「未分组」行（顶层）、树里的主机行（它所在的分组）；
//! - 调整顺序：指针在主机行的上半（卡片网格看左半 / 右半）= 插到它前面，下半 = 插到后面，画一条
//!   插入线，松手整层重新编号（目标在别的分组就同时换过去）；分组只能在同一个父分组下调整；
//! - 平铺模式（「未分组」改过名）：树的空白处 = 移到顶层；
//! - 搜索框里有字时树里的排序拖动不响应（换分组照旧）；主机页右边只有「手动」排序且没在搜索时才能
//!   调整顺序，不是手动排序时松手提示一次「切换到「手动」排序后才能拖动」。
//!
//! 松手后的落库走 `page_logic::plan_host_drop` / `plan_group_drop`（有测试）和 `HostStore`。

use ::gpui::{DragMoveEvent, SharedString, WeakEntity};
use gpui_kit::component::*;
use gpui_kit::*;

use super::MainWindow;
use super::page_logic::{
    OrderState, order_state, plan_group_drop, plan_host_drop, ungrouped_label,
};
use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};

/// 本文件（拖放标签、拖放后的提示）的界面文案。主机名、分组名是用户数据，拼在文案旁边。
#[derive(Clone, Copy, Debug)]
enum DragMsg {
    /// 分组行的拖动标签：名字 + 本键。
    ReorderSuffix,
    /// 分组只能在同一层里调整：名字 + 本键。
    SameLevelSuffix,
    /// 主机拖到分组上：名字 + 本键。
    DropOnGroupSuffix,
    /// 找不到分组时的代称。
    GroupFallback,
    QuoteOpen,
    QuoteClose,
    SwitchToManualHint,
    ReorderFailedPrefix,
    MoveFailedPrefix,
    /// 「『主机名』已移到」+ 目标分组。
    MovedPrefix,
    MovedMid,
}

impl Msg for DragMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::ReorderSuffix => "（调整顺序）",
            Self::SameLevelSuffix => "（在同一层拖动调整顺序）",
            Self::DropOnGroupSuffix => "（拖到左边的分组上）",
            Self::GroupFallback => "分组",
            Self::QuoteOpen => "「",
            Self::QuoteClose => "」",
            Self::SwitchToManualHint => "切换到「手动」排序后才能拖动",
            Self::ReorderFailedPrefix => "调整顺序失败：",
            Self::MoveFailedPrefix => "移动失败：",
            Self::MovedPrefix => "「",
            Self::MovedMid => "」已移到",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::ReorderSuffix => " (reorder)",
            Self::SameLevelSuffix => " (drag within the same level to reorder)",
            Self::DropOnGroupSuffix => " (drop onto a group on the left)",
            Self::GroupFallback => "Group",
            Self::QuoteOpen => "“",
            Self::QuoteClose => "”",
            Self::SwitchToManualHint => "Switch the sort mode to \"Manual\" before dragging",
            Self::ReorderFailedPrefix => "Failed to reorder: ",
            Self::MoveFailedPrefix => "Failed to move: ",
            Self::MovedPrefix => "“",
            Self::MovedMid => "” was moved to ",
        }
    }
}

/// 正在拖的主机。
#[derive(Clone, Debug)]
pub struct DragHost {
    pub id: i64,
    pub name: String,
}

/// 正在拖的分组。
#[derive(Clone, Debug)]
pub struct DragGroup {
    pub id: i64,
    pub parent: Option<i64>,
    pub name: String,
}

/// 插入线 / 高亮画在哪边的元素上：左边的树、右边的卡片 / 行。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Tree,
    Page,
}

/// 指针现在指着哪里，松手会发生什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropHint {
    /// 插到这台主机的前面 / 后面。
    InsertHost { id: i64, after: bool, scope: Scope },
    /// 插到这个分组的前面 / 后面（只在同一层）。
    InsertGroup { id: i64, after: bool },
    /// 移到这个分组（`None` = 未分组 / 顶层）。
    IntoGroup(Option<i64>),
    /// 平铺模式下树的空白处：移到顶层。
    IntoRoot,
}

/// 跟着指针走的小标签。
pub struct DragChip {
    main: WeakEntity<MainWindow>,
    item: DragItem,
}

#[derive(Clone, Debug)]
pub enum DragItem {
    Host(DragHost),
    Group(DragGroup),
}

impl DragChip {
    pub fn new(main: WeakEntity<MainWindow>, item: DragItem) -> Self {
        Self { main, item }
    }
}

impl Render for DragChip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = Tokens::current(cx);
        let text = self
            .main
            .read_with(cx, |main, _| main.drag_label(&self.item))
            .unwrap_or_default();
        h_flex()
            .gap_1p5()
            .items_center()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(tokens.accent)
            .bg(tokens.background)
            .shadow_lg()
            .text_xs()
            .text_color(tokens.text)
            .child(match self.item {
                DragItem::Host(_) => Icon::new(crate::icons::Ico::Server)
                    .size_3()
                    .text_color(tokens.text_muted)
                    .into_any_element(),
                DragItem::Group(_) => crate::icons::folder_icon(false, 12.0).into_any_element(),
            })
            .child(SharedString::from(text))
    }
}

impl MainWindow {
    /// 拖动标签上的文字：拖的是哪个、松手会放到哪。
    pub(crate) fn drag_label(&self, item: &DragItem) -> String {
        let flat = self.store.flat();
        let root = ungrouped_label(flat);
        let group_name = |id: Option<i64>| -> Option<String> {
            match id {
                None => Some(root.to_owned()),
                Some(id) => self
                    .store
                    .groups()
                    .iter()
                    .find(|g| g.id == id)
                    .map(|g| g.name.clone()),
            }
        };
        match item {
            DragItem::Group(group) => match self.drop_hint {
                Some(DropHint::InsertGroup { .. }) => {
                    format!("{}{}", group.name, t(DragMsg::ReorderSuffix))
                }
                _ => format!("{}{}", group.name, t(DragMsg::SameLevelSuffix)),
            },
            DragItem::Host(host) => match self.drop_hint {
                Some(DropHint::InsertHost { id, .. }) => {
                    // 插到别的分组里的主机之间：说明会换分组。
                    let target = self.store.host(id);
                    let source = self.store.host(host.id);
                    let moved =
                        matches!((target, source), (Some(t), Some(s)) if t.group_id != s.group_id);
                    if moved {
                        let name = target
                            .and_then(|t| group_name(t.group_id))
                            .unwrap_or_else(|| t(DragMsg::GroupFallback).to_owned());
                        format!("{} → {name}", host.name)
                    } else {
                        format!("{}{}", host.name, t(DragMsg::ReorderSuffix))
                    }
                }
                Some(DropHint::IntoGroup(group)) => match group_name(group) {
                    Some(name) => format!("{} → {name}", host.name),
                    None => format!("{}{}", host.name, t(DragMsg::DropOnGroupSuffix)),
                },
                Some(DropHint::IntoRoot) => format!("{} → {root}", host.name),
                _ => format!("{}{}", host.name, t(DragMsg::DropOnGroupSuffix)),
            },
        }
    }

    /// 树里搜索框有字：排序拖动不响应。
    fn tree_search_active(&self, cx: &App) -> bool {
        !self.tree_query.read(cx).value().trim().is_empty()
    }

    fn page_order_state(&self, cx: &App) -> OrderState {
        order_state(
            self.store.layout().sort_mode,
            self.vault_query.read(cx).value().as_ref(),
        )
    }

    fn set_hint(&mut self, hint: Option<DropHint>, cx: &mut Context<Self>) {
        if self.drop_hint != hint {
            self.drop_hint = hint;
            cx.notify();
        }
    }

    /// 指针经过一台主机的行 / 卡片：更新插入线或换分组高亮。离开这一行时清掉自己留下的提示。
    pub(super) fn drag_over_host_row(
        &mut self,
        target: i64,
        scope: Scope,
        horizontal: bool,
        event: &DragMoveEvent<DragHost>,
        cx: &mut Context<Self>,
    ) {
        let mine = |hint: &DropHint| matches!(hint, DropHint::InsertHost { id, scope: s, .. } if *id == target && *s == scope);
        if !event.bounds.contains(&event.event.position) {
            if self.drop_hint.as_ref().is_some_and(mine) {
                self.set_hint(None, cx);
            }
            return;
        }
        let dragged = event.drag(cx).id;
        if dragged == target {
            self.set_hint(None, cx);
            return;
        }
        let center = event.bounds.center();
        let after = if horizontal {
            event.event.position.x >= center.x
        } else {
            event.event.position.y >= center.y
        };
        let hint = match scope {
            Scope::Page => match self.page_order_state(cx) {
                // 别的分组的主机也可以放：松手时先换分组再按位置排。
                OrderState::Manual => Some(DropHint::InsertHost {
                    id: target,
                    after,
                    scope,
                }),
                OrderState::Mode | OrderState::Search => None,
            },
            Scope::Tree => {
                if self.tree_search_active(cx) {
                    // 搜索时过滤后的列表不连续：不排序，换分组照旧（落到这台所在的分组）。
                    Some(DropHint::IntoGroup(
                        self.store.host(target).and_then(|h| h.group_id),
                    ))
                } else {
                    Some(DropHint::InsertHost {
                        id: target,
                        after,
                        scope,
                    })
                }
            }
        };
        self.set_hint(hint, cx);
    }

    /// 松手在一台主机的行 / 卡片上。
    pub(super) fn drop_on_host_row(
        &mut self,
        dragged: &DragHost,
        target: i64,
        scope: Scope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hint = self.drop_hint.take();
        match hint {
            Some(DropHint::InsertHost { id, after, .. }) if id == target => {
                self.apply_host_drop(dragged.id, target, after, window, cx);
            }
            Some(DropHint::IntoGroup(group)) => {
                self.move_host_ui(dragged.id, group, window, cx);
            }
            _ => {
                // 右边的卡片在不是手动排序时被松手：提示一次（不响应）。
                if scope == Scope::Page && self.page_order_state(cx) == OrderState::Mode {
                    self.show_mode_hint(window, cx);
                }
            }
        }
        cx.notify();
    }

    fn show_mode_hint(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode_hint_shown {
            return;
        }
        self.mode_hint_shown = true;
        self.notice(t(DragMsg::SwitchToManualHint), window, cx);
    }

    /// 把主机放到 `target` 的前面 / 后面（目标在别的分组就同时换过去）。
    pub(crate) fn apply_host_drop(
        &mut self,
        host_id: i64,
        target: i64,
        after: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(plan) = plan_host_drop(self.store.hosts(), host_id, target, after) else {
            return;
        };
        let moved_to = plan.move_to;
        match self.store.apply_host_drop(host_id, &plan) {
            Ok(()) => {
                if let Some(group) = moved_to {
                    self.announce_moved(host_id, group, window, cx);
                }
            }
            Err(err) => self.notice_error(
                &format!("{}{err}", t(DragMsg::ReorderFailedPrefix)),
                window,
                cx,
            ),
        }
        cx.notify();
    }

    /// 移到某个分组并提示（右键菜单和拖放共用）。
    pub(crate) fn move_host_ui(
        &mut self,
        host_id: i64,
        group: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.store.move_host_to_group(host_id, group) {
            Ok(true) => self.announce_moved(host_id, group, window, cx),
            Ok(false) => {}
            Err(err) => self.notice_error(
                &format!("{}{err}", t(DragMsg::MoveFailedPrefix)),
                window,
                cx,
            ),
        }
        cx.notify();
    }

    fn announce_moved(
        &mut self,
        host_id: i64,
        group: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = self
            .store
            .host(host_id)
            .map(|h| h.name.clone())
            .unwrap_or_default();
        let target = match group {
            None => ungrouped_label(self.store.flat()).to_owned(),
            Some(id) => format!(
                "{}{}{}",
                t(DragMsg::QuoteOpen),
                self.store
                    .groups()
                    .iter()
                    .find(|g| g.id == id)
                    .map_or(t(DragMsg::GroupFallback), |g| g.name.as_str()),
                t(DragMsg::QuoteClose)
            ),
        };
        self.notice_success(
            &format!(
                "{}{name}{}{target}",
                t(DragMsg::MovedPrefix),
                t(DragMsg::MovedMid)
            ),
            window,
            cx,
        );
    }

    // ---- 分组行 ----

    /// 指针经过一个分组行（`group` = `None` 是「未分组」行）：拖的是主机 → 换分组高亮。
    pub(super) fn drag_over_group_row_host(
        &mut self,
        group: Option<i64>,
        event: &DragMoveEvent<DragHost>,
        cx: &mut Context<Self>,
    ) {
        let hint = DropHint::IntoGroup(group);
        if event.bounds.contains(&event.event.position) {
            self.set_hint(Some(hint), cx);
        } else if self.drop_hint == Some(hint) {
            self.set_hint(None, cx);
        }
    }

    /// 指针经过一个分组行：拖的是分组 → 同一层里的插入线。
    pub(super) fn drag_over_group_row_group(
        &mut self,
        target: i64,
        parent: Option<i64>,
        event: &DragMoveEvent<DragGroup>,
        cx: &mut Context<Self>,
    ) {
        let mine =
            |hint: &DropHint| matches!(hint, DropHint::InsertGroup { id, .. } if *id == target);
        if !event.bounds.contains(&event.event.position) {
            if self.drop_hint.as_ref().is_some_and(mine) {
                self.set_hint(None, cx);
            }
            return;
        }
        let dragged = event.drag(cx);
        // 分组不能拖进别的层、不能拖到自己上、搜索时不能拖。
        if dragged.id == target || dragged.parent != parent || self.tree_search_active(cx) {
            self.set_hint(None, cx);
            return;
        }
        let after = event.event.position.y >= event.bounds.center().y;
        self.set_hint(Some(DropHint::InsertGroup { id: target, after }), cx);
    }

    pub(super) fn drop_host_on_group(
        &mut self,
        dragged: &DragHost,
        group: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drop_hint = None;
        self.move_host_ui(dragged.id, group, window, cx);
    }

    pub(super) fn drop_group_on_group(
        &mut self,
        dragged: &DragGroup,
        target: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let hint = self.drop_hint.take();
        if let Some(DropHint::InsertGroup { id, after }) = hint
            && id == target
            && let Some(order) = plan_group_drop(self.store.groups(), dragged.id, target, after)
            && let Err(err) = self.store.reorder_groups(&order)
        {
            self.notice_error(
                &format!("{}{err}", t(DragMsg::ReorderFailedPrefix)),
                window,
                cx,
            );
        }
        cx.notify();
    }

    // ---- 平铺模式下树的空白处 ----

    pub(super) fn drag_over_tree_blank(
        &mut self,
        event: &DragMoveEvent<DragHost>,
        cx: &mut Context<Self>,
    ) {
        if !self.store.flat() {
            return;
        }
        if event.bounds.contains(&event.event.position) {
            // 行上的处理（后注册、后执行）会覆盖这个；没有被覆盖说明指针在空白处。
            if !matches!(
                self.drop_hint,
                Some(
                    DropHint::InsertHost {
                        scope: Scope::Tree,
                        ..
                    } | DropHint::IntoGroup(_)
                )
            ) {
                self.set_hint(Some(DropHint::IntoRoot), cx);
            }
        } else if self.drop_hint == Some(DropHint::IntoRoot) {
            self.set_hint(None, cx);
        }
    }

    pub(super) fn drop_host_on_tree_blank(
        &mut self,
        dragged: &DragHost,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.store.flat() {
            return;
        }
        self.drop_hint = None;
        self.move_host_ui(dragged.id, None, window, cx);
    }

    /// 当前不在拖动：把遗留的提示清掉（拖到窗口外松手、按 Esc 取消时没有「落下」事件）。
    pub(super) fn clear_stale_hint(&mut self, cx: &App) {
        if self.drop_hint.is_some() && !cx.has_active_drag() {
            self.drop_hint = None;
        }
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{DropHint, Scope};

    #[test]
    fn hints_compare_by_value_so_unchanged_moves_do_not_repaint() {
        let a = DropHint::InsertHost {
            id: 1,
            after: false,
            scope: Scope::Page,
        };
        assert_eq!(
            a,
            DropHint::InsertHost {
                id: 1,
                after: false,
                scope: Scope::Page
            }
        );
        assert_ne!(
            a,
            DropHint::InsertHost {
                id: 1,
                after: true,
                scope: Scope::Page
            }
        );
        assert_ne!(
            a,
            DropHint::InsertHost {
                id: 1,
                after: false,
                scope: Scope::Tree
            }
        );
        assert_ne!(DropHint::IntoGroup(None), DropHint::IntoRoot);
    }
}
