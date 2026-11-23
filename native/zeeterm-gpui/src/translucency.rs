//! 终端背景半透明 + 窗口背景材质（V2-U60，轻量版毛玻璃）。
//!
//! 只有终端区域透明：不透明度 100%（默认）时窗口永远是 `Opaque`，绘制路径和以前逐位一致；
//! 低于 100% 才把窗口切成设置里选的背景效果，终端的默认背景色按这个不透明度画，
//! 侧栏、标题栏、标签栏、对话框、设置页这些界面一律保持实色。
//!
//! 这个模块只放「设置 → 窗口背景取值」的纯函数和把取值套到窗口上的薄胶水；
//! 终端怎么画、哪些界面要垫实底在各自的绘制代码里，判断都以这里的 [`is_translucent`] 为准。
//!
//! 各平台上 GPUI（gpui-pre 0.3.10）对每种取值的实际行为（读源码得到，见报告）：
//! - Windows 10 1809（build 17763）起：`Blurred` = 亚克力（`SetWindowCompositionAttribute`，
//!   模糊背后的窗口）；`Transparent` = 纯透明（看到背后的窗口，不模糊）；`MicaBackdrop` 在 build 22621
//!   以下什么都不做（窗口只是透明）。
//! - Windows 11 22H2（build 22621）起：`MicaBackdrop` 是 Mica 材质（取桌面壁纸的模糊色）。
//!   21H2（build 22000～22620）有 Windows 11 外观但没有 Mica 接口，所以 Mica 以 22621 为准。
//! - Linux Wayland：`Blurred` 只在合成器支持 `org_kde_kwin_blur`（KDE）时才真的模糊，否则就是透明；
//!   `Transparent` 和 Mica 都是透明（Mica 在 Linux 上没有意义，当透明）。
//! - Linux X11：所有非 `Opaque` 取值都只是「带透明通道的窗口」，没有模糊；是否透出桌面取决于有没有合成器。
//! - 还有一点：只要不是 `Opaque`，GPUI 整个窗口的文字都改成灰度抗锯齿（不再用 ClearType 子像素），
//!   这是半透明窗口的通用限制，100% 时不受影响。

use ::gpui::WindowBackgroundAppearance;
use serde::{Deserialize, Serialize};

/// 「终端背景不透明度」设置页滑块的范围和步长（百分数）。
pub const OPACITY_MIN: u8 = 50;
pub const OPACITY_MAX: u8 = 100;
pub const OPACITY_STEP: u8 = 5;
/// 默认 100%：窗口 `Opaque`，和没有这个功能时完全一样。
pub const OPACITY_DEFAULT: u8 = 100;

/// GPUI 能设上 Mica 的最低 Windows build（Windows 11 22H2）；更老的系统上设了也没有效果。
pub const MICA_MIN_BUILD: u32 = 22621;

/// 「背景效果」三选一。只在不透明度低于 100% 时生效。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowBackdrop {
    /// 模糊背后的内容（Windows 亚克力；Linux 取决于桌面环境）。
    Blur,
    /// 纯透明，直接看到背后的窗口和桌面。
    Transparent,
    /// Windows 11 的 Mica 材质。
    Mica,
}

impl WindowBackdrop {
    /// 这台电脑的默认值：能用 Mica 就是 Mica，其他一律模糊。
    pub fn platform_default() -> Self {
        default_backdrop(local_build())
    }

    /// 设置文件里的名字；只有调试驱动（`ui_assert`）和测试要用，发行构建里没人调用。
    #[cfg(any(test, debug_assertions))]
    pub fn id(self) -> &'static str {
        match self {
            Self::Blur => "blur",
            Self::Transparent => "transparent",
            Self::Mica => "mica",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "blur" => Some(Self::Blur),
            "transparent" => Some(Self::Transparent),
            "mica" => Some(Self::Mica),
            _ => None,
        }
    }
}

/// 这台电脑的 Windows build 号；不是 Windows 或读不到是 `None`。
pub fn local_build() -> Option<u32> {
    zeeterm_platform_windows::os_build::windows_build_number()
}

/// 这个 build 能不能用 Mica（只有 Windows 11 22H2 及以上）。
pub fn mica_supported(build: Option<u32>) -> bool {
    build.is_some_and(|build| build >= MICA_MIN_BUILD)
}

/// 没选过时的默认背景效果：Windows 11（22H2+）是 Mica，其他平台是模糊。
pub fn default_backdrop(build: Option<u32>) -> WindowBackdrop {
    if mica_supported(build) {
        WindowBackdrop::Mica
    } else {
        WindowBackdrop::Blur
    }
}

/// 选的是 Mica 但这台电脑用不了（设置文件从别的电脑拷来、系统降级）时退回模糊。
pub fn effective_backdrop(choice: WindowBackdrop, build: Option<u32>) -> WindowBackdrop {
    if choice == WindowBackdrop::Mica && !mica_supported(build) {
        WindowBackdrop::Blur
    } else {
        choice
    }
}

/// 把不透明度（百分数）收到步长的整数倍并夹进 `50..=100`；不是有限数、超出范围一律 `None`（调用方按默认值处理）。
pub fn snap_opacity(raw: f64) -> Option<u8> {
    if !raw.is_finite() {
        return None;
    }
    let step = f64::from(OPACITY_STEP);
    let snapped = (raw / step).round() * step;
    if snapped < f64::from(OPACITY_MIN) || snapped > f64::from(OPACITY_MAX) {
        return None;
    }
    Some(snapped as u8)
}

/// 不透明度低于 100%：窗口要切成非 `Opaque`，终端默认背景要按 alpha 画，别的界面要垫实底。
pub fn is_translucent(opacity: u8) -> bool {
    opacity < OPACITY_MAX
}

/// 终端默认背景色的 alpha（0.5～1.0）。
pub fn alpha(opacity: u8) -> f32 {
    f32::from(opacity.clamp(OPACITY_MIN, OPACITY_MAX)) / 100.0
}

/// 设置 → 窗口背景取值。100%（以及任何不低于 100% 的值）一律 `Opaque`；低于 100% 按选项映射，
/// Mica 在用不了的系统上退回模糊。纯函数，`build` 由调用方传入。
pub fn window_appearance(
    opacity: u8,
    backdrop: WindowBackdrop,
    build: Option<u32>,
) -> WindowBackgroundAppearance {
    if !is_translucent(opacity) {
        return WindowBackgroundAppearance::Opaque;
    }
    match effective_backdrop(backdrop, build) {
        WindowBackdrop::Blur => WindowBackgroundAppearance::Blurred,
        WindowBackdrop::Transparent => WindowBackgroundAppearance::Transparent,
        WindowBackdrop::Mica => WindowBackgroundAppearance::MicaBackdrop,
    }
}

/// 这台电脑上、这份设置对应的窗口背景取值。
pub fn appearance_for(opacity: u8, backdrop: WindowBackdrop) -> WindowBackgroundAppearance {
    window_appearance(opacity, backdrop, local_build())
}

/// 同上，直接从设置里取。
pub fn appearance_of(settings: &crate::settings::UiSettings) -> WindowBackgroundAppearance {
    appearance_for(settings.terminal_opacity, settings.window_backdrop)
}

/// 一个窗口上现在套着的背景效果（V2-U60）。窗口开出来时就是设置对应的值，之后每次 render 调 [`Self::sync`]：
/// 想要的和现在的一样就什么都不碰，所以默认的 100% 永远不会多调一次平台接口。
#[derive(Clone, Copy, Debug)]
pub struct BackdropState {
    applied: WindowBackgroundAppearance,
    /// Windows：Mica 现在按哪个明暗画（`Some(dark)`）；没套 Mica 是 `None`。
    mica_dark: Option<bool>,
}

impl BackdropState {
    /// `applied` 是窗口创建时传给 GPUI 的值（`window_background`）。
    pub fn new(applied: WindowBackgroundAppearance) -> Self {
        Self {
            applied,
            mica_dark: None,
        }
    }

    /// 窗口自己 render 时调用。`wanted` 是设置对应的取值，`dark` 是应用现在是不是深色（Mica 的深浅要跟它走）。
    /// 返回是否真的改了窗口。
    ///
    /// GPUI 切换取值时只开新的、不关旧的，也只按系统明暗给 Mica 定深浅，这里补三步：
    /// - 离开 Mica 时关掉系统背景材质，否则「透明」会变成 Mica，并把框架明暗还给系统；
    /// - 从模糊 / 透明切到 Mica 时先回到 `Opaque`，让上一个背景效果关干净；
    /// - 套着 Mica 时，应用的明暗一变（或刚套上）就把 Mica 的深浅对齐到应用的明暗。
    pub fn sync(
        &mut self,
        window: &mut ::gpui::Window,
        wanted: WindowBackgroundAppearance,
        dark: bool,
    ) -> bool {
        let mut changed = false;
        if self.applied != wanted {
            let was_mica = self.applied == WindowBackgroundAppearance::MicaBackdrop;
            let to_mica = wanted == WindowBackgroundAppearance::MicaBackdrop;
            if was_mica && !to_mica {
                clear_system_backdrop(window);
                if self.mica_dark.take().is_some() {
                    restore_system_frame_theme(window);
                }
            }
            if to_mica && self.applied != WindowBackgroundAppearance::Opaque {
                window.set_background_appearance(WindowBackgroundAppearance::Opaque);
            }
            window.set_background_appearance(wanted);
            self.applied = wanted;
            changed = true;
        }
        if self.applied == WindowBackgroundAppearance::MicaBackdrop && self.mica_dark != Some(dark)
        {
            set_frame_dark(window, dark);
            self.mica_dark = Some(dark);
            changed = true;
        }
        changed
    }
}

#[cfg(windows)]
fn win32_hwnd(window: &::gpui::Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(win32) => Some(win32.hwnd.get()),
        _ => None,
    }
}

#[cfg(windows)]
fn clear_system_backdrop(window: &::gpui::Window) {
    if let Some(hwnd) = win32_hwnd(window) {
        zeeterm_platform_windows::dwm::clear_system_backdrop(hwnd);
    }
}

#[cfg(windows)]
fn set_frame_dark(window: &::gpui::Window, dark: bool) {
    if let Some(hwnd) = win32_hwnd(window) {
        zeeterm_platform_windows::dwm::set_dark_frame(hwnd, dark);
    }
}

/// 离开 Mica：窗口框架的明暗还给系统（GPUI 创建窗口时就是按系统明暗设的）。
#[cfg(windows)]
fn restore_system_frame_theme(window: &::gpui::Window) {
    let dark = matches!(
        window.appearance(),
        ::gpui::WindowAppearance::Dark | ::gpui::WindowAppearance::VibrantDark
    );
    set_frame_dark(window, dark);
}

#[cfg(not(windows))]
fn clear_system_backdrop(_window: &::gpui::Window) {}

#[cfg(not(windows))]
fn set_frame_dark(_window: &::gpui::Window, _dark: bool) {}

#[cfg(not(windows))]
fn restore_system_frame_theme(_window: &::gpui::Window) {}

/// 主窗口的 `Root`（组件库）自己给整个窗口铺了一层不透明底色。半透明时把它改成透明，
/// 恢复不透明时还原成组件库的默认。放到本帧收尾再做：调用它的是 `Root` 的子视图，正在 render。
pub fn set_root_fill(window: &mut ::gpui::Window, cx: &mut ::gpui::App, translucent: bool) {
    use ::gpui::Styled as _;
    window.defer(cx, move |window, cx| {
        if window.root::<gpui_kit::base::Root>().flatten().is_none() {
            return;
        }
        gpui_kit::base::Root::update(window, cx, |root, _, cx| {
            root.style().background = translucent.then(|| ::gpui::transparent_black().into());
            cx.notify();
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use WindowBackgroundAppearance as A;

    const WIN10: Option<u32> = Some(19045);
    const WIN11_21H2: Option<u32> = Some(22000);
    const WIN11_22H2: Option<u32> = Some(22621);
    const WIN11_24H2: Option<u32> = Some(26100);

    #[test]
    fn full_opacity_is_always_opaque_whatever_the_backdrop() {
        for backdrop in [
            WindowBackdrop::Blur,
            WindowBackdrop::Transparent,
            WindowBackdrop::Mica,
        ] {
            for build in [None, WIN10, WIN11_21H2, WIN11_22H2, WIN11_24H2] {
                assert_eq!(window_appearance(100, backdrop, build), A::Opaque);
            }
        }
    }

    #[test]
    fn below_full_opacity_the_choice_maps_one_to_one() {
        for opacity in [50, 70, 95] {
            assert_eq!(
                window_appearance(opacity, WindowBackdrop::Blur, WIN11_22H2),
                A::Blurred
            );
            assert_eq!(
                window_appearance(opacity, WindowBackdrop::Transparent, WIN11_22H2),
                A::Transparent
            );
            assert_eq!(
                window_appearance(opacity, WindowBackdrop::Mica, WIN11_22H2),
                A::MicaBackdrop
            );
        }
    }

    #[test]
    fn mica_falls_back_to_blur_where_the_system_cannot_do_it() {
        for build in [None, WIN10, WIN11_21H2, Some(22620)] {
            assert_eq!(
                window_appearance(70, WindowBackdrop::Mica, build),
                A::Blurred,
                "{build:?}"
            );
            assert_eq!(
                effective_backdrop(WindowBackdrop::Mica, build),
                WindowBackdrop::Blur
            );
        }
        // 别的选项不受 build 影响。
        assert_eq!(
            window_appearance(70, WindowBackdrop::Transparent, WIN10),
            A::Transparent
        );
        assert_eq!(
            window_appearance(70, WindowBackdrop::Blur, None),
            A::Blurred
        );
    }

    #[test]
    fn the_default_backdrop_is_mica_only_where_mica_works() {
        assert_eq!(default_backdrop(WIN11_22H2), WindowBackdrop::Mica);
        assert_eq!(default_backdrop(WIN11_24H2), WindowBackdrop::Mica);
        assert_eq!(default_backdrop(Some(22620)), WindowBackdrop::Blur);
        assert_eq!(default_backdrop(WIN11_21H2), WindowBackdrop::Blur);
        assert_eq!(default_backdrop(WIN10), WindowBackdrop::Blur);
        assert_eq!(default_backdrop(None), WindowBackdrop::Blur);
    }

    #[test]
    fn mica_needs_windows_11_22h2() {
        assert!(!mica_supported(None));
        assert!(!mica_supported(WIN10));
        assert!(!mica_supported(WIN11_21H2));
        assert!(!mica_supported(Some(22620)));
        assert!(mica_supported(WIN11_22H2));
        assert!(mica_supported(WIN11_24H2));
    }

    #[test]
    fn opacity_snaps_to_the_step_and_rejects_out_of_range() {
        assert_eq!(snap_opacity(100.0), Some(100));
        assert_eq!(snap_opacity(50.0), Some(50));
        assert_eq!(snap_opacity(72.0), Some(70));
        assert_eq!(snap_opacity(73.0), Some(75));
        assert_eq!(snap_opacity(47.6), Some(50));
        assert_eq!(snap_opacity(102.0), Some(100));
        assert_eq!(snap_opacity(47.0), None);
        assert_eq!(snap_opacity(0.0), None);
        assert_eq!(snap_opacity(-5.0), None);
        assert_eq!(snap_opacity(150.0), None);
        assert_eq!(snap_opacity(f64::NAN), None);
        assert_eq!(snap_opacity(f64::INFINITY), None);
    }

    #[test]
    fn only_values_below_full_count_as_translucent() {
        assert!(!is_translucent(100));
        assert!(is_translucent(95));
        assert!(is_translucent(50));
    }

    #[test]
    fn alpha_is_the_percentage_as_a_fraction() {
        assert_eq!(alpha(100), 1.0);
        assert_eq!(alpha(70), 0.7);
        assert_eq!(alpha(50), 0.5);
        // 越界值不会画出全透明或超过 1 的 alpha。
        assert_eq!(alpha(0), 0.5);
        assert_eq!(alpha(200), 1.0);
    }

    #[test]
    fn backdrop_ids_round_trip() {
        for backdrop in [
            WindowBackdrop::Blur,
            WindowBackdrop::Transparent,
            WindowBackdrop::Mica,
        ] {
            assert_eq!(WindowBackdrop::parse(backdrop.id()), Some(backdrop));
            assert_eq!(
                serde_json::to_string(&backdrop).unwrap(),
                format!("\"{}\"", backdrop.id())
            );
        }
        assert_eq!(WindowBackdrop::parse("acrylic"), None);
        assert_eq!(WindowBackdrop::parse(""), None);
    }
}
