//! Windows 的系统版本号（build）：界面要按它决定能不能提供 Mica 这类只在新版本上有的窗口材质。
//!
//! 只读注册表里的 `CurrentBuildNumber`（普通用户可读，不会像 `GetVersionEx` 那样被应用兼容层改写）。
//! 其他平台没有这个概念，返回 `None`。

/// 当前 Windows 的 build 号（Windows 11 22H2 是 22621）；读不到、不是 Windows 都是 `None`。
pub fn windows_build_number() -> Option<u32> {
    #[cfg(windows)]
    {
        use std::sync::OnceLock;
        // 一次启动里不会变，不必每次问注册表。
        static BUILD: OnceLock<Option<u32>> = OnceLock::new();
        *BUILD.get_or_init(read_build_number)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// 注册表里 `CurrentBuildNumber` 的文本（形如 `22631`，可能带结尾的 NUL）转成数字。
/// 单独成函数是为了不碰注册表也能测。
pub fn parse_build_number(text: &str) -> Option<u32> {
    let digits = text.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

#[cfg(windows)]
fn read_build_number() -> Option<u32> {
    use windows::core::w;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

    let mut buffer = [0u16; 16];
    let mut size = std::mem::size_of_val(&buffer) as u32;
    // SAFETY: 缓冲区和长度都指向本函数的局部变量；只读系统注册表。
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
            w!("CurrentBuildNumber"),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast::<core::ffi::c_void>()),
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let units = (size as usize / 2).min(buffer.len());
    parse_build_number(&String::from_utf16_lossy(&buffer[..units]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_numbers_parse_with_or_without_trailing_nul() {
        assert_eq!(parse_build_number("22631"), Some(22631));
        assert_eq!(parse_build_number("22631\0"), Some(22631));
        assert_eq!(parse_build_number(" 19045 "), Some(19045));
    }

    #[test]
    fn garbage_is_not_a_build_number() {
        for bad in ["", "\0", "abc", "22.631", "-1", "22631x"] {
            assert_eq!(parse_build_number(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn reading_the_real_build_never_panics() {
        // 值取决于这台机器，只验证接口可用；Windows 上读得到时必须是个像样的 build 号。
        if let Some(build) = windows_build_number() {
            assert!(build >= 7600, "{build}");
        }
    }
}
