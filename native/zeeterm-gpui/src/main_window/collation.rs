//! 中文名字的排序规则：和 0.2.14 前端的 `Intl.Collator("zh-Hans-CN", { numeric: true,
//! sensitivity: "base" })` 一致——中文按拼音、数字按数值、英文不分大小写。
//!
//! Windows 自带这套规则（`CompareStringEx`，区域 `zh-CN` 的默认排序就是拼音），所以直接问系统，
//! 不再塞一张拼音表进来。系统调用失败（返回 0）、以及没有这个调用的系统（Linux）走纯 Rust 的 [`fallback`]：
//! 数字按数值、字母不分大小写，汉字借 GBK 编码的顺序得到拼音顺序。

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

/// 没有系统排序时的退路（Linux 上永远走这条）：符号 < 数字 < 字母 < 汉字；连续数字按数值比，
/// 字母不分大小写，汉字按拼音。
///
/// 汉字拼音的来源：GB2312 一级汉字（3755 个最常用字）在 GBK 编码里就是按拼音排好的，
/// 所以把汉字转成 GBK 取码位比较，就得到拼音顺序，不需要自带拼音表。二级字（生僻字，按部首排）
/// 和 GBK 里没有的字排在一级字后面，各自按码位比，保证顺序稳定。多音字取 GB2312 里的那个读音，
/// 和 Windows 的默认读音一致的居多，少数多音字（如「重」）可能和系统排的不同。
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
                let order = char_key(x).cmp(&char_key(y));
                if order != Ordering::Equal {
                    return order;
                }
                a.next();
                b.next();
            }
        }
    }
}

/// 一个字符的排序键：(类别, 类别内的值)。类别靠前的排前面。
fn char_key(c: char) -> (u8, u32) {
    if c.is_ascii_digit() {
        // 只会在「一边是数字、另一边不是」时走到这里（两边都是数字走数值比较）。
        (1, u32::from(c))
    } else if c.is_alphabetic() && !is_han(c) {
        // 字母不分大小写；按小写后的第一个字符比（多字符的小写形式很少见，够稳定就行）。
        (2, c.to_lowercase().next().map_or(u32::from(c), u32::from))
    } else if is_han(c) {
        match gbk_code(c) {
            // 一级字：拼音顺序。
            Some(code) if (GB_LEVEL1_START..=GB_LEVEL1_END).contains(&code) => (3, code),
            // 二级字等 GBK 里有的字：排在一级字之后，按 GBK 码位。
            Some(code) => (4, code),
            // GBK 里没有的汉字：最后，按 Unicode 码位。
            None => (5, u32::from(c)),
        }
    } else {
        // 空白、标点、符号：最前面，按码位。
        (0, u32::from(c))
    }
}

/// GB2312 一级汉字在 GBK 里的码位范围（啊 `B0A1` ～ 座 `D7F9`），按拼音排列。
const GB_LEVEL1_START: u32 = 0xB0A1;
const GB_LEVEL1_END: u32 = 0xD7F9;

fn is_han(c: char) -> bool {
    matches!(u32::from(c), 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2FA1F)
}

/// 汉字的 GBK 码位（双字节，高字节在前）；GBK 里没有这个字返回 `None`。
fn gbk_code(c: char) -> Option<u32> {
    let mut text = [0u8; 4];
    let text = c.encode_utf8(&mut text);
    let mut out = [0u8; 8];
    let (result, _read, written) = encoding_rs::GBK
        .new_encoder()
        .encode_from_utf8_without_replacement(text, &mut out, true);
    if result != encoding_rs::EncoderResult::InputEmpty {
        return None;
    }
    match (written, out[0], out[1]) {
        (2, high, low) if high >= 0x81 => Some((u32::from(high) << 8) | u32::from(low)),
        _ => None,
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

#[cfg(test)]
mod fallback_tests {
    use super::*;

    fn sorted(names: &[&str]) -> Vec<String> {
        let mut names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
        names.sort_by(|a, b| fallback(a, b));
        names
    }

    #[test]
    fn common_hanzi_follow_pinyin_order() {
        // 每个字一个拼音：a ba cai de e fa ge hao shi wo xiao yi zai。
        let in_pinyin_order = [
            "啊", "把", "才", "的", "鹅", "发", "个", "好", "是", "我", "小", "一", "在",
        ];
        let mut shuffled = in_pinyin_order;
        shuffled.reverse();
        assert_eq!(sorted(&shuffled), in_pinyin_order);
        // 同声母不同韵母：阿(a) 安(an) 爱(ai) → ai < an，a 在最前。
        assert_eq!(sorted(&["安", "爱", "阿"]), ["阿", "爱", "安"]);
    }

    #[test]
    fn multi_character_names_compare_character_by_character() {
        assert_eq!(
            sorted(&["张三", "百度", "阿里", "阿里巴巴", "安徽"]),
            ["阿里", "阿里巴巴", "安徽", "百度", "张三"]
        );
    }

    #[test]
    fn symbols_come_before_digits_before_letters_before_hanzi() {
        assert_eq!(
            sorted(&["阿里", "beta", "10", "_tmp", "-x", "Alpha", "2"]),
            ["-x", "_tmp", "2", "10", "Alpha", "beta", "阿里"]
        );
    }

    #[test]
    fn digits_inside_mixed_names_compare_by_value_and_case_is_ignored() {
        assert_eq!(
            sorted(&["服务器10", "服务器2", "web10", "Web2", "web1"]),
            ["web1", "Web2", "web10", "服务器2", "服务器10"]
        );
        assert_eq!(fallback("Server 1", "server 02"), Ordering::Less);
        assert_eq!(fallback("ABC", "abc"), Ordering::Equal);
        // 前导零不影响数值，但整体相同时就是相等。
        assert_eq!(fallback("a01", "a1"), Ordering::Equal);
        assert_eq!(fallback("a0", "a00"), Ordering::Equal);
    }

    #[test]
    fn hanzi_outside_gb2312_level_one_sort_after_the_common_ones_without_panicking() {
        // 「𠀀」(U+20000) 在 GBK 里没有，排在常用字后面；两个这样的字按码位排，顺序稳定。
        assert_eq!(fallback("阿", "\u{20000}"), Ordering::Less);
        assert_eq!(fallback("\u{20000}", "\u{20001}"), Ordering::Less);
        assert_eq!(fallback("\u{20000}", "\u{20000}"), Ordering::Equal);
        // 空串、只有符号。
        assert_eq!(fallback("", ""), Ordering::Equal);
        assert_eq!(fallback("", "阿"), Ordering::Less);
    }

    #[test]
    fn the_order_is_total_and_antisymmetric_on_a_mixed_sample() {
        let sample = [
            "",
            "a",
            "A",
            "a1",
            "a01",
            "a10",
            "web-1",
            "web_1",
            "阿",
            "阿里",
            "百度",
            "张三",
            "\u{20000}",
            "龘",
            "日本語",
            "한국어",
            "é",
            "e",
            "10",
            "9",
        ];
        for x in sample {
            for y in sample {
                assert_eq!(fallback(x, y), fallback(y, x).reverse(), "{x:?} vs {y:?}");
                for z in sample {
                    if fallback(x, y) != Ordering::Greater && fallback(y, z) != Ordering::Greater {
                        assert_ne!(
                            fallback(x, z),
                            Ordering::Greater,
                            "传递性 {x:?} <= {y:?} <= {z:?}"
                        );
                    }
                }
            }
        }
    }
}
