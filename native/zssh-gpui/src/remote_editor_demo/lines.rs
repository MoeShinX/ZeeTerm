//! 按行改文本的编辑命令：切换注释、上下移动行、复制行、删除行、选中行、下方插入空行。
//!
//! gpui-kit 0.8.1 的 Editor 没有这几条（CodeMirror 的 `defaultKeymap` 里有），所以在这里补最小的一份：
//! 全是「文本 + 选区 → 一次替换」的纯函数，不碰 GPUI，单测能直接验行为；界面那边把返回的
//! [`Edit`] 套到编辑器上（一次替换 = 一步撤销）。偏移都是 UTF-8 字节偏移，和编辑器的 API 一致。
//!
//! 行的算法照 CodeMirror：选区覆盖的行 = 选区首尾所在的行；选区非空且结束在某一行行首时，
//! 那一行不算（比如选中前两行再多包含一个换行，不会把第三行也带上）。

use std::ops::Range;

use super::language::Comment;

/// 一次替换：把旧文本的 `range` 换成 `insert`，之后选区设为 `select`（新文本里的偏移）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub range: Range<usize>,
    pub insert: String,
    pub select: Range<usize>,
}

/// 该行行首的偏移（`offset` 所在的行）。
fn line_start(text: &str, offset: usize) -> usize {
    text[..offset].rfind('\n').map_or(0, |at| at + 1)
}

/// 该行行尾的偏移（不含换行符）。
fn line_end(text: &str, offset: usize) -> usize {
    text[offset..]
        .find('\n')
        .map_or(text.len(), |at| offset + at)
}

/// 选区覆盖的整行范围：`[第一行行首, 最后一行行尾)`，不含最后的换行符。
fn covered(text: &str, sel: &Range<usize>) -> Range<usize> {
    let start = line_start(text, sel.start.min(text.len()));
    let mut end = sel.end.min(text.len());
    // 选区非空且结束在行首：那一行没被选到。
    if end > sel.start && end > 0 && text.as_bytes()[end - 1] == b'\n' {
        end -= 1;
    }
    start..line_end(text, end.max(start))
}

/// 一行开头的空白（空格 / 制表符）字节数。
fn indent_len(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

/// 把选区的两端按「若干处 (位置, 删掉几字节, 插入几字节)」换算到新文本里。
fn map_offset(offset: usize, changes: &[(usize, usize, usize)]) -> usize {
    let mut shifted = offset as isize;
    for &(at, removed, inserted) in changes {
        if offset <= at {
            continue;
        }
        let removed_before_offset = removed.min(offset - at);
        // 插入发生在 offset 之前就整段算数；删除只算落在 offset 之前的那一部分。
        shifted += inserted as isize - removed_before_offset as isize;
    }
    shifted.max(0) as usize
}

/// Ctrl+/：切换注释。没有可操作的内容（JSON、全是空行）返回 `None`。
pub fn toggle_comment(text: &str, sel: &Range<usize>, comment: Comment) -> Option<Edit> {
    match comment {
        Comment::None => None,
        Comment::Line(token) => toggle_line_comment(text, sel, token),
        Comment::Block(open, close) => toggle_block_comment(text, sel, open, close),
    }
}

fn toggle_line_comment(text: &str, sel: &Range<usize>, token: &str) -> Option<Edit> {
    let block = covered(text, sel);
    let lines: Vec<(usize, &str)> = {
        let mut out = Vec::new();
        let mut at = block.start;
        for line in text[block.clone()].split('\n') {
            out.push((at, line));
            at += line.len() + 1;
        }
        out
    };
    let mut content = lines.iter().filter(|(_, line)| !line.trim().is_empty());
    content.next()?;
    let all_commented = lines
        .iter()
        .filter(|(_, line)| !line.trim().is_empty())
        .all(|(_, line)| line.trim_start_matches([' ', '\t']).starts_with(token));

    // 位置在旧文本里，按从前到后排好；(位置, 删掉几字节, 插入几字节)。
    let mut changes: Vec<(usize, usize, usize)> = Vec::new();
    let mut new_block = String::new();
    if all_commented {
        for (index, (start, line)) in lines.iter().enumerate() {
            if index > 0 {
                new_block.push('\n');
            }
            if line.trim().is_empty() {
                new_block.push_str(line);
                continue;
            }
            let indent = indent_len(line);
            let after = &line[indent + token.len()..];
            // 注释符号后面跟着的那一个空格一起去掉（加注释时加的就是它）。
            let space = usize::from(after.starts_with(' '));
            new_block.push_str(&line[..indent]);
            new_block.push_str(&after[space..]);
            changes.push((start + indent, token.len() + space, 0));
        }
    } else {
        // 每一行在自己的缩进处加（CodeMirror 的 toggleLineComment 就是这样：不对齐到最浅的缩进）。
        let marker = format!("{token} ");
        for (index, (start, line)) in lines.iter().enumerate() {
            if index > 0 {
                new_block.push('\n');
            }
            if line.trim().is_empty() {
                new_block.push_str(line);
                continue;
            }
            let column = indent_len(line);
            new_block.push_str(&line[..column]);
            new_block.push_str(&marker);
            new_block.push_str(&line[column..]);
            changes.push((start + column, 0, marker.len()));
        }
    }
    let select = map_offset(sel.start, &changes)..map_offset(sel.end, &changes);
    Some(Edit {
        range: block,
        insert: new_block,
        select,
    })
}

fn toggle_block_comment(text: &str, sel: &Range<usize>, open: &str, close: &str) -> Option<Edit> {
    // 选区非空就包选区；光标则包当前行（去掉行首缩进和行尾空白）。
    let target = if sel.start == sel.end {
        let start = line_start(text, sel.start);
        let end = line_end(text, sel.start);
        let line = &text[start..end];
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        let from = start + indent_len(line);
        let to = from + line[indent_len(line)..].trim_end().len();
        from..to
    } else {
        sel.start..sel.end
    };
    let inner = &text[target.clone()];
    let trimmed = inner.trim();
    if trimmed.len() >= open.len() + close.len()
        && trimmed.starts_with(open)
        && trimmed.ends_with(close)
    {
        // 去掉：开头符号和后面一个空格、结尾符号和前面一个空格。
        let lead = inner.len() - inner.trim_start().len();
        let after_open = &inner[lead + open.len()..];
        let space_after = usize::from(after_open.starts_with(' '));
        let tail = inner.trim_end();
        let before_close = &tail[..tail.len() - close.len()];
        let space_before = usize::from(
            before_close.ends_with(' ') && before_close.len() > lead + open.len() + space_after,
        );
        let body = &inner[lead + open.len() + space_after..tail.len() - close.len() - space_before];
        let insert = format!("{}{}{}", &inner[..lead], body, &inner[tail.len()..]);
        let end = target.start + insert.len();
        return Some(Edit {
            range: target.clone(),
            select: target.start..end,
            insert,
        });
    }
    let insert = format!("{open} {inner} {close}");
    let end = target.start + insert.len();
    Some(Edit {
        range: target.clone(),
        select: target.start..end,
        insert,
    })
}

/// Alt+↑ / Alt+↓：把选区覆盖的行整体往上 / 下挪一行。到头了返回 `None`。
pub fn move_lines(text: &str, sel: &Range<usize>, up: bool) -> Option<Edit> {
    let block = covered(text, sel);
    let body = &text[block.clone()];
    if up {
        if block.start == 0 {
            return None;
        }
        let prev_start = line_start(text, block.start - 1);
        let prev = &text[prev_start..block.start - 1];
        let shift = prev.len() + 1;
        Some(Edit {
            range: prev_start..block.end,
            insert: format!("{body}\n{prev}"),
            select: (sel.start - shift)..(sel.end - shift),
        })
    } else {
        if block.end >= text.len() {
            return None;
        }
        let next_start = block.end + 1;
        let next_end = line_end(text, next_start);
        let next = &text[next_start..next_end];
        let shift = next.len() + 1;
        Some(Edit {
            range: block.start..next_end,
            insert: format!("{next}\n{body}"),
            select: (sel.start + shift)..(sel.end + shift),
        })
    }
}

/// Shift+Alt+↑ / ↓：把选区覆盖的行复制一份放在上 / 下方。选区留在「新出现」的那份对应的位置：
/// 复制到上方时选区原地不动（落在上面那份），复制到下方时选区跟到下面那份。
pub fn copy_lines(text: &str, sel: &Range<usize>, up: bool) -> Edit {
    let block = covered(text, sel);
    let body = &text[block.clone()];
    if up {
        Edit {
            range: block.start..block.start,
            insert: format!("{body}\n"),
            select: sel.clone(),
        }
    } else {
        let shift = body.len() + 1;
        Edit {
            range: block.end..block.end,
            insert: format!("\n{body}"),
            select: (sel.start + shift)..(sel.end + shift),
        }
    }
}

/// Ctrl+Shift+K：删掉选区覆盖的行（连同一个换行符），光标落到删完后接上来的那一行。
pub fn delete_lines(text: &str, sel: &Range<usize>) -> Option<Edit> {
    if text.is_empty() {
        return None;
    }
    let block = covered(text, sel);
    let range = if block.end < text.len() {
        block.start..block.end + 1
    } else if block.start > 0 {
        // 删最后一行：把它前面的换行符一起带走。
        block.start - 1..block.end
    } else {
        0..text.len()
    };
    let at = range.start;
    Some(Edit {
        range,
        insert: String::new(),
        select: at..at,
    })
}

/// Alt+L：选中整行；已经整行选中了就再往下多选一行。
pub fn select_line(text: &str, sel: &Range<usize>) -> Range<usize> {
    let block = covered(text, sel);
    let whole = block.start..(block.end + 1).min(text.len());
    if sel.start == whole.start && sel.end == whole.end && whole.end < text.len() {
        let next_end = line_end(text, whole.end);
        return whole.start..(next_end + 1).min(text.len());
    }
    whole
}

/// Ctrl+Enter：在当前行下面插入一个空行（缩进和当前行一样），光标进去。
pub fn insert_blank_line_below(text: &str, sel: &Range<usize>) -> Edit {
    let at = sel.end.min(text.len());
    let start = line_start(text, at);
    let end = line_end(text, at);
    let indent = &text[start..start + indent_len(&text[start..end])];
    let inserted = format!("\n{indent}");
    let cursor = end + inserted.len();
    Edit {
        range: end..end,
        insert: inserted,
        select: cursor..cursor,
    }
}

/// 把一次替换套到整段文本上（单测和调用方的对照用）。
#[cfg(test)]
pub fn apply(text: &str, edit: &Edit) -> String {
    let mut out = String::with_capacity(text.len() + edit.insert.len());
    out.push_str(&text[..edit.range.start]);
    out.push_str(&edit.insert);
    out.push_str(&text[edit.range.end..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(
        text: &str,
        sel: Range<usize>,
        f: impl Fn(&str, &Range<usize>) -> Option<Edit>,
    ) -> (String, Range<usize>) {
        let edit = f(text, &sel).expect("应该有结果");
        let new_text = apply(text, &edit);
        (new_text, edit.select)
    }

    #[test]
    fn hash_comment_is_added_at_each_lines_indent_and_removed_again() {
        let text = "a\n  b\n\n    c\n";
        // 选中 a 到 c 的内容。
        let sel = 0..text.len() - 1;
        let (commented, selection) = run(text, sel.clone(), |t, s| {
            toggle_comment(t, s, Comment::Line("#"))
        });
        assert_eq!(commented, "# a\n  # b\n\n    # c\n");
        // 选区两端跟着内容走。
        assert_eq!(selection.start, 0);
        assert_eq!(
            &commented[selection.clone()],
            &commented[..commented.len() - 1]
        );
        // 再来一次：回到原样。
        let (back, _) = run(&commented, selection, |t, s| {
            toggle_comment(t, s, Comment::Line("#"))
        });
        assert_eq!(back, text);
    }

    #[test]
    fn comment_goes_in_at_each_lines_own_indent_like_codemirror() {
        let text = "    x\n      y\n";
        let (commented, _) = run(text, 0..text.len() - 1, |t, s| {
            toggle_comment(t, s, Comment::Line("//"))
        });
        assert_eq!(commented, "    // x\n      // y\n");
        let (back, _) = run(&commented, 0..commented.len() - 1, |t, s| {
            toggle_comment(t, s, Comment::Line("//"))
        });
        assert_eq!(back, text);
    }

    #[test]
    fn a_mix_of_commented_and_plain_lines_comments_everything() {
        let text = "# a\nb\n";
        let (commented, _) = run(text, 0..text.len() - 1, |t, s| {
            toggle_comment(t, s, Comment::Line("#"))
        });
        assert_eq!(commented, "# # a\n# b\n");
    }

    #[test]
    fn cursor_alone_toggles_its_own_line_and_the_cursor_stays_put() {
        let text = "one\ntwo\nthree\n";
        let (commented, selection) =
            run(text, 5..5, |t, s| toggle_comment(t, s, Comment::Line("#")));
        assert_eq!(commented, "one\n# two\nthree\n");
        assert_eq!(selection, 7..7);
        let (back, selection) = run(&commented, 7..7, |t, s| {
            toggle_comment(t, s, Comment::Line("#"))
        });
        assert_eq!(back, text);
        assert_eq!(selection, 5..5);
    }

    #[test]
    fn selection_ending_at_a_line_start_does_not_touch_that_line() {
        let text = "a\nb\nc\n";
        // 选中 "a\nb\n"：结束在 c 的行首。
        let (commented, _) = run(text, 0..4, |t, s| toggle_comment(t, s, Comment::Line("#")));
        assert_eq!(commented, "# a\n# b\nc\n");
    }

    #[test]
    fn uncomment_without_a_space_after_the_token() {
        let text = "#a\n#b\n";
        let (back, _) = run(text, 0..text.len() - 1, |t, s| {
            toggle_comment(t, s, Comment::Line("#"))
        });
        assert_eq!(back, "a\nb\n");
    }

    #[test]
    fn chinese_text_is_commented_on_char_boundaries() {
        let text = "你好\n世界\n";
        let (commented, _) = run(text, 0..text.len() - 1, |t, s| {
            toggle_comment(t, s, Comment::Line("#"))
        });
        assert_eq!(commented, "# 你好\n# 世界\n");
    }

    #[test]
    fn json_and_blank_selections_do_nothing() {
        assert!(toggle_comment("{}", &(0..2), Comment::None).is_none());
        assert!(toggle_comment("\n\n", &(0..1), Comment::Line("#")).is_none());
    }

    #[test]
    fn block_comment_wraps_the_selection_and_unwraps_it() {
        let text = "a { color: red }";
        let (wrapped, _) = run(text, 0..text.len(), |t, s| {
            toggle_comment(t, s, Comment::Block("/*", "*/"))
        });
        assert_eq!(wrapped, "/* a { color: red } */");
        let (back, _) = run(&wrapped, 0..wrapped.len(), |t, s| {
            toggle_comment(t, s, Comment::Block("/*", "*/"))
        });
        assert_eq!(back, text);
    }

    #[test]
    fn block_comment_on_a_cursor_wraps_the_line_without_its_indent() {
        let text = "<div>\n  <p>hi</p>\n</div>\n";
        let (wrapped, _) = run(text, 10..10, |t, s| {
            toggle_comment(t, s, Comment::Block("<!--", "-->"))
        });
        assert_eq!(wrapped, "<div>\n  <!-- <p>hi</p> -->\n</div>\n");
    }

    #[test]
    fn moving_a_line_down_and_up_swaps_with_its_neighbour() {
        let text = "a\nb\nc\n";
        let (down, selection) = run(text, 0..1, |t, s| move_lines(t, s, false));
        assert_eq!(down, "b\na\nc\n");
        assert_eq!(selection, 2..3);
        let (up, selection) = run(&down, selection, |t, s| move_lines(t, s, true));
        assert_eq!(up, text);
        assert_eq!(selection, 0..1);
    }

    #[test]
    fn moving_several_lines_moves_them_together() {
        let text = "a\nb\nc\nd\n";
        // 选中 b、c 两行。
        let (moved, selection) = run(text, 2..5, |t, s| move_lines(t, s, true));
        assert_eq!(moved, "b\nc\na\nd\n");
        assert_eq!(selection, 0..3);
    }

    #[test]
    fn moving_at_the_edges_does_nothing() {
        assert!(move_lines("a\nb", &(0..0), true).is_none());
        assert!(move_lines("a\nb", &(2..3), false).is_none());
        assert!(move_lines("only", &(1..1), false).is_none());
    }

    #[test]
    fn moving_the_last_line_without_a_trailing_newline_up() {
        let (moved, selection) = run("a\nb", 2..3, |t, s| move_lines(t, s, true));
        assert_eq!(moved, "b\na");
        assert_eq!(selection, 0..1);
    }

    #[test]
    fn copying_down_duplicates_below_and_the_selection_follows() {
        let text = "a\nb\n";
        let edit = copy_lines(text, &(0..1), false);
        assert_eq!(apply(text, &edit), "a\na\nb\n");
        assert_eq!(edit.select, 2..3);
    }

    #[test]
    fn copying_up_duplicates_above_and_the_selection_stays() {
        let text = "a\nb\n";
        let edit = copy_lines(text, &(2..3), true);
        assert_eq!(apply(text, &edit), "a\nb\nb\n");
        assert_eq!(edit.select, 2..3);
    }

    #[test]
    fn copying_the_last_line_without_a_newline() {
        let edit = copy_lines("a\nb", &(2..2), false);
        assert_eq!(apply("a\nb", &edit), "a\nb\nb");
        assert_eq!(edit.select, 4..4);
    }

    #[test]
    fn deleting_lines_takes_the_newline_with_them() {
        let text = "a\nb\nc\n";
        let (after, selection) = run(text, 2..3, delete_lines);
        assert_eq!(after, "a\nc\n");
        assert_eq!(selection, 2..2);
        // 删最后一行（后面没有换行）：前一行的换行符一起带走。
        let (after, selection) = run("a\nb", 2..2, delete_lines);
        assert_eq!(after, "a");
        assert_eq!(selection, 1..1);
        // 只有一行：清空。
        let (after, _) = run("only", 1..1, delete_lines);
        assert_eq!(after, "");
        assert!(delete_lines("", &(0..0)).is_none());
    }

    #[test]
    fn deleting_a_multi_line_selection_removes_every_covered_line() {
        let (after, _) = run("a\nb\nc\nd\n", 2..5, delete_lines);
        assert_eq!(after, "a\nd\n");
    }

    #[test]
    fn select_line_takes_the_whole_line_then_grows() {
        let text = "ab\ncd\nef\n";
        let first = select_line(text, &(1..1));
        assert_eq!(first, 0..3);
        let second = select_line(text, &first);
        assert_eq!(second, 0..6);
    }

    #[test]
    fn blank_line_below_keeps_the_indent_of_the_current_line() {
        let text = "  if x:\nend";
        let edit = insert_blank_line_below(text, &(4..4));
        assert_eq!(apply(text, &edit), "  if x:\n  \nend");
        assert_eq!(edit.select, 10..10);
        let edit = insert_blank_line_below("tail", &(2..2));
        assert_eq!(apply("tail", &edit), "tail\n");
        assert_eq!(edit.select, 5..5);
    }
}
