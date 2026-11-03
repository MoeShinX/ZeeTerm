//! 智能体输出进日志 / 用户可见错误文案之前的脱敏（M-2）。
//!
//! 智能体的 stdout / stderr 里可能带着它刚读到的东西：私钥、`.env`、令牌。这些文字
//! 不能原样进 `tracing` 日志，也不能原样出现在“进程退出了……”这类错误提示里。
//!
//! 这里按**形状**识别，不依赖“知道秘密是什么”（`history::redact` 要求调用方手里有
//! 明文秘密，智能体自己读到的东西我们并不知道）：
//! - PEM 私钥块（`-----BEGIN ... PRIVATE KEY-----` 到 `-----END ...-----`）；流式
//!   逐行进来时块会跨行，由 [`scrub_stream_line`] 带状态处理；
//! - 常见服务的令牌前缀（`sk-`、`ghp_`、`github_pat_`、`xox*-`、`AKIA…`、JWT……）；
//! - `Bearer xxx` / `Basic xxx`；
//! - `password=…`、`"api_key": "…"`、`--token xxx` 这类“关键词 + 值”。
//!
//! 宁可多遮：误伤几个像令牌的普通词，比漏掉一个秘密好。识别不全是已知的——这是
//! 兜底，不是承诺；所以日志里的逐行输出（trace）干脆只记长度。
//!
//! 为什么手写而不用正则：core 没有 `regex` 依赖，这几条形状简单，不值得新增依赖。

/// 被遮住的内容的替代文字。
pub const MASK: &str = "[已隐藏]";

/// 私钥块被整段遮住后的替代文字。
const KEY_DONE: &str = "[已隐藏：私钥]";

/// 私钥块开头在这一行、结尾还没出现：这一行的结尾换成它，下一行起整行都在块里。
/// 流式记录（尾巴缓冲）靠“最后一行以它结尾”判断自己是否还在块内。
pub const KEY_BLOCK_OPEN: &str = "[已隐藏：私钥块…]";

/// 一行脱敏后的单行文字最多留这么多字符（日志和错误提示里都不需要整行）。
pub const MAX_LINE_CHARS: usize = 240;

/// 一行脱敏的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scrubbed {
    /// 脱敏后的文字；整行都在私钥块里时为 `None`（不用记）。
    pub text: Option<String>,
    /// 这一行结束时是否还在私钥块里（下一行要带着这个状态进来）。
    pub in_key_block: bool,
}

/// 无状态版本：把一行（不跨行）里能认出的秘密遮掉。
pub fn scrub_line(line: &str) -> String {
    scrub_stream_line(line, false)
        .text
        .unwrap_or_else(|| KEY_DONE.to_owned())
}

/// 脱敏后再截到 [`MAX_LINE_CHARS`] 个字符（先遮后截：不会把一个令牌截成一半留下）。
pub fn scrub_and_clip(line: &str) -> String {
    clip(&scrub_line(line), MAX_LINE_CHARS)
}

/// 流式版本：`in_key_block` 是上一行结束时的状态。
pub fn scrub_stream_line(line: &str, in_key_block: bool) -> Scrubbed {
    let (after_keys, still_open) = mask_private_keys(line, in_key_block);
    let Some(text) = after_keys else {
        return Scrubbed {
            text: None,
            in_key_block: still_open,
        };
    };
    Scrubbed {
        text: Some(mask_secrets(&text)),
        in_key_block: still_open,
    }
}

/// 最多 `max` 个字符，超出加省略号（按字符切，不会切断 UTF-8）。
pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}…")
}

// ---------------------------------------------------------------- 私钥块

const BEGIN: &str = "-----BEGIN ";
const END: &str = "-----END ";
const DASHES: &str = "-----";

/// 找 `from` 之后第一个 `-----<tag> <label>-----` 的结束位置（`-----` 之后），
/// 且 label 里含 `PRIVATE KEY`。返回 (起点, 终点)。
fn find_marker(text: &str, tag: &str, from: usize) -> Option<(usize, usize)> {
    let mut search = from;
    while let Some(rel) = text.get(search..)?.find(tag) {
        let start = search + rel;
        let label_start = start + tag.len();
        if let Some(close_rel) = text.get(label_start..)?.find(DASHES) {
            let label = &text[label_start..label_start + close_rel];
            if label.contains("PRIVATE KEY") {
                return Some((start, label_start + close_rel + DASHES.len()));
            }
        }
        search = label_start;
    }
    None
}

/// 把私钥块遮掉。返回 (遮后的文字或 `None` = 整行都在块里, 行尾是否仍在块里)。
fn mask_private_keys(line: &str, in_block: bool) -> (Option<String>, bool) {
    let mut out = String::new();
    let mut pos = 0usize;
    let mut in_block = in_block;
    loop {
        if in_block {
            match find_marker(line, END, pos) {
                Some((_, end)) => {
                    out.push_str(KEY_DONE);
                    pos = end;
                    in_block = false;
                }
                None => {
                    // 块一直到行尾：这一行从 pos 起全是块里的内容。
                    return if out.is_empty() && pos == 0 {
                        (None, true)
                    } else {
                        out.push_str(KEY_BLOCK_OPEN);
                        (Some(out), true)
                    };
                }
            }
        } else {
            match find_marker(line, BEGIN, pos) {
                Some((start, begin_end)) => {
                    out.push_str(&line[pos..start]);
                    pos = begin_end;
                    in_block = true;
                }
                None => {
                    out.push_str(&line[pos..]);
                    return (Some(out), false);
                }
            }
        }
    }
}

// ---------------------------------------------------------------- 令牌 / 键值

/// 令牌里可能出现的字符（base64 / url-safe / 常见分隔），含结尾的 `=` 补位。
fn is_token_char(c: char) -> bool {
    is_word_char(c) || c == '='
}

/// 切“词”用的字符：不含 `=`，这样 `X=ghp_…` 里的令牌是单独的一个词。
fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '~' | '+' | '/')
}

/// 以这些开头、且够长的整个“词”按令牌处理。长度是整个词的最短长度。
const TOKEN_PREFIXES: &[(&str, usize)] = &[
    ("sk-", 12),
    ("ghp_", 20),
    ("gho_", 20),
    ("ghu_", 20),
    ("ghs_", 20),
    ("ghr_", 20),
    ("github_pat_", 20),
    ("glpat-", 16),
    ("xoxb-", 12),
    ("xoxp-", 12),
    ("xoxa-", 12),
    ("xoxs-", 12),
    ("npm_", 24),
    ("hf_", 24),
    ("AIza", 30),
];

/// 键名里出现这些就把后面的值遮掉（小写比较）。
const SECRET_KEYWORDS: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "pwd",
    "secret",
    "token",
    "api_key",
    "api-key",
    "apikey",
    "access_key",
    "private_key",
    "authorization",
    "credential",
];

fn is_aws_key_id(word: &str) -> bool {
    word.len() == 20
        && (word.starts_with("AKIA") || word.starts_with("ASIA"))
        && word
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

fn is_jwt(word: &str) -> bool {
    word.starts_with("eyJ") && word.len() >= 30 && word.matches('.').count() == 2
}

fn is_token_word(word: &str) -> bool {
    if is_aws_key_id(word) || is_jwt(word) {
        return true;
    }
    TOKEN_PREFIXES
        .iter()
        .any(|(prefix, min_len)| word.starts_with(prefix) && word.len() >= *min_len)
}

/// 值的结束：空白、引号、逗号、分号、`&`、括号、反斜杠（JSON 转义）。
fn is_value_end(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '"' | '\'' | ',' | ';' | '&' | '}' | ')' | ']' | '\\' | '`'
        )
}

/// 区间列表，已按起点排序并合并。
fn push_range(ranges: &mut Vec<(usize, usize)>, start: usize, end: usize) {
    if start < end {
        ranges.push((start, end));
    }
}

fn mask_secrets(text: &str) -> String {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    // 只有 ASCII 字母被改成小写，字节长度和位置都不变，可以拿它的下标回头切原文。
    let lower = text.to_ascii_lowercase();

    word_tokens(text, &mut ranges);
    bearer_values(text, &lower, &mut ranges);
    keyword_values(text, &lower, &mut ranges);

    if ranges.is_empty() {
        return text.to_owned();
    }
    ranges.sort_unstable();
    let mut out = String::with_capacity(text.len());
    let mut pos = 0usize;
    for (start, end) in ranges {
        if end <= pos {
            continue;
        }
        let start = start.max(pos);
        out.push_str(&text[pos..start]);
        out.push_str(MASK);
        pos = end;
    }
    out.push_str(&text[pos..]);
    out
}

/// 按“词”扫：整个词像令牌就遮。
fn word_tokens(text: &str, ranges: &mut Vec<(usize, usize)>) {
    let mut start: Option<usize> = None;
    for (index, c) in text.char_indices() {
        if is_word_char(c) {
            start.get_or_insert(index);
        } else if let Some(s) = start.take() {
            if is_token_word(&text[s..index]) {
                push_range(ranges, s, index);
            }
        }
    }
    if let Some(s) = start {
        if is_token_word(&text[s..]) {
            push_range(ranges, s, text.len());
        }
    }
}

/// `Bearer xxx` / `Basic xxx`：遮后面的那个词。
fn bearer_values(text: &str, lower: &str, ranges: &mut Vec<(usize, usize)>) {
    for scheme in ["bearer ", "basic "] {
        let mut from = 0usize;
        while let Some(rel) = lower[from..].find(scheme) {
            let at = from + rel;
            from = at + scheme.len();
            // 前面得是词边界，别把 `xbearer ` 当成。
            if text[..at].chars().next_back().is_some_and(is_token_char) {
                continue;
            }
            let value_start = from;
            let value_end = value_start
                + text[value_start..]
                    .find(|c: char| !is_token_char(c))
                    .unwrap_or(text.len() - value_start);
            // 太短的（`Basic auth` 里的普通词）不遮。
            if value_end - value_start >= 8 {
                push_range(ranges, value_start, value_end);
            }
        }
    }
}

/// 关键词 + 分隔符 + 值：`password=x`、`"token": "x"`、`--api-key x`。
fn keyword_values(text: &str, lower: &str, ranges: &mut Vec<(usize, usize)>) {
    let bytes = text.as_bytes();
    for keyword in SECRET_KEYWORDS {
        let mut from = 0usize;
        while let Some(rel) = lower[from..].find(keyword) {
            let at = from + rel;
            let mut cursor = at + keyword.len();
            from = cursor;
            // 关键词后面可以接键名的其余部分（`password_hash`、`tokenizer`——后者没有
            // 分隔符，下面自然匹配不上）。
            while cursor < bytes.len()
                && (bytes[cursor].is_ascii_alphanumeric() || matches!(bytes[cursor], b'_' | b'-'))
            {
                cursor += 1;
            }
            // 可选的收尾引号（JSON 键名，可能是转义过的 `\"`），再是空白。
            if bytes.get(cursor) == Some(&b'\\') && bytes.get(cursor + 1) == Some(&b'"') {
                cursor += 2;
            } else if matches!(bytes.get(cursor), Some(b'"' | b'\'')) {
                cursor += 1;
            }
            let after_key = cursor;
            while matches!(bytes.get(cursor), Some(b' ' | b'\t')) {
                cursor += 1;
            }
            let has_separator = matches!(bytes.get(cursor), Some(b':' | b'='));
            let is_flag = !has_separator
                && cursor > after_key
                && text[..at].ends_with("--")
                && cursor < bytes.len();
            if !has_separator && !is_flag {
                continue;
            }
            if has_separator {
                cursor += 1;
            }
            while matches!(bytes.get(cursor), Some(b' ' | b'\t')) {
                cursor += 1;
            }
            // 值前面的引号，JSON 里还可能是转义过的 `\"`。
            if bytes.get(cursor) == Some(&b'\\') && matches!(bytes.get(cursor + 1), Some(b'"')) {
                cursor += 2;
            } else if matches!(bytes.get(cursor), Some(b'"' | b'\'')) {
                cursor += 1;
            }
            let value_start = cursor;
            let value_end = value_start
                + text[value_start..]
                    .find(is_value_end)
                    .unwrap_or(text.len() - value_start);
            // `Bearer xxx` 已经由上面处理；这里值是 `Bearer` 时不重复遮词本身。
            if value_end > value_start && !MASKED_ALREADY.contains(&&lower[value_start..value_end])
            {
                push_range(ranges, value_start, value_end);
            }
        }
    }
}

/// 值本身是这些词时不遮（后面跟着的真正凭据由 [`bearer_values`] 管）。
const MASKED_ALREADY: &[&str] = &["bearer", "basic"];

#[cfg(test)]
mod tests;
