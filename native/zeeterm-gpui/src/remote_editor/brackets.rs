//! 括号配对高亮和「选中内容的其他出现处」高亮：CodeMirror 的 `bracketMatching()` 和
//! `highlightSelectionMatches()`，gpui-kit 0.8.1 的 Editor 没有，这里补最小的一份。
//!
//! 只做文本层面的扫描（不懂语法，字符串里的括号也会被当成括号）；CodeMirror 靠语法树跳过字符串，
//! 这是已知差异，写在报告里。两个函数都是纯函数，偏移是 UTF-8 字节偏移。

use std::ops::Range;

/// 括号配对最多往外扫多少字节（同 CodeMirror 的 `maxScanDistance`）。
const MAX_SCAN: usize = 10_000;
/// 同一段选中内容最多高亮多少处（同 CodeMirror 的 `maxMatches`）。
const MAX_MATCHES: usize = 100;
/// 文本太大就不找了：高亮选中内容的其他出现处每次选区变化都要扫一遍。
const MAX_TEXT_FOR_MATCHES: usize = 1 << 20;

const PAIRS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];

fn open_of(close: char) -> Option<char> {
    PAIRS.iter().find(|(_, c)| *c == close).map(|(o, _)| *o)
}

fn close_of(open: char) -> Option<char> {
    PAIRS.iter().find(|(o, _)| *o == open).map(|(_, c)| *c)
}

/// 光标前后挨着的括号和它配对的另一个：返回两个括号各自的字节范围（长度都是 1）。
///
/// 和 CodeMirror 一样先看光标**前**面的字符，再看后面的；找不到配对（没闭合）返回 `None`。
pub fn matching_pair(text: &str, cursor: usize) -> Option<(Range<usize>, Range<usize>)> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let before = text[..cursor].chars().next_back().map(|c| (c, cursor - 1));
    let after = text[cursor..].chars().next().map(|c| (c, cursor));
    for (c, at) in [before, after].into_iter().flatten() {
        if let Some(close) = close_of(c) {
            if let Some(other) = scan_forward(text, at, c, close) {
                return Some((at..at + 1, other..other + 1));
            }
        } else if let Some(open) = open_of(c)
            && let Some(other) = scan_backward(text, at, open, c)
        {
            return Some((other..other + 1, at..at + 1));
        }
    }
    None
}

fn scan_forward(text: &str, at: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    for (index, c) in text[at..].char_indices() {
        if index > MAX_SCAN {
            return None;
        }
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return Some(at + index);
            }
        }
    }
    None
}

fn scan_backward(text: &str, at: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    let mut scanned = 0usize;
    for (index, c) in text[..=at].char_indices().rev() {
        scanned = scanned.max(at - index);
        if scanned > MAX_SCAN {
            return None;
        }
        if c == close {
            depth += 1;
        } else if c == open {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

/// 选中了一段内容时，文本里其他相同内容的位置（不含选区自己），最多 [`MAX_MATCHES`] 处。
///
/// 选区为空、含换行、或文本太大时返回空。
pub fn selection_matches(text: &str, sel: &Range<usize>) -> Vec<Range<usize>> {
    if sel.start >= sel.end
        || sel.end > text.len()
        || text.len() > MAX_TEXT_FOR_MATCHES
        || !text.is_char_boundary(sel.start)
        || !text.is_char_boundary(sel.end)
    {
        return Vec::new();
    }
    let needle = &text[sel.clone()];
    if needle.contains('\n') || needle.trim().is_empty() {
        return Vec::new();
    }
    text.match_indices(needle)
        .map(|(at, found)| at..at + found.len())
        .filter(|range| range != sel)
        .take(MAX_MATCHES)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bracket_before_or_after_the_cursor_finds_its_partner() {
        let text = "f(a, (b), c)";
        // 光标在最前面的 ( 后面。
        assert_eq!(matching_pair(text, 2), Some((1..2, 11..12)));
        // 光标在 ( 前面（看后面的字符）。
        assert_eq!(matching_pair(text, 1), Some((1..2, 11..12)));
        // 光标紧跟在最后的 ) 后面。
        assert_eq!(matching_pair(text, 12), Some((1..2, 11..12)));
        // 里面的一对。
        assert_eq!(matching_pair(text, 6), Some((5..6, 7..8)));
    }

    #[test]
    fn square_and_curly_brackets_pair_too() {
        assert_eq!(matching_pair("{[x]}", 1), Some((0..1, 4..5)));
        assert_eq!(matching_pair("{[x]}", 2), Some((1..2, 3..4)));
    }

    #[test]
    fn unclosed_or_mismatched_brackets_have_no_partner() {
        assert_eq!(matching_pair("(abc", 1), None);
        assert_eq!(matching_pair("abc)", 4), None);
        assert_eq!(matching_pair("plain text", 3), None);
        assert_eq!(matching_pair("", 0), None);
    }

    #[test]
    fn brackets_next_to_chinese_text_use_char_boundaries() {
        let text = "（你）(好)";
        // 全角括号不配对；半角括号照常。
        let open = text.find('(').unwrap();
        let close = text.find(')').unwrap();
        assert_eq!(
            matching_pair(text, open + 1),
            Some((open..open + 1, close..close + 1))
        );
        // 光标落在汉字中间（不是字符边界）：不崩，返回 None。
        assert_eq!(matching_pair(text, 1), None);
    }

    #[test]
    fn partners_beyond_the_scan_distance_are_ignored() {
        let text = format!("({})", "x".repeat(MAX_SCAN + 10));
        assert_eq!(matching_pair(&text, 1), None);
    }

    #[test]
    fn other_occurrences_of_the_selection_are_found() {
        let text = "foo bar foo baz foo";
        let found = selection_matches(text, &(0..3));
        assert_eq!(found, vec![8..11, 16..19]);
    }

    #[test]
    fn empty_multiline_and_blank_selections_match_nothing() {
        let text = "a\nb a\nb";
        assert!(selection_matches(text, &(1..1)).is_empty());
        assert!(selection_matches(text, &(0..3)).is_empty());
        assert!(selection_matches("a   b   ", &(1..4)).is_empty());
    }

    #[test]
    fn matches_are_capped() {
        let text = "ab ".repeat(500);
        assert_eq!(selection_matches(&text, &(0..2)).len(), MAX_MATCHES);
    }
}
