//! 窗格级的输入跟踪测试：用一个可控的假 shell 后端，验证写入结果信号、先登记再发送、
//! 同步回环回显、高延迟 / 超时 / 失败、锁屏拒绝、代次隔离，以及"只旁路观察、不改变按键字节"。

// 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`（导致展开时无限递归）。
use super::{
    CoreFilter, FlowGate, NullBackend, Pane, PaneState, QuickSpec, RejectReason, SessionBackend,
    SessionCommand, SessionPump, Size, WriteOrigin, WriteOutcome, WriteSource,
};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zssh_core::terminal_assist::{
    DesyncReason, HandoffKey, InputOrigin, KeyClass, Phase, RestrictionKind, TrackerEvent,
};
use zssh_session::SessionEvent;

const SIZE: Size = Size { rows: 6, cols: 40 };

/// 测试对假后端的遥控：写出去了什么、下一次写入给什么结果、要不要同步回显。
#[derive(Clone)]
struct Remote {
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
    outcome: Arc<Mutex<WriteOutcome>>,
    echo: Arc<Mutex<bool>>,
    tx: Sender<SessionEvent>,
}

impl Remote {
    fn sent(&self) -> Vec<Vec<u8>> {
        self.sent.lock().unwrap().clone()
    }

    fn set_outcome(&self, outcome: WriteOutcome) {
        *self.outcome.lock().unwrap() = outcome;
    }

    fn set_echo(&self, on: bool) {
        *self.echo.lock().unwrap() = on;
    }

    /// 旧连接的遥控器：窗格已经换了事件通道，发不进去也不是错误。
    fn output_ignoring_closed(&self, bytes: &[u8]) {
        let _ = self.tx.send(SessionEvent::Data {
            generation: 1,
            bytes: bytes.to_vec(),
        });
    }

    /// 远端发来一批输出（generation 1）。
    fn output(&self, pane: &mut Pane, bytes: &[u8]) {
        self.tx
            .send(SessionEvent::Data {
                generation: 1,
                bytes: bytes.to_vec(),
            })
            .unwrap();
        pane.poll_events();
    }
}

struct Shell {
    remote: Remote,
}

impl SessionBackend for Shell {
    fn send(&mut self, command: SessionCommand) {
        if let SessionCommand::Write(bytes) = command {
            let _ = self.send_write(bytes);
        }
    }

    fn send_write(&mut self, bytes: Vec<u8>) -> WriteOutcome {
        let outcome = *self.remote.outcome.lock().unwrap();
        if outcome.is_queued() {
            self.remote.sent.lock().unwrap().push(bytes.clone());
            if *self.remote.echo.lock().unwrap() {
                // 同步回环：回显在发送的同一个调用里就进了事件通道。
                let _ = self.remote.tx.send(SessionEvent::Data {
                    generation: 1,
                    bytes,
                });
            }
        }
        outcome
    }
}

fn shell_pane() -> (Pane, Remote) {
    let (tx, rx) = std::sync::mpsc::channel();
    let remote = Remote {
        sent: Arc::new(Mutex::new(Vec::new())),
        outcome: Arc::new(Mutex::new(WriteOutcome::Queued)),
        echo: Arc::new(Mutex::new(true)),
        tx,
    };
    let pump = SessionPump::new(SIZE, 100, Arc::new(FlowGate::new()), CoreFilter::new(), 1);
    let pane = Pane::with_parts(
        pump,
        Box::new(Shell {
            remote: remote.clone(),
        }),
        rx,
        Some(7),
        None,
    );
    (pane, remote)
}

fn at_prompt() -> (Pane, Remote) {
    let (mut pane, remote) = shell_pane();
    remote.output(&mut pane, b"\x1b]133;A\x07$ \x1b]133;B\x07");
    assert_eq!(pane.assist.phase(), Phase::Input);
    (pane, remote)
}

fn later(pane: &mut Pane, by: Duration) {
    let now = Instant::now() + by;
    pane.assist.after_output(pane.driver.pump_mut(), now);
}

fn confirmed(pane: &Pane) -> String {
    pane.assist.tracker().state().confirmed_text.clone()
}

#[test]
fn a_prompt_mark_from_the_remote_puts_the_pane_in_input_and_typing_is_confirmed_by_the_screen() {
    let (mut pane, remote) = at_prompt();
    assert_eq!(pane.assist.tracker().state().confirmed_text, "");
    // 回环后端在发送的同一个调用里就回显：必须先登记再发送才认得出。
    assert_eq!(
        pane.write_as(b"l", WriteOrigin::TypedText),
        WriteOutcome::Queued
    );
    assert_eq!(pane.assist.phase(), Phase::Input, "同步回显已确认");
    assert_eq!(confirmed(&pane), "l");
    assert_eq!(
        pane.write_as(b"s", WriteOrigin::TypedText),
        WriteOutcome::Queued
    );
    assert_eq!(confirmed(&pane), "ls");
    assert_eq!(remote.sent(), vec![b"l".to_vec(), b"s".to_vec()]);
}

#[test]
fn bytes_are_forwarded_unchanged_whatever_the_tracker_thinks() {
    let (mut pane, remote) = at_prompt();
    let sequences: [(&[u8], WriteOrigin); 8] = [
        (b"ab", WriteOrigin::TypedText),
        (b"\t", WriteOrigin::Key(KeyClass::Handoff(HandoffKey::Tab))),
        (
            b"\x1b[A",
            WriteOrigin::Key(KeyClass::Handoff(HandoffKey::Navigation)),
        ),
        (
            b"\x12",
            WriteOrigin::Key(KeyClass::Handoff(HandoffKey::HistorySearch)),
        ),
        (b"\x7f", WriteOrigin::Key(KeyClass::Backspace)),
        (b"\r", WriteOrigin::Key(KeyClass::Enter)),
        (
            b"\x1b[200~x\x1b[201~",
            WriteOrigin::Foreign(WriteSource::Paste),
        ),
        (b"\x1b[<0;1;1M", WriteOrigin::Untracked),
    ];
    for (bytes, origin) in sequences {
        assert_eq!(pane.write_as(bytes, origin), WriteOutcome::Queued);
    }
    let expected: Vec<Vec<u8>> = sequences.iter().map(|(b, _)| b.to_vec()).collect();
    assert_eq!(remote.sent(), expected, "Tab、Enter、方向键原样交给 shell");
}

#[test]
fn without_prompt_marks_nothing_is_tracked_and_typing_is_unaffected() {
    let (mut pane, remote) = shell_pane();
    remote.output(&mut pane, b"user@host:~$ ");
    assert_eq!(
        pane.write_as(b"ls", WriteOrigin::TypedText),
        WriteOutcome::Queued
    );
    assert_eq!(pane.assist.phase(), Phase::Unknown);
    assert!(!pane.assist.tracker().saw_prompt_marks());
    assert_eq!(remote.sent(), vec![b"ls".to_vec()]);
    // 没有任何启发式：就算屏幕上有 $ 也不猜。
    for _ in 0..3 {
        later(&mut pane, Duration::from_secs(10));
    }
    assert_eq!(pane.assist.phase(), Phase::Unknown);
}

#[test]
fn local_connecting_text_can_never_become_a_prompt() {
    let (mut pane, _remote) = shell_pane();
    pane.pump_mut()
        .feed_local(b"connecting \x1b]133;A\x07$ \x1b]133;B\x07\r\n");
    pane.observe_input();
    assert_eq!(pane.assist.phase(), Phase::Unknown);
    assert!(!pane.assist.tracker().saw_prompt_marks());
}

#[test]
fn high_latency_waits_then_confirms_and_the_five_second_limit_is_not_extended_by_output() {
    let (mut pane, remote) = at_prompt();
    remote.set_echo(false);
    assert_eq!(
        pane.write_as(b"ab", WriteOrigin::TypedText),
        WriteOutcome::Queued
    );
    assert_eq!(pane.assist.phase(), Phase::AwaitingEcho);
    assert_eq!(pane.assist.tracker().state().predicted_text, "ab");
    assert_eq!(confirmed(&pane), "", "预测不等于确认");

    // 2 秒后回显了一半。
    remote.output(&mut pane, b"a");
    assert_eq!(pane.assist.phase(), Phase::AwaitingEcho);
    assert_eq!(confirmed(&pane), "a");
    // 之后一直有无关输出（在屏幕别处写字，光标回到原位）也不能把 5 秒延长。
    remote.output(&mut pane, b"\x1b7\x1b[6;1Hnoise\x1b8");
    assert_eq!(pane.assist.phase(), Phase::AwaitingEcho);
    later(&mut pane, Duration::from_secs(6));
    assert_eq!(pane.assist.phase(), Phase::Desynced);
    assert_eq!(
        pane.assist.tracker().state().desync_reason,
        Some(DesyncReason::EchoTimeout)
    );
    assert_eq!(
        remote.sent(),
        vec![b"ab".to_vec()],
        "超时降级，绝不补发退格或补偿文本"
    );
}

#[test]
fn a_late_echo_inside_the_limit_still_confirms() {
    let (mut pane, remote) = at_prompt();
    remote.set_echo(false);
    pane.write_as(b"x", WriteOrigin::TypedText);
    later(&mut pane, Duration::from_secs(3));
    assert_eq!(pane.assist.phase(), Phase::AwaitingEcho);
    remote.output(&mut pane, b"x");
    assert_eq!(pane.assist.phase(), Phase::Input);
    assert_eq!(confirmed(&pane), "x");
}

#[test]
fn a_full_queue_is_rejected_loudly_and_never_counted_as_typed() {
    let (mut pane, remote) = at_prompt();
    remote.set_outcome(WriteOutcome::Rejected(RejectReason::QueueFull));
    assert_eq!(
        pane.write_as(b"x", WriteOrigin::TypedText),
        WriteOutcome::Rejected(RejectReason::QueueFull)
    );
    assert_eq!(pane.assist.phase(), Phase::Desynced);
    assert_eq!(
        pane.assist.tracker().state().desync_reason,
        Some(DesyncReason::WriteRejected(RejectReason::QueueFull))
    );
    assert_eq!(pane.assist.tracker().state().predicted_text, "");
    assert!(remote.sent().is_empty());
}

#[test]
fn a_transport_failure_desyncs_and_a_lost_enter_drops_the_pending_submission() {
    let (mut pane, remote) = at_prompt();
    pane.write_as(b"rm x", WriteOrigin::TypedText);
    assert_eq!(pane.assist.phase(), Phase::Input);
    remote.set_outcome(WriteOutcome::TransportFailed);
    assert_eq!(
        pane.write_as(b"\r", WriteOrigin::Key(KeyClass::Enter)),
        WriteOutcome::TransportFailed
    );
    assert_eq!(pane.assist.phase(), Phase::Desynced);
    assert_eq!(pane.assist.tracker().state().submitted_command, None);

    // 字符写入传输失败（可能已部分写入）：同样失同步。
    let (mut pane, remote) = at_prompt();
    remote.set_outcome(WriteOutcome::TransportFailed);
    assert_eq!(
        pane.write_as("你好".as_bytes(), WriteOrigin::ImeText),
        WriteOutcome::TransportFailed
    );
    assert_eq!(
        pane.assist.tracker().state().desync_reason,
        Some(DesyncReason::TransportFailed)
    );
}

#[test]
fn a_locked_app_rejects_the_write_and_the_tracker_never_pretends_it_was_typed() {
    let (mut pane, remote) = at_prompt();
    crate::app_lock::set_locked(true);
    let outcome = pane.write_as(b"secret", WriteOrigin::TypedText);
    crate::app_lock::set_locked(false);
    assert_eq!(outcome, WriteOutcome::Rejected(RejectReason::Locked));
    assert!(remote.sent().is_empty(), "锁屏时一个字节都不能出去");
    assert_eq!(pane.assist.phase(), Phase::Suspended);
    assert_eq!(pane.assist.tracker().state().predicted_text, "");
    // 解锁后限制解除：回到 Unknown，经协调或新 Prompt 才恢复，不恢复旧状态。
    pane.set_assist_restrictions(false, false, false, true);
    assert_eq!(pane.assist.phase(), Phase::Unknown);
}

#[test]
fn a_pane_that_is_not_live_rejects_writes_without_touching_the_backend() {
    let (mut pane, remote) = at_prompt();
    pane.state = PaneState::Disconnected;
    assert_eq!(
        pane.write_as(b"x", WriteOrigin::TypedText),
        WriteOutcome::Rejected(RejectReason::NotLive)
    );
    pane.state = PaneState::Connecting;
    assert_eq!(
        pane.write_as(b"x", WriteOrigin::Key(KeyClass::Enter)),
        WriteOutcome::Rejected(RejectReason::NotLive)
    );
    assert_eq!(
        pane.write_as(b"", WriteOrigin::TypedText),
        WriteOutcome::Rejected(RejectReason::Empty)
    );
    assert!(remote.sent().is_empty());
}

#[test]
fn a_new_connection_generation_isolates_the_tracker_and_old_marks_are_ignored() {
    let (mut pane, remote) = at_prompt();
    pane.write_as(b"ls", WriteOrigin::TypedText);
    // 取一个候选请求身份，连接换代后它必须作废。
    let id = pane.assist.tracker_mut().identity();
    assert!(pane.assist.tracker().is_current(&id));
    // 重连：新代次、新泵、新后端、新的事件通道。
    pane.generation = 2;
    let (new_tx, new_rx) = std::sync::mpsc::channel();
    pane.attach(
        Box::new(NullBackend),
        new_rx,
        Arc::new(FlowGate::new()),
        None,
    );
    assert_eq!(pane.assist.phase(), Phase::Unknown);
    assert!(!pane.assist.tracker().is_current(&id));
    assert_eq!(confirmed(&pane), "");
    // 旧代次的迟到数据（带 133;B）进不了新泵，不会产生标记。
    new_tx
        .send(SessionEvent::Data {
            generation: 1,
            bytes: b"\x1b]133;A\x07$ \x1b]133;B\x07".to_vec(),
        })
        .unwrap();
    pane.poll_events();
    assert_eq!(pane.assist.phase(), Phase::Unknown);
    // 旧会话的遥控器再也进不了这个窗格。
    remote.output_ignoring_closed(b"\x1b]133;B\x07");
    assert_eq!(pane.assist.phase(), Phase::Unknown);
    // 新代次自己的 Prompt 照常识别。
    new_tx
        .send(SessionEvent::Data {
            generation: 2,
            bytes: b"\x1b]133;A\x07$ \x1b]133;B\x07".to_vec(),
        })
        .unwrap();
    pane.poll_events();
    assert_eq!(pane.assist.phase(), Phase::Input);
}

#[test]
fn the_context_follows_the_pane_generation_and_target() {
    let (mut pane, remote) = at_prompt();
    assert_eq!(pane.assist.tracker().context().target, "host:7");
    pane.write_as(b"l", WriteOrigin::TypedText);
    pane.generation = 5;
    pane.observe_input();
    assert_eq!(pane.assist.tracker().context().connection_generation, 5);
    assert_eq!(
        pane.assist.phase(),
        Phase::Unknown,
        "代次变了整个状态机作废"
    );
    remote.output(&mut pane, b"");
    assert_eq!(pane.assist.phase(), Phase::Unknown);

    let (mut quick, _r) = at_prompt();
    quick.quick = Some(Arc::new(QuickSpec {
        target: zssh_core::model::QuickTarget {
            username: "u".into(),
            host: "web-1".into(),
            port: 2222,
        },
        password: String::new(),
        label: "u@web-1".into(),
    }));
    quick.observe_input();
    assert_eq!(
        quick.assist.tracker().context().target,
        "ssh://u@web-1:2222"
    );
    assert_eq!(quick.assist.phase(), Phase::Unknown, "切目标");
}

#[test]
fn paste_quick_commands_and_ai_injection_need_a_new_prompt_boundary() {
    let (mut pane, remote) = at_prompt();
    pane.write_as(b"ab", WriteOrigin::TypedText);
    pane.write_as(b"pasted", WriteOrigin::Foreign(WriteSource::Paste));
    assert_eq!(pane.assist.phase(), Phase::Unknown);
    remote.output(&mut pane, b"\r\n\x1b]133;A\x07$ \x1b]133;B\x07");
    assert_eq!(pane.assist.phase(), Phase::Input);

    // AI 往终端里写命令不经窗格的写路径：从泵的计数发现。
    remote
        .tx
        .send(SessionEvent::AiArm {
            generation: 1,
            marker: "m".into(),
            payload: "p".into(),
            command: "ls".into(),
        })
        .unwrap();
    pane.poll_events();
    assert_eq!(pane.assist.phase(), Phase::Unknown);
}

#[test]
fn full_screen_programs_mouse_reporting_and_scrollback_suspend_tracking() {
    let (mut pane, remote) = at_prompt();
    remote.output(&mut pane, b"\x1b[?1049h");
    assert_eq!(pane.assist.phase(), Phase::Suspended);
    remote.output(&mut pane, b"\x1b[?1049l");
    assert_eq!(pane.assist.phase(), Phase::Unknown);

    let (mut pane, remote) = at_prompt();
    remote.output(&mut pane, b"\x1b[?1002h\x1b[?1006h");
    pane.set_assist_restrictions(false, false, false, true);
    assert_eq!(pane.assist.phase(), Phase::Suspended);
    assert!(pane.assist.tracker().restrictions().mouse_report);

    let (mut pane, remote) = at_prompt();
    for i in 0..30 {
        remote.output(&mut pane, format!("line {i}\r\n").as_bytes());
    }
    pane.pump_mut().scroll(5);
    pane.observe_input();
    assert_eq!(pane.assist.phase(), Phase::Suspended);
}

#[test]
fn broadcast_and_modals_suspend_and_lifting_them_does_not_restore_stale_state() {
    let (mut pane, _remote) = at_prompt();
    pane.write_as(b"ab", WriteOrigin::TypedText);
    pane.set_assist_restrictions(false, true, false, true);
    assert_eq!(pane.assist.phase(), Phase::Suspended);
    assert_eq!(pane.assist.tracker().state().predicted_text, "");
    pane.set_assist_restrictions(false, false, true, true);
    assert_eq!(
        pane.assist.phase(),
        Phase::Suspended,
        "换成另一个限制仍然暂停"
    );
    pane.set_assist_restrictions(false, false, false, true);
    assert_eq!(pane.assist.phase(), Phase::Unknown);
}

#[test]
fn the_switch_off_stops_tracking_and_back_on_starts_from_unknown() {
    let (mut pane, remote) = at_prompt();
    pane.set_assist_restrictions(false, false, false, false);
    assert!(!pane.assist.enabled());
    assert_eq!(pane.assist.phase(), Phase::Unknown);
    remote.output(&mut pane, b"\x1b]133;A\x07$ \x1b]133;B\x07");
    assert_eq!(pane.assist.phase(), Phase::Unknown, "关着不跟踪");
    assert_eq!(
        pane.write_as(b"x", WriteOrigin::TypedText),
        WriteOutcome::Queued
    );
    assert_eq!(remote.sent(), vec![b"x".to_vec()], "关着也不影响输入");
    pane.set_assist_restrictions(false, false, false, true);
    remote.output(&mut pane, b"\r\n\x1b]133;A\x07$ \x1b]133;B\x07");
    assert_eq!(pane.assist.phase(), Phase::Input);
}

#[test]
fn a_resize_forgets_the_prompt_anchor_until_the_next_prompt() {
    let (mut pane, remote) = at_prompt();
    pane.resize(Size { rows: 8, cols: 30 });
    assert_eq!(pane.assist.phase(), Phase::Unknown);
    later(&mut pane, Duration::from_secs(10));
    assert_eq!(pane.assist.phase(), Phase::Unknown, "旧锚点不再可信");
    remote.output(&mut pane, b"\r\n\x1b]133;A\x07$ \x1b]133;B\x07");
    assert_eq!(pane.assist.phase(), Phase::Input);
}

#[test]
fn ime_preedit_is_not_recorded_and_the_final_commit_registers_once() {
    let (mut pane, remote) = at_prompt();
    pane.assist.on_ime_preedit(true, Instant::now());
    assert!(pane.assist.tracker().state().composing);
    assert!(remote.sent().is_empty(), "预编辑一个字节都不发");
    pane.assist.on_ime_preedit(false, Instant::now());
    pane.write_as("你好".as_bytes(), WriteOrigin::ImeText);
    assert_eq!(confirmed(&pane), "你好");
    assert_eq!(pane.assist.phase(), Phase::Input);
    assert_eq!(remote.sent(), vec!["你好".as_bytes().to_vec()], "只发一次");
}

#[test]
fn enter_then_the_command_start_mark_commits_exactly_one_submission() {
    let (mut pane, remote) = at_prompt();
    pane.write_as(b"ls", WriteOrigin::TypedText);
    pane.write_as(b"\r", WriteOrigin::Key(KeyClass::Enter));
    assert_eq!(
        pane.assist.phase(),
        Phase::Input,
        "回显换行在命令开始前不算失同步"
    );
    remote.output(&mut pane, b"\x1b]133;C\x07");
    assert_eq!(pane.assist.phase(), Phase::Running);
    let started: Vec<_> = pane
        .assist
        .drained_events()
        .iter()
        .filter(|event| matches!(event, TrackerEvent::CommandStarted { .. }))
        .cloned()
        .collect();
    assert_eq!(
        started,
        vec![TrackerEvent::CommandStarted {
            command: Some("ls".into())
        }]
    );
    assert_eq!(pane.assist.tracker().state().origin, InputOrigin::Clean);
}

#[test]
fn every_external_restriction_suspends_and_lifting_it_goes_back_through_unknown() {
    let (mut pane, remote) = at_prompt();
    for kind in [
        RestrictionKind::PasswordPrompt,
        RestrictionKind::Modal,
        RestrictionKind::Locked,
        RestrictionKind::Broadcast,
        RestrictionKind::MouseReport,
    ] {
        pane.assist
            .tracker_mut()
            .set_restriction(kind, true, Instant::now());
        assert_eq!(pane.assist.phase(), Phase::Suspended, "{kind:?}");
        pane.assist
            .tracker_mut()
            .set_restriction(kind, false, Instant::now());
        assert_eq!(pane.assist.phase(), Phase::Unknown, "{kind:?}");
        // 新 Prompt 把它带回输入行，供下一轮。
        remote.output(&mut pane, b"\r\n\x1b]133;A\x07$ \x1b]133;B\x07");
        assert_eq!(pane.assist.phase(), Phase::Input, "{kind:?}");
    }
}

// ---- 临时 Bash / Zsh 集成 ----

use super::assist_ops::{IntegrationError, IntegrationState, StopOutcome};
use zssh_core::terminal_assist::integration::{CLEANUP_COMMAND, HANDSHAKE_TIMEOUT, ShellKind};

#[test]
fn enabling_the_integration_sends_one_space_led_line_and_waits_for_the_first_prompt_mark() {
    let (mut pane, remote) = shell_pane();
    remote.set_echo(false);
    // 没有任何标记：不知道停在哪儿，由用户确认（界面那一步）之后才会走到这里。
    assert_eq!(pane.start_integration(ShellKind::Bash), Ok(()));
    let sent = remote.sent();
    assert_eq!(sent.len(), 1, "只发一条");
    let line = String::from_utf8(sent[0].clone()).unwrap();
    assert!(line.starts_with(' '), "前导空格：{line:.40}");
    assert!(line.ends_with('\r') && line.matches('\r').count() == 1);
    assert!(line.contains("__zeeterm_off"));
    assert!(matches!(
        pane.assist.integration(),
        IntegrationState::Handshake {
            kind: ShellKind::Bash,
            ..
        }
    ));
    assert!(!pane.assist.suggestions_available(), "没就绪不给候选");
    // shell 执行脚本后画出下一个提示符：带着 133;B，握手完成。
    remote.output(&mut pane, b"\r\n\x1b]133;A\x07$ \x1b]133;B\x07");
    assert!(matches!(
        pane.assist.integration(),
        IntegrationState::Active {
            kind: ShellKind::Bash
        }
    ));
    assert!(pane.assist.suggestions_available());
    assert_eq!(pane.assist.phase(), Phase::Input);
}

#[test]
fn the_handshake_stops_after_ten_seconds_without_a_ready_mark_and_sends_nothing_more() {
    let (mut pane, remote) = shell_pane();
    remote.set_echo(false);
    pane.start_integration(ShellKind::Zsh).unwrap();
    later(&mut pane, HANDSHAKE_TIMEOUT - Duration::from_secs(1));
    assert!(matches!(
        pane.assist.integration(),
        IntegrationState::Handshake { .. }
    ));
    later(&mut pane, HANDSHAKE_TIMEOUT + Duration::from_millis(50));
    assert!(matches!(
        pane.assist.integration(),
        IntegrationState::Failed {
            kind: ShellKind::Zsh
        }
    ));
    assert!(!pane.assist.suggestions_available(), "失败后停止本地建议");
    assert_eq!(
        remote.sent().len(),
        1,
        "尚无可靠 Prompt，不自动往远端补发清理命令"
    );
    // 下一次用户确认空闲后可以重试。
    assert_eq!(pane.start_integration(ShellKind::Zsh), Ok(()));
    assert_eq!(remote.sent().len(), 2);
}

#[test]
fn a_late_ready_mark_after_the_timeout_does_not_revive_a_failed_attempt_silently() {
    let (mut pane, remote) = shell_pane();
    remote.set_echo(false);
    pane.start_integration(ShellKind::Bash).unwrap();
    later(&mut pane, HANDSHAKE_TIMEOUT + Duration::from_secs(1));
    assert!(matches!(
        pane.assist.integration(),
        IntegrationState::Failed { .. }
    ));
    // 标记之后到了：状态机自己仍能用（输入行标记是真的），但这次尝试已经报告过失败，不改成成功。
    remote.output(&mut pane, b"\x1b]133;A\x07$ \x1b]133;B\x07");
    assert!(matches!(
        pane.assist.integration(),
        IntegrationState::Failed { .. }
    ));
}

#[test]
fn enabling_is_refused_when_the_pane_is_busy_has_text_or_cannot_take_it() {
    // 命令在运行。
    let (mut running, remote) = at_prompt();
    remote.output(&mut running, b"\x1b]133;C\x07");
    assert_eq!(
        running.start_integration(ShellKind::Bash),
        Err(IntegrationError::Running)
    );
    // 输入行里已经有字：脚本会接在它后面一起执行，绝不发。
    let (mut typed, remote) = at_prompt();
    typed.write_as(b"rm -rf x", WriteOrigin::TypedText);
    let before = remote.sent().len();
    assert_eq!(
        typed.start_integration(ShellKind::Bash),
        Err(IntegrationError::LineNotEmpty)
    );
    assert_eq!(remote.sent().len(), before);
    // 不是已连接的会话。
    let (mut offline, remote) = at_prompt();
    offline.state = PaneState::Disconnected;
    assert_eq!(
        offline.start_integration(ShellKind::Bash),
        Err(IntegrationError::NoSession)
    );
    assert!(remote.sent().is_empty());
    // 锁屏。
    let (mut locked, remote) = at_prompt();
    crate::app_lock::set_locked(true);
    let result = locked.start_integration(ShellKind::Bash);
    crate::app_lock::set_locked(false);
    assert_eq!(result, Err(IntegrationError::Locked));
    assert!(remote.sent().is_empty());
    // 开关关着。
    let (mut off, remote) = at_prompt();
    off.set_assist_restrictions(false, false, false, false);
    assert_eq!(
        off.start_integration(ShellKind::Bash),
        Err(IntegrationError::Disabled)
    );
    assert!(remote.sent().is_empty());
    // 上一次还在握手。
    let (mut twice, remote) = shell_pane();
    remote.set_echo(false);
    twice.start_integration(ShellKind::Bash).unwrap();
    assert_eq!(
        twice.start_integration(ShellKind::Bash),
        Err(IntegrationError::Handshaking)
    );
    assert_eq!(remote.sent().len(), 1);
}

#[test]
fn a_rejected_script_write_reports_the_failure_and_leaves_the_integration_off() {
    let (mut pane, remote) = at_prompt();
    remote.set_outcome(WriteOutcome::Rejected(RejectReason::QueueFull));
    assert_eq!(
        pane.start_integration(ShellKind::Bash),
        Err(IntegrationError::WriteFailed(WriteOutcome::Rejected(
            RejectReason::QueueFull
        )))
    );
    assert_eq!(pane.assist.integration(), IntegrationState::Off);
}

#[test]
fn the_script_never_enters_the_command_history_capture() {
    let (mut pane, remote) = at_prompt();
    pane.start_integration(ShellKind::Bash).unwrap();
    remote.output(&mut pane, b"\x1b]133;C\x07");
    let started: Vec<_> = pane
        .assist
        .drained_events()
        .iter()
        .filter(|event| matches!(event, TrackerEvent::CommandStarted { .. }))
        .cloned()
        .collect();
    assert_eq!(
        started,
        vec![TrackerEvent::CommandStarted { command: None }],
        "集成脚本不是用户命令，不进新历史"
    );
    assert_eq!(history_stats(&pane).entries, 0);
}

#[test]
fn disabling_sends_the_cleanup_only_at_an_empty_idle_prompt_otherwise_it_is_left_to_the_user() {
    // 空闲提示符：发卸载命令，集成回到 Off。
    let (mut pane, remote) = shell_pane();
    pane.start_integration(ShellKind::Bash).unwrap();
    remote.output(&mut pane, b"\r\n\x1b]133;A\x07$ \x1b]133;B\x07");
    assert!(matches!(
        pane.assist.integration(),
        IntegrationState::Active { .. }
    ));
    let before = remote.sent().len();
    assert_eq!(pane.stop_integration(), StopOutcome::Sent);
    let sent = remote.sent();
    assert_eq!(sent.len(), before + 1);
    assert_eq!(
        String::from_utf8(sent.last().unwrap().clone()).unwrap(),
        format!(" {CLEANUP_COMMAND}\r")
    );
    assert_eq!(pane.assist.integration(), IntegrationState::Off);

    // 输入行里有字：不发（会接在用户的文字后面被执行），交给用户手动。
    let (mut typed, remote) = at_prompt();
    typed
        .assist
        .begin_integration(ShellKind::Bash, Instant::now());
    typed.write_as(b"ls", WriteOrigin::TypedText);
    let before = remote.sent().len();
    assert_eq!(typed.stop_integration(), StopOutcome::Manual);
    assert_eq!(remote.sent().len(), before, "没有往远端补发任何东西");
    assert_eq!(typed.assist.integration(), IntegrationState::Off);

    // 握手失败、没有可靠 Prompt：同样不发。
    let (mut failed, remote) = shell_pane();
    remote.set_echo(false);
    failed.start_integration(ShellKind::Zsh).unwrap();
    later(&mut failed, HANDSHAKE_TIMEOUT + Duration::from_secs(1));
    let before = remote.sent().len();
    assert_eq!(failed.stop_integration(), StopOutcome::Manual);
    assert_eq!(remote.sent().len(), before);
}

#[test]
fn the_integration_ends_with_the_session() {
    let (mut pane, remote) = shell_pane();
    pane.start_integration(ShellKind::Bash).unwrap();
    remote.output(&mut pane, b"\r\n\x1b]133;A\x07$ \x1b]133;B\x07");
    assert!(matches!(
        pane.assist.integration(),
        IntegrationState::Active { .. }
    ));
    pane.disconnect("gone".into(), "gone");
    assert_eq!(pane.assist.integration(), IntegrationState::Off);
    assert!(!pane.assist.suggestions_available());
    let view = pane.assist_view_for_test();
    assert!(!view.live);
}

#[test]
fn suggestions_need_marks_and_a_working_integration_and_the_switch_on() {
    let (pane, _remote) = shell_pane();
    assert!(!pane.assist.suggestions_available(), "没有标记不给候选");
    let (pane, _remote) = at_prompt();
    assert!(pane.assist.suggestions_available(), "原生就有标记的 shell");
    let (mut pane, _remote) = at_prompt();
    pane.set_assist_restrictions(false, false, false, false);
    assert!(!pane.assist.suggestions_available(), "总开关关着");
}

// ---- 接受候选（P4 的接入点）：写入结果、高延迟、超时、锁屏 ----

use zssh_core::terminal_assist::AcceptRefusal;

fn typed_prefix() -> (Pane, Remote) {
    let (mut pane, remote) = at_prompt();
    pane.write_as(b"git st", WriteOrigin::TypedText);
    assert_eq!(confirmed(&pane), "git st");
    assert_eq!(pane.assist.phase(), Phase::Input);
    (pane, remote)
}

#[test]
fn accepting_sends_only_the_suffix_never_a_carriage_return_and_confirms_by_the_screen() {
    let (mut pane, remote) = typed_prefix();
    let id = pane.assist.request_identity();
    assert_eq!(
        pane.accept_suggestion("atus", &id),
        Ok(WriteOutcome::Queued)
    );
    let sent = remote.sent();
    assert_eq!(sent.last().unwrap(), &b"atus".to_vec(), "只有后缀");
    assert!(
        sent.iter().all(|chunk| !chunk.contains(&b'\r')),
        "接受不发回车：{sent:?}"
    );
    // 同步回环把 "atus" 回显出来：屏幕确认之后才算 confirmed。
    assert_eq!(confirmed(&pane), "git status");
    assert_eq!(pane.assist.phase(), Phase::Input);
}

#[test]
fn accepting_does_not_pretend_the_suffix_is_confirmed_before_the_echo_and_survives_high_latency() {
    let (mut pane, remote) = typed_prefix();
    remote.set_echo(false);
    let id = pane.assist.request_identity();
    pane.accept_suggestion("atus", &id).unwrap();
    assert_eq!(pane.assist.phase(), Phase::AwaitingEcho);
    assert_eq!(confirmed(&pane), "git st", "不提前改成已确认");
    assert_eq!(pane.assist.tracker().state().predicted_text, "git status");
    // 高延迟：回显分两块到。
    later(&mut pane, Duration::from_secs(2));
    assert_eq!(pane.assist.phase(), Phase::AwaitingEcho);
    remote.output(&mut pane, b"at");
    assert_eq!(confirmed(&pane), "git stat");
    assert_eq!(pane.assist.phase(), Phase::AwaitingEcho);
    remote.output(&mut pane, b"us");
    assert_eq!(confirmed(&pane), "git status");
    assert_eq!(pane.assist.phase(), Phase::Input);
}

#[test]
fn an_accepted_suffix_that_never_echoes_times_out_without_any_compensation() {
    let (mut pane, remote) = typed_prefix();
    remote.set_echo(false);
    let id = pane.assist.request_identity();
    pane.accept_suggestion("atus", &id).unwrap();
    later(&mut pane, Duration::from_secs(6));
    assert_eq!(pane.assist.phase(), Phase::Desynced);
    assert_eq!(
        pane.assist.tracker().state().desync_reason,
        Some(DesyncReason::EchoTimeout)
    );
    let sent = remote.sent();
    assert_eq!(
        sent.last().unwrap(),
        &b"atus".to_vec(),
        "超时之后没有再发任何字节（退格、重试、补偿文本都不发）：{sent:?}"
    );
    assert!(!pane.assist.tracker().can_accept());
}

#[test]
fn a_stale_identity_or_a_changed_line_sends_nothing() {
    let (mut pane, remote) = typed_prefix();
    let before = remote.sent().len();
    let stale = pane.assist.request_identity();
    pane.write_as(b"a", WriteOrigin::TypedText);
    assert_eq!(
        pane.accept_suggestion("tus", &stale),
        Err(AcceptRefusal::StaleRequest)
    );
    assert_eq!(remote.sent().len(), before + 1, "只有那个字符，没有后缀");
    // 新请求被更新的请求顶掉。
    let first = pane.assist.request_identity();
    let _second = pane.assist.request_identity();
    assert_eq!(
        pane.accept_suggestion("tus", &first),
        Err(AcceptRefusal::StaleRequest)
    );
    // 在途写入还没确认时不能接受。
    remote.set_echo(false);
    pane.write_as(b"t", WriteOrigin::TypedText);
    let id = pane.assist.request_identity();
    assert_eq!(
        pane.accept_suggestion("us", &id),
        Err(AcceptRefusal::NotInInput)
    );
}

#[test]
fn accepting_while_locked_not_live_or_with_a_full_queue_reports_and_never_pretends() {
    // 锁屏：不发，状态机暂停。
    let (mut pane, remote) = typed_prefix();
    let id = pane.assist.request_identity();
    let before = remote.sent().len();
    crate::app_lock::set_locked(true);
    let outcome = pane.accept_suggestion("atus", &id);
    crate::app_lock::set_locked(false);
    assert_eq!(outcome, Ok(WriteOutcome::Rejected(RejectReason::Locked)));
    assert_eq!(remote.sent().len(), before);
    assert_eq!(pane.assist.phase(), Phase::Suspended);
    assert_eq!(pane.assist.tracker().state().predicted_text, "");

    // 不是 Live。
    let (mut offline, remote) = typed_prefix();
    let id = offline.assist.request_identity();
    let before = remote.sent().len();
    offline.state = PaneState::Disconnected;
    assert_eq!(
        offline.accept_suggestion("atus", &id),
        Ok(WriteOutcome::Rejected(RejectReason::NotLive))
    );
    assert_eq!(remote.sent().len(), before);

    // 队列满：本地拒绝，失同步，预测里不留这个后缀。
    let (mut full, remote) = typed_prefix();
    let id = full.assist.request_identity();
    remote.set_outcome(WriteOutcome::Rejected(RejectReason::QueueFull));
    assert_eq!(
        full.accept_suggestion("atus", &id),
        Ok(WriteOutcome::Rejected(RejectReason::QueueFull))
    );
    assert_eq!(full.assist.phase(), Phase::Desynced);
    assert_eq!(full.assist.tracker().state().predicted_text, "git st");

    // 传输失败（可能已部分写入）：失同步，不重试后缀。
    let (mut broken, remote) = typed_prefix();
    let id = broken.assist.request_identity();
    let before = remote.sent().len();
    remote.set_outcome(WriteOutcome::TransportFailed);
    assert_eq!(
        broken.accept_suggestion("atus", &id),
        Ok(WriteOutcome::TransportFailed)
    );
    assert_eq!(broken.assist.phase(), Phase::Desynced);
    remote.set_outcome(WriteOutcome::Queued);
    later(&mut broken, Duration::from_secs(10));
    assert_eq!(remote.sent().len(), before, "不自动重试");
}

#[test]
fn accepting_is_refused_while_composing_or_after_the_shell_took_the_line_over() {
    let (mut pane, _remote) = typed_prefix();
    let id = pane.assist.request_identity();
    pane.assist.on_ime_preedit(true, Instant::now());
    assert_eq!(
        pane.accept_suggestion("atus", &id),
        Err(AcceptRefusal::Composing)
    );
    let (mut tabbed, _remote) = typed_prefix();
    tabbed.write_as(b"\t", WriteOrigin::Key(KeyClass::Handoff(HandoffKey::Tab)));
    let id = tabbed.assist.request_identity();
    assert_eq!(
        tabbed.accept_suggestion("atus", &id),
        Err(AcceptRefusal::NotInInput)
    );
}

// ---- 命令历史采集（P4 步骤 1） ----

fn target_of(pane: &Pane) -> String {
    pane.assist.tracker().context().target.clone()
}

fn history_stats(pane: &Pane) -> zssh_core::terminal_assist::history::Stats {
    zssh_core::terminal_assist::history::lock(pane.assist.history()).stats()
}

fn history_texts(pane: &Pane, prefix: &str) -> Vec<String> {
    let target = target_of(pane);
    zssh_core::terminal_assist::history::lock(pane.assist.history())
        .matches(&target, prefix)
        .into_iter()
        .map(|hit| hit.text)
        .collect()
}

/// 输入一行、回车，远端发来命令开始标记（再给一个新的 Prompt）。
fn run_line(pane: &mut Pane, remote: &Remote, line: &str) {
    pane.write_as(line.as_bytes(), WriteOrigin::TypedText);
    pane.write_as(b"\r", WriteOrigin::Key(KeyClass::Enter));
    remote.output(pane, b"\x1b]133;C\x07");
    remote.output(pane, b"\x1b]133;D;0\x07\r\n\x1b]133;A\x07$ \x1b]133;B\x07");
}

#[test]
fn a_submitted_command_enters_the_target_history_only_after_the_start_mark() {
    let (mut pane, remote) = at_prompt();
    pane.write_as(b"ls -la", WriteOrigin::TypedText);
    pane.write_as(b"\r", WriteOrigin::Key(KeyClass::Enter));
    assert_eq!(history_stats(&pane).entries, 0, "只按了 Enter 还不算已执行");
    remote.output(&mut pane, b"\x1b]133;C\x07");
    assert_eq!(history_texts(&pane, "ls"), vec!["ls -la".to_owned()]);
    assert_eq!(target_of(&pane), "host:7", "按目标身份分域");
}

#[test]
fn a_line_that_never_got_its_start_mark_is_discarded_with_the_next_prompt() {
    let (mut pane, remote) = at_prompt();
    pane.write_as(b"vim x", WriteOrigin::TypedText);
    pane.write_as(b"\r", WriteOrigin::Key(KeyClass::Enter));
    remote.output(&mut pane, b"\n\x1b]133;A\x07$ \x1b]133;B\x07");
    assert_eq!(pane.assist.phase(), Phase::Input);
    run_line(&mut pane, &remote, "pwd");
    assert_eq!(history_stats(&pane).entries, 1);
    assert!(history_texts(&pane, "vi").is_empty());
}

#[test]
fn leading_space_pasted_quick_and_uncertain_lines_are_never_collected() {
    let (mut pane, remote) = at_prompt();
    run_line(&mut pane, &remote, " ls -la");
    assert_eq!(history_stats(&pane).entries, 0, "前导空格一律不采集");

    // 粘贴：不是逐键用户命令。
    pane.write_as(b"echo pasted\r", WriteOrigin::Foreign(WriteSource::Paste));
    remote.output(&mut pane, b"\x1b]133;C\x07");
    remote.output(&mut pane, b"\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07");
    // 快捷命令。
    pane.write_as(b"uptime\r", WriteOrigin::Foreign(WriteSource::QuickCommand));
    remote.output(&mut pane, b"\x1b]133;C\x07");
    remote.output(&mut pane, b"\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07");
    assert_eq!(history_stats(&pane).entries, 0);

    // 中途被 shell 接管（Tab 补全）：行内容来源不确定，不采集。
    pane.write_as(b"gi", WriteOrigin::TypedText);
    pane.write_as(b"\t", WriteOrigin::Key(KeyClass::Handoff(HandoffKey::Tab)));
    pane.write_as(b"\r", WriteOrigin::Key(KeyClass::Enter));
    remote.output(&mut pane, b"\x1b]133;C\x07");
    assert_eq!(history_stats(&pane).entries, 0);
}

#[test]
fn history_is_one_domain_per_target_and_never_leaks_across_panes() {
    let (mut a, remote_a) = at_prompt();
    run_line(&mut a, &remote_a, "ls /srv");
    // 另一个目标的窗格共用同一份进程历史，但按目标分域。
    let (tx, rx) = std::sync::mpsc::channel();
    let pump = SessionPump::new(SIZE, 100, Arc::new(FlowGate::new()), CoreFilter::new(), 1);
    let remote_b = Remote {
        sent: Arc::new(Mutex::new(Vec::new())),
        outcome: Arc::new(Mutex::new(WriteOutcome::Queued)),
        echo: Arc::new(Mutex::new(true)),
        tx,
    };
    let mut b = Pane::with_parts(
        pump,
        Box::new(Shell {
            remote: remote_b.clone(),
        }),
        rx,
        Some(99),
        None,
    );
    remote_b.output(&mut b, b"\x1b]133;A\x07$ \x1b]133;B\x07");
    run_line(&mut b, &remote_b, "ls /var");
    assert_eq!(target_of(&b), "host:99");
    assert_eq!(history_texts(&a, "ls"), vec!["ls /srv".to_owned()]);
    assert_eq!(history_texts(&b, "ls"), vec!["ls /var".to_owned()]);
}

#[test]
fn switching_the_history_switch_off_stops_collecting_and_clears_the_module_history() {
    let (mut pane, remote) = at_prompt();
    run_line(&mut pane, &remote, "ls /srv");
    assert_eq!(history_stats(&pane).entries, 1);
    pane.assist.set_history_enabled(false);
    assert_eq!(history_stats(&pane).entries, 0, "关掉即清空");
    run_line(&mut pane, &remote, "ls /var");
    assert_eq!(history_stats(&pane).entries, 0, "关着时不采集");
    pane.assist.set_history_enabled(true);
    run_line(&mut pane, &remote, "ls /opt");
    assert_eq!(history_texts(&pane, "ls"), vec!["ls /opt".to_owned()]);
}
