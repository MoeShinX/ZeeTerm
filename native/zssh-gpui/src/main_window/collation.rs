//! 中文名字的排序规则：和 0.2.14 前端的 `Intl.Collator("zh-Hans-CN", { numeric: true,
//! sensitivity: "base" })` 一致——中文按拼音、数字按数值、英文不分大小写。
//!
//! Windows 自带这套规则（`CompareStringEx`，区域 `zh-CN` 的默认排序就是拼音），ZeeTerm 只发
//! Windows，所以直接问系统，不再塞一张拼音表进来。系统调用失败（返回 0）时退回按字符比较，
//! 排出来的顺序可能和拼音不同，但至少稳定、不 panic。

use std::cmp::Ordering;

/// 比较两个名字（不含「先分数字 / 英文 / 中文 / 符号四段」那一步，那一步在 `host_tree`）。
pub fn compare(a: &str, b: &str) -> Ordering {
    system_compare(a, b).unwrap_or_else(|| fallback(a, b))
}

#[cfg(windows)]
fn system_compare(a: &str, b: &str) -> Option<Ordering> {
    use windows::Win32::Globalization::{
        CompareStringEx, LINGUISTIC_IGNORECASE, SORT_DIGITSASNUMBERS,
    };
    use windows::core::w;

    let wide = |text: &str| -> Vec<u16> { text.encode_utf16().collect() };
    let (left, right) = (wide(a), wide(b));
    // SAFETY: 两个切片在调用期间有效；区域名是以 0 结尾的常量。
    let result = unsafe {
        CompareStringEx(
            w!("zh-CN"),
            LINGUISTIC_IGNORECASE | SORT_DIGITSASNUMBERS,
            &left,
            &right,
            None,
            None,
            None,
        )
    };
    // CSTR_LESS_THAN = 1，CSTR_EQUAL = 2，CSTR_GREATER_THAN = 3；0 是失败。
    match result.0 {
        1 => Some(Ordering::Less),
        2 => Some(Ordering::Equal),
        3 => Some(Ordering::Greater),
        _ => None,
    }
}

#[cfg(not(windows))]
fn system_compare(_a: &str, _b: &str) -> Option<Ordering> {
    None
}

/// 没有系统排序时的退路：连续数字按数值比，其余按小写字符比。
fn fallback(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars<'_>>| {
                    let mut digits = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        digits.push(c);
                        it.next();
                    }
                    digits
                };
                let (da, db) = (take(&mut a), take(&mut b));
                let (ta, tb) = (da.trim_start_matches('0'), db.trim_start_matches('0'));
                let order = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if order != Ordering::Equal {
                    return order;
                }
            }
            (Some(x), Some(y)) => {
                let order = x.to_lowercase().cmp(y.to_lowercase());
                if order != Ordering::Equal {
                    return order;
                }
                a.next();
                b.next();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_names_follow_pinyin_not_code_points() {
        // 码位顺序是 张(5F20) < 百(767E) < 阿(963F)；拼音顺序是 阿 a < 百 b < 张 z。
        let mut names = vec!["张三", "百度", "阿里"];
        names.sort_by(|a, b| compare(a, b));
        assert_eq!(names, ["阿里", "百度", "张三"]);
    }

    #[test]
    fn digits_compare_by_value_and_case_is_ignored() {
        assert_eq!(compare("web2", "web10"), Ordering::Less);
        assert_eq!(compare("Alpha", "beta"), Ordering::Less);
        assert_eq!(compare("ABC", "abc"), Ordering::Equal);
    }

    #[test]
    fn the_fallback_is_stable_and_numeric() {
        assert_eq!(fallback("web2", "web10"), Ordering::Less);
        assert_eq!(fallback("Beta", "alpha"), Ordering::Greater);
        assert_eq!(fallback("", "a"), Ordering::Less);
    }
}
