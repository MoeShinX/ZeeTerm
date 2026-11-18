//! 终端里标出 AI 执行的命令（V2-U50）在窗格 / 工作区这一层的行为：标记按顺序出现在输出前后、
//! 设置开关和语言生效、不改变命令建议的输入跟踪、连续多条 / 超时后再执行都成对。
//! 标记行本身（样式、耗时格式、配对规则）在核心库的 `ai::display` 测试里。

// 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`（导致展开时无限递归）。
use super::ai_marks::labels_for;
use super::assist_tests::{Remote, at_prompt};
use super::{Pane, PaneState, TerminalWorkspace};
use crate::i18n::Lang;
use crate::settings_store::AppSettings;
use gpui::{
    AppContext as _, Entity, IntoElement, ParentElement, Render, Styled, TestAppContext,
    WindowHandle, div, px, size,
};
use std::sync::Arc;
use zssh_core::ai::pty::{build_clear_prefix, build_wrapped_command};
use zssh_core::terminal_assist::Phase;

const MARKER: &str = "__ZSSH_MARKS0000000000__";

fn screen(pane: &Pane) -> Vec<String> {
    pane.snapshot
        .rows
        .iter()
        .map(|row| row.text.clone())
        .collect()
}

/// 一条 AI 命令在窗格里的完整经过：`arm`、shell 回显 + 开始标记、输出、结束标记、新提示符（带 Shell 集成标记）。
fn run_ai(
    pane: &mut Pane,
    remote: &Remote,
    agent: Option<&str>,
    command: &str,
    output: &str,
    code: i32,
) {
    let wrapped = build_wrapped_command(command, MARKER);
    let payload = format!("{}{}\r", build_clear_prefix(), wrapped);
    remote.arm_ai(MARKER, &payload, command, agent);
    remote.output(
        pane,
        format!(
            "\x07{}\r\n\x1b[?2004l\r\r\n{MARKER}_S\r\n{output}{MARKER}_E:{code}\r\n\x1b[?2004h\x1b]133;A\x07$ \x1b]133;B\x07",
            wrapped.trim_end()
        )
        .as_bytes(),
    );
}

fn marked(pane: &mut Pane) {
    pane.pump_mut()
        .set_ai_marks(Some(Arc::new(labels_for(Lang::Zh))));
}

#[test]
fn the_marks_frame_the_command_in_the_right_order_on_the_screen() {
    let (mut pane, remote) = at_prompt();
    marked(&mut pane);
    run_ai(&mut pane, &remote, Some("ZeePilot"), "echo hi", "hi\r\n", 0);
    let rows = screen(&pane);
    let at = |needle: &str| {
        rows.iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("屏幕上没有 {needle:?}：{rows:?}"))
    };
    let begin = at("▍AI（ZeePilot）");
    let echo = at("echo hi");
    let output = rows.iter().position(|row| row == "hi").expect("输出 hi");
    let end = at("✓ 退出码 0");
    let prompt = rows.iter().rposition(|row| row == "$").expect("新提示符");
    assert!(
        begin < echo && echo < output && output < end && end < prompt,
        "顺序：开头标记 < 回显 < 输出 < 结尾标记 < 新提示符：{rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("__ZSSH_")),
        "包装脚本和标记照旧不显示：{rows:?}"
    );
    // 样式复位：结尾标记之后的提示符不带颜色（逐格检查提示符那一行）。
    let prompt_row = &pane.snapshot.rows[prompt];
    assert!(
        prompt_row.cells.iter().all(|cell| cell.fg
            == zssh_terminal::TerminalColor::Named(zssh_terminal::NamedColor::Foreground)),
        "标记的颜色不能染到后面的提示符"
    );
}

#[test]
fn without_the_marks_the_screen_is_what_it_was() {
    // 没配文案（设置关着）：和只有合成回显的老样子一样，一个标记字符都没有。
    let (mut pane, remote) = at_prompt();
    run_ai(&mut pane, &remote, Some("ZeePilot"), "echo hi", "hi\r\n", 0);
    let rows = screen(&pane);
    assert!(
        rows.iter()
            .all(|row| !row.contains('▍') && !row.contains('✓')),
        "{rows:?}"
    );
    assert!(rows.iter().any(|row| row == "$ echo hi"), "{rows:?}");
}

#[test]
fn the_marks_do_not_touch_the_input_tracker_or_the_suggestion_history() {
    // 同一条 AI 命令：开标记的窗格和没开的窗格，输入跟踪的状态、命令建议的历史完全一样；
    // 标记也不会被当成用户敲的字写进历史。
    let (mut plain, plain_remote) = at_prompt();
    let (mut with_marks, marks_remote) = at_prompt();
    marked(&mut with_marks);
    for (pane, remote) in [
        (&mut plain, &plain_remote),
        (&mut with_marks, &marks_remote),
    ] {
        run_ai(pane, remote, Some("Codex"), "uptime", " up 1 day\r\n", 0);
    }
    for (what, pane) in [("没开", &plain), ("开着", &with_marks)] {
        assert_eq!(
            pane.assist.phase(),
            Phase::Input,
            "{what}：新提示符之后回到输入态"
        );
    }
    let (a, b) = (
        plain.assist.tracker().state(),
        with_marks.assist.tracker().state(),
    );
    assert_eq!(a.phase, b.phase);
    assert_eq!(a.confirmed_text, b.confirmed_text);
    assert_eq!(a.predicted_text, b.predicted_text);
    assert_eq!(a.submitted_command, b.submitted_command);
    assert_eq!(a.multiline, b.multiline);
    assert_eq!(a.tail_blank, b.tail_blank);
    let stats = |pane: &Pane| {
        let store = zssh_core::terminal_assist::history::lock(pane.assist.history());
        format!("{:?}", store.stats())
    };
    assert_eq!(stats(&plain), stats(&with_marks), "命令历史不变");
    // 输入行里也没有标记文字：下一次敲键盘的内容干干净净。
    assert_eq!(b.confirmed_text, "");
}

#[test]
fn several_commands_in_a_row_each_get_paired_marks_and_a_timeout_does_not_unbalance_the_next() {
    let (mut pane, remote) = at_prompt();
    marked(&mut pane);
    // 屏幕要装得下四条命令的全部标记（at_prompt 的窗格只有 6 行，早的会滚进回滚）。
    pane.resize(zssh_terminal::Size { rows: 40, cols: 60 });
    pane.refresh();
    run_ai(&mut pane, &remote, Some("ZeePilot"), "echo 1", "1\r\n", 0);
    run_ai(&mut pane, &remote, Some("ZeePilot"), "false", "", 1);
    // 第三条超时：AI 等不及（ai_release），命令还在跑；之后用户的终端里又有新命令。
    let wrapped = build_wrapped_command("sleep 30", "__ZSSH_LATE00000000000__");
    remote.arm_ai(
        "__ZSSH_LATE00000000000__",
        &format!("{}{}\r", build_clear_prefix(), wrapped),
        "sleep 30",
        Some("Claude Code"),
    );
    remote.output(
        &mut pane,
        format!(
            "\x07{}\r\n\x1b[?2004l\r\r\n__ZSSH_LATE00000000000___S\r\nworking",
            wrapped.trim_end()
        )
        .as_bytes(),
    );
    pane.pump_mut().ai_release(1, "__ZSSH_LATE00000000000__");
    pane.refresh();
    run_ai(&mut pane, &remote, None, "echo 2", "2\r\n", 0);
    let rows = screen(&pane);
    let count = |needle: &str| rows.iter().filter(|row| row.contains(needle)).count();
    assert_eq!(count("▍AI"), 4, "四条命令各一行开头标记：{rows:?}");
    assert_eq!(count("✓ 退出码 0"), 2, "{rows:?}");
    assert_eq!(count("✗ 退出码 1"), 1, "{rows:?}");
    assert_eq!(count("⧗"), 1, "{rows:?}");
    assert_eq!(count("▍AI（Claude Code）"), 1);
    assert_eq!(count("▍AI（ZeePilot）"), 2);
    // 没有智能体名的那条只写 AI。
    assert!(
        rows.iter().any(|row| row.trim_end().ends_with("▍AI")),
        "{rows:?}"
    );
    // 顺序：每个开头标记后面最近的是结尾 / 超时标记，不交错。
    let kinds: Vec<char> = rows
        .iter()
        .filter_map(|row| {
            if row.contains("▍AI") {
                Some('B')
            } else if row.contains("✓") || row.contains("✗") {
                Some('E')
            } else if row.contains("⧗") {
                Some('T')
            } else {
                None
            }
        })
        .collect();
    assert_eq!(kinds, ['B', 'E', 'B', 'E', 'B', 'T', 'B', 'E'], "{rows:?}");
}

#[test]
fn an_exit_code_that_cannot_be_read_and_a_ctrl_c_interrupt_end_cleanly() {
    let (mut pane, remote) = at_prompt();
    marked(&mut pane);
    // 用户在执行中按 Ctrl+C：`trap ':' INT` 让包装脚本照常打出结束标记，退出码 130。
    run_ai(
        &mut pane,
        &remote,
        Some("ZeePilot"),
        "sleep 100",
        "^C\r\n",
        130,
    );
    let rows = screen(&pane);
    assert!(
        rows.iter().any(|row| row.contains("✗ 退出码 130")),
        "{rows:?}"
    );
    assert_eq!(pane.state, PaneState::Live);
}

#[test]
fn the_english_labels_read_naturally() {
    let en = labels_for(Lang::En);
    assert_eq!(en.begin_named, "▍AI ({agent})");
    assert_eq!(en.success, "✓ exit {code} · {time}");
    assert_eq!(en.millis, "{n} ms");
    let (mut pane, remote) = at_prompt();
    pane.pump_mut().set_ai_marks(Some(Arc::new(en)));
    run_ai(&mut pane, &remote, Some("OpenCode"), "false", "", 2);
    let rows = screen(&pane);
    assert!(
        rows.iter().any(|row| row.contains("▍AI (OpenCode)")),
        "{rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.starts_with("✗ exit 2 · ") && row.ends_with(" ms")),
        "{rows:?}"
    );
}

// ---- 工作区：设置开关按轮询同步给每个窗格 ----

struct Host {
    workspace: Entity<TerminalWorkspace>,
}

impl Render for Host {
    fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div().size_full().child(self.workspace.clone())
    }
}

fn open(cx: &mut TestAppContext) -> (WindowHandle<Host>, Entity<TerminalWorkspace>) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        AppSettings::init_for_test(cx);
    });
    let window = cx.add_window(|window, cx| {
        let workspace = cx.new(|cx| {
            let mut workspace = TerminalWorkspace::new(window, cx, "ai-marks-test".into());
            workspace.push_test_tab("web-1");
            workspace
        });
        Host { workspace }
    });
    let workspace = window
        .read_with(cx, |host, _| host.workspace.clone())
        .expect("host");
    cx.simulate_window_resize(window.into(), size(px(900.0), px(500.0)));
    cx.update_window(window.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
    })
    .expect("draw");
    (window, workspace)
}

fn simulate(
    workspace: &Entity<TerminalWorkspace>,
    cx: &mut TestAppContext,
    command: &str,
    code: i32,
) {
    workspace.update(cx, |ws, _| {
        ws.poll_sessions();
        ws.dev_ai_begin(Some("ZeePilot"), command);
        ws.dev_ai_output("ok\r\n");
        ws.dev_ai_end(code);
    });
}

#[gpui::test]
fn the_setting_turns_the_marks_on_and_off_for_commands_started_afterwards(cx: &mut TestAppContext) {
    let (_window, workspace) = open(cx);
    // 默认开。
    assert!(workspace.read_with(cx, |ws, _| ws.settings.mark_ai_commands));
    simulate(&workspace, cx, "echo one", 0);
    let lines = workspace.read_with(cx, |ws, _| ws.dev_screen_lines());
    assert!(
        lines.iter().any(|l| l.contains("▍AI（ZeePilot）")),
        "{lines:?}"
    );
    // 关掉：下一条命令没有标记（已经显示的不动）。
    workspace.update(cx, |ws, cx| ws.toggle_mark_ai_commands(cx));
    assert!(!workspace.read_with(cx, |ws, _| ws.settings.mark_ai_commands));
    simulate(&workspace, cx, "echo two", 0);
    let lines = workspace.read_with(cx, |ws, _| ws.dev_screen_lines());
    assert_eq!(
        lines.iter().filter(|l| l.contains("▍AI")).count(),
        1,
        "关掉之后新命令没有开头标记：{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("echo two")),
        "合成回显照旧：{lines:?}"
    );
    // 再打开：又有了。
    workspace.update(cx, |ws, cx| ws.toggle_mark_ai_commands(cx));
    simulate(&workspace, cx, "echo three", 3);
    let lines = workspace.read_with(cx, |ws, _| ws.dev_screen_lines());
    assert_eq!(
        lines.iter().filter(|l| l.contains("▍AI")).count(),
        2,
        "{lines:?}"
    );
    assert!(lines.iter().any(|l| l.contains("✗ 退出码 3")), "{lines:?}");
}
