//! 应用自带的图标：组件库默认只内置 101 个图标，主机页、右键菜单、面板里用到的另外几个
//! （铅笔、垃圾桶、服务器、网格 / 列表视图、新分组、钥匙……）从 Lucide 里按需挑出来，
//! 只把用到的 SVG 编进程序（每个约几百字节），不用把整套 1800 多个都带上。
//!
//! 默认图标仍然走 `gpui_kit::component::IconName`；这里的走 [`Ico`]，两种都能传给 `Icon::new`。

use gpui_kit::AssetSource;
use gpui_kit::assets::{Assets, icon_assets};

pub use gpui_kit::assets::IconName as Ico;

icon_assets!(
    AppIconSet,
    [
        ArrowDownAZ,
        ArrowLeftRight,
        ArrowLeftToLine,
        ArrowRightToLine,
        ArrowUpDown,
        ArrowUpRight,
        Brain,
        Cable,
        Command,
        FolderOpen,
        FolderPlus,
        FolderTree,
        CircleGauge,
        Clock,
        ClipboardPaste,
        Columns2,
        Download,
        FileArchive,
        FilePlus,
        FolderInput,
        Lock,
        PackageOpen,
        SquareArrowDownLeft,
        SquareArrowOutUpRight,
        SquarePlus,
        Upload,
        FileSymlink,
        FileText,
        ListRestart,
        RotateCcw,
        Keyboard,
        KeyRound,
        LayoutGrid,
        Link2,
        List,
        ListChecks,
        CloudUpload,
        Monitor,
        MonitorCog,
        Pencil,
        Radio,
        Rows2,
        Rows3,
        SendHorizontal,
        Server,
        SquarePen,
        ShieldAlert,
        ShieldCheck,
        Terminal,
        Trash,
        Unplug,
        X,
        Zap
    ]
);

/// 应用自己画的图标（原创，不是从图标库里挑的）：文件夹的两层（后板 + 前板），见 [`folder_icon`]。
const OWN_ICONS: &[(&str, &[u8])] = &[
    (
        "zeeterm/folder-back.svg",
        include_bytes!("../assets/icons/folder-back.svg"),
    ),
    (
        "zeeterm/gear.svg",
        include_bytes!("../assets/icons/gear.svg"),
    ),
    (
        "zeeterm/folder-front.svg",
        include_bytes!("../assets/icons/folder-front.svg"),
    ),
    (
        "zeeterm/folder-open-front.svg",
        include_bytes!("../assets/icons/folder-open-front.svg"),
    ),
];

/// 设置齿轮（原创）：8 个圆头的齿、细线、中心一个明显的圆孔，风格取自 macOS / iOS 系统设置，但不是 Apple 的字形
/// （SF Symbols 的许可不允许分发），线条是自己用程序按几何算出来的。侧栏左下的「设置」和标题栏折叠后的入口共用。
pub fn gear_icon() -> gpui_kit::component::Icon {
    gpui_kit::component::Icon::empty().path("zeeterm/gear.svg")
}

/// 固定的黄色文件夹（不随皮肤变）：前板 / 后板的颜色，接近 Windows 资源管理器的文件夹黄。
pub const FOLDER_BODY: u32 = 0xF8C33A;
pub const FOLDER_EDGE: u32 = 0xD9A21B;

/// 实心黄色文件夹：后板（深一点的黄）+ 前板（浅一点的黄）两层单色 SVG 叠起来，展开时前板是斜开的。
/// `size` 是边长（逻辑像素）。颜色固定，选中 / 悬停行上也不变。
pub fn folder_icon(open: bool, size: f32) -> ::gpui::Div {
    use ::gpui::{ParentElement as _, Styled as _, div, px, rgb, svg};
    let layer = |path: &'static str, colour: u32| {
        svg()
            .path(path)
            .absolute()
            .top_0()
            .left_0()
            .size(px(size))
            .text_color(rgb(colour))
    };
    div()
        .relative()
        .flex_none()
        .size(px(size))
        .child(layer("zeeterm/folder-back.svg", FOLDER_EDGE))
        .child(layer(
            if open {
                "zeeterm/folder-open-front.svg"
            } else {
                "zeeterm/folder-front.svg"
            },
            FOLDER_BODY,
        ))
}

/// 组件库默认图标 + 应用自己挑的这几个。
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = OWN_ICONS.iter().find(|(name, _)| *name == path) {
            return Ok(Some(std::borrow::Cow::Borrowed(*bytes)));
        }
        if let Some(bytes) = AppIconSet.load(path)? {
            return Ok(Some(bytes));
        }
        Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<gpui_kit::SharedString>> {
        let mut paths = Assets.list(path)?;
        paths.extend(AppIconSet.list(path)?);
        paths.extend(
            OWN_ICONS
                .iter()
                .filter(|(name, _)| name.starts_with(path))
                .map(|(name, _)| gpui_kit::SharedString::from(*name)),
        );
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_picked_icon_loads_and_the_defaults_still_do() {
        let source = AppAssets;
        for icon in [
            Ico::ArrowDownAZ,
            Ico::ArrowLeftRight,
            Ico::ArrowLeftToLine,
            Ico::ArrowRightToLine,
            Ico::ArrowUpDown,
            Ico::ArrowUpRight,
            Ico::Command,
            Ico::FolderOpen,
            Ico::FolderPlus,
            Ico::FolderTree,
            Ico::CircleGauge,
            Ico::Clock,
            Ico::ClipboardPaste,
            Ico::Columns2,
            Ico::Download,
            Ico::FileArchive,
            Ico::FilePlus,
            Ico::FolderInput,
            Ico::Lock,
            Ico::PackageOpen,
            Ico::SquareArrowDownLeft,
            Ico::SquareArrowOutUpRight,
            Ico::SquarePlus,
            Ico::Upload,
            Ico::FileSymlink,
            Ico::FileText,
            Ico::ListRestart,
            Ico::RotateCcw,
            Ico::Keyboard,
            Ico::KeyRound,
            Ico::LayoutGrid,
            Ico::Link2,
            Ico::List,
            Ico::ListChecks,
            Ico::CloudUpload,
            Ico::Monitor,
            Ico::MonitorCog,
            Ico::Pencil,
            Ico::Radio,
            Ico::Rows2,
            Ico::Rows3,
            Ico::SendHorizontal,
            Ico::Server,
            Ico::ShieldAlert,
            Ico::ShieldCheck,
            Ico::Brain,
            Ico::SquarePen,
            Ico::Trash,
            Ico::Unplug,
            Ico::X,
            Ico::Zap,
        ] {
            let bytes = source
                .load(&icon.path())
                .expect("读图标")
                .unwrap_or_else(|| panic!("{icon:?} 没有编进来"));
            assert!(bytes.starts_with(b"<svg"), "{icon:?} 不是 SVG");
        }
        assert!(
            source
                .load("icons/folder.svg")
                .expect("读默认图标")
                .is_some(),
            "默认图标不能丢"
        );
    }

    #[test]
    fn the_folder_icon_is_two_own_svg_layers_in_fixed_yellows() {
        let source = AppAssets;
        for path in [
            "zeeterm/folder-back.svg",
            "zeeterm/folder-front.svg",
            "zeeterm/folder-open-front.svg",
        ] {
            let bytes = source
                .load(path)
                .expect("读图标")
                .unwrap_or_else(|| panic!("{path} 没有编进来"));
            assert!(bytes.starts_with(b"<svg"), "{path} 不是 SVG");
            assert!(
                source
                    .list("zeeterm/")
                    .unwrap()
                    .iter()
                    .any(|p| p.as_ref() == path)
            );
        }
        // 卡里定的两个黄色：前板（主体）F8C33A，后板（边 / 折角）D9A21B；常量不随皮肤变。
        assert_eq!(FOLDER_BODY, 0xF8C33A);
        assert_eq!(FOLDER_EDGE, 0xD9A21B);
    }

    #[test]
    fn the_settings_gear_is_one_own_svg_used_by_every_settings_entry() {
        let bytes = AppAssets
            .load("zeeterm/gear.svg")
            .expect("读图标")
            .expect("齿轮没有编进来");
        let text = std::str::from_utf8(&bytes).expect("SVG 是文本");
        assert!(text.starts_with("<svg") && text.contains("currentColor"), "线条图标，颜色跟着文字色走");
        assert!(text.contains("<circle"), "中心要有圆孔");
        // 侧栏左下的「设置」和标题栏折叠后的齿轮都走 gear_icon，不再用组件库的 Settings。
        for (name, text) in [
            ("sidebar.rs", include_str!("main_window/sidebar.rs")),
            ("titlebar.rs", include_str!("main_window/titlebar.rs")),
        ] {
            let code = text.split("#[cfg(test)]").next().unwrap_or(text);
            assert!(code.contains("gear_icon()"), "{name} 没用 gear_icon");
            assert!(!code.contains("IconName::Settings"), "{name} 里还有旧齿轮");
        }
    }

    #[test]
    fn tree_and_sftp_folders_use_the_yellow_icon_not_a_skin_coloured_outline() {
        // 主机树的分组、SFTP 列表里的文件夹不能再用组件库的线框 Folder / FolderOpen 图标。
        for (name, text) in [
            ("sidebar.rs", include_str!("main_window/sidebar.rs")),
            ("sftp_demo/view.rs", include_str!("sftp_demo/view.rs")),
        ] {
            let code = text.split("#[cfg(test)]").next().unwrap_or(text);
            assert!(
                !code.contains("IconName::Folder"),
                "{name} 里还有线框文件夹图标"
            );
            assert!(code.contains("folder_icon("), "{name} 没用 folder_icon");
        }
    }
}
