//! 终端底部细栏的「命令建议」入口：点开是一个状态说明弹窗，里面有"本次启用 Bash / Zsh 集成"和
//! "复制脚本手动启用"两组动作。
//!
//! 这一版（P3）只做地基：不显示候选。弹窗告诉用户：当前会话有没有输入行标记、原生 Tab 仍然可用，
//! 并让用户为当前会话临时启用 shell 集成。启用前必须由用户确认"当前窗格正停在空闲提示符"，确认后才发送脚本
//! （见 `zssh_core::terminal_assist::integration`）：无标记时没法自动证明前台是空闲 shell，所以不猜、不在
//! 未知的 REPL 或忙碌会话里自动试探。

use ::gpui::{AnyElement, Window};
use gpui_kit::component::button::Button;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::terminal_assist::Phase;
use zssh_core::terminal_assist::integration::{CLEANUP_COMMAND, HANDSHAKE_TIMEOUT, ShellKind};

use super::MainWindow;
use super::modal::{ModalKey, modal_key};
use super::widgets::{ButtonLook, error_note, modal_card, modal_scrim, note, text_button};
use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::terminal_workspace::{AssistView, IntegrationError, IntegrationState, StopOutcome};
use crate::typography::{Role, Typo as _};

/// 弹窗现在在哪一步。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssistStep {
    /// 状态说明 + 动作。
    Status,
    /// 让用户确认"当前窗格停在空闲提示符"，确认后才发送脚本。
    Confirm(ShellKind),
}

/// 弹出来的命令建议弹窗。
#[derive(Debug)]
pub struct AssistDialog {
    pub step: AssistStep,
    /// 上一个动作的结果（启用被拒绝的原因、需要手动清理的提示……）。
    pub notice: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AssistMsg {
    Title,
    NoSession,
    Disabled,
    MarksPresent,
    NoMarks,
    InputPrefix,
    PhaseUnknown,
    PhaseInput,
    PhaseAwaitingEcho,
    PhaseRunning,
    PhaseDesynced,
    PhaseSuspended,
    ShellBash,
    ShellZsh,
    Handshaking,
    HandshakeWait,
    ActiveSuffix,
    FailedPrefix,
    FailedMid,
    FailedTail,
    NativeKeys,
    EnableBash,
    EnableZsh,
    Disable,
    CopyHeading,
    CopyBash,
    CopyZsh,
    Close,
    ConfirmTitle,
    ConfirmBody,
    ConfirmHistory,
    ConfirmCancel,
    ConfirmGoBash,
    ConfirmGoZsh,
    ErrNoSession,
    ErrDisabled,
    ErrLocked,
    ErrHandshaking,
    ErrRunning,
    ErrLineNotEmpty,
    ErrWrite,
    StoppedSent,
    StoppedManual,
    Copied,
}

impl Msg for AssistMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::Title => "命令建议",
            Self::NoSession => "当前窗格没有已连接的会话，命令建议不可用。",
            Self::Disabled => "命令建议已关闭，不做输入跟踪。",
            Self::MarksPresent => "当前会话提供了输入行标记，可以确认输入位置。",
            Self::NoMarks => "当前会话未提供输入行标记，原生 Tab 仍可用。",
            Self::InputPrefix => "输入状态：",
            Self::PhaseUnknown => "尚不确定",
            Self::PhaseInput => "在输入行上",
            Self::PhaseAwaitingEcho => "等待回显",
            Self::PhaseRunning => "命令执行中",
            Self::PhaseDesynced => "与屏幕失同步，等待下一个提示符",
            Self::PhaseSuspended => "已暂停（锁屏、广播、全屏程序等）",
            Self::ShellBash => "Bash",
            Self::ShellZsh => "Zsh",
            Self::Handshaking => "正在启用 ",
            Self::HandshakeWait => " 集成，等待就绪…",
            Self::ActiveSuffix => " 集成已在本次会话启用。",
            Self::FailedPrefix => "",
            Self::FailedMid => " 集成启用失败：",
            Self::FailedTail => {
                " 秒内没有收到输入行标记，已停止本地建议。确认窗格停在空闲提示符后可以重试；也可以在该 shell 里手动运行下面的清理命令："
            }
            Self::NativeKeys => {
                "原生 Tab 补全、方向键和 Enter 一律原样交给 shell，建议功能不会改变它们。"
            }
            Self::EnableBash => "本次启用 Bash 集成",
            Self::EnableZsh => "本次启用 Zsh 集成",
            Self::Disable => "停用本次集成",
            Self::CopyHeading => "复制脚本手动启用",
            Self::CopyBash => "复制 Bash 脚本",
            Self::CopyZsh => "复制 Zsh 脚本",
            Self::Close => "关闭",
            Self::ConfirmTitle => "确认当前窗格停在空闲提示符",
            Self::ConfirmBody => {
                "请确认当前窗格正停在 shell 的空闲提示符上：没有正在运行的命令，没有打开的编辑器或交互程序，输入行是空的。确认后会向该窗格发送一段只在本次会话生效的脚本，它不修改任何 rc 文件，保留原有的 prompt 和 hook，断开连接后自动失效。"
            }
            Self::ConfirmHistory => {
                "脚本以空格开头发送：是否进入 shell 历史取决于它的设置（Bash 的 HISTCONTROL 含 ignorespace，Zsh 的 HIST_IGNORE_SPACE）。"
            }
            Self::ConfirmCancel => "取消",
            Self::ConfirmGoBash => "确认，启用 Bash 集成",
            Self::ConfirmGoZsh => "确认，启用 Zsh 集成",
            Self::ErrNoSession => "当前窗格没有已连接的会话。",
            Self::ErrDisabled => "命令建议已关闭。",
            Self::ErrLocked => "应用已锁定。",
            Self::ErrHandshaking => "上一次启用还在等待就绪。",
            Self::ErrRunning => "窗格里有命令正在执行，没有发送脚本。",
            Self::ErrLineNotEmpty => "输入行里已有未执行的文字，没有发送脚本；请先清空输入行。",
            Self::ErrWrite => "脚本没能发送出去，输入被拒绝或连接已断开。",
            Self::StoppedSent => "已向窗格发送清理命令。",
            Self::StoppedManual => {
                "没有可靠的空闲提示符，没有向远端发送任何东西。请在该 shell 里手动运行："
            }
            Self::Copied => "脚本已复制。在目标 shell 的空闲提示符下粘贴运行即可。",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::Title => "Command Suggestions",
            Self::NoSession => {
                "This pane has no live session; command suggestions are unavailable."
            }
            Self::Disabled => "Command suggestions are turned off; input is not tracked.",
            Self::MarksPresent => {
                "This session provides input-line marks, so the input position can be confirmed."
            }
            Self::NoMarks => "This session provides no input-line marks. Native Tab still works.",
            Self::InputPrefix => "Input state: ",
            Self::PhaseUnknown => "not sure yet",
            Self::PhaseInput => "at the input line",
            Self::PhaseAwaitingEcho => "waiting for the echo",
            Self::PhaseRunning => "a command is running",
            Self::PhaseDesynced => "out of sync with the screen, waiting for the next prompt",
            Self::PhaseSuspended => "paused (lock, broadcast, full-screen program, ...)",
            Self::ShellBash => "Bash",
            Self::ShellZsh => "Zsh",
            Self::Handshaking => "Enabling the ",
            Self::HandshakeWait => " integration, waiting for it to be ready...",
            Self::ActiveSuffix => " integration is enabled for this session.",
            Self::FailedPrefix => "",
            Self::FailedMid => " integration failed: no input-line mark arrived within ",
            Self::FailedTail => {
                " seconds, so local suggestions are stopped. Make sure the pane is at an idle prompt and try again, or run this cleanup command in that shell by hand: "
            }
            Self::NativeKeys => {
                "Native Tab completion, arrow keys and Enter always go to the shell unchanged; suggestions never alter them."
            }
            Self::EnableBash => "Enable Bash integration for this session",
            Self::EnableZsh => "Enable Zsh integration for this session",
            Self::Disable => "Turn off the integration",
            Self::CopyHeading => "Copy a script to enable it by hand",
            Self::CopyBash => "Copy Bash script",
            Self::CopyZsh => "Copy Zsh script",
            Self::Close => "Close",
            Self::ConfirmTitle => "Confirm the pane is at an idle prompt",
            Self::ConfirmBody => {
                "Please confirm this pane is sitting at an idle shell prompt: no command is running, no editor or interactive program is open, and the input line is empty. After you confirm, a script that only lasts for this session is sent to the pane. It edits no rc file, keeps your existing prompt and hooks, and stops working when the connection ends."
            }
            Self::ConfirmHistory => {
                "The script is sent with a leading space: whether it lands in shell history depends on the shell (HISTCONTROL with ignorespace in Bash, HIST_IGNORE_SPACE in Zsh)."
            }
            Self::ConfirmCancel => "Cancel",
            Self::ConfirmGoBash => "Confirm and enable Bash integration",
            Self::ConfirmGoZsh => "Confirm and enable Zsh integration",
            Self::ErrNoSession => "This pane has no live session.",
            Self::ErrDisabled => "Command suggestions are turned off.",
            Self::ErrLocked => "The app is locked.",
            Self::ErrHandshaking => "The previous attempt is still waiting to become ready.",
            Self::ErrRunning => "A command is running in the pane; the script was not sent.",
            Self::ErrLineNotEmpty => {
                "The input line already has text that has not run; the script was not sent. Clear the line first."
            }
            Self::ErrWrite => {
                "The script could not be sent: input was rejected or the connection is gone."
            }
            Self::StoppedSent => "The cleanup command was sent to the pane.",
            Self::StoppedManual => {
                "There is no reliable idle prompt, so nothing was sent to the remote end. Run this in that shell by hand: "
            }
            Self::Copied => {
                "Script copied. Paste it at an idle prompt of the target shell to run it."
            }
        }
    }
}

fn shell_name(kind: ShellKind) -> &'static str {
    match kind {
        ShellKind::Bash => t(AssistMsg::ShellBash),
        ShellKind::Zsh => t(AssistMsg::ShellZsh),
    }
}

fn phase_text(phase: Phase) -> &'static str {
    t(match phase {
        Phase::Unknown => AssistMsg::PhaseUnknown,
        Phase::Input => AssistMsg::PhaseInput,
        Phase::AwaitingEcho => AssistMsg::PhaseAwaitingEcho,
        Phase::Running => AssistMsg::PhaseRunning,
        Phase::Desynced => AssistMsg::PhaseDesynced,
        Phase::Suspended => AssistMsg::PhaseSuspended,
    })
}

/// 弹窗里的状态说明（纯文字，按语言出词）：先说有没有输入行标记，再说输入状态和临时集成的进度，
/// 最后一句固定说明原生按键不受影响。
pub(super) fn status_lines(view: Option<AssistView>) -> Vec<String> {
    let mut lines = Vec::new();
    match view {
        None => lines.push(t(AssistMsg::NoSession).to_owned()),
        Some(view) if !view.live => lines.push(t(AssistMsg::NoSession).to_owned()),
        Some(view) if !view.enabled => lines.push(t(AssistMsg::Disabled).to_owned()),
        Some(view) => {
            lines.push(
                t(if view.marks {
                    AssistMsg::MarksPresent
                } else {
                    AssistMsg::NoMarks
                })
                .to_owned(),
            );
            lines.push(format!(
                "{}{}",
                t(AssistMsg::InputPrefix),
                phase_text(view.phase)
            ));
            match view.integration {
                IntegrationState::Off => {}
                IntegrationState::Handshake { kind, .. } => lines.push(format!(
                    "{}{}{}",
                    t(AssistMsg::Handshaking),
                    shell_name(kind),
                    t(AssistMsg::HandshakeWait)
                )),
                IntegrationState::Active { kind } => lines.push(format!(
                    "{}{}",
                    shell_name(kind),
                    t(AssistMsg::ActiveSuffix)
                )),
                IntegrationState::Failed { kind } => lines.push(format!(
                    "{}{}{}{}{}{}",
                    t(AssistMsg::FailedPrefix),
                    shell_name(kind),
                    t(AssistMsg::FailedMid),
                    HANDSHAKE_TIMEOUT.as_secs(),
                    t(AssistMsg::FailedTail),
                    CLEANUP_COMMAND
                )),
            }
        }
    }
    lines.push(t(AssistMsg::NativeKeys).to_owned());
    lines
}

/// 启用被拒绝时给用户看的原因。
pub(super) fn error_text(error: IntegrationError) -> &'static str {
    t(match error {
        IntegrationError::NoSession => AssistMsg::ErrNoSession,
        IntegrationError::Disabled => AssistMsg::ErrDisabled,
        IntegrationError::Locked => AssistMsg::ErrLocked,
        IntegrationError::Handshaking => AssistMsg::ErrHandshaking,
        IntegrationError::Running => AssistMsg::ErrRunning,
        IntegrationError::LineNotEmpty => AssistMsg::ErrLineNotEmpty,
        IntegrationError::WriteFailed(_) => AssistMsg::ErrWrite,
    })
}

impl MainWindow {
    /// 状态栏按钮要不要高亮：弹窗开着，或者本次会话的集成已经启用。
    pub(super) fn assist_button_lit(&self, cx: &App) -> bool {
        self.assist_dialog.is_some()
            || self
                .workspace
                .read(cx)
                .assist_view()
                .is_some_and(|view| matches!(view.integration, IntegrationState::Active { .. }))
    }

    pub(super) fn toggle_assist_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.assist_dialog.is_some() {
            self.close_assist_dialog(window, cx);
        } else {
            self.assist_dialog = Some(AssistDialog {
                step: AssistStep::Status,
                notice: None,
            });
            cx.notify();
        }
    }

    pub(super) fn close_assist_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.assist_dialog.take().is_some() {
            self.focus_content(window, cx);
            cx.notify();
        }
    }

    /// 锁屏时把弹窗丢掉。
    pub(super) fn drop_assist_dialog(&mut self) {
        self.assist_dialog = None;
    }

    pub(super) fn assist_ask(&mut self, kind: ShellKind, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.assist_dialog {
            dialog.step = AssistStep::Confirm(kind);
            dialog.notice = None;
            cx.notify();
        }
    }

    pub(super) fn assist_back(&mut self, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.assist_dialog {
            dialog.step = AssistStep::Status;
            cx.notify();
        }
    }

    /// 用户确认了空闲提示符：发送脚本。结果（成功时是"正在握手"，失败时是原因）回到状态页。
    pub(super) fn assist_start(&mut self, kind: ShellKind, cx: &mut Context<Self>) {
        let result = self
            .workspace
            .update(cx, |workspace, cx| workspace.start_integration(kind, cx));
        if let Some(dialog) = &mut self.assist_dialog {
            dialog.step = AssistStep::Status;
            dialog.notice = result.err().map(|error| error_text(error).to_owned());
        }
        cx.notify();
    }

    pub(super) fn assist_stop(&mut self, cx: &mut Context<Self>) {
        let outcome = self
            .workspace
            .update(cx, |workspace, cx| workspace.stop_integration(cx));
        if let Some(dialog) = &mut self.assist_dialog {
            dialog.notice = Some(match outcome {
                StopOutcome::Sent => t(AssistMsg::StoppedSent).to_owned(),
                StopOutcome::Manual => format!("{}{CLEANUP_COMMAND}", t(AssistMsg::StoppedManual)),
            });
        }
        cx.notify();
    }

    fn assist_copy(&mut self, kind: ShellKind, cx: &mut Context<Self>) {
        let script = zssh_core::terminal_assist::integration::script(kind);
        let result = crate::chat_copy::write_plain(&script, cx);
        if let Some(dialog) = &mut self.assist_dialog {
            dialog.notice = Some(match result {
                Ok(()) => t(AssistMsg::Copied).to_owned(),
                Err(reason) => reason,
            });
        }
        cx.notify();
    }

    /// 弹窗开着时的一次按键：Esc 取消（确认步骤回到状态页，状态页关闭），确认步骤里 Enter 确认。
    /// 弹窗没有输入框，所有按键都不该漏到下面的终端里，所以一律拦下。返回是否拦下了。
    pub(super) fn assist_key(
        &mut self,
        key: &str,
        plain: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(step) = self.assist_dialog.as_ref().map(|dialog| dialog.step) else {
            return false;
        };
        match (modal_key(key, plain, false, false), step) {
            (ModalKey::Cancel, AssistStep::Confirm(_)) => self.assist_back(cx),
            (ModalKey::Cancel, AssistStep::Status) => self.close_assist_dialog(window, cx),
            (ModalKey::Confirm, AssistStep::Confirm(kind)) => self.assist_start(kind, cx),
            _ => {}
        }
        true
    }

    /// 盖在窗口上的弹窗；没开就是 `None`。
    pub(super) fn render_assist_dialog(
        &self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let dialog = self.assist_dialog.as_ref()?;
        let entity = cx.entity();
        let scrim_entity = entity.clone();
        let scrim = modal_scrim("assist-scrim", false, move |window, cx| {
            scrim_entity.update(cx, |this, cx| this.close_assist_dialog(window, cx));
        });
        let view = self.workspace.read(cx).assist_view();
        let body = match dialog.step {
            AssistStep::Status => self.render_assist_status(dialog, view, tokens, &entity),
            AssistStep::Confirm(kind) => self.render_assist_confirm(kind, tokens, &entity),
        };
        Some(
            scrim
                .child(modal_card("assist-card", 480.0, false, tokens).child(body))
                .into_any_element(),
        )
    }

    fn render_assist_status(
        &self,
        dialog: &AssistDialog,
        view: Option<AssistView>,
        tokens: &Tokens,
        entity: &Entity<Self>,
    ) -> Div {
        let can_enable = view.is_some_and(|view| {
            view.live
                && view.enabled
                && !matches!(view.integration, IntegrationState::Handshake { .. })
        });
        let integration_on = view.is_some_and(|view| view.integration != IntegrationState::Off);
        let lines = status_lines(view);
        let (last, head) = lines
            .split_last()
            .map(|(last, head)| (last.clone(), head.to_vec()))
            .unwrap_or_default();
        let enable_row = h_flex()
            .gap_2()
            .flex_wrap()
            .child(self.assist_button(
                "assist-enable-bash",
                t(AssistMsg::EnableBash),
                ButtonLook::Primary,
                can_enable,
                entity,
                |this, cx| this.assist_ask(ShellKind::Bash, cx),
            ))
            .child(self.assist_button(
                "assist-enable-zsh",
                t(AssistMsg::EnableZsh),
                ButtonLook::Primary,
                can_enable,
                entity,
                |this, cx| this.assist_ask(ShellKind::Zsh, cx),
            ))
            .when(integration_on, |row| {
                row.child(self.assist_button(
                    "assist-disable",
                    t(AssistMsg::Disable),
                    ButtonLook::Outline,
                    true,
                    entity,
                    |this, cx| this.assist_stop(cx),
                ))
            });
        let copy_row = v_flex()
            .gap_1p5()
            .child(note(t(AssistMsg::CopyHeading), tokens))
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(self.assist_button(
                        "assist-copy-bash",
                        t(AssistMsg::CopyBash),
                        ButtonLook::Outline,
                        true,
                        entity,
                        |this, cx| this.assist_copy(ShellKind::Bash, cx),
                    ))
                    .child(self.assist_button(
                        "assist-copy-zsh",
                        t(AssistMsg::CopyZsh),
                        ButtonLook::Outline,
                        true,
                        entity,
                        |this, cx| this.assist_copy(ShellKind::Zsh, cx),
                    )),
            );
        let close = entity.clone();
        v_flex()
            .gap_3()
            .child(
                div()
                    .typo(Role::GroupTitle, tokens)
                    .child(t(AssistMsg::Title)),
            )
            .child(
                v_flex()
                    .gap_1p5()
                    .children(head.into_iter().map(|line| div().text_sm().child(line)))
                    .child(note(last, tokens)),
            )
            .children(
                dialog
                    .notice
                    .clone()
                    .map(|text| error_note(text, tokens.text_muted)),
            )
            .child(enable_row)
            .child(copy_row)
            .child(h_flex().justify_end().child(text_button(
                "assist-close",
                t(AssistMsg::Close),
                ButtonLook::Outline,
                move |_, window, cx| {
                    close.update(cx, |this, cx| this.close_assist_dialog(window, cx));
                },
            )))
    }

    fn render_assist_confirm(
        &self,
        kind: ShellKind,
        tokens: &Tokens,
        entity: &Entity<Self>,
    ) -> Div {
        let (cancel, go) = (entity.clone(), entity.clone());
        v_flex()
            .gap_3()
            .child(
                div()
                    .typo(Role::GroupTitle, tokens)
                    .child(t(AssistMsg::ConfirmTitle)),
            )
            .child(div().text_sm().child(t(AssistMsg::ConfirmBody)))
            .child(note(t(AssistMsg::ConfirmHistory), tokens))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(text_button(
                        "assist-confirm-cancel",
                        t(AssistMsg::ConfirmCancel),
                        ButtonLook::Outline,
                        move |_, _, cx| {
                            cancel.update(cx, |this, cx| this.assist_back(cx));
                        },
                    ))
                    .child(text_button(
                        "assist-confirm-go",
                        t(match kind {
                            ShellKind::Bash => AssistMsg::ConfirmGoBash,
                            ShellKind::Zsh => AssistMsg::ConfirmGoZsh,
                        }),
                        ButtonLook::Primary,
                        move |_, _, cx| {
                            go.update(cx, |this, cx| this.assist_start(kind, cx));
                        },
                    )),
            )
    }

    /// 弹窗里的一个按钮；`enabled` 为假时置灰、点了没反应。
    fn assist_button(
        &self,
        id: &'static str,
        label: &'static str,
        look: ButtonLook,
        enabled: bool,
        entity: &Entity<Self>,
        action: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> Button {
        let entity = entity.clone();
        text_button(id, label, look, move |_, _, cx| {
            entity.update(cx, |this, cx| action(this, cx));
        })
        .disabled(!enabled)
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{AssistMsg, IntegrationError, error_text, status_lines};
    use crate::i18n::Msg as _;
    use crate::terminal_workspace::{AssistView, IntegrationState};
    use std::time::Instant;
    use zssh_core::terminal_assist::Phase;
    use zssh_core::terminal_assist::integration::{CLEANUP_COMMAND, ShellKind};

    fn view(marks: bool, integration: IntegrationState) -> AssistView {
        AssistView {
            live: true,
            enabled: true,
            phase: Phase::Input,
            marks,
            integration,
            suggestions: marks,
        }
    }

    #[test]
    fn without_marks_the_status_says_native_tab_still_works() {
        let lines = status_lines(Some(view(false, IntegrationState::Off)));
        assert!(lines[0].contains("未提供输入行标记"), "{lines:?}");
        assert!(lines[0].contains("原生 Tab 仍可用"), "{lines:?}");
        assert!(lines[1].contains("输入状态"), "{lines:?}");
        assert!(
            lines.last().unwrap().contains("原样交给 shell"),
            "{lines:?}"
        );
    }

    #[test]
    fn with_marks_the_status_says_the_input_position_can_be_confirmed() {
        let lines = status_lines(Some(view(true, IntegrationState::Off)));
        assert!(lines[0].contains("提供了输入行标记"), "{lines:?}");
    }

    #[test]
    fn the_integration_progress_names_the_shell_and_a_failure_gives_the_manual_cleanup() {
        let handshake = status_lines(Some(view(
            false,
            IntegrationState::Handshake {
                kind: ShellKind::Bash,
                since: Instant::now(),
            },
        )));
        assert!(
            handshake
                .iter()
                .any(|l| l.contains("Bash") && l.contains("等待就绪"))
        );
        let active = status_lines(Some(view(
            true,
            IntegrationState::Active {
                kind: ShellKind::Zsh,
            },
        )));
        assert!(
            active
                .iter()
                .any(|l| l.contains("Zsh") && l.contains("已在本次会话启用"))
        );
        let failed = status_lines(Some(view(
            false,
            IntegrationState::Failed {
                kind: ShellKind::Bash,
            },
        )));
        let text = failed.join("\n");
        assert!(text.contains("10 秒内没有收到输入行标记"), "{text}");
        assert!(text.contains("已停止本地建议"), "{text}");
        assert!(
            text.contains(CLEANUP_COMMAND),
            "手动清理命令要写出来：{text}"
        );
    }

    #[test]
    fn no_session_or_a_disabled_switch_is_said_plainly() {
        assert!(status_lines(None)[0].contains("没有已连接的会话"));
        let mut offline = view(false, IntegrationState::Off);
        offline.live = false;
        assert!(status_lines(Some(offline))[0].contains("没有已连接的会话"));
        let mut disabled = view(false, IntegrationState::Off);
        disabled.enabled = false;
        assert!(status_lines(Some(disabled))[0].contains("已关闭"));
        // 无论哪种，原生按键不受影响这一句都在。
        assert!(
            status_lines(None)
                .last()
                .unwrap()
                .contains("原样交给 shell")
        );
    }

    #[test]
    fn every_rejection_has_its_own_explanation() {
        let all = [
            IntegrationError::NoSession,
            IntegrationError::Disabled,
            IntegrationError::Locked,
            IntegrationError::Handshaking,
            IntegrationError::Running,
            IntegrationError::LineNotEmpty,
            IntegrationError::WriteFailed(zssh_session::WriteOutcome::TransportFailed),
        ];
        let texts: std::collections::HashSet<&str> = all.iter().map(|e| error_text(*e)).collect();
        assert_eq!(texts.len(), all.len());
    }

    #[test]
    fn every_message_has_both_languages() {
        use AssistMsg::*;
        for msg in [
            Title,
            NoSession,
            Disabled,
            MarksPresent,
            NoMarks,
            InputPrefix,
            PhaseUnknown,
            PhaseInput,
            PhaseAwaitingEcho,
            PhaseRunning,
            PhaseDesynced,
            PhaseSuspended,
            ShellBash,
            ShellZsh,
            Handshaking,
            HandshakeWait,
            ActiveSuffix,
            FailedMid,
            FailedTail,
            NativeKeys,
            EnableBash,
            EnableZsh,
            Disable,
            CopyHeading,
            CopyBash,
            CopyZsh,
            Close,
            ConfirmTitle,
            ConfirmBody,
            ConfirmHistory,
            ConfirmCancel,
            ConfirmGoBash,
            ConfirmGoZsh,
            ErrNoSession,
            ErrDisabled,
            ErrLocked,
            ErrHandshaking,
            ErrRunning,
            ErrLineNotEmpty,
            ErrWrite,
            StoppedSent,
            StoppedManual,
            Copied,
        ] {
            assert!(!msg.zh().is_empty() && !msg.en().is_empty(), "{msg:?}");
            assert!(
                !msg.en()
                    .chars()
                    .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
                "英文表里不能混进中文：{msg:?}"
            );
        }
    }
}
