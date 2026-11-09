//! 调试驱动里「命令建议」入口要用的 `act:` 动作。只在 debug 构建里编进去；release 里这里只剩两个空函数。
//! 动作走和界面按钮同一批方法，不绕过真实的确认步骤：
//!
//! - `assist_open`：点开状态栏上的「命令建议」；`assist_close`：关掉；
//! - `assist_ask:<bash|zsh>`：点「本次启用 Bash / Zsh 集成」（进入确认步骤，还没发送任何东西）；
//! - `assist_confirm`：在确认步骤里点「确认，启用…」（这一步才会往窗格发脚本）；`assist_back`：确认步骤里点「取消」；
//! - `assist_disable`：点「停用本次集成」；
//! - `assert_assist:<closed|status|confirm>`：弹窗现在在哪一步；
//! - `assert_assist_text:<文字>`：状态说明（含上一个动作的提示）里有这段文字；
//! - `assert_assist_integration:<off|handshake|active|failed>`：聚焦窗格的临时集成进度。

#[cfg(debug_assertions)]
mod real {
    use ::gpui::Window;
    use gpui_kit::*;
    use zssh_core::terminal_assist::integration::ShellKind;

    use super::super::MainWindow;
    use super::super::assist_ui::{AssistStep, status_lines};
    use super::super::dev_script::Outcome;
    use crate::terminal_workspace::IntegrationState;

    const KNOWN: &[&str] = &[
        "assist_open",
        "assist_close",
        "assist_ask",
        "assist_confirm",
        "assist_back",
        "assist_disable",
        "assert_assist",
        "assert_assist_text",
        "assert_assist_integration",
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
        Some(act(this, name, arg, window, cx))
    }

    fn shell(raw: &str) -> Result<ShellKind, String> {
        match raw {
            "bash" => Ok(ShellKind::Bash),
            "zsh" => Ok(ShellKind::Zsh),
            _ => Err("参数只能是 bash 或 zsh".to_owned()),
        }
    }

    fn act(
        this: &mut MainWindow,
        name: &str,
        arg: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        match name {
            "assist_open" => {
                if this.assist_dialog.is_none() {
                    this.toggle_assist_dialog(window, cx);
                }
                Ok(Outcome::Done)
            }
            "assist_close" => {
                this.close_assist_dialog(window, cx);
                Ok(Outcome::Done)
            }
            "assist_ask" => {
                if this.assist_dialog.is_none() {
                    return Err("命令建议弹窗没有开着".to_owned());
                }
                this.assist_ask(shell(arg)?, cx);
                Ok(Outcome::Done)
            }
            "assist_confirm" => {
                let Some(AssistStep::Confirm(kind)) =
                    this.assist_dialog.as_ref().map(|dialog| dialog.step)
                else {
                    return Err("不在确认步骤".to_owned());
                };
                this.assist_start(kind, cx);
                Ok(Outcome::Done)
            }
            "assist_back" => {
                this.assist_back(cx);
                Ok(Outcome::Done)
            }
            "assist_disable" => {
                this.assist_stop(cx);
                Ok(Outcome::Done)
            }
            "assert_assist" => {
                let have = match this.assist_dialog.as_ref().map(|dialog| dialog.step) {
                    None => "closed",
                    Some(AssistStep::Status) => "status",
                    Some(AssistStep::Confirm(_)) => "confirm",
                };
                if have == arg {
                    Ok(Outcome::Done)
                } else {
                    Err(format!("弹窗在「{have}」，不是「{arg}」"))
                }
            }
            "assert_assist_text" => {
                let view = this.workspace.read(cx).assist_view();
                let mut text = status_lines(view).join("\n");
                if let Some(notice) = this
                    .assist_dialog
                    .as_ref()
                    .and_then(|dialog| dialog.notice.clone())
                {
                    text.push('\n');
                    text.push_str(&notice);
                }
                if text.contains(arg) {
                    Ok(Outcome::Done)
                } else {
                    Err("状态说明里没有这段文字".to_owned())
                }
            }
            "assert_assist_integration" => {
                let state = this
                    .workspace
                    .read(cx)
                    .assist_view()
                    .map(|view| view.integration)
                    .ok_or("没有标签")?;
                let have = match state {
                    IntegrationState::Off => "off",
                    IntegrationState::Handshake { .. } => "handshake",
                    IntegrationState::Active { .. } => "active",
                    IntegrationState::Failed { .. } => "failed",
                };
                // 握手 / 就绪是异步的：没到就继续等（脚本的 sleep 或超时兜底）。
                if have == arg {
                    Ok(Outcome::Done)
                } else if arg == "active" && have == "handshake" {
                    Ok(Outcome::Pending)
                } else {
                    Err(format!("临时集成是「{have}」，不是「{arg}」"))
                }
            }
            _ => Err(format!("未实现的动作 {name}")),
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
