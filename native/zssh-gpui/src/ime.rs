//! GPUI 入口的输入法接线（Windows）。
//!
//! 每个终端窗口有自己的 key；只有最后一个离开英文区的窗口才真正恢复输入法，
//! 和 Tauri 主窗口/文件窗口的规则一致。
//!
//! macOS 不做“进终端自动切英文”：系统输入法由 GPUI 的 `InputHandler` 直接接，
//! 这里保留同名接口返回“没有切换”，调用方不需要按平台分支。

#[cfg(windows)]
use std::sync::Mutex;

use ::gpui::{Context, FocusHandle, Subscription, Window};

#[cfg(windows)]
use zssh_platform_windows::ime::{SharedIme, Win32Ime};

#[cfg(windows)]
static STATE: Mutex<Option<SharedIme>> = Mutex::new(None);

/// `focused = true` 进入终端英文区，`false` 离开并恢复。
#[cfg(windows)]
pub fn apply(window: &str, focused: bool) -> bool {
    let mut state = STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state
        .get_or_insert_with(SharedIme::default)
        .apply(window, focused, &mut Win32Ime)
}

/// macOS 没有“自动切英文”这项 Windows 专属能力；输入法组合由系统接管。
#[cfg(not(windows))]
pub fn apply(_window: &str, _focused: bool) -> bool {
    false
}

/// 窗口销毁：从英文区名单里移除，必要时恢复。
#[cfg(windows)]
pub fn window_destroyed(window: &str) -> bool {
    let mut state = STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state
        .as_mut()
        .is_some_and(|shared| shared.window_destroyed(window, &mut Win32Ime))
}

#[cfg(not(windows))]
pub fn window_destroyed(_window: &str) -> bool {
    false
}

/// 给带焦点句柄的视图接上输入法接线：`key` 是这个视图在英文区名单里的键。
/// 返回的两个订阅要留在视图里（订阅被丢弃即注销，绑定名带下划线让它们活到视图销毁）。
/// 焦进焦点还要做别的判断的视图（如终端的「自动切英文」开关）自己写 `on_focus`，只借用这里的 `apply`。
#[cfg(windows)]
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

/// macOS 上焦点变化不需要切换输入法；订阅仍然返回，调用方的字段和生命周期不用改。
#[cfg(not(windows))]
pub(crate) fn attach<T: 'static>(
    cx: &mut Context<T>,
    focus: &FocusHandle,
    window: &mut Window,
    _key: &str,
) -> (Subscription, Subscription) {
    let on_focus = cx.on_focus(focus, window, |_this, _window, _cx| {});
    let on_blur = cx.on_blur(focus, window, |_this, _window, _cx| {});
    (on_focus, on_blur)
}
