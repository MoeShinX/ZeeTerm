//! 文件列表和传输队列之间的拖动条（V2-U53）：视图这一侧的状态和事件。规则（夹取、拖动换算、收起阈值、
//! 存取）都在 `queue_height.rs` 的纯函数里，这里只负责把它们接到鼠标、量到的高度和设置上。
//!
//! - 拖动用窗口级监听（`tab_drag::sensor`）：指针拖出 6px 的拖动条、进到列表或队列上、甚至出了窗口都照样跟着，
//!   松手也一定收得到；列表的框选 / 拖放、队列头部的展开收起都是从各自的「按下」开始的，按下落在拖动条上
//!   （`stop_propagation`），它们根本不会开始，所以拖动过程中不会误触发。
//! - 高度存本机设置 `layout.sftp_queue_height`，四种用法共用：一个视图松手后写库并把「代数」加一，
//!   别的活着的视图在 100ms 心跳里发现代数变了就重新读（读不到 = 没拖过）。

use std::sync::atomic::{AtomicU64, Ordering};

use ::gpui::{Context, MouseButton, MouseDownEvent, MouseMoveEvent};
use zssh_core::store::Database;

use super::SftpDemo;
use super::queue_height::{self as qh, Drag, DragSession, QueueState};

/// 每次有视图改了存的高度就加一，让别的视图重新读。
static SAVED_GENERATION: AtomicU64 = AtomicU64::new(0);

/// 队列内容区这一帧该怎么摆。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum QueueBody {
    /// 没拖过：按内容、最高 224（和改动前一样）。
    Natural,
    /// 拖过：固定高度（已按可用高度夹取，可能是 0 = 矮到画不下）。
    Fixed(f32),
}

/// 新视图启动时读一次存的高度（不建目录、读不到就是没拖过）。
pub(super) fn load_saved() -> Option<f32> {
    Database::open(&crate::settings::data_dir().join("zssh.db"))
        .ok()
        .and_then(|db| qh::load(&db))
}

/// 当前代数（新视图读完存量后记下，之后只在变了才重读）。
pub(super) fn generation() -> u64 {
    SAVED_GENERATION.load(Ordering::Relaxed)
}

impl SftpDemo {
    /// 文件列表区 + 队列内容区的总高度（量到的上一帧的值；还没量到是 0）。
    fn queue_room(&self) -> f32 {
        self.list_height.get() + self.queue_content_height.get()
    }

    fn queue_state(&self) -> QueueState {
        QueueState {
            open: self.queue_open,
            stored: self.queue_height,
        }
    }

    /// 画面上队列展开了没有、内容区怎么摆：拖动中看拖动的预览，否则看已有的状态。
    pub(super) fn queue_shown(&self) -> (bool, QueueBody) {
        match self.queue_drag.as_ref().and_then(DragSession::live) {
            Some(Drag::Open(height)) => return (true, QueueBody::Fixed(height)),
            Some(Drag::Collapsed) => return (false, QueueBody::Natural),
            None => {}
        }
        let body = match self.queue_height {
            None => QueueBody::Natural,
            Some(height) => QueueBody::Fixed(qh::clamp_content(height, self.queue_room())),
        };
        (self.queue_open, body)
    }

    /// 在拖动条上按下：单击开始拖，双击恢复默认高度。
    pub(super) fn queue_drag_start(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        if event.click_count >= 2 {
            self.queue_drag = None;
            let after = qh::reset(self.queue_state());
            self.apply_queue_state(after, cx);
            return;
        }
        self.queue_drag = Some(DragSession::begin(
            self.queue_open,
            self.queue_content_height.get(),
            f32::from(event.position.y),
        ));
        cx.notify();
    }

    /// 窗口里鼠标动了：拖着的话改预览。按键已经松开（在窗口外松的）就当松手。
    pub(super) fn queue_drag_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if self.queue_drag.is_none() {
            return;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            self.queue_drag_end(cx);
            return;
        }
        let room = self.queue_room();
        if let Some(drag) = self.queue_drag.as_mut() {
            drag.update(f32::from(event.position.y), room);
        }
        cx.notify();
    }

    pub(super) fn queue_drag_end(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.queue_drag.take() else {
            return;
        };
        let after = drag.finish(self.queue_state());
        self.apply_queue_state(after, cx);
    }

    pub(super) fn apply_queue_state(&mut self, after: QueueState, cx: &mut Context<Self>) {
        let height_changed = after.stored != self.queue_height;
        self.queue_open = after.open;
        self.queue_height = after.stored;
        if height_changed {
            match crate::host_db::open_database()
                .map_err(|err| crate::error_text::error_text(&err))
                .and_then(|db| qh::save(&db, after.stored))
            {
                Ok(()) => {
                    // 自己写的不用再读回来；别的视图靠代数发现。
                    self.queue_height_seen = SAVED_GENERATION
                        .fetch_add(1, Ordering::Relaxed)
                        .wrapping_add(1);
                }
                Err(err) => tracing::warn!(%err, "保存传输队列高度失败"),
            }
        }
        cx.notify();
    }

    /// 调试驱动：按下、挪 `dy`、松手一气呵成（只在调试版）。
    #[cfg(debug_assertions)]
    pub(super) fn script_queue_drag(&mut self, dy: f32, cx: &mut Context<Self>) {
        let mut drag = DragSession::begin(self.queue_open, self.queue_content_height.get(), 0.0);
        drag.update(dy, self.queue_room());
        self.queue_drag = Some(drag);
        self.queue_drag_end(cx);
    }

    /// 调试驱动：双击拖动条（只在调试版）。
    #[cfg(debug_assertions)]
    pub(super) fn script_queue_reset(&mut self, cx: &mut Context<Self>) {
        self.queue_drag = None;
        let after = qh::reset(self.queue_state());
        self.apply_queue_state(after, cx);
    }

    /// 100ms 心跳里调：别的视图改了存的高度就跟着换。拖动中的视图不动（松手时自己说了算）。返回要不要重画。
    pub(super) fn refresh_shared_queue_height(&mut self) -> bool {
        let now = generation();
        if now == self.queue_height_seen {
            return false;
        }
        self.queue_height_seen = now;
        if self.queue_drag.is_some() {
            return false;
        }
        let saved = load_saved();
        if saved == self.queue_height {
            return false;
        }
        self.queue_height = saved;
        true
    }
}
