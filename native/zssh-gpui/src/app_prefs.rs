//! 设置页「连接」「SFTP」「外观里的窗口」这几组设置：存在数据库的 `settings` 表里，键名、取值范围、
//! 默认值都和 0.2.14 一致（`stores/settings.ts`），所以同一个库在两个版本之间互相读写不会错位。
//!
//! 值在表里是 JSON 文本（数字 `30`、布尔 `true`、字符串 `"tray"`）；读的时候容忍没带引号的老值、
//! 超范围的数、读不懂的类型，一律回到默认或夹进范围，不崩、不猜。

use zssh_core::store::Database;
use zssh_core::store::settings::{KEEPALIVE_INTERVAL_KEY, POOL_IDLE_TIMEOUT_KEY};

pub const STATS_SHOW_KEY: &str = "stats_show";
pub const STATS_REFRESH_KEY: &str = "stats_refresh_secs";
pub const SFTP_CONCURRENCY_KEY: &str = "sftp_concurrency";
pub const SFTP_PRESERVE_MTIME_KEY: &str = "sftp_preserve_mtime";
pub const SFTP_PRESERVE_PERMISSIONS_KEY: &str = "sftp_preserve_permissions";
pub const SFTP_SHOW_HIDDEN_KEY: &str = "sftp_show_hidden";
pub const CLOSE_ACTION_KEY: &str = "close_action";
pub const FILE_WINDOW_CLOSE_KEY: &str = "file_window_close";
/// 会话日志：开关（默认关）和目录（空 = 数据目录下的 `session-logs`），都只在本机。
pub const SESSION_LOG_ENABLED_KEY: &str = "session_log_enabled";
pub const SESSION_LOG_DIR_KEY: &str = "session_log_dir";

/// 保活间隔（秒，0 = 关闭）：滑块 0～300，步长 5。
pub const KEEPALIVE_RANGE: (u32, u32) = (0, 300);
pub const KEEPALIVE_STEP: u32 = 5;
pub const KEEPALIVE_DEFAULT: u32 = 30;
/// 连接空闲断开（秒，0 = 立刻）：滑块 0～3600，步长 10。
pub const POOL_IDLE_RANGE: (u32, u32) = (0, 3600);
pub const POOL_IDLE_STEP: u32 = 10;
pub const POOL_IDLE_DEFAULT: u32 = 60;
/// 状态栏刷新间隔（秒）：1～30。
pub const STATS_REFRESH_RANGE: (u32, u32) = (1, 30);
pub const STATS_REFRESH_DEFAULT: u32 = 3;
/// SFTP 同时传输的文件数：1～8。
pub const SFTP_CONCURRENCY_RANGE: (u32, u32) = (1, 8);
pub const SFTP_CONCURRENCY_DEFAULT: u32 = 3;

/// 关闭主窗口时：缩到托盘（默认）或退出程序。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CloseChoice {
    #[default]
    Tray,
    Quit,
}

impl CloseChoice {
    pub fn id(self) -> &'static str {
        match self {
            Self::Tray => "tray",
            Self::Quit => "quit",
        }
    }

    /// 只有明确是 `quit` 才退出，缺省或乱七八糟的值一律按默认（缩到托盘），同 0.2.14 的 `parseCloseAction`。
    pub fn parse(raw: Option<&str>) -> Self {
        match text_of(raw).as_deref() {
            Some("quit") => Self::Quit,
            _ => Self::Tray,
        }
    }
}

/// 关闭文件窗口时：断开并关闭（默认）或合并回主窗口。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FileWindowChoice {
    #[default]
    Close,
    Merge,
}

impl FileWindowChoice {
    pub fn id(self) -> &'static str {
        match self {
            Self::Close => "close",
            Self::Merge => "merge",
        }
    }

    pub fn parse(raw: Option<&str>) -> Self {
        match text_of(raw).as_deref() {
            Some("merge") => Self::Merge,
            _ => Self::Close,
        }
    }
}

/// 设置表里的字符串值：`"merge"` 或没带引号的 `merge` 都认。
pub fn text_of(raw: Option<&str>) -> Option<String> {
    crate::main_window::page_logic::setting_text(raw)
}

fn number(raw: Option<&str>, range: (u32, u32), default: u32) -> u32 {
    let parsed = raw
        .map(str::trim)
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|value| match value {
            serde_json::Value::Number(n) => n.as_f64(),
            // 老值有人存成 "30"。
            serde_json::Value::String(text) => text.trim().parse::<f64>().ok(),
            _ => None,
        })
        .filter(|n| n.is_finite());
    match parsed {
        Some(n) => (n.round().clamp(range.0 as f64, range.1 as f64)) as u32,
        None => default,
    }
}

fn flag(raw: Option<&str>, default: bool) -> bool {
    crate::main_window::bar_settings::parse_bool(raw, default)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppPrefs {
    pub keepalive_secs: u32,
    pub pool_idle_secs: u32,
    pub stats_show: bool,
    pub stats_refresh_secs: u32,
    pub sftp_concurrency: u32,
    pub sftp_preserve_mtime: bool,
    pub sftp_preserve_permissions: bool,
    pub sftp_show_hidden: bool,
    pub close_action: CloseChoice,
    pub file_window_close: FileWindowChoice,
    pub session_log_enabled: bool,
    /// 设置里填的会话日志目录（已去掉首尾空白，可能为空）。
    pub session_log_dir: String,
}

impl Default for AppPrefs {
    fn default() -> Self {
        Self::from_lookup(|_| None)
    }
}

impl AppPrefs {
    /// 按键取原始值（表里没有就是 `None`）造一份；测试用它不开库。
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let raw = |key: &str| get(key);
        Self {
            keepalive_secs: number(
                raw(KEEPALIVE_INTERVAL_KEY).as_deref(),
                KEEPALIVE_RANGE,
                KEEPALIVE_DEFAULT,
            ),
            pool_idle_secs: number(
                raw(POOL_IDLE_TIMEOUT_KEY).as_deref(),
                POOL_IDLE_RANGE,
                POOL_IDLE_DEFAULT,
            ),
            stats_show: flag(raw(STATS_SHOW_KEY).as_deref(), true),
            stats_refresh_secs: number(
                raw(STATS_REFRESH_KEY).as_deref(),
                STATS_REFRESH_RANGE,
                STATS_REFRESH_DEFAULT,
            ),
            sftp_concurrency: number(
                raw(SFTP_CONCURRENCY_KEY).as_deref(),
                SFTP_CONCURRENCY_RANGE,
                SFTP_CONCURRENCY_DEFAULT,
            ),
            sftp_preserve_mtime: flag(raw(SFTP_PRESERVE_MTIME_KEY).as_deref(), false),
            sftp_preserve_permissions: flag(raw(SFTP_PRESERVE_PERMISSIONS_KEY).as_deref(), false),
            sftp_show_hidden: flag(raw(SFTP_SHOW_HIDDEN_KEY).as_deref(), false),
            close_action: CloseChoice::parse(raw(CLOSE_ACTION_KEY).as_deref()),
            file_window_close: FileWindowChoice::parse(raw(FILE_WINDOW_CLOSE_KEY).as_deref()),
            session_log_enabled: flag(raw(SESSION_LOG_ENABLED_KEY).as_deref(), false),
            session_log_dir: text_of(raw(SESSION_LOG_DIR_KEY).as_deref())
                .map(|dir| dir.trim().to_owned())
                .filter(|dir| !dir.chars().any(char::is_control))
                .unwrap_or_default(),
        }
    }

    /// 会话日志实际写到哪个目录：设置里填了就用它，没填用数据目录下的 `session-logs`（同 0.2.14）。
    pub fn session_log_dir_path(&self) -> std::path::PathBuf {
        if self.session_log_dir.is_empty() {
            crate::settings::data_dir().join("session-logs")
        } else {
            std::path::PathBuf::from(&self.session_log_dir)
        }
    }

    pub fn load(db: &Database) -> Self {
        Self::from_lookup(|key| db.setting_get(key).ok().flatten())
    }

    /// 打开数据目录里的库读一份；库打不开就全部用默认值。
    pub fn load_default_db() -> Self {
        match Database::open(&crate::settings::data_dir().join("zssh.db")) {
            Ok(db) => Self::load(&db),
            Err(_) => Self::default(),
        }
    }

    /// SFTP 传输引擎的参数：同时传输数、保留修改时间 / 权限（对之后打开的 SFTP 标签生效，
    /// 同 0.2.14 设置页的说明）。
    pub fn transfer_options(&self) -> zssh_core::sftp::transfer::TransferOptions {
        zssh_core::sftp::transfer::TransferOptions {
            concurrency: self.sftp_concurrency as usize,
            preserve_mtime: self.sftp_preserve_mtime,
            preserve_permissions: self.sftp_preserve_permissions,
            ..Default::default()
        }
    }
}

/// 开了会话日志就为这个会话建一个日志文件（纯文本，去掉颜色和光标控制）；没开、目录建不了
/// （没权限）都返回 `None`，会话照常。每个会话一个文件，只对新打开的会话生效。
pub fn open_session_log(label: &str) -> Option<zssh_core::ssh::session_log::SessionLog> {
    open_session_log_with(&AppPrefs::load_default_db(), label)
}

/// 同 [`open_session_log`]，设置由调用方给（测试用）。
pub fn open_session_log_with(
    prefs: &AppPrefs,
    label: &str,
) -> Option<zssh_core::ssh::session_log::SessionLog> {
    if !prefs.session_log_enabled {
        return None;
    }
    let dir = prefs.session_log_dir_path();
    match zssh_core::ssh::session_log::SessionLog::create(
        &dir,
        label,
        &zssh_core::ssh::session_log::local_stamp(),
    ) {
        Ok(log) => Some(log),
        Err(err) => {
            // 目录路径是用户填的本机路径，日志里只写原因，不写路径。
            tracing::warn!(%err, "建不了会话日志文件，这个会话不记日志");
            None
        }
    }
}

/// 写一个整数设置（夹进范围再写）。
pub fn write_number(db: &Database, key: &str, value: u32, range: (u32, u32)) -> Result<(), String> {
    db.setting_set(key, &value.clamp(range.0, range.1).to_string())
        .map_err(|err| err.to_string())
}

pub fn write_flag(db: &Database, key: &str, value: bool) -> Result<(), String> {
    crate::main_window::bar_settings::write_bool(db, key, value)
}

pub fn write_text(db: &Database, key: &str, value: &str) -> Result<(), String> {
    crate::main_window::bar_settings::write_text(db, key, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn prefs(pairs: &[(&str, &str)]) -> AppPrefs {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        AppPrefs::from_lookup(|key| map.get(key).cloned())
    }

    #[test]
    fn defaults_are_the_0_2_14_ones() {
        let p = AppPrefs::default();
        assert_eq!(p.keepalive_secs, 30);
        assert_eq!(p.pool_idle_secs, 60);
        assert!(p.stats_show);
        assert_eq!(p.stats_refresh_secs, 3);
        assert_eq!(p.sftp_concurrency, 3);
        assert!(!p.sftp_preserve_mtime);
        assert!(!p.sftp_preserve_permissions);
        assert!(!p.sftp_show_hidden);
        assert_eq!(p.close_action, CloseChoice::Tray);
        assert_eq!(p.file_window_close, FileWindowChoice::Close);
        assert!(!p.session_log_enabled, "会话日志默认关");
        assert_eq!(p.session_log_dir, "");
    }

    #[test]
    fn the_session_log_dir_falls_back_to_the_data_dir_and_ignores_garbage() {
        let p = prefs(&[
            ("session_log_enabled", "true"),
            ("session_log_dir", r#""  D:\\logs  ""#),
        ]);
        assert!(p.session_log_enabled);
        assert_eq!(p.session_log_dir, r"D:\logs");
        assert_eq!(
            p.session_log_dir_path(),
            std::path::PathBuf::from(r"D:\logs")
        );
        // 没填：数据目录下的 session-logs。
        let default = AppPrefs::default().session_log_dir_path();
        assert_eq!(
            default.file_name().and_then(|n| n.to_str()),
            Some("session-logs")
        );
        // 带控制字符的目录当没填。
        let bad = prefs(&[("session_log_dir", r#""D:\\logs\u0007""#)]);
        assert_eq!(bad.session_log_dir, "");
    }

    #[test]
    fn stored_values_are_read_and_out_of_range_ones_are_clamped() {
        let p = prefs(&[
            ("keepalive_interval_secs", "0"),
            ("pool_idle_timeout_secs", "99999"),
            ("stats_show", "false"),
            ("stats_refresh_secs", "0"),
            ("sftp_concurrency", "20"),
            ("sftp_preserve_mtime", "true"),
            ("close_action", "\"quit\""),
            ("file_window_close", "\"merge\""),
        ]);
        assert_eq!(p.keepalive_secs, 0, "0 = 关闭保活，是合法值");
        assert_eq!(p.pool_idle_secs, 3600);
        assert!(!p.stats_show);
        assert_eq!(p.stats_refresh_secs, 1);
        assert_eq!(p.sftp_concurrency, 8);
        assert!(p.sftp_preserve_mtime);
        assert_eq!(p.close_action, CloseChoice::Quit);
        assert_eq!(p.file_window_close, FileWindowChoice::Merge);
    }

    #[test]
    fn unreadable_values_fall_back_to_the_defaults() {
        let p = prefs(&[
            ("keepalive_interval_secs", "\"soon\""),
            ("pool_idle_timeout_secs", "null"),
            ("stats_show", "maybe"),
            ("stats_refresh_secs", "[1]"),
            ("sftp_concurrency", "{"),
            ("close_action", "42"),
            ("file_window_close", "\"sideways\""),
        ]);
        assert_eq!(p, AppPrefs::default());
    }

    #[test]
    fn numbers_stored_as_strings_and_fractions_are_tolerated() {
        let p = prefs(&[
            ("keepalive_interval_secs", "\"45\""),
            ("stats_refresh_secs", "4.6"),
        ]);
        assert_eq!(p.keepalive_secs, 45);
        assert_eq!(p.stats_refresh_secs, 5);
    }

    #[test]
    fn close_choices_accept_quoted_and_bare_text_and_default_otherwise() {
        assert_eq!(CloseChoice::parse(Some("\"quit\"")), CloseChoice::Quit);
        assert_eq!(CloseChoice::parse(Some("quit")), CloseChoice::Quit);
        assert_eq!(CloseChoice::parse(Some("tray")), CloseChoice::Tray);
        assert_eq!(CloseChoice::parse(None), CloseChoice::Tray);
        assert_eq!(
            FileWindowChoice::parse(Some("merge")),
            FileWindowChoice::Merge
        );
        assert_eq!(
            FileWindowChoice::parse(Some("\"merge\"")),
            FileWindowChoice::Merge
        );
        assert_eq!(FileWindowChoice::parse(Some("x")), FileWindowChoice::Close);
    }

    #[test]
    fn writes_use_the_0_2_14_json_shapes_and_read_back() {
        let db = Database::open_in_memory().expect("内存库");
        write_number(&db, KEEPALIVE_INTERVAL_KEY, 9999, KEEPALIVE_RANGE).unwrap();
        write_flag(&db, STATS_SHOW_KEY, false).unwrap();
        write_text(&db, CLOSE_ACTION_KEY, "quit").unwrap();
        write_text(&db, FILE_WINDOW_CLOSE_KEY, "merge").unwrap();
        assert_eq!(
            db.setting_get(KEEPALIVE_INTERVAL_KEY).unwrap().as_deref(),
            Some("300")
        );
        assert_eq!(
            db.setting_get(CLOSE_ACTION_KEY).unwrap().as_deref(),
            Some("\"quit\"")
        );
        let p = AppPrefs::load(&db);
        assert_eq!(p.keepalive_secs, 300);
        assert!(!p.stats_show);
        assert_eq!(p.close_action, CloseChoice::Quit);
        assert_eq!(p.file_window_close, FileWindowChoice::Merge);
    }

    #[test]
    fn a_session_log_is_only_opened_when_enabled_and_writes_plain_text() {
        let dir = std::env::temp_dir().join(format!("zssh-prefs-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = AppPrefs {
            session_log_dir: dir.display().to_string(),
            ..AppPrefs::default()
        };
        assert!(
            open_session_log_with(&p, "web-1").is_none(),
            "默认关：不建文件"
        );
        assert!(!dir.exists());

        p.session_log_enabled = true;
        let log = open_session_log_with(&p, "web-1").expect("开了就建");
        let path = log.path().to_path_buf();
        log.write(b"\x1b[32mok\x1b[0m done\r\n");
        drop(log);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ok done\n");
        assert!(
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("web-1-") && n.ends_with(".log")),
            "文件名是 <主机名>-<时间>.log"
        );

        // 目录建不出来（路径上有一个同名的文件）：不记日志，不报错。
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        p.session_log_dir = blocker.join("sub").display().to_string();
        assert!(open_session_log_with(&p, "web-1").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sftp_settings_become_transfer_options() {
        let p = prefs(&[
            ("sftp_concurrency", "5"),
            ("sftp_preserve_mtime", "true"),
            ("sftp_preserve_permissions", "true"),
        ]);
        let options = p.transfer_options();
        assert_eq!(options.concurrency, 5);
        assert!(options.preserve_mtime && options.preserve_permissions);
        let defaults = AppPrefs::default().transfer_options();
        assert_eq!(defaults.concurrency, 3);
        assert!(!defaults.preserve_mtime && !defaults.preserve_permissions);
    }
}
