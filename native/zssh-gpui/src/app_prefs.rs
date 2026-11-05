//! 设置页「连接」「SFTP」「外观里的窗口」这几组设置：存在数据库的 `settings` 表里，键名、取值范围、
//! 默认值都和 0.2.14 一致（`stores/settings.ts`），所以同一个库在两个版本之间互相读写不会错位。
//!
//! 值在表里是 JSON 文本（数字 `30`、布尔 `true`、字符串 `"tray"`）；读的时候容忍没带引号的老值、
//! 超范围的数、读不懂的类型，一律回到默认或夹进范围，不崩、不猜。

use zssh_core::store::Database;
use zssh_core::store::settings::{KEEPALIVE_INTERVAL_KEY, POOL_IDLE_TIMEOUT_KEY};

use crate::i18n::{Msg, t};

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
/// 上一次退出时会话日志的状况（有日志没写完 / 写失败 / 建不成文件时留一句话，正常退出就清空），
/// 下次打开设置里的会话日志时显示。只在本机。
pub const SESSION_LOG_EXIT_NOTE_KEY: &str = "session_log_exit_note";
/// 侧栏底部的「属性」面板（选中主机时显示名称 / 主机 / 用户名…）：默认关，只在本机（V2-U17）。
pub const HOST_INFO_PANEL_KEY: &str = "host_info_panel";

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
pub const SFTP_CONCURRENCY_DEFAULT: u32 = 5;

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
    /// 上一次退出时会话日志的状况（空 = 一切正常）。
    pub session_log_exit_note: String,
    /// 在侧栏显示主机属性面板（默认关）。
    pub host_info_panel: bool,
}

impl Default for AppPrefs {
    fn default() -> Self {
        Self::from_lookup(|_| None)
    }
}

impl AppPrefs {
    /// 把「连接空闲断开」推给连接池。启动、改设置、重读设置之后都要调，池才按当前值计时。
    pub fn apply_pool_idle(&self) {
        crate::ssh_backend::set_pool_idle_secs(self.pool_idle_secs);
    }

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
            session_log_exit_note: text_of(raw(SESSION_LOG_EXIT_NOTE_KEY).as_deref())
                .map(|note| note.trim().to_owned())
                .filter(|note| !note.chars().any(char::is_control))
                .unwrap_or_default(),
            host_info_panel: flag(raw(HOST_INFO_PANEL_KEY).as_deref(), false),
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

// ---- 进程级缓存 ----
//
// 连上一个终端要判断"设置里开没开会话日志"，这发生在 UI 线程的附着路径上
// （审查 M-11）。每次都 `Database::open` + 读十几条设置太贵，所以进程里缓存一份；
// 设置页改会话日志相关的项之后调用 [`refresh_cached`] 刷新。

static CACHED_PREFS: std::sync::OnceLock<std::sync::Mutex<Option<std::sync::Arc<AppPrefs>>>> =
    std::sync::OnceLock::new();

fn prefs_cache() -> &'static std::sync::Mutex<Option<std::sync::Arc<AppPrefs>>> {
    CACHED_PREFS.get_or_init(|| std::sync::Mutex::new(None))
}

/// 取一份缓存的设置。缓存空的时候从库里读一次（进程里最多一次）。
pub fn cached() -> std::sync::Arc<AppPrefs> {
    let mut slot = prefs_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(prefs) = slot.as_ref() {
        return std::sync::Arc::clone(prefs);
    }
    let prefs = std::sync::Arc::new(AppPrefs::load_default_db());
    *slot = Some(std::sync::Arc::clone(&prefs));
    prefs
}

/// 设置页保存了会话日志相关设置：把缓存换成新值（其它设置项不走这个缓存）。
pub fn refresh_cached(prefs: AppPrefs) {
    let mut slot = prefs_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = Some(std::sync::Arc::new(prefs));
}

/// 开了会话日志就为这个会话建一个日志文件（纯文本，去掉颜色和光标控制）；没开就返回 `None`，
/// 会话照常。每个会话一个文件，只对新打开的会话生效。
///
/// 设置由调用方给（界面里用进程缓存 [`cached`]，不在 UI 线程开库——审查 M-11）。
/// 建目录和开文件在日志的写线程里做，这里立刻返回；目录建不了（没权限、路径失效、
/// 慢盘出错）时这个会话没有日志，原因记在
/// [`zssh_core::ssh::session_log::problems`] 里，设置页的会话日志一栏会显示。
pub fn open_session_log_with(
    prefs: &AppPrefs,
    label: &str,
) -> Option<zssh_core::ssh::session_log::SessionLog> {
    if !prefs.session_log_enabled {
        return None;
    }
    match start_session_log(prefs, label) {
        Ok(log) => Some(log),
        Err(err) => {
            // 目录路径是用户填的本机路径，日志里只写原因，不写路径。
            tracing::warn!(%err, "起不了会话日志的写线程，这个会话不记日志");
            None
        }
    }
}

/// 不管设置里的开关，为这个会话建一个日志文件（手动「开始记录」用）。位置、命名、写线程
/// 和 [`open_session_log`] 完全一样——会话日志只有这一个写入位置。
pub fn start_session_log(
    prefs: &AppPrefs,
    label: &str,
) -> std::io::Result<zssh_core::ssh::session_log::SessionLog> {
    zssh_core::ssh::session_log::SessionLog::start(
        &prefs.session_log_dir_path(),
        label,
        &zssh_core::ssh::session_log::local_stamp(),
    )
}

/// 退出时让所有会话日志把排着的写完，最多等这么久（磁盘慢时不能让退出无限期卡住）。
pub const SESSION_LOG_EXIT_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// 会话日志的退出提示和问题清单里的界面文案（只在显示时取词；写库的是当时生成的那句话）。
///
/// 文件名、目录、底层错误的原因都来自本机，原样显示，不进这张表；带数量的句子保留 `format!`，
/// 这里只提供固定片段。
#[derive(Clone, Copy, Debug)]
enum PrefsMsg {
    UnfinishedLead,
    UnfinishedTail,
    MoreLead,
    MoreTail,
    ProblemSeparator,
    NoteJoin,
}

impl Msg for PrefsMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::UnfinishedLead => "退出时有 ",
            Self::UnfinishedTail => " 个会话日志还没写完（磁盘太慢？），它们的结尾可能不完整",
            Self::MoreLead => "另有 ",
            Self::MoreTail => " 条",
            Self::ProblemSeparator => "：",
            Self::NoteJoin => "；",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::UnfinishedLead => "Still writing at exit: ",
            Self::UnfinishedTail => " session log(s) (slow disk?); their endings may be incomplete",
            Self::MoreLead => "and ",
            Self::MoreTail => " more",
            Self::ProblemSeparator => ": ",
            Self::NoteJoin => "; ",
        }
    }
}

/// 把 [`zssh_core::ssh::session_log::FinishReport`] 写成一句话（设置页显示）；一切正常返回空串。
pub fn exit_note(report: &zssh_core::ssh::session_log::FinishReport) -> String {
    let mut parts = Vec::new();
    if report.unfinished > 0 {
        parts.push(format!(
            "{}{}{}",
            t(PrefsMsg::UnfinishedLead),
            report.unfinished,
            t(PrefsMsg::UnfinishedTail)
        ));
    }
    for problem in report.problems.iter().take(3) {
        parts.push(file_message(problem));
    }
    if report.problems.len() > 3 {
        parts.push(format!(
            "{}{}{}",
            t(PrefsMsg::MoreLead),
            report.problems.len() - 3,
            t(PrefsMsg::MoreTail)
        ));
    }
    parts.join(t(PrefsMsg::NoteJoin))
}

/// 「文件：原因」一条。文件名和原因都来自本机（用户填的目录 / 底层错误），原样显示。
fn file_message(problem: &zssh_core::ssh::session_log::LogProblem) -> String {
    format!(
        "{}{}{}",
        problem.file,
        t(PrefsMsg::ProblemSeparator),
        problem.message
    )
}

/// 程序退出时调用：让会话日志收尾（有限等待），并把状况记进设置，下次打开会话日志设置时看得到。
/// 一切正常时清掉上次留下的提示。设置库打不开不影响退出。
pub fn finish_session_logs_at_exit() {
    let report = zssh_core::ssh::session_log::finish_all(SESSION_LOG_EXIT_WAIT);
    let note = exit_note(&report);
    let Ok(db) = Database::open(&crate::settings::data_dir().join("zssh.db")) else {
        return;
    };
    record_exit_note(&db, &note);
}

/// 写（或清掉）退出提示；没变化就不碰库。
fn record_exit_note(db: &Database, note: &str) {
    let current = text_of(
        db.setting_get(SESSION_LOG_EXIT_NOTE_KEY)
            .ok()
            .flatten()
            .as_deref(),
    )
    .unwrap_or_default();
    if current.trim() == note {
        return;
    }
    if let Err(err) = write_text(db, SESSION_LOG_EXIT_NOTE_KEY, note) {
        tracing::warn!(%err, "没能记下会话日志的退出状况");
    }
}

/// 设置页显示用：本次运行里会话日志出过的问题，一条一行。
pub fn session_log_problem_lines() -> Vec<String> {
    zssh_core::ssh::session_log::problems()
        .iter()
        .map(file_message)
        .collect()
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
        assert_eq!(p.sftp_concurrency, 5);
        assert!(!p.sftp_preserve_mtime);
        assert!(!p.sftp_preserve_permissions);
        assert!(!p.sftp_show_hidden);
        assert_eq!(p.close_action, CloseChoice::Tray);
        assert_eq!(p.file_window_close, FileWindowChoice::Close);
        assert!(!p.session_log_enabled, "会话日志默认关");
        assert_eq!(p.session_log_dir, "");
    }

    #[test]
    fn the_host_info_panel_is_off_by_default_and_the_choice_is_saved() {
        assert!(!AppPrefs::default().host_info_panel, "属性面板默认关");
        assert!(!prefs(&[("host_info_panel", "garbage")]).host_info_panel);
        let db = Database::open_in_memory().expect("内存库");
        assert!(!AppPrefs::load(&db).host_info_panel);
        write_flag(&db, HOST_INFO_PANEL_KEY, true).unwrap();
        assert!(AppPrefs::load(&db).host_info_panel);
        write_flag(&db, HOST_INFO_PANEL_KEY, false).unwrap();
        assert!(!AppPrefs::load(&db).host_info_panel);
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
        log.write(b"\x1b[32mok\x1b[0m done\r\n");
        // 文件在写线程里建，等它建出来、收尾之后路径和内容才都确定。
        let started = std::time::Instant::now();
        while log.path().is_none() {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "文件一直没建出来"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let path = log.path().map(std::path::Path::to_path_buf).unwrap();
        assert!(log.close_and_wait(std::time::Duration::from_secs(10)));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ok done\n");
        assert!(
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("web-1-") && n.ends_with(".log")),
            "文件名是 <主机名>-<时间>.log"
        );

        // 目录建不出来（路径上有一个同名的文件）：调用立刻返回、会话照常；原因记在问题清单里。
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        p.session_log_dir = blocker.join("sub").display().to_string();
        let broken = open_session_log_with(&p, "blocked-host").expect("立刻拿到对象，失败在后台");
        assert!(broken.close_and_wait(std::time::Duration::from_secs(10)));
        let lines = session_log_problem_lines();
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("blocked-host-") && line.contains("建不了日志文件")),
            "{lines:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_exit_note_says_what_was_left_unwritten_and_what_failed() {
        use zssh_core::ssh::session_log::{FinishReport, LogProblem};
        let clean = FinishReport {
            closed: 2,
            unfinished: 0,
            problems: Vec::new(),
        };
        assert_eq!(exit_note(&clean), "", "一切正常：没有提示");
        let slow = FinishReport {
            closed: 2,
            unfinished: 1,
            problems: vec![LogProblem {
                file: "web-1-20261103-100000.log".to_owned(),
                message: "写入失败（磁盘已满），从这里起的输出没有记录，已丢掉约 12 字节"
                    .to_owned(),
            }],
        };
        let note = exit_note(&slow);
        assert!(note.contains("1 个会话日志还没写完"), "{note}");
        assert!(
            note.contains("web-1-20261103-100000.log：写入失败（磁盘已满）"),
            "{note}"
        );
    }

    #[test]
    fn the_exit_note_is_stored_shown_next_time_and_cleared_by_a_clean_exit() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(AppPrefs::load(&db).session_log_exit_note, "");
        record_exit_note(&db, "退出时有 1 个会话日志还没写完");
        assert_eq!(
            AppPrefs::load(&db).session_log_exit_note,
            "退出时有 1 个会话日志还没写完"
        );
        // 下一次正常退出：提示清掉。
        record_exit_note(&db, "");
        assert_eq!(AppPrefs::load(&db).session_log_exit_note, "");
        // 带控制字符的脏值读成空，不显示。
        db.setting_set(SESSION_LOG_EXIT_NOTE_KEY, "\"a\\u0007b\"")
            .unwrap();
        assert_eq!(AppPrefs::load(&db).session_log_exit_note, "");
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
        assert_eq!(defaults.concurrency, 5);
        assert!(!defaults.preserve_mtime && !defaults.preserve_permissions);
    }
}
