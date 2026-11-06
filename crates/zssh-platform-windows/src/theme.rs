//! 系统主题：读系统的浅色/深色偏好，不改任何设置。

/// `Some(true)` 表示系统是浅色模式；值不存在或读取失败返回 `None`。
pub fn apps_use_light_theme() -> Option<bool> {
    // 验证钩子：用文件覆盖系统值，避免改用户的系统主题。只在调试构建里读：
    // 发行版里一个环境变量就能改掉主题判断，不该留这条路。
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("ZSSH_THEME_FILE") {
        let value = std::fs::read_to_string(path).ok()?;
        return value.trim().parse::<u32>().ok().map(|value| value != 0);
    }
    #[cfg(windows)]
    {
        windows::apps_use_light_theme()
    }
    #[cfg(target_os = "macos")]
    {
        macos::apps_use_light_theme()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        None
    }
}

#[cfg(windows)]
mod windows {
    use windows::core::w;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

    pub fn apps_use_light_theme() -> Option<bool> {
        let mut value: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        // SAFETY: 路径、值名都是常量宽字符串；缓冲区是本地 u32，大小按字节给出。
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
                w!("AppsUseLightTheme"),
                RRF_RT_REG_DWORD,
                None,
                Some(&mut value as *mut _ as *mut core::ffi::c_void),
                Some(&mut size),
            )
        };
        (status == ERROR_SUCCESS).then_some(value != 0)
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2_foundation::{NSString, NSUserDefaults};

    /// macOS 的全局外观：`AppleInterfaceStyle` 存在且为 `Dark` 就是深色；没有这个键就是浅色。
    /// 用户没明确设置过时系统可能不写这个键，按浅色处理（和系统默认一致）。
    pub fn apps_use_light_theme() -> Option<bool> {
        let defaults = NSUserDefaults::standardUserDefaults();
        let key = NSString::from_str("AppleInterfaceStyle");
        match defaults.stringForKey(&key) {
            Some(style) => Some(!style.to_string().eq_ignore_ascii_case("dark")),
            None => Some(true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_detection_never_panics() {
        // 只验证接口可用；具体明暗取决于这台机器的系统设置，不能断言固定值。
        let _ = apps_use_light_theme();
    }
}
