//! 非 Windows 的"本应用窗口里最近一次操作"：把 GPUI 的键盘 / 鼠标事件喂给
//! `zssh_platform_windows::activity::touch()`，应用锁的"空闲 N 分钟自动锁定"靠它判断（V2-U26）。
//!
//! Windows 在界面线程上装了消息钩子（`activity::install_on_current_thread`），一次覆盖所有窗口；
//! Unix 没有钩子，之前没人调用 `touch()`，`idle_for()` 从启动起一直涨，等于**启动 N 分钟后无条件锁定**
//! （不管用户有没有在操作）。这里补上，语义和 Windows 一致：只算本应用窗口里的输入，别的程序里的操作不算。
//!
//! 三条来源，合起来覆盖所有窗口（主窗口、独立终端窗口、文件窗口、各种工具窗口）：
//! - 键盘：`App::intercept_keystrokes`，任何窗口的每个按键都经过它，不受窗口里谁处理了按键的影响；
//! - 鼠标按下 / 抬起 / 滚轮 / 移动：[`probe`] 放在窗口根元素里，每帧给窗口登记鼠标监听
//!   （GPUI 只允许在绘制阶段登记，没有全局的鼠标钩子）；
//! - 兜底：[`install`] 里每半秒扫一遍所有窗口的鼠标位置和修饰键，变了就算操作——没放 `probe` 的小窗口
//!   至少在鼠标移动时也算数（原地点击 / 滚轮这类不动鼠标的操作只在放了 `probe` 的窗口里统计）。

#[cfg(any(not(windows), test))]
use std::collections::HashMap;
#[cfg(not(windows))]
use std::time::Duration;

#[cfg(not(windows))]
use gpui::{App, WindowId};
#[cfg(any(not(windows), test))]
use gpui::{Modifiers, Pixels, Point};
#[cfg(not(windows))]
use zssh_platform_windows::activity::touch;

/// 扫描窗口鼠标位置的间隔。
#[cfg(not(windows))]
const SCAN_EVERY: Duration = Duration::from_millis(500);

/// 记住每个窗口上一次看到的鼠标位置 / 修饰键，判断有没有变化。
#[cfg(any(not(windows), test))]
struct PointerWatch<K> {
    seen: HashMap<K, (Point<Pixels>, Modifiers)>,
}

#[cfg(any(not(windows), test))]
impl<K> Default for PointerWatch<K> {
    fn default() -> Self {
        Self {
            seen: HashMap::new(),
        }
    }
}

#[cfg(any(not(windows), test))]
impl<K: std::hash::Hash + Eq + Copy> PointerWatch<K> {
    /// 看一次某个窗口的状态：和上次不同返回 `true`。第一次见到的窗口只记下来、不算操作
    /// （新窗口刚打开时鼠标本来就停在那里）。
    fn observe(&mut self, window: K, position: Point<Pixels>, modifiers: Modifiers) -> bool {
        match self.seen.insert(window, (position, modifiers)) {
            Some(previous) => previous != (position, modifiers),
            None => false,
        }
    }

    /// 窗口关掉以后不再占着记录。
    fn retain_only(&mut self, live: &[K]) {
        self.seen.retain(|id, _| live.contains(id));
    }
}

/// 开始统计。必须在界面线程、开任何窗口之前调用一次。
#[cfg(not(windows))]
pub fn install(cx: &mut App) {
    // 从现在起算第一次"最近操作"，和 Windows 的 `install_on_current_thread` 一致。
    touch();
    cx.intercept_keystrokes(|_event, _window, _cx| touch())
        .detach();

    cx.spawn(async move |cx| {
        let mut watch = PointerWatch::<WindowId>::default();
        loop {
            cx.background_executor().timer(SCAN_EVERY).await;
            cx.update(|cx| {
                let handles = cx.windows();
                let live: Vec<WindowId> = handles.iter().map(|handle| handle.window_id()).collect();
                watch.retain_only(&live);
                for handle in handles {
                    let id = handle.window_id();
                    // 窗口正被占用（正在更新 / 已销毁）时读不到，这一轮跳过
                    let Ok((position, modifiers)) = handle.update(cx, |_, window, _| {
                        (window.mouse_position(), window.modifiers())
                    }) else {
                        continue;
                    };
                    if watch.observe(id, position, modifiers) {
                        touch();
                    }
                }
            });
        }
    })
    .detach();
}

/// 放进窗口根元素里的探针：零大小，什么都不画，只在每次绘制时给这个窗口登记鼠标监听。
/// Windows 上用不到（消息钩子已经覆盖），返回 `None`。
#[cfg(not(windows))]
pub fn probe() -> Option<gpui::AnyElement> {
    use gpui::{
        IntoElement as _, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ScrollEvent, Styled as _,
        canvas,
    };
    Some(
        canvas(
            |_bounds, _window, _cx| (),
            |_bounds, (), window, _cx| {
                window.on_mouse_event(|_: &MouseDownEvent, _phase, _window, _cx| touch());
                window.on_mouse_event(|_: &MouseUpEvent, _phase, _window, _cx| touch());
                window.on_mouse_event(|_: &MouseMoveEvent, _phase, _window, _cx| touch());
                window.on_mouse_event(|_: &ScrollEvent, _phase, _window, _cx| touch());
            },
        )
        .absolute()
        .size_0()
        .into_any_element(),
    )
}

#[cfg(windows)]
pub fn probe() -> Option<gpui::AnyElement> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px};
    use std::time::Duration;

    fn at(x: f32, y: f32) -> Point<Pixels> {
        point(px(x), px(y))
    }

    #[test]
    fn a_window_seen_for_the_first_time_is_not_activity() {
        let mut watch = PointerWatch::default();
        assert!(!watch.observe(1u32, at(10., 10.), Modifiers::default()));
    }

    #[test]
    fn moving_the_pointer_or_pressing_a_modifier_is_activity() {
        let mut watch = PointerWatch::default();
        let none = Modifiers::default();
        assert!(!watch.observe(1u32, at(10., 10.), none));
        assert!(!watch.observe(1, at(10., 10.), none), "没动就不算");
        assert!(watch.observe(1, at(11., 10.), none), "指针动了");
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        assert!(watch.observe(1, at(11., 10.), shift), "修饰键变了");
        assert!(!watch.observe(1, at(11., 10.), shift));
    }

    #[test]
    fn windows_are_tracked_independently_and_forgotten_when_closed() {
        let mut watch = PointerWatch::default();
        let none = Modifiers::default();
        assert!(!watch.observe(1u32, at(0., 0.), none));
        assert!(!watch.observe(2, at(5., 5.), none));
        assert!(watch.observe(2, at(6., 5.), none));
        assert!(!watch.observe(1, at(0., 0.), none));
        // 窗口 1 关掉又开（编号复用也一样）：当作新窗口，第一次不算
        watch.retain_only(&[2]);
        assert!(!watch.observe(1, at(99., 99.), none));
    }

    #[test]
    fn touching_resets_the_shared_idle_clock() {
        zssh_platform_windows::activity::touch();
        assert!(zssh_platform_windows::activity::idle_for() < Duration::from_secs(1));
    }
}
