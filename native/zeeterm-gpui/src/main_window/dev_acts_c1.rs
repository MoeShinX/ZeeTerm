//! 调试驱动里 V2-C1（多行粘贴确认框绑定发起时的目标）要用的 `act:` 动作。只在 debug 构建里编进去；
//! release 里只剩两个空函数。真实的粘贴用 `clip_set:<文字>` 加 `key:ctrl+v`（走和用户一样的按键路径），
//! 这里管不经剪贴板的发起、核对确认框的内容、以及用代码点「粘贴」/「取消」：
//!
//! - `paste_begin:<文字>`：不读剪贴板，直接按这段文字发起一次粘贴（`\n` `\r` `\e` 等转义同 `type:`）；
//! - `assert_paste:none` 或 `assert_paste:<行数>,<接收窗格数>,single|broadcast`：确认框现在的样子；
//! - `paste_ok` / `paste_cancel`：点确认框的「粘贴」/「取消」（和按钮、回车、Esc 走同一个方法）；
//! - `assert_reconnect:asking|none`、`reconnect_ok` / `reconnect_cancel`：Ctrl+Shift+R 的重连确认框（REVIEW-1111-U），同上。

#[cfg(debug_assertions)]
mod real {
    use ::gpui::Window;
    use gpui_kit::*;

    use super::super::MainWindow;
    use super::super::dev_script::{Outcome, unescape};

    const KNOWN: &[&str] = &[
        "paste_begin",
        "assert_paste",
        "paste_ok",
        "paste_cancel",
        "assert_reconnect",
        "reconnect_ok",
        "reconnect_cancel",
    ];

    pub fn is_known(name: &str) -> bool {
        KNOWN.contains(&name)
    }

    pub(in crate::main_window) fn run(
        this: &mut MainWindow,
        spec: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Option<Result<Outcome, String>> {
        let (name, arg) = spec.split_once(':').unwrap_or((spec, ""));
        if !is_known(name) {
            return None;
        }
        let ws = this.workspace.clone();
        Some(match name {
            "paste_begin" => {
                let text = unescape(arg);
                ws.update(cx, |ws, cx| ws.begin_paste(text, window, cx));
                Ok(Outcome::Done)
            }
            "paste_ok" => {
                ws.update(cx, |ws, cx| ws.dev_paste_confirm(window, cx));
                Ok(Outcome::Done)
            }
            "paste_cancel" => {
                ws.update(cx, |ws, cx| ws.dev_paste_cancel(window, cx));
                Ok(Outcome::Done)
            }
            "reconnect_ok" => {
                ws.update(cx, |ws, cx| ws.dev_reconnect_confirm(window, cx));
                Ok(Outcome::Done)
            }
            "reconnect_cancel" => {
                ws.update(cx, |ws, cx| ws.dev_reconnect_cancel(window, cx));
                Ok(Outcome::Done)
            }
            "assert_reconnect" => {
                let text = if ws.read(cx).dev_reconnect_asking() {
                    "asking"
                } else {
                    "none"
                };
                if text == arg {
                    Ok(Outcome::Done)
                } else {
                    Err(format!("断言没有成立：重连确认框现在是 {text}"))
                }
            }
            _ => {
                let have = ws.read(cx).dev_paste_state();
                let text = match have {
                    None => "none".to_owned(),
                    Some((lines, recipients, broadcast)) => format!(
                        "{lines},{recipients},{}",
                        if broadcast { "broadcast" } else { "single" }
                    ),
                };
                if text == arg {
                    Ok(Outcome::Done)
                } else {
                    Err(format!("断言没有成立：确认框现在是 {text}"))
                }
            }
        })
    }
}

#[cfg(debug_assertions)]
pub use real::is_known;
#[cfg(debug_assertions)]
pub(super) use real::run;

#[cfg(not(debug_assertions))]
pub fn is_known(_name: &str) -> bool {
    false
}

#[cfg(not(debug_assertions))]
pub(super) fn run(
    _this: &mut super::MainWindow,
    _spec: &str,
    _window: &mut ::gpui::Window,
    _cx: &mut gpui_kit::Context<super::MainWindow>,
) -> Option<Result<super::dev_script::Outcome, String>> {
    None
}
