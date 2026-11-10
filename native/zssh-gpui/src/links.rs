//! 纯文本 URL 识别：OSC 8 之外的 `http://` / `https://`。
//!
//! Z-SSH（Tauri 版）现有规则只允许 http/https 打开；这里只做识别，协议校验仍交给
//! `open_external_url`。中文标点作为终止符，避免把「，」吃进 URL。

use zssh_terminal::RowSnapshot;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkSpan {
    pub start_col: usize,
    pub end_col: usize,
    pub url: String,
}

/// 从一行快照里找纯文本链接；OSC 8 链接由 cell 自己带，不在这里重复。
pub fn find_plain_links(row: &RowSnapshot) -> Vec<LinkSpan> {
    let mut chars = Vec::with_capacity(row.cells.len());
    let mut cols = Vec::with_capacity(row.cells.len());
    for (col, cell) in row.cells.iter().enumerate() {
        if cell.spacer {
            continue;
        }
        chars.push(cell.ch);
        cols.push(col);
        if let Some(zerowidth) = &cell.zerowidth {
            for ch in zerowidth {
                chars.push(*ch);
                cols.push(col);
            }
        }
    }

    let mut links = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let Some(scheme_len) = scheme_at(&chars, i) else {
            i += 1;
            continue;
        };
        let start = i;
        let mut end = i + scheme_len;
        while end < chars.len() && !is_terminator(chars[end]) {
            end += 1;
        }
        // 去掉句尾标点。
        while end > start + scheme_len && is_trailing_punctuation(chars[end - 1]) {
            end -= 1;
        }
        if end > start + scheme_len {
            links.push(LinkSpan {
                start_col: cols[start],
                end_col: cols[end - 1],
                url: chars[start..end].iter().collect(),
            });
        }
        i = end.max(i + 1);
    }
    links
}

/// 光标下的链接覆盖的列范围（闭区间）；不是链接返回 `None`。
///
/// OSC 8 链接取同一 URI 的连续格子；纯文本链接取识别出的 URL 区间。
/// 双击选中整个链接用，复制出来的就只有链接本身。
pub fn link_span(row: &RowSnapshot, col: usize) -> Option<(usize, usize)> {
    // 宽字符的占位格没有自己的属性，点在它上面算点在前一个格子。
    let mut col = col;
    while row.cells.get(col)?.spacer {
        col = col.checked_sub(1)?;
    }
    if let Some(uri) = &row.cells[col].hyperlink {
        let mut start = col;
        while start > 0 && row.cells[start - 1].hyperlink.as_ref() == Some(uri) {
            start -= 1;
        }
        let mut end = col;
        while end + 1 < row.cells.len() && row.cells[end + 1].hyperlink.as_ref() == Some(uri) {
            end += 1;
        }
        return Some((start, end));
    }
    find_plain_links(row)
        .into_iter()
        .find(|link| col >= link.start_col && col <= link.end_col)
        .map(|link| (link.start_col, link.end_col))
}

fn scheme_at(chars: &[char], i: usize) -> Option<usize> {
    let rest = chars.get(i..)?;
    let prefix: String = rest.iter().take(8).collect::<String>().to_ascii_lowercase();
    if prefix.starts_with("https://") {
        Some(8)
    } else if prefix.starts_with("http://") {
        Some(7)
    } else {
        None
    }
}

fn is_terminator(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '"' | '\''
                | '<'
                | '>'
                | '`'
                | '\u{ff0c}'
                | '\u{3002}'
                | '\u{ff1b}'
                | '\u{ff1a}'
                | '\u{ff01}'
                | '\u{ff1f}'
                | '\u{ff09}'
                | '\u{3011}'
        )
}

fn is_trailing_punctuation(ch: char) -> bool {
    matches!(ch, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}')
}

#[cfg(test)]
mod tests {
    use super::*;
    use zssh_terminal::{Size, TerminalModel};

    fn row(text: &str) -> RowSnapshot {
        let mut term = TerminalModel::new(Size { rows: 2, cols: 120 }, 10);
        term.feed(text.as_bytes());
        term.snapshot().rows.remove(0)
    }

    #[test]
    fn finds_http_and_https_links() {
        let row = row("see https://example.com/a?b=1 and http://a.test next");
        let links = find_plain_links(&row);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].url, "https://example.com/a?b=1");
        assert_eq!(links[1].url, "http://a.test");
        assert_eq!(row.cells[links[0].start_col].ch, 'h');
    }

    #[test]
    fn strips_trailing_punctuation() {
        let row = row("open https://example.com/path).");
        let links = find_plain_links(&row);
        assert_eq!(links[0].url, "https://example.com/path");
    }

    #[test]
    fn stops_at_cjk_punctuation() {
        let row = row("访问https://example.com/a，完成");
        let links = find_plain_links(&row);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].url, "https://example.com/a");
    }

    #[test]
    fn ignores_bare_schemes_and_other_protocols() {
        for text in ["http://", "https://", "ftp://example.com", "example.com"] {
            assert!(find_plain_links(&row(text)).is_empty(), "{text}");
        }
    }

    #[test]
    fn link_span_covers_exactly_the_url() {
        // 点在 URL 的任意一格（含结尾前的最后一格）都选中整段 URL。
        let line = row("see https://example.com/a?b=1 next");
        for col in [4, 10, 20, 28] {
            assert_eq!(
                link_span(&line, col),
                Some((4, 28)),
                "col {col} 应该落在同一段链接里"
            );
        }
        // 点在链接外没有链接。
        assert_eq!(link_span(&line, 0), None);
        assert_eq!(link_span(&line, 2), None);
        assert_eq!(link_span(&line, 29), None);
        assert_eq!(link_span(&line, 33), None);
        // 结尾标点不属于链接。
        let line = row("open https://example.com/path).");
        assert_eq!(link_span(&line, 10), Some((5, 28)));
        assert_eq!(link_span(&line, 29), None, "右括号不算链接");
    }

    #[test]
    fn link_span_walks_off_the_wide_char_spacer() {
        // 全角标点的占位格不是链接；URL 第一格才是。
        let line = row("访问https://example.com/a，完成");
        // 「问」的右半占位格：算在整行文字里找它的格子。
        let spacer_col = line
            .cells
            .iter()
            .position(|cell| cell.spacer)
            .expect("全角字符有占位格");
        assert_eq!(link_span(&line, spacer_col), None, "中文不是链接");
        let url_start = line.cells[spacer_col..]
            .iter()
            .position(|cell| cell.ch == 'h')
            .map(|at| at + spacer_col)
            .expect("URL 起点");
        let span = link_span(&line, url_start).expect("URL 是链接");
        let text: String = line.cells[span.0..=span.1]
            .iter()
            .filter(|cell| !cell.spacer)
            .map(|cell| cell.ch)
            .collect();
        assert_eq!(text, "https://example.com/a");
    }
}
