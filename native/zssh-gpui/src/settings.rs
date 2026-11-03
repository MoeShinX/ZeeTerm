//! 界面设置的持久化：主题、字号、行高、光标样式。
//!
//! 设置写在数据目录下的 `ui-settings.json`；文件缺失或损坏时按默认值启动，
//! 不阻塞界面。写入先写临时文件再改名，避免中途崩溃留下半截 JSON。

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::highlight::HighlightRule;

// 范围和默认值取 0.2.14 的设置（`stores/settings.ts` 的默认值、设置页滑块的范围，同步校验也是这个范围）。
pub const MIN_FONT_SIZE: f32 = 10.0;
pub const MAX_FONT_SIZE: f32 = 24.0;
pub const MIN_LINE_HEIGHT: f32 = 1.0;
pub const MAX_LINE_HEIGHT: f32 = 1.6;
pub const DEFAULT_FONT_SIZE: f32 = 12.0;
pub const DEFAULT_LINE_HEIGHT: f32 = 1.3;
/// 回滚行数：1000～100000，默认 10000；设置页滑块的步长是 1000。上限是硬的——回滚按行占内存，
/// 不能让一个设置值把它撑到没边。
pub const MIN_SCROLLBACK: usize = 1_000;
pub const MAX_SCROLLBACK: usize = 100_000;
pub const DEFAULT_SCROLLBACK: usize = 10_000;
pub const SCROLLBACK_STEP: usize = 1_000;
/// 终端字体栈（CSS 写法，逗号隔开，靠前的优先）。GPUI 一次只认一个字体名，所以实际用的是栈里
/// 第一个本机装了的（见 `fonts`），一个都没有就退回主题自带的等宽字体。
pub const DEFAULT_FONT_FAMILY: &str = "JetBrains Mono, Cascadia Mono, Consolas, monospace";
/// 字体栈最长这么多个字符（同步校验的上限）。
pub const MAX_FONT_FAMILY_CHARS: usize = 200;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    /// 竖线（0.2.14 的 `bar`）。
    Bar,
}

/// 括号粘贴：自动跟随远端终端模式，或强制开/关。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BracketedPaste {
    #[default]
    Auto,
    On,
    Off,
}

impl BracketedPaste {
    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::On,
            Self::On => Self::Off,
            Self::Off => Self::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动",
            Self::On => "开",
            Self::Off => "关",
        }
    }

    /// 远端终端是否已经开了括号粘贴。
    pub fn wraps(self, terminal_mode: bool) -> bool {
        match self {
            Self::Auto => terminal_mode,
            Self::On => true,
            Self::Off => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    /// 深色主题；`follow_system` 开着时只当作读不到系统值时的回退（新装默认深色）。
    pub dark: bool,
    /// 跟随系统主题（V2-U17 起新装默认开）；`dark` 作为读不到系统值时的回退。
    pub follow_system: bool,
    /// 浅色、深色各用哪个皮肤（`app_theme::SKINS` 的 id）。
    pub skin_light: String,
    pub skin_dark: String,
    /// 终端配色：`skin` = 跟随皮肤（默认），或某个皮肤的 id。
    pub terminal_theme: String,
    /// 终端字体栈，见 [`DEFAULT_FONT_FAMILY`]。
    pub font_family: String,
    pub font_size: f32,
    pub line_height: f32,
    /// 回滚行数（新开的和已开的窗格都用它）。
    pub scrollback: usize,
    pub cursor: CursorShape,
    /// 光标闪烁（0.2.14 `terminal_cursor_blink`，默认开）。
    pub cursor_blink: bool,
    /// 焦点进终端时自动切到英文输入（0.2.14 `terminal_ime_english`，默认开）。
    pub ime_english: bool,
    /// 选中即复制。
    pub copy_on_select: bool,
    /// 右键直接粘贴。新用户默认关：关着时右键弹终端菜单（V2-U25）；在链接上右键不论开关都弹链接菜单。
    pub right_click_paste: bool,
    /// 多行粘贴先确认。
    pub confirm_multiline: bool,
    /// 高亮总开关。
    pub highlight_enabled: bool,
    /// 终端右上角显示延迟徽标（SSH 往返 / Telnet 的 TCP 连接）。默认开；关着时也不发测量请求。
    pub show_latency: bool,
    /// 启动时直接隐藏到托盘。
    pub start_hidden: bool,
    /// 高亮规则；`None` 表示用默认那一组。
    pub highlight_rules: Option<Vec<HighlightRule>>,
    /// 括号粘贴模式。
    pub bracketed_paste: BracketedPaste,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            dark: true,
            follow_system: true,
            skin_light: crate::app_theme::DEFAULT_LIGHT_SKIN.to_owned(),
            skin_dark: crate::app_theme::DEFAULT_DARK_SKIN.to_owned(),
            terminal_theme: crate::app_theme::DEFAULT_TERMINAL_THEME.to_owned(),
            font_family: DEFAULT_FONT_FAMILY.to_owned(),
            font_size: DEFAULT_FONT_SIZE,
            line_height: DEFAULT_LINE_HEIGHT,
            scrollback: DEFAULT_SCROLLBACK,
            cursor: CursorShape::Block,
            cursor_blink: true,
            ime_english: true,
            copy_on_select: false,
            right_click_paste: false,
            confirm_multiline: true,
            highlight_enabled: true,
            show_latency: true,
            start_hidden: false,
            highlight_rules: None,
            bracketed_paste: BracketedPaste::Auto,
        }
    }
}

impl UiSettings {
    /// 把越界/非有限值收回到可用范围。
    pub fn sanitized(mut self) -> Self {
        if !self.font_size.is_finite() {
            self.font_size = DEFAULT_FONT_SIZE;
        }
        if !self.line_height.is_finite() {
            self.line_height = DEFAULT_LINE_HEIGHT;
        }
        self.font_size = self.font_size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
        self.line_height = self.line_height.clamp(MIN_LINE_HEIGHT, MAX_LINE_HEIGHT);
        self.scrollback = self.scrollback.clamp(MIN_SCROLLBACK, MAX_SCROLLBACK);
        self.font_family = sanitize_font_family(&self.font_family);
        // 认不得的皮肤 id（文件被改过、皮肤表以后删了一套）退回默认；明暗对不上的 id 由
        // `skin_for` 在用的时候兜底，这里只管完全不存在的。
        if crate::app_theme::skin(&self.skin_light).is_none_or(|skin| skin.dark) {
            self.skin_light = crate::app_theme::DEFAULT_LIGHT_SKIN.to_owned();
        }
        if crate::app_theme::skin(&self.skin_dark).is_none_or(|skin| !skin.dark) {
            self.skin_dark = crate::app_theme::DEFAULT_DARK_SKIN.to_owned();
        }
        self.terminal_theme =
            crate::app_theme::normalize_terminal_theme(&self.terminal_theme).to_owned();
        self
    }

    /// 这份设置对应的外观选择（给 `app_theme::set_look`）。
    pub fn look(&self) -> crate::app_theme::Look {
        crate::app_theme::Look {
            skin_light: self.skin_light.clone(),
            skin_dark: self.skin_dark.clone(),
            terminal_theme: self.terminal_theme.clone(),
        }
    }

    pub fn load() -> Self {
        Self::load_from(&settings_path())
            .map(Self::sanitized)
            .unwrap_or_default()
    }

    /// 读不到、解析失败都返回 `None`，由调用方用默认值。
    pub fn load_from(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }
}

/// 字体栈：去掉首尾空白；空的、太长的、带控制字符的都退回默认（同步校验也是这三条）。
pub fn sanitize_font_family(raw: &str) -> String {
    let trimmed = raw.trim();
    let usable = !trimmed.is_empty()
        && trimmed.chars().count() <= MAX_FONT_FAMILY_CHARS
        && !trimmed.chars().any(char::is_control);
    if usable {
        trimmed.to_owned()
    } else {
        DEFAULT_FONT_FAMILY.to_owned()
    }
}

/// `ZSSH_DATA_DIR` 优先，其次 `%APPDATA%\ZeeTerm`（与 Tauri 版 Z-SSH 的数据目录分开）。
pub fn data_dir() -> PathBuf {
    resolve_data_dir(
        std::env::var_os("ZSSH_DATA_DIR"),
        std::env::var_os("APPDATA"),
    )
}

fn resolve_data_dir(
    override_dir: Option<std::ffi::OsString>,
    appdata: Option<std::ffi::OsString>,
) -> PathBuf {
    if let Some(dir) = override_dir {
        return PathBuf::from(dir);
    }
    if let Some(appdata) = appdata {
        return PathBuf::from(appdata).join(crate::product::DATA_DIR_NAME);
    }
    std::env::temp_dir().join("zssh-native")
}

pub fn settings_path() -> PathBuf {
    data_dir().join("ui-settings.json")
}

/// 跟随系统时用系统值，读不到回退到已保存的 `dark`。
pub fn resolve_dark(follow_system: bool, fallback_dark: bool, system_light: Option<bool>) -> bool {
    if follow_system {
        system_light.map(|light| !light).unwrap_or(fallback_dark)
    } else {
        fallback_dark
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_data_dir_is_zeeterm_under_appdata() {
        let appdata = std::ffi::OsString::from(r"C:\data\roaming");
        assert_eq!(
            resolve_data_dir(None, Some(appdata.clone())),
            PathBuf::from(r"C:\data\roaming").join("ZeeTerm")
        );
        // 环境变量优先；也不再落到 Tauri 版 Z-SSH 的目录下。
        assert_eq!(
            resolve_data_dir(Some(r"D:\isolated".into()), Some(appdata.clone())),
            PathBuf::from(r"D:\isolated")
        );
        let default = resolve_data_dir(None, Some(appdata));
        assert!(!default.to_string_lossy().contains("Z-SSH"));
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zssh-settings-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trips_through_json() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("ui-settings.json");
        let settings = UiSettings {
            dark: false,
            follow_system: true,
            skin_light: "github-light".to_owned(),
            skin_dark: "nord".to_owned(),
            terminal_theme: "skin".to_owned(),
            font_family: "Cascadia Code, monospace".to_owned(),
            font_size: 17.0,
            line_height: 1.5,
            scrollback: 25_000,
            cursor: CursorShape::Bar,
            cursor_blink: false,
            ime_english: false,
            copy_on_select: true,
            right_click_paste: true,
            confirm_multiline: false,
            highlight_enabled: false,
            show_latency: false,
            start_hidden: true,
            highlight_rules: Some(vec![HighlightRule::new(3, "err(or)?", true)]),
            bracketed_paste: BracketedPaste::On,
        };
        settings.save_to(&path).unwrap();
        assert_eq!(UiSettings::load_from(&path), Some(settings));
    }

    #[test]
    fn an_old_file_without_the_new_fields_gets_the_0_2_14_defaults() {
        let dir = temp_dir("old-terminal");
        let path = dir.join("ui-settings.json");
        std::fs::write(&path, br#"{"font_size": 15.0, "cursor": "underline"}"#).unwrap();
        let loaded = UiSettings::load_from(&path)
            .expect("旧文件照常读")
            .sanitized();
        assert!(loaded.cursor_blink, "光标闪烁默认开");
        assert!(loaded.ime_english, "自动英文输入默认开");
        assert!(loaded.show_latency, "延迟徽标默认开");
        assert_eq!(loaded.cursor, CursorShape::Underline);
    }

    #[test]
    fn the_bar_cursor_round_trips_as_bar() {
        let json = serde_json::to_string(&CursorShape::Bar).unwrap();
        assert_eq!(json, "\"bar\"");
        assert_eq!(
            serde_json::from_str::<CursorShape>(&json).unwrap(),
            CursorShape::Bar
        );
    }

    #[test]
    fn missing_file_falls_back() {
        let dir = temp_dir("missing");
        assert_eq!(UiSettings::load_from(&dir.join("nope.json")), None);
        assert_eq!(
            UiSettings::load_from(&dir.join("nope.json")).unwrap_or_default(),
            UiSettings::default()
        );
    }

    #[test]
    fn corrupt_file_falls_back() {
        let dir = temp_dir("corrupt");
        let path = dir.join("ui-settings.json");
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(UiSettings::load_from(&path), None);
    }

    #[test]
    fn a_leftover_external_editor_field_from_a_dev_build_is_ignored() {
        // 外部编辑器按 0.2.14 / R-8 已经移除；开发期写进 ui-settings.json 的字段必须能正常读，不报错。
        let dir = temp_dir("leftover-external");
        let path = dir.join("ui-settings.json");
        std::fs::write(
            &path,
            br#"{"font_size": 15.0, "external_editor": "notepad"}"#,
        )
        .unwrap();
        let settings = UiSettings::load_from(&path).expect("多余字段不能让读取失败");
        assert_eq!(settings.font_size, 15.0);
    }

    #[test]
    fn unknown_fields_and_missing_fields_are_tolerated() {
        let dir = temp_dir("partial");
        let path = dir.join("ui-settings.json");
        std::fs::write(&path, br#"{"font_size": 16.0, "future_field": 1}"#).unwrap();
        let settings = UiSettings::load_from(&path).unwrap();
        assert_eq!(settings.font_size, 16.0);
        assert_eq!(settings.line_height, DEFAULT_LINE_HEIGHT);
        // 没写过的字段取新用户默认：跟随系统（读不到时深色）、GitHub / 石墨、终端跟随皮肤，回滚 10000 行。
        assert!(settings.dark && settings.follow_system);
        assert_eq!(settings.skin_dark, "graphite");
        assert_eq!(settings.skin_light, "github-light");
        assert_eq!(settings.terminal_theme, "skin");
        assert_eq!(settings.scrollback, 10_000);
        assert_eq!(settings.font_family, DEFAULT_FONT_FAMILY);
        assert!(!settings.copy_on_select);
        assert!(!settings.right_click_paste, "右键默认弹菜单，不再直接粘贴");
        assert!(settings.confirm_multiline);
        assert!(settings.highlight_enabled);
        assert!(!settings.start_hidden);
        assert_eq!(settings.bracketed_paste, BracketedPaste::Auto);
    }

    #[test]
    fn system_theme_resolution() {
        assert!(!resolve_dark(true, false, Some(true)));
        assert!(resolve_dark(true, false, Some(false)));
        assert!(resolve_dark(true, true, None), "读不到系统值用已保存的回退");
        assert!(resolve_dark(false, true, Some(true)), "手动深色不听系统");
        assert!(!resolve_dark(false, false, Some(false)));
    }

    #[test]
    fn bracketed_paste_cycles_and_wraps() {
        assert_eq!(BracketedPaste::Auto.next(), BracketedPaste::On);
        assert_eq!(BracketedPaste::On.next(), BracketedPaste::Off);
        assert_eq!(BracketedPaste::Off.next(), BracketedPaste::Auto);

        assert!(BracketedPaste::Auto.wraps(true));
        assert!(!BracketedPaste::Auto.wraps(false));
        assert!(BracketedPaste::On.wraps(false));
        assert!(!BracketedPaste::Off.wraps(true));
    }

    #[test]
    fn the_new_install_defaults() {
        let s = UiSettings::default();
        assert!(s.follow_system, "新装默认跟随系统");
        assert!(s.dark, "读不到系统明暗时的回退是深色");
        assert_eq!(
            (s.skin_light.as_str(), s.skin_dark.as_str()),
            ("github-light", "graphite")
        );
        assert_eq!(s.terminal_theme, "skin", "终端配色默认跟随皮肤");
        assert_eq!(
            s.font_family,
            "JetBrains Mono, Cascadia Mono, Consolas, monospace"
        );
        assert_eq!((s.font_size, s.line_height), (12.0, 1.3));
        assert_eq!(s.scrollback, 10_000);
        assert!(s.highlight_enabled);
        // 默认值本身不能被 sanitized 改掉。
        assert_eq!(s.clone().sanitized(), s);
    }

    #[test]
    fn a_saved_choice_is_not_replaced_by_the_new_defaults() {
        // 老文件里存过浅色：升级后仍然是浅色，不会被新的「默认深色」改掉。
        let dir = temp_dir("keeps-saved-light");
        let path = dir.join("ui-settings.json");
        std::fs::write(
            &path,
            br#"{"dark": false, "font_size": 14.0, "line_height": 1.35}"#,
        )
        .unwrap();
        let settings = UiSettings::load_from(&path).unwrap().sanitized();
        assert!(!settings.dark);
        assert_eq!(settings.font_size, 14.0);
        assert_eq!(settings.line_height, 1.35);
    }

    #[test]
    fn a_saved_right_click_paste_choice_wins_over_the_new_default() {
        // V2-U25 把默认改成关：存过「开」的老用户仍是开，存过「关」的仍是关，只有从没写过这一项的才取新默认。
        let dir = temp_dir("keeps-saved-right-click");
        let path = dir.join("ui-settings.json");
        std::fs::write(&path, br#"{"right_click_paste": true}"#).unwrap();
        let on = UiSettings::load_from(&path).unwrap().sanitized();
        assert!(on.right_click_paste);
        std::fs::write(&path, br#"{"right_click_paste": false}"#).unwrap();
        let off = UiSettings::load_from(&path).unwrap().sanitized();
        assert!(!off.right_click_paste);
        std::fs::write(&path, br#"{"font_size": 14.0}"#).unwrap();
        let unset = UiSettings::load_from(&path).unwrap().sanitized();
        assert!(!unset.right_click_paste);
        // 打开后存盘再读回来，还是开。
        let saved = UiSettings {
            right_click_paste: true,
            ..Default::default()
        };
        saved.save_to(&path).unwrap();
        assert!(UiSettings::load_from(&path).unwrap().right_click_paste);
    }

    #[test]
    fn scrollback_is_bounded_to_the_0_2_14_range() {
        let low = UiSettings {
            scrollback: 3,
            ..Default::default()
        }
        .sanitized();
        assert_eq!(low.scrollback, MIN_SCROLLBACK);
        let high = UiSettings {
            scrollback: usize::MAX,
            ..Default::default()
        }
        .sanitized();
        assert_eq!(high.scrollback, MAX_SCROLLBACK);
        let ok = UiSettings {
            scrollback: 25_000,
            ..Default::default()
        }
        .sanitized();
        assert_eq!(ok.scrollback, 25_000);
        // 范围和同步校验一致（0.2.14 的 terminal_scrollback：1000～100000）。
        assert_eq!((MIN_SCROLLBACK, MAX_SCROLLBACK), (1_000, 100_000));
    }

    #[test]
    fn appearance_choices_that_do_not_exist_fall_back() {
        let settings = UiSettings {
            skin_light: "nope".to_owned(),
            skin_dark: "paper".to_owned(), // 浅色皮肤放进深色位
            terminal_theme: "system-github-light".to_owned(),
            font_family: "  ".to_owned(),
            ..Default::default()
        }
        .sanitized();
        assert_eq!(settings.skin_light, "github-light");
        assert_eq!(settings.skin_dark, "graphite");
        assert_eq!(settings.terminal_theme, "skin");
        assert_eq!(settings.font_family, DEFAULT_FONT_FAMILY);
        // 存在的选择原样保留。
        let kept = UiSettings {
            skin_light: "ayu-light".to_owned(),
            skin_dark: "kanagawa".to_owned(),
            terminal_theme: "gruvbox-dark".to_owned(),
            font_family: " Consolas ".to_owned(),
            ..Default::default()
        }
        .sanitized();
        assert_eq!(kept.skin_light, "ayu-light");
        assert_eq!(kept.skin_dark, "kanagawa");
        assert_eq!(kept.terminal_theme, "gruvbox-dark");
        assert_eq!(kept.font_family, "Consolas");
    }

    #[test]
    fn font_family_must_be_plain_and_short() {
        assert_eq!(
            sanitize_font_family("Fira Code, monospace"),
            "Fira Code, monospace"
        );
        assert_eq!(sanitize_font_family(""), DEFAULT_FONT_FAMILY);
        let bad_control = String::from("Consolas") + &char::from(7u8).to_string();
        assert_eq!(sanitize_font_family(&bad_control), DEFAULT_FONT_FAMILY);
        assert_eq!(sanitize_font_family(&"x".repeat(201)), DEFAULT_FONT_FAMILY);
        assert_eq!(sanitize_font_family(&"x".repeat(200)), "x".repeat(200));
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let settings = UiSettings {
            font_size: 99.0,
            line_height: 0.2,
            ..Default::default()
        }
        .sanitized();
        assert_eq!(settings.font_size, MAX_FONT_SIZE);
        assert_eq!(settings.line_height, MIN_LINE_HEIGHT);
        // 0.2.14 的行高上限是 1.6、字号下限 10。
        let wide = UiSettings {
            font_size: 6.0,
            line_height: 2.0,
            ..Default::default()
        }
        .sanitized();
        assert_eq!(wide.font_size, MIN_FONT_SIZE);
        assert_eq!(wide.line_height, MAX_LINE_HEIGHT);

        let settings = UiSettings {
            font_size: f32::NAN,
            line_height: f32::INFINITY,
            ..Default::default()
        }
        .sanitized();
        assert_eq!(settings.font_size, DEFAULT_FONT_SIZE);
        assert_eq!(settings.line_height, DEFAULT_LINE_HEIGHT);
    }

    #[test]
    fn save_creates_parent_directory() {
        let dir = temp_dir("nested");
        let path = dir.join("a").join("b").join("ui-settings.json");
        UiSettings::default().save_to(&path).unwrap();
        assert!(path.exists());
        assert!(!path.with_extension("json.tmp").exists());
    }
}
