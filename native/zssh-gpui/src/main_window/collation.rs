//! 中文名字的排序规则：和 0.2.14 前端的 `Intl.Collator("zh-Hans-CN", { numeric: true,
//! sensitivity: "base" })` 一致——中文按拼音、数字按数值、英文不分大小写。
//!
//! Windows 用 `CompareStringEx`，macOS 用 CoreFoundation 的显式中文拼音区域；
//! 不依赖用户当前语言，也不需要内置拼音表。系统调用失败时退回按字符比较，
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

#[cfg(target_os = "macos")]
fn system_compare(a: &str, b: &str) -> Option<Ordering> {
    use std::ffi::c_void;

    #[repr(C)]
    struct CFRange {
        location: isize,
        length: isize,
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCharacters(
            allocator: *const c_void,
            chars: *const u16,
            count: isize,
        ) -> *const c_void;
        fn CFLocaleCreate(allocator: *const c_void, identifier: *const c_void) -> *const c_void;
        fn CFStringCompareWithOptionsAndLocale(
            a: *const c_void,
            b: *const c_void,
            range: CFRange,
            options: usize,
            locale: *const c_void,
        ) -> isize;
        fn CFRelease(value: *const c_void);
    }

    struct OwnedCf(*const c_void);
    impl Drop for OwnedCf {
        fn drop(&mut self) {
            // SAFETY: 只持有 Create 返回的非空对象，每个对象恰好释放一次。
            unsafe { CFRelease(self.0) };
        }
    }
    fn string(text: &str) -> Option<(OwnedCf, isize)> {
        let utf16: Vec<u16> = text.encode_utf16().collect();
        let length = isize::try_from(utf16.len()).ok()?;
        // SAFETY: CFString 复制调用期间有效的 UTF-16 切片；空 allocator 使用默认分配器。
        let value =
            unsafe { CFStringCreateWithCharacters(std::ptr::null(), utf16.as_ptr(), length) };
        if value.is_null() {
            None
        } else {
            Some((OwnedCf(value), length))
        }
    }

    let (left, length) = string(a)?;
    let (right, _) = string(b)?;
    let (identifier, _) = string("zh_CN@collation=pinyin")?;
    // SAFETY: identifier 是有效 CFString，Create 的所有权由 OwnedCf 接管。
    let locale = unsafe { CFLocaleCreate(std::ptr::null(), identifier.0) };
    if locale.is_null() {
        return None;
    }
    let locale = OwnedCf(locale);
    // CaseInsensitive | Nonliteral | Localized | Numerically | DiacriticInsensitive。
    // 显式区域保证中文拼音顺序，不受系统显示语言影响。
    let options = 1 | 16 | 32 | 64 | 128;
    // SAFETY: 字符串和区域在调用期间有效；范围恰好覆盖左字符串的 UTF-16 长度。
    let result = unsafe {
        CFStringCompareWithOptionsAndLocale(
            left.0,
            right.0,
            CFRange {
                location: 0,
                length,
            },
            options,
            locale.0,
        )
    };
    Some(result.cmp(&0))
}

#[cfg(not(any(windows, target_os = "macos")))]
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
