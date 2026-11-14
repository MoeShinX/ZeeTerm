//! 连接后自动启用 Bash / Zsh 集成的判定（纯逻辑，不碰界面和网络）。
//!
//! 手动启用要用户先确认"窗格停在空闲提示符"；自动启用没有人替我们确认，所以只有同时满足下面全部条件才发，
//! 任何一条不满足就不发（发了就是往一个可能正在跑别的东西的终端里写脚本）：
//!
//! - 登录 shell 探测（独立的 exec 通道，不碰终端）明确是 bash 或 zsh；
//! - 这次连接以来用户没往这个窗格输入过任何东西；
//! - 终端输出已经安静 [`QUIET_BEFORE_SEND`]；
//! - 屏幕上最后一行非空内容像提示符、光标紧跟在提示符字符后的那个空格后面（[`screen_at_idle_prompt`]）；
//! - 应用没锁定、AI 没在用这个终端、窗格里还没有集成、远端也还没自己发过输入行标记。
//!
//! 连接后最多等 [`ATTEMPT_WINDOW`]：时间到了还凑不齐就放弃，不会在用户已经开始干活之后才突然发。
//! 每次连接只判一次（发过不论成败、放弃之后都不再判），重连算新的一次——这个"一次"由窗格那一层记。

use std::time::Duration;

use super::cells::{CellKind, InputCell};
use super::integration::ShellKind;
use super::state::ScreenSource;
use crate::ai::shell_kind::LoginShell;

/// 终端输出安静到这么久才算"停在提示符了"。
pub const QUIET_BEFORE_SEND: Duration = Duration::from_millis(1500);
/// 连接建立后最多等这么久，条件一直凑不齐就放弃本次。
pub const ATTEMPT_WINDOW: Duration = Duration::from_secs(15);

/// 提示符常见的结尾字符：普通用户 `$`、root `#`、zsh `%`、PowerShell 风格 / 续行 `>`。
const PROMPT_CHARS: [char; 4] = ['$', '#', '%', '>'];

/// 这一行 + 光标列，看起来是不是一个空的 shell 提示符。
///
/// 规则：去掉行尾空白前，最后一个非空单元格是提示符字符（`$ # % >`）、它后面紧跟一个空格，并且光标正好在那个空格之后
/// （= 输入行是空的）。`cells` 是屏幕上的整行（尾部有空格补齐），所以"后面有空格"不能只看单元格——
/// `Router>` 后面的补齐空格看起来和 `$ ` 一样，要靠光标列区分：它的光标在 `>` 紧后，不在空格之后。
/// `Password: `、`--More--`、没有提示符字符结尾的行、光标不在这个位置、空行都不是。
pub fn looks_like_idle_prompt(cells: &[InputCell], cursor_col: usize) -> bool {
    let Some(last) = cells.iter().rposition(|cell| !cell.is_blank()) else {
        return false;
    };
    let prompt = &cells[last];
    if prompt.kind != CellKind::Normal
        || prompt.extra.is_some()
        || !PROMPT_CHARS.contains(&prompt.ch)
    {
        return false;
    }
    // 提示符字符后面必须还有一格（空格）；字符在最后一列时没有。
    if last + 1 >= cells.len() {
        return false;
    }
    cursor_col == last + 2
}

/// 屏幕上最后一行非空内容是不是空闲提示符，并且光标就在那一行。主屏幕、没有滚回历史才算。
pub fn screen_at_idle_prompt(screen: &impl ScreenSource) -> bool {
    let view = screen.view();
    if view.alt_screen || view.display_offset != 0 || view.rows == 0 {
        return false;
    }
    let last_row = (0..view.rows).rev().find(|&row| {
        screen
            .line(row)
            .is_some_and(|cells| cells.iter().any(|cell| !cell.is_blank()))
    });
    let Some(last_row) = last_row else {
        return false;
    };
    if view.cursor_row != last_row {
        return false;
    }
    screen
        .line(last_row)
        .is_some_and(|cells| looks_like_idle_prompt(&cells, view.cursor_col))
}

/// 登录 shell 探测的进展。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeState {
    /// 还没回来。
    Pending,
    /// 失败、超时、没拿到 shell 路径。
    Failed,
    Shell(LoginShell),
}

/// 没有自动启用的原因（界面按原因出词；只放实际会出现的）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// 没探测到登录 shell（失败、超时、没拿到路径）。
    ShellUnknown,
    /// 登录 shell 不是 bash / zsh。
    NotBashOrZsh,
    /// 连接后用户已经开始输入。
    UserTyped,
    /// 时间窗口内没等到"输出安静 + 停在空闲提示符"。
    NoIdlePrompt,
    /// AI 正在使用这个终端。
    AiBusy,
    Locked,
    /// 远端已经自己发了输入行标记，不需要再启用。
    AlreadyHasMarks,
    /// 脚本没能发出去（输入被拒绝、连接断了……）。
    SendFailed,
}

/// 一次连接上自动启用的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// 已经发出脚本（之后能不能就绪看集成自己的握手状态）。
    Enabled(ShellKind),
    Skipped(SkipReason),
}

/// 判定要看的事实（窗格那一层每次轮询时收集）。
#[derive(Clone, Copy, Debug)]
pub struct Inputs {
    /// 从连接建立到现在。
    pub elapsed: Duration,
    /// 终端输出已经安静了多久。
    pub quiet_for: Duration,
    pub probe: ProbeState,
    /// 这次连接以来用户是否往窗格输入过任何东西。
    pub user_typed: bool,
    pub ai_busy: bool,
    pub locked: bool,
    /// 窗格已经见过输入行标记（远端自己带了集成）。
    pub has_marks: bool,
}

/// 一次判定的结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// 条件还没凑齐、窗口还没过：下次轮询再看。
    Wait,
    /// 可以发这种脚本。
    Send(ShellKind),
    /// 放弃本次连接的自动启用，原因见 [`SkipReason`]。
    Skip(SkipReason),
}

/// 判定现在该等、该发、还是放弃。`at_idle_prompt` 要读屏幕，只在前面的条件都满足后才会调用。
pub fn decide(inputs: &Inputs, at_idle_prompt: impl FnOnce() -> bool) -> Decision {
    if inputs.user_typed {
        return Decision::Skip(SkipReason::UserTyped);
    }
    let expired = inputs.elapsed >= ATTEMPT_WINDOW;
    let kind = match inputs.probe {
        ProbeState::Pending => {
            return if expired {
                Decision::Skip(SkipReason::ShellUnknown)
            } else {
                Decision::Wait
            };
        }
        ProbeState::Failed => return Decision::Skip(SkipReason::ShellUnknown),
        ProbeState::Shell(LoginShell::Other) => return Decision::Skip(SkipReason::NotBashOrZsh),
        ProbeState::Shell(LoginShell::Bash) => ShellKind::Bash,
        ProbeState::Shell(LoginShell::Zsh) => ShellKind::Zsh,
    };
    if inputs.has_marks {
        return Decision::Skip(SkipReason::AlreadyHasMarks);
    }
    let blocker = if inputs.locked {
        Some(SkipReason::Locked)
    } else if inputs.ai_busy {
        Some(SkipReason::AiBusy)
    } else if inputs.quiet_for < QUIET_BEFORE_SEND || !at_idle_prompt() {
        Some(SkipReason::NoIdlePrompt)
    } else {
        None
    };
    match (blocker, expired) {
        // 全部满足，而且还在窗口内。
        (None, false) => Decision::Send(kind),
        (Some(_), false) => Decision::Wait,
        // 窗口已过：哪怕此刻凑齐了也不发（用户多半已经开始干活了）。
        (blocker, true) => Decision::Skip(blocker.unwrap_or(SkipReason::NoIdlePrompt)),
    }
}

#[cfg(test)]
mod tests;
