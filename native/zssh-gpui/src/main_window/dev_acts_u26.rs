//! 调试驱动里 V2-U26（锁屏）要用的 `act:` 动作。只在 debug 构建里编进去；release 里这里只剩两个空函数。
//! 动作都走和界面控件同一批方法；`assert_*` 读的是**落盘**的值或界面真实状态，不对就让脚本失败
//! （条件暂时不成立的会重试到步骤超时，因为密码校验在后台线程里做）。
//!
//! - `lock_enable:<密码>`：直接启用锁屏（和“设置密码”对话框同一个后台任务，正式参数）；
//! - `lock_dialog:<enable|change|disable|reset>`：打开对应的密码对话框；`lock_fill:<第几个框>:<文字>`
//!   往对话框的输入框里填；`lock_submit`：点对话框的确定；
//! - `lock_dialog_idle`：等对话框的后台任务做完；`assert_lock_dialog_error:<文字>`：对话框里的提示含这段文字；
//!   `assert_lock_dialog:<open|closed>`；
//! - `lock_now`：和点锁按钮同一个方法；`lock_click`：就是锁按钮的左键；
//! - `lock_type:<文字>` / `lock_try`：在锁屏页的密码框里填字 / 按解锁；`lock_forgot`：点“忘记密码？”；
//!   `assert_lock_screen_error:<文字>`：锁屏页上的提示含这段文字；
//! - `lock_forgot_reset`：设置里“忘记密码”那个按钮（Windows 验证用 `ZSSH_LOCK_OS_STUB` 的桩，不会弹系统窗口）；
//! - `lock_idle:<分钟>`、`lock_tray:<on|off>`：设置页里那两项背后的写入；
//! - `lock_reset_backoff`：把输错计数清零（脚本不用真等 60 秒）；
//! - `assert_locked:<on|off>`、`assert_lock_enabled:<on|off>`（重新读库）、`assert_lock_wait:<on|off>`：有没有在等待；
//! - `assert_lock_setting:<idle|tray>:<值>`：落盘的自动锁定设置；
//! - `assert_term_lacks:<文字>` / `assert_term_has:<文字>`：活动标签的屏幕上有没有这段文字。

#[cfg(debug_assertions)]
mod real {
    use std::time::Instant;

    use ::gpui::Window;
    use gpui_kit::*;
    use zssh_core::app_lock::{KdfCost, LockConfig};

    use super::super::MainWindow;
    use super::super::app_lock_ui::LockDialogKind;
    use super::super::dev_script::Outcome;
    use super::super::modal::Modal;
    use crate::app_lock::{self as lock_ui, Job, JobOutcome};

    const KNOWN: &[&str] = &[
        "lock_enable",
        "lock_dialog",
        "lock_fill",
        "lock_submit",
        "lock_dialog_idle",
        "assert_lock_dialog_error",
        "assert_lock_dialog",
        "lock_now",
        "lock_click",
        "lock_type",
        "lock_try",
        "lock_forgot",
        "assert_lock_screen_error",
        "lock_forgot_reset",
        "lock_idle",
        "lock_tray",
        "lock_reset_backoff",
        "assert_locked",
        "assert_lock_enabled",
        "assert_lock_wait",
        "assert_lock_setting",
        "assert_term_lacks",
        "assert_term_has",
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

    /// 条件成立 `Done`，暂时不成立 `Pending`（脚本会重试到步骤超时）。
    fn eventually(ok: bool) -> Result<Outcome, String> {
        Ok(if ok { Outcome::Done } else { Outcome::Pending })
    }

    fn kind_of(raw: &str) -> Result<LockDialogKind, String> {
        match raw {
            "enable" => Ok(LockDialogKind::Enable),
            "change" => Ok(LockDialogKind::Change),
            "disable" => Ok(LockDialogKind::Disable),
            "reset" => Ok(LockDialogKind::Reset),
            _ => Err("对话框只有 enable / change / disable / reset".to_owned()),
        }
    }

    fn dialog_error(this: &MainWindow) -> Option<String> {
        match &this.modal {
            Some(Modal::AppLock(dialog)) => dialog.error.clone(),
            _ => None,
        }
    }

    pub(in crate::main_window) fn run(
        this: &mut MainWindow,
        spec: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Option<Result<Outcome, String>> {
        let mut parts = spec.splitn(3, ':');
        let name = parts.next().unwrap_or("");
        if !is_known(name) {
            return None;
        }
        let a = parts.next().unwrap_or("");
        let b = parts.next().unwrap_or("");
        Some(match name {
            "lock_enable" => {
                let outcome = this.store.db().map(|db| {
                    lock_ui::run_job(
                        db,
                        Job::Enable {
                            password: a.to_owned(),
                        },
                        KdfCost::STANDARD,
                    )
                });
                match outcome {
                    Ok(JobOutcome::Done) => {
                        this.reload_lock_config();
                        cx.notify();
                        Ok(Outcome::Done)
                    }
                    Ok(other) => Err(format!("启用失败：{other:?}")),
                    Err(err) => Err(err),
                }
            }
            "lock_dialog" => kind_of(a).map(|kind| {
                this.open_lock_dialog(kind, window, cx);
                Outcome::Done
            }),
            "lock_fill" => fill_dialog_field(this, a, b, window, cx),
            "lock_submit" => {
                this.confirm_modal(window, cx);
                Ok(Outcome::Done)
            }
            "lock_dialog_idle" => eventually(match &this.modal {
                Some(Modal::AppLock(dialog)) => !dialog.busy,
                _ => true,
            }),
            "assert_lock_dialog_error" => {
                let text = rest_after(spec, "assert_lock_dialog_error:");
                match dialog_error(this) {
                    Some(error) if error.contains(text) => Ok(Outcome::Done),
                    Some(error) => Err(format!("对话框的提示是「{error}」，不含「{text}」")),
                    None => Err("对话框里现在没有提示".to_owned()),
                }
            }
            "assert_lock_dialog" => {
                let open = matches!(this.modal, Some(Modal::AppLock(_)));
                match a {
                    "open" => eventually(open),
                    "closed" => eventually(!open),
                    _ => Err("参数只能是 open 或 closed".to_owned()),
                }
            }
            "lock_now" => {
                this.lock_now(cx);
                Ok(Outcome::Done)
            }
            "lock_click" => {
                this.lock_button_clicked(window, cx);
                Ok(Outcome::Done)
            }
            "lock_type" => {
                let text = rest_after(spec, "lock_type:").to_owned();
                match this.app_lock.screen.clone() {
                    Some(screen) => {
                        screen.update(cx, |screen, cx| screen.dev_type(&text, window, cx));
                        Ok(Outcome::Done)
                    }
                    None => Err("锁屏页还没画过".to_owned()),
                }
            }
            "lock_try" => match this.app_lock.screen.clone() {
                Some(screen) => {
                    screen.update(cx, |screen, cx| screen.dev_submit(window, cx));
                    Ok(Outcome::Done)
                }
                None => Err("锁屏页还没画过".to_owned()),
            },
            "lock_forgot" => match this.app_lock.screen.clone() {
                Some(screen) => {
                    screen.update(cx, |screen, cx| screen.dev_toggle_forgot(cx));
                    Ok(Outcome::Done)
                }
                None => Err("锁屏页还没画过".to_owned()),
            },
            "assert_lock_screen_error" => {
                let text = rest_after(spec, "assert_lock_screen_error:");
                match this.app_lock.screen.clone() {
                    Some(screen) => match screen.read(cx).dev_error() {
                        Some(error) if error.contains(text) => Ok(Outcome::Done),
                        // 校验在后台线程里，结果还没回来：等一等。
                        None => Ok(Outcome::Pending),
                        Some(error) => Err(format!("锁屏页的提示是「{error}」，不含「{text}」")),
                    },
                    None => Err("锁屏页还没画过".to_owned()),
                }
            }
            "lock_forgot_reset" => {
                this.start_forgot_reset(window, cx);
                Ok(Outcome::Done)
            }
            "lock_idle" => a
                .parse::<u32>()
                .map_err(|_| "分钟要是数字".to_owned())
                .and_then(|minutes| {
                    let db = this.store.db()?;
                    zssh_core::app_lock::save_idle_minutes(db, minutes)
                        .map_err(|err| err.to_string())?;
                    this.reload_lock_config();
                    cx.notify();
                    Ok(Outcome::Done)
                }),
            "lock_tray" => on_off(a).and_then(|on| {
                let db = this.store.db()?;
                zssh_core::app_lock::save_lock_on_tray(db, on).map_err(|err| err.to_string())?;
                this.reload_lock_config();
                cx.notify();
                Ok(Outcome::Done)
            }),
            "lock_reset_backoff" => {
                lock_ui::dev_reset_backoff();
                Ok(Outcome::Done)
            }
            "assert_locked" => on_off(a).and_then(|want| eventually(lock_ui::is_locked() == want)),
            "assert_lock_enabled" => on_off(a).and_then(|want| {
                // 重新读库，不看内存里的缓存。
                let have = LockConfig::load(this.store.db()?).status.is_enabled();
                if have == want {
                    Ok(Outcome::Done)
                } else {
                    Err(format!("库里启用状态是 {have}，不是 {want}"))
                }
            }),
            "assert_lock_wait" => on_off(a).and_then(|want| {
                let waiting = lock_ui::remaining_wait(Instant::now()).is_some();
                if waiting == want {
                    Ok(Outcome::Done)
                } else {
                    Err(format!("等待状态是 {waiting}，不是 {want}"))
                }
            }),
            "assert_lock_setting" => {
                let db = match this.store.db() {
                    Ok(db) => db,
                    Err(err) => return Some(Err(err)),
                };
                let config = LockConfig::load(db);
                let have = match a {
                    "idle" => config.idle_minutes.to_string(),
                    "tray" => if config.lock_on_tray { "on" } else { "off" }.to_owned(),
                    _ => return Some(Err("只有 idle / tray".to_owned())),
                };
                if have == b {
                    Ok(Outcome::Done)
                } else {
                    Err(format!("落盘的是 {have}，不是 {b}"))
                }
            }
            "assert_term_lacks" | "assert_term_has" => {
                let prefix = format!("{name}:");
                let text = rest_after(spec, &prefix);
                let found = this
                    .workspace
                    .read(cx)
                    .visible_text()
                    .iter()
                    .any(|line| line.contains(text));
                let want = name == "assert_term_has";
                if found == want {
                    Ok(Outcome::Done)
                } else if want {
                    Err("终端屏幕上没有这段文字".to_owned())
                } else {
                    Err("终端屏幕上出现了不该有的文字".to_owned())
                }
            }
            _ => Err(format!("未实现的动作 {name}")),
        })
    }

    fn fill_dialog_field(
        this: &mut MainWindow,
        index: &str,
        text: &str,
        window: &mut Window,
        cx: &mut Context<MainWindow>,
    ) -> Result<Outcome, String> {
        let index: usize = index.parse().map_err(|_| "第几个框要是数字".to_owned())?;
        let Some(Modal::AppLock(dialog)) = &this.modal else {
            return Err("现在没有锁屏密码对话框".to_owned());
        };
        let field = dialog
            .fields
            .get(index)
            .cloned()
            .ok_or_else(|| "没有这个输入框".to_owned())?;
        field.update(cx, |state, cx| state.set_value(text.to_owned(), window, cx));
        Ok(Outcome::Done)
    }

    /// 动作名后面的整段参数（文字里可以带冒号）。
    fn rest_after<'a>(spec: &'a str, prefix: &str) -> &'a str {
        spec.strip_prefix(prefix).unwrap_or("")
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
