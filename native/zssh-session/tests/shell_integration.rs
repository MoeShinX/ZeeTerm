//! 临时 Bash / Zsh 集成脚本的真实 shell 测试：把脚本喂给真的 shell，用终端模型的 OSC 扫描器读它的输出，
//! 验证注入后产生 133 A/B/C/D 和 OSC 7，卸载后 PROMPT_COMMAND / PS1 恢复原样，安装失败自己清理。
//!
//! 本机没有对应的 shell（或者版本太老不支持某个特性）就跳过那部分并打印原因，不算失败。
//! 这些测试把 shell 当子进程跑，只在测试里，不进产品代码。

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io::Write as _;
use std::process::{Command, Stdio};

use zssh_core::terminal_assist::integration::{ShellKind, inject_line, script};
use zssh_terminal::{OscScanner, ShellMarkKind};

/// 起一个交互式 shell（stdin 是管道），喂入 `input`，返回它的 stdout+stderr（合并，保持顺序）。
fn run_shell(shell: &str, args: &str, input: &str) -> Option<Vec<u8>> {
    let mut child = Command::new("bash")
        .arg("-c")
        .arg(format!("exec {shell} {args} 2>&1"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(input.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    Some(out.stdout)
}

fn bash_version() -> Option<(u32, u32)> {
    let out = Command::new("bash")
        .args([
            "--noprofile",
            "--norc",
            "-c",
            "echo \"${BASH_VERSINFO[0]} ${BASH_VERSINFO[1]}\"",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut parts = text.split_whitespace();
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

fn zsh_available() -> bool {
    Command::new("zsh")
        .args(["-f", "-c", "print $ZSH_VERSION"])
        .output()
        .map(|out| out.status.success() && !out.stdout.is_empty())
        .unwrap_or(false)
}

fn marks(bytes: &[u8]) -> Vec<ShellMarkKind> {
    let mut scanner = OscScanner::new();
    let mut found = Vec::new();
    scanner.scan(bytes, &mut found);
    found.into_iter().map(|(_, mark)| mark.kind).collect()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// 去掉文本里的 OSC 序列（`ESC ] … BEL`），探针行前面可能紧贴着 133;C 之类的标记。
fn strip_osc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&']') {
            for next in chars.by_ref() {
                if next == '\x07' {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// 输出里 `NAME=...` 形式的行（我们自己 echo 的探针）。
fn probe(output: &str, name: &str) -> Vec<String> {
    strip_osc(output)
        .lines()
        .filter_map(|line| {
            line.trim_end_matches('\r')
                .strip_prefix(&format!("{name}="))
                .map(str::to_owned)
        })
        .collect()
}

macro_rules! need_bash {
    () => {
        match bash_version() {
            Some(version) => version,
            None => {
                println!("跳过：这台机器上没有可用的 bash");
                return;
            }
        }
    };
}

const BASH_ARGS: &str = "--noprofile --norc -i";

fn bash_session(before: &str, injection: &str, after_on: &str, after_off: &str) -> String {
    format!(
        "{before}\n\
         echo BEFORE_PC=$(declare -p PROMPT_COMMAND 2>&1)\n\
         echo BEFORE_PS1=[$PS1]\n\
         {injection}\n\
         {after_on}\n\
         echo ---OFF---\n\
         __zeeterm_off\n\
         echo AFTER_PC=$(declare -p PROMPT_COMMAND 2>&1)\n\
         echo AFTER_PS1=[$PS1]\n\
         {after_off}\n\
         echo ---END---\n\
         exit\n"
    )
}

fn split_off(output: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let marker = b"---OFF---";
    let at = output
        .windows(marker.len())
        .position(|w| w == marker)
        .expect("输出里应有 ---OFF--- 标记");
    (output[..at].to_vec(), output[at..].to_vec())
}

/// 通用断言：注入之后能产生 A / B / C / D，卸载之后不再产生。
fn assert_marks_before_off_and_none_after(output: &[u8], version: (u32, u32)) {
    let (on, off) = split_off(output);
    let on_marks = marks(&on);
    let has = |pred: fn(&ShellMarkKind) -> bool| on_marks.iter().any(pred);
    assert!(
        has(|k| matches!(k, ShellMarkKind::PromptStart)),
        "应有 133;A：{on_marks:?}\n{}",
        text(&on)
    );
    assert!(
        has(|k| matches!(k, ShellMarkKind::PromptEnd)),
        "应有 133;B：{on_marks:?}"
    );
    assert!(
        has(|k| matches!(k, ShellMarkKind::CommandEnd { exit_code: Some(0) })),
        "应有 133;D;0：{on_marks:?}"
    );
    assert!(
        has(|k| matches!(k, ShellMarkKind::CommandEnd { exit_code: Some(3) })),
        "(exit 3) 之后应有 133;D;3：{on_marks:?}"
    );
    if version >= (4, 4) {
        assert!(
            has(|k| matches!(k, ShellMarkKind::CommandStart)),
            "bash >= 4.4 应有 133;C：{on_marks:?}"
        );
    } else {
        println!("跳过 133;C 断言：bash {}.{} 没有 PS0", version.0, version.1);
    }
    assert!(
        has(|k| matches!(k, ShellMarkKind::Cwd { .. })),
        "应有 OSC 7：{on_marks:?}"
    );
    // 卸载之后（含 `__zeeterm_off` 自己所在那一行之后的提示符）不再有任何标记。
    // ---OFF--- 之后紧跟的下一个提示符还是带着（它在卸载命令之前打印），所以只看卸载命令之后。
    let off_text = text(&off);
    let after_command = off_text
        .split_once("AFTER_PC=")
        .map(|(_, rest)| rest.to_owned())
        .expect("应有 AFTER_PC 探针");
    let later = marks(after_command.as_bytes());
    assert!(
        later.is_empty(),
        "卸载之后不应再有标记：{later:?}\n{after_command}"
    );
}

#[test]
fn bash_with_no_prompt_command_gets_marks_and_goes_back_to_unset() {
    let version = need_bash!();
    let session = bash_session(
        "unset PROMPT_COMMAND",
        &inject_line(ShellKind::Bash),
        "echo hello\n(exit 3)\ntrue",
        "echo plain\ntrue",
    );
    let output = run_shell("bash", BASH_ARGS, &session).expect("起不来 bash");
    let shown = text(&output);
    assert_marks_before_off_and_none_after(&output, version);
    let before = probe(&shown, "BEFORE_PC");
    let after = probe(&shown, "AFTER_PC");
    assert_eq!(before, after, "PROMPT_COMMAND 恢复原值（未设置）\n{shown}");
    assert_eq!(probe(&shown, "BEFORE_PS1"), probe(&shown, "AFTER_PS1"));
}

#[test]
fn bash_keeps_a_string_prompt_command_and_a_hook_the_user_added_later() {
    let version = need_bash!();
    let session = bash_session(
        "PROMPT_COMMAND='echo ORIG-HOOK'",
        &inject_line(ShellKind::Bash),
        "echo hello\n(exit 3)\nPROMPT_COMMAND=\"$PROMPT_COMMAND; echo USER-ADDED\"\ntrue",
        "true",
    );
    let output = run_shell("bash", BASH_ARGS, &session).expect("起不来 bash");
    let shown = text(&output);
    assert_marks_before_off_and_none_after(&output, version);
    // 原来的 hook 在注入后照常运行。
    let (on, _) = split_off(&output);
    assert!(
        text(&on).contains("ORIG-HOOK"),
        "原 hook 照跑：{}",
        text(&on)
    );
    // 卸载只拿掉我们那一项，用户后加的留着。
    let after = probe(&shown, "AFTER_PC").join("\n");
    assert!(after.contains("echo ORIG-HOOK"), "{after}");
    assert!(
        after.contains("echo USER-ADDED"),
        "不覆盖用户期间新加的 hook：{after}"
    );
    assert!(!after.contains("__zeeterm_pc"), "只移除本次注册的：{after}");
}

#[test]
fn bash_string_prompt_command_is_restored_exactly_when_nothing_else_changed() {
    let _version = need_bash!();
    let session = bash_session(
        "PROMPT_COMMAND='echo ORIG-HOOK; true'",
        &inject_line(ShellKind::Bash),
        "echo hello",
        "true",
    );
    let output = run_shell("bash", BASH_ARGS, &session).expect("起不来 bash");
    let shown = text(&output);
    assert_eq!(
        probe(&shown, "BEFORE_PC"),
        probe(&shown, "AFTER_PC"),
        "{shown}"
    );
    assert_eq!(probe(&shown, "BEFORE_PS1"), probe(&shown, "AFTER_PS1"));
}

#[test]
fn bash_keeps_an_array_prompt_command() {
    let version = need_bash!();
    if version < (5, 1) {
        println!(
            "跳过：bash {}.{} 的 PROMPT_COMMAND 不是数组",
            version.0, version.1
        );
        return;
    }
    let session = bash_session(
        "PROMPT_COMMAND=('echo HOOK-ONE' 'echo HOOK-TWO')",
        &inject_line(ShellKind::Bash),
        "echo hello\n(exit 3)\nPROMPT_COMMAND+=('echo USER-ADDED')\ntrue",
        "true",
    );
    let output = run_shell("bash", BASH_ARGS, &session).expect("起不来 bash");
    let shown = text(&output);
    assert_marks_before_off_and_none_after(&output, version);
    let (on, _) = split_off(&output);
    let on_text = text(&on);
    assert!(
        on_text.contains("HOOK-ONE") && on_text.contains("HOOK-TWO"),
        "数组里的 hook 都照常运行：{on_text}"
    );
    let after = probe(&shown, "AFTER_PC").join("\n");
    assert!(
        after.contains("HOOK-ONE") && after.contains("HOOK-TWO"),
        "{after}"
    );
    assert!(after.contains("USER-ADDED"), "不覆盖用户新加的：{after}");
    assert!(!after.contains("__zeeterm_pc"), "{after}");
}

#[test]
fn the_multi_line_copy_version_works_the_same() {
    let version = need_bash!();
    let session = bash_session(
        "PROMPT_COMMAND='echo ORIG-HOOK'",
        &script(ShellKind::Bash),
        "echo hello\n(exit 3)\ntrue",
        "true",
    );
    let output = run_shell("bash", BASH_ARGS, &session).expect("起不来 bash");
    assert_marks_before_off_and_none_after(&output, version);
    let shown = text(&output);
    assert_eq!(probe(&shown, "BEFORE_PC"), probe(&shown, "AFTER_PC"));
}

#[test]
fn enabling_twice_does_not_stack_and_one_off_undoes_it() {
    let _version = need_bash!();
    let line = inject_line(ShellKind::Bash);
    let session = bash_session(
        "PROMPT_COMMAND='echo ORIG-HOOK'",
        &format!("{line}\n{line}"),
        "echo hello",
        "true",
    );
    let output = run_shell("bash", BASH_ARGS, &session).expect("起不来 bash");
    let shown = text(&output);
    assert_eq!(
        probe(&shown, "BEFORE_PC"),
        probe(&shown, "AFTER_PC"),
        "{shown}"
    );
}

#[test]
fn a_failed_install_cleans_up_after_itself() {
    let _version = need_bash!();
    // PROMPT_COMMAND 只读：安装第一步就赋值失败，必须自动卸载，不留函数、不改 PS1、不出标记。
    let session = "readonly PROMPT_COMMAND\n\
         echo BEFORE_PS1=[$PS1]\n"
        .to_owned()
        + &inject_line(ShellKind::Bash)
        + "\necho true_run\n\
         type __zeeterm_off >/dev/null 2>&1 && echo LEFTOVER=function || echo LEFTOVER=none\n\
         echo AFTER_PS1=[$PS1]\n\
         echo ---END---\n\
         exit\n";
    let output = run_shell("bash", BASH_ARGS, &session).expect("起不来 bash");
    let shown = text(&output);
    assert_eq!(
        probe(&shown, "LEFTOVER"),
        vec!["none".to_owned()],
        "{shown}"
    );
    assert_eq!(probe(&shown, "BEFORE_PS1"), probe(&shown, "AFTER_PS1"));
    assert!(
        marks(&output).is_empty(),
        "失败的安装不能留下标记：{:?}",
        marks(&output)
    );
}

#[test]
fn the_cwd_report_is_percent_encoded_and_round_trips() {
    let _version = need_bash!();
    let dir = std::env::temp_dir().join(format!("zt-cwd-{}-{}", std::process::id(), "a b 中文%"));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    let session = format!(
        "{}\ncd '{}'\necho PWD_NOW=$PWD\ntrue\nexit\n",
        inject_line(ShellKind::Bash),
        dir.to_string_lossy().replace('\\', "/")
    );
    let output = run_shell("bash", BASH_ARGS, &session).expect("起不来 bash");
    let _ = std::fs::remove_dir_all(&dir);
    let shown = text(&output);
    let pwd = probe(&shown, "PWD_NOW").pop().expect("PWD_NOW 探针");
    let reported = marks(&output)
        .into_iter()
        .filter_map(|kind| match kind {
            ShellMarkKind::Cwd { path, .. } => Some(path),
            _ => None,
        })
        .next_back()
        .expect("应有 OSC 7");
    assert_eq!(reported, pwd, "百分号编码后解码要还原 $PWD\n{shown}");
    assert!(reported.contains("a b 中文%"), "{reported}");
}

#[test]
fn bash_prompt_marks_arrive_in_a_sane_order() {
    let version = need_bash!();
    let session = bash_session(
        "unset PROMPT_COMMAND",
        &inject_line(ShellKind::Bash),
        "echo hello\ntrue",
        "true",
    );
    let output = run_shell("bash", BASH_ARGS, &session).expect("起不来 bash");
    let (on, _) = split_off(&output);
    let kinds = marks(&on);
    // 每个提示符都是 A 然后 B；有 C 的话它在 B 之后、D 之前。
    let first_a = kinds
        .iter()
        .position(|k| matches!(k, ShellMarkKind::PromptStart))
        .expect("A");
    let first_b = kinds
        .iter()
        .position(|k| matches!(k, ShellMarkKind::PromptEnd))
        .expect("B");
    assert!(first_a < first_b, "{kinds:?}");
    if version >= (4, 4) {
        let first_c = kinds
            .iter()
            .position(|k| matches!(k, ShellMarkKind::CommandStart))
            .expect("C");
        let first_d = kinds
            .iter()
            .position(|k| matches!(k, ShellMarkKind::CommandEnd { .. }))
            .expect("D");
        assert!(first_b < first_c && first_c < first_d, "{kinds:?}");
    }
}

// ---- Zsh：本机没有就跳过（CI 的 macOS 上有）----

fn zsh_session(before: &str, injection: &str) -> String {
    format!(
        "{before}\n\
         echo BEFORE_PRE=${{(j: :)precmd_functions}}\n\
         echo BEFORE_EXE=${{(j: :)preexec_functions}}\n\
         echo BEFORE_PS1=[$PS1]\n\
         {injection}\n\
         echo hello\n\
         (exit 3)\n\
         true\n\
         echo ---OFF---\n\
         __zeeterm_off\n\
         echo AFTER_PRE=${{(j: :)precmd_functions}}\n\
         echo AFTER_EXE=${{(j: :)preexec_functions}}\n\
         echo AFTER_PS1=[$PS1]\n\
         echo plain\n\
         true\n\
         echo ---END---\n\
         exit\n"
    )
}

#[test]
fn zsh_chains_with_existing_hooks_and_unloads_cleanly() {
    if !zsh_available() {
        println!("跳过：这台机器上没有 zsh");
        return;
    }
    // 先确认这个测试台里 zsh 会在每个提示符前运行 precmd（管道输入、没有 ZLE 的情况下各版本不一定一样）。
    let baseline =
        run_shell("zsh", "-f -i", "precmd() { echo PRECMD-RAN }\ntrue\nexit\n").unwrap_or_default();
    if !text(&baseline).contains("PRECMD-RAN") {
        println!("跳过：这个 zsh 在管道输入下不运行 precmd，没法在这里验证 hook");
        return;
    }
    let session = zsh_session(
        "precmd() { echo ORIG-PRECMD }\npreexec() { echo ORIG-PREEXEC }",
        &inject_line(ShellKind::Zsh),
    );
    let output = run_shell("zsh", "-f -i", &session).expect("起不来 zsh");
    let shown = text(&output);
    let (on, off) = split_off(&output);
    let on_marks = marks(&on);
    assert!(
        on_marks
            .iter()
            .any(|k| matches!(k, ShellMarkKind::PromptStart)),
        "应有 133;A：{on_marks:?}\n{shown}"
    );
    assert!(
        on_marks
            .iter()
            .any(|k| matches!(k, ShellMarkKind::PromptEnd)),
        "应有 133;B：{on_marks:?}"
    );
    assert!(
        on_marks
            .iter()
            .any(|k| matches!(k, ShellMarkKind::CommandStart)),
        "应有 133;C：{on_marks:?}"
    );
    assert!(
        on_marks
            .iter()
            .any(|k| matches!(k, ShellMarkKind::CommandEnd { exit_code: Some(3) })),
        "应有 133;D;3：{on_marks:?}"
    );
    // 已有的 precmd / preexec 函数照常运行，没被覆盖。
    assert!(text(&on).contains("ORIG-PRECMD"), "{shown}");
    assert!(text(&on).contains("ORIG-PREEXEC"), "{shown}");
    // 卸载：hook 数组和 PS1 恢复，之后不再有标记。
    assert_eq!(
        probe(&shown, "BEFORE_PRE"),
        probe(&shown, "AFTER_PRE"),
        "{shown}"
    );
    assert_eq!(
        probe(&shown, "BEFORE_EXE"),
        probe(&shown, "AFTER_EXE"),
        "{shown}"
    );
    assert_eq!(
        probe(&shown, "BEFORE_PS1"),
        probe(&shown, "AFTER_PS1"),
        "{shown}"
    );
    let after = text(&off)
        .split_once("AFTER_PS1=")
        .map(|(_, rest)| rest.to_owned())
        .expect("AFTER_PS1 探针");
    assert!(marks(after.as_bytes()).is_empty(), "卸载后不应再有标记");
}
