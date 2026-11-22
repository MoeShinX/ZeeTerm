//! 系统界面语言：只用来决定「跟随系统」时用中文还是英文（界面语言只有这两种）。

/// 系统界面语言是不是中文。主语言 ID 0x04 = 中文（简体、繁体都算）；其余一律按英文处理。
pub fn system_is_chinese() -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::Globalization::GetUserDefaultUILanguage;
        // SAFETY: 无参数、无副作用，只读当前用户的语言设置。
        let lang_id = unsafe { GetUserDefaultUILanguage() };
        (lang_id & 0x3ff) == 0x04
    }
    // macOS：从 Finder / Dock 启动的程序一般没有 LANG，系统界面语言要问 NSLocale。
    #[cfg(target_os = "macos")]
    {
        macos::preferred_language_is_chinese()
    }
    // 其他 Unix 按 POSIX locale 判断；LC_ALL 优先于 LC_MESSAGES，最后才是 LANG。
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .filter_map(|key| std::env::var(key).ok())
            .find(|value| !value.is_empty())
            .is_some_and(|value| value.to_ascii_lowercase().starts_with("zh"))
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2_foundation::{NSArray, NSLocale, NSString};

    /// macOS 的首选语言（`NSLocale.preferredLanguages` 的第一项），以 `zh` 开头算中文。
    /// 界面语言只有中/英两种，所以只看首选语言，不再往下找备用语言。
    pub fn preferred_language_is_chinese() -> bool {
        let languages: objc2::rc::Retained<NSArray<NSString>> = NSLocale::preferredLanguages();
        let Some(first) = languages.firstObject() else {
            return false;
        };
        first.to_string().to_lowercase().starts_with("zh")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_detection_never_panics() {
        // 只验证接口可用；系统语言取决于这台机器，不能断言具体值。
        let _ = system_is_chinese();
    }
}
