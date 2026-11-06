//! 双击选词、三击选行。
//!
//! 只做纯函数：给定一行快照和列号，返回要选中的列范围。宽字符的占位格
//! 跟随前一个 cell，复制时由 `text_between` 跳过。

use zssh_terminal::{CellSnapshot, RowSnapshot};

/// 词字符：不是分隔符的都算。分隔符和 VS Code / xterm.js 系终端（Tabby、Hyper 等）的默认一致：
/// 空白，以及 `( ) [ ] { } ' , " `` ` `` ─ ‘ ’ “ ” |`；`< >`、`;`、`=`、`+`、`!`、`#` 等都不是分隔符。
/// 这样 `user@host:/tmp/file.txt`、`key=value`、带符号的密码 / 令牌（`X4sLk+1k6=/abc!#`）都能整段选中，
/// `(abc)`、`"abc",` 只选 `abc`。
///
/// 全角 / CJK 标点（`，`、`：`、`（`、`）`、`。`……）也断开：它们不是 ASCII，上面那份默认清单没列，但
/// 中文输出里把它们和前后的词连在一起没有任何用处。
fn is_word_char(ch: char) -> bool {
    if ch.is_whitespace() || ch.is_control() {
        return false;
    }
    if matches!(
        ch,
        '(' | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '\''
            | ','
            | '"'
            | '`'
            | '─'
            | '‘'
            | '’'
            | '“'
            | '”'
            | '|'
    ) {
        return false;
    }
    !is_cjk_punctuation(ch)
}

/// CJK 符号和标点（U+3000–303F）、全角形式里的标点（U+FF01–FF0F、FF1A–FF20、FF3B–FF40、FF5B–FF65）。
/// 全角字母和数字（U+FF10–FF19、FF21–FF3A、FF41–FF5A）仍是词字符。
fn is_cjk_punctuation(ch: char) -> bool {
    matches!(
        ch,
        '\u{3000}'..='\u{303f}'
            | '\u{ff01}'..='\u{ff0f}'
            | '\u{ff1a}'..='\u{ff20}'
            | '\u{ff3b}'..='\u{ff40}'
            | '\u{ff5b}'..='\u{ff65}'
    )
}

/// 词尾的句末标点（`ping 10.0.0.1.`、`error:`）：词里有别的字符时从选区里去掉，
/// 双击 IP 不该把句号一起带走。
fn is_trailing_punct(ch: char) -> bool {
    matches!(ch, '.' | ':')
}

/// 占位格（宽字符的右半）不是任何字，点在它上面等于点在前一个 cell。
fn leading_col(row: &RowSnapshot, col: usize) -> Option<usize> {
    let mut col = col;
    while row.cells.get(col)?.spacer {
        col = col.checked_sub(1)?;
    }
    Some(col)
}

fn is_word_cell(cell: &CellSnapshot) -> bool {
    cell.spacer || is_word_char(cell.ch)
}

/// 双击：返回光标所在词的列范围；空白或符号上返回 `None`。
pub fn word_span(row: &RowSnapshot, col: usize) -> Option<(usize, usize)> {
    let col = leading_col(row, col)?;
    if !is_word_char(row.cells.get(col)?.ch) {
        return None;
    }

    let mut start = col;
    while start > 0 && is_word_cell(&row.cells[start - 1]) {
        start -= 1;
    }
    // 起点不能落在占位格上（宽字符的右半）。
    while row.cells[start].spacer && start < col {
        start += 1;
    }

    let mut end = col;
    while end + 1 < row.cells.len() && is_word_cell(&row.cells[end + 1]) {
        end += 1;
    }

    // 去掉句末标点，但不能把被点的那一格去掉（点的就是 `:` 本身时原样保留）。
    let floor = col.max(start);
    let mut trimmed = end;
    let mut peeled = false;
    while trimmed > floor {
        let cell = &row.cells[trimmed];
        if is_trailing_punct(cell.ch) && !cell.spacer {
            peeled = true;
        } else if !cell.spacer {
            break;
        }
        trimmed -= 1;
    }
    if peeled {
        end = trimmed;
    }
    Some((start, end))
}

/// 三击：整行。
pub fn line_span(row: &RowSnapshot) -> (usize, usize) {
    (0, row.cells.len().saturating_sub(1))
}

/// 左键按下一次该做什么：`click_count` 是系统给的连击次数（1、2、3……，一直累加）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PressAction {
    /// 选中这一段列范围（双击词 / 三击行）；不开始拖选。
    Select((usize, usize)),
    /// 单击（或双击落在空白 / 符号上）：清掉旧选区，只记锚点，拖出这一格才成为选区。
    Anchor,
}

pub fn press_action(row: &RowSnapshot, col: usize, click_count: usize) -> PressAction {
    let span = match click_count {
        0 | 1 => None,
        2 => word_span(row, col),
        _ => Some(line_span(row)),
    };
    span.map_or(PressAction::Anchor, PressAction::Select)
}

/// Ctrl+C / Ctrl+Shift+C 在终端里该做什么（和 0.2.14 前端的 `terminalKeys.ts` 一致）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyKey {
    /// 有非空选区：复制并清掉选区。
    Copy,
    /// 没有选区的 Ctrl+C：把 ^C 发给远端，用来中断命令。
    Interrupt,
    /// 没有选区的 Ctrl+Shift+C：用户想要的是复制不是中断，什么都不发。
    Swallow,
}

/// `selected_text` 是当前选区的文字（没有选区传空串）；空白格也会得到空串，
/// 这种选区复制不出东西，不能把 ^C 吞掉。
pub fn copy_key_action(selected_text: &str, shift: bool) -> CopyKey {
    if !selected_text.is_empty() {
        CopyKey::Copy
    } else if shift {
        CopyKey::Swallow
    } else {
        CopyKey::Interrupt
    }
}

/// 拖选的选区：按下的格子是锚点，指针还停在锚点格就不算选区，
/// 单击不能留下一个"空选区"（它会让之后的 Ctrl+C 永远走复制）。
pub fn drag_selection(
    anchor: (usize, usize),
    head: (usize, usize),
) -> Option<((usize, usize), (usize, usize))> {
    (anchor != head).then_some((anchor, head))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zssh_terminal::{Size, Snapshot, TerminalModel};

    fn snapshot(text: &str) -> Snapshot {
        let mut term = TerminalModel::new(Size { rows: 2, cols: 80 }, 10);
        term.feed(text.as_bytes());
        term.snapshot()
    }

    #[test]
    fn selects_a_word() {
        let snap = snapshot("hello world");
        assert_eq!(word_span(&snap.rows[0], 7), Some((6, 10)));
        assert_eq!(word_span(&snap.rows[0], 0), Some((0, 4)));
    }

    #[test]
    fn selects_paths_and_urls_as_one_word() {
        let snap = snapshot("open /tmp/file.txt now");
        assert_eq!(word_span(&snap.rows[0], 7), Some((5, 17)));
        let snap = snapshot("see https://example.com/a now");
        assert_eq!(word_span(&snap.rows[0], 10), Some((4, 24)));
    }

    fn word_at(text: &str, needle: &str) -> Option<String> {
        let snap = snapshot(text);
        let row = &snap.rows[0];
        let byte = row.text.find(needle)?;
        // 行文字不含占位格，列号要按单元格数一遍。
        let col = row
            .text
            .char_indices()
            .take_while(|(i, _)| *i < byte)
            .map(|(_, ch)| if wide(ch) { 2 } else { 1 })
            .sum::<usize>();
        let (start, end) = word_span(row, col)?;
        Some(
            row.cells[start..=end]
                .iter()
                .filter(|cell| !cell.spacer)
                .map(|cell| cell.ch)
                .collect(),
        )
    }

    fn wide(ch: char) -> bool {
        ('\u{1100}'..='\u{115f}').contains(&ch)
            || ('\u{2e80}'..='\u{a4cf}').contains(&ch)
            || ('\u{ac00}'..='\u{d7a3}').contains(&ch)
            || ('\u{ff00}'..='\u{ff60}').contains(&ch)
    }

    #[test]
    fn ips_hostnames_and_emails_are_one_word() {
        for (text, needle, want) in [
            ("ping 192.0.2.17 now", "192.0.2.17", "192.0.2.17"),
            ("[root@10-7-46-164 ~]#", "10-7", "root@10-7-46-164"),
            ("10.0.0.1:22 open", "10.0.0.1", "10.0.0.1:22"),
            (
                "mail user@web-1.example.org now",
                "web-1",
                "user@web-1.example.org",
            ),
            ("cd ~/work/app now", "work", "~/work/app"),
            ("a_b-c.d now", "b-c", "a_b-c.d"),
        ] {
            assert_eq!(word_at(text, needle).as_deref(), Some(want), "{text}");
        }
    }

    #[test]
    fn quotes_brackets_commas_and_pipes_break_words_like_other_terminals() {
        assert_eq!(word_at("x=\"v1\" y", "v1").as_deref(), Some("v1"));
        assert_eq!(word_at("f(a, b)", "a,").as_deref(), Some("a"));
        assert_eq!(word_at("[abc] d", "abc").as_deref(), Some("abc"));
        assert_eq!(word_at("(abc)", "abc").as_deref(), Some("abc"));
        assert_eq!(word_at("\"abc\",", "abc").as_deref(), Some("abc"));
        assert_eq!(word_at("{abc}", "abc").as_deref(), Some("abc"));
        assert_eq!(word_at("`abc` x", "abc").as_deref(), Some("abc"));
        assert_eq!(word_at("'abc'", "abc").as_deref(), Some("abc"));
        assert_eq!(word_at("“abc” x", "abc").as_deref(), Some("abc"));
        assert_eq!(word_at("a|b", "b").as_deref(), Some("b"));
        assert_eq!(word_at("─abc─", "abc").as_deref(), Some("abc"));
    }

    #[test]
    fn symbols_that_are_not_separators_stay_inside_the_word() {
        // 带符号的密码 / 令牌整段选中，点在中间或两端都一样。
        for needle in ["X4sLk", "+1k6", "=/abc", "!#"] {
            assert_eq!(
                word_at("pw X4sLk+1k6=/abc!# end", needle).as_deref(),
                Some("X4sLk+1k6=/abc!#"),
                "{needle}"
            );
        }
        assert_eq!(
            word_at("set key=value now", "key").as_deref(),
            Some("key=value")
        );
        assert_eq!(
            word_at("set key=value now", "value").as_deref(),
            Some("key=value")
        );
        // `<`、`>`、`;` 不是分隔符（和 VS Code 默认一致）。
        assert_eq!(word_at("a;b c", "b").as_deref(), Some("a;b"));
        assert_eq!(word_at("<tag> c", "tag").as_deref(), Some("<tag>"));
        assert_eq!(word_at("a&&b c", "b").as_deref(), Some("a&&b"));
        // 句末的 . 和 : 仍然剥掉。
        assert_eq!(
            word_at("pw X4sLk+1k6=/abc!#. ok", "X4sLk").as_deref(),
            Some("X4sLk+1k6=/abc!#")
        );
    }

    #[test]
    fn a_trailing_period_or_colon_is_not_part_of_the_word() {
        assert_eq!(
            word_at("ping 10.0.0.1. ok", "10.0").as_deref(),
            Some("10.0.0.1")
        );
        assert_eq!(word_at("error: boom", "error").as_deref(), Some("error"));
        // 点的就是标点本身：不能把被点的格子去掉。
        let snap = snapshot("end.");
        assert_eq!(word_span(&snap.rows[0], 3), Some((0, 3)));
    }

    #[test]
    fn full_width_text_and_punctuation() {
        // 中文连成一词，全角标点断开。
        assert_eq!(word_at("你好，world", "你好").as_deref(), Some("你好"));
        assert_eq!(word_at("你好，world", "world").as_deref(), Some("world"));
        assert_eq!(word_at("（abc）", "abc").as_deref(), Some("abc"));
        // 全角冒号后面的词不会把占位格带进起点。
        let snap = snapshot("名称：host-1");
        let row = &snap.rows[0];
        let (start, end) = word_span(row, 8).expect("host-1 是一个词");
        assert!(!row.cells[start].spacer, "起点落在占位格上");
        let text: String = row.cells[start..=end]
            .iter()
            .filter(|c| !c.spacer)
            .map(|c| c.ch)
            .collect();
        assert_eq!(text, "host-1");
    }

    #[test]
    fn clicking_the_right_half_of_a_wide_char_still_selects_the_word() {
        let snap = snapshot("中文 ab");
        let row = &snap.rows[0];
        assert!(row.cells[1].spacer);
        assert_eq!(word_span(row, 1), Some((0, 3)));
        assert_eq!(word_span(row, 0), Some((0, 3)));
        // 全角标点的右半不是词。
        let snap = snapshot("a：b");
        assert_eq!(word_span(&snap.rows[0], 2), None);
    }

    #[test]
    fn successive_presses_double_then_triple() {
        let snap = snapshot("run 192.0.2.17 now");
        let row = &snap.rows[0];
        // 同一位置连按：1 单击只记锚点；2 选词；3 及以后选整行（系统的连击数一直累加）。
        assert_eq!(press_action(row, 8, 1), PressAction::Anchor);
        assert_eq!(press_action(row, 8, 2), PressAction::Select((4, 13)));
        assert_eq!(press_action(row, 8, 3), PressAction::Select((0, 79)));
        assert_eq!(press_action(row, 8, 4), PressAction::Select((0, 79)));
        // 双击落在空格上：和单击一样（清选区、记锚点），不残留上一次的选区。
        assert_eq!(press_action(row, 3, 2), PressAction::Anchor);
        // 三击落在空白上也是整行。
        assert_eq!(press_action(row, 40, 3), PressAction::Select((0, 79)));
    }

    #[test]
    fn whitespace_and_symbols_have_no_word() {
        let snap = snapshot("a   b");
        assert_eq!(word_span(&snap.rows[0], 1), None);
    }

    #[test]
    fn cjk_run_is_one_word() {
        let snap = snapshot("中文输出 test");
        assert_eq!(word_span(&snap.rows[0], 2), Some((0, 7)));
    }

    #[test]
    fn line_span_covers_the_grid() {
        let snap = snapshot("abc");
        assert_eq!(line_span(&snap.rows[0]), (0, 79));
    }

    #[test]
    fn ctrl_c_copies_only_a_non_empty_selection() {
        assert_eq!(copy_key_action("abc", false), CopyKey::Copy);
        assert_eq!(copy_key_action("abc", true), CopyKey::Copy);
        assert_eq!(copy_key_action("", false), CopyKey::Interrupt);
        assert_eq!(copy_key_action("", true), CopyKey::Swallow);
    }

    #[test]
    fn a_click_without_drag_is_not_a_selection() {
        assert_eq!(drag_selection((3, 4), (3, 4)), None);
        assert_eq!(drag_selection((3, 4), (3, 9)), Some(((3, 4), (3, 9))));
    }
}
