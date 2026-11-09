//! 命令建议在窗格这一层的行为：什么时候检索、结果怎么核对、什么条件下一个候选都不生成、
//! 选择模式和按键分流、接受只追加后缀。

// 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`（导致展开时无限递归）。
use super::assist_tests::{Remote, at_prompt, history_stats, run_line, shell_pane};
use super::{Pane, PaneState, WriteOrigin, WriteSource};
use std::sync::Arc;
use std::time::{Duration, Instant};
use zssh_core::terminal_assist::suggest::{CandidateSource, QuickSource};
use zssh_core::terminal_assist::{HandoffKey, KeyClass, Phase, RestrictionKind};

/// 测试里的时钟：每次观察都用"起点 + 偏移"，让 80 ms 的稳定时间不依赖真实等待。
struct Clock(Instant);

impl Clock {
    fn new() -> Self {
        Self(Instant::now())
    }

    fn tick(&self, pane: &mut Pane, ms: u64) -> bool {
        pane.observe_input_at(self.0 + Duration::from_millis(ms))
    }

    /// 让输入稳定下来：先看一眼开始计时，过了 80 ms 再看一次。
    fn settle(&self, pane: &mut Pane) -> bool {
        let first = self.tick(pane, 0);
        let second = self.tick(pane, 10_000);
        first || second
    }
}

fn type_text(pane: &mut Pane, text: &str) {
    for ch in text.chars() {
        pane.write_as(ch.to_string().as_bytes(), WriteOrigin::TypedText);
    }
}

/// 在提示符上，聚焦，历史里有几条命令。
fn prompt_with_history() -> (Pane, Remote) {
    let (mut pane, remote) = at_prompt();
    pane.assist.set_suggest_active(true);
    for line in [
        "git status",
        "git status",
        "git stash",
        "ls -la",
        "systemctl status nginx",
    ] {
        run_line(&mut pane, &remote, line);
    }
    assert_eq!(pane.assist.phase(), Phase::Input);
    (pane, remote)
}

fn shown(pane: &Pane) -> Vec<String> {
    pane.assist
        .suggest_view()
        .map(|view| view.items.iter().map(|item| item.text.clone()).collect())
        .unwrap_or_default()
}

fn plain_keys_sent(remote: &Remote) -> Vec<u8> {
    remote.sent().concat()
}

#[test]
fn two_characters_stable_for_80_ms_show_history_candidates_and_preselect_nothing() {
    let (mut pane, _remote) = prompt_with_history();
    type_text(&mut pane, "gi");
    let clock = Clock::new();
    assert!(
        shown(&pane).is_empty(),
        "刚打完字，还没稳定 80 ms，不能立刻出现"
    );
    assert!(clock.settle(&mut pane), "稳定之后浮层出现了");
    assert_eq!(
        shown(&pane),
        vec!["git status".to_owned(), "git stash".to_owned()],
        "用得多的在前"
    );
    let view = pane.assist.suggest_view().expect("view");
    assert_eq!(view.selected, None, "不预选");
    assert!(!view.select_mode);
    assert_eq!(pane.assist.suggest_typed(), Some("gi"));
}

#[test]
fn one_character_leading_space_and_exact_matches_make_no_request() {
    let (mut pane, _remote) = prompt_with_history();
    let clock = Clock::new();
    type_text(&mut pane, "g");
    clock.settle(&mut pane);
    assert_eq!(pane.assist.searches_started(), 0);
    assert!(shown(&pane).is_empty());

    let (mut spaced, _remote) = prompt_with_history();
    type_text(&mut spaced, " gi");
    clock.settle(&mut spaced);
    assert_eq!(
        spaced.assist.searches_started(),
        0,
        "前导空格表示别记，也就不提示"
    );

    // 完全输入了一条历史命令：没有比它更长的候选，展示为空。
    let (mut exact, _remote) = prompt_with_history();
    type_text(&mut exact, "ls -la");
    clock.settle(&mut exact);
    assert!(shown(&exact).is_empty());
}

#[test]
fn quick_commands_are_merged_after_history_and_excluded_or_secret_ones_never_show() {
    let (mut pane, _remote) = prompt_with_history();
    pane.assist.set_quick_sources(&Arc::new(vec![
        QuickSource {
            text: "git remote -v".into(),
            excluded: false,
        },
        QuickSource {
            text: "git push --token=abc123".into(),
            excluded: false,
        },
        QuickSource {
            text: "git log --oneline".into(),
            excluded: true,
        },
        QuickSource {
            text: "git status".into(),
            excluded: false,
        },
    ]));
    type_text(&mut pane, "gi");
    Clock::new().settle(&mut pane);
    let view = pane.assist.suggest_view().expect("view");
    let texts: Vec<&str> = view.items.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(texts, vec!["git status", "git stash", "git remote -v"]);
    assert_eq!(
        view.items[0].source,
        CandidateSource::History,
        "重复的留历史"
    );
    assert_eq!(view.items[2].source, CandidateSource::QuickCommand);
}

#[test]
fn typing_more_voids_the_shown_list_at_once_and_a_new_one_comes_after_it_settles() {
    let (mut pane, _remote) = prompt_with_history();
    type_text(&mut pane, "gi");
    let clock = Clock::new();
    clock.settle(&mut pane);
    assert_eq!(shown(&pane).len(), 2);
    type_text(&mut pane, "t s");
    assert!(
        shown(&pane).is_empty(),
        "输入变了，旧列表立刻失效，不等下一次 tick"
    );
    clock.settle(&mut pane);
    assert_eq!(
        shown(&pane),
        vec!["git status".to_owned(), "git stash".to_owned()]
    );
    type_text(&mut pane, "tas");
    clock.settle(&mut pane);
    assert_eq!(shown(&pane), vec!["git stash".to_owned()]);
}

#[test]
fn a_request_in_flight_dies_when_the_input_revision_changes() {
    let (mut pane, _remote) = prompt_with_history();
    pane.assist.defer_search_jobs();
    type_text(&mut pane, "gi");
    let clock = Clock::new();
    clock.settle(&mut pane);
    assert!(pane.assist.search_in_flight());
    assert_eq!(pane.assist.searches_started(), 1);

    // 请求还没回来，用户又打了一个字：旧请求作废（稳定之后为新输入另发一个）。
    type_text(&mut pane, "t");
    clock.tick(&mut pane, 20_000);
    assert_eq!(
        pane.assist.searches_started(),
        2,
        "旧的作废，新输入另发一个"
    );
    // 两个任务现在才跑完：只有新输入那个的结果能展示。
    assert_eq!(pane.assist.run_deferred_jobs(), 2);
    clock.tick(&mut pane, 30_000);
    assert_eq!(
        pane.assist.suggest_typed(),
        Some("git"),
        "旧输入 gi 的迟到结果不展示"
    );

    // 另一个顺序：结果已经到了通道里、但在核对之前输入变了。
    let (mut late, _remote) = prompt_with_history();
    late.assist.defer_search_jobs();
    type_text(&mut late, "gi");
    clock.settle(&mut late);
    assert_eq!(late.assist.run_deferred_jobs(), 1);
    type_text(&mut late, "t");
    clock.tick(&mut late, 40_000);
    assert!(
        shown(&late).is_empty(),
        "已经躺在通道里的旧结果，核对不过 is_current 就被丢弃"
    );
}

#[test]
fn a_result_is_checked_against_the_confirmed_text_before_it_is_shown() {
    let (mut pane, _remote) = prompt_with_history();
    pane.assist.defer_search_jobs();
    type_text(&mut pane, "gi");
    let clock = Clock::new();
    clock.settle(&mut pane);
    // 结果回来之前窗格换了目标：请求身份不再有效。
    pane.assist
        .sync_context(pane.id, pane.generation + 1, "host:7", Instant::now());
    pane.assist.run_deferred_jobs();
    clock.tick(&mut pane, 20_000);
    assert!(shown(&pane).is_empty());
}

/// 一次只改一个条件，看着它让"一个候选都不生成"：不只是禁止接受。
#[test]
fn no_candidate_is_generated_when_the_state_is_not_a_reliable_shell_line() {
    type Setup = (&'static str, fn(&mut Pane, &Remote));
    let cases: [Setup; 12] = [
        ("广播", |pane, _| {
            pane.set_assist_restrictions(false, true, false, true);
        }),
        ("粘贴确认等模态框", |pane, _| {
            pane.set_assist_restrictions(false, false, true, true);
        }),
        ("锁屏", |pane, _| {
            pane.assist.note_locked(Instant::now());
        }),
        ("密码交互", |pane, _| {
            pane.assist.tracker_mut().set_restriction(
                RestrictionKind::PasswordPrompt,
                true,
                Instant::now(),
            );
        }),
        ("鼠标上报", |pane, remote| {
            remote.output(pane, b"\x1b[?1000h");
            pane.set_assist_restrictions(false, false, false, true);
        }),
        ("备用屏幕", |pane, remote| {
            remote.output(pane, b"\x1b[?1049h");
        }),
        ("滚回历史", |pane, remote| {
            for i in 0..30 {
                remote.output(pane, format!("line {i}\r\n").as_bytes());
            }
            pane.pump_mut().scroll(5);
        }),
        ("断线", |pane, _| pane.state = PaneState::Disconnected),
        ("输入法预编辑", |pane, _| {
            pane.assist.on_ime_preedit(true, Instant::now());
        }),
        ("自动建议开关关着", |pane, _| {
            pane.assist.set_enabled(false, Instant::now());
        }),
        ("不是聚焦窗格", |pane, _| {
            pane.assist.set_suggest_active(false)
        }),
        ("粘贴过内容后还没有新提示符", |pane, _| {
            pane.write_as(b"echo x", WriteOrigin::Foreign(WriteSource::Paste));
        }),
    ];
    for (name, setup) in cases {
        // 两种次序：条件先成立再打字，和先打好字、稳定之前条件才成立。
        for typed_first in [false, true] {
            // 输入法预编辑一旦有字符提交就结束了，所以只有"先打字"这一种次序有意义。
            if name == "输入法预编辑" && !typed_first {
                continue;
            }
            let (mut pane, remote) = prompt_with_history();
            if typed_first {
                type_text(&mut pane, "gi");
                setup(&mut pane, &remote);
            } else {
                setup(&mut pane, &remote);
                type_text(&mut pane, "gi");
            }
            let clock = Clock::new();
            clock.settle(&mut pane);
            clock.tick(&mut pane, 20_000);
            assert_eq!(
                pane.assist.searches_started(),
                0,
                "{name}（先打字={typed_first}）：不能发起检索"
            );
            assert!(
                shown(&pane).is_empty(),
                "{name}（先打字={typed_first}）：不能有候选"
            );
        }
    }

    // 没有输入行标记的会话：不猜。
    let (mut bare, remote) = shell_pane();
    bare.assist.set_suggest_active(true);
    remote.output(&mut bare, b"user@host:~$ ");
    type_text(&mut bare, "gi");
    let clock = Clock::new();
    clock.settle(&mut bare);
    assert_eq!(bare.assist.searches_started(), 0, "没有标记");
}

#[test]
fn a_restriction_arriving_cancels_the_request_in_flight_and_what_is_shown() {
    for restrict in [
        (|pane: &mut Pane| {
            pane.set_assist_restrictions(false, true, false, true);
        }) as fn(&mut Pane),
        |pane| pane.assist.note_locked(Instant::now()),
        |pane| pane.state = PaneState::Disconnected,
    ] {
        let (mut pane, _remote) = prompt_with_history();
        pane.assist.defer_search_jobs();
        type_text(&mut pane, "gi");
        let clock = Clock::new();
        clock.settle(&mut pane);
        assert!(pane.assist.search_in_flight());
        restrict(&mut pane);
        clock.tick(&mut pane, 20_000);
        assert!(!pane.assist.search_in_flight(), "在途检索被取消");
        pane.assist.run_deferred_jobs();
        clock.tick(&mut pane, 30_000);
        assert!(shown(&pane).is_empty());
    }
    // 已经展示出来的，限制一来也立刻没有。
    let (mut pane, _remote) = prompt_with_history();
    type_text(&mut pane, "gi");
    let clock = Clock::new();
    clock.settle(&mut pane);
    assert_eq!(shown(&pane).len(), 2);
    pane.assist.note_locked(Instant::now());
    assert!(shown(&pane).is_empty(), "锁屏的同一刻就没有，不等 tick");
}

// ---- 按键：不显式进入选择模式，Tab / Enter / 方向键原样交给 shell ----

#[test]
fn without_select_mode_tab_enter_and_arrows_pass_through_unchanged_and_never_accept() {
    let keys: [(&str, bool); 8] = [
        ("tab", true),
        ("enter", true),
        ("up", true),
        ("down", true),
        ("up", false),   // Alt+↑
        ("down", false), // Ctrl+Alt+↓
        ("tab", false),  // Shift+Tab
        ("enter", false),
    ];
    for (key, plain) in keys {
        let (mut pane, remote) = prompt_with_history();
        type_text(&mut pane, "gi");
        Clock::new().settle(&mut pane);
        assert_eq!(shown(&pane).len(), 2, "{key}");
        let before = remote.sent().len();
        assert!(
            !pane.suggest_key(key, plain, false, false),
            "{key}（plain={plain}）：建议这一层不拦"
        );
        assert_eq!(pane.assist.selected_index(), None, "{key}：不预选");
        assert_eq!(remote.sent().len(), before, "建议这一层什么也不发");
        // 随后键本身照原样发给 shell（窗格层的写入路径不改字节）。
        let (bytes, origin): (&[u8], WriteOrigin) = match key {
            "tab" => (b"\t", WriteOrigin::Key(KeyClass::Handoff(HandoffKey::Tab))),
            "enter" => (b"\r", WriteOrigin::Key(KeyClass::Enter)),
            "up" => (
                b"\x1b[A",
                WriteOrigin::Key(KeyClass::Handoff(HandoffKey::Navigation)),
            ),
            _ => (
                b"\x1b[B",
                WriteOrigin::Key(KeyClass::Handoff(HandoffKey::Navigation)),
            ),
        };
        pane.write_as(bytes, origin);
        assert_eq!(remote.sent().last().map(Vec::as_slice), Some(bytes));
        assert!(shown(&pane).is_empty(), "{key}：交给 shell 之后列表关闭");
        let sent = remote.sent()[before..].concat();
        assert!(
            !String::from_utf8_lossy(&sent).contains("t status"),
            "{key}：没有悄悄接受第一条"
        );
    }
}

#[test]
fn escape_closes_a_shown_list_but_still_goes_to_the_program() {
    let (mut pane, _remote) = prompt_with_history();
    assert!(
        !pane.suggest_key("escape", true, false, false),
        "没有列表：Esc 交给前台程序"
    );
    type_text(&mut pane, "gi");
    Clock::new().settle(&mut pane);
    assert_eq!(shown(&pane).len(), 2);
    assert!(
        !pane.suggest_key("escape", false, false, false),
        "带修饰键的 Esc 不归列表"
    );
    assert!(
        !pane.suggest_key("escape", true, false, false),
        "没进选择模式：列表不拥有 Esc，照样交给前台程序"
    );
    assert!(shown(&pane).is_empty(), "关闭");
    // 同一个输入不会自己再弹出来。
    Clock::new().settle(&mut pane);
    assert!(shown(&pane).is_empty());
    // 再打一个字才会重新出现。
    type_text(&mut pane, "t");
    Clock::new().settle(&mut pane);
    assert_eq!(shown(&pane).len(), 2);
}

/// 显式进入选择模式后，列表拿到了键盘：Esc 归它，收起列表并被消费，不发给 shell。
#[test]
fn escape_in_select_mode_belongs_to_the_list() {
    let (mut pane, remote) = prompt_with_history();
    type_text(&mut pane, "gi");
    Clock::new().settle(&mut pane);
    assert!(pane.assist.enter_select_mode());
    let before = remote.sent().len();
    assert!(
        pane.suggest_key("escape", true, false, false),
        "选择模式里消费"
    );
    assert!(shown(&pane).is_empty(), "收起列表");
    assert!(
        pane.assist
            .suggest_view()
            .is_none_or(|view| !view.select_mode),
        "退出选择模式"
    );
    assert_eq!(remote.sent().len(), before, "不发任何字节");
}

#[test]
fn explicit_select_mode_moves_only_the_selection_and_accept_appends_only_the_suffix() {
    let (mut pane, remote) = prompt_with_history();
    type_text(&mut pane, "gi");
    Clock::new().settle(&mut pane);
    assert!(pane.assist.enter_select_mode());
    let view = pane.assist.suggest_view().expect("view");
    assert!(view.select_mode);
    assert_eq!(view.selected, None, "进入选择模式仍不选中");

    let before = remote.sent().len();
    assert!(pane.suggest_key("down", true, false, false));
    assert_eq!(pane.assist.selected_index(), Some(0));
    assert!(pane.suggest_key("down", true, false, false));
    assert_eq!(pane.assist.selected_index(), Some(1));
    assert!(pane.suggest_key("down", true, false, false));
    assert_eq!(pane.assist.selected_index(), Some(0), "回绕");
    assert!(pane.suggest_key("up", true, false, false));
    assert_eq!(pane.assist.selected_index(), Some(1));
    assert!(pane.suggest_key("up", true, false, false));
    assert_eq!(pane.assist.selected_index(), Some(0));
    assert_eq!(remote.sent().len(), before, "上下只选候选，不发任何字节");

    // Tab 接受：只追加后缀，不发回车。
    assert!(pane.suggest_key("tab", true, false, false));
    assert_eq!(
        remote.sent().last().map(Vec::as_slice),
        Some(b"t status".as_slice()),
        "只有后缀"
    );
    assert!(
        !remote
            .sent()
            .iter()
            .skip(before)
            .any(|b| b.contains(&b'\r')),
        "接受不发回车"
    );
    assert!(shown(&pane).is_empty(), "接受后退出模式、关闭列表");
    assert!(!pane.assist.in_select_mode());
    assert_eq!(pane.assist.tracker().state().confirmed_text, "git status");
    assert_eq!(pane.assist.phase(), Phase::Input);
}

#[test]
fn enter_in_select_mode_accepts_without_executing_and_a_held_key_stays_consumed_until_release() {
    let (mut pane, remote) = prompt_with_history();
    type_text(&mut pane, "gi");
    Clock::new().settle(&mut pane);
    pane.assist.enter_select_mode();
    pane.suggest_key("down", true, false, false);
    let before = remote.sent().len();
    assert!(pane.suggest_key("enter", true, false, false));
    assert_eq!(remote.sent().len(), before + 1);
    // 按住 Enter 产生的重复事件：继续消费，绝不能变成 CR（那会执行刚填入的命令）。
    for _ in 0..5 {
        assert!(pane.suggest_key("enter", true, true, false));
    }
    assert_eq!(remote.sent().len(), before + 1, "重复事件一个字节也没发");
    assert!(!plain_keys_sent(&remote).ends_with(b"\r"));
    // 松开之后，另一次独立的按下按原终端规则处理。
    pane.assist.release_latch("enter");
    assert!(!pane.suggest_key("enter", true, false, false));

    // 松开事件丢了：新的一次按下（非重复）也会放行。
    let (mut lost, _remote) = prompt_with_history();
    type_text(&mut lost, "gi");
    Clock::new().settle(&mut lost);
    lost.assist.enter_select_mode();
    lost.suggest_key("down", true, false, false);
    assert!(lost.suggest_key("tab", true, false, false));
    assert!(lost.suggest_key("tab", true, true, false));
    assert!(!lost.suggest_key("tab", true, false, false), "新的按下不吃");

    // 失焦清 latch。
    let (mut blurred, _remote) = prompt_with_history();
    type_text(&mut blurred, "gi");
    Clock::new().settle(&mut blurred);
    blurred.assist.enter_select_mode();
    blurred.suggest_key("down", true, false, false);
    blurred.suggest_key("tab", true, false, false);
    blurred.assist.focus_lost();
    assert!(!blurred.suggest_key("tab", true, true, false));
}

/// 复查 R1：Enter 接受候选后一直按住，远端迟迟不回显（超过 5 秒失同步），重复事件也不能变成 CR。
#[test]
fn a_held_enter_after_accepting_sends_no_cr_even_when_the_echo_times_out() {
    let (mut pane, remote) = prompt_with_history();
    type_text(&mut pane, "gi");
    let clock = Clock::new();
    clock.settle(&mut pane);
    pane.assist.enter_select_mode();
    pane.suggest_key("down", true, false, false);
    remote.set_echo(false);
    assert!(pane.suggest_key("enter", true, false, false));
    let after_accept = remote.sent().len();
    clock.tick(&mut pane, 20_000);
    assert_eq!(
        pane.assist.phase(),
        Phase::Desynced,
        "后缀一直没回显，失同步"
    );
    for _ in 0..3 {
        assert!(
            pane.suggest_key("enter", true, true, false),
            "失同步时还按着：照样消费"
        );
    }
    // 屏幕安静后重新协调回来，也不改变"这颗键还按着"。
    clock.tick(&mut pane, 30_000);
    for _ in 0..3 {
        assert!(
            pane.suggest_key("enter", true, true, false),
            "重新协调后还按着：照样消费"
        );
    }
    assert_eq!(remote.sent().len(), after_accept, "一个 CR 都没发");
    assert!(!plain_keys_sent(&remote).ends_with(b"\r"));
}

#[test]
fn select_mode_without_a_selection_closes_and_hands_the_key_back_untouched() {
    for key in ["tab", "enter"] {
        let (mut pane, remote) = prompt_with_history();
        type_text(&mut pane, "gi");
        Clock::new().settle(&mut pane);
        pane.assist.enter_select_mode();
        let before = remote.sent().len();
        assert!(
            !pane.suggest_key(key, true, false, false),
            "{key}：没有选择，按原终端键处理"
        );
        assert!(!pane.assist.in_select_mode(), "{key}：模式已关闭");
        assert_eq!(
            remote.sent().len(),
            before,
            "{key}：建议这一层没偷偷选第一条"
        );
    }
}

#[test]
fn ordinary_input_or_modified_keys_leave_select_mode_and_go_to_the_normal_chain_once() {
    for (key, plain) in [
        ("x", true),
        ("backspace", true),
        ("left", true),
        ("up", false),
        ("c", false),
    ] {
        let (mut pane, _remote) = prompt_with_history();
        type_text(&mut pane, "gi");
        Clock::new().settle(&mut pane);
        pane.assist.enter_select_mode();
        pane.suggest_key("down", true, false, false);
        assert!(!pane.suggest_key(key, plain, false, false), "{key}");
        assert!(!pane.assist.in_select_mode(), "{key}：退出选择模式");
        assert_eq!(pane.assist.selected_index(), None);
    }
    // 输入法最终提交 / 任何别的写入同样让它退出，这次写入照常发送一次。
    let (mut pane, remote) = prompt_with_history();
    type_text(&mut pane, "gi");
    Clock::new().settle(&mut pane);
    pane.assist.enter_select_mode();
    let before = remote.sent().len();
    pane.write_as("你".as_bytes(), WriteOrigin::ImeText);
    assert!(!pane.assist.in_select_mode());
    assert_eq!(remote.sent().len(), before + 1, "不丢字、不重复");
    // 预编辑开始同样退出并收起列表。
    let (mut composing, _remote) = prompt_with_history();
    type_text(&mut composing, "gi");
    Clock::new().settle(&mut composing);
    composing.assist.enter_select_mode();
    composing.assist.on_ime_preedit(true, Instant::now());
    assert!(!composing.assist.in_select_mode());
    assert!(shown(&composing).is_empty());
}

#[test]
fn select_mode_needs_a_valid_list_and_a_stale_selection_is_never_accepted() {
    let (mut pane, remote) = prompt_with_history();
    assert!(!pane.assist.enter_select_mode(), "没有候选，进不去");
    type_text(&mut pane, "gi");
    Clock::new().settle(&mut pane);
    pane.assist.enter_select_mode();
    pane.suggest_key("down", true, false, false);
    // 选中之后输入变了：列表作废，模式随之退出，Tab 是普通 Tab。
    type_text(&mut pane, "t");
    let before = remote.sent().len();
    assert!(!pane.suggest_key("tab", true, false, false));
    assert_eq!(remote.sent().len(), before);
    assert!(pane.accept_candidate(0).is_err(), "旧列表没有东西可接受");
    assert_eq!(remote.sent().len(), before);
}

#[test]
fn the_bound_key_enters_select_mode_only_when_it_applies_and_is_a_toggle() {
    // 没有候选：绑定键不拦，原样交给终端。
    let (mut pane, _remote) = prompt_with_history();
    assert!(!pane.suggest_key("s", false, false, true));
    type_text(&mut pane, "gi");
    Clock::new().settle(&mut pane);
    assert!(
        pane.suggest_key("s", false, false, true),
        "有候选：拦下并进入选择模式"
    );
    assert!(pane.assist.in_select_mode());
    // 按住的重复事件不会退出模式。
    assert!(pane.suggest_key("s", false, true, true));
    assert!(pane.assist.in_select_mode());
    // 松开后再按一次 = 退出。
    pane.assist.release_latch("s");
    assert!(pane.suggest_key("s", false, false, true));
    assert!(!pane.assist.in_select_mode());
}

#[test]
fn accepting_a_candidate_by_index_follows_the_pane_write_path_and_closes_the_list() {
    let (mut pane, remote) = prompt_with_history();
    type_text(&mut pane, "gi");
    Clock::new().settle(&mut pane);
    let before = remote.sent().len();
    let outcome = pane.accept_candidate(1);
    assert_eq!(
        outcome,
        Ok(zssh_core::terminal_assist::WriteOutcome::Queued)
    );
    assert_eq!(
        remote.sent().last().map(Vec::as_slice),
        Some(b"t stash".as_slice())
    );
    assert_eq!(remote.sent().len(), before + 1);
    assert!(shown(&pane).is_empty());
    assert_eq!(pane.assist.tracker().state().confirmed_text, "git stash");
    // 再接受一次：没有列表了，什么也不发。
    assert!(pane.accept_candidate(0).is_err());
    assert_eq!(remote.sent().len(), before + 1);
    // 命令没有被执行，也没有进历史。
    assert_eq!(history_stats(&pane).entries, 4);
}

#[test]
fn candidates_are_never_written_into_the_terminal_cells() {
    let (mut pane, _remote) = prompt_with_history();
    type_text(&mut pane, "gi");
    let before = pane.snapshot.clone();
    Clock::new().settle(&mut pane);
    assert_eq!(shown(&pane).len(), 2);
    pane.refresh();
    assert_eq!(
        pane.snapshot.rows, before.rows,
        "候选只画在浮层里，不写进终端格子"
    );
}
