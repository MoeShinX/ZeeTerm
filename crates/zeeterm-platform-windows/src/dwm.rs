//! 窗口背景材质（Mica 之类）的收尾：GPUI 切换窗口背景效果时只会开新的，不会关掉上一个，
//! 也不知道应用自己选的明暗。
//!
//! GPUI 在 Windows 上用两条互不相干的路做背景效果：模糊 / 透明走 `SetWindowCompositionAttribute`，
//! Mica 走 `DwmSetWindowAttribute(DWMWA_SYSTEMBACKDROP_TYPE)`。从 Mica 切到「透明」或「模糊」时，
//! 系统背景材质还留在窗口上（透明就变成了 Mica），所以离开 Mica 时要在这里把它关掉。
//! Mica 的深浅取决于窗口的「深色框架」属性，GPUI 只按系统的明暗设它；应用里选了和系统相反的明暗时，
//! Mica 的深浅会和界面对不上，所以套上 Mica 时按应用自己的明暗再设一次。

#[cfg(windows)]
fn set_dword_attribute(
    hwnd: isize,
    attribute: windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE,
    value: i32,
) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::DwmSetWindowAttribute;

    // SAFETY: 句柄来自本进程自己的窗口；属性值是指向局部变量的 4 字节整数。
    let result = unsafe {
        DwmSetWindowAttribute(
            HWND(hwnd as *mut core::ffi::c_void),
            attribute,
            std::ptr::from_ref(&value).cast::<core::ffi::c_void>(),
            std::mem::size_of_val(&value) as u32,
        )
    };
    if let Err(err) = result {
        tracing::debug!(%err, "设置窗口 DWM 属性失败（系统可能不支持）");
    }
}

/// 去掉窗口上的系统背景材质（`DWMSBT_NONE`）。只在 Windows 11 22H2（build 22621）以上有意义，
/// 老系统上调用会失败，失败不影响任何东西。`hwnd` 是 Win32 窗口句柄。
#[cfg(windows)]
pub fn clear_system_backdrop(hwnd: isize) {
    use windows::Win32::Graphics::Dwm::{DWMSBT_NONE, DWMWA_SYSTEMBACKDROP_TYPE};
    set_dword_attribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, DWMSBT_NONE.0);
}

/// 设窗口框架的明暗（`DWMWA_USE_IMMERSIVE_DARK_MODE`）：决定 Mica 用深色还是浅色材质。
#[cfg(windows)]
pub fn set_dark_frame(hwnd: isize, dark: bool) {
    use windows::Win32::Graphics::Dwm::DWMWA_USE_IMMERSIVE_DARK_MODE;
    set_dword_attribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, i32::from(dark));
}

#[cfg(not(windows))]
pub fn clear_system_backdrop(_hwnd: isize) {}

#[cfg(not(windows))]
pub fn set_dark_frame(_hwnd: isize, _dark: bool) {}
