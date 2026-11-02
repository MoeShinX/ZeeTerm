//! 系统主题：读 Windows 的浅色/深色偏好。
//!
//! 只读 `HKCU\...\Themes\Personalize\AppsUseLightTheme`，不改任何设置。

use windows::core::w;
use windows::Win32::Foundation::ERROR_SUCCESS;
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
    (status == ERROR_SUCCESS).then_some(value != 0)
}
