//! 主机页卡片 / 列表行的键盘操作（V2-U1 第 5 条）：
//!
//! - Tab / Shift+Tab：卡片 / 行是 Tab 停靠点（`tab_stop`），走框架自带的焦点顺序（页面顶部的搜索框和按钮之后，
//!   按屏幕上的次序一张一张过）；聚焦的卡片 / 行有可见的焦点框（`focus_visible`）；
//! - 方向键在卡片 / 行之间移动：网格里 ← → 前后一张、↑ ↓ 上下一排（同一列），列表里 ↑ ↓ 上下一行；
//!   Home / End 到第一张 / 最后一张；
//! - Enter 连接，F2 编辑（和 0.2.14 `VaultView.tsx` 的 `hostKeyDown` 一致：Enter 连接、F2 打开详情）；
//!   Esc 放开焦点。
//!
//! 0.2.14 只有 Enter / F2 和浏览器自带的 Tab 顺序，方向键是这一卡新加的（用户要的）。
//! 纯函数 [`neighbor`] 算「往某个方向走一步到哪张」，不依赖 GPUI。

use ::gpui::{FocusHandle, KeyDownEvent, Window};
use gpui_kit::*;

use super::MainWindow;

/// 主机页当前显示的卡片（按屏幕顺序）和网格每排几张（列表视图是 1）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VaultNav {
    pub ids: Vec<i64>,
    pub columns: usize,
}

/// 键盘上的一步。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Left,
    Right,
    Up,
    Down,
    First,
    Last,
}

impl Step {
    pub fn from_key(key: &str) -> Option<Self> {
        Some(match key {
            "left" => Self::Left,
            "right" => Self::Right,
            "up" => Self::Up,
            "down" => Self::Down,
            "home" => Self::First,
            "end" => Self::Last,
            _ => return None,
        })
    }
}

/// 从第 `index` 张出发走一步，到哪一张（越界返回 `None`，原地不动）。`columns` 是每排几张，
/// 列表视图传 1：← → 在列表里没有意义（返回 `None`），↑ ↓ 上下一行。
pub fn neighbor(count: usize, columns: usize, index: usize, step: Step) -> Option<usize> {
    if count == 0 || index >= count {
        return None;
    }
    let columns = columns.max(1);
    let target = match step {
        Step::First => 0,
        Step::Last => count - 1,
        Step::Left => {
            if columns == 1 {
                return None;
            }
            index.checked_sub(1)?
        }
        Step::Right => {
            if columns == 1 {
                return None;
            }
            index + 1
        }
        Step::Up => index.checked_sub(columns)?,
        Step::Down => index + columns,
    };
    // 下一排比这一排短：↓ 落到最后一张，不落空（资源管理器、0.2.14 的列表都是这个手感）。
    let target = if step == Step::Down && target >= count && index / columns < (count - 1) / columns
    {
        count - 1
    } else {
        target
    };
    (target < count && target != index).then_some(target)
}

impl MainWindow {
    /// 给当前显示的每台主机备好焦点句柄（卡片 / 行靠它接键盘），删掉的主机的句柄清掉。
    pub(super) fn sync_vault_nav(&mut self, ids: Vec<i64>, columns: usize, cx: &mut Context<Self>) {
        for id in &ids {
            self.host_focus
                .entry(*id)
                .or_insert_with(|| cx.focus_handle().tab_stop(true));
        }
        self.host_focus.retain(|id, _| ids.contains(id));
        self.vault_nav = VaultNav { ids, columns };
    }

    pub(super) fn host_focus_handle(&self, id: i64) -> Option<FocusHandle> {
        self.host_focus.get(&id).cloned()
    }

    /// 现在键盘焦点在第几张卡片上。
    #[cfg(debug_assertions)]
    pub(crate) fn focused_host_index(&self, window: &Window) -> Option<usize> {
        self.vault_nav.ids.iter().position(|id| {
            self.host_focus
                .get(id)
                .is_some_and(|handle| handle.is_focused(window))
        })
    }

    /// 把键盘焦点放到第 `index` 张卡片上（同时选中那台主机，左边的树跟着高亮）。
    pub(crate) fn focus_host_index(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(id) = self.vault_nav.ids.get(index).copied() else {
            return false;
        };
        let Some(handle) = self.host_focus.get(&id).cloned() else {
            return false;
        };
        window.focus(&handle, cx);
        self.tree.selected_host = Some(id);
        cx.notify();
        true
    }

    /// 卡片 / 行聚焦着时的按键：Enter 连接、F2 编辑、方向键移动、Esc 放开。
    /// 返回有没有处理（处理了调用方要停止冒泡）。
    pub(crate) fn host_key_down(
        &mut self,
        id: i64,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let mods = event.keystroke.modifiers;
        // 带 Ctrl / Alt / Shift 的组合键留给窗口级快捷键，卡片自己只认单键。
        if mods.control || mods.alt || mods.shift || mods.platform {
            return false;
        }
        let key = event.keystroke.key.to_ascii_lowercase();
        match key.as_str() {
            "enter" => {
                self.open_host(id, window, cx);
                true
            }
            "f2" => {
                self.edit_host(id, window, cx);
                true
            }
            "escape" => {
                window.focus(&self.focus, cx);
                cx.notify();
                true
            }
            other => {
                let Some(step) = Step::from_key(other) else {
                    return false;
                };
                let nav = &self.vault_nav;
                let Some(index) = nav.ids.iter().position(|candidate| *candidate == id) else {
                    return false;
                };
                if let Some(target) = neighbor(nav.ids.len(), nav.columns, index, step) {
                    self.focus_host_index(target, window, cx);
                }
                // 到头了也算处理过：方向键不能再往别处冒泡（比如滚动页面）。
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Step, neighbor};

    #[test]
    fn arrows_walk_a_grid_by_card_and_by_row() {
        // 7 张，每排 3 张：
        //  0 1 2
        //  3 4 5
        //  6
        assert_eq!(neighbor(7, 3, 0, Step::Right), Some(1));
        assert_eq!(
            neighbor(7, 3, 2, Step::Right),
            Some(3),
            "排尾继续往后 = 下一排排头"
        );
        assert_eq!(neighbor(7, 3, 3, Step::Left), Some(2));
        assert_eq!(neighbor(7, 3, 0, Step::Left), None, "第一张往前没有了");
        assert_eq!(neighbor(7, 3, 1, Step::Down), Some(4));
        assert_eq!(neighbor(7, 3, 4, Step::Up), Some(1));
        assert_eq!(neighbor(7, 3, 1, Step::Up), None, "第一排往上没有了");
        assert_eq!(neighbor(7, 3, 6, Step::Right), None, "最后一张往后没有了");
    }

    #[test]
    fn down_into_a_shorter_last_row_lands_on_the_last_card() {
        // 第 3 排只有一张（6）：从 5 号往下落到 6，不落空。
        assert_eq!(neighbor(7, 3, 5, Step::Down), Some(6));
        assert_eq!(neighbor(7, 3, 4, Step::Down), Some(6));
        // 已经在最后一排不再往下。
        assert_eq!(neighbor(7, 3, 6, Step::Down), None);
        // 整排都满的时候往下是同一列。
        assert_eq!(neighbor(6, 3, 1, Step::Down), Some(4));
        assert_eq!(neighbor(6, 3, 4, Step::Down), None);
    }

    #[test]
    fn home_and_end_jump_to_the_ends() {
        assert_eq!(neighbor(7, 3, 4, Step::First), Some(0));
        assert_eq!(neighbor(7, 3, 4, Step::Last), Some(6));
        assert_eq!(neighbor(7, 3, 0, Step::First), None, "已经在那儿");
    }

    #[test]
    fn a_list_moves_by_row_and_ignores_left_and_right() {
        assert_eq!(neighbor(5, 1, 2, Step::Down), Some(3));
        assert_eq!(neighbor(5, 1, 2, Step::Up), Some(1));
        assert_eq!(neighbor(5, 1, 0, Step::Up), None);
        assert_eq!(neighbor(5, 1, 4, Step::Down), None);
        assert_eq!(neighbor(5, 1, 2, Step::Left), None);
        assert_eq!(neighbor(5, 1, 2, Step::Right), None);
    }

    #[test]
    fn nothing_moves_in_an_empty_or_stale_list() {
        assert_eq!(neighbor(0, 3, 0, Step::Right), None);
        assert_eq!(neighbor(3, 3, 9, Step::Left), None, "下标已经失效");
        assert_eq!(neighbor(1, 0, 0, Step::Down), None, "列数 0 当 1 处理");
    }

    #[test]
    fn keys_map_to_steps() {
        assert_eq!(Step::from_key("left"), Some(Step::Left));
        assert_eq!(Step::from_key("home"), Some(Step::First));
        assert_eq!(Step::from_key("end"), Some(Step::Last));
        assert_eq!(Step::from_key("a"), None);
    }
}
