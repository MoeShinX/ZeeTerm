//! V2-U44 B：断线自动重连在工作区一侧的行为。时间用手写的 `Instant`（不真等），连接结果用 `dial_script`
//! 备好（失败 / 连上进程内测试服务器），不连任何真实服务器。

use super::{
    DialStep, Pane, PaneState, Size, Tab, TermMsg, TerminalWorkspace, auto_overlay_line,
    next_generation,
};
use crate::auto_reconnect::AutoView;
use crate::settings_store::AppSettings;
use gpui::{AppContext as _, TestAppContext, WindowHandle};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use zssh_core::ssh::flow::FlowGate;
use zssh_session::{CoreFilter, SessionBackend, SessionCommand, SessionEvent, SessionPump};

const SIZE: Size = Size { rows: 10, cols: 50 };

struct Recorder(Arc<Mutex<Vec<SessionCommand>>>);

impl SessionBackend for Recorder {
    fn send(&mut self, command: SessionCommand) {
        self.0.lock().expect("log").push(command);
    }
}

fn open(cx: &mut TestAppContext) -> WindowHandle<TerminalWorkspace> {
    cx.update(|cx| {
        gpui_kit::init(cx);
        AppSettings::init_for_test(cx);
    });
    cx.add_window(|window, cx| TerminalWorkspace::new(window, cx, "auto-tests".to_owned()))
}

/// 一个已连上的主机会话窗格（假后端，事件从 `Sender` 灌进去）；`ssh` 决定它算不算 SSH 会话。
fn live_pane(host: i64, ssh: bool) -> (Pane, mpsc::Sender<SessionEvent>) {
    let (tx, rx) = mpsc::channel();
    let pump = SessionPump::new(SIZE, 100, Arc::new(FlowGate::new()), CoreFilter::new(), 1);
    let mut pane = Pane::with_parts(
        pump,
        Box::new(Recorder(Arc::new(Mutex::new(Vec::new())))),
        rx,
        Some(host),
        None,
    );
    pane.ssh_session = ssh;
    (pane, tx)
}

fn push(ws: &mut TerminalWorkspace, title: &str, pane: Pane) -> u64 {
    let tab = Tab::new(title.to_owned(), pane);
    let id = tab.id;
    ws.tabs.push(tab);
    id
}

/// 让会话结束：`code` / `reason` 就是远端给的。
fn end_session(tx: &mpsc::Sender<SessionEvent>, code: Option<i32>, reason: Option<&str>) {
    match reason {
        Some(message) => tx
            .send(SessionEvent::Error {
                generation: 1,
                message: message.to_owned(),
            })
            .expect("send"),
        None => tx
            .send(SessionEvent::Exit {
                generation: 1,
                code,
            })
            .expect("send"),
    }
}

fn pane_of(ws: &TerminalWorkspace, tab: usize) -> &Pane {
    &ws.tabs.items()[tab].panes[0]
}

fn pane_mut(ws: &mut TerminalWorkspace, tab: usize) -> &mut Pane {
    &mut ws.tabs.items_mut()[tab].panes[0]
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn screen(pane: &Pane) -> String {
    pane.snapshot
        .rows
        .iter()
        .map(|row| row.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 一拍：先收各窗格的事件（和界面的 60 毫秒节拍同一个顺序），再走自动重连的节拍。
fn beat(ws: &mut TerminalWorkspace, now: Instant, locked: bool) -> bool {
    ws.poll_sessions();
    ws.auto_reconnect_tick(now, locked)
}

fn view(ws: &TerminalWorkspace, tab: usize, now: Instant) -> Option<AutoView> {
    pane_of(ws, tab).auto_view(now)
}

#[gpui::test]
fn an_unexpected_ssh_drop_starts_a_round_that_fires_after_three_seconds(cx: &mut TestAppContext) {
    let window = open(cx);
    window
        .update(cx, |ws, _, _| {
            let (pane, tx) = live_pane(7, true);
            push(ws, "web-1", pane);
            let base = Instant::now();
            // 没断之前什么都没有。
            assert!(!beat(ws, base, false));
            assert_eq!(view(ws, 0, base), None);

            end_session(&tx, None, None);
            assert!(beat(ws, base, false));
            assert_eq!(pane_of(ws, 0).state, PaneState::Disconnected);
            assert_eq!(
                view(ws, 0, base),
                Some(AutoView::Waiting {
                    secs: 3,
                    attempt: 1
                })
            );
            // 倒计时每秒变一次，没变不重画。
            assert!(!beat(ws, base + Duration::from_millis(100), false));
            assert!(beat(ws, base + secs(1), false));
            assert_eq!(
                view(ws, 0, base + secs(1)),
                Some(AutoView::Waiting {
                    secs: 2,
                    attempt: 1
                })
            );
            // 到点之前不重连。
            ws.dial_script
                .push_back(DialStep::Fail("连接失败：timed out".to_owned()));
            beat(ws, base + Duration::from_millis(2900), false);
            assert_eq!(pane_of(ws, 0).state, PaneState::Disconnected);
            assert_eq!(ws.dial_script.len(), 1, "还没到点，没有去拨号");
            // 到点：窗格原地换成新连接（正在连），遮罩继续盖着写「正在重连…」。
            beat(ws, base + secs(3), false);
            assert!(ws.dial_script.is_empty(), "用掉了一次拨号");
            assert_eq!(pane_of(ws, 0).state, PaneState::Connecting);
            assert_eq!(
                view(ws, 0, base + secs(3)),
                Some(AutoView::Connecting { attempt: 1 })
            );
            assert!(
                pane_of(ws, 0).overlay_text().is_some(),
                "正在重连时遮罩还盖着"
            );
            assert_eq!(pane_of(ws, 0).overlay_reason(), None);
        })
        .expect("window");
}

#[gpui::test]
fn failures_back_off_5_10_20_30_and_stop_after_ten_attempts(cx: &mut TestAppContext) {
    let window = open(cx);
    window
        .update(cx, |ws, _, cx| {
            let (pane, tx) = live_pane(7, true);
            push(ws, "web-1", pane);
            let base = Instant::now();
            end_session(&tx, None, Some("读远端失败：connection reset"));
            beat(ws, base, false);

            let mut now = base + secs(3);
            let waits = [5u64, 10, 20, 30, 30, 30, 30, 30, 30];
            for attempt in 1..=10u32 {
                ws.dial_script
                    .push_back(DialStep::Fail("连接失败：timed out".to_owned()));
                beat(ws, now, false);
                assert_eq!(
                    view(ws, 0, now),
                    Some(AutoView::Connecting { attempt }),
                    "第 {attempt} 次正在连"
                );
                // 失败结果由下一拍处理。
                beat(ws, now, false);
                if attempt == 10 {
                    break;
                }
                let wait = waits[attempt as usize - 1];
                assert_eq!(
                    view(ws, 0, now),
                    Some(AutoView::Waiting {
                        secs: wait,
                        attempt: attempt + 1
                    }),
                    "第 {attempt} 次失败后等 {wait} 秒"
                );
                now += secs(wait);
            }
            // 第十次也失败：停下，遮罩回到手动，终端里留一行原因。
            assert_eq!(view(ws, 0, now), Some(AutoView::Stopped));
            assert_eq!(pane_of(ws, 0).state, PaneState::Disconnected);
            assert!(screen(pane_of(ws, 0)).contains("timed out"));
            let calls_before = ws.dial_script.len();
            beat(ws, now + secs(600), false);
            assert_eq!(ws.dial_script.len(), calls_before, "用尽后不再拨号");
            assert_eq!(view(ws, 0, now + secs(600)), Some(AutoView::Stopped));
            // 手动重连仍然可用，并且清掉这一轮。
            ws.dial_script
                .push_back(DialStep::Fail("连接失败：refused".to_owned()));
            assert!(ws.reconnect_overlay_pane(0, cx));
            beat(ws, now + secs(601), false);
            assert_eq!(
                view(ws, 0, now + secs(601)),
                None,
                "手动重连不接着自动那一轮"
            );
            assert_eq!(pane_of(ws, 0).overlay_reason(), Some("连接失败：refused"));
        })
        .expect("window");
}

#[gpui::test]
fn only_unexpected_ssh_drops_start_a_round(cx: &mut TestAppContext) {
    let window = open(cx);
    window
        .update(cx, |ws, _, _| {
            let base = Instant::now();
            /// 名字、是不是 SSH、退出码、原因、是否该开始一轮。
            type Case<'a> = (&'a str, bool, Option<i32>, Option<&'a str>, bool);
            let cases: [Case; 8] = [
                ("ssh no code", true, None, None, true),
                ("ssh reason", true, None, Some("保活超时"), true),
                ("ssh exit 0", true, Some(0), None, false),
                ("ssh exit 1", true, Some(1), None, false),
                ("ssh exit 255", true, Some(255), None, false),
                ("telnet no code", false, None, None, false),
                ("serial reason", false, None, Some("设备被拔掉"), false),
                (
                    "ssh reason with code",
                    true,
                    Some(0),
                    Some("连接被重置"),
                    true,
                ),
            ];
            for (name, ssh, code, reason, expect) in cases {
                let (pane, tx) = live_pane(7, ssh);
                let index = ws.tabs.len();
                push(ws, name, pane);
                end_session(&tx, code, reason);
                beat(ws, base, false);
                assert_eq!(
                    pane_of(ws, index).auto.is_active(),
                    expect,
                    "{name}：自动重连开没开"
                );
                assert_eq!(
                    pane_of(ws, index).state,
                    PaneState::Disconnected,
                    "{name}：都显示已断开"
                );
            }
        })
        .expect("window");
}

#[gpui::test]
fn a_user_disconnect_and_a_first_connection_failure_never_start_a_round(cx: &mut TestAppContext) {
    let window = open(cx);
    window
        .update(cx, |ws, _, cx| {
            let base = Instant::now();
            // 用户在右键菜单里主动断开。
            let (pane, _tx) = live_pane(7, true);
            push(ws, "web-1", pane);
            assert_eq!(ws.disconnect_tab(0, cx), 1);
            beat(ws, base, false);
            assert!(!pane_of(ws, 0).auto.is_active());
            beat(ws, base + secs(60), false);
            assert_eq!(view(ws, 0, base + secs(60)), None, "主动断开从不自动重连");

            // 第一次连接就失败：用户就在跟前，按现有逻辑显示失败。
            let pending = crate::ssh_backend::ready(Err("连接失败：refused".to_owned()));
            let mut failing = Pane::connecting(SIZE, 100, next_generation(), Some(8), pending);
            failing.ssh_session = true;
            push(ws, "web-2", failing);
            beat(ws, base, false);
            assert_eq!(pane_of(ws, 1).overlay_reason(), Some("连接失败：refused"));
            assert!(!pane_of(ws, 1).auto.is_active());
            beat(ws, base + secs(60), false);
            assert_eq!(view(ws, 1, base + secs(60)), None);
        })
        .expect("window");
}

#[gpui::test]
fn reconnecting_over_the_test_server_keeps_the_scrollback_and_clears_the_count(
    cx: &mut TestAppContext,
) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("运行时");
    let dir = tempfile::Builder::new()
        .prefix("zssh-u44-")
        .tempdir()
        .expect("临时目录");
    let connect = || {
        let addr = runtime.block_on(crate::test_ssh_server::spawn());
        let connection =
            runtime.block_on(crate::test_ssh_server::connect_trusting(addr, dir.path()));
        crate::ssh_backend::SshSessionFactory::from_connection(connection)
    };
    let window = open(cx);
    window
        .update(cx, |ws, _, _| {
            let base = Instant::now();
            // 真的连上测试服务器，打点字，再让服务端不发退出码直接断开整条连接。
            let mut pane = Pane::start_quick(
                Arc::new(super::QuickSpec {
                    target: zssh_core::model::QuickTarget {
                        username: "tester".to_owned(),
                        host: "192.0.2.10".to_owned(),
                        port: 22,
                    },
                    password: String::new(),
                    label: "tester@lab".to_owned(),
                }),
                Some(connect()),
                SIZE,
                100,
                next_generation(),
            );
            let id = pane.id;
            wait_for(&mut pane, |p| p.state == PaneState::Live);
            assert!(pane.ssh_session, "有已认证连接的会话算 SSH");
            pane.write(b"hello\r");
            wait_for(&mut pane, |p| screen(p).contains("ECHO:hello"));
            push(ws, "demo", pane);
            pane_mut(ws, 0).write(b"drop\r");
            wait_until(ws, |ws| pane_of(ws, 0).state == PaneState::Disconnected);
            beat(ws, base, false);
            assert!(pane_of(ws, 0).auto.is_active(), "意外断开开了一轮");

            // 第一次到点：拨号失败，一轮继续；第二次到点：连上。
            ws.dial_script
                .push_back(DialStep::Fail("连接失败：timed out".to_owned()));
            ws.dial_script.push_back(DialStep::Connect(connect()));
            beat(ws, base + secs(3), false);
            beat(ws, base + secs(3), false);
            assert_eq!(
                view(ws, 0, base + secs(3)),
                Some(AutoView::Waiting {
                    secs: 5,
                    attempt: 2
                })
            );
            beat(ws, base + secs(8), false);
            wait_until(ws, |ws| pane_of(ws, 0).state == PaneState::Live);
            let pane = pane_of(ws, 0);
            assert_eq!(pane.id, id, "窗格编号不变，分屏树认得它");
            assert_eq!(pane.auto_view(base + secs(9)), None, "连上了，计数清零");
            assert!(!pane.auto.is_active());
            let text = screen(pane);
            assert!(text.contains("ECHO:hello"), "回滚保留：{text}");
            assert!(
                text.contains(crate::i18n::t(TermMsg::Reconnected)),
                "写了重连标记：{text}"
            );
            assert_eq!(
                text.matches(crate::i18n::t(TermMsg::Connecting)).count(),
                0,
                "自动重连的尝试不往回滚里刷「正在连接…」：{text}"
            );

            // 之后再断开算新的一轮：又从第 1 次、3 秒开始。
            pane_mut(ws, 0).write(b"drop\r");
            wait_until(ws, |ws| pane_of(ws, 0).state == PaneState::Disconnected);
            let later = base + secs(100);
            beat(ws, later, false);
            assert_eq!(
                view(ws, 0, later),
                Some(AutoView::Waiting {
                    secs: 3,
                    attempt: 1
                })
            );
        })
        .expect("window");
}

fn wait_for(pane: &mut Pane, done: impl Fn(&Pane) -> bool) {
    let deadline = Instant::now() + secs(20);
    loop {
        pane.poll_events();
        if done(pane) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "等了 20 秒仍未满足：{}",
            screen(pane)
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn wait_until(ws: &mut TerminalWorkspace, done: impl Fn(&TerminalWorkspace) -> bool) {
    let deadline = Instant::now() + secs(20);
    loop {
        ws.poll_sessions();
        if done(ws) {
            return;
        }
        assert!(Instant::now() < deadline, "等了 20 秒仍未满足");
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[gpui::test]
fn cancel_stops_the_round_and_the_overlay_goes_back_to_manual(cx: &mut TestAppContext) {
    let window = open(cx);
    window
        .update(cx, |ws, _, cx| {
            let (pane, tx) = live_pane(7, true);
            let id = pane.id;
            push(ws, "web-1", pane);
            let base = Instant::now();
            end_session(&tx, None, None);
            beat(ws, base, false);
            assert!(pane_of(ws, 0).auto.is_waiting());

            // 等待中取消：不再到点重连，遮罩只剩手动重连的提示。
            ws.dial_script
                .push_back(DialStep::Fail("连接失败：x".to_owned()));
            ws.cancel_auto_reconnect(id, cx);
            beat(ws, base + secs(60), false);
            assert_eq!(ws.dial_script.len(), 1, "取消后没有再拨号");
            assert_eq!(view(ws, 0, base + secs(60)), None);
            assert!(
                pane_of(ws, 0).overlay_reason().is_some(),
                "遮罩还在，按 R 仍可手动重连"
            );

            // 正在连的时候取消：这一次尝试一并放弃，窗格回到已断开，原因沿用。
            let (pane, tx) = live_pane(8, true);
            let id = pane.id;
            push(ws, "web-2", pane);
            let reason = "读远端失败：reset";
            end_session(&tx, None, Some(reason));
            beat(ws, base, false);
            let pane_tab = 1usize;
            ws.dial_script.clear();
            ws.dial_script
                .push_back(DialStep::Fail("连接失败：y".to_owned()));
            // 这一拍到点发起重连：窗格换成「正在连」（备好的失败结果要下一次收事件才到）。
            ws.auto_reconnect_tick(base + secs(3), false);
            assert_eq!(pane_of(ws, pane_tab).state, PaneState::Connecting);
            ws.cancel_auto_reconnect(id, cx);
            assert_eq!(pane_of(ws, pane_tab).state, PaneState::Disconnected);
            assert_eq!(pane_of(ws, pane_tab).overlay_reason(), Some(reason));
            // 被放弃的那次尝试的结果不会再改任何东西。
            beat(ws, base + secs(4), false);
            assert_eq!(view(ws, pane_tab, base + secs(4)), None);
        })
        .expect("window");
}

#[gpui::test]
fn turning_the_setting_off_stops_waiting_and_connecting_rounds_at_once(cx: &mut TestAppContext) {
    let window = open(cx);
    window
        .update(cx, |ws, _, cx| {
            let base = Instant::now();
            let (waiting, tx_a) = live_pane(7, true);
            push(ws, "waiting", waiting);
            let (connecting, tx_b) = live_pane(8, true);
            push(ws, "connecting", connecting);
            end_session(&tx_a, None, None);
            end_session(&tx_b, None, None);
            beat(ws, base, false);
            ws.dial_script
                .push_back(DialStep::Fail("连接失败：z".to_owned()));
            // 只让第二个窗格到点（把第一个往后推）。
            pane_mut(ws, 0).auto.pause_for(secs(30));
            beat(ws, base + secs(3), false);
            assert!(pane_of(ws, 0).auto.is_waiting());
            assert_eq!(pane_of(ws, 1).state, PaneState::Connecting);

            ws.toggle_auto_reconnect(cx);
            assert!(!ws.settings().auto_reconnect);
            assert!(!pane_of(ws, 0).auto.is_active(), "关设置立刻取消等待的");
            assert!(!pane_of(ws, 1).auto.is_active(), "关设置立刻取消正在连的");
            assert_eq!(pane_of(ws, 1).state, PaneState::Disconnected);

            // 关着的时候再断开不会开始新的一轮。
            let (again, tx_c) = live_pane(9, true);
            push(ws, "again", again);
            end_session(&tx_c, None, None);
            beat(ws, base + secs(10), false);
            assert!(!pane_of(ws, 2).auto.is_active());
            beat(ws, base + secs(100), false);
            assert_eq!(pane_of(ws, 2).state, PaneState::Disconnected);
        })
        .expect("window");
}

#[gpui::test]
fn closing_a_tab_while_waiting_leaves_nothing_behind(cx: &mut TestAppContext) {
    let window = open(cx);
    window
        .update(cx, |ws, _, cx| {
            let base = Instant::now();
            let (pane, tx) = live_pane(7, true);
            let tab = push(ws, "web-1", pane);
            end_session(&tx, None, None);
            beat(ws, base, false);
            ws.dial_script
                .push_back(DialStep::Fail("连接失败：x".to_owned()));
            ws.close_tab(0, cx);
            assert!(ws.tab_ids().iter().all(|id| *id != tab));
            beat(ws, base + secs(3), false);
            beat(ws, base + secs(600), false);
            assert_eq!(ws.dial_script.len(), 1, "标签关了，到点也没有人去拨号");
        })
        .expect("window");
}

#[gpui::test]
fn the_lock_screen_pauses_the_countdown_and_resumes_it_after_unlock(cx: &mut TestAppContext) {
    let window = open(cx);
    window
        .update(cx, |ws, _, _| {
            let base = Instant::now();
            let (pane, tx) = live_pane(7, true);
            push(ws, "web-1", pane);
            end_session(&tx, None, None);
            beat(ws, base, false);
            ws.dial_script
                .push_back(DialStep::Fail("连接失败：x".to_owned()));
            // 锁着 10 秒：每秒一拍，计时暂停、不拨号。
            for tick in 1..=10u64 {
                beat(ws, base + secs(tick), true);
            }
            assert_eq!(ws.dial_script.len(), 1, "锁着不重连");
            assert_eq!(pane_of(ws, 0).state, PaneState::Disconnected);
            // 解锁：倒计时从还剩的 3 秒接着数，而不是已经过期。
            beat(ws, base + secs(10), false);
            assert_eq!(
                view(ws, 0, base + secs(10)),
                Some(AutoView::Waiting {
                    secs: 3,
                    attempt: 1
                })
            );
            beat(ws, base + secs(12), false);
            assert_eq!(ws.dial_script.len(), 1);
            beat(ws, base + secs(13), false);
            assert!(ws.dial_script.is_empty(), "解锁后到点才重连");
        })
        .expect("window");
}

#[gpui::test]
fn a_failure_that_needs_a_person_stops_this_host_and_every_pane_waiting_on_it(
    cx: &mut TestAppContext,
) {
    let window = open(cx);
    window
        .update(cx, |ws, _, _| {
            let base = Instant::now();
            // 三个窗格：两个连同一台主机（7），一个连另一台（8）。
            let (a, tx_a) = live_pane(7, true);
            let (b, tx_b) = live_pane(7, true);
            let (other, tx_other) = live_pane(8, true);
            push(ws, "a", a);
            push(ws, "b", b);
            push(ws, "other", other);
            for tx in [&tx_a, &tx_b, &tx_other] {
                end_session(tx, None, None);
            }
            beat(ws, base, false);
            // 只让 a 先到点（b 往后推，同一台主机上它还在等）。
            pane_mut(ws, 1).auto.pause_for(secs(30));
            ws.dial_script
                .push_back(DialStep::Fail(crate::ssh_backend::mark_needs_human(
                    "连接失败：认证失败",
                )));
            ws.dial_script
                .push_back(DialStep::Fail("连接失败：timed out".to_owned()));
            beat(ws, base + secs(3), false);
            // a 的失败要人处理 -> 下一拍收到：a 停止，同主机还在等的 b 一并停，别的主机照旧。
            beat(ws, base + secs(3), false);
            assert_eq!(view(ws, 0, base + secs(3)), Some(AutoView::Stopped));
            assert_eq!(view(ws, 1, base + secs(3)), Some(AutoView::Stopped));
            assert!(
                !screen(pane_of(ws, 0)).contains('\u{1}'),
                "记号不能漏到终端"
            );
            assert!(screen(pane_of(ws, 0)).contains("认证失败"));
            assert!(pane_of(ws, 2).auto.is_active(), "另一台主机的窗格不受影响");
            beat(ws, base + secs(600), false);
            assert_eq!(
                view(ws, 0, base + secs(600)),
                Some(AutoView::Stopped),
                "不再试"
            );
        })
        .expect("window");
}

#[gpui::test]
fn an_attempt_that_popped_a_prompt_gets_no_second_try_even_if_the_error_looks_transient(
    cx: &mut TestAppContext,
) {
    let window = open(cx);
    window
        .update(cx, |ws, _, _| {
            let base = Instant::now();
            let (pane, tx) = live_pane(7, true);
            push(ws, "web-1", pane);
            end_session(&tx, None, None);
            beat(ws, base, false);
            ws.dial_script
                .push_back(DialStep::Fail("连接失败：timed out".to_owned()));
            beat(ws, base + secs(3), false);
            // 这次尝试里弹过密码提问（用户答完以后又因为网络没连上）。
            let generation = pane_of(ws, 0).generation;
            crate::ssh_interact::note_prompted_for_test(generation);
            beat(ws, base + secs(3), false);
            assert!(
                pane_of(ws, 0).auto_failure.is_none() && !pane_of(ws, 0).auto.is_active(),
                "弹过提问的尝试失败后不再试：一次断线最多弹一次"
            );
            assert_eq!(view(ws, 0, base + secs(3)), Some(AutoView::Stopped));
        })
        .expect("window");
}

#[gpui::test]
fn the_overlay_draws_in_every_auto_state_without_a_component_library_root(cx: &mut TestAppContext) {
    let window = open(cx);
    window
        .update(cx, |ws, _, _| {
            let (pane, tx) = live_pane(7, true);
            push(ws, "web-1", pane);
            end_session(&tx, None, Some("读远端失败：reset"));
            beat(ws, Instant::now(), false);
        })
        .expect("window");
    cx.update_window(window.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
    })
    .expect("画出倒计时遮罩");
}

#[test]
fn the_overlay_line_wording() {
    assert_eq!(
        auto_overlay_line(AutoView::Waiting {
            secs: 12,
            attempt: 3
        }),
        "12 秒后自动重连（第 3 次）"
    );
    assert_eq!(
        auto_overlay_line(AutoView::Connecting { attempt: 2 }),
        "正在重连…"
    );
    assert_eq!(auto_overlay_line(AutoView::Stopped), "已停止自动重连");
}

#[test]
fn a_remote_exit_event_carries_no_auto_flag_for_an_offline_pane() {
    // 离线演示窗格（没有主机）收到结束事件不算会话断开，更不会自动重连。
    let (tx, rx) = mpsc::channel();
    let pump = SessionPump::new(SIZE, 100, Arc::new(FlowGate::new()), CoreFilter::new(), 1);
    let mut pane = Pane::with_parts(
        pump,
        Box::new(Recorder(Arc::new(Mutex::new(Vec::new())))),
        rx,
        None,
        None,
    );
    tx.send(SessionEvent::Exit {
        generation: 1,
        code: None,
    })
    .expect("send");
    pane.poll_events();
    assert!(!pane.auto_trigger);
}
