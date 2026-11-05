//! GPUI 入口的输入法接线：复用 `zssh-platform-windows` 的状态机。
//!
//! 每个终端窗口有自己的 key；只有最后一个离开英文区的窗口才真正恢复输入法，
//! 和 Tauri 主窗口/文件窗口的规则一致。

use std::sync::Mutex;

use ::gpui::{Context, FocusHandle, Subscription, Window};

use zssh_platform_windows::ime::{SharedIme, Win32Ime};

static STATE: Mutex<Option<SharedIme>> = Mutex::new(None);

/// `focused = true` 进入终端英文区，`false` 离开并恢复。
pub fn apply(window: &str, focused: bool) -> bool {
    let mut state = STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state
        .get_or_insert_with(SharedIme::default)
        .apply(window, focused, &mut Win32Ime)
}

/// 窗口销毁：从英文区名单里移除，必要时恢复。
pub fn window_destroyed(window: &str) -> bool {
    let mut state = STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state
        .as_mut()
        .is_some_and(|shared| shared.window_destroyed(window, &mut Win32Ime))
}

/// 给带焦点句柄的视图接上输入法接线：`key` 是这个视图在英文区名单里的键。
/// 返回的两个订阅要留在视图里（订阅被丢弃即注销，绑定名带下划线让它们活到视图销毁）。
/// 焦进焦点还要做别的判断的视图（如终端的「自动切英文」开关）自己写 `on_focus`，只借用这里的 `apply`。
pub(crate) fn attach<T: 'static>(
    cx: &mut Context<T>,
    focus: &FocusHandle,
    window: &mut Window,
    key: &str,
) -> (Subscription, Subscription) {
    let focus_key = key.to_owned();
    let on_focus = cx.on_focus(focus, window, move |_this, _window, _cx| {
        apply(&focus_key, true);
    });
    let blur_key = key.to_owned();
    let on_blur = cx.on_blur(focus, window, move |_this, _window, _cx| {
        apply(&blur_key, false);
    });
    (on_focus, on_blur)
}
