//! 主机树的虚线连接线（像 0.2.14 / Xshell 的会话树）：每一层子项左边有一条竖向的细虚线，对齐上一层
//! 展开箭头的中心；每个子项前面一小段横向虚线连到竖线；父分组里最后一个子项的竖线只画到它这一行中间（└ 形）。
//! 根层（"全部主机"、顶层分组、"未分组"本身、平铺模式下的根层主机）不画。
//!
//! 树是一排扁平的行，不为连接线多挂元素：每个子项行自己画属于它的那几段（祖先里后面还有兄弟的各层竖线、
//! 自己这一层的竖线 ├ / └、横线），所以展开 / 收起、拖动排序、搜索过滤之后线都是对的——它只看当前可见的行。
//!
//! 本文件是纯几何：[`row_guides`] 从可见行算出每行的层级 / 是否最后一个 / 哪些祖先层的竖线要穿过；
//! [`segments`] 把它变成行内的几条线段；[`dots`] 把线段切成间隔 [`PITCH`] 的 1px 小点。小点按"整个列表里的
//! 绝对位置"对齐，相邻两行接缝处点距不变，竖线看起来是一条连续的虚线。绘制（`paint`）用一个画布元素，
//! 画在行底色之上，悬停 / 选中的底色盖不住线。

use super::host_tree::TreeRow;

/// 树里一行的高度（和 `sidebar::ROW_HEIGHT` 一致，有测试核对）。
pub const ROW_HEIGHT: f32 = 30.0;
/// 行与行之间的缝（列表的 `gap_px`）。竖线穿过这条缝，上下两行才接得上。
pub const ROW_GAP: f32 = 1.0;
/// 每层缩进（和 `sidebar::INDENT` 一致）。
pub const INDENT: f32 = 16.0;
/// 行内左边距（`tree_line` 的 `pl(8 + 缩进)`）。
const ROW_PAD: f32 = 8.0;
/// 展开箭头框的边长。
const TOGGLE: f32 = 16.0;
/// 展开箭头框和图标之间的间距（行内 `gap_1p5`）。
const ICON_GAP: f32 = 6.0;
/// 小点的间距（一个点 1px，空 2px）。
pub const PITCH: f32 = 3.0;
/// 横线停在子项图标左边多远。
const STUB_CLEARANCE: f32 = 4.0;

/// 相邻两行顶边的距离。
pub const ROW_PITCH: f32 = ROW_HEIGHT + ROW_GAP;

/// 连接线要连到的子项种类（横线终点不一样：子分组连到它的展开箭头，主机连到它的图标）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leaf {
    Group,
    Host,
}

/// 一行的连接线形状。顶层行（深度 0）和非树行没有。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowGuide {
    /// 这一行自己的深度（≥ 1）。
    pub depth: usize,
    /// 自己是不是父分组里最后一个可见子项（└，竖线只画到这一行中间）。
    pub last: bool,
    /// 祖先各层的竖线要不要穿过这一行：下标 `i` 对应深度 `i + 1` 的祖先，
    /// 祖先后面还有兄弟就是 `true`（长度 = `depth - 1`）。
    pub trail: Vec<bool>,
    pub kind: Leaf,
}

/// 一行在树结构里的深度；"全部主机"、分隔线不属于任何分组，当作把上面全部隔断的根。
fn tree_depth(row: &TreeRow) -> Option<usize> {
    match row {
        TreeRow::Group { depth, .. } | TreeRow::Host { depth, .. } => Some(*depth),
        TreeRow::Ungrouped { .. } => Some(0),
        TreeRow::All { .. } | TreeRow::Divider => None,
    }
}

/// 可见行 → 每一行的连接线形状（和 `rows` 一一对应）。O(行数)。
pub fn row_guides(rows: &[TreeRow]) -> Vec<Option<RowGuide>> {
    // 倒着扫：`later[d]` = 在当前位置之后、同一父分组之内，还有没有深度为 d 的行。
    let mut last = vec![false; rows.len()];
    let mut later: Vec<bool> = Vec::new();
    for (at, row) in rows.iter().enumerate().rev() {
        let depth = tree_depth(row).unwrap_or(0);
        if later.len() <= depth {
            later.resize(depth + 1, false);
        }
        last[at] = !later[depth];
        // 这一行之前的行，看不到比它更深的行（它们是它的子孙）；它自己这一层则有了"后面还有"。
        later.truncate(depth + 1);
        later[depth] = true;
    }

    // 顺着扫：`last_at[d]` = 当前这一行的深度 d 的祖先是不是最后一个。
    let mut last_at: Vec<bool> = Vec::new();
    rows.iter()
        .enumerate()
        .map(|(at, row)| {
            let depth = tree_depth(row).unwrap_or(0);
            last_at.truncate(depth);
            // 数据里万一跳层（不该发生），缺的那几层当作最后一个，不让下面的切片越界。
            last_at.resize(depth, true);
            last_at.push(last[at]);
            if depth == 0 {
                return None;
            }
            let kind = match row {
                TreeRow::Group { .. } => Leaf::Group,
                TreeRow::Host { .. } => Leaf::Host,
                _ => return None,
            };
            Some(RowGuide {
                depth,
                last: last[at],
                // 深度 1..depth-1 的祖先：它后面还有兄弟，它那一层的竖线就要穿过这一行。
                trail: last_at[1..depth].iter().map(|is_last| !is_last).collect(),
                kind,
            })
        })
        .collect()
}

/// 第 `level` 层子项的竖线离行左边的距离：上一层展开箭头框的中心。
/// `level` = 子项深度 - 1（顶层分组的子项是 0）。
pub fn guide_x(level: usize) -> f32 {
    ROW_PAD + level as f32 * INDENT + TOGGLE / 2.0
}

/// 行内的一条 1px 粗的线段（位置和大小都是行内坐标，单位 px）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Segment {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Segment {
    fn vertical(x: f32, y0: f32, y1: f32) -> Self {
        Self {
            x,
            y: y0,
            w: 1.0,
            h: y1 - y0,
        }
    }

    fn horizontal(x0: f32, x1: f32, y: f32) -> Self {
        Self {
            x: x0,
            y,
            w: x1 - x0,
            h: 1.0,
        }
    }

    pub fn is_vertical(&self) -> bool {
        self.h > self.w
    }
}

/// 横线的终点（行内 x）：子分组连到它的展开箭头框左边，主机连到它的图标左边留一点空。
fn stub_end(depth: usize, kind: Leaf) -> f32 {
    let content = ROW_PAD + depth as f32 * INDENT;
    match kind {
        Leaf::Group => content,
        // 主机行前面是一个和展开箭头同宽的空位，再隔一个间距才是图标。
        Leaf::Host => content + TOGGLE + ICON_GAP - STUB_CLEARANCE,
    }
}

/// 一行里要画的线段：祖先层穿过的竖线、自己这一层的竖线（├ 整行高并穿过行间缝；└ 只到横线那一行）、横线。
pub fn segments(guide: &RowGuide) -> Vec<Segment> {
    let mid = ROW_HEIGHT / 2.0;
    let mut out = Vec::new();
    for (index, continues) in guide.trail.iter().enumerate() {
        if *continues {
            // 深度 index+1 的祖先在它父亲那一层（level = index）有一条竖线。
            out.push(Segment::vertical(guide_x(index), 0.0, ROW_PITCH));
        }
    }
    let x = guide_x(guide.depth - 1);
    out.push(Segment::vertical(
        x,
        0.0,
        if guide.last { mid + 1.0 } else { ROW_PITCH },
    ));
    out.push(Segment::horizontal(
        x,
        stub_end(guide.depth, guide.kind).max(x + 1.0),
        mid,
    ));
    out
}

/// 线段 → 小点的左上角（行内坐标）。`row_top` 是这一行顶边在整个列表里的位置：竖线的点落在
/// 列表绝对位置能被 [`PITCH`] 整除的地方，相邻两行的竖线因此是同一条连续的虚线；横线从起点开始数。
pub fn dots(segment: &Segment, row_top: f32) -> Vec<(f32, f32)> {
    let mut out = Vec::new();
    if segment.is_vertical() {
        let end = segment.y + segment.h;
        // 第一个落在 [y, end) 里、列表绝对位置对齐 PITCH 的 y。
        let offset = (row_top + segment.y).rem_euclid(PITCH);
        let mut y = if offset == 0.0 {
            segment.y
        } else {
            segment.y + (PITCH - offset)
        };
        while y < end {
            out.push((segment.x, y));
            y += PITCH;
        }
    } else {
        let end = segment.x + segment.w;
        let mut x = segment.x;
        while x < end {
            out.push((x, segment.y));
            x += PITCH;
        }
    }
    out
}

/// 一行的全部小点（行内坐标）。
pub fn row_dots(guide: &RowGuide, row_top: f32) -> Vec<(f32, f32)> {
    segments(guide)
        .iter()
        .flat_map(|segment| dots(segment, row_top))
        .collect()
}

/// 画布元素的宽度：最右边那个点之外再留 1px。
pub fn canvas_width(guide: &RowGuide) -> f32 {
    segments(guide)
        .iter()
        .map(|segment| segment.x + segment.w)
        .fold(0.0, f32::max)
        + 1.0
}

/// 给一行造画布元素（绝对定位，盖在行的左上角，画在行底色之上）。`at` 是这一行在可见行里的下标。
pub fn paint(
    guide: &RowGuide,
    at: usize,
    color: ::gpui::Hsla,
) -> impl ::gpui::IntoElement + ::gpui::Styled {
    use ::gpui::{Bounds, canvas, fill, point, px, size};
    use gpui_kit::prelude::Styled as _;

    let points = row_dots(guide, at as f32 * ROW_PITCH);
    let width = canvas_width(guide);
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            for (x, y) in &points {
                let dot = Bounds {
                    origin: bounds.origin + point(px(*x), px(*y)),
                    size: size(px(1.0), px(1.0)),
                };
                window.paint_quad(fill(dot, color));
            }
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .w(px(width))
    .h(px(ROW_PITCH))
}

/// 每行一个短标记，测试和调试驱动用来断言整棵树的线："-" 没线；"├" / "└" 是这一行自己的形状，
/// 前面每个祖先层一个字符（"|" 竖线穿过，"." 没有）。
#[cfg(any(test, debug_assertions))]
pub fn shape_strings(rows: &[TreeRow]) -> Vec<String> {
    row_guides(rows)
        .into_iter()
        .map(|guide| match guide {
            None => "-".to_owned(),
            Some(g) => {
                let trail: String = g.trail.iter().map(|c| if *c { '|' } else { '.' }).collect();
                format!("{trail}{}", if g.last { '└' } else { '├' })
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(id: i64, depth: usize) -> TreeRow {
        TreeRow::Group {
            id,
            name: format!("g{id}"),
            depth,
            expanded: true,
            count: 0,
        }
    }

    fn host(id: i64, depth: usize) -> TreeRow {
        TreeRow::Host { id, depth }
    }

    #[test]
    fn root_rows_have_no_lines_and_a_lone_child_is_a_corner() {
        let rows = vec![
            TreeRow::All { count: 1 },
            group(1, 0),
            host(10, 1),
            TreeRow::Ungrouped {
                expanded: false,
                count: 0,
            },
        ];
        assert_eq!(shape_strings(&rows), ["-", "-", "└", "-"]);
    }

    #[test]
    fn the_last_child_ends_its_line_and_earlier_children_branch() {
        // 分组 1：子分组 2（里面一台主机）、主机 11、主机 12。
        let rows = vec![
            TreeRow::All { count: 3 },
            group(1, 0),
            group(2, 1),
            host(10, 2),
            host(11, 1),
            host(12, 1),
        ];
        assert_eq!(shape_strings(&rows), ["-", "-", "├", "|└", "├", "└"]);
    }

    #[test]
    fn ancestor_lines_pass_through_only_while_a_later_sibling_exists() {
        // 分组 1 下面：子分组 2（主机 10、11）、子分组 3（主机 12）。
        let rows = vec![
            group(1, 0),
            group(2, 1),
            host(10, 2),
            host(11, 2),
            group(3, 1),
            host(12, 2),
        ];
        // 2 后面还有 3：它的孩子行要有穿过的竖线；3 是最后一个：它的孩子行没有。
        assert_eq!(shape_strings(&rows), ["-", "├", "|├", "|└", "└", ".└"]);
    }

    #[test]
    fn three_levels_deep_keeps_one_flag_per_ancestor() {
        let rows = vec![
            group(1, 0),
            group(2, 1),
            group(3, 2),
            host(10, 3),
            host(11, 2),
            host(12, 1),
        ];
        let guides = row_guides(&rows);
        assert_eq!(
            guides[3].as_ref().map(|g| g.trail.clone()),
            Some(vec![true, true])
        );
        assert_eq!(
            guides[4].as_ref().map(|g| g.trail.clone()),
            Some(vec![true])
        );
        assert_eq!(guides[5].as_ref().map(|g| g.trail.clone()), Some(vec![]));
        assert_eq!(shape_strings(&rows), ["-", "├", "|├", "||└", "|└", "└"]);
    }

    #[test]
    fn a_collapsed_group_is_just_a_row_and_its_children_do_not_exist() {
        // 分组 2 收起：它下面没有行，它自己照常是 ├ / └。
        let rows = vec![
            group(1, 0),
            TreeRow::Group {
                id: 2,
                name: "g2".into(),
                depth: 1,
                expanded: false,
                count: 4,
            },
            host(11, 1),
        ];
        assert_eq!(shape_strings(&rows), ["-", "├", "└"]);
        // 整个顶层分组收起：只剩根行。
        let rows = vec![
            TreeRow::All { count: 3 },
            TreeRow::Group {
                id: 1,
                name: "g1".into(),
                depth: 0,
                expanded: false,
                count: 3,
            },
        ];
        assert_eq!(shape_strings(&rows), ["-", "-"]);
    }

    #[test]
    fn two_root_groups_do_not_share_lines() {
        let rows = vec![
            group(1, 0),
            host(10, 1),
            group(2, 0),
            host(11, 1),
            host(12, 1),
        ];
        assert_eq!(shape_strings(&rows), ["-", "└", "-", "├", "└"]);
    }

    #[test]
    fn ungrouped_hosts_hang_off_the_ungrouped_row_but_flat_root_hosts_do_not() {
        let rows = vec![
            TreeRow::All { count: 2 },
            group(1, 0),
            TreeRow::Ungrouped {
                expanded: true,
                count: 2,
            },
            host(10, 1),
            host(11, 1),
        ];
        assert_eq!(shape_strings(&rows), ["-", "-", "-", "├", "└"]);
        // 平铺模式：根层主机深度 0，中间有分隔线，都没有线。
        let flat = vec![
            TreeRow::All { count: 2 },
            group(1, 0),
            host(5, 1),
            TreeRow::Divider,
            host(10, 0),
            host(11, 0),
        ];
        assert_eq!(shape_strings(&flat), ["-", "-", "└", "-", "-", "-"]);
    }

    #[test]
    fn search_results_use_the_rows_that_are_shown() {
        // 搜索把分组全部展开、只留命中的主机：同样的算法，只看当前可见行。
        let rows = vec![
            TreeRow::All { count: 1 },
            group(1, 0),
            group(2, 1),
            host(10, 2),
        ];
        assert_eq!(shape_strings(&rows), ["-", "-", "└", ".└"]);
    }

    #[test]
    fn real_visible_rows_get_lines_that_match_the_tree() {
        use std::collections::BTreeSet;
        use zssh_core::model::{AuthMethod, Group, Host, ProxyMode};
        let mk_host = |id: i64, group: Option<i64>, name: &str| Host {
            protocol: zssh_core::model::ProtocolSettings::default(),
            id,
            group_id: group,
            name: name.to_owned(),
            host: format!("192.0.2.{id}"),
            port: 22,
            username: "ops".to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: None,
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: ProxyMode::Default,
            tags: Vec::new(),
            sort: 0,
            legacy_algorithms: false,
            created_at: 0,
            updated_at: 0,
        };
        let mk_group = |id: i64, parent: Option<i64>, name: &str| Group {
            id,
            name: name.to_owned(),
            parent_id: parent,
            sort: 0,
        };
        let groups = vec![mk_group(1, None, "prod"), mk_group(2, Some(1), "db")];
        let hosts = vec![
            mk_host(10, Some(1), "web-1"),
            mk_host(11, Some(2), "db-1"),
            mk_host(12, None, "tmp-1"),
        ];
        let open: BTreeSet<String> = ["g1", "g2", "ungrouped"]
            .iter()
            .map(|k| (*k).to_owned())
            .collect();
        let rows = super::super::host_tree::visible_rows(&groups, &hosts, None, &open, false);
        // 全部主机 / prod / db(子分组) / db-1 / web-1 / 未分组 / tmp-1
        assert_eq!(shape_strings(&rows), ["-", "-", "├", "|└", "└", "-", "└"]);
    }

    #[test]
    fn the_constants_match_the_sidebar_rows() {
        assert_eq!(ROW_HEIGHT, super::super::sidebar::ROW_HEIGHT);
        assert_eq!(INDENT, super::super::sidebar::INDENT);
    }

    #[test]
    fn lines_sit_on_the_centre_of_the_parents_expand_arrow() {
        // 顶层分组的箭头框 [8, 24]，中心 16；下一层缩进 16，中心 32。
        assert_eq!(guide_x(0), 16.0);
        assert_eq!(guide_x(1), 32.0);
        assert_eq!(guide_x(2), 48.0);
    }

    #[test]
    fn a_branch_row_draws_the_full_height_line_and_a_corner_row_stops_at_the_stub() {
        let branch = RowGuide {
            depth: 1,
            last: false,
            trail: vec![],
            kind: Leaf::Host,
        };
        let corner = RowGuide {
            last: true,
            ..branch.clone()
        };
        let own = |g: &RowGuide| segments(g)[0];
        // ├ 穿过行间缝，和下一行接上。
        assert_eq!(own(&branch).h, ROW_PITCH);
        // └ 只到横线所在那一行（含横线的 1px）。
        assert_eq!(own(&corner).h, ROW_HEIGHT / 2.0 + 1.0);
        // 横线在行的垂直中间，从竖线起，到主机图标左边留一点空。
        let stub = *segments(&corner).last().expect("横线");
        assert!(!stub.is_vertical());
        assert_eq!((stub.x, stub.y), (guide_x(0), ROW_HEIGHT / 2.0));
        assert!(stub.x + stub.w < ROW_PAD + INDENT + TOGGLE + ICON_GAP);
        // 子分组的横线更短：只到它的展开箭头。
        let group_stub = *segments(&RowGuide {
            kind: Leaf::Group,
            ..corner
        })
        .last()
        .expect("横线");
        assert!(group_stub.w < stub.w);
    }

    #[test]
    fn ancestor_trail_lines_are_full_height_at_the_right_columns() {
        let guide = RowGuide {
            depth: 3,
            last: true,
            trail: vec![true, false],
            kind: Leaf::Host,
        };
        let verticals: Vec<Segment> = segments(&guide)
            .into_iter()
            .filter(Segment::is_vertical)
            .collect();
        // 第 1 层祖先有后续兄弟 → level 0 的竖线整行高；第 2 层没有；再加自己（level 2）的 └。
        assert_eq!(verticals.len(), 2);
        assert_eq!(verticals[0].x, guide_x(0));
        assert_eq!(verticals[0].h, ROW_PITCH);
        assert_eq!(verticals[1].x, guide_x(2));
    }

    #[test]
    fn dotted_lines_continue_across_rows_without_a_hiccup() {
        // 把几行的竖线点放到列表绝对坐标里：点的间距始终是 PITCH，包括行与行的接缝。
        let branch = RowGuide {
            depth: 1,
            last: false,
            trail: vec![],
            kind: Leaf::Group,
        };
        let mut absolute: Vec<f32> = Vec::new();
        for at in 0..6 {
            let top = at as f32 * ROW_PITCH;
            let vertical = segments(&branch)[0];
            absolute.extend(dots(&vertical, top).into_iter().map(|(_, y)| top + y));
        }
        assert!(absolute.len() > 50);
        for pair in absolute.windows(2) {
            assert_eq!(pair[1] - pair[0], PITCH, "{pair:?}");
        }
    }

    #[test]
    fn a_dotted_line_has_no_dot_outside_its_segment() {
        let guide = RowGuide {
            depth: 2,
            last: true,
            trail: vec![true],
            kind: Leaf::Host,
        };
        for top in [0.0, ROW_PITCH, 5.0 * ROW_PITCH] {
            for segment in segments(&guide) {
                for (x, y) in dots(&segment, top) {
                    assert!(x >= segment.x && x < segment.x + segment.w.max(1.0));
                    assert!(y >= segment.y && y < segment.y + segment.h.max(1.0));
                }
            }
        }
        // 横线至少有起点那个点，和竖线在拐角接上。
        let corner = RowGuide {
            depth: 1,
            last: true,
            trail: vec![],
            kind: Leaf::Group,
        };
        let stub = *segments(&corner).last().expect("横线");
        assert_eq!(dots(&stub, 0.0)[0], (stub.x, stub.y));
    }

    #[test]
    fn the_canvas_covers_every_dot() {
        let guide = RowGuide {
            depth: 3,
            last: false,
            trail: vec![true, true],
            kind: Leaf::Host,
        };
        let width = canvas_width(&guide);
        for (x, y) in row_dots(&guide, 0.0) {
            assert!(x + 1.0 <= width && y + 1.0 <= ROW_PITCH);
        }
    }
}
