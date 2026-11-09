//! 调试驱动里「命令建议」入口要用的 `act:` 动作。整个模块只在 debug 构建里编进去（`mod.rs` 里带
//! `cfg(debug_assertions)`，和 `dev_acts_u35` 等一样），release 里不存在。
//! 动作走和界面按钮同一批方法，不绕过真实的确认步骤：
//!
//! - `assist_open`：点开状态栏上的「命令建议」；`assist_close`：关掉；
//! - `assist_ask:<bash|zsh>`：点「本次启用 Bash / Zsh 集成」（进入确认步骤，还没发送任何东西）；
//! - `assist_confirm`：在确认步骤里点「确认，启用…」（这一步才会往窗格发脚本）；`assist_back`：确认步骤里点「取消」；
//! - `assist_disable`：点「停用本次集成」；
//! - `assert_assist:<closed|status|confirm>`：弹窗现在在哪一步；
//! - `assert_assist_text:<文字>`：状态说明（含上一个动作的提示）里有这段文字；
//! - `assert_assist_integration:<off|handshake|active|failed>`：聚焦窗格的临时集成进度；
//! - `assist_select`：点「选择建议」（状态栏 / 浮层标题条的入口，已在选择模式里就是退出）；
//! - `assist_clear:<current|all>`：设置里的「清除当前目标 / 清除全部」；
//! - `assist_pref:<auto|history>:<on|off>`：设置里的两个开关（自动建议 / 记录命令历史）；
//! - `assert_suggest:<数量>`：聚焦窗格现在展示的候选条数（0 = 没有浮层）；`assert_suggest_text:<文字>`：候选里有这条命令；
//!   `assert_suggest_mode:<on|off>`：现在是不是在选择模式；`assert_suggest_selected:<序号|none>`：选中了第几条（从 0 数）。
//!   候选要等输入稳定约 80 ms 才出现，这几个断言遇到"还没有"时会继续等（`Pending`）。

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
        "assist_select",
        "assist_clear",
        "assist_pref",
        "assert_suggest",
        "assert_suggest_text",
        "assert_suggest_mode",
        "assert_suggest_selected",
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
            "assist_select" => {
                this.assist_select_clicked(window, cx);
                Ok(Outcome::Done)
            }
            "assist_clear" => {
                match arg {
                    "current" => this.assist_clear_history(false, cx),
                    "all" => this.assist_clear_history(true, cx),
                    _ => return Err("参数只能是 current 或 all".to_owned()),
                }
                Ok(Outcome::Done)
            }
            "assist_pref" => {
                let (which, value) = arg.split_once(':').ok_or("要写成 auto:on 这样")?;
                let on = match value {
                    "on" => true,
                    "off" => false,
                    _ => return Err("值只能是 on 或 off".to_owned()),
                };
                match which {
                    "auto" => this.set_assist_enabled(on, window, cx),
                    "history" => this.set_assist_history(on, window, cx),
                    _ => return Err("开关只能是 auto 或 history".to_owned()),
                }
                Ok(Outcome::Done)
            }
            "assert_suggest" => {
                let want: usize = arg.parse().map_err(|_| "要给一个数量".to_owned())?;
                let have = this.workspace.read(cx).dev_suggest_texts().len();
                settled(have == want, have == 0 && want > 0, || {
                    format!("现在有 {have} 条候选，不是 {want}")
                })
            }
            "assert_suggest_text" => {
                let texts = this.workspace.read(cx).dev_suggest_texts();
                settled(
                    texts.iter().any(|text| text == arg),
                    texts.is_empty(),
                    || "候选里没有这条命令".to_owned(),
                )
            }
            "assert_suggest_mode" => {
                let on = this.workspace.read(cx).suggest_in_select_mode();
                match arg {
                    "on" | "off" => settled(on == (arg == "on"), false, || {
                        format!("选择模式现在是{}", if on { "开" } else { "关" })
                    }),
                    _ => Err("参数只能是 on 或 off".to_owned()),
                }
            }
            "assert_suggest_selected" => {
                let have = this.workspace.read(cx).dev_suggest_selected();
                let want = match arg {
                    "none" => None,
                    other => Some(
                        other
                            .parse::<usize>()
                            .map_err(|_| "要给序号或 none".to_owned())?,
                    ),
                };
                settled(have == want, false, || {
                    format!("选中的是 {have:?}，不是 {want:?}")
                })
            }
            _ => Err(format!("未实现的动作 {name}")),
        }
    }

    /// 断言结果：满足就完成；不满足但"还可能出现"（候选要等输入稳定）就继续等；否则报错。
    fn settled(
        ok: bool,
        keep_waiting: bool,
        why: impl FnOnce() -> String,
    ) -> Result<Outcome, String> {
        if ok {
            Ok(Outcome::Done)
        } else if keep_waiting {
            Ok(Outcome::Pending)
        } else {
            Err(why())
        }
    }
}

pub use real::is_known;
pub(super) use real::run;
