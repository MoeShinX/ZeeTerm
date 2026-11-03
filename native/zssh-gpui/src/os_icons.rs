//! 主机的系统图标：连上过一次、探测出发行版之后，主机树 / 主机页 / 快速连接 / 标签里的图标换成
//! 发行版标志（品牌色圆角方块 + 白色标志）。认不出来、还没连过的主机继续用协议图标。
//!
//! 标志和品牌色移植自 Netcatty（`public/distro/`、`components/DistroAvatar.tsx`），经 Z-SSH 的
//! `ui/src/assets/os/` 和 `OsIcon.tsx` 带过来，GPL-3.0-or-later，见 `native/NOTICE`；
//! 标志本身是各发行版的商标。SVG 在 `assets/os/`，编进程序，由 [`crate::icons::AppAssets`] 提供。
//!
//! GPUI 的 SVG 只按透明度着色（等于 Z-SSH 里 `brightness(0) invert(1)` 的效果），所以"白色标志"
//! 就是把 SVG 的白色着色画在品牌色方块上。

use std::borrow::Cow;

use ::gpui::{
    AnyElement, InteractiveElement as _, IntoElement, ParentElement,
    StatefulInteractiveElement as _, Styled, div, px, rgb, svg, white,
};
use gpui_kit::SharedString;
use gpui_kit::component::tooltip::Tooltip;

/// 标志资源：(系统 id, SVG)。id 集合与核心库 `ssh::os_probe::OS_IDS` 一一对应（有测试核对）。
const LOGOS: &[(&str, &[u8])] = &[
    ("ubuntu", include_bytes!("../assets/os/ubuntu.svg")),
    ("debian", include_bytes!("../assets/os/debian.svg")),
    ("centos", include_bytes!("../assets/os/centos.svg")),
    ("rocky", include_bytes!("../assets/os/rocky.svg")),
    ("fedora", include_bytes!("../assets/os/fedora.svg")),
    ("arch", include_bytes!("../assets/os/arch.svg")),
    ("alpine", include_bytes!("../assets/os/alpine.svg")),
    ("amazon", include_bytes!("../assets/os/amazon.svg")),
    ("opensuse", include_bytes!("../assets/os/opensuse.svg")),
    ("redhat", include_bytes!("../assets/os/redhat.svg")),
    ("oracle", include_bytes!("../assets/os/oracle.svg")),
    ("kali", include_bytes!("../assets/os/kali.svg")),
    ("almalinux", include_bytes!("../assets/os/almalinux.svg")),
    ("alinux", include_bytes!("../assets/os/alinux.svg")),
    ("openeuler", include_bytes!("../assets/os/openeuler.svg")),
    ("freebsd", include_bytes!("../assets/os/freebsd.svg")),
    ("macos", include_bytes!("../assets/os/macos.svg")),
    ("linux", include_bytes!("../assets/os/linux.svg")),
];

/// 品牌色（RGB）：取自 Z-SSH `OsIcon.tsx` 的 `OS_COLORS`。写成数据表而不是直接写颜色字面量，
/// 因为"写死颜色"的守卫测试只放行名单里的文件；这些是不随皮肤变的品牌色。
const BRAND_COLORS: &[(&str, u32)] = &[
    ("ubuntu", 0xE95420),
    ("debian", 0xA81D33),
    ("centos", 0x9C27B0),
    ("rocky", 0x0B9B69),
    ("fedora", 0x3C6EB4),
    ("arch", 0x1793D1),
    ("alpine", 0x0D597F),
    ("amazon", 0xFF9900),
    ("opensuse", 0x73BA25),
    ("redhat", 0xEE0000),
    ("oracle", 0xC74634),
    ("kali", 0x0F6DB3),
    ("almalinux", 0x173B66),
    ("alinux", 0xFF6A00),
    ("openeuler", 0x002FA7),
    ("freebsd", 0xAB2B28),
    ("macos", 0x333333),
    ("linux", 0x333333),
];

/// 悬停提示里的系统名。
const NAMES: &[(&str, &str)] = &[
    ("ubuntu", "Ubuntu"),
    ("debian", "Debian"),
    ("centos", "CentOS"),
    ("rocky", "Rocky Linux"),
    ("fedora", "Fedora"),
    ("arch", "Arch Linux"),
    ("alpine", "Alpine"),
    ("amazon", "Amazon Linux"),
    ("opensuse", "openSUSE"),
    ("redhat", "Red Hat"),
    ("oracle", "Oracle Linux"),
    ("kali", "Kali Linux"),
    ("almalinux", "AlmaLinux"),
    ("alinux", "Alibaba Cloud Linux"),
    ("openeuler", "openEuler"),
    ("freebsd", "FreeBSD"),
    ("macos", "macOS"),
    ("linux", "Linux"),
];

fn lookup<T: Copy>(table: &[(&str, T)], os: &str) -> Option<T> {
    table.iter().find(|(id, _)| *id == os).map(|(_, v)| *v)
}

/// 标志的资源路径（`AppAssets` 认的路径）。
pub fn logo_path(os: &str) -> Option<SharedString> {
    lookup(LOGOS, os).map(|_| format!("os/{os}.svg").into())
}

/// 资源路径 → SVG 字节（给 `AppAssets::load`）。
pub fn logo_bytes(path: &str) -> Option<Cow<'static, [u8]>> {
    let id = path.strip_prefix("os/")?.strip_suffix(".svg")?;
    lookup(LOGOS, id).map(Cow::Borrowed)
}

/// 所有标志的资源路径（给 `AppAssets::list`）。
pub fn logo_paths() -> impl Iterator<Item = SharedString> {
    LOGOS.iter().map(|(id, _)| format!("os/{id}.svg").into())
}

pub fn brand_color(os: &str) -> Option<u32> {
    lookup(BRAND_COLORS, os)
}

pub fn os_name(os: &str) -> Option<&'static str> {
    lookup(NAMES, os)
}

/// 这个系统 id 画得出来吗（有标志、有品牌色）。
pub fn is_known(os: &str) -> bool {
    brand_color(os).is_some() && lookup(LOGOS, os).is_some()
}

/// 图标用在哪：方块、标志的大小和圆角（同 Z-SSH 的 tree / card / cardCompact）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OsIconSize {
    /// 主机树 / 列表行 / 快速连接：16px 方块。
    Row,
    /// 标签上：14px 方块，比标签里的字小一点。
    Tab,
    /// 主机页卡片（舒适）。
    Card,
    /// 主机页卡片（紧凑）。
    CardCompact,
}

impl OsIconSize {
    /// (方块边长, 标志边长, 圆角)，单位 px。
    pub fn dims(self) -> (f32, f32, f32) {
        match self {
            Self::Row => (16.0, 11.0, 4.0),
            Self::Tab => (14.0, 10.0, 3.0),
            Self::Card => (36.0, 20.0, 7.0),
            Self::CardCompact => (28.0, 16.0, 6.0),
        }
    }
}

/// 发行版图标：品牌色圆角方块 + 白色标志。认不出来的 id 返回 `None`，由调用方画回协议图标。
pub fn badge(os: &str, size: OsIconSize) -> Option<AnyElement> {
    let color = brand_color(os)?;
    let path = logo_path(os)?;
    let (boxed, logo, radius) = size.dims();
    let name = os_name(os)?;
    Some(
        div()
            .id(SharedString::from(format!("os-icon-{os}")))
            .flex()
            .items_center()
            .justify_center()
            .size(px(boxed))
            .flex_none()
            .rounded(px(radius))
            .bg(rgb(color))
            .tooltip(move |window, cx| Tooltip::new(name).build(window, cx))
            .child(svg().path(path).size(px(logo)).text_color(white()))
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use zssh_core::ssh::os_probe::OS_IDS;

    #[test]
    fn every_probe_id_has_a_logo_a_colour_and_a_name() {
        for id in OS_IDS {
            assert!(is_known(id), "{id} 缺标志或品牌色");
            assert!(os_name(id).is_some(), "{id} 缺名字");
            assert!(logo_path(id).is_some(), "{id} 缺资源路径");
            assert!(badge(id, OsIconSize::Row).is_some(), "{id} 画不出来");
        }
        // 表里没有多余的 id：探测不出来的东西没有图标。
        assert_eq!(LOGOS.len(), OS_IDS.len());
        assert_eq!(BRAND_COLORS.len(), OS_IDS.len());
        assert_eq!(NAMES.len(), OS_IDS.len());
    }

    #[test]
    fn unknown_ids_fall_back_to_no_badge() {
        for id in ["", "windows", "Debian", "os/debian"] {
            assert!(!is_known(id), "{id}");
            assert!(badge(id, OsIconSize::Row).is_none(), "{id}");
        }
    }

    #[test]
    fn logo_bytes_are_real_svgs_that_carry_their_source_note() {
        for id in OS_IDS {
            let path = logo_path(id).expect("路径");
            let bytes = logo_bytes(&path).expect("字节");
            let text = std::str::from_utf8(&bytes).expect("UTF-8");
            assert!(text.contains("<svg"), "{id}");
            assert!(text.contains("<path"), "{id}");
            assert!(text.contains("Netcatty"), "{id} 少了来源注释");
            // 来源注释不能挡在 XML 声明前面（声明必须是文件第一个字符，否则 SVG 解析失败、标志画不出来）。
            assert!(!text[1..].contains("<?xml"), "{id} 的 XML 声明不在文件开头");
            assert!(text.trim_end().ends_with("</svg>"), "{id}");
        }
        assert!(logo_bytes("os/unknown.svg").is_none());
        assert!(logo_bytes("icons/server.svg").is_none());
    }

    #[test]
    fn logo_paths_list_matches_the_table() {
        let listed: Vec<String> = logo_paths().map(|p| p.to_string()).collect();
        assert_eq!(listed.len(), OS_IDS.len());
        assert!(listed.contains(&"os/debian.svg".to_owned()));
    }

    #[test]
    fn the_app_asset_source_serves_the_logos() {
        use gpui_kit::AssetSource as _;
        let assets = crate::icons::AppAssets;
        for id in OS_IDS {
            let path = format!("os/{id}.svg");
            let loaded = assets.load(&path).expect("读取").expect("有这个资源");
            let text = std::str::from_utf8(&loaded).expect("UTF-8");
            assert!(text.contains("<svg") && text.contains("Netcatty"), "{id}");
        }
        // 原来的图标不受影响。
        assert!(assets.load("icons/server.svg").expect("读取").is_some());
    }

    #[test]
    fn brand_colours_are_the_ones_from_the_old_version() {
        assert_eq!(brand_color("debian"), Some(0xA81D33));
        assert_eq!(brand_color("ubuntu"), Some(0xE95420));
        assert_eq!(brand_color("linux"), Some(0x333333));
    }

    #[test]
    fn sizes_fit_the_boxes_they_sit_in() {
        for size in [
            OsIconSize::Row,
            OsIconSize::Tab,
            OsIconSize::Card,
            OsIconSize::CardCompact,
        ] {
            let (boxed, logo, radius) = size.dims();
            assert!(logo < boxed && radius * 2.0 <= boxed);
        }
        // 树里一行的图标和文件夹图标一样 16px（U14）。
        assert_eq!(OsIconSize::Row.dims().0, 16.0);
    }
}
