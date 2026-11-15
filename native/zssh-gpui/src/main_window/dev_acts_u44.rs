//! 调试驱动里 V2-U44（关闭前确认、断线自动重连）要用的 `act:` 动作。只在 debug 构建里编进去；release 里这里
//! 只剩两个空函数。`u44_close*` 走和标签上的 ✕ / 右键菜单同一批方法（会弹确认框），不是调试脚本 `close:` 那种
//! 不问的程序内部关闭。
//!
//! - `u44_close:<标签下标>`：像点了这个标签上的 ✕；
//! - `u44_close_scope:<标签下标>:<others|left|right|all>`：像在这个标签上右键选了「关闭其他 / 左侧 / 右侧 / 全部」；
//! - `u44_assert_ask:<on|off>`：主窗口现在开没开关闭确认框（条件暂时不成立会重试到步骤超时）；
//! - `u44_assert_term_ask:<on|off>`：第一个独立终端窗口现在开没开关闭确认层；
//! - `u44_auto:<窗格下标>:<waiting|connecting|stopped>[:<第几次>[:<剩几秒>]]`：把活动标签的这个会话窗格摆成
//!   自动重连的某个阶段（截图用，不需要真有会掉线的 SSH 服务器）。

#[cfg(debug_assertions)]
mod real {
    use ::gpui::Window;
    use gpui_kit::*;

    use super::super::MainWindow;
    use super::super::dev_script::Outcome;
    use super::super::modal::Modal;
    use crate::close_confirm::CloseScope;

    const KNOWN: &[&str] = &[
        "u44_close",
        "u44_close_scope",
        "u44_assert_ask",
        "u44_assert_term_ask",
        "u44_auto",
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
        let mut parts = spec.split(':');
        let name = parts.next().unwrap_or("");
        if !is_known(name) {
            return None;
        }
        let args: Vec<&str> = parts.collect();
        Some(run_inner(this, name, &args, window, cx))
    }

    fn number<T: std::str::FromStr>(raw: Option<&&str>, what: &str) -> Result<T, String> {
        raw.and_then(|raw| raw.parse().ok())
            .ok_or_else(|| format!("{what}要是数字"))
    }

    fn on_off(raw: Option<&&str>) -> Result<bool, String> {
        match raw.copied() {
            Some("on") => Ok(true),
            Some("off") => Ok(false),
            _ => Err("参数只能是 on 或 off".to_owned()),
        }
    }

    fn run_inner(
        this: &mut MainWindow,
        name: &str,
        args: &[&str],
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        match name {
            "u44_close" => {
                let index: usize = number(args.first(), "标签下标")?;
                this.close_tab(index, window, cx);
                Ok(Outcome::Done)
            }
            "u44_close_scope" => {
                let index: usize = number(args.first(), "标签下标")?;
                let scope = match args.get(1).copied() {
                    Some("others") => CloseScope::Others,
                    Some("left") => CloseScope::Left,
                    Some("right") => CloseScope::Right,
                    Some("all") => CloseScope::All,
                    _ => return Err("范围只能是 others / left / right / all".to_owned()),
                };
                this.close_tabs(index, scope, window, cx);
                Ok(Outcome::Done)
            }
            "u44_assert_ask" => {
                let want = on_off(args.first())?;
                let open = matches!(this.modal, Some(Modal::CloseTabs(_)));
                Ok(if open == want {
                    Outcome::Done
                } else {
                    Outcome::Pending
                })
            }
            "u44_assert_term_ask" => {
                let want = on_off(args.first())?;
                let handle = crate::term_window::handles(cx)
                    .first()
                    .copied()
                    .ok_or_else(|| "没有独立终端窗口".to_owned())?;
                let open = handle
                    .read_with(cx, |view, _| view.dev_close_ask_open())
                    .map_err(|_| "窗口已经关闭".to_owned())?;
                Ok(if open == want {
                    Outcome::Done
                } else {
                    Outcome::Pending
                })
            }
            _ => {
                let pane: usize = number(args.first(), "窗格下标")?;
                let kind = args.get(1).copied().unwrap_or("");
                if !matches!(kind, "waiting" | "connecting" | "stopped") {
                    return Err("阶段只能是 waiting / connecting / stopped".to_owned());
                }
                let attempt: u32 = args.get(2).map_or(Ok(1), |raw| {
                    raw.parse().map_err(|_| "第几次要是数字".to_owned())
                })?;
                let secs: u64 = args.get(3).map_or(Ok(27), |raw| {
                    raw.parse().map_err(|_| "剩几秒要是数字".to_owned())
                })?;
                if this.workspace.update(cx, |ws, cx| {
                    ws.dev_force_auto(pane, kind, attempt, secs, cx)
                }) {
                    Ok(Outcome::Done)
                } else {
                    Err("这个窗格不是会话窗格".to_owned())
                }
            }
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
