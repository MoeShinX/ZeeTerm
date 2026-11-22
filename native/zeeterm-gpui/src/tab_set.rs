//! 标签列表的纯模型：谁是活动标签、关掉谁以后谁接着当活动标签。
//!
//! 规则照 0.2.14 前端（`App.tsx` 的 `closeTab` / `closeTabs` / `cycleTab` /
//! `state/sessions.ts` 的 `insertTabAfter`）：
//! - 新标签默认加在最后并成为活动标签；复制标签紧挨在原标签右边；
//! - 关掉活动标签，优先回到它左边那个，没有左边才用右边那个；
//! - 关掉的标签在活动标签左边时，活动标签跟着往前挪一格，指向的仍是同一个标签；
//! - 「关闭其他」只剩被点的那个；「关闭右侧」「关闭左侧」若活动标签被关掉，落到被点的那个；
//! - Ctrl+Tab 循环切换，少于两个标签时不动。
//!
//! 放成不依赖 GPUI 的泛型，是为了让这些规则不用起窗口就能单测。

/// 把 `items[from]` 取出来插到 `to`（拖动换位；文件标签这类不放在 [`TabSet`] 里的列表也用它）。
/// 越界或 `from == to` 什么也不做，返回 `false`。
pub fn move_within<T>(items: &mut Vec<T>, from: usize, to: usize) -> bool {
    if from == to || from >= items.len() || to >= items.len() {
        return false;
    }
    let item = items.remove(from);
    items.insert(to, item);
    true
}

#[derive(Debug)]
pub struct TabSet<T> {
    items: Vec<T>,
    /// 只在 `items` 非空时有意义；空列表时恒为 0。
    active: usize,
}

impl<T> Default for TabSet<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> TabSet<T> {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            active: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn items(&self) -> &[T] {
        &self.items
    }

    pub fn items_mut(&mut self) -> &mut [T] {
        &mut self.items
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        self.items.get(index)
    }

    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        self.items.get_mut(index)
    }

    /// 活动标签的下标；没有标签时是 `None`。
    pub fn active_index(&self) -> Option<usize> {
        (!self.items.is_empty()).then_some(self.active)
    }

    pub fn active(&self) -> Option<&T> {
        self.items.get(self.active)
    }

    pub fn active_mut(&mut self) -> Option<&mut T> {
        self.items.get_mut(self.active)
    }

    /// 切到第 `index` 个；越界或本来就是它返回 `false`。
    pub fn select(&mut self, index: usize) -> bool {
        if index >= self.items.len() || index == self.active {
            return false;
        }
        self.active = index;
        true
    }

    /// 加在最后并设为活动标签，返回它的下标。
    pub fn push(&mut self, item: T) -> usize {
        self.items.push(item);
        self.active = self.items.len() - 1;
        self.active
    }

    /// 紧挨在 `after` 右边插入并设为活动标签；`after` 越界就加在最后。
    pub fn insert_after(&mut self, after: usize, item: T) -> usize {
        if after >= self.items.len() {
            return self.push(item);
        }
        let at = after + 1;
        self.items.insert(at, item);
        self.active = at;
        at
    }

    /// 关掉第 `index` 个，返回被关的标签。
    pub fn remove(&mut self, index: usize) -> Option<T> {
        if index >= self.items.len() {
            return None;
        }
        let removed = self.items.remove(index);
        if self.items.is_empty() {
            self.active = 0;
        } else if index == self.active {
            // 优先左边，没有左边就是顶上来的右边那个（它现在就在 `index`）。
            self.active = if index > 0 { index - 1 } else { 0 };
        } else if index < self.active {
            self.active -= 1;
        }
        Some(removed)
    }

    /// 只留下第 `keep` 个，返回被关掉的（保持原来的顺序）。
    /// 产品里已经改走 [`Self::remove_matching`]（按编号，V2-U44）；这三个按下标的版本留作它的参照规则，只在测试里对照。
    #[cfg(test)]
    pub fn remove_others(&mut self, keep: usize) -> Vec<T> {
        if keep >= self.items.len() {
            return Vec::new();
        }
        let mut kept = None;
        let mut removed = Vec::new();
        for (index, item) in std::mem::take(&mut self.items).into_iter().enumerate() {
            if index == keep {
                kept = Some(item);
            } else {
                removed.push(item);
            }
        }
        self.items.extend(kept);
        self.active = 0;
        removed
    }

    /// 关掉第 `index` 个右边的所有标签，返回被关掉的。
    #[cfg(test)]
    pub fn remove_right(&mut self, index: usize) -> Vec<T> {
        if index >= self.items.len() {
            return Vec::new();
        }
        let removed = self.items.split_off(index + 1);
        if self.active > index {
            self.active = index;
        }
        removed
    }

    #[cfg(test)]
    /// 关掉第 `index` 个左边的所有标签，返回被关掉的（保持原来的顺序）。活动标签在被点标签右边或
    /// 就是它时仍指向同一个标签（下标跟着前移）；活动标签被关掉则落到被点的那个。
    pub fn remove_left(&mut self, index: usize) -> Vec<T> {
        if index == 0 || index >= self.items.len() {
            return Vec::new();
        }
        let rest = self.items.split_off(index);
        let removed = std::mem::replace(&mut self.items, rest);
        // 被点的标签现在是第 0 个：活动标签在它左边时（被关掉了）饱和减到 0 正好落在它上面。
        self.active = self.active.saturating_sub(index);
        removed
    }

    /// 关掉所有满足 `doomed` 的标签，返回被关掉的（保持原来的顺序）。哪些标签由调用方按编号圈定（不是下标：
    /// 确认框开着时标签可能挪位），所以这里只认谓词。活动标签的去向和上面按下标的几个版本、单个 `remove` 一致：
    /// 活动标签没被关就还是它；被关了，先落到满足 `anchor` 的那个（被点的标签），没有就回到它左边最近留下的，
    /// 再没有就用右边最近留下的。
    pub fn remove_matching(
        &mut self,
        doomed: impl Fn(&T) -> bool,
        anchor: impl Fn(&T) -> bool,
    ) -> Vec<T> {
        if self.items.is_empty() {
            return Vec::new();
        }
        let active = self.active.min(self.items.len() - 1);
        let flags: Vec<bool> = self.items.iter().map(&doomed).collect();
        if !flags.iter().any(|gone| *gone) {
            return Vec::new();
        }
        // 留下的标签在旧列表里的下标；新下标就是它在这个表里的位置。
        let kept: Vec<usize> = (0..self.items.len()).filter(|at| !flags[*at]).collect();
        let new_active = if !flags[active] {
            kept.iter().position(|at| *at == active)
        } else {
            kept.iter()
                .position(|at| anchor(&self.items[*at]))
                .or_else(|| kept.iter().rposition(|at| *at < active))
                .or_else(|| kept.iter().position(|at| *at > active))
        };
        let mut removed = Vec::new();
        let mut rest = Vec::new();
        for (item, gone) in std::mem::take(&mut self.items).into_iter().zip(flags) {
            if gone {
                removed.push(item);
            } else {
                rest.push(item);
            }
        }
        self.items = rest;
        self.active = new_active.unwrap_or(0);
        removed
    }

    /// 关掉全部，返回被关掉的。
    pub fn remove_all(&mut self) -> Vec<T> {
        self.active = 0;
        std::mem::take(&mut self.items)
    }

    /// 把第 `from` 个挪到第 `to` 个的位置（拖动换位），中间的标签各让一格；活动标签仍指向原来那个标签
    /// （被挪的是活动标签，它跟着到 `to`；活动标签夹在中间，下标随之前移或后移一格）。
    /// 越界或 `from == to` 什么也不做，返回 `false`。
    pub fn move_item(&mut self, from: usize, to: usize) -> bool {
        if !move_within(&mut self.items, from, to) {
            return false;
        }
        self.active = if self.active == from {
            to
        } else if from < self.active && self.active <= to {
            self.active - 1
        } else if to <= self.active && self.active < from {
            self.active + 1
        } else {
            self.active
        };
        true
    }

    /// 把活动标签往 `step` 方向挪（环绕）；少于两个标签或没挪动返回 `false`。
    pub fn cycle(&mut self, step: isize) -> bool {
        let len = self.items.len();
        if len < 2 {
            return false;
        }
        let next = (self.active as isize + step).rem_euclid(len as isize) as usize;
        if next == self.active {
            return false;
        }
        self.active = next;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::TabSet;

    fn set(names: &[&'static str], active: usize) -> TabSet<&'static str> {
        let mut tabs = TabSet::new();
        for name in names {
            tabs.push(*name);
        }
        tabs.select(active);
        tabs
    }

    fn names(tabs: &TabSet<&'static str>) -> Vec<&'static str> {
        tabs.items().to_vec()
    }

    #[test]
    fn an_empty_set_has_no_active_tab() {
        let mut tabs: TabSet<&str> = TabSet::new();
        assert_eq!(tabs.active_index(), None);
        assert_eq!(tabs.active(), None);
        assert!(!tabs.select(0));
        assert!(!tabs.cycle(1));
        assert_eq!(tabs.remove(0), None);
        assert!(tabs.remove_others(0).is_empty());
        assert!(tabs.remove_right(0).is_empty());
        assert!(tabs.remove_left(0).is_empty());
        assert!(tabs.remove_all().is_empty());
    }

    #[test]
    fn new_tabs_go_last_and_become_active() {
        let mut tabs = set(&["a", "b"], 0);
        assert_eq!(tabs.push("c"), 2);
        assert_eq!(tabs.active(), Some(&"c"));
        assert_eq!(names(&tabs), ["a", "b", "c"]);
    }

    #[test]
    fn a_duplicate_sits_right_of_its_source() {
        let mut tabs = set(&["a", "b", "c"], 0);
        assert_eq!(tabs.insert_after(0, "a2"), 1);
        assert_eq!(names(&tabs), ["a", "a2", "b", "c"]);
        assert_eq!(tabs.active(), Some(&"a2"));
        // 源标签不存在时退回加在最后。
        assert_eq!(tabs.insert_after(99, "z"), 4);
        assert_eq!(tabs.active(), Some(&"z"));
    }

    #[test]
    fn closing_the_active_tab_prefers_the_left_neighbour() {
        let mut tabs = set(&["a", "b", "c"], 1);
        assert_eq!(tabs.remove(1), Some("b"));
        assert_eq!(tabs.active(), Some(&"a"));

        let mut tabs = set(&["a", "b", "c"], 2);
        tabs.remove(2);
        assert_eq!(tabs.active(), Some(&"b"));
    }

    #[test]
    fn closing_the_first_active_tab_falls_to_the_right_one() {
        let mut tabs = set(&["a", "b", "c"], 0);
        tabs.remove(0);
        assert_eq!(tabs.active(), Some(&"b"));
        assert_eq!(tabs.active_index(), Some(0));
    }

    #[test]
    fn closing_a_tab_left_of_the_active_one_keeps_the_same_tab_active() {
        let mut tabs = set(&["a", "b", "c", "d"], 3);
        tabs.remove(1);
        assert_eq!(tabs.active(), Some(&"d"));
        assert_eq!(tabs.active_index(), Some(2));
    }

    #[test]
    fn closing_a_tab_right_of_the_active_one_changes_nothing() {
        let mut tabs = set(&["a", "b", "c"], 0);
        tabs.remove(2);
        assert_eq!(tabs.active(), Some(&"a"));
    }

    #[test]
    fn closing_the_last_tab_leaves_nothing_active() {
        let mut tabs = set(&["a"], 0);
        assert_eq!(tabs.remove(0), Some("a"));
        assert!(tabs.is_empty());
        assert_eq!(tabs.active_index(), None);
    }

    #[test]
    fn close_others_keeps_only_the_clicked_tab_and_activates_it() {
        let mut tabs = set(&["a", "b", "c", "d"], 3);
        let removed = tabs.remove_others(1);
        assert_eq!(removed, ["a", "c", "d"]);
        assert_eq!(names(&tabs), ["b"]);
        assert_eq!(tabs.active(), Some(&"b"));
    }

    #[test]
    fn close_right_moves_a_closed_active_tab_back_to_the_clicked_one() {
        let mut tabs = set(&["a", "b", "c", "d"], 3);
        let removed = tabs.remove_right(1);
        assert_eq!(removed, ["c", "d"]);
        assert_eq!(names(&tabs), ["a", "b"]);
        assert_eq!(tabs.active(), Some(&"b"));

        // 活动标签在被点标签左边或就是它：不动。
        let mut tabs = set(&["a", "b", "c"], 0);
        tabs.remove_right(1);
        assert_eq!(tabs.active(), Some(&"a"));
    }

    #[test]
    fn close_right_on_the_last_tab_closes_nothing() {
        let mut tabs = set(&["a", "b"], 1);
        assert!(tabs.remove_right(1).is_empty());
        assert_eq!(names(&tabs), ["a", "b"]);
    }

    #[test]
    fn close_left_moves_a_closed_active_tab_to_the_clicked_one() {
        let mut tabs = set(&["a", "b", "c", "d"], 0);
        let removed = tabs.remove_left(2);
        assert_eq!(removed, ["a", "b"]);
        assert_eq!(names(&tabs), ["c", "d"]);
        assert_eq!(tabs.active(), Some(&"c"));
        assert_eq!(tabs.active_index(), Some(0));
    }

    #[test]
    fn close_left_keeps_the_same_tab_active_when_it_is_the_clicked_one_or_to_its_right() {
        let mut tabs = set(&["a", "b", "c", "d"], 2);
        tabs.remove_left(2);
        assert_eq!(tabs.active(), Some(&"c"));
        assert_eq!(tabs.active_index(), Some(0));

        let mut tabs = set(&["a", "b", "c", "d"], 3);
        tabs.remove_left(1);
        assert_eq!(names(&tabs), ["b", "c", "d"]);
        assert_eq!(tabs.active(), Some(&"d"));
        assert_eq!(tabs.active_index(), Some(2));
    }

    #[test]
    fn close_left_on_the_first_tab_or_out_of_range_closes_nothing() {
        let mut tabs = set(&["a", "b"], 1);
        assert!(tabs.remove_left(0).is_empty());
        assert!(tabs.remove_left(5).is_empty());
        assert_eq!(names(&tabs), ["a", "b"]);
        assert_eq!(tabs.active(), Some(&"b"));
    }

    /// V2-U44：关闭改成按标签编号圈定范围（`remove_matching`），结果必须和按下标的参照版本逐个组合一致：
    /// 被关的标签、剩下的顺序、活动标签落在哪都相同。
    #[test]
    fn closing_by_id_matches_the_index_based_rules_for_every_scope_and_active_tab() {
        use crate::close_confirm::{CloseScope, scope_ids};
        let ids: Vec<u64> = (100..105).collect();
        for count in 1..=ids.len() {
            let all = &ids[..count];
            for active in 0..count {
                for clicked in 0..count {
                    for scope in [
                        CloseScope::Others,
                        CloseScope::Left,
                        CloseScope::Right,
                        CloseScope::All,
                    ] {
                        let build = || {
                            let mut tabs = TabSet::new();
                            for id in all {
                                tabs.push(*id);
                            }
                            tabs.select(active);
                            tabs
                        };
                        let mut reference = build();
                        let want = match scope {
                            CloseScope::Others => reference.remove_others(clicked),
                            CloseScope::Left => reference.remove_left(clicked),
                            CloseScope::Right => reference.remove_right(clicked),
                            CloseScope::All => reference.remove_all(),
                        };
                        let doomed = scope_ids(all, clicked, scope);
                        let anchor = all[clicked];
                        let mut tabs = build();
                        let got =
                            tabs.remove_matching(|id| doomed.contains(id), |id| *id == anchor);
                        let what = format!("{count} 个标签 活动={active} 点击={clicked} {scope:?}");
                        assert_eq!(got, want, "被关的标签 {what}");
                        assert_eq!(tabs.items(), reference.items(), "剩下的 {what}");
                        if !tabs.is_empty() {
                            assert_eq!(tabs.active(), reference.active(), "活动标签 {what}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn closing_by_id_skips_missing_ids_and_a_single_close_matches_remove() {
        let mut tabs = set(&["a", "b", "c"], 1);
        // 范围里没有的名字不影响别的；一个都不匹配时什么也不动。
        assert!(tabs.remove_matching(|n| *n == "zzz", |_| false).is_empty());
        assert_eq!(tabs.active(), Some(&"b"));
        // 单个关闭（没有锚点）和 `remove` 一样：关活动的先回左边，没有左边才用右边。
        let mut tabs = set(&["a", "b", "c"], 1);
        assert_eq!(tabs.remove_matching(|n| *n == "b", |_| false), ["b"]);
        assert_eq!(tabs.active(), Some(&"a"));
        let mut tabs = set(&["a", "b", "c"], 0);
        tabs.remove_matching(|n| *n == "a", |_| false);
        assert_eq!(tabs.active(), Some(&"b"));
        // 关的不是活动标签：它跟着前移一格，仍指向同一个标签。
        let mut tabs = set(&["a", "b", "c"], 2);
        tabs.remove_matching(|n| *n == "a", |_| false);
        assert_eq!(tabs.active(), Some(&"c"));
        assert_eq!(tabs.active_index(), Some(1));
        // 关光了：空。
        let mut tabs = set(&["a", "b"], 1);
        assert_eq!(tabs.remove_matching(|_| true, |_| false).len(), 2);
        assert!(tabs.is_empty());
        assert_eq!(tabs.active_index(), None);
    }

    #[test]
    fn close_all_empties_the_set() {
        let mut tabs = set(&["a", "b"], 1);
        assert_eq!(tabs.remove_all(), ["a", "b"]);
        assert!(tabs.is_empty());
        assert_eq!(tabs.active_index(), None);
    }

    #[test]
    fn cycling_wraps_in_both_directions_and_needs_two_tabs() {
        let mut tabs = set(&["a", "b", "c"], 2);
        assert!(tabs.cycle(1));
        assert_eq!(tabs.active(), Some(&"a"));
        assert!(tabs.cycle(-1));
        assert_eq!(tabs.active(), Some(&"c"));

        let mut single = set(&["a"], 0);
        assert!(!single.cycle(1));
        assert_eq!(single.active(), Some(&"a"));
    }

    #[test]
    fn moving_the_active_tab_takes_the_selection_with_it() {
        let mut tabs = set(&["a", "b", "c", "d"], 1);
        assert!(tabs.move_item(1, 3));
        assert_eq!(names(&tabs), ["a", "c", "d", "b"]);
        assert_eq!(tabs.active(), Some(&"b"));
        assert_eq!(tabs.active_index(), Some(3));

        assert!(tabs.move_item(3, 0));
        assert_eq!(names(&tabs), ["b", "a", "c", "d"]);
        assert_eq!(tabs.active(), Some(&"b"));
        assert_eq!(tabs.active_index(), Some(0));
    }

    #[test]
    fn moving_a_tab_across_the_active_one_shifts_it_by_one_but_keeps_it_selected() {
        // 活动标签在被挪标签的右边，被挪的从它左边跳到右边：活动标签前移一格。
        let mut tabs = set(&["a", "b", "c", "d"], 2);
        assert!(tabs.move_item(0, 3));
        assert_eq!(names(&tabs), ["b", "c", "d", "a"]);
        assert_eq!(tabs.active(), Some(&"c"));
        assert_eq!(tabs.active_index(), Some(1));

        // 反方向：被挪的从右边跳到活动标签左边，活动标签后移一格。
        let mut tabs = set(&["a", "b", "c", "d"], 1);
        assert!(tabs.move_item(3, 0));
        assert_eq!(names(&tabs), ["d", "a", "b", "c"]);
        assert_eq!(tabs.active(), Some(&"b"));
        assert_eq!(tabs.active_index(), Some(2));
    }

    #[test]
    fn moving_a_tab_that_does_not_cross_the_active_one_leaves_its_index_alone() {
        // 活动标签在左边，被挪的两个都在它右边。
        let mut tabs = set(&["a", "b", "c", "d"], 0);
        assert!(tabs.move_item(1, 3));
        assert_eq!(names(&tabs), ["a", "c", "d", "b"]);
        assert_eq!(tabs.active(), Some(&"a"));
        assert_eq!(tabs.active_index(), Some(0));

        // 活动标签在右边，被挪的两个都在它左边。
        let mut tabs = set(&["a", "b", "c", "d"], 3);
        assert!(tabs.move_item(0, 2));
        assert_eq!(names(&tabs), ["b", "c", "a", "d"]);
        assert_eq!(tabs.active(), Some(&"d"));
        assert_eq!(tabs.active_index(), Some(3));
    }

    #[test]
    fn moving_a_tab_onto_the_active_slot_from_either_side_keeps_the_same_tab_active() {
        // `to` 正好是活动标签原来的位置：两个方向都要把它挤开一格而不是丢掉。
        let mut tabs = set(&["a", "b", "c"], 1);
        assert!(tabs.move_item(0, 1));
        assert_eq!(names(&tabs), ["b", "a", "c"]);
        assert_eq!(tabs.active(), Some(&"b"));

        let mut tabs = set(&["a", "b", "c"], 1);
        assert!(tabs.move_item(2, 1));
        assert_eq!(names(&tabs), ["a", "c", "b"]);
        assert_eq!(tabs.active(), Some(&"b"));
    }

    #[test]
    fn moving_a_tab_onto_itself_or_out_of_range_changes_nothing() {
        let mut tabs = set(&["a", "b", "c"], 2);
        assert!(!tabs.move_item(1, 1));
        assert!(!tabs.move_item(3, 0));
        assert!(!tabs.move_item(0, 3));
        assert_eq!(names(&tabs), ["a", "b", "c"]);
        assert_eq!(tabs.active(), Some(&"c"));

        let mut empty: TabSet<&str> = TabSet::new();
        assert!(!empty.move_item(0, 0));
        assert_eq!(empty.active_index(), None);
    }

    #[test]
    fn move_within_works_on_a_plain_vec() {
        let mut items = vec!["a", "b", "c"];
        assert!(super::move_within(&mut items, 0, 2));
        assert_eq!(items, ["b", "c", "a"]);
        assert!(!super::move_within(&mut items, 2, 2));
        assert!(!super::move_within(&mut items, 0, 3));
        assert_eq!(items, ["b", "c", "a"]);
    }

    #[test]
    fn select_ignores_out_of_range_and_the_current_tab() {
        let mut tabs = set(&["a", "b"], 0);
        assert!(!tabs.select(0));
        assert!(!tabs.select(5));
        assert!(tabs.select(1));
        assert_eq!(tabs.active(), Some(&"b"));
    }
}
