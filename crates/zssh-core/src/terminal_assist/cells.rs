//! 从终端单元格里读出"输入行"。
//!
//! 要点（都是因为这里不能出错）：
//! - 读的是**单元格**，不是裁过尾部空格的行文本：`ls ` 末尾那个空格是输入的一部分；
//! - 宽字符占两格（本体 + 右侧占位格），占位格不产生文字；组合字符跟在基础字符后面，算同一个单元；
//! - 只读光标所在的那一行，成本是列数，不扫整屏；
//! - 只取输入起点到光标之间的文字（"光标前文本"）。光标后面的东西（zsh 的右侧提示、行中编辑留下的
//!   尾巴）是不是输入无法确定，只记"尾部是否为空白"。

use super::text_index::{ByteIdx, TermCol};

/// 输入跟踪最多处理的 UTF-8 字节数（超过就不再跟踪这一行）。
pub const MAX_TRACKED_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellKind {
    Normal,
    /// 宽字符本体（右边紧跟一个 `WideSpacer`）。
    Wide,
    /// 宽字符右侧的占位格：不产生文字。
    WideSpacer,
    /// 行尾放不下宽字符时的占位格：说明这一行是软换行的前半截。
    LeadingSpacer,
}

/// 一个终端单元格里和输入有关的部分。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputCell {
    pub ch: char,
    /// 跟在基础字符后面的组合字符。
    pub extra: Option<Box<[char]>>,
    pub kind: CellKind,
}

impl InputCell {
    pub fn plain(ch: char) -> Self {
        Self {
            ch,
            extra: None,
            kind: CellKind::Normal,
        }
    }

    fn is_blank(&self) -> bool {
        matches!(self.ch, ' ' | '\0') && self.extra.is_none()
    }
}

/// 读不出输入行的原因。每一种都意味着"不能确定"，调用方应该失同步而不是猜。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// 光标跑到了输入起点的左边（提示符被改写、屏幕清了）。
    CursorBeforeAnchor,
    /// 光标或输入起点越过了这一行的格子数。
    OutOfRange,
    /// 光标落在宽字符的占位格上。
    CursorInsideWide,
    /// 输入起点到光标之间有软换行的占位格：输入跨了物理行。
    Wrapped,
    /// 光标在最后一列且那一格有字：自动换行挂起，无法区分"光标在字后"还是"字压着光标"。
    PendingWrap,
    /// 超过 [`MAX_TRACKED_BYTES`]。
    TooLong,
}

/// 读出来的输入行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputLine {
    /// 输入起点到光标之间的文字，保留尾部空格。
    pub before_cursor: String,
    /// 光标位置，等于 `before_cursor.len()`（光标前文本的末尾）。
    pub cursor: ByteIdx,
    /// 光标之后（含光标格）到行尾是否全是空白。不是的话，光标不在行尾（或者那是右侧提示），
    /// 可以确认已输入的文字，但不能往里"补全"。
    pub tail_blank: bool,
}

/// 从一行单元格里读输入。`anchor` 是输入起点列（收到 `133;B` 时的光标列），`cursor` 是现在的光标列。
pub fn read_input_line(
    cells: &[InputCell],
    anchor: TermCol,
    cursor: TermCol,
) -> Result<InputLine, ReadError> {
    if cursor.0 < anchor.0 {
        return Err(ReadError::CursorBeforeAnchor);
    }
    if anchor.0 > cells.len() || cursor.0 > cells.len() {
        return Err(ReadError::OutOfRange);
    }
    if cells
        .get(cursor.0)
        .is_some_and(|cell| cell.kind == CellKind::WideSpacer)
    {
        return Err(ReadError::CursorInsideWide);
    }
    // 自动换行挂起：光标停在最后一列，而那一格已经有字。
    if cursor.0 + 1 == cells.len()
        && cells
            .get(cursor.0)
            .is_some_and(|cell| !cell.is_blank() && cell.kind == CellKind::Normal)
    {
        return Err(ReadError::PendingWrap);
    }

    let mut text = String::new();
    for cell in &cells[anchor.0..cursor.0] {
        match cell.kind {
            CellKind::LeadingSpacer => return Err(ReadError::Wrapped),
            CellKind::WideSpacer => continue,
            CellKind::Normal | CellKind::Wide => {}
        }
        text.push(if cell.ch == '\0' { ' ' } else { cell.ch });
        if let Some(extra) = &cell.extra {
            text.extend(extra.iter());
        }
        if text.len() > MAX_TRACKED_BYTES {
            return Err(ReadError::TooLong);
        }
    }
    // 光标前文本如果以占位格结束（光标正好紧跟宽字符本体之后）没问题；
    // 但本体被光标切开（光标在本体上、本体已经被读走）不会发生：光标格是 Wide 时本体不在范围内。
    let tail_blank = cells[cursor.0.min(cells.len())..]
        .iter()
        .all(|cell| cell.is_blank() || cell.kind == CellKind::WideSpacer);
    Ok(InputLine {
        cursor: ByteIdx(text.len()),
        before_cursor: text,
        tail_blank,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(text: &str, cols: usize) -> Vec<InputCell> {
        let mut cells = Vec::new();
        for ch in text.chars() {
            let wide = unicode_wide(ch);
            if ch == '\u{301}' {
                if let Some(last) = cells.last_mut() {
                    let last: &mut InputCell = last;
                    last.extra = Some(vec![ch].into_boxed_slice());
                }
                continue;
            }
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
        while cells.len() < cols {
            cells.push(InputCell::plain(' '));
        }
        cells
    }

    fn unicode_wide(ch: char) -> bool {
        ('\u{4e00}'..='\u{9fff}').contains(&ch)
    }

    #[test]
    fn reads_the_text_between_the_anchor_and_the_cursor_keeping_trailing_spaces() {
        let cells = row("$ ls  ", 20);
        let line = read_input_line(&cells, TermCol(2), TermCol(6)).expect("读得出");
        assert_eq!(line.before_cursor, "ls  ");
        assert_eq!(line.cursor, ByteIdx(4));
        assert!(line.tail_blank);
    }

    #[test]
    fn an_empty_input_is_the_empty_string() {
        let cells = row("$ ", 10);
        let line = read_input_line(&cells, TermCol(2), TermCol(2)).expect("读得出");
        assert_eq!(line.before_cursor, "");
        assert_eq!(line.cursor, ByteIdx(0));
    }

    #[test]
    fn wide_characters_count_two_columns_but_one_character_and_three_bytes() {
        let cells = row("$ echo 你好", 30);
        // 提示符 2 列 + "echo " 5 列 + 两个汉字 4 列 = 光标在第 11 列。
        let line = read_input_line(&cells, TermCol(2), TermCol(11)).expect("读得出");
        assert_eq!(line.before_cursor, "echo 你好");
        assert_eq!(line.cursor, ByteIdx("echo 你好".len()));
        assert_eq!(ByteIdx("echo 你好".len()).0, 5 + 6, "字节数不等于列数");
        // 光标落在汉字本体和占位格之间的列（占位格上）读不了。
        assert_eq!(
            read_input_line(&cells, TermCol(2), TermCol(8)),
            Err(ReadError::CursorInsideWide)
        );
    }

    #[test]
    fn combining_marks_stay_with_their_base_character() {
        let cells = row("$ e\u{301}x", 12);
        let line = read_input_line(&cells, TermCol(2), TermCol(4)).expect("读得出");
        assert_eq!(line.before_cursor, "e\u{301}x");
    }

    #[test]
    fn text_after_the_cursor_is_not_input_but_is_reported() {
        let cells = row("$ ls -la    [12:00]", 30);
        let line = read_input_line(&cells, TermCol(2), TermCol(7)).expect("读得出");
        assert_eq!(line.before_cursor, "ls -l");
        assert!(
            !line.tail_blank,
            "光标后面还有字：可能是行中编辑，也可能是右侧提示"
        );
    }

    #[test]
    fn a_cursor_left_of_the_anchor_or_off_the_row_is_unreadable() {
        let cells = row("$ ls", 10);
        assert_eq!(
            read_input_line(&cells, TermCol(2), TermCol(1)),
            Err(ReadError::CursorBeforeAnchor)
        );
        assert_eq!(
            read_input_line(&cells, TermCol(2), TermCol(11)),
            Err(ReadError::OutOfRange)
        );
        assert_eq!(
            read_input_line(&cells, TermCol(11), TermCol(11)),
            Err(ReadError::OutOfRange)
        );
    }

    #[test]
    fn a_soft_wrapped_row_is_refused() {
        let mut cells = row("$ aaaaaaa", 10);
        cells[9] = InputCell {
            ch: ' ',
            extra: None,
            kind: CellKind::LeadingSpacer,
        };
        assert_eq!(
            read_input_line(&cells, TermCol(2), TermCol(10)),
            Err(ReadError::Wrapped)
        );
    }

    #[test]
    fn a_pending_wrap_at_the_last_column_is_refused() {
        let cells = row("$ aaaaaaaa", 10);
        assert_eq!(
            read_input_line(&cells, TermCol(2), TermCol(9)),
            Err(ReadError::PendingWrap)
        );
    }

    #[test]
    fn input_longer_than_the_tracking_limit_is_refused() {
        let cells: Vec<InputCell> =
            std::iter::repeat_n(InputCell::plain('a'), MAX_TRACKED_BYTES + 10).collect();
        assert_eq!(
            read_input_line(&cells, TermCol(0), TermCol(MAX_TRACKED_BYTES + 5)),
            Err(ReadError::TooLong)
        );
        let ok = read_input_line(&cells, TermCol(0), TermCol(MAX_TRACKED_BYTES)).expect("刚好");
        assert_eq!(ok.before_cursor.len(), MAX_TRACKED_BYTES);
    }
}
