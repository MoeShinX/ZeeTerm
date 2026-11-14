//! 连接后自动启用 Shell 集成在窗格这一层的行为：什么条件下发、什么条件下一个字节都不发、只试一次。
//! 探测登录 shell 的 exec 通道用替身代替（不连任何服务器）；时间一律用"起点 + 偏移"的假时钟，不真等。

// 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`（导致展开时无限递归）。
use super::assist_ops::IntegrationState;
use super::assist_tests::{Remote, history_stats, shell_pane_for};
use super::{Pane, PaneState, WriteOrigin, WriteSource};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use zssh_core::ai::shell_kind::LoginShell;
use zssh_core::terminal_assist::KeyClass;
use zssh_core::terminal_assist::auto_integration::{
    ATTEMPT_WINDOW, Outcome, QUIET_BEFORE_SEND, SkipReason,
};
use zssh_core::terminal_assist::integration::{ShellKind, inject_line};

/// 测试里的时钟：连接建立的时刻 + 偏移。
struct Clock(Instant);

impl Clock {
    fn tick(&self, pane: &mut Pane, ms: u64) {
        pane.observe_input_at(self.0 + Duration::from_millis(ms));
    }
}

/// 一个刚连上的 SSH 窗格：设置打开、探测结果由替身给、屏幕上是一个空闲的 `$ ` 提示符（没有任何 OSC 133 标记）。
fn ssh_pane(shell: Option<LoginShell>) -> (Pane, Remote, Clock) {
    ssh_pane_with(shell, true, true)
}

/// 同 [`ssh_pane`]，两个开关（自动启用、命令建议总开关）在屏幕出现之前就按给定值设好。
fn ssh_pane_with(
    shell: Option<LoginShell>,
    auto_setting: bool,
    assist_enabled: bool,
) -> (Pane, Remote, Clock) {
    let clock = Clock(Instant::now());
    let (mut pane, remote) = shell_pane_for(Some(7));
    remote.set_echo(false);
    pane.assist.set_auto_setting(auto_setting);
    pane.set_assist_restrictions(false, false, false, assist_enabled);
    pane.assist.set_probe_hook_for_test(move || shell);
    pane.assist.begin_auto(true, clock.0);
    remote.output(&mut pane, b"Welcome\r\nuser@web-1:~$ ");
    (pane, remote, clock)
}

fn script_line(kind: ShellKind) -> Vec<u8> {
    format!("{}\r", inject_line(kind)).into_bytes()
}

fn outcome(pane: &Pane) -> Option<Outcome> {
    pane.assist.auto_outcome()
}

#[test]
fn with_the_switch_off_nothing_is_probed_or_sent() {
    let (mut pane, remote, clock) = ssh_pane_with(Some(LoginShell::Bash), false, true);
    for ms in [2_000, 5_000, 14_000] {
        clock.tick(&mut pane, ms);
    }
    assert!(remote.sent().is_empty());
    assert_eq!(pane.assist.probes_started_for_test(), 0, "也不探测");
    // 窗口走完之后这次连接就不再试了：窗口外再打开开关也不会补发。
    clock.tick(&mut pane, 16_000);
    pane.assist.set_auto_setting(true);
    clock.tick(&mut pane, 20_000);
    assert!(remote.sent().is_empty());
    assert_eq!(outcome(&pane), None, "关着时也没有要说明的结果");
}

#[test]
fn the_command_suggestion_master_switch_off_also_disables_it() {
    let (mut pane, remote, clock) = ssh_pane_with(Some(LoginShell::Bash), true, false);
    clock.tick(&mut pane, 3_000);
    assert!(remote.sent().is_empty());
    assert_eq!(pane.assist.probes_started_for_test(), 0);
}

#[test]
fn bash_at_a_quiet_idle_prompt_sends_exactly_the_manual_script_once_and_starts_the_handshake() {
    let (mut pane, remote, clock) = ssh_pane(Some(LoginShell::Bash));
    clock.tick(&mut pane, 1_000);
    assert!(remote.sent().is_empty(), "还没安静够 1.5 秒");
    clock.tick(&mut pane, 1_600);
    assert_eq!(
        remote.sent(),
        vec![script_line(ShellKind::Bash)],
        "恰好一行，和手动启用发的完全一样"
    );
    let line = String::from_utf8(remote.sent()[0].clone()).unwrap();
    assert!(
        line.starts_with(' ') && line.ends_with('\r'),
        "空格开头、一行"
    );
    assert!(matches!(
        pane.assist.integration(),
        IntegrationState::Handshake {
            kind: ShellKind::Bash,
            ..
        }
    ));
    assert_eq!(outcome(&pane), Some(Outcome::Enabled(ShellKind::Bash)));
    // 之后再怎么轮询都不会再发。
    for ms in [2_000, 4_000, 9_000] {
        clock.tick(&mut pane, ms);
    }
    assert_eq!(remote.sent().len(), 1);
    // 握手照常完成，完成之后也不会再发。
    remote.output(&mut pane, b"\r\n\x1b]133;A\x07$ \x1b]133;B\x07");
    assert!(matches!(
        pane.assist.integration(),
        IntegrationState::Active {
            kind: ShellKind::Bash
        }
    ));
    clock.tick(&mut pane, 20_000);
    assert_eq!(remote.sent().len(), 1);
}

#[test]
fn the_script_is_the_same_one_the_manual_path_sends() {
    let (mut manual, manual_remote) = shell_pane_for(Some(7));
    manual_remote.set_echo(false);
    manual.start_integration(ShellKind::Zsh).unwrap();
    let (mut auto, remote, clock) = ssh_pane(Some(LoginShell::Zsh));
    clock.tick(&mut auto, 2_000);
    assert_eq!(remote.sent(), manual_remote.sent());
    assert_eq!(remote.sent(), vec![script_line(ShellKind::Zsh)]);
}

#[test]
fn other_shells_and_a_failed_probe_send_nothing_and_say_why() {
    for (shell, reason) in [
        (Some(LoginShell::Other), SkipReason::NotBashOrZsh),
        (None, SkipReason::ShellUnknown),
    ] {
        let (mut pane, remote, clock) = ssh_pane(shell);
        clock.tick(&mut pane, 2_000);
        assert!(remote.sent().is_empty(), "{shell:?}");
        assert_eq!(outcome(&pane), Some(Outcome::Skipped(reason)), "{shell:?}");
        // 已经放弃：之后再轮询也不会发。
        clock.tick(&mut pane, 5_000);
        assert!(remote.sent().is_empty());
    }
}

#[test]
fn without_a_registered_terminal_the_real_probe_fails_and_nothing_is_sent() {
    let clock = Clock(Instant::now());
    let (mut pane, remote) = shell_pane_for(Some(7));
    remote.set_echo(false);
    pane.assist.set_auto_setting(true);
    // 没有替身：走真实路径，但窗格没有登记过终端，探测立刻失败（不连任何服务器）。
    pane.assist.begin_auto(true, clock.0);
    remote.output(&mut pane, b"user@web-1:~$ ");
    clock.tick(&mut pane, 3_000);
    assert!(remote.sent().is_empty());
    assert_eq!(
        outcome(&pane),
        Some(Outcome::Skipped(SkipReason::ShellUnknown))
    );
}

#[test]
fn anything_the_user_did_after_connecting_blocks_it_for_good() {
    let origins = [
        WriteOrigin::TypedText,
        WriteOrigin::ImeText,
        WriteOrigin::Key(KeyClass::Enter),
        WriteOrigin::Foreign(WriteSource::Paste),
        WriteOrigin::Foreign(WriteSource::QuickCommand),
        WriteOrigin::Untracked,
    ];
    for origin in origins {
        let (mut pane, remote, clock) = ssh_pane(Some(LoginShell::Bash));
        let bytes: &[u8] = if matches!(origin, WriteOrigin::Key(_)) {
            b"\r"
        } else {
            b"x"
        };
        pane.write_as(bytes, origin);
        let before = remote.sent().len();
        clock.tick(&mut pane, 2_000);
        clock.tick(&mut pane, 3_000);
        assert_eq!(
            remote.sent().len(),
            before,
            "{origin:?}：只有用户自己写的那一次"
        );
        assert_eq!(
            outcome(&pane),
            Some(Outcome::Skipped(SkipReason::UserTyped)),
            "{origin:?}"
        );
    }
}

#[test]
fn output_that_keeps_coming_or_a_screen_that_is_not_an_idle_prompt_waits_and_then_gives_up() {
    // 输出一直没安静：每隔一秒来一点，最后一次动静在 14 秒。
    let (mut noisy, remote, clock) = ssh_pane(Some(LoginShell::Bash));
    for ms in (1_000..=14_000).step_by(1_000) {
        noisy.pump_mut().feed_local(b".");
        clock.tick(&mut noisy, ms);
    }
    assert!(remote.sent().is_empty());
    clock.tick(&mut noisy, 15_500);
    assert!(remote.sent().is_empty(), "窗口已过");
    assert_eq!(
        outcome(&noisy),
        Some(Outcome::Skipped(SkipReason::NoIdlePrompt))
    );

    // 不像提示符：密码提示、交换机提示符（没有空格）、`--More--`。
    for screen in [
        &b"user@web-1's password: "[..],
        b"Router>",
        b"--More--",
        b"Last login: today",
    ] {
        let clock = Clock(Instant::now());
        let (mut pane, remote) = shell_pane_for(Some(7));
        remote.set_echo(false);
        pane.assist.set_auto_setting(true);
        pane.assist
            .set_probe_hook_for_test(|| Some(LoginShell::Zsh));
        pane.assist.begin_auto(true, clock.0);
        remote.output(&mut pane, screen);
        for ms in [2_000, 5_000, 14_000] {
            clock.tick(&mut pane, ms);
        }
        assert!(
            remote.sent().is_empty(),
            "{}",
            String::from_utf8_lossy(screen)
        );
        clock.tick(&mut pane, 15_100);
        assert_eq!(
            outcome(&pane),
            Some(Outcome::Skipped(SkipReason::NoIdlePrompt)),
            "{}",
            String::from_utf8_lossy(screen)
        );
    }
}

#[test]
fn a_cursor_that_is_not_right_after_the_prompt_blocks_it() {
    let clock = Clock(Instant::now());
    let (mut pane, remote) = shell_pane_for(Some(7));
    remote.set_echo(false);
    pane.assist.set_auto_setting(true);
    pane.assist
        .set_probe_hook_for_test(|| Some(LoginShell::Bash));
    pane.assist.begin_auto(true, clock.0);
    // 提示符画完，光标又被挪回行中间（比如 shell 在重绘一个带内容的输入行）。
    remote.output(&mut pane, b"user@web-1:~$ \x1b[3D");
    clock.tick(&mut pane, 3_000);
    assert!(remote.sent().is_empty());
}

#[test]
fn conditions_that_only_line_up_after_fifteen_seconds_send_nothing() {
    let clock = Clock(Instant::now());
    let (mut pane, remote) = shell_pane_for(Some(7));
    remote.set_echo(false);
    pane.assist.set_auto_setting(true);
    pane.assist
        .set_probe_hook_for_test(|| Some(LoginShell::Bash));
    pane.assist.begin_auto(true, clock.0);
    // 屏幕一直是空的，直到第 14 秒才画出提示符。
    clock.tick(&mut pane, 5_000);
    pane.pump_mut().feed_local(b"user@web-1:~$ ");
    clock.tick(&mut pane, 14_000);
    assert!(remote.sent().is_empty(), "刚画出来，还没安静够");
    let late = ATTEMPT_WINDOW.as_millis() as u64 + 500;
    assert!(late - 14_000 >= QUIET_BEFORE_SEND.as_millis() as u64);
    clock.tick(&mut pane, late);
    assert!(
        remote.sent().is_empty(),
        "到这时才凑齐，已经过了 15 秒：不发"
    );
    assert_eq!(
        outcome(&pane),
        Some(Outcome::Skipped(SkipReason::NoIdlePrompt))
    );
}

#[test]
fn telnet_serial_and_offline_panes_never_probe_or_send() {
    // Telnet / 串口：连上时没有 SSH 连接，`begin_auto(false)`。
    let clock = Clock(Instant::now());
    let (mut pane, remote) = shell_pane_for(Some(7));
    remote.set_echo(false);
    pane.assist.set_auto_setting(true);
    pane.assist
        .set_probe_hook_for_test(|| Some(LoginShell::Bash));
    pane.assist.begin_auto(false, clock.0);
    remote.output(&mut pane, b"login: ok\r\nrouter# ");
    for ms in [2_000, 5_000, 20_000] {
        clock.tick(&mut pane, ms);
    }
    assert!(remote.sent().is_empty());
    assert_eq!(pane.assist.probes_started_for_test(), 0);
    assert_eq!(outcome(&pane), None);

    // 离线面板（没有目标、不是会话）：同样什么也不发。
    let clock = Clock(Instant::now());
    let (mut offline, remote) = shell_pane_for(None);
    remote.set_echo(false);
    offline.assist.set_auto_setting(true);
    offline
        .assist
        .set_probe_hook_for_test(|| Some(LoginShell::Bash));
    offline.assist.begin_auto(true, clock.0);
    remote.output(&mut offline, b"user@web-1:~$ ");
    clock.tick(&mut offline, 3_000);
    assert!(remote.sent().is_empty());
    assert_eq!(offline.assist.probes_started_for_test(), 0);

    // 已经断开的窗格：不发，也不探测。
    let (mut gone, remote, clock) = ssh_pane(Some(LoginShell::Bash));
    gone.state = PaneState::Disconnected;
    clock.tick(&mut gone, 3_000);
    assert!(remote.sent().is_empty());
}

#[test]
fn an_ai_using_the_terminal_or_a_locked_app_blocks_it() {
    // AI 正在用这个终端：一直占着就一直不发，窗口过了放弃并说明。
    let (mut pane, remote, clock) = ssh_pane(Some(LoginShell::Bash));
    let id = "term-auto-test-ai-busy";
    pane.terminal_id = Some(id.to_owned());
    crate::ai_terminals::terminals().set_ai_active(9101, HashSet::from([id.to_owned()]));
    clock.tick(&mut pane, 2_000);
    clock.tick(&mut pane, 14_000);
    assert!(remote.sent().is_empty());
    clock.tick(&mut pane, 15_500);
    crate::ai_terminals::terminals().set_ai_active(9101, HashSet::new());
    assert!(remote.sent().is_empty());
    assert_eq!(outcome(&pane), Some(Outcome::Skipped(SkipReason::AiBusy)));

    // AI 在窗口内用完了：随后照常发。
    let (mut pane, remote, clock) = ssh_pane(Some(LoginShell::Bash));
    let id = "term-auto-test-ai-done";
    pane.terminal_id = Some(id.to_owned());
    crate::ai_terminals::terminals().set_ai_active(9102, HashSet::from([id.to_owned()]));
    clock.tick(&mut pane, 2_000);
    assert!(remote.sent().is_empty());
    crate::ai_terminals::terminals().set_ai_active(9102, HashSet::new());
    clock.tick(&mut pane, 3_000);
    assert_eq!(remote.sent().len(), 1);

    // 应用锁定：不发。
    let (mut pane, remote, clock) = ssh_pane(Some(LoginShell::Bash));
    crate::app_lock::set_locked(true);
    clock.tick(&mut pane, 2_000);
    clock.tick(&mut pane, 3_000);
    let sent_while_locked = remote.sent().len();
    crate::app_lock::set_locked(false);
    assert_eq!(sent_while_locked, 0);
    // 解锁后窗口还没过：继续等到条件满足再发。
    clock.tick(&mut pane, 5_000);
    assert_eq!(remote.sent().len(), 1);
}

#[test]
fn it_is_tried_once_per_connection_and_a_reconnect_gets_a_fresh_try() {
    let (mut pane, remote, clock) = ssh_pane(Some(LoginShell::Bash));
    clock.tick(&mut pane, 2_000);
    assert_eq!(remote.sent().len(), 1);
    // 握手失败（没收到标记）后也不会自动再发。
    for ms in [13_000, 20_000, 40_000] {
        clock.tick(&mut pane, ms);
    }
    assert_eq!(remote.sent().len(), 1, "不论成败只发一次");

    // 重新连接：新的一次。
    let reconnect = Clock(Instant::now());
    pane.assist.on_connection_reset(reconnect.0);
    pane.assist.begin_auto(true, reconnect.0);
    remote.output(&mut pane, b"\r\nuser@web-1:~$ ");
    reconnect.tick(&mut pane, 2_000);
    assert_eq!(remote.sent().len(), 2, "重连后再试一次");
    reconnect.tick(&mut pane, 6_000);
    assert_eq!(remote.sent().len(), 2);
}

#[test]
fn a_connection_that_gave_up_stays_given_up_even_if_the_screen_later_looks_ready() {
    let (mut pane, remote, clock) = ssh_pane(Some(LoginShell::Bash));
    pane.write_as(b"x", WriteOrigin::TypedText);
    clock.tick(&mut pane, 2_000);
    assert_eq!(
        outcome(&pane),
        Some(Outcome::Skipped(SkipReason::UserTyped))
    );
    // 用户清掉了输入、屏幕回到空闲提示符：这次连接不再自动尝试。
    pane.write_as(b"\x7f", WriteOrigin::Key(KeyClass::Backspace));
    clock.tick(&mut pane, 6_000);
    clock.tick(&mut pane, 9_000);
    assert_eq!(remote.sent().concat(), b"x\x7f".to_vec());
}

#[test]
fn stopping_the_integration_by_hand_means_no_automatic_enable_again_this_connection() {
    let (mut pane, remote, clock) = ssh_pane(Some(LoginShell::Zsh));
    clock.tick(&mut pane, 2_000);
    remote.output(&mut pane, b"\r\n\x1b]133;A\x07$ \x1b]133;B\x07");
    assert!(matches!(
        pane.assist.integration(),
        IntegrationState::Active { .. }
    ));
    pane.stop_integration();
    assert_eq!(pane.assist.integration(), IntegrationState::Off);
    assert_eq!(outcome(&pane), None, "停用后不再显示已自动启用");
    let after_stop = remote.sent().len();
    for ms in [5_000, 9_000, 30_000] {
        clock.tick(&mut pane, ms);
    }
    assert_eq!(
        remote.sent().len(),
        after_stop,
        "停用后本次连接不再自动启用"
    );
    assert_eq!(pane.assist.integration(), IntegrationState::Off);
}

#[test]
fn enabling_by_hand_while_waiting_takes_over_and_the_automatic_one_stands_down() {
    let (mut pane, remote, clock) = ssh_pane(Some(LoginShell::Bash));
    assert!(pane.assist.auto_waiting_for_test());
    pane.start_integration(ShellKind::Zsh).unwrap();
    assert!(!pane.assist.auto_waiting_for_test());
    for ms in [2_000, 5_000] {
        clock.tick(&mut pane, ms);
    }
    assert_eq!(remote.sent().len(), 1, "只有手动的那一条");
    assert_eq!(remote.sent()[0], script_line(ShellKind::Zsh));
    assert_eq!(outcome(&pane), None);
}

#[test]
fn a_session_that_already_has_prompt_marks_is_left_alone() {
    let clock = Clock(Instant::now());
    let (mut pane, remote) = shell_pane_for(Some(7));
    remote.set_echo(false);
    pane.assist.set_auto_setting(true);
    pane.assist
        .set_probe_hook_for_test(|| Some(LoginShell::Bash));
    pane.assist.begin_auto(true, clock.0);
    remote.output(&mut pane, b"\x1b]133;A\x07$ \x1b]133;B\x07");
    clock.tick(&mut pane, 3_000);
    assert!(remote.sent().is_empty());
    assert_eq!(
        outcome(&pane),
        Some(Outcome::Skipped(SkipReason::AlreadyHasMarks))
    );
}

#[test]
fn a_rejected_script_is_reported_and_not_retried() {
    let (mut pane, remote, clock) = ssh_pane(Some(LoginShell::Bash));
    remote.set_outcome(zssh_session::WriteOutcome::Rejected(
        zssh_session::RejectReason::QueueFull,
    ));
    clock.tick(&mut pane, 2_000);
    assert!(remote.sent().is_empty());
    assert_eq!(
        outcome(&pane),
        Some(Outcome::Skipped(SkipReason::SendFailed))
    );
    assert_eq!(pane.assist.integration(), IntegrationState::Off);
    remote.set_outcome(zssh_session::WriteOutcome::Queued);
    clock.tick(&mut pane, 5_000);
    assert!(remote.sent().is_empty(), "不重试");
}

#[test]
fn the_automatic_script_never_enters_the_command_history() {
    let (mut pane, remote, clock) = ssh_pane(Some(LoginShell::Bash));
    clock.tick(&mut pane, 2_000);
    assert_eq!(remote.sent().len(), 1);
    // shell 执行了脚本：命令开始 / 结束标记，随后带标记的新提示符。
    remote.output(&mut pane, b"\x1b]133;C\x07");
    remote.output(
        &mut pane,
        b"\x1b]133;D;0\x07\r\n\x1b]133;A\x07$ \x1b]133;B\x07",
    );
    assert_eq!(history_stats(&pane).entries, 0, "集成脚本不是用户命令");
}
