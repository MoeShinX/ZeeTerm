//! 系统主题：Windows 读注册表；Unix 读 GTK_THEME。
//!
//! 只读系统设置，不改任何东西。

#[cfg(windows)]
use windows::core::w;
#[cfg(windows)]
use windows::Win32::Foundation::ERROR_SUCCESS;
#[cfg(windows)]
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

/// `Some(true)` 表示系统是浅色模式；值不存在或读取失败返回 `None`。
pub fn apps_use_light_theme() -> Option<bool> {
    // 验证据点：用文件覆盖系统值，避免改用户的系统主题。只在调试构建里读：
    // 发行版里一个环境变量就能改掉主题判断，不该留这条路。
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("ZSSH_THEME_FILE") {
        let value = std::fs::read_to_string(path).ok()?;
        return value.trim().parse::<u32>().ok().map(|value| value != 0);
    }

    #[cfg(windows)]
    {
        let mut value: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
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
        return (status == ERROR_SUCCESS).then_some(value != 0);
    }

    // GTK 桌面通常把偏好放在 GTK_THEME（如 Adwaita:dark）；读不到就交给上层用默认值，
    // 不假装知道系统主题。GNOME 的 color-scheme 读取留到平台设置阶段。
    #[cfg(not(windows))]
    {
        let theme = std::env::var("GTK_THEME").ok()?;
        Some(!theme.to_ascii_lowercase().contains("dark"))
    }
}
