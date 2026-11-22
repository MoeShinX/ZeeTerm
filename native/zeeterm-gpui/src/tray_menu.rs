//! 托盘菜单有哪些项：纯数据，Windows 的 `tray.rs`（tray-icon）和 Linux 的 `tray_linux.rs`
//! （StatusNotifierItem）共用同一份，两边的菜单次序和文字永远一致，也不用起托盘就能测。

pub(crate) const SHOW_ID: &str = "tray-show";
pub(crate) const LOCK_ID: &str = "tray-lock";
pub(crate) const CHECK_ID: &str = "tray-check-update";
pub(crate) const UPDATE_ID: &str = "tray-update";
pub(crate) const QUIT_ID: &str = "tray-quit";

/// 托盘菜单里的一项。抽成纯数据，菜单有哪些项不用起窗口就能测（同 0.2.14 的 `TrayEntry`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayEntry {
    Show,
    /// 立即锁定（只在启用了锁屏时才有这一项）。
    Lock,
    /// 检查更新：和「设置 → 关于」里的「检查更新」同一个入口（`MainWindow::check_for_updates`）。
    CheckUpdate,
    /// 后台已经下好了更新：`version` 不带前缀 `v`。
    RestartInstall {
        version: String,
    },
    Separator,
    Quit,
}

impl TrayEntry {
    pub fn label(&self) -> Option<String> {
        use crate::i18n::{Key, t};
        match self {
            Self::Show => Some(format!(
                "{}{}",
                t(Key::TrayShowPrefix),
                crate::product::NAME
            )),
            Self::Lock => Some(t(Key::TrayLock).to_owned()),
            Self::CheckUpdate => Some(t(Key::TrayCheckUpdate).to_owned()),
            Self::RestartInstall { version } => {
                Some(format!("{}{version}", t(Key::TrayRestartInstallPrefix)))
            }
            Self::Separator => None,
            Self::Quit => Some(t(Key::TrayQuit).to_owned()),
        }
    }

    pub(crate) fn id(&self) -> Option<&'static str> {
        match self {
            Self::Show => Some(SHOW_ID),
            Self::Lock => Some(LOCK_ID),
            Self::CheckUpdate => Some(CHECK_ID),
            Self::RestartInstall { .. } => Some(UPDATE_ID),
            Self::Separator => None,
            Self::Quit => Some(QUIT_ID),
        }
    }
}

/// 托盘菜单该有哪些项。「显示」下面是「检查更新」（和关于页同一个入口，打开更新窗口）；
/// 只有"在线更新开着、并且有一个已验签、排队等安装的更新"才再多一项「重启安装 vX」。
///「锁定」跟在「显示」后面，只有启用了锁屏才有。
pub fn menu_entries(
    auto_update: bool,
    ready_version: Option<&str>,
    lock_enabled: bool,
) -> Vec<TrayEntry> {
    let mut entries = vec![TrayEntry::Show];
    if lock_enabled {
        entries.push(TrayEntry::Lock);
    }
    entries.push(TrayEntry::CheckUpdate);
    if let (true, Some(version)) = (auto_update, ready_version) {
        entries.push(TrayEntry::RestartInstall {
            version: version.trim_start_matches('v').to_owned(),
        });
    }
    entries.push(TrayEntry::Separator);
    entries.push(TrayEntry::Quit);
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(entries: &[TrayEntry]) -> Vec<Option<String>> {
        entries.iter().map(TrayEntry::label).collect()
    }

    #[test]
    fn the_official_menu_has_the_restart_item_only_when_an_update_is_queued() {
        let idle = menu_entries(crate::product::AUTO_UPDATE_ENABLED, None, false);
        assert_eq!(
            labels(&idle),
            vec![
                Some("显示 ZeeTerm".to_owned()),
                Some("检查更新".to_owned()),
                None,
                Some("退出".to_owned())
            ]
        );
        let queued = menu_entries(crate::product::AUTO_UPDATE_ENABLED, Some("9.9.9"), false);
        assert_eq!(
            labels(&queued),
            vec![
                Some("显示 ZeeTerm".to_owned()),
                Some("检查更新".to_owned()),
                Some("重启安装 v9.9.9".to_owned()),
                None,
                Some("退出".to_owned())
            ]
        );
    }

    #[test]
    fn restart_install_appears_only_with_auto_update_and_a_ready_package() {
        let ready = menu_entries(true, Some("v1.2.3"), false);
        assert_eq!(
            labels(&ready),
            vec![
                Some("显示 ZeeTerm".to_owned()),
                Some("检查更新".to_owned()),
                Some("重启安装 v1.2.3".to_owned()),
                None,
                Some("退出".to_owned())
            ]
        );
        assert_eq!(menu_entries(true, None, false).len(), 4);
        assert_eq!(menu_entries(false, Some("1.2.3"), false).len(), 4);
    }

    #[test]
    fn the_lock_item_exists_only_when_the_lock_screen_is_enabled_and_sits_under_show() {
        let off = menu_entries(crate::product::AUTO_UPDATE_ENABLED, None, false);
        assert!(
            !off.contains(&TrayEntry::Lock),
            "没启用锁屏时菜单里没有「锁定」"
        );
        let on = menu_entries(crate::product::AUTO_UPDATE_ENABLED, None, true);
        assert_eq!(on[0], TrayEntry::Show);
        assert_eq!(on[1], TrayEntry::Lock, "「锁定」紧跟在「显示」后面");
        assert_eq!(on[2], TrayEntry::CheckUpdate);
        assert_eq!(on.last(), Some(&TrayEntry::Quit));
        assert_eq!(TrayEntry::Lock.label().as_deref(), Some("锁定"));
        // 加上排队的更新：三个都在，次序是 显示、锁定、检查更新、重启安装、分隔线、退出。
        let all = menu_entries(true, Some("2.0.0"), true);
        assert_eq!(
            labels(&all),
            vec![
                Some("显示 ZeeTerm".to_owned()),
                Some("锁定".to_owned()),
                Some("检查更新".to_owned()),
                Some("重启安装 v2.0.0".to_owned()),
                None,
                Some("退出".to_owned())
            ]
        );
    }

    #[test]
    fn check_update_sits_right_under_show_with_or_without_a_ready_package() {
        for ready in [None, Some("1.2.3")] {
            for auto in [false, true] {
                let entries = menu_entries(auto, ready, false);
                assert_eq!(entries[0], TrayEntry::Show);
                assert_eq!(
                    entries[1],
                    TrayEntry::CheckUpdate,
                    "auto={auto} ready={ready:?}"
                );
                assert_eq!(entries.last(), Some(&TrayEntry::Quit));
            }
        }
        // 每个有字的项都有自己的 id，互不相同（点了才分得清）。
        let ids: Vec<_> = menu_entries(true, Some("1.2.3"), false)
            .iter()
            .filter_map(TrayEntry::id)
            .collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(ids.len(), unique.len());
    }
}
