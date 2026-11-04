//! 查看 / 复制已保存的密码前，先验证锁屏密码（V2-U35）。
//!
//! 只在**启用了锁屏**时生效；没设锁屏密码的用户行为不变。第一次查看 / 复制时弹一个小对话框
//! （不是整窗锁屏）输锁屏密码，通过后 5 分钟内不再问；整窗锁屏 / 解锁都会让这 5 分钟作废。
//! 输错的计数和锁屏页**共用同一份**（`crate::app_lock::{record_attempt, remaining_wait}`），
//! 不能借这个对话框绕过退避。Windows Hello 的“忘记密码”入口不放在这里，只在锁屏页。
//!
//! 连接时自动使用已保存的密码登录不经过这里——那不是给人看的。
//!
//! 威胁模型：锁屏防的是旁人看屏幕 / 顺手操作，不防本机上的恶意程序（它能直接改库、读进程内存绕过）。
//!
//! 做法：每个会拿到明文的入口，在**真正去保险库取明文之前**问一句 [`MainWindow::reveal_allowed`]。
//! 不用问就继续；要问就弹对话框、记下“验证通过后接着做什么”（[`RevealAction`]），通过后用同一个
//! 入口方法重做一遍——这一次窗口期是新的，直接放行。这个对话框不是 `Modal`：主机属性对话框本身就是
//! `Modal`，同一时间只有一个，要压在它上面就得自己是单独的一层。

use std::time::{Duration, Instant};

use ::gpui::{AnyElement, Entity, Window};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::*;
use gpui_kit::*;
use zssh_core::app_lock::REVEAL_WINDOW;

use super::MainWindow;
use super::modal::{ModalKey, modal_key};
use super::widgets::{ButtonLook, error_note, field, modal_card, modal_scrim, note, text_button};
use crate::app_lock::{self as lock_ui, Job, JobOutcome};
use crate::app_theme::Tokens;
use crate::typography::{Role, Typo as _};

/// 验证通过后要接着做的事（也就是哪个入口在等）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevealAction {
    /// 主机属性对话框里密码框的小眼睛。
    HostDialogEye,
    /// 侧栏属性面板里密码行的小眼睛。
    InfoEye(i64),
    /// 侧栏属性面板里密码行的右键「复制」。
    InfoCopyPassword(i64),
    /// 主机右键「复制密码」。
    CopyHostPassword(i64),
    /// 主机右键「复制连接信息」（连接信息里带着密码时）。
    CopyHostInfo(i64),
}

impl RevealAction {
    /// 对话框里写“要做什么”：查看，还是复制。
    pub fn purpose(self) -> &'static str {
        match self {
            Self::HostDialogEye | Self::InfoEye(_) => "查看已保存的密码",
            Self::InfoCopyPassword(_) | Self::CopyHostPassword(_) => "复制已保存的密码",
            Self::CopyHostInfo(_) => "复制连接信息（含已保存的密码）",
        }
    }
}

pub const ASK_TITLE: &str = "验证锁屏密码";

/// 对话框的说明：要做什么、以及验证一次管多久。
pub fn ask_description(action: RevealAction) -> String {
    format!(
        "{}之前，先输入锁屏密码。验证通过后 {} 分钟内不再询问。",
        action.purpose(),
        REVEAL_WINDOW.as_secs() / 60
    )
}

/// 点“验证”（或回车）时、交给后台校验之前的检查：还在退避期就直接给提示（密码对也不放行），
/// 没输入也给提示。两种都不记入输错次数。
pub fn submit_precheck(waiting: Option<Duration>, password_is_empty: bool) -> Result<(), String> {
    if let Some(left) = waiting {
        return Err(lock_ui::wait_text(left));
    }
    if password_is_empty {
        return Err("请输入锁屏密码".to_owned());
    }
    Ok(())
}

/// 密码错时对话框里的提示（后面接上退避的等待文字）。
pub fn wrong_message(wait: Duration) -> String {
    if wait.is_zero() {
        "密码错误".to_owned()
    } else {
        format!("密码错误。{}", lock_ui::wait_text(wait))
    }
}

/// 弹出来的验证对话框的状态。
pub struct RevealAsk {
    pub action: RevealAction,
    pub input: Entity<InputState>,
    pub error: Option<String>,
    /// 后台正在核对密码：不能再点、不能关。
    pub busy: bool,
}

impl MainWindow {
    /// 现在能不能去取明文。要先验证就弹对话框并返回 `false`（验证通过后会自动接着做 `action`）；
    /// 不用验证返回 `true`。
    pub(super) fn reveal_allowed(
        &mut self,
        action: RevealAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // 锁着时界面根本不画，走到这里只可能是调试驱动：没法问，一律不放行。
        if lock_ui::is_locked() {
            return false;
        }
        // 读最新的设置：刚在设置里启用 / 关掉锁屏，不用等两秒一次的重读。
        self.reload_lock_config();
        if !lock_ui::reveal_needs_verification(self.lock_enabled(), Instant::now()) {
            return true;
        }
        self.open_reveal_ask(action, window, cx);
        false
    }

    fn open_reveal_ask(
        &mut self,
        action: RevealAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| InputState::new(window, cx).masked(true));
        let handle = input.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        self.reveal_ask = Some(RevealAsk {
            action,
            input,
            error: None,
            busy: false,
        });
        cx.notify();
    }

    /// 取消 / Esc。核对密码的后台任务在跑时不关。
    pub(super) fn reveal_ask_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.reveal_ask.as_ref().is_none_or(|ask| ask.busy) {
            return;
        }
        self.reveal_ask = None;
        self.restore_focus_after_reveal(window, cx);
        cx.notify();
    }

    /// 锁屏时把对话框收掉（里面刚敲的密码、等着做的动作一并作废）。
    pub(super) fn drop_reveal_ask(&mut self) {
        self.reveal_ask = None;
    }

    /// 键盘焦点回到原处：主机属性对话框开着就回它的密码框，否则回当前页。
    fn restore_focus_after_reveal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.modal, Some(super::modal::Modal::HostProps(_)))
            && let Some(form) = &self.host_form
        {
            let handle = form.password.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        } else {
            self.focus_content(window, cx);
        }
    }

    /// “验证”按钮 / 回车。
    pub(super) fn reveal_ask_submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ask) = &mut self.reveal_ask else {
            return;
        };
        if ask.busy {
            return;
        }
        let password = ask.input.read(cx).value().to_string();
        if let Err(message) =
            submit_precheck(lock_ui::remaining_wait(Instant::now()), password.is_empty())
        {
            ask.error = Some(message);
            cx.notify();
            return;
        }
        ask.busy = true;
        ask.error = None;
        cx.notify();
        lock_ui::spawn_job(
            window,
            cx,
            Job::Verify { password },
            |this, outcome, window, cx| this.reveal_ask_done(outcome, window, cx),
        );
    }

    fn reveal_ask_done(
        &mut self,
        outcome: JobOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 校验期间锁屏了：对话框已经收掉，这次结果作废（更不能把窗口期记成已验证）。
        let Some(ask) = &mut self.reveal_ask else {
            return;
        };
        ask.busy = false;
        match outcome {
            JobOutcome::Done => {
                let now = Instant::now();
                lock_ui::record_attempt(true, now);
                lock_ui::reveal_mark_verified(now);
                let action = ask.action;
                self.reveal_ask = None;
                self.restore_focus_after_reveal(window, cx);
                self.perform_reveal(action, window, cx);
            }
            JobOutcome::WrongPassword => {
                let wait = lock_ui::record_attempt(false, Instant::now());
                ask.error = Some(wrong_message(wait));
                let input = ask.input.clone();
                input.update(cx, |state, cx| state.set_value("", window, cx));
                let handle = input.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
            JobOutcome::Failed(reason) => ask.error = Some(reason),
        }
        cx.notify();
    }

    /// 验证通过后接着做：和用户直接点那个入口调的是同一个方法，窗口期这时是新的，不会再问。
    fn perform_reveal(
        &mut self,
        action: RevealAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            RevealAction::HostDialogEye => self.host_toggle_password(window, cx),
            RevealAction::InfoEye(host_id) => self.toggle_info_password(host_id, window, cx),
            RevealAction::InfoCopyPassword(host_id) => self.copy_info_password(host_id, window, cx),
            RevealAction::CopyHostPassword(host_id) => self.copy_host_password(host_id, window, cx),
            RevealAction::CopyHostInfo(host_id) => self.copy_host_info(host_id, window, cx),
        }
    }

    /// 对话框开着时的一次按键：Esc 取消、Enter 验证，其余交给输入框。返回是否拦下了。
    pub(super) fn reveal_ask_key(
        &mut self,
        key: &str,
        plain: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.reveal_ask.is_none() {
            return false;
        }
        match modal_key(key, plain, false, false) {
            ModalKey::Cancel => {
                self.reveal_ask_cancel(window, cx);
                true
            }
            ModalKey::Confirm => {
                self.reveal_ask_submit(window, cx);
                true
            }
            ModalKey::Pass => false,
        }
    }

    // ---- 画 ----

    /// 盖在整个窗口（含主机属性对话框）上面的验证小对话框；没有就是 `None`。
    pub(super) fn render_reveal_ask(
        &self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let ask = self.reveal_ask.as_ref()?;
        let entity = cx.entity();
        let cancel = entity.clone();
        let ok = entity;
        // 点遮罩不算取消：刚敲的密码不该被一次误点丢掉（和锁屏密码对话框一致）。
        let scrim = modal_scrim("reveal-scrim", false, |_, _| {});
        let body = v_flex()
            .gap_4()
            .child(
                v_flex()
                    .gap_1p5()
                    .child(div().typo(Role::GroupTitle, tokens).child(ASK_TITLE))
                    .child(note(ask_description(ask.action), tokens)),
            )
            .child(field("锁屏密码", None, Input::new(&ask.input), tokens))
            .children(
                ask.error
                    .clone()
                    .map(|text| error_note(text, tokens.danger)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(text_button(
                        "reveal-cancel",
                        "取消",
                        ButtonLook::Outline,
                        move |_, window, cx| {
                            cancel.update(cx, |this, cx| this.reveal_ask_cancel(window, cx));
                        },
                    ))
                    .child(text_button(
                        "reveal-ok",
                        if ask.busy { "验证中…" } else { "验证" },
                        ButtonLook::Primary,
                        move |_, window, cx| {
                            ok.update(cx, |this, cx| this.reveal_ask_submit(window, cx));
                        },
                    )),
            );
        Some(
            scrim
                .child(modal_card("reveal-card", 400.0, false, tokens).child(body))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{ASK_TITLE, RevealAction, ask_description, submit_precheck, wrong_message};
    use std::time::Duration;

    #[test]
    fn the_description_says_what_is_being_unlocked_and_for_how_long() {
        let view = ask_description(RevealAction::HostDialogEye);
        assert!(view.contains("查看已保存的密码"), "{view}");
        assert!(view.contains("5 分钟"), "{view}");
        let copy = ask_description(RevealAction::CopyHostPassword(3));
        assert!(copy.contains("复制已保存的密码"), "{copy}");
        let info = ask_description(RevealAction::CopyHostInfo(3));
        assert!(info.contains("复制连接信息"), "{info}");
        assert!(!ASK_TITLE.is_empty());
    }

    #[test]
    fn every_entry_point_reads_as_view_or_copy() {
        for (action, expect) in [
            (RevealAction::HostDialogEye, "查看"),
            (RevealAction::InfoEye(1), "查看"),
            (RevealAction::InfoCopyPassword(1), "复制"),
            (RevealAction::CopyHostPassword(1), "复制"),
            (RevealAction::CopyHostInfo(1), "复制"),
        ] {
            assert!(action.purpose().starts_with(expect), "{action:?}");
        }
    }

    #[test]
    fn a_wait_blocks_the_attempt_before_the_password_is_even_looked_at() {
        let blocked = submit_precheck(Some(Duration::from_secs(5)), false);
        assert_eq!(blocked, Err("输错次数过多，请 5 秒后再试".to_owned()));
        // 退避期间输入为空也是先报等待：不给“试探”留口子。
        assert!(
            submit_precheck(Some(Duration::from_millis(900)), true)
                .unwrap_err()
                .contains("1 秒后再试")
        );
    }

    #[test]
    fn an_empty_password_is_refused_without_counting_as_a_wrong_try() {
        assert_eq!(
            submit_precheck(None, true),
            Err("请输入锁屏密码".to_owned())
        );
        assert_eq!(submit_precheck(None, false), Ok(()));
    }

    #[test]
    fn the_wrong_password_message_carries_the_shared_wait() {
        assert_eq!(wrong_message(Duration::ZERO), "密码错误");
        assert_eq!(
            wrong_message(Duration::from_secs(10)),
            "密码错误。输错次数过多，请 10 秒后再试"
        );
    }
}
