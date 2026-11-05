//! 拖标签出窗口 / 拖到别的窗口的标签栏上合并（V2-U1 第 7 条）的公共部分。
//!
//! 为什么不用 GPUI 自带的拖放（`on_drag` / `on_drop`）：它只在一个窗口里有效，指针一出窗口就没人
//! 收事件了。Windows 上按下鼠标时窗口会捕获鼠标，指针出了窗口仍然给这个窗口发移动和抬起事件
//! （坐标是相对这个窗口的，可以是负数或超出窗口大小），所以这里用窗口级的鼠标监听
//! （`Window::on_mouse_event`，不受「指针是不是在某个元素上」限制）自己做：
//!
//! 1. 在标签上按下左键：记下 [`Press`]（标签、标题、起点），还没算拖动；
//! 2. 指针离起点超过 [`START_DISTANCE`]：开始拖动，全局状态 [`DragView`] 记下来（各窗口画反馈）；
//! 3. 每次移动：按窗口位置 + 指针位置算出屏幕坐标，判断指针现在是在本窗口里、窗口外的空地，还是
//!    别的窗口的标签栏上（[`Hover`]）；
//! 4. 松手：在窗口外空地 = 弹出成独立窗口；在别的窗口标签栏上 = 合并过去；在本窗口里 = 什么也不做；
//! 5. Esc：取消。
//!
//! 判断「落在哪个窗口的标签栏」的几何是纯函数（[`strip_hit`] 等），有单测；真正交接标签的是各窗口
//! 自己（终端标签交的是同一个 `Tab`，SFTP 标签交的是同一个视图实体，会话不重连、传输不中断）。

use ::gpui::{AnyWindowHandle, App, Global, Window, WindowId};

use crate::i18n::{Msg, t};

/// 本文件（标签拖拽）的界面文案。
#[derive(Clone, Copy, Debug)]
enum TabDragMsg {
    PopOut,
    IntoFileWindow,
    MergeIntoWindow,
}

impl Msg for TabDragMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::PopOut => "松开鼠标：弹出成独立窗口 · Esc 取消",
            Self::IntoFileWindow => "松开鼠标：放进文件窗口 · Esc 取消",
            Self::MergeIntoWindow => "松开鼠标：合并到那个窗口 · Esc 取消",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::PopOut => "Release to open in its own window · Esc to cancel",
            Self::IntoFileWindow => "Release to move into the file window · Esc to cancel",
            Self::MergeIntoWindow => "Release to merge into that window · Esc to cancel",
        }
    }
}

/// 指针离按下的地方超过这么多像素才算拖动（再小是手抖的点击）。
pub const START_DISTANCE: f32 = 6.0;

/// 拖的是什么标签。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabKind {
    Terminal,
    Sftp,
}

/// 标签在它自己窗口里的编号。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragKey {
    /// 终端标签：工作区里的标签 id。
    Terminal(u64),
    /// 文件（SFTP）标签：主窗口 / 文件窗口里的标签 id。
    Sftp(u64),
}

impl DragKey {
    pub fn kind(self) -> TabKind {
        match self {
            Self::Terminal(_) => TabKind::Terminal,
            Self::Sftp(_) => TabKind::Sftp,
        }
    }
}

/// 指针现在在哪儿（相对被拖标签所在的窗口）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hover {
    /// 还在自己窗口里。
    Inside,
    /// 在窗口外的空地：松手会弹出成独立窗口。
    Outside,
    /// 在另一个窗口的标签栏上：松手会合并过去。
    Over(WindowId),
}

/// 正在拖动的东西（各窗口读它画反馈）。
#[derive(Clone, Debug)]
pub struct DragView {
    pub source: WindowId,
    pub key: DragKey,
    pub title: String,
    pub hover: Hover,
    /// 指针在源窗口里的位置（窗口里画跟着走的标签提示用）。
    pub pos: (f32, f32),
}

#[derive(Default)]
struct DragGlobal(Option<DragView>);

impl Global for DragGlobal {}

/// 现在有没有正在进行的标签拖动，是什么。
pub fn current(cx: &App) -> Option<DragView> {
    cx.try_global::<DragGlobal>()?.0.clone()
}

fn set(cx: &mut App, view: Option<DragView>) {
    cx.set_global(DragGlobal(view));
    // 每个窗口都要按新状态重画（目标窗口的标签栏要亮起来）。
    cx.refresh_windows();
}

/// 一个窗口在屏幕上的矩形和它标签栏的高度（顶部这一条算「标签栏」）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WinRect {
    pub id: WindowId,
    pub origin: (f32, f32),
    pub size: (f32, f32),
    pub strip_height: f32,
}

/// 屏幕上的点在不在某个窗口的标签栏里；重叠的窗口取列表里靠前的（调用方按层叠从上到下排）。
pub fn strip_hit(windows: &[WinRect], screen: (f32, f32)) -> Option<WindowId> {
    windows
        .iter()
        .find(|rect| {
            let (x, y) = (screen.0 - rect.origin.0, screen.1 - rect.origin.1);
            x >= 0.0 && y >= 0.0 && x < rect.size.0 && y < rect.strip_height.min(rect.size.1)
        })
        .map(|rect| rect.id)
}

/// 指针（相对窗口的坐标）离按下的点远不远（超过 [`START_DISTANCE`]）。
pub fn exceeds(start: (f32, f32), now: (f32, f32)) -> bool {
    let (dx, dy) = (now.0 - start.0, now.1 - start.1);
    dx * dx + dy * dy > START_DISTANCE * START_DISTANCE
}

/// 窗口相对坐标是不是在窗口里面。
pub fn inside(local: (f32, f32), size: (f32, f32)) -> bool {
    local.0 >= 0.0 && local.1 >= 0.0 && local.0 < size.0 && local.1 < size.1
}

/// 指针此刻的归属：自己窗口里、别的窗口标签栏上、还是窗口外的空地。
pub fn classify(local: (f32, f32), size: (f32, f32), over_other: Option<WindowId>) -> Hover {
    if inside(local, size) {
        Hover::Inside
    } else if let Some(id) = over_other {
        Hover::Over(id)
    } else {
        Hover::Outside
    }
}

/// 松手之后要做什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drop {
    /// 弹出成独立窗口。
    TearOff,
    /// 合并到这个窗口。
    Merge(WindowId),
    /// 什么也不做（还在自己窗口里 / 没拖起来 / 按了 Esc）。
    Nothing,
}

pub fn decide(hover: Hover) -> Drop {
    match hover {
        Hover::Inside => Drop::Nothing,
        Hover::Outside => Drop::TearOff,
        Hover::Over(id) => Drop::Merge(id),
    }
}

/// 一个窗口自己的拖动状态机：按下 -> 拖动 -> 松手 / 取消。三种窗口（主窗口、终端窗口、文件窗口）
/// 各持一份，行为一致。
#[derive(Default)]
pub struct Dragger {
    press: Option<Press>,
}

struct Press {
    key: DragKey,
    title: String,
    start: (f32, f32),
    began: bool,
}

impl Dragger {
    /// 在标签上按下左键。
    pub fn press(&mut self, key: DragKey, title: String, pos: (f32, f32)) {
        self.press = Some(Press {
            key,
            title,
            start: pos,
            began: false,
        });
    }

    /// 有没有按下的标签（窗口据此决定要不要挂窗口级的鼠标监听）。
    pub fn pending(&self) -> bool {
        self.press.is_some()
    }

    /// 被拖的标签（拖动中才有）。
    pub fn key(&self) -> Option<DragKey> {
        self.press
            .as_ref()
            .filter(|press| press.began)
            .map(|press| press.key)
    }

    /// 指针移动（`local` 是相对本窗口的坐标，`left_down` 是左键还按着）。返回要不要重画。
    pub fn moved(
        &mut self,
        local: (f32, f32),
        left_down: bool,
        window: &Window,
        cx: &mut App,
    ) -> bool {
        match self.step(local, left_down) {
            Step::Idle | Step::Hold => return false,
            Step::Cancel => {
                // 没收到抬起事件（比如鼠标在别处被抬起）：当作取消。
                self.cancel(cx);
                return true;
            }
            Step::Drag => {}
        }
        let Some(press) = self.press.as_ref() else {
            return false;
        };
        let source = id_of(window);
        let hover = hover_of(window, local, cx);
        let view = DragView {
            source,
            key: press.key,
            title: press.title.clone(),
            hover,
            pos: local,
        };
        if current(cx).as_ref().map(|old| (old.hover, old.pos)) != Some((hover, local)) {
            set(cx, Some(view));
        }
        true
    }

    /// 一次指针移动对按下状态的影响（纯状态，不碰窗口）：没按下 / 左键已松开 / 还没离开起点够远 / 在拖。
    /// 双击标签（按下、抬起、再按下、再抬起，指针不动）里每一次移动都落在 `Hold`，永远不会进入拖动。
    fn step(&mut self, local: (f32, f32), left_down: bool) -> Step {
        let Some(press) = self.press.as_mut() else {
            return Step::Idle;
        };
        if !left_down {
            return Step::Cancel;
        }
        if !press.began {
            if !exceeds(press.start, local) {
                return Step::Hold;
            }
            press.began = true;
        }
        Step::Drag
    }

    /// 松手：返回要做什么（以及被拖的标签）；没拖起来返回 `None`。状态清掉。
    pub fn release(&mut self, cx: &mut App) -> Option<(DragKey, Drop)> {
        let press = self.press.take()?;
        let view = current(cx);
        if view.is_some() {
            set(cx, None);
        }
        if !press.began {
            return None;
        }
        let hover = view.map_or(Hover::Inside, |view| view.hover);
        Some((press.key, decide(hover)))
    }

    /// Esc / 失去捕获：取消。
    pub fn cancel(&mut self, cx: &mut App) {
        if self.press.take().is_some() && current(cx).is_some() {
            set(cx, None);
        }
    }
}

/// [`Dragger::step`] 的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Idle,
    Cancel,
    Hold,
    Drag,
}

/// 指针在哪：本窗口里、别的窗口的标签栏上、窗口外的空地。
fn hover_of(window: &Window, local: (f32, f32), cx: &mut App) -> Hover {
    let own = window.bounds();
    let size = (f32::from(own.size.width), f32::from(own.size.height));
    if inside(local, size) {
        return Hover::Inside;
    }
    let screen = (
        f32::from(own.origin.x) + local.0,
        f32::from(own.origin.y) + local.1,
    );
    let others = other_windows(cx, id_of(window));
    let rects: Vec<WinRect> = others.iter().map(|(_, rect)| *rect).collect();
    classify(local, size, strip_hit(&rects, screen))
}

/// 应用里别的（我们自己的）窗口：句柄和它在屏幕上的矩形、标签栏高度。
pub fn other_windows(cx: &mut App, exclude: WindowId) -> Vec<(AnyWindowHandle, WinRect)> {
    let mut found: Vec<(AnyWindowHandle, f32)> = Vec::new();
    if let Some((handle, _)) = crate::file_window::main_window(cx) {
        found.push((handle, crate::main_window::titlebar::BAR_HEIGHT));
    }
    if let Some(handle) = crate::file_window::window_handle(cx) {
        found.push((handle.into(), crate::file_window::STRIP_HEIGHT));
    }
    for handle in crate::term_window::handles(cx) {
        found.push((handle.into(), crate::term_window::STRIP_HEIGHT));
    }
    found
        .into_iter()
        .filter(|(handle, _)| handle.window_id() != exclude)
        .filter_map(|(handle, strip_height)| {
            let bounds = handle.update(cx, |_, window, _| window.bounds()).ok()?;
            Some((
                handle,
                WinRect {
                    id: handle.window_id(),
                    origin: (f32::from(bounds.origin.x), f32::from(bounds.origin.y)),
                    size: (f32::from(bounds.size.width), f32::from(bounds.size.height)),
                    strip_height,
                },
            ))
        })
        .collect()
}

/// 窗口级的鼠标 / 按键监听：拖动期间（`active`）指针不管在不在某个元素上、在不在窗口里，都能收到移动
/// 和抬起事件（Windows 按下鼠标时窗口会捕获鼠标），Esc 取消。要放进窗口的元素树里（尺寸为 0，不挡
/// 任何东西）。没有按下的标签时不挂任何监听，平时没有开销。
pub fn sensor<V: 'static>(
    view: &::gpui::Entity<V>,
    active: bool,
    on_move: impl Fn(&mut V, &::gpui::MouseMoveEvent, &mut Window, &mut ::gpui::Context<V>)
    + Clone
    + 'static,
    on_up: impl Fn(&mut V, &::gpui::MouseUpEvent, &mut Window, &mut ::gpui::Context<V>)
    + Clone
    + 'static,
) -> impl ::gpui::IntoElement {
    use ::gpui::{DispatchPhase, MouseMoveEvent, MouseUpEvent, canvas, px};
    use gpui_kit::prelude::Styled as _;

    let view = view.clone();
    canvas(
        |_, _, _| (),
        move |_, _, window, _| {
            if !active {
                return;
            }
            let (moving, move_view) = (on_move.clone(), view.clone());
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                if phase == DispatchPhase::Capture {
                    move_view.update(cx, |this, cx| moving(this, event, window, cx));
                }
            });
            let (releasing, up_view) = (on_up.clone(), view.clone());
            window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                if phase == DispatchPhase::Capture {
                    up_view.update(cx, |this, cx| releasing(this, event, window, cx));
                }
            });
        },
    )
    .absolute()
    .size(px(0.0))
}

/// 窗口相对坐标 -> 屏幕坐标。
pub fn screen_point(window: &Window, local: (f32, f32)) -> (f32, f32) {
    let origin = window.bounds().origin;
    (f32::from(origin.x) + local.0, f32::from(origin.y) + local.1)
}

/// 拖动中指针出了窗口时的提示文字：松手会发生什么。还在窗口里不写。
pub fn hint_text(view: &DragView) -> Option<&'static str> {
    match (view.hover, view.key.kind()) {
        (Hover::Inside, _) => None,
        (Hover::Outside, TabKind::Terminal) => Some(t(TabDragMsg::PopOut)),
        (Hover::Outside, TabKind::Sftp) => Some(t(TabDragMsg::IntoFileWindow)),
        (Hover::Over(_), _) => Some(t(TabDragMsg::MergeIntoWindow)),
    }
}

/// 标签栏上的提示小标签（被拖的标签所在的窗口画它）。
pub fn hint_chip(
    window: &Window,
    cx: &App,
    tokens: &crate::app_theme::Tokens,
) -> Option<::gpui::AnyElement> {
    use ::gpui::{IntoElement as _, SharedString, px};
    use gpui_kit::prelude::Styled as _;
    use gpui_kit::*;

    let view = current(cx).filter(|view| view.source == id_of(window))?;
    let text = hint_text(&view)?;
    Some(
        div()
            .flex_none()
            .mb_2()
            .ml_2()
            .px_2()
            .h(px(22.0))
            .flex()
            .items_center()
            .rounded_md()
            .bg(tokens.accent)
            .text_color(tokens.on_accent)
            .text_xs()
            .child(SharedString::from(text))
            .into_any_element(),
    )
}

/// 指针还在窗口里时，跟着指针走的标签提示（被拖的标签标题）；出了窗口就不画（窗口外画不了）。
pub fn ghost_chip(
    window: &Window,
    cx: &App,
    tokens: &crate::app_theme::Tokens,
) -> Option<::gpui::AnyElement> {
    use ::gpui::{IntoElement as _, SharedString, px};
    use gpui_kit::prelude::Styled as _;
    use gpui_kit::*;

    let view = current(cx).filter(|view| view.source == id_of(window))?;
    if view.hover != Hover::Inside {
        return None;
    }
    Some(
        div()
            .absolute()
            .left(px(view.pos.0 + 12.0))
            .top(px(view.pos.1 + 12.0))
            .px_2()
            .h(px(26.0))
            .flex()
            .items_center()
            .rounded_md()
            .border_1()
            .border_color(tokens.accent)
            .bg(tokens.surface)
            .text_color(tokens.text)
            .text_sm()
            .opacity(0.9)
            .child(SharedString::from(view.title.clone()))
            .into_any_element(),
    )
}

/// 这个窗口的编号。
pub fn id_of(window: &Window) -> WindowId {
    window.window_handle().window_id()
}

#[cfg(test)]
mod tests {
    use ::gpui::WindowId;

    use super::{
        DragKey, Dragger, Drop, Hover, Step, WinRect, classify, decide, exceeds, inside, strip_hit,
    };

    fn id(n: u64) -> WindowId {
        WindowId::from(n)
    }

    fn rect(n: u64, x: f32, y: f32, w: f32, h: f32) -> WinRect {
        WinRect {
            id: id(n),
            origin: (x, y),
            size: (w, h),
            strip_height: 40.0,
        }
    }

    #[test]
    fn a_tiny_wobble_is_a_click_not_a_drag() {
        assert!(!exceeds((100.0, 20.0), (103.0, 22.0)));
        assert!(!exceeds((100.0, 20.0), (100.0, 26.0)), "正好 6 像素还不算");
        assert!(exceeds((100.0, 20.0), (100.0, 27.0)));
        assert!(exceeds((100.0, 20.0), (80.0, 20.0)), "往左也算");
    }

    #[test]
    fn a_double_click_on_a_tab_never_turns_into_a_drag() {
        // 按下、（手抖）小幅移动、抬起，再来一遍：全程在 6 像素以内，不会开始拖，标签不会被"拖走"。
        let mut dragger = Dragger::default();
        for _ in 0..2 {
            dragger.press(DragKey::Sftp(3), "web-1".into(), (100.0, 20.0));
            assert!(dragger.pending());
            assert_eq!(dragger.step((102.0, 21.0), true), Step::Hold);
            assert_eq!(dragger.step((100.0, 26.0), true), Step::Hold);
            assert_eq!(dragger.key(), None, "没开始拖就不该有被拖的标签");
            // 抬起：没拖起来，什么也不做。
            let press = dragger.press.take().expect("按下过");
            assert!(!press.began);
        }
    }

    #[test]
    fn moving_past_the_threshold_starts_the_drag_and_a_lost_button_cancels_it() {
        let mut dragger = Dragger::default();
        dragger.press(DragKey::Sftp(3), "web-1".into(), (100.0, 20.0));
        assert_eq!(dragger.step((100.0, 40.0), true), Step::Drag);
        assert_eq!(dragger.key(), Some(DragKey::Sftp(3)));
        assert_eq!(dragger.step((100.0, 45.0), false), Step::Cancel);
        assert_eq!(Dragger::default().step((1.0, 1.0), true), Step::Idle);
    }

    #[test]
    fn inside_is_the_window_rectangle_in_its_own_coordinates() {
        assert!(inside((0.0, 0.0), (800.0, 600.0)));
        assert!(inside((799.0, 599.0), (800.0, 600.0)));
        assert!(!inside((800.0, 100.0), (800.0, 600.0)));
        assert!(!inside((-1.0, 100.0), (800.0, 600.0)));
        assert!(!inside((10.0, -5.0), (800.0, 600.0)));
        assert!(!inside((10.0, 600.0), (800.0, 600.0)));
    }

    #[test]
    fn only_the_top_strip_of_another_window_accepts_a_drop() {
        let windows = [rect(2, 1000.0, 100.0, 600.0, 500.0)];
        // 在另一个窗口的标签栏里（相对它的 y 在 0..40）。
        assert_eq!(strip_hit(&windows, (1100.0, 120.0)), Some(id(2)));
        assert_eq!(strip_hit(&windows, (1000.0, 100.0)), Some(id(2)));
        // 在它的内容区里：不算（只有标签栏接收）。
        assert_eq!(strip_hit(&windows, (1100.0, 200.0)), None);
        // 在它外面。
        assert_eq!(strip_hit(&windows, (900.0, 120.0)), None);
        assert_eq!(strip_hit(&windows, (1700.0, 120.0)), None);
        assert_eq!(strip_hit(&windows, (1100.0, 90.0)), None, "在它上面");
    }

    #[test]
    fn overlapping_windows_pick_the_first_in_stacking_order() {
        let windows = [
            rect(3, 500.0, 100.0, 600.0, 500.0),
            rect(2, 400.0, 80.0, 600.0, 500.0),
        ];
        assert_eq!(strip_hit(&windows, (550.0, 120.0)), Some(id(3)));
    }

    #[test]
    fn the_drop_follows_where_the_pointer_is() {
        let size = (1000.0, 700.0);
        // 还在自己窗口里（哪怕正好也在别的窗口的标签栏范围里）。
        assert_eq!(classify((500.0, 20.0), size, Some(id(2))), Hover::Inside);
        // 出了窗口：落在别的窗口标签栏上 = 合并，否则 = 弹出。
        assert_eq!(
            classify((1200.0, 20.0), size, Some(id(2))),
            Hover::Over(id(2))
        );
        assert_eq!(classify((1200.0, 20.0), size, None), Hover::Outside);
        assert_eq!(classify((-30.0, 300.0), size, None), Hover::Outside);
        assert_eq!(decide(Hover::Inside), Drop::Nothing);
        assert_eq!(decide(Hover::Outside), Drop::TearOff);
        assert_eq!(decide(Hover::Over(id(2))), Drop::Merge(id(2)));
    }
}
