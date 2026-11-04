//! 调试驱动里 V2-U35（启动时锁定 + 查看 / 复制密码前验证）要用的 `act:` 动作。只在 debug 构建里编进去；
//! release 里这里只剩两个空函数。动作走和界面控件同一批方法，不绕过真实的验证逻辑。
//!
//! - `lock_settings`：打开 设置 → 锁屏（不弹对话框）；
//! - `lock_start:<on|off>`：设置页「启动时锁定」开关背后的写入（没设密码时打不开，和界面一样给错误提示）；
//! - `assert_lock_start:<flag|effective>:<on|off>`：重新读库，开关本身 / 开关加锁屏启用后的“启动即锁”；
//! - `copy_host_pw:<主机>` / `copy_host_conn:<主机>`：右键「复制密码」/「复制连接信息」；主机写 `@名字` 或 id；
//!   复制内容用 `ai_clip` 断言（抓取模式下不碰系统剪贴板）；
//! - `reveal_window:<fresh|stale>`：5 分钟窗口期是否还有效（按“锁屏已启用”算）；`reveal_reset`：让窗口期作废；
//! - `assert_reveal_ask:<open|closed>`：验证小对话框开着没有；`assert_reveal_action:<eye_host|eye_info|copy_info|copy_pw|copy_conn>`：
//!   它在等哪个入口；`reveal_fill:<密码>` / `reveal_submit` / `reveal_cancel` / `reveal_idle`（等后台核对做完）；
//!   `assert_reveal_error:<文字>`：对话框里的提示含这段文字。

#[cfg(debug_assertions)]
mod real {
    use std::time::Instant;

    use ::gpui::Window;
    use gpui_kit::*;
    use zssh_core::app_lock::LockConfig;

    use super::super::MainWindow;
    use super::super::dev_script::Outcome;
    use super::super::reveal_ui::RevealAction;
    use crate::app_lock as lock_ui;

    const KNOWN: &[&str] = &[
        "lock_settings",
        "lock_start",
        "assert_lock_start",
        "copy_host_pw",
        "copy_host_conn",
        "reveal_window",
        "reveal_reset",
        "assert_reveal_ask",
        "assert_reveal_action",
        "reveal_fill",
        "reveal_submit",
        "reveal_cancel",
        "reveal_idle",
        "assert_reveal_error",
    ];

    pub fn is_known(name: &str) -> bool {
        KNOWN.contains(&name)
    }

    fn on_off(raw: &str) -> Result<bool, String> {
        match raw {
            "on" => Ok(true),
            "off" => Ok(false),
            _ => Err("参数只能是 on 或 off".to_owned()),
        }
    }

    fn eventually(ok: bool) -> Result<Outcome, String> {
        Ok(if ok { Outcome::Done } else { Outcome::Pending })
    }

    fn host_id(this: &MainWindow, raw: &str) -> Result<i64, String> {
        if let Some(name) = raw.strip_prefix('@') {
            return this
                .store
                .hosts()
                .iter()
                .find(|host| host.name == name)
                .map(|host| host.id)
                .ok_or_else(|| "没有这个名字的主机".to_owned());
        }
        raw.parse::<i64>()
            .map_err(|_| "主机引用不是数字也不是 @名字".to_owned())
    }

    fn action_name(action: RevealAction) -> &'static str {
        match action {
            RevealAction::HostDialogEye => "eye_host",
            RevealAction::InfoEye(_) => "eye_info",
            RevealAction::InfoCopyPassword(_) => "copy_info",
            RevealAction::CopyHostPassword(_) => "copy_pw",
            RevealAction::CopyHostInfo(_) => "copy_conn",
        }
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

    fn act(
        this: &mut MainWindow,
        name: &str,
        arg: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        match name {
            "lock_settings" => {
                this.open_lock_settings(window, cx);
                Ok(Outcome::Done)
            }
            "lock_start" => {
                let on = on_off(arg)?;
                this.set_lock_on_start(on, window, cx);
                Ok(Outcome::Done)
            }
            "assert_lock_start" => {
                let (which, want) = arg
                    .split_once(':')
                    .ok_or("assert_lock_start:<flag|effective>:<on|off>")?;
                let want = on_off(want)?;
                let config = LockConfig::load(this.store.db()?);
                let have = match which {
                    "flag" => config.lock_on_start,
                    "effective" => config.start_locked(),
                    _ => return Err("只有 flag / effective".to_owned()),
                };
                if have == want {
                    Ok(Outcome::Done)
                } else {
                    Err(format!("库里是 {have}，不是 {want}"))
                }
            }
            "copy_host_pw" => {
                let id = host_id(this, arg)?;
                this.copy_host_password(id, window, cx);
                Ok(Outcome::Done)
            }
            "copy_host_conn" => {
                let id = host_id(this, arg)?;
                this.copy_host_info(id, window, cx);
                Ok(Outcome::Done)
            }
            "reveal_window" => {
                let stale = lock_ui::reveal_needs_verification(true, Instant::now());
                match arg {
                    "fresh" => eventually(!stale),
                    "stale" => eventually(stale),
                    _ => Err("参数只能是 fresh 或 stale".to_owned()),
                }
            }
            "reveal_reset" => {
                lock_ui::dev_reset_reveal();
                Ok(Outcome::Done)
            }
            "assert_reveal_ask" => {
                let open = this.reveal_ask.is_some();
                match arg {
                    "open" => eventually(open),
                    "closed" => eventually(!open),
                    _ => Err("参数只能是 open 或 closed".to_owned()),
                }
            }
            "assert_reveal_action" => match &this.reveal_ask {
                Some(ask) if action_name(ask.action) == arg => Ok(Outcome::Done),
                Some(ask) => Err(format!("在等 {}，不是 {arg}", action_name(ask.action))),
                None => Err("验证对话框没有开着".to_owned()),
            },
            "reveal_fill" => {
                let text = super::super::dev_script::unescape(arg);
                let input = this
                    .reveal_ask
                    .as_ref()
                    .map(|ask| ask.input.clone())
                    .ok_or("验证对话框没有开着")?;
                input.update(cx, |state, cx| state.set_value(text, window, cx));
                Ok(Outcome::Done)
            }
            "reveal_submit" => {
                this.reveal_ask_submit(window, cx);
                Ok(Outcome::Done)
            }
            "reveal_cancel" => {
                this.reveal_ask_cancel(window, cx);
                Ok(Outcome::Done)
            }
            "reveal_idle" => eventually(this.reveal_ask.as_ref().is_none_or(|ask| !ask.busy)),
            "assert_reveal_error" => {
                let text = arg;
                match this.reveal_ask.as_ref().and_then(|ask| ask.error.clone()) {
                    Some(error) if error.contains(text) => Ok(Outcome::Done),
                    Some(error) => Err(format!("提示是「{error}」，不含「{text}」")),
                    // 后台核对还没回来：等一等。
                    None => Ok(Outcome::Pending),
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
