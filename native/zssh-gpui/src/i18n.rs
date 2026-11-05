//! 界面语言：只有简体中文和英文两种。
//!
//! 规则（V2-I18N）：
//! - 应用自己的文案才翻译；远程服务器的输出、用户名、路径、命令、用户自定义名称（主机名、
//!   分组名、标签名……）一律原样显示，不走这里；
//! - 底层错误尽量带着类型和参数上到界面，在这里按语言生成提示，不靠替换中文字符串；
//! - 现有用户的设置文件里没有语言字段 → 按中文处理（保留他们原来的习惯）；全新安装 →
//!   跟随系统。

use std::sync::atomic::{AtomicU8, Ordering::Relaxed};

use ::gpui::{App, Global};

/// 当前语言（进程级）：托盘这种拿不到 `App` 的地方也读得到。
static CURRENT: AtomicU8 = AtomicU8::new(0);

/// 实际生效的语言。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Lang {
    #[default]
    Zh,
    En,
}

impl Lang {
    fn as_u8(self) -> u8 {
        match self {
            Self::Zh => 0,
            Self::En => 1,
        }
    }

    fn from_u8(raw: u8) -> Self {
        if raw == 1 { Self::En } else { Self::Zh }
    }
}

/// 设置里选的偏好：跟随系统 / 强制中文 / 强制英文。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum LangPref {
    /// 跟随系统（全新安装的默认值）。
    #[default]
    System,
    Zh,
    En,
}

impl LangPref {
    /// 存进 `ui-settings.json` 的值。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Zh => "zh",
            Self::En => "en",
        }
    }

    /// 从设置里读回来的值；认不得的一律当成「跟随系统」。
    pub(crate) fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "zh" => Self::Zh,
            "en" => Self::En,
            _ => Self::System,
        }
    }

    /// 按偏好和系统语言算出实际语言：只有中文和英文两种，非中文一律英文。
    pub(crate) fn resolve(self, system_is_chinese: bool) -> Lang {
        match self {
            Self::Zh => Lang::Zh,
            Self::En => Lang::En,
            Self::System => {
                if system_is_chinese {
                    Lang::Zh
                } else {
                    Lang::En
                }
            }
        }
    }
}

/// 全局语言；还没设过（测试、第一个窗口早于设置加载）按中文。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct I18n;

impl Global for I18n {}

/// 现在生效的语言。
pub(crate) fn current() -> Lang {
    Lang::from_u8(CURRENT.load(Relaxed))
}

/// 切换语言：写进程级当前值 + 全局后让所有窗口重绘。
/// 已开着的窗口下一帧就是新文案；托盘菜单在下次构建时用新文案。
pub(crate) fn set(lang: Lang, cx: &mut App) {
    CURRENT.store(lang.as_u8(), Relaxed);
    cx.set_global(I18n);
    // 借用主题切换那条「所有窗口重绘」的路径，明暗保持当前值不变。
    crate::app_theme::set_mode(crate::app_theme::is_dark(cx), cx);
}

/// 取一句界面文案。键是稳定标识，不是中文原文——以后加语言只加表，不改调用点。
///
/// 只放**应用自己的**文案：远端输出、用户名、路径、命令、用户自定义名称都不经这里。
pub(crate) fn t(key: Key) -> &'static str {
    match current() {
        Lang::Zh => key.zh(),
        Lang::En => key.en(),
    }
}

/// 界面文案的键。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Key {
    // ---- 托盘 ----
    TrayShowPrefix,
    TrayLock,
    TrayCheckUpdate,
    TrayRestartInstallPrefix,
    TrayQuit,
    // ---- 主机右键菜单 ----
    MenuConnect,
    MenuConnectNewTab,
    MenuOpenSftp,
    MenuCopyName,
    MenuCopyAddress,
    MenuCopyPassword,
    MenuCopyInfo,
    MenuInstallPublicKey,
    MenuDelete,
    MenuForwards,
    MenuMoveToGroup,
    MenuMoveUp,
    MenuMoveDown,
    MenuProperties,
    MenuSortByName,
    // ---- 设置页 · 外观 ----
    LanguageCardTitle,
    LanguageFollowSystem,
    LanguageZh,
    LanguageEn,
    LanguageRestartHint,
}

impl Key {
    fn zh(self) -> &'static str {
        match self {
            Self::TrayShowPrefix => "显示 ",
            Self::TrayLock => "锁定",
            Self::TrayCheckUpdate => "检查更新",
            Self::TrayRestartInstallPrefix => "重启安装 v",
            Self::TrayQuit => "退出",
            Self::MenuConnect => "连接",
            Self::MenuConnectNewTab => "在新标签连接",
            Self::MenuOpenSftp => "打开 SFTP",
            Self::MenuCopyName => "复制主机名",
            Self::MenuCopyAddress => "复制地址",
            Self::MenuCopyPassword => "复制密码",
            Self::MenuCopyInfo => "复制连接信息",
            Self::MenuInstallPublicKey => "安装公钥…",
            Self::MenuDelete => "删除",
            Self::MenuForwards => "端口转发…",
            Self::MenuMoveToGroup => "移动到分组",
            Self::MenuMoveUp => "上移",
            Self::MenuMoveDown => "下移",
            Self::MenuProperties => "属性…",
            Self::MenuSortByName => "按名字重新排序（顶层主机）",
            Self::LanguageCardTitle => "界面语言",
            Self::LanguageFollowSystem => "跟随系统",
            Self::LanguageZh => "简体中文",
            Self::LanguageEn => "English",
            Self::LanguageRestartHint => {
                "部分已打开的内容（例如系统托盘菜单）要重启程序后才会换语言。"
            }
        }
    }

    fn en(self) -> &'static str {
        match self {
            Self::TrayShowPrefix => "Show ",
            Self::TrayLock => "Lock",
            Self::TrayCheckUpdate => "Check for Updates",
            Self::TrayRestartInstallPrefix => "Restart to install v",
            Self::TrayQuit => "Quit",
            Self::MenuConnect => "Connect",
            Self::MenuConnectNewTab => "Connect in New Tab",
            Self::MenuOpenSftp => "Open SFTP",
            Self::MenuCopyName => "Copy Host Name",
            Self::MenuCopyAddress => "Copy Address",
            Self::MenuCopyPassword => "Copy Password",
            Self::MenuCopyInfo => "Copy Connection Info",
            Self::MenuInstallPublicKey => "Install Public Key…",
            Self::MenuDelete => "Delete",
            Self::MenuForwards => "Port Forwarding…",
            Self::MenuMoveToGroup => "Move to Group",
            Self::MenuMoveUp => "Move Up",
            Self::MenuMoveDown => "Move Down",
            Self::MenuProperties => "Properties…",
            Self::MenuSortByName => "Sort Top-Level Hosts by Name",
            Self::LanguageCardTitle => "Interface Language",
            Self::LanguageFollowSystem => "Follow System",
            Self::LanguageZh => "简体中文",
            Self::LanguageEn => "English",
            Self::LanguageRestartHint => {
                "Some already-open surfaces (such as the system tray menu) switch after you restart the app."
            }
        }
    }
}

/// 三个语言偏好在设置页上显示的字（两个语言下都显示各自的名字）。
pub(crate) fn pref_label(pref: LangPref) -> &'static str {
    match pref {
        LangPref::System => t(Key::LanguageFollowSystem),
        LangPref::Zh => t(Key::LanguageZh),
        LangPref::En => t(Key::LanguageEn),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preference_resolves_to_one_of_two_languages() {
        assert_eq!(
            LangPref::Zh.resolve(false),
            Lang::Zh,
            "强制中文，系统英文也是中文"
        );
        assert_eq!(
            LangPref::En.resolve(true),
            Lang::En,
            "强制英文，系统中文也是英文"
        );
        assert_eq!(LangPref::System.resolve(true), Lang::Zh);
        assert_eq!(
            LangPref::System.resolve(false),
            Lang::En,
            "跟随系统：非中文一律英文（没有第三种语言）"
        );
    }

    #[test]
    fn saved_values_round_trip_and_unknown_falls_back_to_system() {
        for pref in [LangPref::System, LangPref::Zh, LangPref::En] {
            assert_eq!(LangPref::parse(pref.as_str()), pref);
        }
        assert_eq!(LangPref::parse("fr"), LangPref::System);
        assert_eq!(LangPref::parse(""), LangPref::System);
    }

    #[test]
    fn every_key_has_both_languages() {
        for key in [
            Key::TrayShowPrefix,
            Key::TrayLock,
            Key::TrayCheckUpdate,
            Key::TrayRestartInstallPrefix,
            Key::TrayQuit,
            Key::MenuConnect,
            Key::MenuConnectNewTab,
            Key::MenuOpenSftp,
            Key::MenuCopyName,
            Key::MenuCopyAddress,
            Key::MenuCopyPassword,
            Key::MenuCopyInfo,
            Key::MenuInstallPublicKey,
            Key::MenuDelete,
            Key::MenuForwards,
            Key::MenuMoveToGroup,
            Key::MenuMoveUp,
            Key::MenuMoveDown,
            Key::MenuProperties,
            Key::MenuSortByName,
            Key::LanguageCardTitle,
            Key::LanguageFollowSystem,
            Key::LanguageZh,
            Key::LanguageEn,
            Key::LanguageRestartHint,
        ] {
            assert!(!key.zh().is_empty(), "{key:?} 缺中文");
            assert!(!key.en().is_empty(), "{key:?} 缺英文");
        }
    }

    #[test]
    fn keys_translate_in_both_languages() {
        // 不改进程级当前语言：其它测试并行跑，改全局会让它们的断言看到英文。
        assert_eq!(current(), Lang::Zh, "没设过时默认中文");
        assert_eq!(Key::TrayQuit.zh(), "退出");
        assert_eq!(Key::TrayQuit.en(), "Quit");
        assert_eq!(Key::MenuDelete.zh(), "删除");
        assert_eq!(Key::MenuDelete.en(), "Delete");
        assert_eq!(Key::LanguageZh.en(), "简体中文", "语言名两边都写自己的名字");
    }
}
