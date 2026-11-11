//! 调试驱动里 V2-U29（SSH 提问改成应用内对话框）要用的 `act:` 动作。只在 debug 构建里编进去；release 里只剩两个空函数。
//! 请求走的是和真实连接同一条中转站（`ssh_prompt::hub`），按键用脚本的 `key:escape` / `key:enter` 走真实按键分发：
//!
//! - `hostkey_demo:<unknown|other|changed|revoked>`：放一个假的主机指纹确认请求（主机用文档保留域名）；
//! - `prompt_demo:<password|rejected|ki>`：放一个假的输入请求（一个密码框 / 保存的密码被拒后重新询问的密码框 / 带说明的两题 keyboard-interactive）；
//! - `assert_ssh_prompt:<open|closed>`：本窗口现在有没有提问对话框；
//! - `ssh_prompt_press:<按钮文字>`：点指纹对话框上的一个按钮（和鼠标点同一个方法）；
//! - `ssh_prompt_type:<第几个输入框>|<文字>`：往输入对话框里填内容；
//! - `assert_host_password:<主机名>|<期望的密码>`：保险库里这台主机存的密码是不是它（失败时不回显密码）；
//! - `ssh_prompt_remember:<on|off>`：勾 / 取消「用这个密码更新保存的密码」（只有保存的密码被拒后的重新询问有；
//!   勾上后 `assert_ssh_answer` 看到的回答末尾带 `;remember`）；
//! - `assert_ssh_answer:<reject|trust_save|trust_once|cancel|answers:<a,b>>`：最早一个已回答的演示请求的回答，
//!   对不上就失败；`assert_ssh_answer:none` = 还没有任何演示请求被回答；
//! - `assert_ssh_waiting:<个数>`：还没被回答的演示请求有几个（排队时只显示一个，其余在这里等）。

#[cfg(debug_assertions)]
mod real {
    use ::gpui::Window;
    use gpui_kit::*;

    use super::super::MainWindow;
    use super::super::dev_script::Outcome;
    use crate::ssh_prompt_ui::dev;

    const KNOWN: &[&str] = &[
        "hostkey_demo",
        "prompt_demo",
        "assert_ssh_prompt",
        "ssh_prompt_press",
        "ssh_prompt_type",
        "ssh_prompt_remember",
        "assert_host_password",
        "assert_ssh_answer",
        "assert_ssh_waiting",
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
        Some(run_inner(this, name, arg, window, cx))
    }

    fn run_inner(
        this: &mut MainWindow,
        name: &str,
        arg: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        let layer = this.ssh_prompt.clone();
        match name {
            "hostkey_demo" => dev::host_key_demo(arg).map(|()| Outcome::Done),
            "prompt_demo" => dev::prompt_demo(arg).map(|()| Outcome::Done),
            "assert_ssh_prompt" => {
                let open = layer.read(cx).is_open();
                match arg {
                    "open" if open => Ok(Outcome::Done),
                    "closed" if !open => Ok(Outcome::Done),
                    "open" | "closed" => Err(format!(
                        "断言没有成立：提问对话框现在是{}的",
                        if open { "开着" } else { "关着" }
                    )),
                    _ => Err("参数只能是 open 或 closed".to_owned()),
                }
            }
            "ssh_prompt_press" => layer
                .update(cx, |layer, cx| layer.dev_press(arg, window, cx))
                .map(|()| Outcome::Done),
            "ssh_prompt_type" => {
                let (index, text) = arg
                    .split_once('|')
                    .ok_or("参数要写成 <第几个输入框>|<文字>")?;
                let index: usize = index.trim().parse().map_err(|_| "输入框序号要是数字")?;
                layer
                    .update(cx, |layer, cx| layer.dev_type(index, text, window, cx))
                    .map(|()| Outcome::Done)
            }
            "ssh_prompt_remember" => {
                let ticked = match arg {
                    "on" => true,
                    "off" => false,
                    _ => return Err("参数只能是 on 或 off".to_owned()),
                };
                layer
                    .update(cx, |layer, cx| layer.dev_set_remember(ticked, cx))
                    .map(|()| Outcome::Done)
            }
            "assert_host_password" => {
                // 保险库里这台主机现在存的密码是不是期望的那个；断言失败的提示不回显任何密码。
                let (name, expected) = arg
                    .split_once('|')
                    .ok_or("参数要写成 <主机名>|<期望的密码>")?;
                let db = crate::host_db::open_database().map_err(|err| err.to_string())?;
                let host = db
                    .host_list()
                    .map_err(|err| err.to_string())?
                    .into_iter()
                    .find(|host| host.name == name)
                    .ok_or("没有这台主机")?;
                let saved = db
                    .host_saved_password(host.id)
                    .map_err(|_| "这台主机没有保存的密码")?;
                if saved.as_str() == expected {
                    Ok(Outcome::Done)
                } else {
                    Err("断言没有成立：保存的密码和期望的不一样".to_owned())
                }
            }
            "assert_ssh_answer" => {
                let have = dev::take_answer();
                match (arg, have) {
                    ("none", None) => Ok(Outcome::Done),
                    (want, Some(have)) if want == have => Ok(Outcome::Done),
                    (_, have) => Err(format!(
                        "断言没有成立：演示请求的回答是 {}",
                        have.as_deref().unwrap_or("（还没有）")
                    )),
                }
            }
            "assert_ssh_waiting" => {
                let want: usize = arg.trim().parse().map_err(|_| "参数要是数字")?;
                let have = dev::waiting();
                if have == want {
                    Ok(Outcome::Done)
                } else {
                    Err(format!("断言没有成立：还有 {have} 个演示请求在等回答"))
                }
            }
            _ => Err(format!("不认识的动作：{name}")),
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
