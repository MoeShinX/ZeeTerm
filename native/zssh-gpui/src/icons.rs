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
        Cable,
        Command,
        FolderOpen,
        FolderPlus,
        FolderTree,
        CircleGauge,
        Clock,
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
        Terminal,
        Trash,
        Unplug,
        X,
        Zap
    ]
);

/// 组件库默认图标 + 应用自己挑的这几个。
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some(bytes) = AppIconSet.load(path)? {
            return Ok(Some(bytes));
        }
        Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<gpui_kit::SharedString>> {
        let mut paths = Assets.list(path)?;
        paths.extend(AppIconSet.list(path)?);
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
}
