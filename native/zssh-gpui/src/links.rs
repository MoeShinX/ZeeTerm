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
}
