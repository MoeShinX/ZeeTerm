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

/// 悬停时显示在终端角落的"真实地址"。OSC 8 的显示文字可以和真实地址完全不同（`点这里领奖` -> 钓鱼站），
/// 用户双击就会用浏览器打开，所以悬停时必须能看到真正的目标。
///
/// 太长时截断，但**协议和域名部分（含 `user@`、端口）一定保留**：钓鱼的关键信息就在这里，
/// 被截掉的只能是路径 / 参数。域名部分自己就超长（有人拿一长串子域名把真正的主机顶出视野）时，
/// 留尾巴——真正的主机在 `@` 之后、最右边。控制字符和能改变显示顺序 / 隐藏的字符（双向覆盖、零宽）
/// 一律换成 `U+FFFD`，免得提示本身被拿来骗人。
pub fn hover_label(url: &str, max_chars: usize) -> String {
    let max_chars = max_chars.max(8);
    let chars: Vec<char> = url
        .chars()
        .map(|ch| {
            if is_deceptive_char(ch) {
                '\u{fffd}'
            } else {
                ch
            }
        })
        .collect();
    if chars.len() <= max_chars {
        return chars.into_iter().collect();
    }
    // 协议 + 域名部分 = 到 `://` 之后第一个 `/`、`?`、`#` 为止；没有 `://`（mailto: 之类）就当没有域名部分。
    let authority_end = chars
        .windows(3)
        .position(|w| w == [':', '/', '/'])
        .map(|at| at + 3)
        .map(|from| {
            chars[from..]
                .iter()
                .position(|ch| matches!(ch, '/' | '?' | '#'))
                .map_or(chars.len(), |off| from + off)
        })
        .unwrap_or(0);
    if authority_end >= max_chars {
        // 域名部分就放不下：留尾巴。
        let tail: String = chars[..authority_end]
            .iter()
            .rev()
            .take(max_chars - 1)
            .rev()
            .collect();
        return format!("\u{2026}{tail}");
    }
    let room = max_chars - authority_end;
    let head: String = chars[..authority_end].iter().collect();
    if room <= 1 {
        return format!("{head}\u{2026}");
    }
    let rest: String = chars[authority_end..authority_end + room - 1]
        .iter()
        .collect();
    format!("{head}{rest}\u{2026}")
}

/// 控制字符、双向排版控制符和零宽字符：放进提示里会让文字乱序或看不见。
fn is_deceptive_char(ch: char) -> bool {
    ch.is_control()
        || matches!(
            ch,
            '\u{200b}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                | '\u{feff}'
        )
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
    fn a_short_url_is_shown_whole() {
        assert_eq!(
            hover_label("https://example.com/a?b=1", 80),
            "https://example.com/a?b=1"
        );
    }

    #[test]
    fn a_long_url_keeps_scheme_and_host_and_loses_the_path() {
        let url = format!("https://login.example.com:8443/{}", "a".repeat(200));
        let label = hover_label(&url, 50);
        assert_eq!(label.chars().count(), 50);
        assert!(
            label.starts_with("https://login.example.com:8443/"),
            "{label}"
        );
        assert!(label.ends_with('\u{2026}'));
    }

    #[test]
    fn userinfo_tricks_stay_visible_in_the_host_part() {
        // `https://trusted.example@evil.test/..`：真正的主机在 `@` 后面，不能被截掉。
        let url = format!("https://trusted.example@evil.test/{}", "x".repeat(100));
        let label = hover_label(&url, 60);
        assert!(
            label.starts_with("https://trusted.example@evil.test/"),
            "{label}"
        );
    }

    #[test]
    fn an_overlong_host_part_keeps_its_tail_where_the_real_host_is() {
        let url = format!("https://{}.evil.test/path", "a".repeat(120));
        let label = hover_label(&url, 40);
        assert_eq!(label.chars().count(), 40);
        assert!(label.starts_with('\u{2026}'));
        assert!(label.ends_with(".evil.test"), "{label}");
    }

    #[test]
    fn urls_without_a_host_part_are_cut_from_the_end() {
        let label = hover_label(&format!("mailto:{}", "a".repeat(100)), 20);
        assert_eq!(label.chars().count(), 20);
        assert!(label.starts_with("mailto:a"));
    }

    #[test]
    fn control_and_direction_characters_never_reach_the_label() {
        let label = hover_label("https://a.test/\u{1b}[31m\u{202e}evil\u{200b}", 80);
        assert!(!label.chars().any(is_deceptive_char), "{label:?}");
        assert!(label.contains('\u{fffd}'));
    }

    #[test]
    fn ignores_bare_schemes_and_other_protocols() {
        for text in ["http://", "https://", "ftp://example.com", "example.com"] {
            assert!(find_plain_links(&row(text)).is_empty(), "{text}");
        }
    }
}
