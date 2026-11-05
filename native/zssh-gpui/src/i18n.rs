//! 界面语言：只有简体中文和英文两种。
//!
//! 规则（V2-I18N）：
//! - 应用自己的文案才翻译；远程服务器的输出、用户名、路径、命令、用户自定义名称（主机名、
//!   分组名、标签名……）一律原样显示，不走这里；
//! - 底层错误尽量带着类型和参数上到界面，在这里按语言生成提示，不靠替换中文字符串；
//! - 现有用户的设置文件里没有语言字段 → 按中文处理（保留他们原来的习惯）；全新安装 →
//!   跟随系统。
//!
//! 文案表（`Key` / `t`）随第一个翻译面（托盘、主机右键菜单）一起加进来。

use ::gpui::{App, Global};

/// 实际生效的语言。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Lang {
    #[default]
    Zh,
    En,
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

/// 切换语言：写全局后让所有窗口重绘。已开着的窗口读的是同一个全局，下一帧就是新文案。
pub(crate) fn set(lang: Lang, cx: &mut App) {
    let _ = lang;
    cx.set_global(I18n);
    // 借用主题切换那条「所有窗口重绘」的路径，明暗保持当前值不变。
    crate::app_theme::set_mode(crate::app_theme::is_dark(cx), cx);
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
    fn unknown_saved_values_fall_back_to_system() {
        assert_eq!(LangPref::parse("zh"), LangPref::Zh);
        assert_eq!(LangPref::parse("EN"), LangPref::En);
        assert_eq!(LangPref::parse("fr"), LangPref::System);
        assert_eq!(LangPref::parse(""), LangPref::System);
    }
}
