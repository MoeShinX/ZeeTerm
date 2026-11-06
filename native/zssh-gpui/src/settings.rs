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
    Block,
    /// 新装默认（V2-U32）；已经存过设置的用户保持自己存下的值。
    #[default]
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

/// 括号粘贴三态在设置页上的名字。
#[derive(Clone, Copy, Debug)]
enum BracketedMsg {
    Auto,
    On,
    Off,
}

impl crate::i18n::Msg for BracketedMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::Auto => "自动",
            Self::On => "开",
            Self::Off => "关",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::On => "On",
            Self::Off => "Off",
        }
    }
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
            Self::Auto => crate::i18n::t(BracketedMsg::Auto),
            Self::On => crate::i18n::t(BracketedMsg::On),
            Self::Off => crate::i18n::t(BracketedMsg::Off),
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

/// 文件面板默认开在哪（「设置 → 文件传输 → 文件面板 → 打开位置」）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SftpOpenIn {
    /// 终端右侧并排（停靠面板）。
    #[default]
    Dock,
    /// 主窗口里的新标签页。
    Tab,
    /// 独立的文件窗口。
    Window,
}

/// 文件视图里本地栏和远端栏怎么排。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SftpLocalLayout {
    /// 视图够宽左右排，否则上下排。
    #[default]
    Auto,
    /// 左边本地、右边远端。
    Side,
    /// 上面本地、下面远端。
    Stacked,
}

/// 新开文件视图时，本地栏从哪个目录开始。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SftpLocalDir {
    /// Windows 的「下载」文件夹。
    #[default]
    Downloads,
    /// 这台主机上次用到的本地目录。
    Last,
    /// 用户自己选的目录（[`UiSettings::sftp_local_dir_custom`]）。
    Custom,
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
    /// 界面语言偏好：`system` / `zh` / `en`。没选过（全新安装、老设置文件里没有这个字段）
    /// 一律跟随系统：系统主语言是中文（简体、繁体都算）用中文，其他语言一律英文。
    pub language: String,
    /// 「文件传输 → 文件面板」：快捷键 / 标题栏「文件」默认把文件视图开在哪。
    pub sftp_open_in: SftpOpenIn,
    /// 新开的文件视图显示本地栏吗（视图里的「本地」按钮只管当前这一个视图，不改这里）。
    pub sftp_show_local: bool,
    /// 本地栏和远端栏的排列。
    pub sftp_local_layout: SftpLocalLayout,
    /// 本地栏的起始目录来源。
    pub sftp_local_dir: SftpLocalDir,
    /// [`SftpLocalDir::Custom`] 用的目录；空 = 还没选。
    pub sftp_local_dir_custom: String,
    /// 新开的文件视图默认「跟随终端目录」。
    pub sftp_follow_cwd: bool,
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
            cursor: CursorShape::default(),
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
            language: "system".to_owned(),
            sftp_open_in: SftpOpenIn::Dock,
            sftp_show_local: true,
            sftp_local_layout: SftpLocalLayout::Auto,
            sftp_local_dir: SftpLocalDir::Downloads,
            sftp_local_dir_custom: String::new(),
            sftp_follow_cwd: false,
        }
    }
}

impl UiSettings {
    /// 当前的界面语言偏好。
    pub fn language_pref(&self) -> crate::i18n::LangPref {
        crate::i18n::LangPref::parse(&self.language)
    }

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

    /// 「外观」页「恢复本页默认设置」负责的字段：明暗模式、跟随系统、浅色 / 深色皮肤、终端配色。
    /// 逐个字段显式写出来（不用整份 `Self::default()` 覆盖），别的页的值、高亮规则一律不碰；
    /// 以后给 [`UiSettings`] 加了字段，要在这里或 [`Self::reset_terminal_page`] 里认领，
    /// 测试 `every_field_has_an_owner_or_is_deliberately_left_alone` 会逼着你做这个决定。
    pub fn reset_appearance_page(&mut self) {
        let d = Self::default();
        self.dark = d.dark;
        self.follow_system = d.follow_system;
        self.skin_light = d.skin_light;
        self.skin_dark = d.skin_dark;
        self.terminal_theme = d.terminal_theme;
    }

    /// 「终端」页「恢复本页默认设置」负责的字段：字体、字号、行高、回滚行数、延迟徽标、光标、
    /// 输入与剪贴板、括号粘贴、高亮总开关。高亮规则是用户自己的数据，不在其中。
    pub fn reset_terminal_page(&mut self) {
        let d = Self::default();
        self.font_family = d.font_family;
        self.font_size = d.font_size;
        self.line_height = d.line_height;
        self.scrollback = d.scrollback;
        self.show_latency = d.show_latency;
        self.cursor = d.cursor;
        self.cursor_blink = d.cursor_blink;
        self.ime_english = d.ime_english;
        self.copy_on_select = d.copy_on_select;
        self.right_click_paste = d.right_click_paste;
        self.confirm_multiline = d.confirm_multiline;
        self.bracketed_paste = d.bracketed_paste;
        self.highlight_enabled = d.highlight_enabled;
    }

    /// 「文件传输」页「恢复本页默认设置」负责的字段：只有存在 `ui-settings.json` 里的「文件面板」六项。
    /// 同时传输数、保留修改时间 / 权限、默认显示隐藏文件存在数据库设置表里，不在这里。
    /// 「上次用过的」本地目录是各主机的使用记录，也不算设置，不清。
    pub fn reset_sftp_page(&mut self) {
        let d = Self::default();
        self.sftp_open_in = d.sftp_open_in;
        self.sftp_show_local = d.sftp_show_local;
        self.sftp_local_layout = d.sftp_local_layout;
        self.sftp_local_dir = d.sftp_local_dir;
        self.sftp_local_dir_custom = d.sftp_local_dir_custom;
        self.sftp_follow_cwd = d.sftp_follow_cwd;
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

/// `ZSSH_DATA_DIR` 优先；否则按平台放正式资料目录：
/// - Windows：`%APPDATA%\ZeeTerm`（环境变量被清掉时问系统「漫游应用数据」已知文件夹）；
/// - macOS：`~/Library/Application Support/ZeeTerm`；
/// - 其他 Unix：`$XDG_DATA_HOME/ZeeTerm`，没设时 `~/.local/share/ZeeTerm`。
///
/// 一个都拿不到就**不再退回临时目录**——临时目录随时会被清理，凭据库、主机库放进去等于
/// 悄悄丢数据。启动时 [`ensure_data_dir_or_exit`] 已经把这种情况拦下来并提示退出；
/// 万一别的地方先走到这里，也一样提示后退出。
pub fn data_dir() -> PathBuf {
    try_data_dir().unwrap_or_else(|| exit_without_data_dir())
}

/// 同 [`data_dir`]，拿不到时返回 `None`。
pub fn try_data_dir() -> Option<PathBuf> {
    resolve_data_dir(std::env::var_os("ZSSH_DATA_DIR"), platform_data_base())
}

/// 启动第一件事：确认有正式的数据目录，没有就弹框说明后退出（不建窗口、不写任何文件）。
pub fn ensure_data_dir_or_exit() {
    if try_data_dir().is_none() {
        exit_without_data_dir();
    }
}

fn exit_without_data_dir() -> ! {
    tracing::error!("找不到数据目录：系统没有可用的应用数据位置");
    show_fatal(no_data_dir_message());
    std::process::exit(1);
}

/// 这时设置还没读，界面语言按系统语言挑：中文系统中文，其他英文。
fn no_data_dir_message() -> &'static str {
    if zssh_platform_windows::locale::system_is_chinese() {
        #[cfg(windows)]
        const MESSAGE: &str = "ZeeTerm 找不到保存数据的位置（%APPDATA% 和系统的「应用数据」文件夹都不可用），为避免把凭据和主机写进会被清理的临时目录，程序已退出。\n\n请从开始菜单或桌面快捷方式正常启动；如果需要指定位置，可以设置环境变量 ZSSH_DATA_DIR。"; // i18n-allow: 设置还没读、按系统语言二选一，启动前的致命提示
        #[cfg(target_os = "macos")]
        const MESSAGE: &str = "ZeeTerm 找不到保存数据的位置（~/Library/Application Support 不可用），为避免把凭据和主机写进会被清理的临时目录，程序已退出。\n\n请确认当前用户的主目录可用；如果需要指定位置，可以设置环境变量 ZSSH_DATA_DIR。"; // i18n-allow: 设置还没读、按系统语言二选一，启动前的致命提示
        #[cfg(not(any(windows, target_os = "macos")))]
        const MESSAGE: &str = "ZeeTerm 找不到保存数据的位置，为避免把凭据和主机写进会被清理的临时目录，程序已退出。\n\n如果需要指定位置，可以设置环境变量 ZSSH_DATA_DIR。"; // i18n-allow: 设置还没读、按系统语言二选一，启动前的致命提示
        MESSAGE
    } else {
        #[cfg(windows)]
        const MESSAGE: &str = "ZeeTerm cannot find a place to keep its data (neither %APPDATA% nor the system Application Data folder is available). To avoid putting credentials and hosts in a temporary folder that may be cleaned up, ZeeTerm has exited.\n\nStart it normally from the Start menu or a desktop shortcut, or set the ZSSH_DATA_DIR environment variable to choose a location.";
        #[cfg(target_os = "macos")]
        const MESSAGE: &str = "ZeeTerm cannot find a place to keep its data (~/Library/Application Support is unavailable). To avoid putting credentials and hosts in a temporary folder that may be cleaned up, ZeeTerm has exited.\n\nCheck that your home folder is available, or set the ZSSH_DATA_DIR environment variable to choose a location.";
        #[cfg(not(any(windows, target_os = "macos")))]
        const MESSAGE: &str = "ZeeTerm cannot find a place to keep its data. To avoid putting credentials and hosts in a temporary folder that may be cleaned up, ZeeTerm has exited.\n\nSet the ZSSH_DATA_DIR environment variable to choose a location.";
        MESSAGE
    }
}

#[cfg(windows)]
fn show_fatal(text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use windows::core::HSTRING;
    // SAFETY: 两个字符串在调用期间都活着；没有父窗口。
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(text),
            &HSTRING::from(crate::product::NAME),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(not(windows))]
fn show_fatal(text: &str) {
    eprintln!("{text}");
}

#[cfg(target_os = "macos")]
enum AlreadyRunningMsg {
    Message,
}

#[cfg(target_os = "macos")]
impl crate::i18n::Msg for AlreadyRunningMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::Message => {
                "ZeeTerm 已经在运行。请在程序坞里点它的图标切回去；再次双击不会打开新的窗口。"
            }
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::Message => {
                "ZeeTerm is already running. Click its icon in the Dock to switch to it; opening it again does not create another window."
            }
        }
    }
}

/// 第二个实例启动时提示”已经在运行”。
///
/// macOS 上还不能把已有窗口带到前面，无声退出会让用户以为双击没有反应。
/// 这里用系统对话框说一声；对话框起不来时至少写到 stderr。
#[cfg(target_os = "macos")]
pub fn notify_already_running() {
    let text = crate::i18n::t(AlreadyRunningMsg::Message);
    // osascript 是系统自带的，不依赖本程序有没有签名。失败也不影响退出。
    let _ = std::process::Command::new("/usr/bin/osascript")
        .args([
            "-e",
            &format!("display alert \"ZeeTerm\" message \"{text}\""),
        ])
        .status();
    eprintln!("{text}");
}

#[cfg(not(target_os = "macos"))]
pub fn notify_already_running() {}

/// 系统给的“应用数据”基准目录（还不是 ZeeTerm 自己的目录）。
#[cfg(windows)]
fn platform_data_base() -> Option<PathBuf> {
    let usable = |dir: PathBuf| dir.is_absolute().then_some(dir);
    std::env::var_os("APPDATA")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .and_then(usable)
        .or_else(|| known_roaming_appdata().and_then(usable))
}

/// macOS 的 `~/Library/Application Support`（用 HOME，不猜别的路径）。
#[cfg(target_os = "macos")]
fn platform_data_base() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").filter(|dir| !dir.is_empty())?);
    home.is_absolute()
        .then(|| home.join("Library").join("Application Support"))
}

/// 其他 Unix 的 XDG data 目录；没有 `XDG_DATA_HOME` 时用 `~/.local/share`。
#[cfg(not(any(windows, target_os = "macos")))]
fn platform_data_base() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME").filter(|dir| !dir.is_empty()) {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return Some(dir);
        }
    }
    let home = PathBuf::from(std::env::var_os("HOME").filter(|dir| !dir.is_empty())?);
    home.is_absolute()
        .then(|| home.join(".local").join("share"))
}

/// Windows 的「漫游应用数据」已知文件夹（就是平时 `%APPDATA%` 指向的地方）。
#[cfg(windows)]
fn known_roaming_appdata() -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{
        FOLDERID_RoamingAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
    };
    // SAFETY: 返回的字符串由 shell 分配，复制出来之后立刻用 `CoTaskMemFree` 释放。
    unsafe {
        let raw = SHGetKnownFolderPath(&FOLDERID_RoamingAppData, KF_FLAG_DEFAULT, None).ok()?;
        let text = raw.to_string().ok();
        CoTaskMemFree(Some(raw.0 as *const _));
        text.filter(|text| !text.is_empty()).map(PathBuf::from)
    }
}

/// 选数据目录。空字符串当作没设（空的 `ZSSH_DATA_DIR` 拼出来是相对路径）；基准目录必须是绝对路径。
fn resolve_data_dir(
    override_dir: Option<std::ffi::OsString>,
    base: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(dir) = override_dir.filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    base.filter(|dir| dir.is_absolute())
        .map(|base| base.join(crate::product::DATA_DIR_NAME))
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

    /// 每个字段都不是默认值的一份设置。故意不写 `..Default::default()`：以后给 `UiSettings` 加字段，
    /// 这里编译不过，逼着加字段的人决定它归哪一页重置（或明确不重置）。
    fn all_changed() -> UiSettings {
        UiSettings {
            dark: false,
            follow_system: false,
            skin_light: "x-light".to_owned(),
            skin_dark: "x-dark".to_owned(),
            terminal_theme: "x-term".to_owned(),
            font_family: "X Mono".to_owned(),
            font_size: 20.0,
            line_height: 1.5,
            scrollback: 12345,
            cursor: CursorShape::Bar,
            cursor_blink: false,
            ime_english: false,
            copy_on_select: true,
            right_click_paste: true,
            confirm_multiline: false,
            highlight_enabled: false,
            show_latency: false,
            start_hidden: true,
            highlight_rules: Some(Vec::new()),
            bracketed_paste: BracketedPaste::On,
            language: "en".to_owned(),
            sftp_open_in: SftpOpenIn::Window,
            sftp_show_local: false,
            sftp_local_layout: SftpLocalLayout::Stacked,
            sftp_local_dir: SftpLocalDir::Custom,
            sftp_local_dir_custom: "D:/x".to_owned(),
            sftp_follow_cwd: true,
        }
    }

    #[test]
    fn resetting_the_appearance_page_only_touches_appearance_fields() {
        let mut s = all_changed();
        s.reset_appearance_page();
        let d = UiSettings::default();
        let mut expected = all_changed();
        expected.dark = d.dark;
        expected.follow_system = d.follow_system;
        expected.skin_light = d.skin_light;
        expected.skin_dark = d.skin_dark;
        expected.terminal_theme = d.terminal_theme;
        assert_eq!(s, expected);
        // 语言、终端字体、启动行为、高亮规则都还是改过的值。
        assert_eq!(s.language, "en");
        assert_eq!(s.font_family, "X Mono");
        assert!(s.start_hidden);
        assert!(s.highlight_rules.is_some());
    }

    #[test]
    fn resetting_the_terminal_page_only_touches_terminal_fields() {
        let mut s = all_changed();
        s.reset_terminal_page();
        let d = UiSettings::default();
        let mut expected = all_changed();
        expected.font_family = d.font_family;
        expected.font_size = d.font_size;
        expected.line_height = d.line_height;
        expected.scrollback = d.scrollback;
        expected.show_latency = d.show_latency;
        expected.cursor = d.cursor;
        expected.cursor_blink = d.cursor_blink;
        expected.ime_english = d.ime_english;
        expected.copy_on_select = d.copy_on_select;
        expected.right_click_paste = d.right_click_paste;
        expected.confirm_multiline = d.confirm_multiline;
        expected.bracketed_paste = d.bracketed_paste;
        expected.highlight_enabled = d.highlight_enabled;
        assert_eq!(s, expected);
        // 主题 / 皮肤 / 终端配色（外观页的）、语言、启动行为、高亮规则都没动。
        assert!(!s.dark && !s.follow_system);
        assert_eq!(s.terminal_theme, "x-term");
        assert_eq!(s.language, "en");
        assert!(s.start_hidden);
        assert!(s.highlight_rules.is_some());
    }

    #[test]
    fn every_field_has_an_owner_or_is_deliberately_left_alone() {
        // 三页都重置一遍之后，和默认值还不一样的字段，必须正是「故意不重置」的那三个：
        // 语言和启动时隐藏（常规页，没有重置按钮）、高亮规则（用户自己的数据）。
        let mut s = all_changed();
        s.reset_appearance_page();
        s.reset_terminal_page();
        s.reset_sftp_page();
        let d = UiSettings::default();
        let mut left = Vec::new();
        if s.language != d.language {
            left.push("language");
        }
        if s.start_hidden != d.start_hidden {
            left.push("start_hidden");
        }
        if s.highlight_rules != d.highlight_rules {
            left.push("highlight_rules");
        }
        assert_eq!(left, ["language", "start_hidden", "highlight_rules"]);
        let mut rest = s.clone();
        rest.language = d.language.clone();
        rest.start_hidden = d.start_hidden;
        rest.highlight_rules = d.highlight_rules.clone();
        assert_eq!(rest, d, "其余字段两页合起来必须全部回到默认");
    }

    #[test]
    fn resetting_the_sftp_page_only_touches_the_file_panel_fields() {
        let mut s = all_changed();
        s.reset_sftp_page();
        let d = UiSettings::default();
        let mut expected = all_changed();
        expected.sftp_open_in = d.sftp_open_in;
        expected.sftp_show_local = d.sftp_show_local;
        expected.sftp_local_layout = d.sftp_local_layout;
        expected.sftp_local_dir = d.sftp_local_dir;
        expected.sftp_local_dir_custom = d.sftp_local_dir_custom;
        expected.sftp_follow_cwd = d.sftp_follow_cwd;
        assert_eq!(s, expected);
        // 外观 / 终端 / 语言 / 启动行为 / 高亮规则都还是改过的值。
        assert!(!s.dark && !s.follow_system);
        assert_eq!(s.font_family, "X Mono");
        assert_eq!(s.cursor, CursorShape::Bar);
        assert_eq!(s.language, "en");
        assert!(s.start_hidden);
        assert!(s.highlight_rules.is_some());
        // 另外两页的重置也不碰文件面板的字段。
        let mut other = all_changed();
        other.reset_appearance_page();
        other.reset_terminal_page();
        assert_eq!(other.sftp_open_in, SftpOpenIn::Window);
        assert_eq!(other.sftp_local_dir_custom, "D:/x");
    }

    #[test]
    fn the_file_panel_defaults_match_the_card_and_old_files_get_them() {
        let d = UiSettings::default();
        assert_eq!(d.sftp_open_in, SftpOpenIn::Dock);
        assert!(d.sftp_show_local);
        assert_eq!(d.sftp_local_layout, SftpLocalLayout::Auto);
        assert_eq!(d.sftp_local_dir, SftpLocalDir::Downloads);
        assert_eq!(d.sftp_local_dir_custom, "");
        assert!(!d.sftp_follow_cwd);
        // 旧设置文件没有这六个字段：照常读，取默认值，别的字段不受影响。
        let old: UiSettings =
            serde_json::from_str(r#"{"font_size": 15.0, "cursor": "block"}"#).expect("旧文件");
        assert_eq!(old.sftp_open_in, SftpOpenIn::Dock);
        assert!(old.sftp_show_local);
        assert_eq!(old.sftp_local_layout, SftpLocalLayout::Auto);
        assert_eq!(old.sftp_local_dir, SftpLocalDir::Downloads);
        assert!(!old.sftp_follow_cwd);
        assert_eq!(old.cursor, CursorShape::Block);
        // 存下来的写法固定（小写单词），以后改名会弄坏已有的设置文件。
        let json = serde_json::to_string(&all_changed()).unwrap();
        for word in [
            "\"sftp_open_in\":\"window\"",
            "\"sftp_local_layout\":\"stacked\"",
            "\"sftp_local_dir\":\"custom\"",
        ] {
            assert!(json.contains(word), "{word} 不在 {json}");
        }
    }

    /// 没选过语言（全新安装、老设置文件里没有这个字段）一律跟随系统；选过的照旧。
    #[test]
    fn language_follows_the_system_unless_chosen() {
        assert_eq!(
            UiSettings::default().language_pref(),
            crate::i18n::LangPref::System
        );
        let old: UiSettings = serde_json::from_str(r#"{"dark":true}"#).expect("老设置文件");
        assert_eq!(old.language_pref(), crate::i18n::LangPref::System);
        let chosen: UiSettings = serde_json::from_str(r#"{"language":"zh"}"#).expect("选过中文");
        assert_eq!(chosen.language_pref(), crate::i18n::LangPref::Zh);
        // 非中文系统一律英文。
        assert_eq!(
            crate::i18n::LangPref::System.resolve(false),
            crate::i18n::Lang::En
        );
        assert_eq!(
            crate::i18n::LangPref::System.resolve(true),
            crate::i18n::Lang::Zh
        );
    }

    #[test]
    fn the_default_data_dir_is_zeeterm_under_the_platform_base() {
        let base = std::env::temp_dir().join("zssh-settings-base");
        assert_eq!(
            resolve_data_dir(None, Some(base.clone())),
            Some(base.join("ZeeTerm"))
        );
        // 环境变量优先；也不再落到 Tauri 版 Z-SSH 的目录下。
        assert_eq!(
            resolve_data_dir(Some(r"D:\isolated".into()), Some(base.clone())),
            Some(PathBuf::from(r"D:\isolated"))
        );
        let default = resolve_data_dir(None, Some(base)).unwrap();
        assert!(!default.to_string_lossy().contains("Z-SSH"));
    }

    #[test]
    fn a_missing_or_relative_base_never_falls_back_to_temp() {
        // 基准目录拿不到、是空的或相对路径：没有数据目录，绝不退回临时目录。
        assert_eq!(resolve_data_dir(None, None), None);
        assert_eq!(
            resolve_data_dir(None, Some(PathBuf::from("relative"))),
            None
        );
        // 空的 ZSSH_DATA_DIR 当作没设。
        assert_eq!(resolve_data_dir(Some("".into()), None), None);
        // 非空 ZSSH_DATA_DIR 按用户给的路径用（这是产品功能，允许自定义位置）。
        assert_eq!(
            resolve_data_dir(Some(r"D:\isolated".into()), None),
            Some(PathBuf::from(r"D:\isolated"))
        );
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
            language: "en".to_owned(),
            sftp_open_in: SftpOpenIn::Tab,
            sftp_show_local: false,
            sftp_local_layout: SftpLocalLayout::Side,
            sftp_local_dir: SftpLocalDir::Custom,
            sftp_local_dir_custom: "D:/work".to_owned(),
            sftp_follow_cwd: true,
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
    fn a_stored_cursor_shape_is_kept_and_a_missing_one_gets_the_new_default() {
        let dir = temp_dir("cursor-default");
        let path = dir.join("ui-settings.json");
        std::fs::write(&path, br#"{"font_size": 15.0, "cursor": "block"}"#).unwrap();
        let kept = UiSettings::load_from(&path).expect("旧文件照常读");
        assert_eq!(kept.cursor, CursorShape::Block, "存过方块的用户不被改");
        std::fs::write(&path, br#"{"font_size": 15.0}"#).unwrap();
        let fresh = UiSettings::load_from(&path).expect("旧文件照常读");
        assert_eq!(
            fresh.cursor,
            CursorShape::Underline,
            "没存过光标样式的用下划线"
        );
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
        assert_eq!(s.cursor, CursorShape::Underline, "光标默认下划线");
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
