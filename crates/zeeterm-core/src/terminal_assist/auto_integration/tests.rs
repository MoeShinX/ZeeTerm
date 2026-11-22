use super::*;
use crate::terminal_assist::state::ScreenView;

const COLS: usize = 40;

/// 屏幕上的一整行：文字后面用空格补到整行宽，和真实屏幕一样。宽字符补一个占位格。
fn row(text: &str) -> Vec<InputCell> {
    let mut cells = Vec::new();
    for ch in text.chars() {
        let wide = ch as u32 > 0x2e80;
        cells.push(InputCell {
            ch,
            extra: None,
            kind: if wide {
                CellKind::Wide
            } else {
                CellKind::Normal
            },
        });
        if wide {
            cells.push(InputCell {
                ch: ' ',
                extra: None,
                kind: CellKind::WideSpacer,
            });
        }
    }
    while cells.len() < COLS {
        cells.push(InputCell::plain(' '));
    }
    cells
}

fn prompt_at(text: &str, cursor: usize) -> bool {
    looks_like_idle_prompt(&row(text), cursor)
}

#[test]
fn an_empty_prompt_with_the_cursor_right_after_its_space_is_idle() {
    assert!(prompt_at("$ ", 2));
    assert!(prompt_at("# ", 2));
    assert!(prompt_at("% ", 2));
    assert!(prompt_at("> ", 2));
    assert!(prompt_at("root@web-1:~# ", 14));
    assert!(prompt_at("user@web-1 ~ % ", 15));
    assert!(prompt_at("[user@web-1 ~]$ ", 16));
    assert!(prompt_at("(venv) user@web-1:~/app$ ", 25));
    // 宽字符（中文提示符）：列号按单元格数，不按字符数。
    assert!(prompt_at("你好 $ ", 7));
}

#[test]
fn password_prompts_pagers_and_other_lines_are_not_a_prompt() {
    assert!(!prompt_at("Password: ", 10));
    assert!(!prompt_at("[sudo] password for user: ", 26));
    assert!(!prompt_at("--More--", 8));
    assert!(!prompt_at(" 12:00:01 up 3 days", 19));
    assert!(!prompt_at("Continue connecting (yes/no)? ", 30));
    assert!(!prompt_at(
        "Last login: Mon Jan  1 00:00:00 from 192.0.2.7",
        46
    ));
    // 提示符后面已经有了命令文字。
    assert!(!prompt_at("$ ls", 4));
    assert!(!prompt_at("$ ls ", 5));
}

#[test]
fn a_prompt_without_the_trailing_space_is_not_idle_even_though_the_screen_pads_with_blanks() {
    // 交换机：`Router>` 后面没有空格，光标紧跟在 `>` 后面。整行补齐的空格不能被当成提示符后的那个空格。
    assert!(!prompt_at("Router>", 7));
    assert!(!prompt_at("Switch#", 7));
    assert!(!prompt_at("web-1$", 6));
}

#[test]
fn the_cursor_must_sit_right_after_the_space_of_the_prompt() {
    assert!(!prompt_at("$ ", 1), "光标在提示符字符上");
    assert!(
        !prompt_at("$ ", 3),
        "光标在空格之后还有一格：输入行里已经有空格"
    );
    assert!(!prompt_at("$ ", 20), "光标不在这行的提示符附近");
    assert!(!prompt_at("$   ", 4), "用户已经敲了空格");
    assert!(!prompt_at("$ ", 0));
}

#[test]
fn an_empty_line_or_a_full_width_line_is_not_a_prompt() {
    assert!(!prompt_at("", 0));
    assert!(!prompt_at("", 1));
    let full = format!("{}$", "x".repeat(COLS - 1));
    assert!(
        !prompt_at(&full, COLS),
        "提示符字符在最后一列，后面没有空格"
    );
    assert!(!looks_like_idle_prompt(&[], 0));
}

#[test]
fn a_prompt_character_inside_a_composed_or_wide_cell_does_not_count() {
    let mut cells = row("$ ");
    cells[0].extra = Some(vec!['\u{301}'].into_boxed_slice());
    assert!(!looks_like_idle_prompt(&cells, 2));
    let mut wide = row("$ ");
    wide[0].kind = CellKind::Wide;
    assert!(!looks_like_idle_prompt(&wide, 2));
}

/// 假屏幕：几行文字 + 光标。
struct FakeScreen {
    rows: Vec<Vec<InputCell>>,
    cursor_row: usize,
    cursor_col: usize,
    alt_screen: bool,
    display_offset: usize,
}

impl FakeScreen {
    fn new(lines: &[&str], cursor_row: usize, cursor_col: usize) -> Self {
        let mut rows: Vec<_> = lines.iter().map(|line| row(line)).collect();
        while rows.len() < 6 {
            rows.push(row(""));
        }
        Self {
            rows,
            cursor_row,
            cursor_col,
            alt_screen: false,
            display_offset: 0,
        }
    }
}

impl ScreenSource for FakeScreen {
    fn view(&self) -> ScreenView {
        ScreenView {
            rows: self.rows.len(),
            cols: COLS,
            cursor_row: self.cursor_row,
            cursor_col: self.cursor_col,
            alt_screen: self.alt_screen,
            display_offset: self.display_offset,
            content_revision: 0,
        }
    }

    fn line(&self, row: usize) -> Option<Vec<InputCell>> {
        self.rows.get(row).cloned()
    }
}

#[test]
fn the_last_non_empty_row_must_be_the_prompt_and_hold_the_cursor() {
    // 登录横幅 + 提示符，光标在提示符行。
    assert!(screen_at_idle_prompt(&FakeScreen::new(
        &["Welcome to web-1", "Last login: today", "user@web-1:~$ "],
        2,
        14
    )));
    // 提示符下面还有别的内容：最后一行非空内容不是提示符。
    assert!(!screen_at_idle_prompt(&FakeScreen::new(
        &["user@web-1:~$ ", "Loading..."],
        0,
        14
    )));
    // 光标不在最后一行非空内容那一行。
    assert!(!screen_at_idle_prompt(&FakeScreen::new(
        &["user@web-1:~$ "],
        3,
        0
    )));
    // 整屏是空的。
    assert!(!screen_at_idle_prompt(&FakeScreen::new(&[], 0, 0)));
    // 密码提示不是。
    assert!(!screen_at_idle_prompt(&FakeScreen::new(
        &["user@web-1 password: "],
        0,
        21
    )));
}

#[test]
fn the_alternate_screen_and_the_scrollback_never_count() {
    let mut alt = FakeScreen::new(&["$ "], 0, 2);
    alt.alt_screen = true;
    assert!(!screen_at_idle_prompt(&alt));
    let mut scrolled = FakeScreen::new(&["$ "], 0, 2);
    scrolled.display_offset = 3;
    assert!(!screen_at_idle_prompt(&scrolled));
}

// ---- 判定 ----

fn ready() -> Inputs {
    Inputs {
        elapsed: Duration::from_secs(3),
        quiet_for: Duration::from_secs(2),
        probe: ProbeState::Shell(LoginShell::Bash),
        user_typed: false,
        ai_busy: false,
        locked: false,
        has_marks: false,
    }
}

fn judge(inputs: &Inputs, at_prompt: bool) -> Decision {
    decide(inputs, || at_prompt)
}

#[test]
fn everything_in_place_sends_the_script_for_the_login_shell() {
    assert_eq!(judge(&ready(), true), Decision::Send(ShellKind::Bash));
    let zsh = Inputs {
        probe: ProbeState::Shell(LoginShell::Zsh),
        ..ready()
    };
    assert_eq!(judge(&zsh, true), Decision::Send(ShellKind::Zsh));
}

#[test]
fn the_shell_must_be_known_and_be_bash_or_zsh() {
    let other = Inputs {
        probe: ProbeState::Shell(LoginShell::Other),
        ..ready()
    };
    assert_eq!(
        judge(&other, true),
        Decision::Skip(SkipReason::NotBashOrZsh)
    );
    let failed = Inputs {
        probe: ProbeState::Failed,
        ..ready()
    };
    assert_eq!(
        judge(&failed, true),
        Decision::Skip(SkipReason::ShellUnknown)
    );
    // 探测还没回来：等；窗口过了还没回来：放弃。
    let pending = Inputs {
        probe: ProbeState::Pending,
        ..ready()
    };
    assert_eq!(judge(&pending, true), Decision::Wait);
    let pending_late = Inputs {
        elapsed: ATTEMPT_WINDOW,
        ..pending
    };
    assert_eq!(
        judge(&pending_late, true),
        Decision::Skip(SkipReason::ShellUnknown)
    );
}

#[test]
fn any_user_input_since_connecting_gives_up_for_good() {
    let typed = Inputs {
        user_typed: true,
        ..ready()
    };
    assert_eq!(judge(&typed, true), Decision::Skip(SkipReason::UserTyped));
    // 即使探测还没回来也一样：用户已经开始用这个终端了。
    let typed_early = Inputs {
        user_typed: true,
        probe: ProbeState::Pending,
        ..ready()
    };
    assert_eq!(
        judge(&typed_early, false),
        Decision::Skip(SkipReason::UserTyped)
    );
}

#[test]
fn the_output_must_be_quiet_for_one_and_a_half_seconds_and_the_screen_must_show_an_idle_prompt() {
    let noisy = Inputs {
        quiet_for: QUIET_BEFORE_SEND - Duration::from_millis(1),
        ..ready()
    };
    assert_eq!(judge(&noisy, true), Decision::Wait);
    let just_quiet = Inputs {
        quiet_for: QUIET_BEFORE_SEND,
        ..ready()
    };
    assert_eq!(judge(&just_quiet, true), Decision::Send(ShellKind::Bash));
    // 安静了但屏幕不像提示符：等。
    assert_eq!(judge(&ready(), false), Decision::Wait);
}

#[test]
fn the_screen_is_only_read_when_the_cheap_conditions_already_hold() {
    let mut reads = 0;
    let noisy = Inputs {
        quiet_for: Duration::ZERO,
        ..ready()
    };
    decide(&noisy, || {
        reads += 1;
        true
    });
    let typed = Inputs {
        user_typed: true,
        ..ready()
    };
    decide(&typed, || {
        reads += 1;
        true
    });
    let locked = Inputs {
        locked: true,
        ..ready()
    };
    decide(&locked, || {
        reads += 1;
        true
    });
    assert_eq!(reads, 0);
}

#[test]
fn a_locked_app_an_active_ai_or_existing_marks_block_it() {
    let locked = Inputs {
        locked: true,
        ..ready()
    };
    assert_eq!(judge(&locked, true), Decision::Wait);
    let ai = Inputs {
        ai_busy: true,
        ..ready()
    };
    assert_eq!(judge(&ai, true), Decision::Wait);
    let marks = Inputs {
        has_marks: true,
        ..ready()
    };
    assert_eq!(
        judge(&marks, true),
        Decision::Skip(SkipReason::AlreadyHasMarks)
    );
}

#[test]
fn after_fifteen_seconds_it_gives_up_even_if_everything_lines_up_late() {
    let almost = Inputs {
        elapsed: ATTEMPT_WINDOW - Duration::from_millis(1),
        ..ready()
    };
    assert_eq!(judge(&almost, true), Decision::Send(ShellKind::Bash));
    let late = Inputs {
        elapsed: ATTEMPT_WINDOW,
        ..ready()
    };
    assert_eq!(
        judge(&late, true),
        Decision::Skip(SkipReason::NoIdlePrompt),
        "超过 15 秒才凑齐：不发"
    );
    // 放弃时给的是当时真正挡住的原因。
    assert_eq!(
        judge(
            &Inputs {
                locked: true,
                ..late
            },
            true
        ),
        Decision::Skip(SkipReason::Locked)
    );
    assert_eq!(
        judge(
            &Inputs {
                ai_busy: true,
                ..late
            },
            true
        ),
        Decision::Skip(SkipReason::AiBusy)
    );
    assert_eq!(
        judge(
            &Inputs {
                quiet_for: Duration::ZERO,
                ..late
            },
            false
        ),
        Decision::Skip(SkipReason::NoIdlePrompt)
    );
}
