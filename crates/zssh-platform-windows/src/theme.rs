//! 系统主题：Windows 读注册表；Unix 用桌面门户给的明暗（由界面层从 GPUI 取来传入），读不到才看 GTK_THEME。
//!
//! 只读系统设置，不改任何东西。

#[cfg(windows)]
use windows::core::w;
#[cfg(windows)]
use windows::Win32::Foundation::ERROR_SUCCESS;
#[cfg(windows)]
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

/// 桌面（xdg-desktop-portal 的 color-scheme）告诉应用的明暗偏好。
///
/// GPUI 把门户的"深色 / 浅色 / 无偏好"折成 `Dark` / `Light`，"无偏好"和"门户不可用"都显示成
/// `Light`，所以 `Light` 不能当成"系统明确是浅色"，只能当成"没听到要深色"。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortalScheme {
    Dark,
    Light,
}

/// `Some(true)` 表示系统是浅色模式；值不存在或读取失败返回 `None`。
///
/// 不带门户信息的版本：Windows 读注册表，Unix 只看 `GTK_THEME`。Unix 上的界面请用
/// [`apps_use_light_theme_with`] 把 GPUI 读到的门户明暗传进来。
pub fn apps_use_light_theme() -> Option<bool> {
    apps_use_light_theme_with(None)
}

/// 同 [`apps_use_light_theme`]，Unix 上额外接收界面层从 GPUI（xdg-desktop-portal）读到的明暗。
/// `portal` 在 Windows 上被忽略。
pub fn apps_use_light_theme_with(portal: Option<PortalScheme>) -> Option<bool> {
    // 验证据点：用文件覆盖系统值，避免改用户的系统主题。只在调试构建里读：
    // 发行版里一个环境变量就能改掉主题判断，不该留这条路。
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("ZSSH_THEME_FILE") {
        let value = std::fs::read_to_string(path).ok()?;
        return value.trim().parse::<u32>().ok().map(|value| value != 0);
    }

    #[cfg(windows)]
    {
        let _ = portal;
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

    #[cfg(not(windows))]
    {
        resolve_unix(portal, std::env::var("GTK_THEME").ok().as_deref())
    }
}

/// Unix 的判定（纯函数，环境变量由调用方读好传入，测试里不用碰进程环境）。
///
/// - 门户说深色：深色。
/// - 门户没说深色（浅色 / 无偏好 / 门户不可用，GPUI 区分不了）：`GTK_THEME` 点名了主题就按名字里有没有
///   `dark` 判断（和 GTK 程序一样，这个变量是会话级的显式覆盖）；没设就是浅色。
/// - 完全没有门户信息（`portal == None`，例如早期没有窗口的时候）：只看 `GTK_THEME`，没设就是 `None`，
///   交给上层用已保存的明暗，不假装知道系统主题。
pub fn resolve_unix(portal: Option<PortalScheme>, gtk_theme: Option<&str>) -> Option<bool> {
    let by_gtk = gtk_theme
        .map(str::trim)
        .filter(|theme| !theme.is_empty())
        .map(|theme| !theme.to_ascii_lowercase().contains("dark"));
    match portal {
        Some(PortalScheme::Dark) => Some(false),
        Some(PortalScheme::Light) => Some(by_gtk.unwrap_or(true)),
        None => by_gtk,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_portal_saying_dark_wins() {
        assert_eq!(resolve_unix(Some(PortalScheme::Dark), None), Some(false));
        // 即使 GTK_THEME 指了个浅色主题，门户明确说深色就是深色
        assert_eq!(
            resolve_unix(Some(PortalScheme::Dark), Some("Adwaita")),
            Some(false)
        );
    }

    #[test]
    fn a_light_portal_reading_is_light_unless_gtk_theme_names_a_dark_theme() {
        assert_eq!(resolve_unix(Some(PortalScheme::Light), None), Some(true));
        assert_eq!(
            resolve_unix(Some(PortalScheme::Light), Some("")),
            Some(true)
        );
        assert_eq!(
            resolve_unix(Some(PortalScheme::Light), Some("Adwaita")),
            Some(true)
        );
        assert_eq!(
            resolve_unix(Some(PortalScheme::Light), Some("Adwaita:dark")),
            Some(false)
        );
        assert_eq!(
            resolve_unix(Some(PortalScheme::Light), Some("  Yaru-DARK ")),
            Some(false)
        );
    }

    #[test]
    fn without_any_portal_reading_only_gtk_theme_counts() {
        assert_eq!(resolve_unix(None, None), None);
        assert_eq!(resolve_unix(None, Some("")), None);
        assert_eq!(resolve_unix(None, Some("Adwaita:dark")), Some(false));
        assert_eq!(resolve_unix(None, Some("Adwaita")), Some(true));
    }
}
