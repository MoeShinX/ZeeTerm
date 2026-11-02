//! 双击选词、三击选行。
//!
//! 只做纯函数：给定一行快照和列号，返回要选中的列范围。宽字符的占位格
//! 跟随前一个 cell，复制时由 `text_between` 跳过。

use zssh_terminal::RowSnapshot;

/// 词字符：字母、数字、下划线、连字符、点、斜杠、冒号、@。
/// 这样 `user@host:/tmp/file.txt`、URL、路径都能整段选中。
fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | ':' | '@')
}

/// 双击：返回光标所在词的列范围；空白或符号上返回 `None`。
pub fn word_span(row: &RowSnapshot, col: usize) -> Option<(usize, usize)> {
    let cell = row.cells.get(col)?;
    if cell.spacer || !is_word_char(cell.ch) {
        return None;
    }

    let mut start = col;
    while start > 0 {
        let previous = &row.cells[start - 1];
        if previous.spacer {
            start -= 1;
            continue;
        }
        if is_word_char(previous.ch) {
            start -= 1;
        } else {
            break;
        }
    }

    let mut end = col;
    while end + 1 < row.cells.len() {
        let next = &row.cells[end + 1];
        if next.spacer {
            end += 1;
            continue;
        }
        if is_word_char(next.ch) {
            end += 1;
        } else {
            break;
        }
    }
    Some((start, end))
}

/// 三击：整行。
pub fn line_span(row: &RowSnapshot) -> (usize, usize) {
    (0, row.cells.len().saturating_sub(1))
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
