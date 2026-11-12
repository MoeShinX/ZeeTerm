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
//! 4. 松手：在窗口外空地 = 弹出成独立窗口；在别的窗口标签栏上 = 合并过去；在本窗口里 = 什么也不做
//!    （但指针在本窗口标签栏里时已经实时换过位了，见下，松手保留新顺序）；
//! 5. Esc：取消，换过位的话恢复成拖动开始前的顺序。
//!
//! 本窗口标签栏里的换位（像浏览器 / Xshell）：拖动中指针还在本窗口标签栏那一条里时，指针越过相邻标签
//! 的中线，被拖的标签就挪到那个位置，其他标签让开（[`reorder_target`]）。标签的左右边界不假设固定宽度，
//! 而是每帧画标签时记下真实位置（[`TabSpans`]）；换位后边界作废，等下一帧重新记下再算，免得拿着旧位置
//! 来回抖。换位只在同一段标签里进行（`keys` 由窗口按标签种类给），由各窗口自己落到自己的数据上。
//!
//! 判断「落在哪个窗口的标签栏」的几何是纯函数（[`strip_hit`] 等），有单测；真正交接标签的是各窗口
//! 自己（终端标签交的是同一个 `Tab`，SFTP 标签交的是同一个视图实体，会话不重连、传输不中断）。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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
    /// 指针在源窗口的标签栏那一条里：这时被拖的标签自己在栏里换位，不再画跟着指针的提示。
    pub in_strip: bool,
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

/// 一个标签在窗口里的左右边界（窗口坐标，已经算上标签条的滚动）。
pub type Span = (f32, f32);

/// 指针在标签栏那一条里吗（窗口相对坐标；标签栏是窗口顶部 `strip_height` 高的一条）。
pub fn in_strip(local: (f32, f32), size: (f32, f32), strip_height: f32) -> bool {
    inside(local, size) && local.1 < strip_height
}

/// 拖动换位：被拖的标签现在排第 `from` 个，指针横坐标 `pointer_x`，`spans` 是这一段标签现在的左右边界
/// （按排列顺序）。返回它该排第几个：指针越过哪个相邻标签的中线，就挪到那一个的位置；没越过（含正好压在
/// 中线上）不动。靠相邻标签的中线判断（不是被拖标签自己的边），所以宽窄不一的标签换过去之后，要换回来
/// 得越过对方换位后的中线，不会在边界上来回抖。指针在最左 / 最右之外就停在第一个 / 最后一个。
pub fn reorder_target(spans: &[Span], from: usize, pointer_x: f32) -> usize {
    if from >= spans.len() {
        return from;
    }
    let mid = |index: usize| (spans[index].0 + spans[index].1) / 2.0;
    let mut to = from;
    while to + 1 < spans.len() && pointer_x > mid(to + 1) {
        to += 1;
    }
    while to > 0 && pointer_x < mid(to - 1) {
        to -= 1;
    }
    to
}

/// 把 `current` 这一段标签的顺序还原成 `original`（Esc 取消换位）要做的挪动：一串 `(from, to)`，
/// 照顺序依次做 `move_item(from, to)` 就还原。拖动期间多出来 / 少掉的标签（连接新开、被关掉）不参与：
/// `original` 里已经没有的跳过，`current` 里多出来的留在最后。
pub fn restore_moves(current: &[DragKey], original: &[DragKey]) -> Vec<(usize, usize)> {
    let mut order = current.to_vec();
    let mut moves = Vec::new();
    let wanted = original.iter().filter(|key| current.contains(key));
    for (to, key) in wanted.enumerate() {
        let Some(from) = order.iter().position(|other| other == key) else {
            continue;
        };
        if from != to {
            let moved = order.remove(from);
            order.insert(to, moved);
            moves.push((from, to));
        }
    }
    moves
}

/// 各标签上一帧画出来的真实左右边界（每个窗口一份，由 [`Dragger`] 持有）。画标签时在每个标签里放一个
/// [`TabSpans::track`]，它在布局阶段把这个标签的位置记下来；换位时按需读取。
#[derive(Clone, Default)]
pub struct TabSpans(Rc<RefCell<HashMap<DragKey, Span>>>);

impl TabSpans {
    /// 放进标签元素里（标签要有 `relative()`）：尺寸跟标签一样、不挡任何东西，布局时记下标签的边界。
    pub fn track(&self, key: DragKey) -> impl ::gpui::IntoElement {
        use ::gpui::canvas;
        use gpui_kit::prelude::Styled as _;

        let spans = self.clone();
        canvas(
            move |bounds, _, _| {
                spans
                    .0
                    .borrow_mut()
                    .insert(key, (f32::from(bounds.left()), f32::from(bounds.right())));
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full()
    }

    /// `keys` 这一段标签的边界，按 `keys` 的顺序；有一个还没记下来（刚换过位、还没重画）、或者左边界
    /// 不是从左到右递增（记下的还是换位之前的）都返回 `None`，等下一帧。
    pub fn collect(&self, keys: &[DragKey]) -> Option<Vec<Span>> {
        let spans = self.0.borrow();
        let found: Vec<Span> = keys
            .iter()
            .map(|key| spans.get(key).copied())
            .collect::<Option<_>>()?;
        found
            .windows(2)
            .all(|pair| pair[0].0 <= pair[1].0)
            .then_some(found)
    }

    /// 换位之后边界作废：下一帧重画时重新记下。
    pub fn clear(&self) {
        self.0.borrow_mut().clear();
    }

    #[cfg(test)]
    fn set(&self, key: DragKey, span: Span) {
        self.0.borrow_mut().insert(key, span);
    }
}

/// 一次指针移动的结果。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Moved {
    /// 要重画。
    pub redraw: bool,
    /// 拖动在进行（已经越过起拖距离）：窗口据此去算要不要换位。
    pub dragging: bool,
    /// 左键其实已经松开（没收到抬起事件）被当成取消；换过位的话这里是拖动开始前的顺序，窗口要还原。
    pub restore: Option<Vec<DragKey>>,
}

/// 一个窗口自己的拖动状态机：按下 -> 拖动 -> 松手 / 取消。三种窗口（主窗口、终端窗口、文件窗口）
/// 各持一份，行为一致。
#[derive(Default)]
pub struct Dragger {
    press: Option<Press>,
    /// 本窗口标签栏的高度（指针在这一条里才在本窗口换位）；`Default` 的 0 表示不换位。
    strip_height: f32,
    spans: TabSpans,
    /// 上一次按下之后是不是真的拖过：拖完松手那一下的「点击」不该再选中标签。
    dragged: bool,
}

struct Press {
    key: DragKey,
    title: String,
    start: (f32, f32),
    began: bool,
    /// 第一次换位之前这一段标签的顺序（Esc 还原用）。
    original: Option<Vec<DragKey>>,
    reordered: bool,
}

impl Dragger {
    /// `strip_height` 是这个窗口标签栏的高度。
    pub fn new(strip_height: f32) -> Self {
        Self {
            strip_height,
            ..Self::default()
        }
    }

    /// 各标签的真实边界记录（画标签时放进去一个 [`TabSpans::track`]）。
    pub fn spans(&self) -> &TabSpans {
        &self.spans
    }

    /// 刚拖过标签（越过了起拖距离）：这之后紧跟着的那一下「点击」是拖动松手，标签的点击处理要忽略它，
    /// 否则换完位松手会把落在鼠标下的标签选中，「原来选中的仍然选中」就不成立了。下一次按下时清掉。
    pub fn just_dragged(&self) -> bool {
        self.dragged
    }

    /// 在标签上按下左键。
    pub fn press(&mut self, key: DragKey, title: String, pos: (f32, f32)) {
        self.dragged = false;
        self.press = Some(Press {
            key,
            title,
            start: pos,
            began: false,
            original: None,
            reordered: false,
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

    /// 指针移动（`local` 是相对本窗口的坐标，`left_down` 是左键还按着）。
    pub fn moved(
        &mut self,
        local: (f32, f32),
        left_down: bool,
        window: &Window,
        cx: &mut App,
    ) -> Moved {
        match self.step(local, left_down) {
            Step::Idle | Step::Hold => return Moved::default(),
            Step::Cancel => {
                // 没收到抬起事件（比如鼠标在别处被抬起）：当作取消。
                let restore = self.cancel(cx);
                return Moved {
                    redraw: true,
                    dragging: false,
                    restore,
                };
            }
            Step::Drag => {}
        }
        let Some(press) = self.press.as_ref() else {
            return Moved::default();
        };
        let source = id_of(window);
        let hover = hover_of(window, local, cx);
        let view = DragView {
            source,
            key: press.key,
            title: press.title.clone(),
            hover,
            pos: local,
            in_strip: in_strip(local, window_size(window), self.strip_height),
        };
        if current(cx)
            .as_ref()
            .map(|old| (old.hover, old.pos, old.in_strip))
            != Some((hover, local, view.in_strip))
        {
            set(cx, Some(view));
        }
        Moved {
            redraw: true,
            dragging: true,
            restore: None,
        }
    }

    /// 拖动中、指针在本窗口标签栏里：被拖的标签该不该挪位置。`keys` 是被拖标签所在那一段的全部标签
    /// （按现在的排列顺序，只含同一种），`size` 是窗口大小。要挪就返回 `(from, to)`（在 `keys` 里的
    /// 下标），窗口把它落到自己的数据上；同时记下换位前的顺序（取消时还原）、让边界记录作废。
    /// 边界还没记全（刚换过位、没重画过）或者指针不在标签栏里，返回 `None`。
    pub fn reorder_move(
        &mut self,
        local: (f32, f32),
        size: (f32, f32),
        keys: &[DragKey],
    ) -> Option<(usize, usize)> {
        let press = self.press.as_mut().filter(|press| press.began)?;
        if !in_strip(local, size, self.strip_height) {
            return None;
        }
        let from = keys.iter().position(|key| *key == press.key)?;
        let spans = self.spans.collect(keys)?;
        let to = reorder_target(&spans, from, local.0);
        if to == from {
            return None;
        }
        if press.original.is_none() {
            press.original = Some(keys.to_vec());
        }
        press.reordered = true;
        self.spans.clear();
        Some((from, to))
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

    /// 松手：返回要做什么（以及被拖的标签）；没拖起来返回 `None`。状态清掉。已经换过的位置保留。
    pub fn release(&mut self, cx: &mut App) -> Option<(DragKey, Drop)> {
        let press = self.press.take()?;
        let view = current(cx);
        if view.is_some() {
            set(cx, None);
        }
        if !press.began {
            return None;
        }
        self.dragged = true;
        let hover = view.map_or(Hover::Inside, |view| view.hover);
        Some((press.key, decide(hover)))
    }

    /// Esc / 失去捕获：取消。拖动中换过位的话返回拖动开始前的顺序，窗口要还原。
    pub fn cancel(&mut self, cx: &mut App) -> Option<Vec<DragKey>> {
        let press = self.press.take()?;
        if current(cx).is_some() {
            set(cx, None);
        }
        self.dragged |= press.began;
        press.original.filter(|_| press.reordered)
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
    // 在标签栏里时被拖的标签自己在栏里换位，不再叠一个跟着指针的提示。
    if view.hover != Hover::Inside || view.in_strip {
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

/// 这个窗口现在的大小。
pub fn window_size(window: &Window) -> (f32, f32) {
    let size = window.bounds().size;
    (f32::from(size.width), f32::from(size.height))
}

/// 这个窗口的编号。
pub fn id_of(window: &Window) -> WindowId {
    window.window_handle().window_id()
}

#[cfg(test)]
mod tests {
    use ::gpui::WindowId;

    use super::{
        DragKey, Dragger, Drop, Hover, Span, Step, WinRect, classify, decide, exceeds, in_strip,
        inside, reorder_target, restore_moves, strip_hit,
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

    // ---- 本窗口标签栏里的换位 ----

    /// 三个一样宽的标签：0..100、100..200、200..300（中线 50、150、250）。
    fn even() -> Vec<Span> {
        vec![(0.0, 100.0), (100.0, 200.0), (200.0, 300.0)]
    }

    #[test]
    fn a_tab_only_moves_once_the_pointer_crosses_the_neighbours_midline() {
        let spans = even();
        // 第 0 个往右拖：没过第 1 个的中线（150）不动，正好压在中线上也不动，过了才换。
        assert_eq!(reorder_target(&spans, 0, 60.0), 0);
        assert_eq!(reorder_target(&spans, 0, 149.0), 0);
        assert_eq!(reorder_target(&spans, 0, 150.0), 0);
        assert_eq!(reorder_target(&spans, 0, 151.0), 1);
        // 第 2 个往左拖：同理，中线是 150。
        assert_eq!(reorder_target(&spans, 2, 250.0), 2);
        assert_eq!(reorder_target(&spans, 2, 151.0), 2);
        assert_eq!(reorder_target(&spans, 2, 150.0), 2);
        assert_eq!(reorder_target(&spans, 2, 149.0), 1);
    }

    #[test]
    fn a_fast_drag_can_skip_over_several_tabs_at_once() {
        let spans = even();
        assert_eq!(reorder_target(&spans, 0, 260.0), 2);
        assert_eq!(reorder_target(&spans, 2, 10.0), 0);
    }

    #[test]
    fn dragging_past_either_end_stops_at_the_first_or_last_slot() {
        let spans = even();
        assert_eq!(reorder_target(&spans, 1, -500.0), 0);
        assert_eq!(reorder_target(&spans, 1, 9000.0), 2);
        assert_eq!(reorder_target(&spans, 0, -5.0), 0);
        assert_eq!(reorder_target(&spans, 2, 9000.0), 2);
    }

    #[test]
    fn tabs_of_different_widths_use_their_real_midlines() {
        // 宽 180、窄 100、中 140：中线分别是 90、230、350。
        let spans = vec![(0.0, 180.0), (180.0, 280.0), (280.0, 420.0)];
        // 第 0 个（宽的）往右拖：要过窄标签的中线 230，不是过它自己的边（180）。
        assert_eq!(reorder_target(&spans, 0, 200.0), 0);
        assert_eq!(reorder_target(&spans, 0, 231.0), 1);
        assert_eq!(reorder_target(&spans, 0, 351.0), 2);
        // 第 2 个往左拖：要过窄标签的中线 230。
        assert_eq!(reorder_target(&spans, 2, 250.0), 2);
        assert_eq!(reorder_target(&spans, 2, 229.0), 1);
        assert_eq!(reorder_target(&spans, 2, 89.0), 0);
    }

    #[test]
    fn swapping_unequal_tabs_does_not_flicker_back_on_the_next_frame() {
        // 宽标签 A（0..180）和窄标签 B（180..280）：指针 231 过了 B 的中线 230，A 换到 1 号。
        let before = vec![(0.0, 180.0), (180.0, 280.0)];
        assert_eq!(reorder_target(&before, 0, 231.0), 1);
        // 换位后 B 在 0..100、A 在 100..280：指针没动（231），A 现在是第 1 个，要回去得过 B 的中线 50。
        let after = vec![(0.0, 100.0), (100.0, 280.0)];
        assert_eq!(reorder_target(&after, 1, 231.0), 1);
        assert_eq!(reorder_target(&after, 1, 60.0), 1);
        assert_eq!(reorder_target(&after, 1, 49.0), 0);
    }

    #[test]
    fn a_single_tab_has_nowhere_to_go_and_bad_input_is_harmless() {
        assert_eq!(reorder_target(&[(0.0, 100.0)], 0, 500.0), 0);
        assert_eq!(reorder_target(&[(0.0, 100.0)], 0, -500.0), 0);
        assert_eq!(reorder_target(&[], 0, 10.0), 0);
        // 下标越界：原样返回，不 panic。
        assert_eq!(reorder_target(&even(), 7, 10.0), 7);
    }

    #[test]
    fn only_the_top_strip_of_the_own_window_counts_as_the_tab_bar() {
        let size = (800.0, 600.0);
        assert!(in_strip((10.0, 0.0), size, 40.0));
        assert!(in_strip((799.0, 39.0), size, 40.0));
        assert!(!in_strip((10.0, 40.0), size, 40.0), "下沿不含");
        assert!(!in_strip((10.0, 200.0), size, 40.0));
        assert!(!in_strip((-1.0, 10.0), size, 40.0), "窗口左边外面");
        assert!(!in_strip((800.0, 10.0), size, 40.0), "窗口右边外面");
        assert!(!in_strip((10.0, -3.0), size, 40.0), "窗口上边外面");
        assert!(!in_strip((10.0, 10.0), size, 0.0), "没有标签栏高度就不换位");
    }

    fn keys(ids: &[u64]) -> Vec<DragKey> {
        ids.iter().map(|id| DragKey::Terminal(*id)).collect()
    }

    /// 照 `restore_moves` 给的步骤在 `order` 上做 `remove` + `insert`，和各窗口的 `move_item` 同义。
    fn apply(order: &mut Vec<DragKey>, moves: &[(usize, usize)]) {
        for (from, to) in moves {
            let key = order.remove(*from);
            order.insert(*to, key);
        }
    }

    #[test]
    fn restoring_the_original_order_replays_back_to_the_start() {
        let original = keys(&[1, 2, 3, 4]);
        for current in [
            keys(&[1, 2, 3, 4]),
            keys(&[2, 1, 3, 4]),
            keys(&[4, 3, 2, 1]),
            keys(&[2, 3, 4, 1]),
            keys(&[3, 1, 4, 2]),
        ] {
            let moves = restore_moves(&current, &original);
            let mut order = current.clone();
            apply(&mut order, &moves);
            assert_eq!(order, original, "从 {current:?} 还原");
        }
        assert!(restore_moves(&original, &original).is_empty());
    }

    #[test]
    fn restoring_ignores_tabs_that_came_or_went_during_the_drag() {
        let original = keys(&[1, 2, 3]);
        // 拖动期间 2 被关了、5 新开了：剩下的按原来的相对顺序，新来的留在最后。
        let current = keys(&[3, 5, 1]);
        let mut order = current.clone();
        apply(&mut order, &restore_moves(&current, &original));
        assert_eq!(order, keys(&[1, 3, 5]));
    }

    #[test]
    fn restoring_works_for_file_tab_keys_too() {
        // 文件标签的编号也一样处理。
        let original = vec![DragKey::Sftp(1), DragKey::Sftp(2)];
        let current = vec![DragKey::Sftp(2), DragKey::Sftp(1)];
        assert_eq!(restore_moves(&current, &original), vec![(1, 0)]);
    }

    /// 已经拖起来（越过起拖距离）的拖动器，被拖的是终端标签 2，标签栏高 40。
    fn dragging_two() -> Dragger {
        let mut dragger = Dragger::new(40.0);
        dragger.press(DragKey::Terminal(2), "web-1".into(), (150.0, 20.0));
        assert_eq!(dragger.step((150.0, 20.0), true), Step::Hold);
        assert_eq!(dragger.step((160.0, 20.0), true), Step::Drag);
        dragger
    }

    fn lay_out(dragger: &Dragger, ids: &[u64]) {
        for (index, id) in ids.iter().enumerate() {
            let left = index as f32 * 100.0;
            dragger
                .spans()
                .set(DragKey::Terminal(*id), (left, left + 100.0));
        }
    }

    #[test]
    fn reorder_move_swaps_once_then_waits_for_the_next_frame_to_remeasure() {
        let mut dragger = dragging_two();
        lay_out(&dragger, &[1, 2, 3]);
        let order = keys(&[1, 2, 3]);
        let size = (800.0, 600.0);
        // 还没过中线：不动。
        assert_eq!(dragger.reorder_move((220.0, 20.0), size, &order), None);
        // 往右越过 3 的中线（250）：2 要去第 3 个位置。
        assert_eq!(
            dragger.reorder_move((260.0, 20.0), size, &order),
            Some((1, 2))
        );
        // 换位后窗口还没重画：边界作废，再来一次移动也不会拿旧位置乱算。
        let moved = keys(&[1, 3, 2]);
        assert_eq!(dragger.reorder_move((262.0, 20.0), size, &moved), None);
        // 下一帧重画记下新位置后又能算了：这次没过中线，不动。
        lay_out(&dragger, &[1, 3, 2]);
        assert_eq!(dragger.reorder_move((262.0, 20.0), size, &moved), None);
    }

    #[test]
    fn reorder_move_ignores_a_pointer_outside_the_tab_bar_or_a_drag_that_has_not_started() {
        let mut dragger = dragging_two();
        lay_out(&dragger, &[1, 2, 3]);
        let order = keys(&[1, 2, 3]);
        let size = (800.0, 600.0);
        // 指针到了窗口内容区 / 窗口外：不在这里换位（松手按弹出 / 合并的老逻辑走）。
        assert_eq!(dragger.reorder_move((260.0, 300.0), size, &order), None);
        assert_eq!(dragger.reorder_move((900.0, 20.0), size, &order), None);
        assert_eq!(dragger.reorder_move((260.0, -20.0), size, &order), None);

        // 没越过起拖距离：手抖不换位。
        let mut shaky = Dragger::new(40.0);
        shaky.press(DragKey::Terminal(2), "web-1".into(), (150.0, 20.0));
        lay_out(&shaky, &[1, 2, 3]);
        assert_eq!(shaky.step((152.0, 21.0), true), Step::Hold);
        assert_eq!(shaky.reorder_move((260.0, 20.0), size, &order), None);
        // 没有按下的标签。
        let mut idle = Dragger::new(40.0);
        lay_out(&idle, &[1, 2, 3]);
        assert_eq!(idle.reorder_move((260.0, 20.0), size, &order), None);
    }

    #[test]
    fn reorder_move_needs_every_tab_measured_and_the_dragged_one_in_the_group() {
        let mut dragger = dragging_two();
        let order = keys(&[1, 2, 3]);
        let size = (800.0, 600.0);
        // 一个都没量过。
        assert_eq!(dragger.reorder_move((260.0, 20.0), size, &order), None);
        // 只量了一部分。
        dragger.spans().set(DragKey::Terminal(1), (0.0, 100.0));
        dragger.spans().set(DragKey::Terminal(2), (100.0, 200.0));
        assert_eq!(dragger.reorder_move((260.0, 20.0), size, &order), None);
        // 量全了，但被拖的标签不在这一组里（比如拿成了别种标签的列表）。
        lay_out(&dragger, &[1, 2, 3]);
        assert_eq!(
            dragger.reorder_move((260.0, 20.0), size, &keys(&[1, 3])),
            None
        );
    }

    #[test]
    fn stale_measurements_in_the_wrong_order_are_refused() {
        let dragger = Dragger::new(40.0);
        // 换位之后窗口还没重画，记下的位置还是换位之前的：按新顺序取出来不是从左到右。
        lay_out(&dragger, &[1, 2, 3]);
        assert!(dragger.spans().collect(&keys(&[1, 2, 3])).is_some());
        assert!(dragger.spans().collect(&keys(&[1, 3, 2])).is_none());
        dragger.spans().clear();
        assert!(dragger.spans().collect(&keys(&[1, 2, 3])).is_none());
    }

    #[test]
    fn cancel_hands_back_the_original_order_only_if_something_moved() {
        let size = (800.0, 600.0);
        let order = keys(&[1, 2, 3]);

        // 换过位：Press 里记下换位之前的顺序（`cancel` 据此还给窗口，它要 `App`，这里直接看 Press）。
        let mut dragger = dragging_two();
        lay_out(&dragger, &[1, 2, 3]);
        assert!(dragger.reorder_move((260.0, 20.0), size, &order).is_some());
        let press = dragger.press.take().expect("按下过");
        assert!(press.reordered);
        assert_eq!(press.original, Some(order.clone()));

        // 没换过位：没有要还原的东西。
        let mut dragger = dragging_two();
        lay_out(&dragger, &[1, 2, 3]);
        assert!(dragger.reorder_move((220.0, 20.0), size, &order).is_none());
        let press = dragger.press.take().expect("按下过");
        assert!(!press.reordered);
        assert_eq!(press.original, None);
    }

    #[test]
    fn a_new_press_clears_the_just_dragged_mark() {
        let mut dragger = Dragger::new(40.0);
        assert!(!dragger.just_dragged());
        dragger.press(DragKey::Terminal(2), "web-1".into(), (150.0, 20.0));
        assert!(!dragger.just_dragged(), "还没拖，单击照常选中");
        // 手抖没越过起拖距离：松手也算单击。
        assert_eq!(dragger.step((152.0, 20.0), true), Step::Hold);
        assert!(!dragger.just_dragged());
        // 越过了：拖动；松手（`release` / `cancel`，它们要 `App`，这里直接置位）之后那一下点击要被忽略。
        assert_eq!(dragger.step((190.0, 20.0), true), Step::Drag);
        dragger.dragged = true;
        assert!(dragger.just_dragged());
        dragger.press(DragKey::Terminal(3), "web-2".into(), (10.0, 10.0));
        assert!(!dragger.just_dragged(), "下一次按下重新开始");
    }
}
