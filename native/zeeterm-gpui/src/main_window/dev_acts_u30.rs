//! 调试驱动里 V2-U30（断线遮罩）要用的 `act:` 动作。只在 debug 构建里编进去；release 里这里只剩两个空函数。
//! 动作只读窗格的真实状态；点遮罩、按 R、滚轮都用脚本里的 `click` / `key` / `wheel` 步骤走真实的输入分发。
//!
//! - `assert_overlay:<main|pop>:<窗格下标>:<on|off>`：这个窗格盖没盖断线遮罩（`pop` 是第一个独立终端窗口）；
//!   条件暂时不成立会重试到步骤超时；
//! - `assert_overlay_reason:<main|pop>:<窗格下标>:<文字>`：遮罩上的原因含这段文字；
//! - `assert_scrolled:<main|pop>:<窗格下标>:<on|off>`：这个窗格有没有翻在回滚里（滚轮透传验证用）。

#[cfg(debug_assertions)]
mod real {
    use ::gpui::Window;
    use gpui_kit::*;

    use super::super::MainWindow;
    use super::super::dev_script::Outcome;
    use crate::terminal_workspace::TerminalWorkspace;

    const KNOWN: &[&str] = &["assert_overlay", "assert_overlay_reason", "assert_scrolled"];

    pub fn is_known(name: &str) -> bool {
        KNOWN.contains(&name)
    }

    pub(in crate::main_window) fn run(
        this: &mut MainWindow,
        spec: &str,
        _window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Option<Result<Outcome, String>> {
        let mut parts = spec.splitn(4, ':');
        let name = parts.next().unwrap_or("");
        if !is_known(name) {
            return None;
        }
        let which = parts.next().unwrap_or("");
        let pane = parts.next().unwrap_or("");
        let want = parts.next().unwrap_or("");
        Some(check(this, name, which, pane, want, cx))
    }

    fn check(
        this: &MainWindow,
        name: &str,
        which: &str,
        pane: &str,
        want: &str,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        let pane: usize = pane.parse().map_err(|_| "窗格下标要是数字".to_owned())?;
        let workspace: Entity<TerminalWorkspace> = match which {
            "main" => this.workspace.clone(),
            "pop" => {
                let handle = crate::term_window::handles(cx)
                    .first()
                    .copied()
                    .ok_or_else(|| "没有独立终端窗口".to_owned())?;
                handle
                    .read_with(cx, |view, _| view.dev_workspace())
                    .map_err(|_| "窗口已经关闭".to_owned())?
            }
            _ => return Err("窗口只能是 main 或 pop".to_owned()),
        };
        let ws = workspace.read(cx);
        let ok = match name {
            "assert_overlay" => {
                let shown = ws
                    .dev_overlay(pane)
                    .ok_or_else(|| "没有这个窗格".to_owned())?
                    .is_some();
                shown == on_off(want)?
            }
            "assert_overlay_reason" => ws
                .dev_overlay(pane)
                .ok_or_else(|| "没有这个窗格".to_owned())?
                .is_some_and(|reason| reason.contains(want)),
            _ => {
                let scrolled = ws
                    .dev_scrolled_back(pane)
                    .ok_or_else(|| "没有这个窗格".to_owned())?;
                scrolled == on_off(want)?
            }
        };
        Ok(if ok { Outcome::Done } else { Outcome::Pending })
    }

    fn on_off(raw: &str) -> Result<bool, String> {
        match raw {
            "on" => Ok(true),
            "off" => Ok(false),
            _ => Err("参数只能是 on 或 off".to_owned()),
        }
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
