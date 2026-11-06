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
    // Unix 按 POSIX locale 判断；LC_ALL 优先于 LC_MESSAGES，最后才是 LANG。
    #[cfg(not(windows))]
    {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .filter_map(|key| std::env::var(key).ok())
            .find(|value| !value.is_empty())
            .is_some_and(|value| value.to_ascii_lowercase().starts_with("zh"))
    }
}
