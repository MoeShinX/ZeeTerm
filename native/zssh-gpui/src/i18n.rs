//! 界面语言：只有简体中文和英文两种。
//!
//! 规则（V2-I18N）：
//! - 应用自己的文案才翻译；远程服务器的输出、用户名、路径、命令、用户自定义名称（主机名、
//!   分组名、标签名……）一律原样显示，不走这里；
//! - 底层错误尽量带着类型和参数上到界面，在这里按语言生成提示，不靠替换中文字符串；
//! - 没选过语言（全新安装、老设置文件里没有语言字段）→ 跟随系统：中文系统用中文，其他一律
//!   英文；选过的照旧（见 `settings.rs` 的 `language_follows_the_system_unless_chosen`）。

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

/// 全局语言。这里的「没设过」只发生在设置还没加载时（测试、第一个窗口早于设置加载），
/// 暂按中文；设置加载后按偏好和系统语言重新算，不是「老用户一律中文」。
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
    // Telnet / 串口会话往终端写的提示在 zssh-protocols 里生成，那个 crate 不依赖界面层：语言变了就告诉它。
    zssh_protocols::set_language(match lang {
        Lang::Zh => zssh_protocols::Language::Zh,
        Lang::En => zssh_protocols::Language::En,
    });
    cx.set_global(I18n);
    // 借用主题切换那条「所有窗口重绘」的路径，明暗保持当前值不变。
    crate::app_theme::set_mode(crate::app_theme::is_dark(cx), cx);
}

/// 一条界面文案：两个语言各一句。各功能模块可以在自己的文件里定义枚举并实现它，
/// 不用把全应用的文案堆在一个大表里（也避免多人同时改一个文件）。
pub(crate) trait Msg {
    fn zh(&self) -> &'static str;
    fn en(&self) -> &'static str;
}

/// 取一句界面文案。键是稳定标识，不是中文原文——以后加语言只加表，不改调用点。
///
/// 只放**应用自己的**文案：远端输出、用户名、路径、命令、用户自定义名称都不经这里。
pub(crate) fn t<M: Msg>(key: M) -> &'static str {
    match current() {
        Lang::Zh => Msg::zh(&key),
        Lang::En => Msg::en(&key),
    }
}

/// 现在是不是英文界面（需要自己拼句子、带数字/单复数的文案用）。
pub(crate) fn is_en() -> bool {
    current() == Lang::En
}

/// 英文单复数：`n == 1` 用 `one`，其余用 `many`。中文没有单复数，调用方按 `is_en()` 分开拼。
pub(crate) fn plural(n: u64, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 { one } else { many }
}

/// 计数短语：中文 `{n}{zh_suffix}`（后缀自带量词，如 `" 项"`），英文 `{n} {one|many}`。
///
/// 只适用于"数字在前"的短语；`还有 N 项…` 这种整句请用整句模板，不要用片段拼。
pub(crate) fn count(
    n: u64,
    zh_suffix: &str,
    en_one: &'static str,
    en_many: &'static str,
) -> String {
    if is_en() {
        format!("{n} {}", plural(n, en_one, en_many))
    } else {
        format!("{n}{zh_suffix}")
    }
}

/// 标点和连接符：中文用全角（`名称（说明）`、`项目：值`、`「名称」`、顿号 / 全角逗号），
/// 英文用半角（`Name (note)`、`Label: value`、`“Name”`、`, `）。
///
/// 拼句子时统一走下面的 [`paren`] / [`colon`] / [`quote`] / [`list_sep`] / [`comma`] /
/// [`semicolon`]，不要在调用点写死全角字符（英文界面会露出来）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Punct {
    ParenOpen,
    ParenClose,
    WrapOpen,
    WrapClose,
    Colon,
    QuoteOpen,
    QuoteClose,
    ListSep,
    Comma,
    Semicolon,
}

impl Msg for Punct {
    fn zh(&self) -> &'static str {
        match self {
            Self::ParenOpen => "（",
            Self::ParenClose => "）",
            Self::WrapOpen => "（",
            Self::WrapClose => "）",
            Self::Colon => "：",
            Self::QuoteOpen => "「",
            Self::QuoteClose => "」",
            Self::ListSep => "、",
            Self::Comma => "，",
            Self::Semicolon => "；",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::ParenOpen => " (",
            Self::ParenClose => ")",
            Self::WrapOpen => "(",
            Self::WrapClose => ")",
            Self::Colon => ": ",
            Self::QuoteOpen => "“",
            Self::QuoteClose => "”",
            Self::ListSep => ", ",
            Self::Comma => ", ",
            Self::Semicolon => "; ",
        }
    }
}

/// 括注：中文 `名称（说明）`，英文 `Name (note)`。
pub(crate) fn paren(main: &str, note: &str) -> String {
    format!(
        "{main}{}{note}{}",
        t(Punct::ParenOpen),
        t(Punct::ParenClose)
    )
}

/// 把一小段内容括起来（前面不加空格，由所在布局的间距管）：中文 `（内容）`，英文 `(content)`。
pub(crate) fn parens(inner: &str) -> String {
    format!("{}{inner}{}", t(Punct::WrapOpen), t(Punct::WrapClose))
}

/// 冒号分隔：中文 `项目：值`，英文 `Label: value`。
pub(crate) fn colon(label: &str, value: &str) -> String {
    format!("{label}{}{value}", t(Punct::Colon))
}

/// 引用一个名字：中文 `「名称」`，英文 `“Name”`。
pub(crate) fn quote(name: &str) -> String {
    format!("{}{name}{}", t(Punct::QuoteOpen), t(Punct::QuoteClose))
}

/// 名单里的连接符：中文顿号，英文 `, `。
pub(crate) fn list_sep() -> &'static str {
    t(Punct::ListSep)
}

/// 子句之间的逗号：中文 `，`，英文 `, `。
pub(crate) fn comma() -> &'static str {
    t(Punct::Comma)
}

/// 并列说明之间的分号：中文 `；`，英文 `; `。
pub(crate) fn semicolon() -> &'static str {
    t(Punct::Semicolon)
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
    MenuInstallPubkey,
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
    // ---- 设置页导航 ----
    NavCredentials,
    NavQuickCommands,
    NavProxies,
    NavForwards,
    NavAppearance,
    NavTerminal,
    NavConnection,
    NavSftp,
    NavAi,
    NavSync,
    NavLock,
    NavShortcuts,
    NavAbout,
}

impl Msg for Key {
    fn zh(&self) -> &'static str {
        let key = *self;
        match key {
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
            Self::MenuInstallPubkey => "安装公钥…",
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
            Self::NavCredentials => "凭据",
            Self::NavQuickCommands => "快捷命令",
            Self::NavProxies => "代理",
            Self::NavForwards => "端口转发",
            Self::NavAppearance => "外观",
            Self::NavTerminal => "终端",
            Self::NavConnection => "连接",
            Self::NavSftp => "SFTP",
            Self::NavAi => "AI",
            Self::NavSync => "同步",
            Self::NavLock => "锁屏",
            Self::NavShortcuts => "快捷键",
            Self::NavAbout => "关于",
        }
    }

    fn en(&self) -> &'static str {
        let key = *self;
        match key {
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
            Self::MenuInstallPubkey => "Install Public Key…",
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
            Self::NavCredentials => "Credentials",
            Self::NavQuickCommands => "Snippets",
            Self::NavProxies => "Proxies",
            Self::NavForwards => "Port Forwarding",
            Self::NavAppearance => "Appearance",
            Self::NavTerminal => "Terminal",
            Self::NavConnection => "Connection",
            Self::NavSftp => "SFTP",
            Self::NavAi => "AI",
            Self::NavSync => "Sync",
            Self::NavLock => "App Lock",
            Self::NavShortcuts => "Shortcuts",
            Self::NavAbout => "About",
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
            Key::MenuInstallPubkey,
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
            Key::NavCredentials,
            Key::NavQuickCommands,
            Key::NavProxies,
            Key::NavForwards,
            Key::NavAppearance,
            Key::NavTerminal,
            Key::NavConnection,
            Key::NavSftp,
            Key::NavAi,
            Key::NavSync,
            Key::NavLock,
            Key::NavShortcuts,
            Key::NavAbout,
        ] {
            assert!(!key.zh().is_empty(), "{key:?} 缺中文");
            assert!(!key.en().is_empty(), "{key:?} 缺英文");
        }
    }

    #[test]
    fn punctuation_follows_the_language() {
        // 默认中文：全角标点。
        assert_eq!(current(), Lang::Zh, "没设过时默认中文");
        assert_eq!(paren("名称", "说明"), "名称（说明）");
        assert_eq!(parens("3/5"), "（3/5）");
        assert_eq!(colon("项目", "值"), "项目：值");
        assert_eq!(quote("web"), "「web」");
        assert_eq!(list_sep(), "、");
        assert_eq!(comma(), "，");
        assert_eq!(semicolon(), "；");
        // 英文：半角，而且一个全角字符都不能漏出来。
        for punct in [
            Punct::ParenOpen,
            Punct::ParenClose,
            Punct::WrapOpen,
            Punct::WrapClose,
            Punct::Colon,
            Punct::QuoteOpen,
            Punct::QuoteClose,
            Punct::ListSep,
            Punct::Comma,
            Punct::Semicolon,
        ] {
            assert!(
                punct
                    .en()
                    .chars()
                    .all(|c| c.is_ascii() || c == '“' || c == '”'),
                "{punct:?} 的英文里有全角字符"
            );
        }
        assert_eq!(Punct::ParenOpen.en(), " (");
        assert_eq!(Punct::Colon.en(), ": ");
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
