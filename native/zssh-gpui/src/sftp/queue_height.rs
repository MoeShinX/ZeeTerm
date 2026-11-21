//! 文件面板里「文件列表」和「传输队列」之间的拖动条（V2-U53）：高度规则，纯函数，不依赖 GPUI。
//!
//! - 队列内容区（头部下面那个带边框的框）没拖过时按内容高度、最高 [`NATURAL_MAX`]（和以前一模一样）；
//!   拖过之后用固定高度，这样分界线停在用户放的位置，任务少也不缩回去。
//! - 固定高度存本机设置 `layout.sftp_queue_height`（不同步；缺省 / 0 / 坏值 = 没拖过）。四种用法
//!   （标签整页、右侧停靠、左侧边栏、独立文件窗口）共用这一个值，各自按自己的可用高度夹取。
//! - 「可用高度」`room` = 文件列表区高度 + 队列内容区高度（这两块是视图里唯一会伸缩的部分，
//!   拖动只是在它们之间挪边界，所以这个和不随拖动变化，只随窗口 / 面板高度变）。

use zssh_core::store::Database;

/// 本机设置键。
pub const SETTING_KEY: &str = "layout.sftp_queue_height";
/// 没拖过时队列内容区最高多高（原来写死的 `max_h(224)`）。
pub const NATURAL_MAX: f32 = 224.0;
/// 拖过之后内容区的最小高度。
pub const CONTENT_MIN: f32 = 60.0;
/// 文件列表区（列表头 + 至少两行）至少留多高：夹取时列表优先，内容区让给它。
pub const LIST_MIN: f32 = 120.0;
/// 拖到内容区比这个还矮，松手就收起队列。
pub const COLLAPSE_BELOW: f32 = 40.0;
/// 存量值的硬顶：再大的当脏数据；量不到可用高度时也拿它当上限。
const STORED_HARD_MAX: f32 = 20_000.0;

/// 当前可用高度下内容区最高多高：保证文件列表不小于 [`LIST_MIN`]。
/// 可用高度还没量到（<= 0 / 非有限数）时只有硬顶；窗口矮到连列表的最小高度都不够时是 0。
pub fn max_content(room: f32) -> f32 {
    if !room.is_finite() || room <= 0.0 {
        return STORED_HARD_MAX;
    }
    (room - LIST_MIN).floor().max(0.0)
}

/// 夹取内容区高度：不小于 [`CONTENT_MIN`]、不大于 [`max_content`]。两者冲突（窗口很矮）时上限赢，
/// 也就是列表先保住，内容区变得比最小值还矮，最矮到 0。
pub fn clamp_content(desired: f32, room: f32) -> f32 {
    let desired = if desired.is_finite() {
        desired
    } else {
        NATURAL_MAX
    };
    desired
        .round()
        .max(CONTENT_MIN)
        .min(max_content(room))
        .max(0.0)
}

/// 夹取以后还有没有地方画内容区（矮到 0 就不画，免得只剩两条边框）。
pub fn has_room(height: f32) -> bool {
    height >= 1.0
}

/// 读回来的存量高度做体检：不是有限数、比最小值还小（含 0 和负数）、大得离谱的一律当没拖过。
pub fn sanitize_stored(raw: Option<f32>) -> Option<f32> {
    match raw {
        Some(v) if v.is_finite() && (CONTENT_MIN..=STORED_HARD_MAX).contains(&v) => Some(v.round()),
        _ => None,
    }
}

/// 把设置表里的 JSON 数字读出来；不是数字就是 `None`。
pub fn parse_stored(raw: Option<&str>) -> Option<f32> {
    raw.and_then(|text| serde_json::from_str::<serde_json::Value>(text.trim()).ok())
        .and_then(|value| value.as_f64())
        .map(|value| value as f32)
}

/// 从设置表读：缺省和坏值都是 `None`（没拖过）。
pub fn load(db: &Database) -> Option<f32> {
    sanitize_stored(parse_stored(
        db.setting_get(SETTING_KEY).ok().flatten().as_deref(),
    ))
}

/// 写进设置表。`None`（恢复默认）写 0：读回来当没拖过，设置表本来也没有删除接口。
pub fn save(db: &Database, height: Option<f32>) -> Result<(), String> {
    let value = height.map_or(0, |h| h.round() as i64);
    db.setting_set(SETTING_KEY, &value.to_string())
        .map_err(|err| crate::error_text::error_text(&err))
}

/// 队列现在该怎么摆：展开没有、内容区用固定高度还是按内容。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QueueState {
    pub open: bool,
    /// 拖出来的固定高度；`None` = 没拖过（按内容、最高 [`NATURAL_MAX`]）。
    pub stored: Option<f32>,
}

/// 拖动中的瞬时结果。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Drag {
    /// 展开，内容区这么高（已夹取）。
    Open(f32),
    /// 拖到最低：松手会收起。
    Collapsed,
}

/// 一次拖动。拖动条在队列的上沿，往上拖（y 变小）内容区变高；收起时起点高度是 0。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DragSession {
    start_y: f32,
    start_height: f32,
    live: Option<Drag>,
}

impl DragSession {
    /// `content_height`：按下时内容区实际多高（没展开就不用管，按 0 算）。
    pub fn begin(open: bool, content_height: f32, y: f32) -> Self {
        Self {
            start_y: y,
            start_height: if open && content_height.is_finite() {
                content_height.max(0.0)
            } else {
                0.0
            },
            live: None,
        }
    }

    /// 鼠标挪到 `y`：算出现在的样子并记下。
    pub fn update(&mut self, y: f32, room: f32) -> Drag {
        let desired = self.start_height + (self.start_y - y);
        let result = if !desired.is_finite() || desired < COLLAPSE_BELOW {
            Drag::Collapsed
        } else {
            Drag::Open(clamp_content(desired, room))
        };
        self.live = Some(result);
        result
    }

    /// 现在预览的样子；还没动过鼠标是 `None`（单击不改任何东西）。
    pub fn live(&self) -> Option<Drag> {
        self.live
    }

    /// 松手：拖动结果落成新状态。没动过 = 原样；拖到最低 = 收起并保留拖之前的高度（不记 0）。
    pub fn finish(&self, before: QueueState) -> QueueState {
        match self.live {
            None => before,
            Some(Drag::Open(height)) => QueueState {
                open: true,
                stored: Some(height),
            },
            Some(Drag::Collapsed) => QueueState {
                open: false,
                stored: before.stored,
            },
        }
    }
}

/// 双击拖动条：恢复默认高度（按内容、最高 [`NATURAL_MAX`]）；展开 / 收起不动。
pub fn reset(before: QueueState) -> QueueState {
    QueueState {
        stored: None,
        ..before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(open: bool, stored: Option<f32>) -> QueueState {
        QueueState { open, stored }
    }

    #[test]
    fn the_content_height_is_clamped_between_the_minimum_and_what_the_list_can_spare() {
        // 可用 600：内容区最高 600 - 120 = 480。
        assert_eq!(max_content(600.0), 480.0);
        assert_eq!(clamp_content(300.0, 600.0), 300.0);
        assert_eq!(clamp_content(10.0, 600.0), CONTENT_MIN);
        assert_eq!(clamp_content(9000.0, 600.0), 480.0);
        // 取整。
        assert_eq!(clamp_content(300.4, 600.0), 300.0);
    }

    #[test]
    fn a_tiny_room_keeps_the_list_and_never_overflows() {
        // 只够列表的最小高度：内容区 0，不画。
        assert_eq!(clamp_content(200.0, LIST_MIN), 0.0);
        assert!(!has_room(clamp_content(200.0, LIST_MIN)));
        // 比列表最小高度还矮的窗口也一样，不出负数。
        assert_eq!(clamp_content(200.0, 50.0), 0.0);
        // 只多出 30：内容区 30，比最小值还矮，列表优先。
        assert_eq!(clamp_content(200.0, LIST_MIN + 30.0), 30.0);
        // 刚好够最小值。
        assert_eq!(clamp_content(200.0, LIST_MIN + CONTENT_MIN), CONTENT_MIN);
        // 任何可用高度下结果都不超过 room - LIST_MIN，也不为负。
        for room in (1..=700).map(|r| r as f32) {
            for want in [-50.0, 0.0, 59.0, 60.0, 224.0, 900.0, f32::MAX] {
                let h = clamp_content(want, room);
                assert!(h >= 0.0, "room {room} want {want}");
                assert!(
                    h <= (room - LIST_MIN).max(0.0),
                    "room {room} want {want}: {h}"
                );
            }
        }
    }

    #[test]
    fn an_unmeasured_room_only_applies_the_minimum_and_the_hard_cap() {
        for room in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert_eq!(clamp_content(200.0, room), 200.0);
            assert_eq!(clamp_content(10.0, room), CONTENT_MIN);
        }
        assert_eq!(clamp_content(f32::MAX, 0.0), STORED_HARD_MAX);
    }

    #[test]
    fn a_non_finite_desired_height_falls_back_to_the_natural_cap() {
        assert_eq!(clamp_content(f32::NAN, 600.0), NATURAL_MAX);
        assert_eq!(clamp_content(f32::INFINITY, 600.0), NATURAL_MAX);
    }

    #[test]
    fn stored_values_that_make_no_sense_count_as_never_dragged() {
        assert_eq!(sanitize_stored(None), None);
        assert_eq!(sanitize_stored(Some(0.0)), None);
        assert_eq!(sanitize_stored(Some(-5.0)), None);
        assert_eq!(sanitize_stored(Some(CONTENT_MIN - 1.0)), None);
        assert_eq!(sanitize_stored(Some(f32::NAN)), None);
        assert_eq!(sanitize_stored(Some(f32::INFINITY)), None);
        assert_eq!(sanitize_stored(Some(1.0e9)), None);
        assert_eq!(sanitize_stored(Some(CONTENT_MIN)), Some(CONTENT_MIN));
        assert_eq!(sanitize_stored(Some(180.4)), Some(180.0));
        assert_eq!(
            sanitize_stored(Some(STORED_HARD_MAX)),
            Some(STORED_HARD_MAX)
        );
    }

    #[test]
    fn stored_text_is_read_leniently() {
        assert_eq!(parse_stored(Some("180")), Some(180.0));
        assert_eq!(parse_stored(Some(" 180.5 ")), Some(180.5));
        for bad in ["", "abc", "null", "\"180\"", "[1]", "{}", "NaN"] {
            assert_eq!(parse_stored(Some(bad)), None, "{bad:?}");
        }
        assert_eq!(parse_stored(None), None);
    }

    #[test]
    fn dragging_up_grows_the_content_and_dragging_down_shrinks_it() {
        let mut up = DragSession::begin(true, 150.0, 500.0);
        assert_eq!(up.update(450.0, 700.0), Drag::Open(200.0));
        let mut down = DragSession::begin(true, 150.0, 500.0);
        assert_eq!(down.update(540.0, 700.0), Drag::Open(110.0));
        // 回到起点就是起点高度。
        assert_eq!(down.update(500.0, 700.0), Drag::Open(150.0));
    }

    #[test]
    fn dragging_is_clamped_by_the_room_and_the_minimum() {
        let mut up = DragSession::begin(true, 150.0, 500.0);
        // 往上拖过头：列表至少留 120，可用 400 → 内容区最高 280。
        assert_eq!(up.update(-4000.0, 400.0), Drag::Open(280.0));
        // 往下拖一点点（仍高于收起阈值）：不小于最小值 60。
        let mut down = DragSession::begin(true, 150.0, 500.0);
        assert_eq!(
            down.update(500.0 + 150.0 - 45.0, 700.0),
            Drag::Open(CONTENT_MIN)
        );
    }

    #[test]
    fn dragging_a_collapsed_queue_up_opens_it_at_the_dragged_height() {
        // 收起时起点高度按 0：往上拖 100 → 100。
        let mut s = DragSession::begin(false, 999.0, 600.0);
        assert_eq!(s.update(500.0, 700.0), Drag::Open(100.0));
        // 只往上拖了一点（不到阈值）：仍是收起。
        let mut small = DragSession::begin(false, 0.0, 600.0);
        assert_eq!(small.update(590.0, 700.0), Drag::Collapsed);
        // 刚过阈值：展开，按最小值。
        let mut edge = DragSession::begin(false, 0.0, 600.0);
        assert_eq!(
            edge.update(600.0 - COLLAPSE_BELOW, 700.0),
            Drag::Open(CONTENT_MIN)
        );
    }

    #[test]
    fn a_collapsed_queue_opens_when_the_drag_ends_above_the_threshold() {
        let before = state(false, Some(180.0));
        let mut s = DragSession::begin(false, 0.0, 600.0);
        s.update(480.0, 700.0);
        assert_eq!(s.finish(before), state(true, Some(120.0)));
    }

    #[test]
    fn dragging_to_the_bottom_collapses_and_keeps_the_previous_height() {
        // 拖之前固定 180：拖到最低收起，仍记着 180，不记 0。
        let before = state(true, Some(180.0));
        let mut s = DragSession::begin(true, 180.0, 500.0);
        s.update(520.0, 700.0);
        assert_eq!(s.live(), Some(Drag::Open(160.0)));
        s.update(700.0, 700.0);
        assert_eq!(s.live(), Some(Drag::Collapsed));
        assert_eq!(s.finish(before), state(false, Some(180.0)));
        // 没拖过的（按内容）拖到最低：收起，仍是没拖过。
        let natural = state(true, None);
        let mut s = DragSession::begin(true, 90.0, 500.0);
        s.update(700.0, 700.0);
        assert_eq!(s.finish(natural), state(false, None));
        // 刚好在阈值上：40 不收起，39 收起。
        let mut at = DragSession::begin(true, 100.0, 500.0);
        assert_eq!(
            at.update(500.0 + 100.0 - COLLAPSE_BELOW, 700.0),
            Drag::Open(CONTENT_MIN)
        );
        assert_eq!(
            at.update(500.0 + 100.0 - COLLAPSE_BELOW + 1.0, 700.0),
            Drag::Collapsed
        );
    }

    #[test]
    fn a_drag_can_come_back_from_collapsed_before_release() {
        let before = state(true, Some(180.0));
        let mut s = DragSession::begin(true, 180.0, 500.0);
        s.update(690.0, 700.0);
        assert_eq!(s.live(), Some(Drag::Collapsed));
        s.update(480.0, 700.0);
        assert_eq!(s.finish(before), state(true, Some(200.0)));
    }

    #[test]
    fn a_click_without_moving_changes_nothing() {
        for before in [
            state(true, None),
            state(true, Some(150.0)),
            state(false, None),
        ] {
            let s = DragSession::begin(before.open, 100.0, 500.0);
            assert_eq!(s.live(), None);
            assert_eq!(s.finish(before), before);
        }
    }

    #[test]
    fn double_click_restores_the_default_without_touching_open_state() {
        assert_eq!(reset(state(true, Some(150.0))), state(true, None));
        assert_eq!(reset(state(false, Some(150.0))), state(false, None));
        assert_eq!(reset(state(true, None)), state(true, None));
    }

    #[test]
    fn the_height_is_saved_and_read_back_and_never_dragged_means_no_value() {
        let db = Database::open_in_memory().expect("内存库");
        assert_eq!(load(&db), None, "缺省 = 没拖过");
        save(&db, Some(180.0)).expect("写");
        assert_eq!(load(&db), Some(180.0));
        save(&db, Some(333.6)).expect("写");
        assert_eq!(load(&db), Some(334.0));
        // 恢复默认：写 0，读回来是没拖过。
        save(&db, None).expect("写");
        assert_eq!(load(&db), None);
    }

    #[test]
    fn bad_stored_values_read_back_as_never_dragged() {
        let db = Database::open_in_memory().expect("内存库");
        for bad in ["abc", "-40", "0", "59", "1e30", "\"200\"", "null", ""] {
            db.setting_set(SETTING_KEY, bad).expect("写");
            assert_eq!(load(&db), None, "{bad:?}");
        }
    }

    #[test]
    fn the_setting_is_local_only_and_never_synced() {
        assert_eq!(
            zssh_core::sync::settings::sync_mode(SETTING_KEY),
            Some(zssh_core::sync::settings::SyncMode::LocalOnly)
        );
        assert!(!zssh_core::sync::settings::is_synced(SETTING_KEY));
    }
}
