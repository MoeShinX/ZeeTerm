//! 终端输入法的纯逻辑：组合目标、提交门禁和候选框位置。

use ::gpui::{Bounds, Pixels, Point, px};

/// 组合期间目标固定；切换焦点、广播或连接代次后宁可丢弃，不能改发别处。
#[derive(Clone, Debug)]
pub struct Composition<T> {
    marked: String,
    target: Option<T>,
    active: bool,
}

impl<T> Default for Composition<T> {
    fn default() -> Self {
        Self {
            marked: String::new(),
            target: None,
            active: false,
        }
    }
}

impl<T: PartialEq> Composition<T> {
    pub fn marked(&self) -> &str {
        &self.marked
    }

    pub fn invalidate(&mut self) {
        if self.active {
            self.target = None;
        }
    }

    pub fn matches_target(&self, target: Option<&T>) -> bool {
        !self.active || (self.target.is_some() && self.target.as_ref() == target)
    }

    pub fn set_marked(&mut self, text: &str, target: Option<T>, allowed: bool) {
        if text.is_empty() {
            *self = Self::default();
            return;
        }
        if !self.active {
            self.target = if allowed { target } else { None };
            self.marked = text.to_owned();
            self.active = true;
            return;
        }
        // 门禁关闭后整个组合失效；重新打开也不能复活旧输入。
        if !allowed || self.target != target {
            self.target = None;
        }
        self.marked = text.to_owned();
    }

    pub fn commit(&mut self, text: &str, target: Option<T>, allowed: bool) -> Option<Vec<u8>> {
        let send =
            allowed && target.is_some() && self.matches_target(target.as_ref()) && !text.is_empty();
        *self = Self::default();
        send.then(|| text.as_bytes().to_vec())
    }
}

/// 焦点、模态和锁屏由按键与系统文本回调共用，避免系统 IME 绕过按键监听。
pub fn input_allowed(focused: bool, modal: bool, locked: bool, has_tab: bool) -> bool {
    focused && !modal && !locked && has_tab
}

/// 光标格子在窗口里的矩形，给输入法放候选框。
pub fn cursor_bounds(
    left: f32,
    top: f32,
    cell_width: f32,
    cell_height: f32,
    col: usize,
    row: usize,
) -> Bounds<Pixels> {
    Bounds {
        origin: Point {
            x: px(left + col as f32 * cell_width),
            y: px(top + row as f32 * cell_height),
        },
        size: ::gpui::Size {
            width: px(cell_width),
            height: px(cell_height),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commits_once_and_cancellation_sends_nothing() {
        let mut composition = Composition::default();
        composition.set_marked("ni", Some(1), true);
        assert_eq!(composition.marked(), "ni");
        assert_eq!(
            composition.commit("你", Some(1), true),
            Some("你".as_bytes().to_vec())
        );
        assert_eq!(composition.marked(), "");
        composition.set_marked("hao", Some(1), true);
        assert_eq!(composition.commit("", Some(1), true), None);
    }

    #[test]
    fn target_changes_do_not_redirect_or_rebind_a_composition() {
        let mut composition = Composition::default();
        composition.set_marked("ni", Some(1), true);
        assert!(!composition.matches_target(Some(&2)));
        composition.set_marked("你", Some(2), true);
        assert_eq!(composition.commit("你", Some(2), true), None);
        composition.set_marked("hao", Some(2), true);
        assert_eq!(
            composition.commit("好", Some(2), true),
            Some("好".as_bytes().to_vec())
        );
    }

    #[test]
    fn invalidated_composition_cannot_revive_when_focus_returns() {
        let mut composition = Composition::default();
        composition.set_marked("ni", Some(1), true);
        composition.invalidate();
        composition.set_marked("你", Some(1), true);
        assert_eq!(composition.commit("你", Some(1), true), None);
    }

    #[test]
    fn modal_blocks_direct_commit_and_existing_composition() {
        let mut composition = Composition::default();
        assert_eq!(composition.commit("a", Some(1), false), None);
        composition.set_marked("ni", Some(1), true);
        assert_eq!(composition.commit("你", Some(1), false), None);
        composition.set_marked("ni", Some(1), false);
        assert_eq!(composition.commit("你", Some(1), true), None);
    }

    #[test]
    fn input_gate_requires_focus_and_no_modal_or_lock() {
        assert!(input_allowed(true, false, false, true));
        assert!(!input_allowed(false, false, false, true));
        assert!(!input_allowed(true, true, false, true));
        assert!(!input_allowed(true, false, true, true));
        assert!(!input_allowed(true, false, false, false));
    }

    #[test]
    fn cursor_bounds_follow_the_cell() {
        let bounds = cursor_bounds(10.0, 20.0, 8.0, 16.0, 3, 2);
        assert_eq!(bounds.origin.x, px(34.0));
        assert_eq!(bounds.origin.y, px(52.0));
    }
}
