//! 调试驱动里 V2-U50（终端里标出 AI 执行的命令）要用的 `act:` 动作。只在 debug 构建里编进去；release 里这里
//! 只剩两个空函数。动作模拟「AI 在活动窗格里执行命令」的几个阶段，走和真实流程相同的显示过滤器
//! （不连服务器、不拉智能体），阶段之间用 `sleep:<毫秒>` 拉开，耗时标记就是真实的间隔：
//!
//! - `u50_ai:begin:<智能体名|->:<命令>`：AI 命令开始（`-` = 不知道智能体名）；
//! - `u50_ai:out:<文字>`：命令的输出，`\r` `\n` `\e` 会换成控制字符，不能含 `;`；
//! - `u50_ai:end:<退出码>`：命令结束（结束标记行 + 新提示符）；
//! - `u50_ai:timeout`：AI 超时返回，命令还在跑；
//! - `u50_assert_screen:<文字>`：活动窗格屏幕上有这段文字（条件暂时不成立会重试到步骤超时）。
//! - 开关用 `ui_set:mark_ai_commands:on|off`（`dev_acts_u2`）。

#[cfg(debug_assertions)]
mod real {
    use ::gpui::Window;
    use gpui_kit::*;

    use super::super::MainWindow;
    use super::super::dev_acts_u49::unescape;
    use super::super::dev_script::Outcome;

    const KNOWN: &[&str] = &["u50_ai", "u50_assert_screen"];

    pub fn is_known(name: &str) -> bool {
        KNOWN.contains(&name)
    }

    pub(in crate::main_window) fn run(
        this: &mut MainWindow,
        spec: &str,
        _window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Option<Result<Outcome, String>> {
        let (name, rest) = spec.split_once(':').unwrap_or((spec, ""));
        if !is_known(name) {
            return None;
        }
        Some(run_inner(this, name, rest, cx))
    }

    fn run_inner(
        this: &mut MainWindow,
        name: &str,
        rest: &str,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        if name == "u50_assert_screen" {
            let found = this
                .workspace
                .read(cx)
                .dev_screen_lines()
                .iter()
                .any(|line| line.contains(rest));
            return Ok(if found {
                Outcome::Done
            } else {
                Outcome::Pending
            });
        }
        let (stage, arg) = rest.split_once(':').unwrap_or((rest, ""));
        match stage {
            "begin" => {
                let (agent, command) = arg
                    .split_once(':')
                    .ok_or("u50_ai:begin:<智能体名|->:<命令>")?;
                let agent = (agent != "-").then_some(agent);
                this.workspace.update(cx, |ws, cx| {
                    ws.dev_ai_begin(agent, command);
                    cx.notify();
                });
            }
            "out" => {
                let text = String::from_utf8_lossy(&unescape(arg)).into_owned();
                this.workspace.update(cx, |ws, cx| {
                    ws.dev_ai_output(&text);
                    cx.notify();
                });
            }
            "end" => {
                let code: i32 = arg.parse().map_err(|_| "u50_ai:end:<退出码>")?;
                this.workspace.update(cx, |ws, cx| {
                    ws.dev_ai_end(code);
                    cx.notify();
                });
            }
            "timeout" => {
                this.workspace.update(cx, |ws, cx| {
                    ws.dev_ai_timeout();
                    cx.notify();
                });
            }
            _ => return Err("u50_ai:<begin|out|end|timeout>".to_owned()),
        }
        Ok(Outcome::Done)
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
