//! 「资源管理 + 应用设置」统一页的分类导航：「资源管理」组 4 个（凭据、快捷命令、代理、端口转发）和
//! 「应用设置」组 10 个（常规 / 外观 / 终端 / 连接与网络 / 文件传输 / AI 助手 / 安全与锁定 /
//! 同步与备份 / 快捷键 / 关于），默认进哪一类、记住上次的分类，以及每个设置分类里有哪些设置项
//! （清单，测试对照 0.2.14 用）和快捷键表。
//!
//! 分类的内部 id 一个都不改（窗口状态文件里记的就是它），V2-SETTINGS-1 只新增 `general`、改显示名和
//! 顺序；「资源管理」组是 V2-U3 把原来散在标题栏 / 主机页 / 侧面板里的管理入口收进来的，只搬现有功能。
//! 这里只放纯数据和规则，不碰界面布局（唯一的界面依赖是快捷键作用文案「按键 → 当前语言的文字」
//! 那个渲染转换，见 [`ShortcutMsg`]），状态测试不用起窗口。

use crate::i18n::{Msg, t};
use crate::icons::Ico;

/// 统一页的分类。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SettingsSection {
    /// 管理：凭据（密码 / 私钥 / 身份）。
    Credentials,
    /// 管理：快捷命令。
    QuickCommands,
    /// 管理：代理。
    Proxies,
    /// 管理：端口转发。
    Forwards,
    /// 常规：语言、启动与退出、检查更新、数据位置（没有上次分类记录时先进这里）。
    #[default]
    General,
    /// 外观：主题与皮肤、终端配色、主机树。
    Appearance,
    Terminal,
    Connection,
    Sftp,
    Ai,
    Sync,
    /// 锁屏（V2-U26）：启用 / 密码 / 自动锁定 / 忘记密码。
    Lock,
    Shortcuts,
    About,
}

impl SettingsSection {
    /// 「管理」组（导航里排在前面）。
    pub const MANAGE: [Self; 4] = [
        Self::Credentials,
        Self::QuickCommands,
        Self::Proxies,
        Self::Forwards,
    ];

    /// 「应用设置」组，导航里从上到下的顺序。
    pub const PREFERENCES: [Self; 10] = [
        Self::General,
        Self::Appearance,
        Self::Terminal,
        Self::Connection,
        Self::Sftp,
        Self::Ai,
        Self::Lock,
        Self::Sync,
        Self::Shortcuts,
        Self::About,
    ];

    /// 导航栏里从上到下的全部分类。
    pub const ALL: [Self; 14] = [
        Self::Credentials,
        Self::QuickCommands,
        Self::Proxies,
        Self::Forwards,
        Self::General,
        Self::Appearance,
        Self::Terminal,
        Self::Connection,
        Self::Sftp,
        Self::Ai,
        Self::Lock,
        Self::Sync,
        Self::Shortcuts,
        Self::About,
    ];

    /// 属于「管理」组。
    pub fn is_manage(self) -> bool {
        Self::MANAGE.contains(&self)
    }

    /// 窗口状态文件里的写法（沿用 0.2.14 `SectionId` 的写法不改；管理 4 类和 `general` 是新增的稳定 id）。
    pub fn id(self) -> &'static str {
        match self {
            Self::Credentials => "credentials",
            Self::QuickCommands => "quick-commands",
            Self::Proxies => "proxies",
            Self::Forwards => "forwards",
            Self::General => "general",
            Self::Appearance => "appearance",
            Self::Terminal => "terminal",
            Self::Connection => "connection",
            Self::Sftp => "sftp",
            Self::Ai => "ai",
            Self::Sync => "sync",
            Self::Lock => "lock",
            Self::Shortcuts => "shortcuts",
            Self::About => "about",
        }
    }

    /// 读回窗口状态；不认识的值（文件被手改或版本不同）按 `None` 处理。
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|section| section.id() == raw)
    }

    /// 导航栏上的名字在文案表里的键。
    fn key(self) -> crate::i18n::Key {
        use crate::i18n::Key;
        match self {
            Self::Credentials => Key::NavCredentials,
            Self::QuickCommands => Key::NavQuickCommands,
            Self::Proxies => Key::NavProxies,
            Self::Forwards => Key::NavForwards,
            Self::General => Key::NavGeneral,
            Self::Appearance => Key::NavAppearance,
            Self::Terminal => Key::NavTerminal,
            Self::Connection => Key::NavConnection,
            Self::Sftp => Key::NavSftp,
            Self::Ai => Key::NavAi,
            Self::Sync => Key::NavSync,
            Self::Lock => Key::NavLock,
            Self::Shortcuts => Key::NavShortcuts,
            Self::About => Key::NavAbout,
        }
    }

    /// 导航栏上的名字（走文案表：切语言即时生效）。
    pub fn label(self) -> &'static str {
        t(self.key())
    }

    /// 导航栏里用的名字：英文的「Connections & Network」在 208px 宽的导航里放不下，导航里写短的
    /// 「Network」（悬停显示完整名字，页面标题也是完整名字）；其余分类和 [`Self::label`] 一样。
    pub fn nav_label(self) -> &'static str {
        match self {
            Self::Connection => t(crate::i18n::Key::NavConnectionShort),
            _ => self.label(),
        }
    }

    /// 导航栏图标：原有的偏好分类沿用 0.2.14 用的 Lucide 图标（MonitorCog / SquareTerminal / Link2 /
    /// FolderOpen / Bot / CloudUpload / Keyboard / Info）；管理 4 类沿用原来入口上的图标
    /// （钥匙、命令、网络、左右箭头）；常规是新增的，用 Settings2。
    pub fn icon(self) -> Ico {
        match self {
            Self::Credentials => Ico::KeyRound,
            Self::QuickCommands => Ico::Command,
            Self::Proxies => Ico::Network,
            Self::Forwards => Ico::ArrowLeftRight,
            Self::General => Ico::Settings2,
            Self::Appearance => Ico::MonitorCog,
            Self::Terminal => Ico::SquareTerminal,
            Self::Connection => Ico::Link2,
            Self::Sftp => Ico::FolderOpen,
            Self::Ai => Ico::Bot,
            Self::Sync => Ico::CloudUpload,
            Self::Lock => Ico::Lock,
            Self::Shortcuts => Ico::Keyboard,
            Self::About => Ico::Info,
        }
    }

    /// 重启后恢复：存的值认得就用它（旧版本记下的分类 id 照常认），没有记录或不认得就进默认的「常规」。
    pub fn restored(saved: Option<&str>) -> Self {
        saved.and_then(Self::parse).unwrap_or_default()
    }

    /// 打开设置页该显示哪一类：从底部同步状态栏点进来直接定位「同步」，
    /// 其余情况回到记住的那一类。
    pub fn on_open(remembered: Self, want_sync: bool) -> Self {
        if want_sync { Self::Sync } else { remembered }
    }

    /// 这个分类里的设置项（稳定 id）。清单是手写的静态表，只给测试用（不是从界面自动抽的，
    /// 界面上实际有什么以窗口级截图为准）。和 0.2.14 对照用：测试里写死了 0.2.14 每一项现在该在哪个分类，
    /// 少一项就失败；ZeeTerm 多出来的项单独列在 [`Self::extra_ids`]。
    #[cfg(test)]
    pub fn setting_ids(self) -> &'static [&'static str] {
        match self {
            // 管理组放的是列表和表单，不是一项项设置：没有设置项清单。
            Self::Credentials | Self::QuickCommands | Self::Proxies | Self::Forwards => &[],
            Self::General => &[
                "language",
                "start-hidden",
                "close-action",
                "file-window-close",
                "general-update",
                "general-data-dir",
            ],
            Self::Appearance => &[
                "theme-mode",
                "skin-light",
                "skin-dark",
                "terminal-theme",
                "host-tree-flat",
                "host-info-panel",
                "host-tree-guides",
            ],
            Self::Terminal => &[
                "font-family",
                "font-size",
                "line-height",
                "scrollback",
                "show-latency",
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
                "x11",
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
            Self::Lock => &[
                "lock-enabled",
                "lock-password",
                "lock-start",
                "lock-idle",
                "lock-on-tray",
                "lock-forgot",
            ],
            Self::Shortcuts => &["shortcuts-list"],
            Self::About => &[
                "about-version",
                "about-update",
                "about-auto-check",
                "about-known-hosts",
                "about-license",
            ],
        }
    }

    /// ZeeTerm 有、0.2.14 里没有的设置项（放在最合适的分类里，报告里列出）。
    #[cfg(test)]
    pub fn extra_ids(self) -> &'static [&'static str] {
        match self {
            // 启动时隐藏到托盘：0.2.14 没有；常规页的检查更新 / 数据位置是 0.2.14「关于」页里
            // 那两行搬过来的（0.2.14 里就有，不算多出来），所以不在这里。
            Self::General => &["language", "start-hidden"],
            // 主机树的属性面板、虚线连接线开关：0.2.14 的树一直画线，没有这两个开关。
            Self::Appearance => &["host-info-panel", "host-tree-guides"],
            // 括号粘贴：0.2.14 没有这一项（xterm 自动跟随远端）；延迟徽标同样是 ZeeTerm 新增。
            Self::Terminal => &["bracketed-paste", "show-latency"],
            // X11 转发：0.2.14 没有。
            Self::Connection => &["x11"],
            // 整个「锁屏」分类都是 ZeeTerm 新增的（0.2.14 没有）。
            Self::Lock => &[
                "lock-enabled",
                "lock-password",
                "lock-start",
                "lock-idle",
                "lock-on-tray",
                "lock-forgot",
            ],
            _ => &[],
        }
    }
}

/// 快捷键表里一行的作用文案（键名固定，切语言即时生效）。
///
/// 只放应用自己的说明；键位组合（`Shortcut::keys`）是固定的按键写法，不走这里。
#[derive(Clone, Copy, Debug)]
pub enum ShortcutMsg {
    NewTab,
    ClosePane,
    SplitRight,
    SplitDown,
    SwitchTabs,
    OpenSettings,
    ReconnectPane,
    ToggleBroadcast,
    ToggleAiPanel,
    ToggleFilePanel,
    SendBreak,
    FontZoom,
    CopyPaste,
    ScrollPages,
    CloseSettings,
    LockApp,
}

impl Msg for ShortcutMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::NewTab => "新标签",
            Self::ClosePane => "关闭当前窗格（只剩一个时关掉整个标签）",
            Self::SplitRight => "左右分屏（复用连接）",
            Self::SplitDown => "上下分屏",
            Self::SwitchTabs => "切换标签",
            Self::OpenSettings => "打开 / 关闭设置与管理页",
            Self::ReconnectPane => "重连断线的窗格",
            Self::ToggleBroadcast => "开关广播输入",
            Self::ToggleAiPanel => "开关 AI 面板",
            Self::ToggleFilePanel => "开关文件（SFTP）面板（仅 SSH 主机）",
            Self::SendBreak => "发送 Break（串口标签；右键标签也有）",
            Self::FontZoom => "终端字号放大 / 缩小 / 恢复默认",
            Self::CopyPaste if cfg!(target_os = "macos") => "有选区时复制；粘贴（Shift+Insert 同）",
            Self::CopyPaste => "有选区时复制；粘贴（Ctrl+Shift+V、Shift+Insert 同）",
            Self::ScrollPages => "终端翻页（Shift+Home / End 到顶 / 到底）",
            Self::CloseSettings => "关闭设置与管理页，回到原来的页面",
            Self::LockApp => "锁定 ZeeTerm（要先在 设置 → 安全与锁定 里启用并设密码）",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::NewTab => "New tab",
            Self::ClosePane => {
                "Close the current pane (closes the whole tab when it is the last one)"
            }
            Self::SplitRight => "Split left/right (reuses the connection)",
            Self::SplitDown => "Split top/bottom",
            Self::SwitchTabs => "Switch tabs",
            Self::OpenSettings => "Open / close Settings & Manage",
            Self::ReconnectPane => "Reconnect a disconnected pane",
            Self::ToggleBroadcast => "Toggle broadcast input",
            Self::ToggleAiPanel => "Toggle the AI panel",
            Self::ToggleFilePanel => "Toggle the file (SFTP) panel (SSH hosts only)",
            Self::SendBreak => "Send Break (serial tabs; also on the tab's right-click menu)",
            Self::FontZoom => "Enlarge / shrink / reset the terminal font size",
            Self::CopyPaste if cfg!(target_os = "macos") => {
                "Copy when there is a selection; paste (also Shift+Insert)"
            }
            Self::CopyPaste => {
                "Copy when there is a selection; paste (also Ctrl+Shift+V, Shift+Insert)"
            }
            Self::ScrollPages => {
                "Page through the terminal (Shift+Home / End to jump to the top / bottom)"
            }
            Self::CloseSettings => "Close Settings & Manage and return to the previous page",
            Self::LockApp => {
                "Lock ZeeTerm (enable it and set a password under Settings → Security & Lock first)"
            }
        }
    }
}

/// 快捷键行在界面上直接当文字用（`main_window::settings_page` 里 `.child(shortcut.what)`）：
/// 取词发生在绘制时，切语言后下一帧就是新文案。
impl ::gpui::IntoElement for ShortcutMsg {
    type Element = ::gpui::SharedString;

    fn into_element(self) -> Self::Element {
        ::gpui::SharedString::new_static(t(self))
    }
}

/// 同一个键也能当 `&str` 读（`shortcut.what.contains(..)` 这类只读判断），同样是当前语言的文案。
impl std::ops::Deref for ShortcutMsg {
    type Target = str;

    fn deref(&self) -> &str {
        t(*self)
    }
}

/// macOS 上主窗口认的是 Cmd 组合（见 `main_window::on_key_down` 的 macOS 分支）：表里写那一份；
/// 原来的 Ctrl+Shift 组合在 macOS 上也照样生效，只是不再写进表里。
const fn mac_or(mac: &'static str, other: &'static str) -> &'static str {
    if cfg!(target_os = "macos") {
        mac
    } else {
        other
    }
}

/// 快捷键表里的一行：按键、作用。
pub struct Shortcut {
    pub keys: &'static str,
    pub what: ShortcutMsg,
}

/// 快捷键分类的内容：前七行是 0.2.14 的清单（按键以 ZeeTerm 代码里真正生效的为准，
/// 0.2.14 写的「断线窗格上按 R 重连」在 ZeeTerm 里是 Ctrl+Shift+R），后面是 ZeeTerm 里也生效的其他键。
/// 只读说明，不能改键（0.2.14 也是「不支持自定义」）。
pub const SHORTCUTS: &[Shortcut] = &[
    Shortcut {
        keys: mac_or("Cmd+T", "Ctrl+Shift+T"),
        what: ShortcutMsg::NewTab,
    },
    Shortcut {
        keys: mac_or("Cmd+W", "Ctrl+Shift+W"),
        what: ShortcutMsg::ClosePane,
    },
    Shortcut {
        keys: mac_or("Cmd+D", "Ctrl+Shift+D"),
        what: ShortcutMsg::SplitRight,
    },
    Shortcut {
        keys: mac_or("Cmd+E", "Ctrl+Shift+E"),
        what: ShortcutMsg::SplitDown,
    },
    Shortcut {
        keys: "Ctrl+Tab / Ctrl+Shift+Tab",
        what: ShortcutMsg::SwitchTabs,
    },
    Shortcut {
        keys: mac_or("Cmd+,", "Ctrl+,"),
        what: ShortcutMsg::OpenSettings,
    },
    Shortcut {
        keys: "Ctrl+Shift+R",
        what: ShortcutMsg::ReconnectPane,
    },
    Shortcut {
        keys: mac_or("Cmd+B", "Ctrl+Shift+B"),
        what: ShortcutMsg::ToggleBroadcast,
    },
    Shortcut {
        keys: "Ctrl+Shift+A",
        what: ShortcutMsg::ToggleAiPanel,
    },
    Shortcut {
        keys: mac_or("Cmd+Shift+F", "Ctrl+Shift+F"),
        what: ShortcutMsg::ToggleFilePanel,
    },
    Shortcut {
        keys: "Ctrl+Shift+K",
        what: ShortcutMsg::SendBreak,
    },
    Shortcut {
        keys: "Ctrl+= / Ctrl+- / Ctrl+0",
        what: ShortcutMsg::FontZoom,
    },
    Shortcut {
        keys: mac_or("Cmd+C / Cmd+V", "Ctrl+C / Ctrl+V"),
        what: ShortcutMsg::CopyPaste,
    },
    Shortcut {
        keys: "Shift+PageUp / PageDown",
        what: ShortcutMsg::ScrollPages,
    },
    Shortcut {
        keys: "Esc",
        what: ShortcutMsg::CloseSettings,
    },
    // 放在表尾：快捷键分组（`settings_page::SHORTCUT_GROUPS`）按下标引用这张表。
    Shortcut {
        keys: "Ctrl+Alt+L",
        what: ShortcutMsg::LockApp,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_settings_group_has_ten_entries_in_the_planned_order() {
        let names: Vec<_> = SettingsSection::PREFERENCES
            .iter()
            .map(|s| s.label())
            .collect();
        assert_eq!(
            names,
            [
                "常规",
                "外观",
                "终端",
                "连接与网络",
                "文件传输",
                "AI 助手",
                "安全与锁定",
                "同步与备份",
                "快捷键",
                "关于"
            ]
        );
        // 内部 id 只新增了 general，旧的一个都没改（窗口状态文件里记的就是它们）。
        let ids: Vec<_> = SettingsSection::PREFERENCES
            .iter()
            .map(|s| s.id())
            .collect();
        assert_eq!(
            ids,
            [
                "general",
                "appearance",
                "terminal",
                "connection",
                "sftp",
                "ai",
                "lock",
                "sync",
                "shortcuts",
                "about"
            ]
        );
    }

    #[test]
    fn the_english_names_follow_the_plan() {
        // 直接读英文表，不去切全局语言（别的测试默认中文）。
        let names: Vec<_> = SettingsSection::ALL.iter().map(|s| s.key().en()).collect();
        assert_eq!(
            names,
            [
                "Credentials",
                "Snippets",
                "Proxies",
                "Port Forwarding",
                "General",
                "Appearance",
                "Terminal",
                "Connections & Network",
                "File Transfer",
                "AI Assistant",
                "Security & Lock",
                "Sync & Backup",
                "Keyboard Shortcuts",
                "About"
            ]
        );
    }

    #[test]
    fn only_the_connection_section_has_a_shorter_english_nav_name() {
        for section in SettingsSection::ALL {
            if section != SettingsSection::Connection {
                assert_eq!(section.nav_label(), section.label());
            }
        }
        // 中文名不长，导航里和完整名字一样；英文导航里用短名字，完整名字在悬停提示和页面标题里。
        assert_eq!(
            SettingsSection::Connection.nav_label(),
            SettingsSection::Connection.label()
        );
        assert_eq!(crate::i18n::Key::NavConnectionShort.en(), "Network");
    }

    #[test]
    fn the_manage_group_has_four_entries_in_front_of_the_ten_settings() {
        let manage: Vec<_> = SettingsSection::MANAGE.iter().map(|s| s.label()).collect();
        assert_eq!(manage, ["凭据", "快捷命令", "代理", "端口转发"]);
        let ids: Vec<_> = SettingsSection::MANAGE.iter().map(|s| s.id()).collect();
        assert_eq!(
            ids,
            ["credentials", "quick-commands", "proxies", "forwards"]
        );
        assert_eq!(SettingsSection::ALL.len(), 14);
        assert_eq!(
            SettingsSection::ALL[..4],
            SettingsSection::MANAGE[..],
            "管理组在前"
        );
        assert_eq!(
            SettingsSection::ALL[4..],
            SettingsSection::PREFERENCES[..],
            "设置组在后"
        );
        for section in SettingsSection::MANAGE {
            assert!(section.is_manage());
        }
        for section in SettingsSection::PREFERENCES {
            assert!(!section.is_manage());
        }
    }

    #[test]
    fn every_section_id_is_unique_and_the_old_ones_are_unchanged() {
        // 旧版本（V2-SETTINGS-1 之前）窗口状态里可能记下的全部分类 id：一个都不能变。
        for old in [
            "credentials",
            "quick-commands",
            "proxies",
            "forwards",
            "appearance",
            "terminal",
            "connection",
            "sftp",
            "ai",
            "sync",
            "lock",
            "shortcuts",
            "about",
        ] {
            let section = SettingsSection::parse(old).expect("旧分类 id 要认得");
            assert_eq!(section.id(), old);
        }
        let ids: std::collections::HashSet<_> =
            SettingsSection::ALL.iter().map(|s| s.id()).collect();
        assert_eq!(ids.len(), SettingsSection::ALL.len());
    }

    #[test]
    fn ids_round_trip_and_garbage_is_ignored() {
        for section in SettingsSection::ALL {
            assert_eq!(SettingsSection::parse(section.id()), Some(section));
        }
        assert_eq!(SettingsSection::parse(""), None);
        assert_eq!(SettingsSection::parse("Appearance"), None);
        assert_eq!(SettingsSection::parse("nope"), None);
        // 管理组的 id 也认得，大小写 / 旧写法不认。
        assert_eq!(
            SettingsSection::parse("quick-commands"),
            Some(SettingsSection::QuickCommands)
        );
        assert_eq!(SettingsSection::parse("quick_commands"), None);
    }

    #[test]
    fn opens_on_general_by_default_and_restores_every_saved_section() {
        assert_eq!(SettingsSection::default(), SettingsSection::General);
        assert_eq!(SettingsSection::restored(None), SettingsSection::General);
        assert_eq!(
            SettingsSection::restored(Some("garbage")),
            SettingsSection::General
        );
        // 旧版本记下的「外观」这类分类照常恢复，不会被默认值盖掉。
        assert_eq!(
            SettingsSection::restored(Some("appearance")),
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

    /// 0.2.14 里的每个设置项（来自 `SettingsPage.tsx`、`HighlightSection.tsx`、`SessionLogSection.tsx`、
    /// `sync/` 和 `AboutSection`）和它**现在**该在的分类。ZeeTerm 的分类清单必须全包含。
    /// 搬过家的：关闭行为（外观 → 常规）、数据目录（关于 → 常规）；检查更新入口常规和关于两处都有（同一份实现）。
    fn expected_0_2_14() -> Vec<(SettingsSection, &'static str)> {
        use SettingsSection::*;
        let mut all = vec![];
        for (section, ids) in [
            (
                Appearance,
                &[
                    "theme-mode",
                    "skin-light",
                    "skin-dark",
                    "terminal-theme",
                    "host-tree-flat",
                ][..],
            ),
            (
                General,
                &[
                    "close-action",
                    "file-window-close",
                    "general-update",
                    "general-data-dir",
                ][..],
            ),
            (
                Terminal,
                &[
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
                ][..],
            ),
            (
                Connection,
                &[
                    "ssh-default-proxy",
                    "app-network-proxy",
                    "pool-idle-timeout",
                    "keepalive-interval",
                    "stats-show",
                    "stats-refresh",
                ][..],
            ),
            (
                Sftp,
                &[
                    "sftp-concurrency",
                    "sftp-preserve-mtime",
                    "sftp-preserve-permissions",
                    "sftp-show-hidden",
                ][..],
            ),
            (
                Ai,
                &[
                    "ai-default-agent",
                    "ai-exec-mode",
                    "ai-write-backup-keep",
                    "ai-clear-history",
                    "ai-agent-opencode",
                    "ai-agent-codex",
                    "ai-agent-claude",
                ][..],
            ),
            (
                Sync,
                &["sync-connection", "sync-backups", "sync-offline-backup"][..],
            ),
            (Shortcuts, &["shortcuts-list"][..]),
            (
                About,
                &[
                    "about-version",
                    "about-update",
                    "about-auto-check",
                    "about-known-hosts",
                    "about-license",
                ][..],
            ),
        ] {
            for id in ids {
                all.push((section, *id));
            }
        }
        all
    }

    #[test]
    fn every_setting_the_0_2_14_one_has_still_has_an_entry_in_its_new_section() {
        for (section, id) in expected_0_2_14() {
            assert!(
                section.setting_ids().contains(&id),
                "{} 缺少原有的设置项 {id}",
                section.label()
            );
        }
    }

    #[test]
    fn every_setting_has_exactly_one_home_except_the_shared_update_entry() {
        let mut seen = std::collections::HashMap::new();
        for section in SettingsSection::PREFERENCES {
            for id in section.setting_ids() {
                if let Some(other) = seen.insert(*id, section) {
                    panic!(
                        "设置项 {id} 同时出现在 {} 和 {}",
                        other.label(),
                        section.label()
                    );
                }
            }
        }
        // 外观页不再带语言和窗口行为，它们在常规页。
        for id in [
            "language",
            "close-action",
            "file-window-close",
            "start-hidden",
        ] {
            assert_eq!(seen[id], SettingsSection::General, "{id}");
        }
        // 数据目录搬到常规页，关于页不再重复显示。
        assert!(
            !SettingsSection::About
                .setting_ids()
                .contains(&"general-data-dir")
        );
        // 检查更新：常规一个入口、关于一个入口，id 不同但走同一份实现（见 about_view::check_update_now）。
        assert!(
            SettingsSection::General
                .setting_ids()
                .contains(&"general-update")
        );
        assert!(
            SettingsSection::About
                .setting_ids()
                .contains(&"about-update")
        );
    }

    #[test]
    fn extras_are_listed_and_not_mistaken_for_0_2_14_ones() {
        let original = expected_0_2_14();
        for section in SettingsSection::PREFERENCES {
            for id in section.extra_ids() {
                assert!(
                    section.setting_ids().contains(id),
                    "多出来的项 {id} 要在本分类的清单里"
                );
                assert!(
                    !original.contains(&(section, *id)),
                    "{id} 0.2.14 里就有，不算多出来"
                );
            }
        }
    }

    #[test]
    fn the_shortcut_table_has_the_0_2_14_rows_and_no_duplicate_keys() {
        let keys: Vec<_> = SHORTCUTS.iter().map(|s| s.keys).collect();
        for needed in [
            mac_or("Cmd+T", "Ctrl+Shift+T"),
            mac_or("Cmd+W", "Ctrl+Shift+W"),
            mac_or("Cmd+D", "Ctrl+Shift+D"),
            mac_or("Cmd+E", "Ctrl+Shift+E"),
            "Ctrl+Tab / Ctrl+Shift+Tab",
            mac_or("Cmd+,", "Ctrl+,"),
            "Ctrl+Shift+K",
        ] {
            assert!(keys.contains(&needed), "缺 {needed}");
        }
        let unique: std::collections::HashSet<_> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len());
    }
}
