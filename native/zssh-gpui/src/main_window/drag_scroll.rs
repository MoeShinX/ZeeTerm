//! 拖动主机 / 分组时，指针靠近滚动区上沿 / 下沿就自动滚动（侧栏主机树、主机页共用这一套）。
//!
//! gpui 只在鼠标移动时给 `on_drag_move`，指针停在边缘不动就没有事件，所以滚动靠一个"帧回调"：
//! 指针进了感应带才挂上，每帧按指针现在的位置算速度、滚一点、再挂下一帧；条件不满足（松手 / Esc /
//! 拖出窗口 / 离开感应带 / 已经滚到头）就不再挂，不常驻。
//!
//! 滚动之后行在指针下面换了，但指针没动就没有鼠标事件，插入线会指着旧行。每帧在滚动前先补发一个
//! 位置不变的鼠标移动：它走的是各行原来的 `on_drag_move`，按上一帧已经画好的新位置重新算落点，
//! 不用另写一份命中计算。
//!
//! 纯计算（速度、偏移、这一帧该不该滚）放在最上面，有单元测试；下面才是接 gpui 的部分。

use std::time::Instant;

use ::gpui::{
    App, Bounds, MouseButton, MouseMoveEvent, Pixels, PlatformInput, Point, ScrollHandle,
    WeakEntity, Window, point, px,
};
use gpui_kit::*;

use super::MainWindow;

/// 感应带的高度（贴着滚动区上沿 / 下沿）。
pub const EDGE_BAND: f32 = 32.0;
/// 带的内侧边缘的速度：每帧（按 60 帧算）滚多少像素。
pub const MIN_STEP: f32 = 2.0;
/// 贴着边缘（或已经拖出边缘）的速度。
pub const MAX_STEP: f32 = 16.0;
/// 速度按时间折算：掉帧时一帧滚得多一些，总速度不变；但一帧最多折算这么久，免得卡一下后猛冲。
const FRAME_SECS: f32 = 1.0 / 60.0;
const MAX_FRAME_SECS: f32 = 0.05;

/// 滚动区在窗口里的矩形（逻辑像素）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Area {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Area {
    pub fn from_bounds(bounds: Bounds<Pixels>) -> Self {
        Self {
            left: f32::from(bounds.origin.x),
            top: f32::from(bounds.origin.y),
            right: f32::from(bounds.origin.x + bounds.size.width),
            bottom: f32::from(bounds.origin.y + bounds.size.height),
        }
    }
}

/// 指针在 `(x, y)` 时每帧该往下滚多少像素：正 = 往下（看后面的内容），负 = 往上，0 = 不滚。
///
/// 横向要落在滚动区里（拖去别处就不滚）；纵向进了上 / 下沿的感应带才滚，越靠近边缘越快，
/// 拖出边缘之后按最快算（侧栏上面是搜索框，往上拖过头很常见）。
pub fn edge_step(x: f32, y: f32, area: Area) -> f32 {
    if !(area.left..=area.right).contains(&x) {
        return 0.0;
    }
    // 区域矮于两条带时每条带只占一半，免得上下沿的感应带重叠。
    let band = EDGE_BAND.min((area.bottom - area.top) / 2.0);
    if band <= 0.0 {
        return 0.0;
    }
    let speed = |depth: f32| MIN_STEP + (MAX_STEP - MIN_STEP) * depth.clamp(0.0, 1.0);
    if y < area.top + band {
        -speed((area.top + band - y) / band)
    } else if y > area.bottom - band {
        speed((y - (area.bottom - band)) / band)
    } else {
        0.0
    }
}

/// 按经过的时间折算这一帧的步长（`dt` 单位秒）。
pub fn scaled_step(step: f32, dt: f32) -> f32 {
    step * dt.clamp(0.0, MAX_FRAME_SECS) / FRAME_SECS
}

/// 往下滚 `step` 之后的偏移。偏移是 ≤ 0 的数（0 = 在顶部），`max` 是最大可滚距离。
pub fn next_offset(offset: f32, max: f32, step: f32) -> f32 {
    (offset - step).max(-max.max(0.0)).min(0.0)
}

/// 这一帧的结论。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tick {
    /// 不用再滚了（也不用再挂下一帧）。
    Stop,
    /// 滚到这个偏移，并挂下一帧。
    Scroll { offset: f32 },
}

/// 这一帧该不该滚、滚到哪。`dragging` 为假（松手 / Esc / 拖出窗口）一律停；滚到头也停。
pub fn plan_tick(
    dragging: bool,
    pointer: (f32, f32),
    area: Area,
    offset: f32,
    max: f32,
    dt: f32,
) -> Tick {
    if !dragging {
        return Tick::Stop;
    }
    let step = edge_step(pointer.0, pointer.1, area);
    if step == 0.0 {
        return Tick::Stop;
    }
    let next = next_offset(offset, max, scaled_step(step, dt));
    if (next - offset).abs() < f32::EPSILON {
        Tick::Stop
    } else {
        Tick::Scroll { offset: next }
    }
}

/// 滚动区还能不能往 `step` 的方向动（滚到头了就不必挂帧回调）。
fn can_move(offset: f32, max: f32, step: f32) -> bool {
    next_offset(offset, max, step) != offset
}

/// 哪一块滚动区。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollArea {
    /// 左边的主机树。
    Tree,
    /// 右边的主机页（卡片 / 行）。
    Page,
}

/// 自动滚动的状态：两块滚动区的句柄，加"现在是哪块在滚"。
pub struct DragScroll {
    pub tree: ScrollHandle,
    pub page: ScrollHandle,
    active: Option<ScrollArea>,
    last_tick: Instant,
}

impl Default for DragScroll {
    fn default() -> Self {
        Self {
            tree: ScrollHandle::new(),
            page: ScrollHandle::new(),
            active: None,
            last_tick: Instant::now(),
        }
    }
}

impl DragScroll {
    fn handle(&self, area: ScrollArea) -> &ScrollHandle {
        match area {
            ScrollArea::Tree => &self.tree,
            ScrollArea::Page => &self.page,
        }
    }

    /// 现在有没有一个在跑的帧回调（调试驱动和测试用）。
    #[cfg(debug_assertions)]
    pub fn running(&self) -> bool {
        self.active.is_some()
    }
}

/// 指针在窗口里（拖到窗口外时 Windows 仍会把坐标发过来，要自己判）。
fn inside_window(window: &Window, pointer: Point<Pixels>) -> bool {
    let size = window.viewport_size();
    pointer.x >= px(0.0)
        && pointer.y >= px(0.0)
        && pointer.x <= size.width
        && pointer.y <= size.height
}

impl MainWindow {
    /// 滚动区上的 `on_drag_move` 调这个（拖主机或分组时，指针每动一下）：进了感应带就挂上帧回调。
    pub(super) fn drag_scroll_probe(
        &mut self,
        area: ScrollArea,
        pointer: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let handle = self.drag_scroll.handle(area).clone();
        let step = if inside_window(window, pointer) {
            edge_step(
                f32::from(pointer.x),
                f32::from(pointer.y),
                Area::from_bounds(handle.bounds()),
            )
        } else {
            0.0
        };
        let movable = step != 0.0
            && can_move(
                f32::from(handle.offset().y),
                f32::from(handle.max_offset().y),
                step,
            );
        if !movable {
            if self.drag_scroll.active == Some(area) {
                self.drag_scroll.active = None;
            }
            return;
        }
        if self.drag_scroll.active.is_none() {
            self.drag_scroll.last_tick = Instant::now();
            schedule(cx.entity().downgrade(), window);
        }
        self.drag_scroll.active = Some(area);
    }

    /// 一帧：返回 `true` = 还要再挂下一帧。
    fn drag_scroll_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(area) = self.drag_scroll.active else {
            return false;
        };
        let handle = self.drag_scroll.handle(area).clone();
        let pointer = window.mouse_position();
        let now = Instant::now();
        let dt = now
            .saturating_duration_since(self.drag_scroll.last_tick)
            .as_secs_f32();
        self.drag_scroll.last_tick = now;
        let offset = handle.offset();
        let plan = plan_tick(
            cx.has_active_drag() && inside_window(window, pointer),
            (f32::from(pointer.x), f32::from(pointer.y)),
            Area::from_bounds(handle.bounds()),
            f32::from(offset.y),
            f32::from(handle.max_offset().y),
            dt,
        );
        match plan {
            Tick::Scroll { offset: next } => {
                handle.set_offset(point(offset.x, px(next)));
                cx.notify();
                true
            }
            Tick::Stop => {
                self.drag_scroll.active = None;
                // 松手 / Esc 没有"落下"事件：把遗留的落点提示清掉。
                self.clear_stale_hint(cx);
                cx.notify();
                false
            }
        }
    }
}

/// 挂下一帧的回调。回调里先补发鼠标移动（要在实体借用之外发，监听器里会再借主窗口），再滚。
fn schedule(weak: WeakEntity<MainWindow>, window: &mut Window) {
    window.on_next_frame(move |window, cx: &mut App| {
        if cx.has_active_drag() {
            let position = window.mouse_position();
            window.dispatch_event(
                PlatformInput::MouseMove(MouseMoveEvent {
                    position,
                    pressed_button: Some(MouseButton::Left),
                    modifiers: window.modifiers(),
                }),
                cx,
            );
        }
        let more = weak
            .update(cx, |this, cx| this.drag_scroll_tick(window, cx))
            .unwrap_or(false);
        if more {
            schedule(weak, window);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{
        Area, EDGE_BAND, MAX_STEP, MIN_STEP, Tick, can_move, edge_step, next_offset, plan_tick,
        scaled_step,
    };

    const AREA: Area = Area {
        left: 0.0,
        top: 100.0,
        right: 200.0,
        bottom: 500.0,
    };

    #[test]
    fn outside_the_bands_does_not_scroll() {
        assert_eq!(edge_step(100.0, 300.0, AREA), 0.0);
        // 带的内侧边界上也算带外。
        assert_eq!(edge_step(100.0, 100.0 + EDGE_BAND, AREA), 0.0);
        assert_eq!(edge_step(100.0, 500.0 - EDGE_BAND, AREA), 0.0);
        // 横向不在滚动区里：拖去别处了。
        assert_eq!(edge_step(-1.0, 101.0, AREA), 0.0);
        assert_eq!(edge_step(201.0, 499.0, AREA), 0.0);
    }

    #[test]
    fn directions_follow_the_edge() {
        assert!(edge_step(100.0, 110.0, AREA) < 0.0);
        assert!(edge_step(100.0, 490.0, AREA) > 0.0);
    }

    #[test]
    fn the_closer_to_the_edge_the_faster_within_the_limits() {
        let far = edge_step(100.0, 100.0 + EDGE_BAND - 1.0, AREA).abs();
        let mid = edge_step(100.0, 116.0, AREA).abs();
        let edge = edge_step(100.0, 100.0, AREA).abs();
        assert!(far >= MIN_STEP && far < mid && mid < edge);
        assert_eq!(edge, MAX_STEP);
        // 拖出边缘仍是最快，不会更快。
        assert_eq!(edge_step(100.0, 40.0, AREA), -MAX_STEP);
        assert_eq!(edge_step(100.0, 900.0, AREA), MAX_STEP);
        // 下沿同理。
        assert_eq!(edge_step(100.0, 500.0, AREA), MAX_STEP);
    }

    #[test]
    fn a_short_area_does_not_overlap_its_bands() {
        let short = Area {
            top: 0.0,
            bottom: 40.0,
            ..AREA
        };
        // 正中间：上下两条带各占一半，刚好都不算。
        assert_eq!(edge_step(100.0, 20.0, short), 0.0);
        assert!(edge_step(100.0, 5.0, short) < 0.0);
        assert!(edge_step(100.0, 35.0, short) > 0.0);
        let flat = Area {
            top: 10.0,
            bottom: 10.0,
            ..AREA
        };
        assert_eq!(edge_step(100.0, 10.0, flat), 0.0);
    }

    #[test]
    fn the_step_follows_elapsed_time_with_a_cap() {
        assert!((scaled_step(10.0, 1.0 / 60.0) - 10.0).abs() < 1e-3);
        assert!((scaled_step(10.0, 2.0 / 60.0) - 20.0).abs() < 1e-3);
        // 卡住很久之后只按上限折算。
        assert!((scaled_step(10.0, 5.0) - 30.0).abs() < 1e-3);
        assert_eq!(scaled_step(10.0, -1.0), 0.0);
    }

    #[test]
    fn the_offset_stops_at_the_top_and_the_bottom() {
        assert_eq!(next_offset(-5.0, 300.0, -16.0), 0.0);
        assert_eq!(next_offset(0.0, 300.0, -16.0), 0.0);
        assert_eq!(next_offset(-295.0, 300.0, 16.0), -300.0);
        assert_eq!(next_offset(-300.0, 300.0, 16.0), -300.0);
        assert_eq!(next_offset(0.0, 0.0, 16.0), 0.0);
        assert_eq!(next_offset(-10.0, 300.0, 4.0), -14.0);
        assert!(!can_move(-300.0, 300.0, 4.0));
        assert!(!can_move(0.0, 300.0, -4.0));
        assert!(can_move(-100.0, 300.0, 4.0));
        assert!(can_move(-100.0, 300.0, -4.0));
    }

    #[test]
    fn a_stationary_pointer_in_the_band_keeps_scrolling_until_the_end() {
        let dt = 1.0 / 60.0;
        let mut offset = 0.0;
        let mut frames = 0;
        // 指针一直停在下沿：一帧一帧滚下去，直到滚到底就停，不会无限继续。
        while let Tick::Scroll { offset: next } =
            plan_tick(true, (100.0, 500.0), AREA, offset, 100.0, dt)
        {
            assert!(next < offset);
            offset = next;
            frames += 1;
            assert!(frames < 100, "should have reached the end");
        }
        assert_eq!(offset, -100.0);
        // 到底之后再问还是停。
        assert_eq!(
            plan_tick(true, (100.0, 500.0), AREA, offset, 100.0, dt),
            Tick::Stop
        );
        // 往回拖到上沿又能滚回去。
        assert!(matches!(
            plan_tick(true, (100.0, 100.0), AREA, offset, 100.0, dt),
            Tick::Scroll { offset: next } if next > offset
        ));
    }

    #[test]
    fn it_stops_when_the_drag_ends_or_the_pointer_leaves_the_band() {
        let dt = 1.0 / 60.0;
        // 松手 / Esc / 拖出窗口：dragging 为假。
        assert_eq!(
            plan_tick(false, (100.0, 500.0), AREA, -10.0, 100.0, dt),
            Tick::Stop
        );
        // 指针回到带外。
        assert_eq!(
            plan_tick(true, (100.0, 300.0), AREA, -10.0, 100.0, dt),
            Tick::Stop
        );
        // 指针拖去了别的区域。
        assert_eq!(
            plan_tick(true, (400.0, 500.0), AREA, -10.0, 100.0, dt),
            Tick::Stop
        );
    }
}
