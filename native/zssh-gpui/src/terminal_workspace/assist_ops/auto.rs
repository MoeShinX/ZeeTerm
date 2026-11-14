//! 连接后自动启用 Shell 集成：窗格这一层的状态和接线。判定规则（什么算空闲提示符、什么时候发、什么时候放弃）
//! 在核心库的 `terminal_assist::auto_integration`，这里只负责收集事实、记"这次连接试过没有"、起登录 shell 探测。
//!
//! 每次连接最多判一次：[`Stage`] 只会 `Waiting` → `Done`，不会倒回去；断线 / 重连由 `on_connection_reset`
//! 回到 `Inactive`，下次连上再由 `begin_auto` 重新开始（重连算新的一次）。
//!
//! 发送本身不在这里：`auto_poll` 只返回"该发哪种脚本"，窗格调用和手动启用同一条 `start_integration`
//! （同样的脚本、同样以空格开头、同样的握手和失败清理）。

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Instant;

use zssh_core::ai::shell_kind::LoginShell;
use zssh_core::terminal_assist::WriteSource;
use zssh_core::terminal_assist::auto_integration::{
    self as rules, ATTEMPT_WINDOW, Decision, Inputs, Outcome, ProbeState,
};
use zssh_core::terminal_assist::integration::ShellKind;
use zssh_session::{CoreFilter, SessionPump};

use super::{IntegrationState, PaneAssist, PumpScreen, WriteOrigin};

/// 窗格每次轮询时告诉自动启用的、不属于输入跟踪的外部事实。
pub(crate) struct AutoEnv<'a> {
    /// 这个窗格的终端在 AI 终端登记表里的编号（探测登录 shell 用）。
    pub(crate) terminal_id: Option<&'a str>,
    pub(crate) locked: bool,
    /// AI 正在这个终端里执行。
    pub(crate) ai_busy: bool,
}

/// 登录 shell 探测：还没起 / 在路上 / 有结论。
enum Probe {
    NotStarted,
    Running(Receiver<Option<LoginShell>>),
    Resolved(ProbeState),
}

/// 等待发送时机的这一次连接。
struct Waiting {
    /// 连接建立的时刻（15 秒窗口从这里算）。
    since: Instant,
    /// 终端上一次有动静（有新输出 / 光标动了）的时刻。
    last_activity: Instant,
    /// 上一次看到的屏幕指纹：（内容修订号, 光标行, 光标列）。变了就是有动静。
    fingerprint: Option<(u64, usize, usize)>,
    probe: Probe,
    /// 连接后用户是否往这个窗格输入过任何东西。
    user_typed: bool,
}

#[derive(Default)]
enum Stage {
    /// 不适用：没连上、不是 SSH、断线之后。
    #[default]
    Inactive,
    Waiting(Waiting),
    /// 这次连接已经判完，不会再试。`Some` 是要在弹窗里说明的结果。
    Done(Option<Outcome>),
}

#[derive(Default)]
pub(super) struct AutoState {
    /// 设置里的 `terminal_assist.auto_integration`。
    setting: bool,
    stage: Stage,
    /// 测试里代替真实的 exec 探测：直接给出结果。
    #[cfg(test)]
    probe_hook: Option<Box<dyn Fn() -> Option<LoginShell>>>,
    /// 测试里数一数起过几次探测（"不探测"的证据）。
    #[cfg(test)]
    probes_started: u32,
}

impl PaneAssist {
    /// 设置里的开关。每帧由工作区告诉各窗格；关着时只让窗口走完，不探测、不发送。
    pub(crate) fn set_auto_setting(&mut self, on: bool) {
        self.auto.setting = on;
    }

    /// 连上了：只有 SSH 会话才开始等时机。Telnet / 串口 / 离线面板一律不自动发任何东西，也不探测。
    pub(crate) fn begin_auto(&mut self, is_ssh: bool, now: Instant) {
        self.auto.stage = if is_ssh {
            Stage::Waiting(Waiting {
                since: now,
                last_activity: now,
                fingerprint: None,
                probe: Probe::NotStarted,
                user_typed: false,
            })
        } else {
            Stage::Inactive
        };
    }

    /// 断线 / 重连：这次连接的自动启用到此为止。
    pub(super) fn end_auto(&mut self) {
        self.auto.stage = Stage::Inactive;
    }

    /// 窗格被写入了：除了集成脚本自己，任何写入（按键、输入法、粘贴、撰写栏、快捷命令、广播……）都算
    /// "用户已经开始用这个终端"。
    pub(super) fn note_user_write(&mut self, origin: WriteOrigin) {
        if origin == WriteOrigin::Foreign(WriteSource::IntegrationScript) {
            return;
        }
        if let Stage::Waiting(waiting) = &mut self.auto.stage {
            waiting.user_typed = true;
        }
    }

    /// 本次连接自动启用的结果；没有要说明的是 `None`。
    pub(crate) fn auto_outcome(&self) -> Option<Outcome> {
        match self.auto.stage {
            Stage::Done(outcome) => outcome,
            _ => None,
        }
    }

    /// 还在等时机时被手动启用（或自动发送本身）接管：不再等，也没有要说明的。已经判完的不动。
    pub(super) fn auto_stop_waiting(&mut self) {
        if matches!(self.auto.stage, Stage::Waiting(_)) {
            self.auto.stage = Stage::Done(None);
        }
    }

    /// 用户手动停用集成：本次连接不再自动启用，"已自动启用"的说明也撤掉（别的跳过原因照旧保留）。
    pub(super) fn auto_forget(&mut self) {
        let clear = match self.auto.stage {
            Stage::Waiting(_) => true,
            Stage::Done(Some(Outcome::Enabled(_))) => true,
            Stage::Inactive | Stage::Done(_) => false,
        };
        if clear {
            self.auto.stage = Stage::Done(None);
        }
    }

    /// 自动发送没成功（脚本没能排进发送队列）：记成跳过。
    pub(crate) fn auto_finish(&mut self, outcome: Outcome) {
        self.auto.stage = Stage::Done(Some(outcome));
    }

    /// 窗格每次轮询（每帧）调用：收集事实、判定。返回 `Some` 时窗格应当调用 `start_integration` 发这种脚本；
    /// 返回前已经把本次连接记成"发过"，所以不论发送成败都不会再来第二次。
    pub(crate) fn auto_poll(
        &mut self,
        pump: &SessionPump<CoreFilter>,
        now: Instant,
        env: &AutoEnv<'_>,
    ) -> Option<ShellKind> {
        let Stage::Waiting(waiting) = &mut self.auto.stage else {
            return None;
        };
        let elapsed = now.saturating_duration_since(waiting.since);
        // 手动启用抢先了（弹窗确认过的）：自动的不再插手。
        if self.integration != IntegrationState::Off {
            self.auto.stage = Stage::Done(None);
            return None;
        }
        // 屏幕指纹：有新输出、光标动了、窗口大小变了，都说明终端还没安静下来。
        let screen = pump.screen_probe();
        let fingerprint = (
            screen.content_revision,
            screen.cursor_row,
            screen.cursor_col,
        );
        if waiting.fingerprint != Some(fingerprint) {
            waiting.fingerprint = Some(fingerprint);
            waiting.last_activity = now;
        }
        // 设置关着、命令建议总开关关着：什么也不做，只让窗口走完（窗口内打开设置仍然有效，之后不再试）。
        if !self.auto.setting || !self.enabled {
            if elapsed >= ATTEMPT_WINDOW {
                self.auto.stage = Stage::Done(None);
            }
            return None;
        }
        // 用户输入过就不用再探测了：结论已经定了。
        if !waiting.user_typed {
            if matches!(waiting.probe, Probe::NotStarted) {
                waiting.probe = start_probe(
                    env.terminal_id,
                    #[cfg(test)]
                    &mut self.auto.probes_started,
                    #[cfg(test)]
                    self.auto.probe_hook.as_deref(),
                );
            }
            if let Probe::Running(rx) = &waiting.probe {
                match rx.try_recv() {
                    Ok(Some(shell)) => waiting.probe = Probe::Resolved(ProbeState::Shell(shell)),
                    Ok(None) | Err(TryRecvError::Disconnected) => {
                        waiting.probe = Probe::Resolved(ProbeState::Failed);
                    }
                    Err(TryRecvError::Empty) => {}
                }
            }
        }
        let probe = match &waiting.probe {
            Probe::Resolved(state) => *state,
            Probe::NotStarted | Probe::Running(_) => ProbeState::Pending,
        };
        let inputs = Inputs {
            elapsed,
            quiet_for: now.saturating_duration_since(waiting.last_activity),
            probe,
            user_typed: waiting.user_typed,
            ai_busy: env.ai_busy,
            locked: env.locked,
            has_marks: self.tracker.saw_prompt_marks(),
        };
        match rules::decide(&inputs, || rules::screen_at_idle_prompt(&PumpScreen(pump))) {
            Decision::Wait => None,
            Decision::Skip(reason) => {
                self.auto.stage = Stage::Done(Some(Outcome::Skipped(reason)));
                None
            }
            Decision::Send(kind) => {
                // 先记成"发过"：不论后面发送成败，这次连接都不再试。
                self.auto.stage = Stage::Done(Some(Outcome::Enabled(kind)));
                Some(kind)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn set_probe_hook_for_test(
        &mut self,
        hook: impl Fn() -> Option<LoginShell> + 'static,
    ) {
        self.auto.probe_hook = Some(Box::new(hook));
    }

    #[cfg(test)]
    pub(crate) fn probes_started_for_test(&self) -> u32 {
        self.auto.probes_started
    }

    #[cfg(test)]
    pub(crate) fn auto_waiting_for_test(&self) -> bool {
        matches!(self.auto.stage, Stage::Waiting(_))
    }
}

/// 起一次登录 shell 探测。测试里给了替身就直接用它的结果；否则走独立的 exec 通道（后台线程，界面线程不等）。
fn start_probe(
    terminal_id: Option<&str>,
    #[cfg(test)] started: &mut u32,
    #[cfg(test)] hook: Option<&dyn Fn() -> Option<LoginShell>>,
) -> Probe {
    #[cfg(test)]
    {
        *started += 1;
        if let Some(hook) = hook {
            return Probe::Resolved(match hook() {
                Some(shell) => ProbeState::Shell(shell),
                None => ProbeState::Failed,
            });
        }
    }
    let (tx, rx) = mpsc::channel();
    crate::agent_host::spawn_login_shell_probe(terminal_id.map(str::to_owned), tx);
    Probe::Running(rx)
}
