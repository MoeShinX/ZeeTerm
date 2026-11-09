//! 状态迁移表（方案 7.2）逐行测试：`row_N_*` 对应表里第 N 行。
//! 用一块假屏幕模拟终端：写字、换行、挪光标，状态机只通过 `ScreenSource` 看它。

use std::time::{Duration, Instant};

use super::*;
use crate::terminal_assist::cells::CellKind;

// ---- 假屏幕 ----

struct Screen {
    cols: usize,
    rows: Vec<Vec<InputCell>>,
    cursor: (usize, usize),
    alt: bool,
    offset: usize,
    revision: u64,
}

fn is_wide(ch: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&ch)
}

impl Screen {
    fn new(rows: usize, cols: usize) -> Self {
        Self {
            cols,
            rows: vec![vec![InputCell::plain(' '); cols]; rows],
            cursor: (0, 0),
            alt: false,
            offset: 0,
            revision: 1,
        }
    }

    fn put(&mut self, text: &str) {
        for ch in text.chars() {
            let (row, col) = self.cursor;
            if ch == '\u{301}' {
                if col > 0 {
                    self.rows[row][col - 1].extra = Some(vec![ch].into_boxed_slice());
                }
                continue;
            }
            if is_wide(ch) {
                self.rows[row][col] = InputCell {
                    ch,
                    extra: None,
                    kind: CellKind::Wide,
                };
                self.rows[row][col + 1] = InputCell {
                    ch: ' ',
                    extra: None,
                    kind: CellKind::WideSpacer,
                };
                self.cursor.1 += 2;
            } else {
                self.rows[row][col] = InputCell::plain(ch);
                self.cursor.1 += 1;
            }
        }
        self.revision += 1;
    }

    fn newline(&mut self) {
        self.cursor = (self.cursor.0 + 1, 0);
        self.revision += 1;
    }

    /// 擦掉光标前 n 个字符（退格效果）。
    fn erase_back(&mut self, n: usize) {
        for _ in 0..n {
            self.cursor.1 -= 1;
            let (row, col) = self.cursor;
            self.rows[row][col] = InputCell::plain(' ');
        }
        self.revision += 1;
    }

    fn text(&self, row: usize) -> String {
        self.rows[row].iter().map(|c| c.ch).collect()
    }
}

impl ScreenSource for Screen {
    fn view(&self) -> ScreenView {
        ScreenView {
            rows: self.rows.len(),
            cols: self.cols,
            cursor_row: self.cursor.0,
            cursor_col: self.cursor.1,
            alt_screen: self.alt,
            display_offset: self.offset,
            content_revision: self.revision,
        }
    }

    fn line(&self, row: usize) -> Option<Vec<InputCell>> {
        self.rows.get(row).cloned()
    }
}

// ---- 测试台 ----

struct Rig {
    t: InputTracker,
    s: Screen,
    now: Instant,
}

fn ctx() -> TrackerContext {
    TrackerContext {
        pane_id: 1,
        connection_generation: 1,
        target: "ssh://u@host:22".into(),
        broadcast_revision: 0,
    }
}

impl Rig {
    fn new() -> Self {
        Self {
            t: InputTracker::new(ctx()),
            s: Screen::new(6, 40),
            now: Instant::now(),
        }
    }

    fn tick(&mut self, ms: u64) {
        self.now += Duration::from_millis(ms);
    }

    fn anchor(&self) -> MarkAnchor {
        MarkAnchor {
            row: self.s.cursor.0,
            col: self.s.cursor.1,
            alt_screen: self.s.alt,
        }
    }

    fn mark(&mut self, kind: MarkKind) {
        let anchor = self.anchor();
        self.t.on_mark(kind, anchor, &self.s, self.now);
    }

    /// 打印提示符并收到 133;B。
    fn prompt(&mut self) {
        self.mark(MarkKind::PromptStart);
        self.s.put("$ ");
        self.mark(MarkKind::PromptEnd);
    }

    fn observe(&mut self) {
        self.t.observe(&self.s, self.now);
    }

    /// 逐键输入并立刻回显（普通情况）。
    fn type_echoed(&mut self, text: &str) {
        let seq = self.t.on_text(text, IntentOrigin::Manual, self.now);
        self.s.put(text);
        self.observe();
        if let Some(seq) = seq {
            self.t.on_write_outcome(seq, WriteOutcome::Queued, self.now);
        }
    }

    fn phase(&self) -> Phase {
        self.t.phase()
    }
}

fn at_prompt() -> Rig {
    let mut rig = Rig::new();
    rig.prompt();
    assert_eq!(rig.phase(), Phase::Input);
    rig
}

// ---- 第 1 行：Unknown，可靠 Prompt 输入起点 + cells/光标锚点确认 -> Input ----

#[test]
fn row_1_a_prompt_input_mark_with_a_matching_cursor_starts_a_new_epoch() {
    let mut rig = Rig::new();
    assert_eq!(rig.phase(), Phase::Unknown);
    let epoch = rig.t.state().prompt_epoch;
    rig.prompt();
    assert_eq!(rig.phase(), Phase::Input);
    assert_eq!(
        rig.t.state().prompt_epoch,
        epoch + 1,
        "新 Prompt 换新 epoch"
    );
    assert_eq!(rig.t.state().confirmed_text, "");
    assert_eq!(
        rig.t.state().input_anchor,
        Some(InputAnchor {
            row: 0,
            col: TermCol(2)
        })
    );
    assert_eq!(rig.t.state().origin, InputOrigin::Clean);
    assert!(rig.t.saw_prompt_marks());
}

#[test]
fn row_1_text_already_on_the_line_when_the_mark_is_handled_is_read_but_not_clean() {
    let mut rig = Rig::new();
    rig.mark(MarkKind::PromptStart);
    rig.s.put("$ ");
    let anchor = rig.anchor();
    // 回显和 B 在同一批里到达：处理标记时屏幕上已经有字了。
    rig.s.put("ls");
    rig.t.on_mark(MarkKind::PromptEnd, anchor, &rig.s, rig.now);
    assert_eq!(rig.phase(), Phase::Input);
    assert_eq!(rig.t.state().confirmed_text, "ls");
    assert_eq!(
        rig.t.state().origin,
        InputOrigin::Mixed,
        "来源不明，不采集历史"
    );
}

#[test]
fn row_1_a_cursor_that_is_not_on_the_anchor_row_does_not_become_input() {
    let mut rig = Rig::new();
    rig.s.put("$ ");
    let anchor = rig.anchor();
    rig.s.newline();
    rig.t.on_mark(MarkKind::PromptEnd, anchor, &rig.s, rig.now);
    assert_eq!(rig.phase(), Phase::Unknown);
}

#[test]
fn row_1_a_prompt_mark_on_the_alternate_screen_is_not_an_input_line() {
    let mut rig = Rig::new();
    rig.s.alt = true;
    rig.s.put("$ ");
    rig.mark(MarkKind::PromptEnd);
    assert_ne!(rig.phase(), Phase::Input);
}

// ---- 第 2 行：Input，手动字符 / IME 最终提交 ----

#[test]
fn row_2_a_manual_character_registers_the_intent_before_sending_and_waits_for_the_echo() {
    let mut rig = at_prompt();
    let seq = rig
        .t
        .on_text("l", IntentOrigin::Manual, rig.now)
        .expect("在输入行上要登记");
    // 还没发送、没回显：意图已经在了，disposition 还没有结果。
    assert_eq!(rig.phase(), Phase::AwaitingEcho);
    let intent = rig.t.pending_intents().next().expect("意图");
    assert_eq!(intent.local_seq, seq);
    assert_eq!(intent.expected_base, "");
    assert_eq!(intent.append_suffix, "l");
    assert_eq!(intent.disposition, Disposition::Pending);
    assert_eq!(rig.t.state().predicted_text, "l");
    assert_eq!(rig.t.state().confirmed_text, "", "预测不等于确认");
}

#[test]
fn row_2_typing_cancels_outstanding_candidate_requests() {
    let mut rig = at_prompt();
    let id = rig.t.identity();
    assert!(rig.t.is_current(&id));
    rig.type_echoed("l");
    assert!(!rig.t.is_current(&id), "输入变了，旧请求作废");
}

#[test]
fn row_2_an_ime_commit_is_one_intent_and_confirms_by_character() {
    let mut rig = at_prompt();
    rig.t.on_ime_preedit(true, rig.now);
    let seq = rig
        .t
        .on_text("你好", IntentOrigin::ImeCommit, rig.now)
        .expect("提交登记一次");
    assert!(!rig.t.state().composing, "提交结束预编辑");
    assert_eq!(rig.t.pending_intents().count(), 1);
    rig.s.put("你");
    rig.observe();
    assert_eq!(rig.phase(), Phase::AwaitingEcho, "只回显了一半");
    rig.s.put("好");
    rig.observe();
    rig.t.on_write_outcome(seq, WriteOutcome::Queued, rig.now);
    assert_eq!(rig.phase(), Phase::Input);
    assert_eq!(rig.t.state().confirmed_text, "你好");
    assert_eq!(
        rig.t.state().cursor_byte,
        ByteIdx(6),
        "字节下标不是列数也不是 UTF-16"
    );
}

#[test]
fn row_2_text_outside_an_input_line_is_not_registered() {
    let mut rig = Rig::new();
    assert_eq!(rig.t.on_text("x", IntentOrigin::Manual, rig.now), None);
    assert_eq!(rig.phase(), Phase::Unknown);
    let mut running = at_prompt();
    running.mark(MarkKind::CommandStart);
    assert_eq!(running.phase(), Phase::Running);
    assert_eq!(
        running.t.on_text("x", IntentOrigin::Manual, running.now),
        None
    );
}

// ---- 第 3 行：任意，IME 预编辑 ----

#[test]
fn row_3_preedit_does_not_record_search_or_fill_and_commits_only_once() {
    let mut rig = at_prompt();
    let id = rig.t.identity();
    rig.t.on_ime_preedit(true, rig.now);
    assert!(rig.t.state().composing);
    assert!(!rig.t.is_current(&id), "预编辑取消在途请求");
    assert_eq!(rig.t.pending_intents().count(), 0, "预编辑不产生写入意图");
    assert_eq!(rig.t.state().predicted_text, "");
    assert!(!rig.t.can_accept(), "预编辑期间不能填入");
    // 预编辑期间屏幕上的光标前文本没变，结束预编辑后按屏幕重新确认。
    rig.t.on_ime_preedit(false, rig.now);
    rig.observe();
    assert_eq!(rig.phase(), Phase::Input);
    assert!(!rig.t.state().composing);
}

#[test]
fn row_3_composing_works_in_any_phase() {
    let mut rig = Rig::new();
    rig.t.on_ime_preedit(true, rig.now);
    assert!(rig.t.state().composing);
    rig.t.on_ime_preedit(false, rig.now);
    assert!(!rig.t.state().composing);
}

// ---- 第 4 行：AwaitingEcho，屏幕确认单调前缀 ----

#[test]
fn row_4_chunked_echo_advances_the_confirmed_position_and_returns_to_input_only_when_complete() {
    let mut rig = at_prompt();
    let a = rig.t.on_text("l", IntentOrigin::Manual, rig.now).unwrap();
    rig.t.on_write_outcome(a, WriteOutcome::Queued, rig.now);
    let b = rig.t.on_text("s", IntentOrigin::Manual, rig.now).unwrap();
    rig.t.on_write_outcome(b, WriteOutcome::Queued, rig.now);
    let c = rig
        .t
        .on_text(" -la", IntentOrigin::Manual, rig.now)
        .unwrap();
    rig.t.on_write_outcome(c, WriteOutcome::Queued, rig.now);
    assert_eq!(rig.t.state().predicted_text, "ls -la");

    rig.s.put("l");
    rig.observe();
    assert_eq!(rig.t.state().confirmed_text, "l");
    assert_eq!(rig.phase(), Phase::AwaitingEcho);

    rig.s.put("s -");
    rig.observe();
    assert_eq!(
        rig.t.state().confirmed_text,
        "ls -",
        "分块回显推进，可以停在意图中间"
    );
    assert_eq!(rig.phase(), Phase::AwaitingEcho);
    assert_eq!(rig.t.pending_intents().count(), 1, "前两条已确认的意图丢掉");

    rig.s.put("la");
    rig.observe();
    assert_eq!(rig.phase(), Phase::Input);
    assert_eq!(rig.t.state().confirmed_text, "ls -la");
    assert_eq!(rig.t.state().predicted_text, "ls -la");
    assert_eq!(rig.t.pending_intents().count(), 0);
}

#[test]
fn row_4_ansi_redraws_that_leave_the_same_cells_do_not_confuse_the_confirmation() {
    let mut rig = at_prompt();
    rig.type_echoed("ab");
    // 远端先清掉这一行再整行重画（颜色重绘之类）：最终落在格子里的字没变，回显照常确认。
    let seq = rig.t.on_text("c", IntentOrigin::Manual, rig.now).unwrap();
    rig.s.put("c");
    rig.s.revision += 5;
    rig.observe();
    rig.t.on_write_outcome(seq, WriteOutcome::Queued, rig.now);
    assert_eq!(rig.phase(), Phase::Input);
    assert_eq!(rig.t.state().confirmed_text, "abc");
}

#[test]
fn row_4_wide_and_combining_characters_confirm_by_cell_not_by_byte() {
    let mut rig = at_prompt();
    rig.type_echoed("echo ");
    rig.type_echoed("你好");
    rig.type_echoed("e");
    rig.type_echoed("\u{301}");
    assert_eq!(rig.phase(), Phase::Input);
    assert_eq!(rig.t.state().confirmed_text, "echo 你好e\u{301}");
}

#[test]
fn row_4_a_synchronous_loopback_echo_arriving_inside_the_send_call_is_still_recognised() {
    let mut rig = at_prompt();
    // 先登记。
    let seq = rig.t.on_text("x", IntentOrigin::Manual, rig.now).unwrap();
    // 发送过程中（结果还没返回）回显就到了。
    rig.s.put("x");
    rig.observe();
    assert_eq!(rig.t.state().confirmed_text, "x", "回显已经确认");
    assert_eq!(
        rig.phase(),
        Phase::AwaitingEcho,
        "但发送结果还没回来，还有在途写入，不能回 Input"
    );
    rig.t.on_write_outcome(seq, WriteOutcome::Queued, rig.now);
    assert_eq!(rig.phase(), Phase::Input);
}

#[test]
fn row_4_no_screen_change_means_no_progress_not_a_failure() {
    let mut rig = at_prompt();
    let seq = rig.t.on_text("x", IntentOrigin::Manual, rig.now).unwrap();
    rig.t.on_write_outcome(seq, WriteOutcome::Queued, rig.now);
    rig.tick(2000);
    rig.observe();
    assert_eq!(
        rig.phase(),
        Phase::AwaitingEcho,
        "高延迟：2 秒还没回显，继续等"
    );
    rig.s.put("x");
    rig.tick(100);
    rig.observe();
    assert_eq!(rig.phase(), Phase::Input);
}

// ---- 第 5 行：AwaitingEcho，不匹配 / 超时 / 写入拒绝 / 部分失败 -> Desynced ----

#[test]
fn row_5_a_mismatching_echo_desyncs_and_clears_pending() {
    let mut rig = at_prompt();
    let seq = rig.t.on_text("ls", IntentOrigin::Manual, rig.now).unwrap();
    rig.t.on_write_outcome(seq, WriteOutcome::Queued, rig.now);
    rig.s.put("lx");
    rig.observe();
    assert_eq!(rig.phase(), Phase::Desynced);
    assert_eq!(
        rig.t.state().desync_reason,
        Some(DesyncReason::EchoMismatch)
    );
    assert_eq!(rig.t.pending_intents().count(), 0);
    assert_eq!(rig.t.state().predicted_text, rig.t.state().confirmed_text);
    assert!(!rig.t.can_accept());
}

#[test]
fn row_5_the_echo_deadline_is_five_seconds_from_registration_and_output_does_not_extend_it() {
    let mut rig = at_prompt();
    let seq = rig.t.on_text("ab", IntentOrigin::Manual, rig.now).unwrap();
    rig.t.on_write_outcome(seq, WriteOutcome::Queued, rig.now);
    // 只回显了第一个字符，之后屏幕上别处一直有输出（revision 一直在变）。
    rig.s.put("a");
    rig.observe();
    for _ in 0..4 {
        rig.tick(1000);
        rig.s.revision += 1;
        rig.observe();
        assert_eq!(rig.phase(), Phase::AwaitingEcho);
    }
    rig.tick(1000);
    rig.s.revision += 1;
    rig.observe();
    assert_eq!(rig.phase(), Phase::Desynced);
    assert_eq!(rig.t.state().desync_reason, Some(DesyncReason::EchoTimeout));
}

#[test]
fn row_5_each_intent_has_its_own_deadline() {
    let mut rig = at_prompt();
    let first = rig.t.on_text("a", IntentOrigin::Manual, rig.now).unwrap();
    rig.t.on_write_outcome(first, WriteOutcome::Queued, rig.now);
    rig.tick(4000);
    let second = rig.t.on_text("b", IntentOrigin::Manual, rig.now).unwrap();
    rig.t
        .on_write_outcome(second, WriteOutcome::Queued, rig.now);
    rig.s.put("a");
    rig.observe();
    rig.tick(1500);
    // 第二条才等了 1.5 秒，第一条已经确认：不超时。
    rig.observe();
    assert_eq!(rig.phase(), Phase::AwaitingEcho);
    rig.tick(3600);
    rig.observe();
    assert_eq!(rig.phase(), Phase::Desynced);
}

#[test]
fn row_5_a_rejected_write_desyncs_and_never_pretends_it_was_typed() {
    for reason in [
        RejectReason::Locked,
        RejectReason::NotLive,
        RejectReason::QueueFull,
    ] {
        let mut rig = at_prompt();
        let seq = rig.t.on_text("x", IntentOrigin::Manual, rig.now).unwrap();
        rig.t
            .on_write_outcome(seq, WriteOutcome::Rejected(reason), rig.now);
        assert_eq!(rig.phase(), Phase::Desynced, "{reason:?}");
        assert_eq!(
            rig.t.state().desync_reason,
            Some(DesyncReason::WriteRejected(reason))
        );
        assert_eq!(rig.t.state().confirmed_text, "");
        assert_eq!(rig.t.state().predicted_text, "", "没发出去的不留在预测里");
    }
}

#[test]
fn row_5_a_partial_transport_failure_desyncs_and_the_tracker_never_offers_compensation() {
    let mut rig = at_prompt();
    let a = rig.t.on_text("ab", IntentOrigin::Manual, rig.now).unwrap();
    rig.t.on_write_outcome(a, WriteOutcome::Queued, rig.now);
    let b = rig.t.on_text("cd", IntentOrigin::Manual, rig.now).unwrap();
    rig.s.put("ab");
    rig.observe();
    rig.t
        .on_write_outcome(b, WriteOutcome::TransportFailed, rig.now);
    assert_eq!(rig.phase(), Phase::Desynced);
    assert_eq!(
        rig.t.state().desync_reason,
        Some(DesyncReason::TransportFailed)
    );
    // 已确认的 "ab" 还认；没有任何 API 会产出要发给 shell 的字节。
    assert_eq!(rig.t.state().confirmed_text, "ab");
    assert_eq!(rig.t.pending_intents().count(), 0);
}

#[test]
fn row_5_an_outcome_for_a_forgotten_intent_is_ignored() {
    let mut rig = at_prompt();
    let seq = rig.t.on_text("x", IntentOrigin::Manual, rig.now).unwrap();
    rig.t.on_connection_reset(rig.now);
    rig.t
        .on_write_outcome(seq, WriteOutcome::TransportFailed, rig.now);
    assert_eq!(rig.phase(), Phase::Unknown, "重置后迟到的结果不改状态");
}

#[test]
fn row_5_too_many_unconfirmed_writes_or_a_line_longer_than_the_cap_stop_tracking() {
    let mut rig = at_prompt();
    for _ in 0..MAX_PENDING_INTENTS {
        let seq = rig.t.on_text("a", IntentOrigin::Manual, rig.now).unwrap();
        rig.t.on_write_outcome(seq, WriteOutcome::Queued, rig.now);
    }
    assert_eq!(rig.t.on_text("a", IntentOrigin::Manual, rig.now), None);
    assert_eq!(
        rig.t.state().desync_reason,
        Some(DesyncReason::TooManyPending)
    );

    let mut long = at_prompt();
    let big = "a".repeat(MAX_TRACKED_BYTES);
    let seq = long
        .t
        .on_text(&big, IntentOrigin::Manual, long.now)
        .unwrap();
    long.t.on_write_outcome(seq, WriteOutcome::Queued, long.now);
    assert_eq!(long.t.on_text("b", IntentOrigin::Manual, long.now), None);
    assert_eq!(long.t.state().desync_reason, Some(DesyncReason::TooLong));
}

// ---- 第 6 行：Input，显式接受候选 ----

#[test]
fn row_6_accepting_sends_only_the_suffix_and_never_marks_it_confirmed_early() {
    let mut rig = at_prompt();
    rig.type_echoed("git st");
    let id = rig.t.identity();
    let seq = rig.t.begin_accept("atus", &id, rig.now).expect("可以接受");
    assert_eq!(rig.phase(), Phase::AwaitingEcho);
    let intent = rig.t.pending_intents().next().unwrap();
    assert_eq!(intent.origin, IntentOrigin::Accept);
    assert_eq!(intent.expected_base, "git st");
    assert_eq!(intent.append_suffix, "atus", "只有后缀，没有回车");
    assert_eq!(rig.t.state().confirmed_text, "git st", "不提前改成已确认");
    assert_eq!(rig.t.state().predicted_text, "git status");
    rig.t.on_write_outcome(seq, WriteOutcome::Queued, rig.now);
    rig.s.put("atus");
    rig.observe();
    assert_eq!(rig.phase(), Phase::Input);
    assert_eq!(rig.t.state().confirmed_text, "git status");
}

#[test]
fn row_6_accepting_is_refused_unless_everything_still_matches() {
    // 请求过期。
    let mut rig = at_prompt();
    rig.type_echoed("ab");
    let stale = rig.t.identity();
    rig.type_echoed("c");
    assert_eq!(
        rig.t.begin_accept("d", &stale, rig.now),
        Err(AcceptRefusal::StaleRequest)
    );
    // 请求被新请求顶掉。
    let first = rig.t.identity();
    let _second = rig.t.identity();
    assert_eq!(
        rig.t.begin_accept("d", &first, rig.now),
        Err(AcceptRefusal::StaleRequest)
    );
    // 有在途写入。
    let seq = rig.t.on_text("x", IntentOrigin::Manual, rig.now).unwrap();
    let id = rig.t.identity();
    assert_eq!(
        rig.t.begin_accept("d", &id, rig.now),
        Err(AcceptRefusal::NotInInput)
    );
    rig.t.on_write_outcome(seq, WriteOutcome::Queued, rig.now);
    rig.s.put("x");
    rig.observe();
    // 预编辑中。
    let id = rig.t.identity();
    rig.t.on_ime_preedit(true, rig.now);
    assert_eq!(
        rig.t.begin_accept("d", &id, rig.now),
        Err(AcceptRefusal::Composing)
    );
    rig.t.on_ime_preedit(false, rig.now);
    // 坏后缀。
    let id = rig.t.identity();
    assert_eq!(
        rig.t.begin_accept("", &id, rig.now),
        Err(AcceptRefusal::BadSuffix)
    );
    assert_eq!(
        rig.t.begin_accept("a\nb", &id, rig.now),
        Err(AcceptRefusal::BadSuffix)
    );
    assert!(rig.t.begin_accept("d", &id, rig.now).is_ok());
}

#[test]
fn row_6_accepting_is_refused_when_the_cursor_is_not_at_the_end_of_the_line() {
    let mut rig = at_prompt();
    rig.type_echoed("ls");
    // 行中编辑：光标回到 "l" 后面，后面还有 "s"。
    rig.s.cursor.1 -= 1;
    rig.s.revision += 1;
    rig.observe();
    // 内容的"光标前文本"变成 "l"，与确认的 "ls" 不同 -> 失同步，不可接受。
    assert_eq!(rig.phase(), Phase::Desynced);
    let id = rig.t.identity();
    assert!(rig.t.begin_accept("x", &id, rig.now).is_err());
}

// ---- 第 7 行：Input，普通 Enter ----

fn press_enter_and_start(rig: &mut Rig) {
    rig.t.on_key(KeyClass::Enter, rig.now);
    rig.s.newline();
    rig.observe();
    rig.mark(MarkKind::CommandStart);
}

#[test]
fn row_7_enter_only_saves_a_snapshot_and_the_command_start_event_commits_it() {
    let mut rig = at_prompt();
    rig.type_echoed("ls -la");
    rig.t.on_key(KeyClass::Enter, rig.now);
    assert_eq!(
        rig.phase(),
        Phase::Input,
        "还没有命令开始事件，不进 Running"
    );
    let snapshot = rig.t.state().submitted_command.clone().expect("待提交快照");
    assert_eq!(snapshot.text, "ls -la");
    assert_eq!(snapshot.prompt_epoch, rig.t.state().prompt_epoch);
    assert!(
        !rig.t
            .take_events()
            .iter()
            .any(|e| matches!(e, TrackerEvent::CommandStarted { .. })),
        "Enter 本身不算已执行"
    );
    // CR 回显换行：这是预期的，不能因此失同步。
    rig.s.newline();
    rig.observe();
    assert_eq!(rig.phase(), Phase::Input);
    rig.mark(MarkKind::CommandStart);
    assert_eq!(rig.phase(), Phase::Running);
    assert!(rig.t.take_events().contains(&TrackerEvent::CommandStarted {
        command: Some("ls -la".into())
    }));
    assert_eq!(rig.t.state().submitted_command, None);
}

#[test]
fn row_7_a_new_prompt_or_a_context_change_discards_the_unconfirmed_snapshot() {
    let mut rig = at_prompt();
    rig.type_echoed("rm x");
    rig.t.on_key(KeyClass::Enter, rig.now);
    rig.s.newline();
    // 没有命令开始，直接来了新 Prompt（命令被 shell 拒绝 / 取消）。
    rig.prompt();
    assert_eq!(rig.t.state().submitted_command, None);
    rig.mark(MarkKind::CommandStart);
    assert!(rig
        .t
        .take_events()
        .iter()
        .all(|e| !matches!(e, TrackerEvent::CommandStarted { command: Some(_) })));

    let mut reset = at_prompt();
    reset.type_echoed("rm y");
    reset.t.on_key(KeyClass::Enter, reset.now);
    reset.t.on_connection_reset(reset.now);
    assert_eq!(reset.t.state().submitted_command, None);
}

#[test]
fn row_7_typing_after_enter_before_the_start_event_drops_the_snapshot() {
    let mut rig = at_prompt();
    rig.type_echoed("ls");
    rig.t.on_key(KeyClass::Enter, rig.now);
    let seq = rig.t.on_text("x", IntentOrigin::Manual, rig.now).unwrap();
    rig.t.on_write_outcome(seq, WriteOutcome::Queued, rig.now);
    assert_eq!(rig.t.state().submitted_command, None);
}

#[test]
fn row_7_history_conditions_leading_space_empty_mixed_origin_and_shell_reported_mismatch() {
    // 前导空格：不采集。
    let mut spaced = at_prompt();
    spaced.type_echoed(" secret");
    press_enter_and_start(&mut spaced);
    assert!(spaced
        .t
        .take_events()
        .iter()
        .all(|e| !matches!(e, TrackerEvent::CommandStarted { command: Some(_) })));
    // 空命令：不采集。
    let mut empty = at_prompt();
    press_enter_and_start(&mut empty);
    assert!(empty
        .t
        .take_events()
        .iter()
        .all(|e| !matches!(e, TrackerEvent::CommandStarted { command: Some(_) })));
    // 来源不明（从屏幕上原有的内容读出来的）：不采集。
    let mut mixed = Rig::new();
    mixed.mark(MarkKind::PromptStart);
    mixed.s.put("$ ");
    let anchor = mixed.anchor();
    mixed.s.put("from-history");
    mixed
        .t
        .on_mark(MarkKind::PromptEnd, anchor, &mixed.s, mixed.now);
    press_enter_and_start(&mut mixed);
    assert!(mixed
        .t
        .take_events()
        .iter()
        .all(|e| !matches!(e, TrackerEvent::CommandStarted { command: Some(_) })));
    // shell 报告的命令行和本地快照不一致：不采集。
    let mut differ = at_prompt();
    differ.type_echoed("ls");
    differ.mark(MarkKind::CommandLine("something-else".into()));
    press_enter_and_start(&mut differ);
    assert!(differ
        .t
        .take_events()
        .iter()
        .all(|e| !matches!(e, TrackerEvent::CommandStarted { command: Some(_) })));
    // 一致的 633;E 不妨碍采集。
    let mut same = at_prompt();
    same.type_echoed("ls");
    same.mark(MarkKind::CommandLine("ls".into()));
    press_enter_and_start(&mut same);
    assert!(same
        .t
        .take_events()
        .contains(&TrackerEvent::CommandStarted {
            command: Some("ls".into())
        }));
}

#[test]
fn row_7_enter_after_a_tab_handoff_is_not_collected_even_when_the_line_is_reconciled() {
    let mut rig = at_prompt();
    rig.type_echoed("gi");
    rig.t.on_key(KeyClass::Handoff(HandoffKey::Tab), rig.now);
    rig.s.put("t ");
    rig.tick(1000);
    rig.observe();
    rig.tick(300);
    rig.observe();
    assert_eq!(rig.phase(), Phase::Input);
    assert_eq!(rig.t.state().origin, InputOrigin::Mixed);
    press_enter_and_start(&mut rig);
    assert!(rig
        .t
        .take_events()
        .iter()
        .all(|e| !matches!(e, TrackerEvent::CommandStarted { command: Some(_) })));
}

#[test]
fn row_7_enter_without_a_start_event_for_five_seconds_desyncs() {
    let mut rig = at_prompt();
    rig.type_echoed("ls");
    rig.t.on_key(KeyClass::Enter, rig.now);
    rig.tick(5000);
    rig.observe();
    assert_eq!(rig.phase(), Phase::Desynced);
    assert_eq!(
        rig.t.state().desync_reason,
        Some(DesyncReason::EnterWithoutStart)
    );
}

// ---- 第 8 行：AwaitingEcho / Desynced 中用户 Enter ----

#[test]
fn row_8_enter_while_awaiting_echo_or_desynced_collects_nothing() {
    let mut rig = at_prompt();
    let seq = rig.t.on_text("ls", IntentOrigin::Manual, rig.now).unwrap();
    rig.t.on_write_outcome(seq, WriteOutcome::Queued, rig.now);
    rig.t.on_key(KeyClass::Enter, rig.now);
    assert_eq!(rig.phase(), Phase::Desynced);
    assert_eq!(
        rig.t.state().desync_reason,
        Some(DesyncReason::EnterWhileUncertain)
    );
    assert_eq!(rig.t.state().submitted_command, None);
    rig.mark(MarkKind::CommandStart);
    assert!(rig
        .t
        .take_events()
        .iter()
        .all(|e| !matches!(e, TrackerEvent::CommandStarted { command: Some(_) })));

    let mut desynced = at_prompt();
    desynced
        .t
        .on_key(KeyClass::Handoff(HandoffKey::Tab), desynced.now);
    desynced.t.on_key(KeyClass::Enter, desynced.now);
    assert_eq!(desynced.phase(), Phase::Desynced);
    assert_eq!(desynced.t.state().submitted_command, None);
}

// ---- 第 9 行：Input，Tab / 方向键 / Ctrl+R / vi / 未知编辑 ----

#[test]
fn row_9_keys_handed_to_the_shell_desync_the_line() {
    for key in [
        HandoffKey::Tab,
        HandoffKey::Navigation,
        HandoffKey::HistorySearch,
        HandoffKey::Escape,
        HandoffKey::Control,
        HandoffKey::Other,
    ] {
        let mut rig = at_prompt();
        rig.type_echoed("ab");
        let id = rig.t.identity();
        rig.t.on_key(KeyClass::Handoff(key), rig.now);
        assert_eq!(rig.phase(), Phase::Desynced, "{key:?}");
        assert_eq!(
            rig.t.state().desync_reason,
            Some(DesyncReason::Handoff(key))
        );
        assert!(!rig.t.is_current(&id), "{key:?}：候选作废");
        assert_eq!(rig.t.state().predicted_text, "ab", "不丢已确认的");
    }
}

#[test]
fn row_9_it_comes_back_to_input_only_when_anchor_cells_and_cursor_reconcile_uniquely() {
    let mut rig = at_prompt();
    rig.type_echoed("gi");
    rig.t.on_key(KeyClass::Handoff(HandoffKey::Tab), rig.now);
    // Tab 的响应还没到：不能马上把旧内容当成现在的输入行。
    rig.tick(100);
    rig.observe();
    assert_eq!(rig.phase(), Phase::Desynced);
    // shell 补全了 "t "，屏幕变了；变化之后要安静一小会儿才重新读。
    rig.s.put("t ");
    rig.observe();
    assert_eq!(rig.phase(), Phase::Desynced);
    rig.tick(100);
    rig.observe();
    assert_eq!(rig.phase(), Phase::Desynced, "还没安静够");
    rig.tick(100);
    rig.observe();
    assert_eq!(rig.phase(), Phase::Input);
    assert_eq!(rig.t.state().confirmed_text, "git ", "读出来保留尾部空格");
    assert_eq!(rig.t.state().origin, InputOrigin::Mixed);
}

#[test]
fn row_9_a_tab_that_changes_nothing_recovers_after_the_quiet_wait() {
    let mut rig = at_prompt();
    rig.type_echoed("gi");
    rig.t.on_key(KeyClass::Handoff(HandoffKey::Tab), rig.now);
    rig.tick(500);
    rig.observe();
    assert_eq!(rig.phase(), Phase::Desynced);
    rig.tick(400);
    rig.observe();
    assert_eq!(rig.phase(), Phase::Input);
    assert_eq!(rig.t.state().confirmed_text, "gi");
}

#[test]
fn row_9_when_the_line_cannot_be_reconciled_it_waits_for_a_new_prompt() {
    let mut rig = at_prompt();
    rig.type_echoed("ab");
    rig.t
        .on_key(KeyClass::Handoff(HandoffKey::Navigation), rig.now);
    // 输入换行了：光标不在输入起点那一行。
    rig.s.newline();
    for _ in 0..10 {
        rig.tick(1000);
        rig.observe();
    }
    assert_eq!(rig.phase(), Phase::Desynced);
    assert!(rig.t.state().multiline);
    // 新 Prompt 才能恢复。
    rig.prompt();
    assert_eq!(rig.phase(), Phase::Input);
    assert!(!rig.t.state().multiline);
}

#[test]
fn row_9_repeated_recovery_attempts_are_bounded_while_nothing_changes() {
    struct Counting<'a> {
        inner: &'a Screen,
        reads: std::cell::Cell<usize>,
    }
    impl ScreenSource for Counting<'_> {
        fn view(&self) -> ScreenView {
            self.inner.view()
        }
        fn line(&self, row: usize) -> Option<Vec<InputCell>> {
            self.reads.set(self.reads.get() + 1);
            self.inner.line(row)
        }
    }
    let mut rig = at_prompt();
    rig.type_echoed("ab");
    rig.t
        .on_key(KeyClass::Handoff(HandoffKey::Navigation), rig.now);
    // 光标跑到输入起点左边：同一行，单元格会被读，但读不出输入。
    rig.s.cursor.1 = 0;
    rig.s.revision += 1;
    rig.observe();
    let counting = Counting {
        inner: &rig.s,
        reads: std::cell::Cell::new(0),
    };
    for _ in 0..200 {
        rig.now += Duration::from_millis(900);
        rig.t.observe(&counting, rig.now);
    }
    assert!(
        counting.reads.get() <= 8,
        "屏幕不变时不能每次都去读单元格：{}",
        counting.reads.get()
    );
}

// ---- 第 10 行：Backspace / Delete ----

#[test]
fn row_10_backspace_and_delete_invalidate_first_and_never_guess_by_utf8_bytes() {
    for key in [KeyClass::Backspace, KeyClass::Delete] {
        let mut rig = at_prompt();
        rig.type_echoed("你好");
        let id = rig.t.identity();
        rig.t.on_key(key, rig.now);
        assert_eq!(rig.phase(), Phase::Desynced);
        assert_eq!(rig.t.state().desync_reason, Some(DesyncReason::Edit));
        assert!(!rig.t.is_current(&id));
        // 屏幕：退格擦掉一个宽字符（2 列）。
        rig.s.erase_back(2);
        rig.tick(10);
        rig.observe();
        rig.tick(200);
        rig.observe();
        assert_eq!(rig.phase(), Phase::Input);
        assert_eq!(
            rig.t.state().confirmed_text,
            "你",
            "按实际 cells 读，不是按字节猜删多少"
        );
    }
}

#[test]
fn row_10_when_the_cells_cannot_confirm_it_stays_desynced() {
    let mut rig = at_prompt();
    rig.type_echoed("ab");
    rig.t.on_key(KeyClass::Backspace, rig.now);
    // 光标跑到了输入起点左边（整行被清）。
    rig.s.cursor.1 = 0;
    rig.s.revision += 1;
    rig.observe();
    rig.tick(300);
    rig.observe();
    assert_eq!(rig.phase(), Phase::Desynced);
}

// ---- 第 11 行：粘贴 / 快捷执行 / AI / 凭据回答 / 协议回复 ----

#[test]
fn row_11_foreign_writes_close_suggestions_and_need_a_new_boundary() {
    for source in [
        WriteSource::Paste,
        WriteSource::QuickCommand,
        WriteSource::Compose,
        WriteSource::AiInjection,
        WriteSource::CredentialAnswer,
        WriteSource::IntegrationScript,
    ] {
        let mut rig = at_prompt();
        rig.type_echoed("ls");
        rig.t.on_key(KeyClass::Enter, rig.now);
        let id = rig.t.identity();
        rig.t.on_foreign_write(source, rig.now);
        assert_eq!(rig.phase(), Phase::Unknown, "{source:?}");
        assert!(!rig.t.is_current(&id), "{source:?}");
        assert_eq!(rig.t.state().submitted_command, None, "不作为用户命令采集");
        // 屏幕再怎么安静，也不会自己回到 Input：必须等新 Prompt。
        for _ in 0..5 {
            rig.tick(1000);
            rig.observe();
        }
        assert_eq!(rig.phase(), Phase::Unknown, "{source:?}");
        rig.prompt();
        assert_eq!(rig.phase(), Phase::Input);
    }
}

#[test]
fn row_11_a_foreign_write_to_a_running_program_changes_nothing() {
    let mut rig = at_prompt();
    rig.mark(MarkKind::CommandStart);
    rig.t.on_foreign_write(WriteSource::Paste, rig.now);
    assert_eq!(rig.phase(), Phase::Running);
}

// ---- 第 12 行：Running，命令结束但尚无新 Prompt ----

#[test]
fn row_12_command_end_without_a_new_prompt_is_unknown_not_input() {
    let mut rig = at_prompt();
    rig.mark(MarkKind::CommandStart);
    assert_eq!(rig.phase(), Phase::Running);
    rig.mark(MarkKind::CommandEnd { exit_code: Some(1) });
    assert_eq!(rig.phase(), Phase::Unknown);
    assert!(rig
        .t
        .take_events()
        .contains(&TrackerEvent::CommandFinished { exit_code: Some(1) }));
    for _ in 0..5 {
        rig.tick(1000);
        rig.observe();
    }
    assert_eq!(
        rig.phase(),
        Phase::Unknown,
        "没有新 Prompt 不能假定可以输入"
    );
    rig.prompt();
    assert_eq!(rig.phase(), Phase::Input);
}

// ---- 第 13 行：任意，锁屏 / 广播 / 备用屏幕 / 密码交互 / 滚回历史 / 模态 ----

#[test]
fn row_13_every_restriction_suspends_cancels_requests_and_drops_predictions() {
    for kind in [
        RestrictionKind::Locked,
        RestrictionKind::Broadcast,
        RestrictionKind::PasswordPrompt,
        RestrictionKind::Modal,
        RestrictionKind::MouseReport,
    ] {
        let mut rig = at_prompt();
        let seq = rig.t.on_text("ls", IntentOrigin::Manual, rig.now).unwrap();
        rig.t.on_write_outcome(seq, WriteOutcome::Queued, rig.now);
        let id = rig.t.identity();
        rig.t.set_restriction(kind, true, rig.now);
        assert_eq!(rig.phase(), Phase::Suspended, "{kind:?}");
        assert!(!rig.t.is_current(&id), "{kind:?}");
        assert_eq!(rig.t.pending_intents().count(), 0);
        assert_eq!(rig.t.state().predicted_text, "", "不保留可自动填入的预测");
        assert!(!rig.t.can_accept());
        assert_eq!(rig.t.on_text("x", IntentOrigin::Manual, rig.now), None);
    }
}

#[test]
fn row_13_the_alternate_screen_and_scrollback_come_from_the_screen_reading() {
    let mut rig = at_prompt();
    rig.s.alt = true;
    rig.s.revision += 1;
    rig.observe();
    assert_eq!(rig.phase(), Phase::Suspended);
    assert!(rig.t.restrictions().alt_screen);

    let mut back = at_prompt();
    back.s.offset = 3;
    back.s.revision += 1;
    back.observe();
    assert_eq!(back.phase(), Phase::Suspended);
    assert!(back.t.restrictions().scrollback);
}

#[test]
fn row_13_suspension_drops_a_pending_submission() {
    let mut rig = at_prompt();
    rig.type_echoed("ls");
    rig.t.on_key(KeyClass::Enter, rig.now);
    rig.t.set_restriction(RestrictionKind::Modal, true, rig.now);
    assert_eq!(rig.t.state().submitted_command, None);
    rig.t
        .set_restriction(RestrictionKind::Modal, false, rig.now);
    rig.mark(MarkKind::CommandStart);
    assert!(rig
        .t
        .take_events()
        .iter()
        .all(|e| !matches!(e, TrackerEvent::CommandStarted { command: Some(_) })));
}

// ---- 第 14 行：Suspended，限制解除 ----

#[test]
fn row_14_lifting_a_restriction_goes_to_unknown_and_never_restores_the_old_awaiting_echo() {
    let mut rig = at_prompt();
    let seq = rig.t.on_text("ls", IntentOrigin::Manual, rig.now).unwrap();
    rig.t.on_write_outcome(seq, WriteOutcome::Queued, rig.now);
    rig.t
        .set_restriction(RestrictionKind::Locked, true, rig.now);
    rig.t
        .set_restriction(RestrictionKind::Locked, false, rig.now);
    assert_eq!(rig.phase(), Phase::Unknown);
    assert_eq!(rig.t.pending_intents().count(), 0);
    assert_eq!(rig.t.state().predicted_text, "");
}

#[test]
fn row_14_a_strict_screen_reconciliation_can_bring_it_back_without_a_new_prompt() {
    let mut rig = at_prompt();
    rig.type_echoed("ls");
    rig.t.set_restriction(RestrictionKind::Modal, true, rig.now);
    rig.t
        .set_restriction(RestrictionKind::Modal, false, rig.now);
    assert_eq!(rig.phase(), Phase::Unknown);
    rig.tick(300);
    rig.observe();
    assert_eq!(rig.phase(), Phase::Input, "锚点仍有效，屏幕能唯一读出");
    assert_eq!(rig.t.state().confirmed_text, "ls");
    assert_eq!(rig.t.state().origin, InputOrigin::Mixed);
}

#[test]
fn row_14_lifting_while_another_restriction_remains_stays_suspended() {
    let mut rig = at_prompt();
    rig.t
        .set_restriction(RestrictionKind::Locked, true, rig.now);
    rig.t
        .set_restriction(RestrictionKind::Broadcast, true, rig.now);
    rig.t
        .set_restriction(RestrictionKind::Locked, false, rig.now);
    assert_eq!(rig.phase(), Phase::Suspended);
    rig.t
        .set_restriction(RestrictionKind::Broadcast, false, rig.now);
    assert_eq!(rig.phase(), Phase::Unknown);
}

#[test]
fn row_14_a_running_command_stays_not_input_after_a_full_screen_program_exits() {
    let mut rig = at_prompt();
    rig.mark(MarkKind::CommandStart);
    rig.s.alt = true;
    rig.s.revision += 1;
    rig.observe();
    assert_eq!(rig.phase(), Phase::Suspended);
    rig.s.alt = false;
    rig.s.revision += 1;
    rig.observe();
    for _ in 0..5 {
        rig.tick(1000);
        rig.observe();
    }
    assert_eq!(rig.phase(), Phase::Unknown, "命令开始过，输入起点已经作废");
    assert_ne!(rig.phase(), Phase::Input);
}

// ---- 第 15 行：任意，断线 / 重连 / 切目标 / 集成重置 ----

#[test]
fn row_15_connection_changes_reset_everything_and_bump_the_epochs() {
    let mut rig = at_prompt();
    rig.type_echoed("ls");
    let id = rig.t.identity();
    let epoch = rig.t.state().prompt_epoch;
    let mut moved = ctx();
    moved.connection_generation = 2;
    rig.t.set_context(moved, rig.now);
    assert_eq!(rig.phase(), Phase::Unknown);
    assert!(!rig.t.is_current(&id));
    assert!(rig.t.state().prompt_epoch > epoch);
    assert_eq!(rig.t.state().confirmed_text, "");
    assert_eq!(rig.t.state().input_anchor, None);
    assert!(!rig.t.saw_prompt_marks());

    let mut other = at_prompt();
    let mut target = ctx();
    target.target = "ssh://u@other:22".into();
    other.t.set_context(target, other.now);
    assert_eq!(other.phase(), Phase::Unknown, "切目标");

    let mut pane = at_prompt();
    let mut moved = ctx();
    moved.pane_id = 9;
    pane.t.set_context(moved, pane.now);
    assert_eq!(pane.phase(), Phase::Unknown, "窗格身份变了");
}

#[test]
fn row_15_integration_reset_bumps_the_integration_epoch_but_a_reconnect_does_not() {
    let mut rig = at_prompt();
    let before = rig.t.integration_epoch();
    rig.t.on_connection_reset(rig.now);
    assert_eq!(rig.t.integration_epoch(), before);
    rig.prompt();
    let id = rig.t.identity();
    rig.t.on_integration_reset(rig.now);
    assert_eq!(rig.t.integration_epoch(), before + 1);
    assert_eq!(rig.phase(), Phase::Unknown);
    assert!(!rig.t.is_current(&id));
}

#[test]
fn row_15_only_a_broadcast_revision_change_just_expires_old_requests() {
    let mut rig = at_prompt();
    let id = rig.t.identity();
    let mut changed = ctx();
    changed.broadcast_revision = 4;
    rig.t.set_context(changed, rig.now);
    assert_eq!(rig.phase(), Phase::Input, "不是换连接");
    assert!(!rig.t.is_current(&id));
}

// ---- 其他约束 ----

#[test]
fn a_request_identity_is_current_only_while_every_field_matches_and_it_is_the_latest() {
    let mut rig = at_prompt();
    let id = rig.t.identity();
    assert!(rig.t.is_current(&id));
    let mut tampered = id.clone();
    tampered.pane_id += 1;
    assert!(!rig.t.is_current(&tampered));
    let mut tampered = id.clone();
    tampered.target_identity.push('x');
    assert!(!rig.t.is_current(&tampered));
    let mut tampered = id.clone();
    tampered.caret = ByteIdx(3);
    assert!(!rig.t.is_current(&tampered));
    let mut tampered = id.clone();
    tampered.integration_epoch += 1;
    assert!(!rig.t.is_current(&tampered));
    let newer = rig.t.identity();
    assert!(!rig.t.is_current(&id), "新请求顶掉旧请求");
    assert!(rig.t.is_current(&newer));
    let with_cwd = rig.t.identity_with_cwd();
    assert!(rig.t.is_current(&with_cwd));
    rig.mark(MarkKind::Cwd {
        host: None,
        path: "/tmp".into(),
    });
    assert!(!rig.t.is_current(&with_cwd), "目录变了，用到路径的请求作废");
}

#[test]
fn osc_7_updates_the_cwd_and_emits_one_event() {
    let mut rig = at_prompt();
    assert_eq!(rig.t.cwd(), None);
    rig.mark(MarkKind::Cwd {
        host: Some("web-1".into()),
        path: "/srv/app".into(),
    });
    assert_eq!(rig.t.cwd(), Some((Some("web-1"), "/srv/app")));
    assert!(rig.t.take_events().contains(&TrackerEvent::CwdChanged {
        host: Some("web-1".into()),
        path: "/srv/app".into()
    }));
}

#[test]
fn the_accept_key_latch_consumes_repeats_until_release_and_clears_on_blur() {
    let mut rig = at_prompt();
    rig.t.arm_accept_latch("tab");
    assert!(rig.t.latch_consumes("tab"));
    assert!(!rig.t.latch_consumes("enter"), "另一个按键不受影响");
    rig.t.release_accept_latch("enter");
    assert!(rig.t.latch_consumes("tab"), "松开别的键不清它");
    rig.t.release_accept_latch("tab");
    assert!(!rig.t.latch_consumes("tab"));
    rig.t.arm_accept_latch("enter");
    rig.t.on_focus_lost();
    assert!(!rig.t.latch_consumes("enter"), "失焦清理");
}

#[test]
fn typing_in_the_middle_of_a_line_is_still_tracked_on_the_text_before_the_cursor() {
    let mut rig = at_prompt();
    rig.type_echoed("ab");
    // 用户移动光标到 a 后面（方向键交给 shell，失同步），并由屏幕协调回来。
    rig.t
        .on_key(KeyClass::Handoff(HandoffKey::Navigation), rig.now);
    rig.s.cursor.1 -= 1;
    rig.s.revision += 1;
    rig.tick(1000);
    rig.observe();
    rig.tick(200);
    rig.observe();
    assert_eq!(rig.phase(), Phase::Input);
    assert_eq!(rig.t.state().confirmed_text, "a", "光标前文本");
    assert!(!rig.t.state().tail_blank, "光标后面有 b");
    assert!(!rig.t.can_accept(), "光标不在行尾不能填入");
    // 在这里键入 c：屏幕上 a 后面变成 c，b 被推后。
    let seq = rig.t.on_text("c", IntentOrigin::Manual, rig.now).unwrap();
    rig.s.rows[0][3] = InputCell::plain('c');
    rig.s.rows[0][4] = InputCell::plain('b');
    rig.s.cursor.1 += 1;
    rig.s.revision += 1;
    rig.observe();
    rig.t.on_write_outcome(seq, WriteOutcome::Queued, rig.now);
    assert_eq!(rig.phase(), Phase::Input);
    assert_eq!(rig.t.state().confirmed_text, "ac");
    assert_eq!(rig.s.text(0).trim_end(), "$ acb");
}

#[test]
fn an_unexpected_change_in_input_state_desyncs_then_recovers_when_the_screen_settles() {
    let mut rig = at_prompt();
    rig.type_echoed("ab");
    // 后台任务往输入行里打印了东西。
    rig.s.cursor.1 = 2;
    rig.s.put("XY");
    rig.s.cursor.1 = 6;
    rig.observe();
    assert_eq!(rig.phase(), Phase::Desynced);
    assert_eq!(
        rig.t.state().desync_reason,
        Some(DesyncReason::UnexpectedChange)
    );
}

#[test]
fn the_tracker_cap_is_4096_utf8_bytes_and_wide_text_counts_bytes() {
    let mut rig = at_prompt();
    // 1365 个汉字 = 4095 字节：还可以；再来一个汉字 = 4098 字节：超限。
    let text: String = std::iter::repeat_n('你', 1365).collect();
    let seq = rig
        .t
        .on_text(&text, IntentOrigin::ImeCommit, rig.now)
        .unwrap();
    rig.t.on_write_outcome(seq, WriteOutcome::Queued, rig.now);
    assert_eq!(rig.t.on_text("你", IntentOrigin::ImeCommit, rig.now), None);
    assert_eq!(rig.t.state().desync_reason, Some(DesyncReason::TooLong));
}

#[test]
fn an_unregistered_key_that_never_left_invalidates_what_was_inferred_from_it() {
    // Enter 没发出去（队列满）：待提交快照作废，输入行是否还在不知道。
    let mut rig = at_prompt();
    rig.type_echoed("rm x");
    rig.t.on_key(KeyClass::Enter, rig.now);
    assert!(rig.t.state().submitted_command.is_some());
    rig.t
        .on_unsent_write(WriteOutcome::Rejected(RejectReason::QueueFull), rig.now);
    assert_eq!(rig.phase(), Phase::Desynced);
    assert_eq!(rig.t.state().submitted_command, None);
    // 成功排队的不改变状态。
    let mut ok = at_prompt();
    ok.t.on_unsent_write(WriteOutcome::Queued, ok.now);
    assert_eq!(ok.phase(), Phase::Input);
    // 传输失败同理。
    let mut failed = at_prompt();
    failed
        .t
        .on_key(KeyClass::Handoff(HandoffKey::Tab), failed.now);
    failed
        .t
        .on_unsent_write(WriteOutcome::TransportFailed, failed.now);
    assert_eq!(failed.phase(), Phase::Desynced);
}

#[test]
fn a_geometry_change_forgets_the_anchor_and_waits_for_a_new_prompt() {
    let mut rig = at_prompt();
    rig.type_echoed("ls");
    rig.t.on_geometry_changed(rig.now);
    assert_eq!(rig.phase(), Phase::Unknown);
    assert_eq!(rig.t.state().input_anchor, None);
    for _ in 0..5 {
        rig.tick(1000);
        rig.observe();
    }
    assert_eq!(
        rig.phase(),
        Phase::Unknown,
        "旧锚点的行列号可能已经不对应原来的格子"
    );
    rig.prompt();
    assert_eq!(rig.phase(), Phase::Input);

    let mut running = at_prompt();
    running.mark(MarkKind::CommandStart);
    running.t.on_geometry_changed(running.now);
    assert_eq!(running.phase(), Phase::Running, "执行中的命令不受影响");
}

#[test]
fn events_do_not_grow_without_bound() {
    let mut rig = at_prompt();
    for _ in 0..1000 {
        rig.mark(MarkKind::PromptStart);
        rig.mark(MarkKind::PromptEnd);
        rig.mark(MarkKind::CommandStart);
    }
    assert!(rig.t.take_events().len() <= 512);
}
