//! 设置页的分类导航：8 个分类的顺序、名称、图标，默认进哪一类、记住上次的分类，
//! 以及每个分类里有哪些设置项（清单，测试对照 0.2.14 用）和快捷键表。
//!
//! 分类和顺序取自 0.2.14 的 `SettingsPage.tsx`（外观 / 终端 / 连接 / SFTP / AI / 同步 / 快捷键 / 关于）。
//! 这里只放纯数据和规则，不碰界面框架，状态测试不用起窗口。

use crate::icons::Ico;

/// 设置页的分类。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SettingsSection {
    /// 外观：主题、皮肤、终端配色、主机树、窗口。
    #[default]
    Appearance,
    Terminal,
    Connection,
    Sftp,
    Ai,
    Sync,
    Shortcuts,
    About,
}

impl SettingsSection {
    /// 导航栏里的顺序（同 0.2.14 的 `SECTIONS`）。
    pub const ALL: [Self; 8] = [
        Self::Appearance,
        Self::Terminal,
        Self::Connection,
        Self::Sftp,
        Self::Ai,
        Self::Sync,
        Self::Shortcuts,
        Self::About,
    ];

    /// 窗口状态文件里的写法（和 0.2.14 的 `SectionId` 一致）。
    pub fn id(self) -> &'static str {
        match self {
            Self::Appearance => "appearance",
            Self::Terminal => "terminal",
            Self::Connection => "connection",
            Self::Sftp => "sftp",
            Self::Ai => "ai",
            Self::Sync => "sync",
            Self::Shortcuts => "shortcuts",
            Self::About => "about",
        }
    }

    /// 读回窗口状态；不认识的值（文件被手改或版本不同）按 `None` 处理。
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|section| section.id() == raw)
    }

    /// 导航栏上的名字。
    pub fn label(self) -> &'static str {
        match self {
            Self::Appearance => "外观",
            Self::Terminal => "终端",
            Self::Connection => "连接",
            Self::Sftp => "SFTP",
            Self::Ai => "AI",
            Self::Sync => "同步",
            Self::Shortcuts => "快捷键",
            Self::About => "关于",
        }
    }

    /// 导航栏图标：0.2.14 用的 Lucide 图标（MonitorCog / SquareTerminal / Link2 / FolderOpen /
    /// Bot / CloudUpload / Keyboard / Info）。
    pub fn icon(self) -> Ico {
        match self {
            Self::Appearance => Ico::MonitorCog,
            Self::Terminal => Ico::SquareTerminal,
            Self::Connection => Ico::Link2,
            Self::Sftp => Ico::FolderOpen,
            Self::Ai => Ico::Bot,
            Self::Sync => Ico::CloudUpload,
            Self::Shortcuts => Ico::Keyboard,
            Self::About => Ico::Info,
        }
    }

    /// 重启后恢复：存的值认得就用它，否则回到默认的「外观」。
    pub fn restored(saved: Option<&str>) -> Self {
        saved.and_then(Self::parse).unwrap_or_default()
    }

    /// 打开设置页该显示哪一类：从底部同步状态栏点进来直接定位「同步」，
    /// 其余情况回到记住的那一类。
    pub fn on_open(remembered: Self, want_sync: bool) -> Self {
        if want_sync { Self::Sync } else { remembered }
    }

    /// 这个分类里的设置项（稳定 id）。清单是手写的静态表，只给测试用（不是从界面自动抽的，
    /// 界面上实际有什么以窗口级截图为准）。和 0.2.14 对照用：测试里写死了 0.2.14 每个分类应有的项，
    /// 少一项就失败；ZeeTerm 多出来的项单独列在 [`Self::extra_ids`]。
    #[cfg(test)]
    pub fn setting_ids(self) -> &'static [&'static str] {
        match self {
            Self::Appearance => &[
                "theme-mode",
                "skin-light",
                "skin-dark",
                "terminal-theme",
                "host-tree-flat",
                "close-action",
                "file-window-close",
                "start-hidden",
            ],
            Self::Terminal => &[
                "font-family",
                "font-size",
                "line-height",
                "scrollback",
                "cursor-style",
                "cursor-blink",
                "ime-english",
                "copy-on-select",
                "right-click-paste",
                "confirm-multiline",
                "bracketed-paste",
                "highlight-enabled",
                "highlight-rules",
                "session-log-enabled",
                "session-log-dir",
            ],
            Self::Connection => &[
                "ssh-default-proxy",
                "app-network-proxy",
                "pool-idle-timeout",
                "keepalive-interval",
                "stats-show",
                "stats-refresh",
            ],
            Self::Sftp => &[
                "sftp-concurrency",
                "sftp-preserve-mtime",
                "sftp-preserve-permissions",
                "sftp-show-hidden",
            ],
            Self::Ai => &[
                "ai-default-agent",
                "ai-exec-mode",
                "ai-write-backup-keep",
                "ai-clear-history",
                "ai-agent-opencode",
                "ai-agent-codex",
                "ai-agent-claude",
            ],
            Self::Sync => &["sync-connection", "sync-backups", "sync-offline-backup"],
            Self::Shortcuts => &["shortcuts-list"],
            Self::About => &[
                "about-version",
                "about-update",
                "about-auto-check",
                "about-data-dir",
                "about-known-hosts",
                "about-license",
            ],
        }
    }

    /// ZeeTerm 有、0.2.14 这个分类里没有的设置项（放在最合适的分类里，报告里列出）。
    #[cfg(test)]
    pub fn extra_ids(self) -> &'static [&'static str] {
        match self {
            // 括号粘贴：0.2.14 没有这一项（xterm 自动跟随远端）；启动时隐藏到托盘：0.2.14 没有。
            Self::Terminal => &["bracketed-paste"],
            Self::Appearance => &["start-hidden"],
            _ => &[],
        }
    }
}

/// 快捷键表里的一行：按键、作用。
pub struct Shortcut {
    pub keys: &'static str,
    pub what: &'static str,
}

/// 快捷键分类的内容：前七行是 0.2.14 的清单（按键以 ZeeTerm 代码里真正生效的为准，
/// 0.2.14 写的「断线窗格上按 R 重连」在 ZeeTerm 里是 Ctrl+Shift+R），后面是 ZeeTerm 里也生效的其他键。
/// 只读说明，不能改键（0.2.14 也是「不支持自定义」）。
pub const SHORTCUTS: &[Shortcut] = &[
    Shortcut {
        keys: "Ctrl+Shift+T",
        what: "新标签",
    },
    Shortcut {
        keys: "Ctrl+Shift+W",
        what: "关闭当前窗格（只剩一个时关掉整个标签）",
    },
    Shortcut {
        keys: "Ctrl+Shift+D",
        what: "左右分屏（复用连接）",
    },
    Shortcut {
        keys: "Ctrl+Shift+E",
        what: "上下分屏",
    },
    Shortcut {
        keys: "Ctrl+Tab / Ctrl+Shift+Tab",
        what: "切换标签",
    },
    Shortcut {
        keys: "Ctrl+,",
        what: "打开 / 关闭设置",
    },
    Shortcut {
        keys: "Ctrl+Shift+R",
        what: "重连断线的窗格",
    },
    Shortcut {
        keys: "Ctrl+Shift+B",
        what: "开关广播输入",
    },
    Shortcut {
        keys: "Ctrl+Shift+A",
        what: "开关 AI 面板",
    },
    Shortcut {
        keys: "Ctrl+Shift+F",
        what: "开关文件（SFTP）面板",
    },
    Shortcut {
        keys: "Ctrl+= / Ctrl+- / Ctrl+0",
        what: "终端字号放大 / 缩小 / 恢复默认",
    },
    Shortcut {
        keys: "Ctrl+C / Ctrl+V",
        what: "有选区时复制；粘贴（Ctrl+Shift+V、Shift+Insert 同）",
    },
    Shortcut {
        keys: "Shift+PageUp / PageDown",
        what: "终端翻页（Shift+Home / End 到顶 / 到底）",
    },
    Shortcut {
        keys: "Esc",
        what: "关闭设置页",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eight_sections_in_the_0_2_14_order_with_the_same_names() {
        let names: Vec<_> = SettingsSection::ALL.iter().map(|s| s.label()).collect();
        assert_eq!(
            names,
            [
                "外观",
                "终端",
                "连接",
                "SFTP",
                "AI",
                "同步",
                "快捷键",
                "关于"
            ]
        );
        let ids: Vec<_> = SettingsSection::ALL.iter().map(|s| s.id()).collect();
        assert_eq!(
            ids,
            [
                "appearance",
                "terminal",
                "connection",
                "sftp",
                "ai",
                "sync",
                "shortcuts",
                "about"
            ]
        );
    }

    #[test]
    fn ids_round_trip_and_garbage_is_ignored() {
        for section in SettingsSection::ALL {
            assert_eq!(SettingsSection::parse(section.id()), Some(section));
        }
        assert_eq!(SettingsSection::parse(""), None);
        assert_eq!(SettingsSection::parse("Appearance"), None);
        assert_eq!(SettingsSection::parse("nope"), None);
    }

    #[test]
    fn opens_on_appearance_by_default_and_remembers_the_last_section() {
        assert_eq!(SettingsSection::default(), SettingsSection::Appearance);
        assert_eq!(SettingsSection::restored(None), SettingsSection::Appearance);
        assert_eq!(
            SettingsSection::restored(Some("garbage")),
            SettingsSection::Appearance
        );
        for section in SettingsSection::ALL {
            assert_eq!(SettingsSection::restored(Some(section.id())), section);
        }
    }

    #[test]
    fn the_sync_status_bar_jumps_straight_to_sync() {
        assert_eq!(
            SettingsSection::on_open(SettingsSection::Terminal, true),
            SettingsSection::Sync
        );
        assert_eq!(
            SettingsSection::on_open(SettingsSection::Terminal, false),
            SettingsSection::Terminal
        );
    }

    /// 0.2.14 每个分类里的设置项（来自 `SettingsPage.tsx`、`HighlightSection.tsx`、
    /// `SessionLogSection.tsx`、`sync/` 和 `AboutSection`）。ZeeTerm 的分类清单必须全包含。
    fn expected_0_2_14(section: SettingsSection) -> &'static [&'static str] {
        match section {
            SettingsSection::Appearance => &[
                "theme-mode",
                "skin-light",
                "skin-dark",
                "terminal-theme",
                "host-tree-flat",
                "close-action",
                "file-window-close",
            ],
            SettingsSection::Terminal => &[
                "font-family",
                "font-size",
                "line-height",
                "scrollback",
                "cursor-style",
                "cursor-blink",
                "ime-english",
                "copy-on-select",
                "right-click-paste",
                "confirm-multiline",
                "highlight-enabled",
                "highlight-rules",
                "session-log-enabled",
                "session-log-dir",
            ],
            SettingsSection::Connection => &[
                "ssh-default-proxy",
                "app-network-proxy",
                "pool-idle-timeout",
                "keepalive-interval",
                "stats-show",
                "stats-refresh",
            ],
            SettingsSection::Sftp => &[
                "sftp-concurrency",
                "sftp-preserve-mtime",
                "sftp-preserve-permissions",
                "sftp-show-hidden",
            ],
            SettingsSection::Ai => &[
                "ai-default-agent",
                "ai-exec-mode",
                "ai-write-backup-keep",
                "ai-clear-history",
                "ai-agent-opencode",
                "ai-agent-codex",
                "ai-agent-claude",
            ],
            SettingsSection::Sync => &["sync-connection", "sync-backups", "sync-offline-backup"],
            SettingsSection::Shortcuts => &["shortcuts-list"],
            SettingsSection::About => &[
                "about-version",
                "about-update",
                "about-auto-check",
                "about-data-dir",
                "about-known-hosts",
                "about-license",
            ],
        }
    }

    #[test]
    fn every_section_has_all_the_settings_the_0_2_14_one_has() {
        for section in SettingsSection::ALL {
            let have = section.setting_ids();
            for id in expected_0_2_14(section) {
                assert!(
                    have.contains(id),
                    "{} 缺少 0.2.14 里有的设置项 {id}",
                    section.label()
                );
            }
        }
    }

    #[test]
    fn extras_are_listed_and_ids_are_not_duplicated_across_sections() {
        let mut seen = std::collections::HashSet::new();
        for section in SettingsSection::ALL {
            for id in section.setting_ids() {
                assert!(seen.insert(*id), "设置项 {id} 出现在两个分类里");
            }
            for id in section.extra_ids() {
                assert!(
                    section.setting_ids().contains(id),
                    "多出来的项 {id} 要在本分类的清单里"
                );
                assert!(
                    !expected_0_2_14(section).contains(id),
                    "{id} 0.2.14 里就有，不算多出来"
                );
            }
        }
    }

    #[test]
    fn the_shortcut_table_has_the_0_2_14_rows_and_no_duplicate_keys() {
        let keys: Vec<_> = SHORTCUTS.iter().map(|s| s.keys).collect();
        for needed in [
            "Ctrl+Shift+T",
            "Ctrl+Shift+W",
            "Ctrl+Shift+D",
            "Ctrl+Shift+E",
            "Ctrl+Tab / Ctrl+Shift+Tab",
            "Ctrl+,",
        ] {
            assert!(keys.contains(&needed), "缺 {needed}");
        }
        let unique: std::collections::HashSet<_> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len());
    }
}
