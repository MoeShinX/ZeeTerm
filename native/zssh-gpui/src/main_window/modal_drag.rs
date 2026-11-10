//! 主窗口对话框的拖动：按住卡片顶部的标题区域拖，卡片跟着鼠标走。
//!
//! 偏移只属于"当前这一个"对话框：对话框关掉或换成另一种，下一帧画的时候回到居中（`sync`）。
//! 卡片始终是 flex 居中的，拖动只加一个相对偏移（`left` / `top`），布局和 `host_props_overlay`
//! 这类叠在遮罩上的浮层都不受影响（浮层盖满整个遮罩，自己居中，它开着的时候下面的卡片也点不到、拖不了）。
//!
//! 这里只有状态和纯计算；事件接线（把手上按下、遮罩上移动 / 松开）在 `modal.rs` 的 `render_modal`。

use std::cell::Cell;
use std::mem::Discriminant;
use std::rc::Rc;

use ::gpui::{Div, InteractiveElement as _, MouseButton, Styled as _, px};

use super::modal::Modal;

/// 拖动把手的高度：卡片 `p_5` 的上内边距 20 + 一行标题 24（带关闭按钮的大对话框，标题行里最高的是 24 的按钮）。
/// 各对话框正文里第一个输入框都在这条之下（见任务报告里逐个核对的结论），所以把手不会和输入框里拖选文字冲突。
/// 主机属性对话框用紧凑卡片（上内边距 16 + 标题 21 = 37，标题到分隔线之间还有 12 的空隙）：把手 44 只多盖住标题下面
/// 7 像素的空白，到不了分隔线（49）更碰不到第一行字段，所以没有跟着缩小。
pub const HANDLE_HEIGHT: f32 = 44.0;

/// 按下之后指针要离开按下点这么远（像素）才算拖动；之前只是点击，卡片不动、按钮照常响应。
pub const DRAG_THRESHOLD: f32 = 4.0;

/// 卡片左右方向至少要留在窗口里的宽度（卡片比这窄就是整张卡片）。
pub const KEEP_VISIBLE_X: f32 = 120.0;

/// 按下点离开多远才算拖动。
pub fn exceeds_threshold(from: (f32, f32), to: (f32, f32)) -> bool {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    dx * dx + dy * dy >= DRAG_THRESHOLD * DRAG_THRESHOLD
}

/// 把偏移夹到合法范围。`card` 是卡片大小，`viewport` 是窗口大小；卡片原本在窗口正中。
///
/// 竖直方向：标题区（把手）整条留在窗口里（卡片上沿不能超出窗口顶、也不能低到把手出窗口底）。
/// 水平方向：卡片至少 [`KEEP_VISIBLE_X`] 宽的一段留在窗口里。窗口比卡片还小（范围反过来）时，
/// 以靠近左 / 上的边界为准，不会 panic。
pub fn clamp_offset(offset: (f32, f32), card: (f32, f32), viewport: (f32, f32)) -> (f32, f32) {
    let base_left = (viewport.0 - card.0) / 2.0;
    let base_top = (viewport.1 - card.1) / 2.0;
    let keep = KEEP_VISIBLE_X.min(card.0);
    // 偏移的范围：left + dx + width >= keep 且 left + dx <= viewport - keep
    let min_x = keep - card.0 - base_left;
    let max_x = (viewport.0 - keep - base_left).max(min_x);
    // top + dy >= 0 且 top + dy <= viewport - handle
    let min_y = -base_top;
    let max_y = (viewport.1 - HANDLE_HEIGHT.min(card.1) - base_top).max(min_y);
    (offset.0.clamp(min_x, max_x), offset.1.clamp(min_y, max_y))
}

/// 按下之后的一次手势。
#[derive(Clone, Copy, Debug, PartialEq)]
struct Press {
    mouse: (f32, f32),
    offset: (f32, f32),
    dragging: bool,
}

/// 当前对话框的拖动状态。
pub struct ModalDrag {
    /// 偏移属于哪一种对话框；`None` = 现在没有对话框。
    kind: Option<Discriminant<Modal>>,
    offset: (f32, f32),
    press: Option<Press>,
    /// 卡片上一帧的大小，画卡片时由一块不可见的画布写入（夹取需要它）。
    card_size: Rc<Cell<Option<(f32, f32)>>>,
}

impl Default for ModalDrag {
    fn default() -> Self {
        Self {
            kind: None,
            offset: (0.0, 0.0),
            press: None,
            card_size: Rc::new(Cell::new(None)),
        }
    }
}

impl ModalDrag {
    /// 每次画对话框前调用：对话框换了一种或关掉了，偏移和进行中的手势都清掉，回到居中。
    pub fn sync(&mut self, kind: Option<Discriminant<Modal>>) {
        if self.kind != kind {
            self.kind = kind;
            self.offset = (0.0, 0.0);
            self.press = None;
            self.card_size.set(None);
        }
    }

    /// 给卡片里的测量画布用：每帧把卡片大小写进去。
    pub fn card_size_cell(&self) -> Rc<Cell<Option<(f32, f32)>>> {
        self.card_size.clone()
    }

    /// 画卡片用的偏移。窗口缩小后原偏移可能把标题区带出窗口，这里夹回来并写回。
    pub fn offset_for_render(&mut self, viewport: (f32, f32)) -> (f32, f32) {
        if self.offset != (0.0, 0.0)
            && let Some(card) = self.card_size.get()
        {
            self.offset = clamp_offset(self.offset, card, viewport);
        }
        self.offset
    }

    /// 在把手上按下左键。
    pub fn press(&mut self, mouse: (f32, f32)) {
        self.press = Some(Press {
            mouse,
            offset: self.offset,
            dragging: false,
        });
    }

    /// 指针移动。返回偏移是否变了（要不要重画）。`left_pressed` 为假说明左键已经在别处松开
    /// （比如指针出了窗口再松开），手势结束。
    pub fn motion(&mut self, mouse: (f32, f32), left_pressed: bool, viewport: (f32, f32)) -> bool {
        let Some(mut press) = self.press else {
            return false;
        };
        if !left_pressed {
            self.press = None;
            return true;
        }
        if !press.dragging {
            if !exceeds_threshold(press.mouse, mouse) {
                return false;
            }
            press.dragging = true;
            self.press = Some(press);
        }
        let wanted = (
            press.offset.0 + (mouse.0 - press.mouse.0),
            press.offset.1 + (mouse.1 - press.mouse.1),
        );
        let next = match self.card_size.get() {
            Some(card) => clamp_offset(wanted, card, viewport),
            None => wanted,
        };
        let changed = next != self.offset;
        self.offset = next;
        changed
    }

    /// 左键松开。返回这次手势是不是真的拖动过（要重画光标）。
    pub fn release(&mut self) -> bool {
        self.press.take().is_some_and(|press| press.dragging)
    }

    /// 正在拖动（已经越过阈值）。
    pub fn dragging(&self) -> bool {
        self.press.is_some_and(|press| press.dragging)
    }
}

/// 卡片里画在内容下面的透明把手：只负责接按下事件，内容（按钮、标题）画在它上面，点击照常响应。
pub fn handle(
    dragging: bool,
    on_press: impl Fn(&::gpui::MouseDownEvent, &mut ::gpui::Window, &mut ::gpui::App) + 'static,
) -> Div {
    let strip = ::gpui::div()
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .h(px(HANDLE_HEIGHT))
        .on_mouse_down(MouseButton::Left, on_press);
    // GPUI 没有"可移动"的专用光标：手形（张开 / 抓住）最接近。
    if dragging {
        strip.cursor_grabbing()
    } else {
        strip.cursor_grab()
    }
}

/// 卡片里不可见的测量画布：每帧把卡片（含边框）的大小写进 `cell`。
pub fn measure(cell: Rc<Cell<Option<(f32, f32)>>>) -> impl ::gpui::IntoElement {
    ::gpui::canvas(
        move |bounds, _, _| {
            // 画布铺满卡片的内容区（不含 1px 边框），补回两侧边框。
            cell.set(Some((
                f32::from(bounds.size.width) + 2.0,
                f32::from(bounds.size.height) + 2.0,
            )));
        },
        |_, _, _, _| {},
    )
    .absolute()
    .inset_0()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARD: (f32, f32) = (400.0, 200.0);
    const VIEW: (f32, f32) = (1000.0, 700.0);

    #[test]
    fn a_small_wiggle_is_a_click_not_a_drag() {
        assert!(!exceeds_threshold((100.0, 100.0), (101.0, 102.0)));
        assert!(!exceeds_threshold((100.0, 100.0), (100.0, 100.0)));
        assert!(exceeds_threshold((100.0, 100.0), (104.0, 100.0)));
        assert!(exceeds_threshold((100.0, 100.0), (103.0, 103.0)));
    }

    #[test]
    fn the_card_stays_put_until_the_pointer_has_moved_far_enough() {
        let mut drag = ModalDrag::default();
        drag.press((500.0, 300.0));
        assert!(!drag.motion((502.0, 301.0), true, VIEW));
        assert_eq!(drag.offset_for_render(VIEW), (0.0, 0.0));
        assert!(!drag.dragging());
        // 越过阈值之后偏移 = 指针相对按下点的位移，不是"超出阈值的部分"。
        assert!(drag.motion((520.0, 310.0), true, VIEW));
        assert!(drag.dragging());
        assert_eq!(drag.offset_for_render(VIEW), (20.0, 10.0));
        // 松开之后偏移留着。
        assert!(drag.release());
        assert_eq!(drag.offset_for_render(VIEW), (20.0, 10.0));
        assert!(!drag.dragging());
    }

    #[test]
    fn releasing_without_moving_is_not_a_drag() {
        let mut drag = ModalDrag::default();
        drag.press((500.0, 300.0));
        assert!(!drag.release());
        assert_eq!(drag.offset_for_render(VIEW), (0.0, 0.0));
    }

    #[test]
    fn moves_without_a_press_do_nothing() {
        let mut drag = ModalDrag::default();
        assert!(!drag.motion((900.0, 900.0), true, VIEW));
        assert_eq!(drag.offset_for_render(VIEW), (0.0, 0.0));
    }

    #[test]
    fn a_second_drag_continues_from_where_the_first_one_stopped() {
        let mut drag = ModalDrag::default();
        drag.press((500.0, 300.0));
        drag.motion((540.0, 300.0), true, VIEW);
        drag.release();
        drag.press((540.0, 300.0));
        drag.motion((550.0, 330.0), true, VIEW);
        assert_eq!(drag.offset_for_render(VIEW), (50.0, 30.0));
    }

    #[test]
    fn the_title_strip_cannot_leave_the_window() {
        // 卡片原位 (300, 250)。往上最多到卡片上沿贴窗口顶：dy = -250。
        let (_, y) = clamp_offset((0.0, -9999.0), CARD, VIEW);
        assert_eq!(y, -250.0);
        // 往下：把手（44）要整条留着：卡片上沿最低 700 - 44 = 656，dy = 656 - 250。
        let (_, y) = clamp_offset((0.0, 9999.0), CARD, VIEW);
        assert_eq!(y, 406.0);
        // 往左：至少 120 宽留在窗口里，右沿 >= 120：dx = 120 - 400 - 300 = -580。
        let (x, _) = clamp_offset((-9999.0, 0.0), CARD, VIEW);
        assert_eq!(x, -580.0);
        // 往右：左沿 <= 1000 - 120 = 880：dx = 880 - 300 = 580。
        let (x, _) = clamp_offset((9999.0, 0.0), CARD, VIEW);
        assert_eq!(x, 580.0);
        // 范围内的偏移原样保留。
        assert_eq!(clamp_offset((10.0, -20.0), CARD, VIEW), (10.0, -20.0));
    }

    #[test]
    fn a_taller_card_than_the_window_still_clamps_without_panicking() {
        // 窗口比卡片还矮：基准位置在窗口上方之外，范围反过来也不能 panic，且结果在 [min, max] 的下界。
        let tiny = (300.0, 30.0);
        let (x, y) = clamp_offset((50.0, 50.0), (400.0, 200.0), tiny);
        assert!(x.is_finite() && y.is_finite());
        let (x, y) = clamp_offset((-50.0, -50.0), (400.0, 200.0), tiny);
        assert!(x.is_finite() && y.is_finite());
    }

    #[test]
    fn shrinking_the_window_pulls_a_dragged_card_back() {
        let mut drag = ModalDrag::default();
        drag.card_size_cell().set(Some(CARD));
        drag.press((500.0, 300.0));
        drag.motion((500.0, 700.0), true, VIEW);
        let (_, y) = drag.offset_for_render(VIEW);
        assert_eq!(y, 400.0);
        // 窗口变矮到 400：卡片原位 top = 100，把手要在窗口里：top + dy <= 356 -> dy <= 256。
        let (_, y) = drag.offset_for_render((1000.0, 400.0));
        assert_eq!(y, 256.0);
    }

    #[test]
    fn dragging_never_goes_past_the_edge_even_with_a_huge_pointer_move() {
        let mut drag = ModalDrag::default();
        drag.card_size_cell().set(Some(CARD));
        drag.press((500.0, 300.0));
        drag.motion((-5000.0, -5000.0), true, VIEW);
        assert_eq!(drag.offset_for_render(VIEW), (-580.0, -250.0));
    }

    #[test]
    fn the_offset_belongs_to_one_dialog_only() {
        let mut drag = ModalDrag::default();
        let confirm = std::mem::discriminant(&Modal::AiPresets);
        drag.sync(Some(confirm));
        drag.press((0.0, 0.0));
        drag.motion((30.0, 40.0), true, VIEW);
        drag.release();
        assert_eq!(drag.offset_for_render(VIEW), (30.0, 40.0));
        // 同一个对话框重画：偏移保留。
        drag.sync(Some(confirm));
        assert_eq!(drag.offset_for_render(VIEW), (30.0, 40.0));
        // 关掉再打开：回到居中。
        drag.sync(None);
        drag.sync(Some(confirm));
        assert_eq!(drag.offset_for_render(VIEW), (0.0, 0.0));
    }

    #[test]
    fn losing_the_button_outside_the_window_ends_the_gesture() {
        let mut drag = ModalDrag::default();
        drag.press((500.0, 300.0));
        drag.motion((540.0, 300.0), true, VIEW);
        assert!(drag.dragging());
        // 指针出窗口松开，回来时已经没按着：手势结束，之后移动不再拖。
        drag.motion((560.0, 300.0), false, VIEW);
        assert!(!drag.dragging());
        let before = drag.offset_for_render(VIEW);
        drag.motion((900.0, 600.0), true, VIEW);
        assert_eq!(drag.offset_for_render(VIEW), before);
    }
}
