//! 分屏树与布局（V2-G06x）：0.2.14 的 `ui/src/state/splitTree.ts` 和 `layout.ts` 的 Rust 版。
//!
//! 纯数据、不依赖 GPUI，单测直接调。一个标签里每个终端窗格是树的叶子，分割点是二叉节点；
//! 拆分、关闭后的合并、比例夹取、拖动换算都和 0.2.14 同一套规则：
//!
//! - 拆分把一个叶子换成「原叶子 + 新叶子」，比例 0.5，新窗格在后半（右 / 下）；
//! - 关闭一个窗格，它所在的分割点被兄弟节点顶掉；焦点窗格被关时落到最近兄弟的第一个叶子；
//! - 比例有两层夹取：树上存的比例夹在 `[0.1, 0.9]`（`set_ratio`），拖动时再按像素夹——
//!   两边都要留出一个窗格的最小尺寸（`ratio_from_pointer`）。

/// 窗格的编号：全进程唯一，树的叶子只记它。
pub type PaneId = u64;

/// 分割方向。`X` = 左右排（一左一右），`Y` = 上下排。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    X,
    Y,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Pane(PaneId),
    Split(Box<Split>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Split {
    pub id: u64,
    pub axis: Axis,
    /// 第一个子节点占的比例。
    pub ratio: f32,
    pub first: Node,
    pub second: Node,
}

/// 拖动分隔条的比例下限 / 上限：不允许把某一侧拖没（`splitTree.ts` 的 `MIN_RATIO`/`MAX_RATIO`）。
pub const MIN_RATIO: f32 = 0.1;
pub const MAX_RATIO: f32 = 0.9;

pub fn clamp_ratio(ratio: f32) -> f32 {
    if !ratio.is_finite() {
        return 0.5;
    }
    ratio.clamp(MIN_RATIO, MAX_RATIO)
}

/// 下一个分割点的编号（全进程递增）。
pub fn next_split_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl Node {
    /// 所有叶子，按视觉顺序（左到右、上到下）。
    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<PaneId>) {
        match self {
            Node::Pane(id) => out.push(*id),
            Node::Split(split) => {
                split.first.collect(out);
                split.second.collect(out);
            }
        }
    }

    /// 第一个叶子（默认焦点）。
    pub fn first_pane(&self) -> PaneId {
        match self {
            Node::Pane(id) => *id,
            Node::Split(split) => split.first.first_pane(),
        }
    }

    pub fn has_pane(&self, pane: PaneId) -> bool {
        match self {
            Node::Pane(id) => *id == pane,
            Node::Split(split) => split.first.has_pane(pane) || split.second.has_pane(pane),
        }
    }

    /// 把 `pane` 一分为二：原来的留在前半，新窗格在后半。返回有没有找到 `pane`。
    pub fn split_pane(
        &mut self,
        pane: PaneId,
        new_pane: PaneId,
        split_id: u64,
        axis: Axis,
    ) -> bool {
        match self {
            Node::Pane(id) if *id == pane => {
                *self = Node::Split(Box::new(Split {
                    id: split_id,
                    axis,
                    ratio: 0.5,
                    first: Node::Pane(pane),
                    second: Node::Pane(new_pane),
                }));
                true
            }
            Node::Pane(_) => false,
            Node::Split(split) => {
                split.first.split_pane(pane, new_pane, split_id, axis)
                    || split.second.split_pane(pane, new_pane, split_id, axis)
            }
        }
    }

    /// 关掉一个窗格：它所在的分割点被兄弟节点顶掉。返回 `None` 表示这是最后一个窗格，
    /// 整个标签该关了；`pane` 不在树里就原样返回。
    pub fn close_pane(self, pane: PaneId) -> Option<Node> {
        match self {
            Node::Pane(id) => (id != pane).then_some(Node::Pane(id)),
            Node::Split(split) => {
                let Split {
                    id,
                    axis,
                    ratio,
                    first,
                    second,
                } = *split;
                let first_had = first.has_pane(pane);
                let second_had = second.has_pane(pane);
                if !first_had && !second_had {
                    return Some(Node::Split(Box::new(Split {
                        id,
                        axis,
                        ratio,
                        first,
                        second,
                    })));
                }
                let first = first.close_pane(pane);
                let second = second.close_pane(pane);
                match (first, second) {
                    (None, second) => second,
                    (first, None) => first,
                    (Some(first), Some(second)) => Some(Node::Split(Box::new(Split {
                        id,
                        axis,
                        ratio,
                        first,
                        second,
                    }))),
                }
            }
        }
    }

    /// 和 `pane` 相邻（沿树往上最近的另一半）的第一个叶子。
    pub fn sibling_first_pane(&self, pane: PaneId) -> Option<PaneId> {
        match self {
            Node::Pane(_) => None,
            Node::Split(split) => sibling_of(split, pane).map(Node::first_pane),
        }
    }

    /// 所有分割点的 (编号, 方向, 比例)，按视觉顺序（先序）。验证钩子和测试读。
    #[cfg(any(debug_assertions, test))]
    pub fn splits(&self) -> Vec<(u64, Axis, f32)> {
        let mut out = Vec::new();
        self.collect_splits(&mut out);
        out
    }

    #[cfg(any(debug_assertions, test))]
    fn collect_splits(&self, out: &mut Vec<(u64, Axis, f32)>) {
        if let Node::Split(split) = self {
            out.push((split.id, split.axis, split.ratio));
            split.first.collect_splits(out);
            split.second.collect_splits(out);
        }
    }

    /// 调整某个分割点的比例（夹在 `[MIN_RATIO, MAX_RATIO]`）。返回有没有找到。
    pub fn set_ratio(&mut self, split_id: u64, ratio: f32) -> bool {
        match self {
            Node::Pane(_) => false,
            Node::Split(split) => {
                if split.id == split_id {
                    split.ratio = clamp_ratio(ratio);
                    return true;
                }
                split.first.set_ratio(split_id, ratio) || split.second.set_ratio(split_id, ratio)
            }
        }
    }
}

fn sibling_of(split: &Split, pane: PaneId) -> Option<&Node> {
    if split.first.has_pane(pane) {
        if matches!(split.first, Node::Pane(_)) {
            return Some(&split.second);
        }
        if let Node::Split(inner) = &split.first {
            return sibling_of(inner, pane).or(Some(&split.second));
        }
    }
    if split.second.has_pane(pane) {
        if matches!(split.second, Node::Pane(_)) {
            return Some(&split.first);
        }
        if let Node::Split(inner) = &split.second {
            return sibling_of(inner, pane).or(Some(&split.first));
        }
    }
    None
}

/// 关掉一个窗格并给出新的焦点（`closePaneWithFocus`）：
///
/// - 关的不是焦点窗格：焦点不动；
/// - 关的是焦点窗格：落到相邻窗格（最近的另一半）的第一个叶子；
/// - 关的是最后一个窗格：树变成 `None`，焦点也没了。
pub fn close_pane_with_focus(
    tree: Node,
    pane: PaneId,
    focused: PaneId,
) -> (Option<Node>, Option<PaneId>) {
    let sibling = tree.sibling_first_pane(pane);
    let Some(tree) = tree.close_pane(pane) else {
        return (None, None);
    };
    if pane != focused {
        return (Some(tree), Some(focused));
    }
    let focus = match sibling {
        Some(sibling) if tree.has_pane(sibling) => sibling,
        _ => tree.first_pane(),
    };
    (Some(tree), Some(focus))
}

// ---------------- 布局（layout.ts） ----------------

/// 矩形，单位是逻辑像素，原点在终端区域左上角。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Divider {
    pub split_id: u64,
    pub axis: Axis,
    /// 分隔条自己的矩形。
    pub rect: Rect,
    /// 拖动时算比例用的轨道：整个分割点的起点和长度。
    pub track_start: f32,
    pub track_len: f32,
    /// 轨道方向可用的长度（扣掉分隔条本身）。
    pub usable: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layout {
    /// 每个叶子的矩形，按视觉顺序。
    pub rects: Vec<(PaneId, Rect)>,
    pub dividers: Vec<Divider>,
}

impl Layout {
    pub fn rect_of(&self, pane: PaneId) -> Option<Rect> {
        self.rects
            .iter()
            .find(|(id, _)| *id == pane)
            .map(|(_, rect)| *rect)
    }
}

/// 分隔条厚度（px）：和 0.2.14 的 `DIVIDER_SIZE` 一致。
pub const DIVIDER_SIZE: f32 = 4.0;
/// 分隔条的命中宽度（px）：比可见线宽，每边多出来的部分透明、叠在窗格边缘上（`HIT_EXTRA`）。
pub const DIVIDER_HIT: f32 = 8.0;

/// 每个窗格的最小尺寸（0.2.14 UI-1 第 3 节）：约 20 列 × 5 行、按默认 14px 等宽字体的单元格估算。
/// 拖分隔条不能把窗格拖成 0。
pub const MIN_PANE_WIDTH: f32 = 170.0;
pub const MIN_PANE_HEIGHT: f32 = 85.0;

/// 拖动时的比例夹取：轨道两边都要留出最小尺寸。
pub fn clamp_ratio_px(ratio: f32, usable: f32, axis: Axis) -> f32 {
    let min_pixels = match axis {
        Axis::X => MIN_PANE_WIDTH,
        Axis::Y => MIN_PANE_HEIGHT,
    };
    // 轨道本来就不到两个最小尺寸时（很窄的窗口 / 套了好几层分屏）：贴在平分上，
    // 不让拖到极端（区间恒定合法）。
    let min_ratio = (min_pixels / usable.max(1.0)).min(0.5);
    ratio.clamp(min_ratio, 1.0 - min_ratio)
}

/// 算出一棵树在给定尺寸下的摆法。
pub fn compute_layout(tree: &Node, width: f32, height: f32) -> Layout {
    let mut layout = Layout::default();
    walk(
        tree,
        Rect {
            x: 0.0,
            y: 0.0,
            w: width.max(0.0),
            h: height.max(0.0),
        },
        &mut layout,
    );
    layout
}

fn walk(node: &Node, rect: Rect, layout: &mut Layout) {
    let split = match node {
        Node::Pane(id) => {
            layout.rects.push((*id, rect));
            return;
        }
        Node::Split(split) => split,
    };
    let horizontal = split.axis == Axis::X;
    let total = if horizontal { rect.w } else { rect.h };
    let usable = (total - DIVIDER_SIZE).max(0.0);
    let first = usable * split.ratio;
    let second = (usable - first).max(0.0);
    if horizontal {
        walk(&split.first, Rect { w: first, ..rect }, layout);
        layout.dividers.push(Divider {
            split_id: split.id,
            axis: Axis::X,
            rect: Rect {
                x: rect.x + first,
                y: rect.y,
                w: DIVIDER_SIZE,
                h: rect.h,
            },
            track_start: rect.x,
            track_len: total,
            usable,
        });
        walk(
            &split.second,
            Rect {
                x: rect.x + first + DIVIDER_SIZE,
                w: second,
                ..rect
            },
            layout,
        );
    } else {
        walk(&split.first, Rect { h: first, ..rect }, layout);
        layout.dividers.push(Divider {
            split_id: split.id,
            axis: Axis::Y,
            rect: Rect {
                x: rect.x,
                y: rect.y + first,
                w: rect.w,
                h: DIVIDER_SIZE,
            },
            track_start: rect.y,
            track_len: total,
            usable,
        });
        walk(
            &split.second,
            Rect {
                y: rect.y + first + DIVIDER_SIZE,
                h: second,
                ..rect
            },
            layout,
        );
    }
}

/// 拖动分隔条时，指针位置（相对终端区域）换算成比例；指针在分隔条中间，所以要补半个厚度。
pub fn ratio_from_pointer(divider: &Divider, pointer: f32) -> f32 {
    if divider.usable <= 0.0 {
        return 0.5;
    }
    let raw = (pointer - divider.track_start - DIVIDER_SIZE / 2.0) / divider.usable;
    clamp_ratio_px(raw, divider.usable, divider.axis)
}

/// 核对一份布局把 `width × height` 的区域铺满、没有重叠：窗格和分隔条的面积加起来等于总面积
/// （浮点误差内），窗格两两不相交，都在区域里。验证钩子拿它核对真实窗口里的布局。
#[cfg(any(debug_assertions, test))]
pub fn check_tiling(layout: &Layout, width: f32, height: f32) -> Result<(), String> {
    let eps = 0.5;
    let inside = |rect: &Rect| {
        rect.w >= -eps
            && rect.h >= -eps
            && rect.x >= -eps
            && rect.y >= -eps
            && rect.x + rect.w <= width + eps
            && rect.y + rect.h <= height + eps
    };
    let rects: Vec<Rect> = layout
        .rects
        .iter()
        .map(|(_, rect)| *rect)
        .chain(layout.dividers.iter().map(|divider| divider.rect))
        .collect();
    if let Some(bad) = rects.iter().find(|rect| !inside(rect)) {
        return Err(format!("a rect lies outside the area: {bad:?}"));
    }
    for (i, a) in rects.iter().enumerate() {
        for b in &rects[i + 1..] {
            let overlap_w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
            let overlap_h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
            if overlap_w > eps && overlap_h > eps {
                return Err(format!("rects overlap: {a:?} and {b:?}"));
            }
        }
    }
    let covered: f32 = rects.iter().map(|rect| rect.w * rect.h).sum();
    let total = width * height;
    if (covered - total).abs() > total * 1e-3 + 1.0 {
        return Err(format!(
            "not fully covered: rect area {covered}, area size {total}"
        ));
    }
    Ok(())
}

/// 指针（相对终端区域）落在哪条分隔条上：命中区比可见线宽（`DIVIDER_HIT`）。
pub fn divider_at(layout: &Layout, x: f32, y: f32) -> Option<&Divider> {
    let grow = (DIVIDER_HIT - DIVIDER_SIZE) / 2.0;
    layout.dividers.iter().find(|divider| {
        let rect = divider.rect;
        let hit = match divider.axis {
            Axis::X => Rect {
                x: rect.x - grow,
                w: rect.w + grow * 2.0,
                ..rect
            },
            Axis::Y => Rect {
                y: rect.y - grow,
                h: rect.h + grow * 2.0,
                ..rect
            },
        };
        hit.contains(x, y)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single(id: PaneId) -> Node {
        Node::Pane(id)
    }

    fn split(tree: &mut Node, pane: PaneId, new: PaneId, split_id: u64, axis: Axis) {
        assert!(tree.split_pane(pane, new, split_id, axis));
    }

    /// A | (B / C)：左右分，再把右边上下分。
    fn three() -> Node {
        let mut tree = single(1);
        split(&mut tree, 1, 2, 10, Axis::X);
        split(&mut tree, 2, 3, 11, Axis::Y);
        tree
    }

    #[test]
    fn splitting_puts_the_new_pane_after_the_old_one() {
        let mut tree = single(1);
        split(&mut tree, 1, 2, 10, Axis::X);
        assert_eq!(tree.pane_ids(), vec![1, 2]);
        let Node::Split(root) = &tree else {
            panic!("应该是分割点");
        };
        assert_eq!((root.axis, root.ratio), (Axis::X, 0.5));
        assert_eq!(root.first, Node::Pane(1));
        assert_eq!(root.second, Node::Pane(2));
    }

    #[test]
    fn a_pane_can_be_split_again_in_either_direction() {
        let mut tree = three();
        // 再把 C 左右分：D 在 C 的右边，visual 顺序 A B C D。
        split(&mut tree, 3, 4, 12, Axis::X);
        assert_eq!(tree.pane_ids(), vec![1, 2, 3, 4]);
        // 把 A 上下分：E 在 A 下面，紧跟在 A 后面。
        split(&mut tree, 1, 5, 13, Axis::Y);
        assert_eq!(tree.pane_ids(), vec![1, 5, 2, 3, 4]);
    }

    #[test]
    fn splitting_an_unknown_pane_changes_nothing() {
        let mut tree = three();
        let before = tree.clone();
        assert!(!tree.split_pane(99, 100, 20, Axis::X));
        assert_eq!(tree, before);
    }

    #[test]
    fn closing_a_pane_lets_its_sibling_take_the_place() {
        let tree = three();
        // 关掉 B：C 顶掉 (B / C) 这个分割点，成了 A | C。
        let after = tree.close_pane(2).expect("还剩窗格");
        assert_eq!(after.pane_ids(), vec![1, 3]);
        let Node::Split(root) = &after else {
            panic!("应该还是分割点");
        };
        assert_eq!(root.axis, Axis::X);
        assert_eq!(root.second, Node::Pane(3));
    }

    #[test]
    fn closing_the_middle_pane_of_a_four_pane_grid_keeps_the_rest_in_place() {
        // 左右分，两边各上下分：A/B | C/D。
        let mut tree = single(1);
        split(&mut tree, 1, 3, 10, Axis::X);
        split(&mut tree, 1, 2, 11, Axis::Y);
        split(&mut tree, 3, 4, 12, Axis::Y);
        assert_eq!(tree.pane_ids(), vec![1, 2, 3, 4]);
        let after = tree.close_pane(2).expect("还剩窗格");
        assert_eq!(after.pane_ids(), vec![1, 3, 4]);
        // A 占满左半，C/D 仍是右半的上下分。
        let layout = compute_layout(&after, 1004.0, 504.0);
        let a = layout.rect_of(1).expect("A");
        let c = layout.rect_of(3).expect("C");
        let d = layout.rect_of(4).expect("D");
        assert_eq!(a.h, 504.0);
        assert!(c.x > a.x + a.w && d.x == c.x);
        assert!(d.y > c.y);
    }

    #[test]
    fn closing_the_last_pane_empties_the_tree() {
        assert_eq!(single(1).close_pane(1), None);
    }

    #[test]
    fn closing_an_unknown_pane_returns_the_tree_untouched() {
        let tree = three();
        assert_eq!(tree.clone().close_pane(99), Some(tree));
    }

    #[test]
    fn closing_keeps_the_ratio_of_the_splits_that_survive() {
        let mut tree = three();
        assert!(tree.set_ratio(10, 0.3));
        let after = tree.close_pane(3).expect("还剩窗格");
        let Node::Split(root) = &after else {
            panic!("应该是分割点");
        };
        assert!((root.ratio - 0.3).abs() < f32::EPSILON);
        assert_eq!(after.pane_ids(), vec![1, 2]);
    }

    #[test]
    fn the_sibling_is_the_nearest_other_half() {
        let tree = three();
        // B 的兄弟是 C；C 的兄弟是 B；A 的兄弟是 (B/C) 的第一个叶子 B。
        assert_eq!(tree.sibling_first_pane(2), Some(3));
        assert_eq!(tree.sibling_first_pane(3), Some(2));
        assert_eq!(tree.sibling_first_pane(1), Some(2));
        assert_eq!(single(1).sibling_first_pane(1), None);
    }

    #[test]
    fn closing_a_pane_moves_focus_like_0_2_14() {
        // 关的不是焦点：焦点不动。
        let (tree, focus) = close_pane_with_focus(three(), 3, 1);
        assert_eq!(tree.expect("树").pane_ids(), vec![1, 2]);
        assert_eq!(focus, Some(1));
        // 关的是焦点：落到相邻窗格。
        let (tree, focus) = close_pane_with_focus(three(), 2, 2);
        assert_eq!(tree.expect("树").pane_ids(), vec![1, 3]);
        assert_eq!(focus, Some(3));
        // 关掉左边整块（A）：相邻的是右边那块的第一个叶子。
        let (_, focus) = close_pane_with_focus(three(), 1, 1);
        assert_eq!(focus, Some(2));
        // 最后一个窗格：树和焦点都没了。
        let (tree, focus) = close_pane_with_focus(single(1), 1, 1);
        assert_eq!((tree, focus), (None, None));
    }

    #[test]
    fn ratios_are_clamped_and_unknown_splits_are_ignored() {
        let mut tree = three();
        assert!(tree.set_ratio(10, 0.0));
        assert!(tree.set_ratio(11, 5.0));
        let Node::Split(root) = &tree else {
            panic!("应该是分割点");
        };
        assert_eq!(root.ratio, MIN_RATIO);
        let Node::Split(inner) = &root.second else {
            panic!("应该是分割点");
        };
        assert_eq!(inner.ratio, MAX_RATIO);
        assert!(!tree.set_ratio(99, 0.5));
        assert_eq!(clamp_ratio(f32::NAN), 0.5);
    }

    // ---- 布局 ----

    #[test]
    fn a_single_pane_fills_the_area() {
        let layout = compute_layout(&single(1), 800.0, 600.0);
        assert_eq!(
            layout.rect_of(1),
            Some(Rect {
                x: 0.0,
                y: 0.0,
                w: 800.0,
                h: 600.0
            })
        );
        assert!(layout.dividers.is_empty());
    }

    #[test]
    fn left_right_split_halves_the_width_minus_the_divider() {
        let mut tree = single(1);
        split(&mut tree, 1, 2, 10, Axis::X);
        let layout = compute_layout(&tree, 804.0, 600.0);
        let half = (804.0 - DIVIDER_SIZE) / 2.0;
        assert_eq!(layout.rect_of(1).map(|r| r.w), Some(half));
        assert_eq!(layout.rect_of(2).map(|r| r.x), Some(half + DIVIDER_SIZE));
        assert_eq!(layout.dividers.len(), 1);
        assert_eq!(layout.dividers[0].rect.x, half);
        assert_eq!(layout.dividers[0].rect.w, DIVIDER_SIZE);
    }

    #[test]
    fn top_bottom_split_halves_the_height_minus_the_divider() {
        let mut tree = single(1);
        split(&mut tree, 1, 2, 10, Axis::Y);
        let layout = compute_layout(&tree, 800.0, 604.0);
        let half = (604.0 - DIVIDER_SIZE) / 2.0;
        assert_eq!(layout.rect_of(1).map(|r| r.h), Some(half));
        assert_eq!(layout.rect_of(2).map(|r| r.y), Some(half + DIVIDER_SIZE));
        assert_eq!(layout.dividers[0].axis, Axis::Y);
    }

    #[test]
    fn nested_splits_only_divide_their_own_area() {
        let layout = compute_layout(&three(), 804.0, 404.0);
        let half_w = (804.0 - DIVIDER_SIZE) / 2.0;
        let half_h = (404.0 - DIVIDER_SIZE) / 2.0;
        assert_eq!(
            layout.rect_of(1),
            Some(Rect {
                x: 0.0,
                y: 0.0,
                w: half_w,
                h: 404.0
            })
        );
        let b = layout.rect_of(2).expect("B");
        let c = layout.rect_of(3).expect("C");
        assert_eq!((b.x, b.y, b.h), (half_w + DIVIDER_SIZE, 0.0, half_h));
        assert_eq!(
            (c.x, c.y, c.h),
            (half_w + DIVIDER_SIZE, half_h + DIVIDER_SIZE, half_h)
        );
        assert_eq!(layout.dividers.len(), 2);
    }

    #[test]
    fn a_changed_ratio_moves_the_boundary() {
        let mut tree = single(1);
        split(&mut tree, 1, 2, 10, Axis::X);
        assert!(tree.set_ratio(10, 0.8));
        let layout = compute_layout(&tree, 1004.0, 400.0);
        assert!((layout.rect_of(1).map_or(0.0, |r| r.w) - 800.0).abs() < 0.01);
        assert!((layout.rect_of(2).map_or(0.0, |r| r.w) - 200.0).abs() < 0.01);
    }

    #[test]
    fn a_zero_sized_area_does_not_blow_up() {
        let mut tree = single(1);
        split(&mut tree, 1, 2, 10, Axis::X);
        let layout = compute_layout(&tree, 0.0, 0.0);
        assert_eq!(layout.rect_of(1).map(|r| r.w), Some(0.0));
        assert_eq!(layout.dividers[0].usable, 0.0);
        assert_eq!(ratio_from_pointer(&layout.dividers[0], 100.0), 0.5);
    }

    #[test]
    fn the_pointer_in_the_middle_gives_one_half() {
        let mut tree = single(1);
        split(&mut tree, 1, 2, 10, Axis::X);
        let layout = compute_layout(&tree, 804.0, 600.0);
        let divider = layout.dividers[0];
        let pointer = DIVIDER_SIZE / 2.0 + divider.track_start + divider.usable / 2.0;
        assert!((ratio_from_pointer(&divider, pointer) - 0.5).abs() < 1e-5);
    }

    #[test]
    fn dragging_keeps_a_minimum_size_on_both_sides() {
        let mut tree = single(1);
        split(&mut tree, 1, 2, 10, Axis::X);
        let layout = compute_layout(&tree, 1004.0, 600.0);
        let divider = layout.dividers[0];
        let left = ratio_from_pointer(&divider, 0.0);
        assert!((left - MIN_PANE_WIDTH / divider.usable).abs() < 1e-5);
        let right = ratio_from_pointer(&divider, 1004.0);
        assert!((right - (1.0 - MIN_PANE_WIDTH / divider.usable)).abs() < 1e-5);
        // 上下方向用最小高。
        let mut tree = single(1);
        split(&mut tree, 1, 2, 10, Axis::Y);
        let layout = compute_layout(&tree, 800.0, 704.0);
        let vertical = layout.dividers[0];
        assert!(
            (ratio_from_pointer(&vertical, 0.0) - MIN_PANE_HEIGHT / vertical.usable).abs() < 1e-5
        );
    }

    #[test]
    fn a_track_too_short_for_two_minimums_stays_at_the_middle() {
        assert_eq!(clamp_ratio_px(0.1, 200.0, Axis::X), 0.5);
        assert_eq!(clamp_ratio_px(0.9, 200.0, Axis::X), 0.5);
        assert_eq!(clamp_ratio_px(0.5, 0.0, Axis::X), 0.5);
        assert!((clamp_ratio_px(0.01, 1000.0, Axis::X) - 0.17).abs() < 1e-5);
        assert_eq!(clamp_ratio_px(0.5, 1000.0, Axis::X), 0.5);
    }

    #[test]
    fn layouts_tile_the_area_without_gaps_or_overlaps() {
        let mut tree = single(1);
        split(&mut tree, 1, 3, 10, Axis::X);
        split(&mut tree, 1, 2, 11, Axis::Y);
        split(&mut tree, 3, 4, 12, Axis::Y);
        for (w, h) in [(1004.0, 604.0), (817.0, 433.0), (300.0, 200.0)] {
            let layout = compute_layout(&tree, w, h);
            assert_eq!(check_tiling(&layout, w, h), Ok(()), "{w}x{h}");
        }
        // 拖过比例、关掉中间的窗格之后仍然铺满。
        assert!(tree.set_ratio(10, 0.3));
        assert!(tree.set_ratio(11, 0.7));
        let tree = tree.close_pane(2).expect("还剩窗格");
        let layout = compute_layout(&tree, 1004.0, 604.0);
        assert_eq!(check_tiling(&layout, 1004.0, 604.0), Ok(()));
    }

    #[test]
    fn the_tiling_check_notices_overlaps_and_gaps() {
        let mut tree = single(1);
        split(&mut tree, 1, 2, 10, Axis::X);
        let mut layout = compute_layout(&tree, 804.0, 600.0);
        layout.rects[1].1.x -= 50.0; // 右半往左压住分隔条
        assert!(check_tiling(&layout, 804.0, 600.0).is_err());
        let mut layout = compute_layout(&tree, 804.0, 600.0);
        layout.rects[0].1.w -= 100.0; // 左半缩小，留出空隙
        assert!(check_tiling(&layout, 804.0, 600.0).is_err());
    }

    #[test]
    fn splits_are_listed_in_visual_order() {
        let tree = three();
        let splits = tree.splits();
        assert_eq!(splits.len(), 2);
        assert_eq!((splits[0].0, splits[0].1), (10, Axis::X));
        assert_eq!((splits[1].0, splits[1].1), (11, Axis::Y));
        assert!(single(1).splits().is_empty());
    }

    #[test]
    fn the_divider_hit_area_is_wider_than_the_visible_line() {
        let mut tree = single(1);
        split(&mut tree, 1, 2, 10, Axis::X);
        let layout = compute_layout(&tree, 804.0, 600.0);
        let line = layout.dividers[0].rect;
        let mid_y = 300.0;
        // 可见线内、左右各 2px 的透明命中区都算；再远就不算。
        assert!(divider_at(&layout, line.x + 1.0, mid_y).is_some());
        assert!(divider_at(&layout, line.x - 1.5, mid_y).is_some());
        assert!(divider_at(&layout, line.x + DIVIDER_SIZE + 1.5, mid_y).is_some());
        assert!(divider_at(&layout, line.x - 3.0, mid_y).is_none());
        assert!(divider_at(&layout, line.x + DIVIDER_SIZE + 3.0, mid_y).is_none());
    }
}
