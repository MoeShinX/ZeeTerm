//! 字体：终端字体栈的解析、本机已装字体的筛选、「加载失败就回退」的规则（V2-G06x），
//! 以及界面（非终端）字体的选择（V2-U4）。
//!
//! 0.2.14 的设置存一整条 CSS 字体栈（`JetBrains Mono, Cascadia Mono, Consolas, monospace`），
//! 浏览器按顺序找、找不到的跳过。GPUI 一次只认一个字体名，而且名字查不到时会退到界面字体
//! （比例字体，终端的格子宽度就全错了），所以这里自己做同一件事：按栈的顺序取第一个
//! **本机装了的**，一个都没有就用主题自带的等宽字体。

/// 设置页下拉里供选的常见等宽字体（同 0.2.14 `SettingsPage.tsx` 的 `COMMON_MONO_FONTS`）。
pub const COMMON_MONO_FONTS: &[&str] = &[
    "JetBrains Mono",
    "Cascadia Mono",
    "Cascadia Code",
    "Consolas",
    "Courier New",
    "Lucida Console",
    "Source Code Pro",
    "Fira Code",
];

/// CSS 里的通用族名：不是具体字体，不拿去本机查。
const GENERIC_FAMILIES: &[&str] = &[
    "monospace",
    "serif",
    "sans-serif",
    "cursive",
    "fantasy",
    "system-ui",
    "ui-monospace",
];

/// 把字体栈拆成一个个字体名：按逗号分，去掉空白和包着名字的引号，空项丢掉。
pub fn stack_families(stack: &str) -> Vec<String> {
    stack
        .split(',')
        .map(|part| part.trim().trim_matches(['"', '\'']).trim().to_owned())
        .filter(|name| !name.is_empty())
        .collect()
}

fn is_generic(name: &str) -> bool {
    GENERIC_FAMILIES
        .iter()
        .any(|generic| generic.eq_ignore_ascii_case(name))
}

fn installed_match<'a>(name: &str, installed: &'a [String]) -> Option<&'a String> {
    installed
        .iter()
        .find(|candidate| candidate.eq_ignore_ascii_case(name))
}

/// 栈里第一个本机装了的字体名（用本机列表里的写法）；没有返回 `None`。
pub fn first_installed(stack: &str, installed: &[String]) -> Option<String> {
    stack_families(stack)
        .iter()
        .filter(|name| !is_generic(name))
        .find_map(|name| installed_match(name, installed).cloned())
}

/// 终端实际用的字体：栈里第一个装了的，没有就用 `fallback`（主题自带的等宽字体）。
pub fn resolve_family(stack: &str, installed: &[String], fallback: &str) -> String {
    first_installed(stack, installed).unwrap_or_else(|| fallback.to_owned())
}

/// 常见等宽字体里本机装了的（下拉里只列这些，顺序同 [`COMMON_MONO_FONTS`]）。
pub fn installed_common(installed: &[String]) -> Vec<&'static str> {
    COMMON_MONO_FONTS
        .iter()
        .copied()
        .filter(|name| installed_match(name, installed).is_some())
        .collect()
}

/// 栈的第一个字体名（下拉里当前选中的那项）。
pub fn leading_family(stack: &str) -> Option<String> {
    stack_families(stack).into_iter().next()
}

/// 栈的第一个名字是不是常见字体之一（不是就当成「自定义」）。
pub fn is_common(stack: &str) -> bool {
    leading_family(stack).is_some_and(|first| {
        COMMON_MONO_FONTS
            .iter()
            .any(|common| common.eq_ignore_ascii_case(&first))
    })
}

/// 在下拉里选了一个常见字体：写成「它 + 常用的后备」（同 0.2.14）。
pub fn stack_for_choice(name: &str) -> String {
    format!("{name}, Cascadia Mono, Consolas, monospace")
}

/// 字体回退提示的一句话。
#[derive(Clone, Copy, Debug)]
#[allow(clippy::enum_variant_names)]
enum FontsMsg {
    FallbackPrefix,
    FallbackMid,
    FallbackSuffix,
}

impl crate::i18n::Msg for FontsMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::FallbackPrefix => "本机没有「",
            Self::FallbackMid => "」，现在用的是「",
            Self::FallbackSuffix => "」。",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::FallbackPrefix => "This computer does not have “",
            Self::FallbackMid => "”; using “",
            Self::FallbackSuffix => "” instead.",
        }
    }
}

/// 设置的字体和实际用的字体不一样时给用户的一句话（栈里第一个字体本机没有）。
pub fn fallback_note(stack: &str, installed: &[String], fallback: &str) -> Option<String> {
    let wanted = stack_families(stack)
        .into_iter()
        .find(|name| !is_generic(name))?;
    let used = resolve_family(stack, installed, fallback);
    if used.eq_ignore_ascii_case(&wanted) {
        return None;
    }
    Some(format!(
        "{}{wanted}{}{used}{}",
        crate::i18n::t(FontsMsg::FallbackPrefix),
        crate::i18n::t(FontsMsg::FallbackMid),
        crate::i18n::t(FontsMsg::FallbackSuffix)
    ))
}

/// 界面西文字体的候选，按好看程度排：Windows 11 自带的 `Segoe UI Variable Text`（为正文大小调过的可变字体），
/// 没有（Windows 10）就用 `Segoe UI`。
pub const UI_LATIN_FONTS: &[&str] = &["Segoe UI Variable Text", "Segoe UI"];
/// 界面中文字体的候选：`Microsoft YaHei UI`（UI 版行距更紧、字形和 Segoe UI 搭得更齐），没有就用普通的雅黑。
pub const UI_CJK_FONTS: &[&str] = &["Microsoft YaHei UI", "Microsoft YaHei"];
/// GPUI 的虚拟族名：映射到系统当前的界面字体（`SPI_GETICONTITLELOGFONT`，中文版 Windows 上是雅黑 UI）。
pub const SYSTEM_UI_FONT: &str = ".SystemUIFont";

/// 界面实际用的字体：主字体 + 主字体没有的字（中文）才去找的后备字体。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiFont {
    /// 主字体族名；候选一个都没装时是 [`SYSTEM_UI_FONT`]，交给系统决定。
    pub family: String,
    /// 后备字体族名（空 = 不指定，由 DirectWrite 按系统的字体回退表选）。
    pub fallbacks: Vec<String>,
}

/// 按本机已装字体选界面字体：西文取 [`UI_LATIN_FONTS`] 里第一个装了的，中文取 [`UI_CJK_FONTS`] 里第一个装了的
/// 放进后备。字体存不存在只看传进来的名单，所以 Windows 10（没有 Segoe UI Variable）、
/// 没装雅黑的系统都落到各自能用的组合上，不会出现方框；一个候选都没有就是「全交给系统」。
pub fn resolve_ui_font(installed: &[String]) -> UiFont {
    let pick = |candidates: &[&str]| {
        candidates
            .iter()
            .find_map(|name| installed_match(name, installed).cloned())
    };
    let family = pick(UI_LATIN_FONTS).unwrap_or_else(|| SYSTEM_UI_FONT.to_owned());
    let fallbacks = pick(UI_CJK_FONTS)
        .filter(|cjk| !cjk.eq_ignore_ascii_case(&family))
        .into_iter()
        .collect();
    UiFont { family, fallbacks }
}

/// 本机的界面字体，整个进程里只算一次。
pub fn ui_font(cx: &::gpui::App) -> &'static UiFont {
    static FONT: std::sync::OnceLock<UiFont> = std::sync::OnceLock::new();
    FONT.get_or_init(|| {
        let font = resolve_ui_font(installed_fonts(cx));
        tracing::info!("界面字体：{}，后备 {:?}", font.family, font.fallbacks);
        font
    })
}

/// 本机已装字体名，整个进程里只列一次（枚举要花几十毫秒，运行期间系统字体不会变）。
pub fn installed_fonts(cx: &::gpui::App) -> &'static [String] {
    static NAMES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| cx.text_system().all_font_names())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn a_css_stack_is_split_into_family_names() {
        assert_eq!(
            stack_families("JetBrains Mono, \"Cascadia Mono\", 'Consolas' ,, monospace"),
            vec!["JetBrains Mono", "Cascadia Mono", "Consolas", "monospace"]
        );
        assert!(stack_families("  , ").is_empty());
    }

    #[test]
    fn the_first_installed_family_in_the_stack_wins() {
        let stack = "JetBrains Mono, Cascadia Mono, Consolas, monospace";
        let fonts = installed(&["Arial", "Consolas", "Cascadia Mono"]);
        assert_eq!(
            first_installed(stack, &fonts).as_deref(),
            Some("Cascadia Mono")
        );
        let only_consolas = installed(&["Consolas"]);
        assert_eq!(
            first_installed(stack, &only_consolas).as_deref(),
            Some("Consolas")
        );
    }

    #[test]
    fn matching_ignores_case_and_returns_the_system_spelling() {
        let fonts = installed(&["Cascadia Mono"]);
        assert_eq!(
            first_installed("cascadia mono", &fonts).as_deref(),
            Some("Cascadia Mono")
        );
    }

    #[test]
    fn generic_names_are_never_looked_up_on_the_machine() {
        let fonts = installed(&["monospace"]);
        assert_eq!(first_installed("monospace", &fonts), None);
    }

    #[test]
    fn nothing_installed_falls_back_to_the_theme_font() {
        let fonts = installed(&["Arial"]);
        assert_eq!(
            resolve_family("JetBrains Mono, Consolas, monospace", &fonts, "Courier New"),
            "Courier New"
        );
        assert_eq!(resolve_family("", &fonts, "Courier New"), "Courier New");
    }

    #[test]
    fn the_dropdown_only_lists_installed_common_fonts() {
        let fonts = installed(&["Consolas", "Fira Code", "Comic Sans MS", "courier new"]);
        assert_eq!(
            installed_common(&fonts),
            vec!["Consolas", "Courier New", "Fira Code"]
        );
        assert!(installed_common(&installed(&["Arial"])).is_empty());
    }

    #[test]
    fn choosing_a_common_font_writes_it_with_the_usual_fallbacks() {
        assert_eq!(
            stack_for_choice("Fira Code"),
            "Fira Code, Cascadia Mono, Consolas, monospace"
        );
        assert_eq!(
            leading_family(&stack_for_choice("Fira Code")).as_deref(),
            Some("Fira Code")
        );
    }

    #[test]
    fn a_stack_starting_with_an_unknown_font_counts_as_custom() {
        assert!(is_common("Consolas, monospace"));
        assert!(is_common("jetbrains mono"));
        assert!(!is_common("Iosevka, monospace"));
        assert!(!is_common(""));
    }

    #[test]
    fn the_ui_font_prefers_segoe_ui_variable_text_with_yahei_ui_for_chinese() {
        let fonts = installed(&[
            "Segoe UI",
            "Segoe UI Variable Text",
            "Microsoft YaHei",
            "Microsoft YaHei UI",
        ]);
        assert_eq!(
            resolve_ui_font(&fonts),
            UiFont {
                family: "Segoe UI Variable Text".to_owned(),
                fallbacks: vec!["Microsoft YaHei UI".to_owned()],
            }
        );
    }

    #[test]
    fn windows_10_without_segoe_ui_variable_falls_back_to_segoe_ui() {
        let fonts = installed(&["Segoe UI", "Microsoft YaHei UI", "Arial"]);
        let font = resolve_ui_font(&fonts);
        assert_eq!(font.family, "Segoe UI");
        assert_eq!(font.fallbacks, vec!["Microsoft YaHei UI"]);
    }

    #[test]
    fn without_yahei_ui_the_plain_yahei_is_the_chinese_fallback() {
        let fonts = installed(&["Segoe UI Variable Text", "Microsoft YaHei"]);
        assert_eq!(
            resolve_ui_font(&fonts).fallbacks,
            vec!["Microsoft YaHei".to_owned()]
        );
    }

    #[test]
    fn without_any_candidate_the_system_decides_and_no_fallback_is_forced() {
        let fonts = installed(&["Arial", "SimSun"]);
        assert_eq!(
            resolve_ui_font(&fonts),
            UiFont {
                family: SYSTEM_UI_FONT.to_owned(),
                fallbacks: Vec::new(),
            }
        );
        assert_eq!(resolve_ui_font(&[]).family, SYSTEM_UI_FONT);
    }

    #[test]
    fn the_ui_font_matches_names_ignoring_case_and_uses_the_system_spelling() {
        let fonts = installed(&["segoe ui variable text", "MICROSOFT YAHEI UI"]);
        let font = resolve_ui_font(&fonts);
        assert_eq!(font.family, "segoe ui variable text");
        assert_eq!(font.fallbacks, vec!["MICROSOFT YAHEI UI"]);
    }

    #[test]
    fn a_chinese_font_never_doubles_as_its_own_fallback() {
        // 极端情况：只装了雅黑 UI。它当不了「西文候选」，主字体交给系统，中文后备仍是它。
        let fonts = installed(&["Microsoft YaHei UI"]);
        let font = resolve_ui_font(&fonts);
        assert_eq!(font.family, SYSTEM_UI_FONT);
        assert_eq!(font.fallbacks, vec!["Microsoft YaHei UI"]);
    }

    #[test]
    fn the_note_appears_only_when_the_wanted_font_is_missing() {
        let fonts = installed(&["Consolas"]);
        let stack = "JetBrains Mono, Consolas, monospace";
        // 想要的 JetBrains Mono 没装，实际用 Consolas。
        let note = fallback_note(stack, &fonts, "Courier New").expect("应该有提示");
        assert!(
            note.contains("JetBrains Mono") && note.contains("Consolas"),
            "{note}"
        );
        // 想要的装了：没有提示。
        assert_eq!(
            fallback_note("Consolas, monospace", &fonts, "Courier New"),
            None
        );
        // 只写通用族名：没有「想要的具体字体」，不提示。
        assert_eq!(fallback_note("monospace", &fonts, "Courier New"), None);
    }
}
