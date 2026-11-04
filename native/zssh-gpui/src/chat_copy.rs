//! AI 助手聊天里「复制」按钮和右键菜单的写入口（V2-U31）。
//!
//! 所有聊天界面的复制都走 [`write`]：正常情况下就是写系统剪贴板；测试 / 调试脚本可以先
//! [`capture_start`]，之后写入的内容进内存缓冲、不碰系统剪贴板（自动化不能动用户的剪贴板）。
//! 调试构建里还能用环境变量 `ZSSH_FAKE_CLIPBOARD=1` 让整个进程一开始就走缓冲。

use std::sync::Mutex;

use gpui_kit::*;

/// `Some` = 抓取模式：写入的内容攒在这里，不写系统剪贴板。
static CAPTURE: Mutex<Option<Vec<String>>> = Mutex::new(None);

/// 写一段文字。
pub fn write(text: String, cx: &mut App) {
    if capture_push(&text) {
        return;
    }
    cx.write_to_clipboard(ClipboardItem::new_string(text));
}

/// 写一段敏感文字（密码）：正常情况走系统的「排除历史」写法，30 秒后自动清除；抓取模式下
/// 和 [`write`] 一样只进内存缓冲，测试里不碰系统剪贴板。
pub fn write_sensitive(text: &str, _cx: &mut App) -> Result<(), String> {
    write_sensitive_with(text, zssh_platform_windows::clipboard::write_sensitive)
}

/// [`write_sensitive`] 的内核：`system` 是真正写系统剪贴板的函数，抓取模式下不会被调用（测试用它证明这一点）。
fn write_sensitive_with(
    text: &str,
    system: impl FnOnce(&str) -> Result<(), String>,
) -> Result<(), String> {
    if capture_push(text) {
        return Ok(());
    }
    system(text)
}

/// 抓取模式下记下来并返回 `true`；不是抓取模式返回 `false`。
fn capture_push(text: &str) -> bool {
    #[cfg(debug_assertions)]
    if crate::dev_hooks::var("ZSSH_FAKE_CLIPBOARD").is_some_and(|v| v == "1") {
        capture_start();
    }
    let Ok(mut guard) = CAPTURE.lock() else {
        return false;
    };
    match guard.as_mut() {
        Some(buffer) => {
            buffer.push(text.to_owned());
            true
        }
        None => false,
    }
}

/// 进入抓取模式（已经在抓就保留已抓到的内容）。
#[cfg(any(test, debug_assertions))]
pub fn capture_start() {
    if let Ok(mut guard) = CAPTURE.lock()
        && guard.is_none()
    {
        *guard = Some(Vec::new());
    }
}

/// 取走抓到的内容（按写入顺序）；没进抓取模式返回空。
#[cfg(any(test, debug_assertions))]
pub fn capture_take() -> Vec<String> {
    CAPTURE
        .lock()
        .ok()
        .and_then(|mut guard| guard.as_mut().map(std::mem::take))
        .unwrap_or_default()
}

/// 抓到的最后一次写入（不取走）。
#[cfg(debug_assertions)]
pub fn capture_last() -> Option<String> {
    CAPTURE
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().and_then(|buffer| buffer.last().cloned()))
}

#[cfg(test)]
mod tests {
    use super::{capture_push, capture_start, capture_take, write_sensitive_with};

    /// 抓取缓冲是进程里唯一的一份，用它的测试不能并行。
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn capture_collects_in_order_and_take_drains() {
        let _serial = SERIAL.lock().unwrap_or_else(|poison| poison.into_inner());
        capture_start();
        let _ = capture_take();
        assert!(capture_push("a"));
        assert!(capture_push("b"));
        assert_eq!(capture_take(), vec!["a".to_owned(), "b".to_owned()]);
        assert!(capture_take().is_empty());
    }

    #[test]
    fn sensitive_writes_stay_in_the_buffer_while_capturing() {
        let _serial = SERIAL.lock().unwrap_or_else(|poison| poison.into_inner());
        capture_start();
        let _ = capture_take();
        let result =
            write_sensitive_with(
                "demo-pass",
                |_| Err("抓取模式下不该写系统剪贴板".to_owned()),
            );
        assert_eq!(result, Ok(()));
        assert_eq!(capture_take(), vec!["demo-pass".to_owned()]);
    }
}
