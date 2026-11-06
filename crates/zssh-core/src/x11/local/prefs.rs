//! X11 的设置项（数据层 + 默认值；界面在后续卡）。
//!
//! 三个键都只留在本机（见 `sync::settings::SETTINGS_SYNC_CLASS`）：显示地址和"退出时关不关 X
//! 服务器"都跟这台电脑装了什么相关，同步过去会把另一台电脑改坏。读取时容错：值缺失、写坏、
//! 不合法一律退回默认，不报错。

use crate::error::ZsshError;
use crate::store::Database;
use crate::x11::display::parse_display;

/// X 显示来源：`"auto"`（默认）或 `"custom"`。
pub const X11_DISPLAY_MODE_KEY: &str = "x11_display_mode";
/// 指定显示时用的 `DISPLAY` 写法，例如 `localhost:0`。
pub const X11_DISPLAY_CUSTOM_KEY: &str = "x11_display_custom";
/// ZeeTerm 退出时是否一并关闭它自己拉起的 X 服务器，默认是（任务卡写"默认关闭"，
/// 按"默认把它关掉"理解，写进报告请用户确认）。
pub const X11_CLOSE_OWNED_KEY: &str = "x11_close_owned_on_exit";

/// 指定显示的默认写法：Windows 上 X410、MobaXterm 的 X 服务常用 `localhost:0`，Linux 上是本机第一个显示 `:0`。
pub const DEFAULT_CUSTOM_DISPLAY: &str = if cfg!(unix) { ":0" } else { "localhost:0" };

/// X 显示来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayMode {
    /// 自动：先用已在运行的 X 服务器，没有就拉起 VcXsrv。
    Auto,
    /// 指定显示（用户自己管理的 X 服务器），值是 `DISPLAY` 写法。
    Custom(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct X11Prefs {
    pub mode: DisplayMode,
    /// ZeeTerm 退出时关闭自己拉起的 X 服务器。用户自己开的 X 服务器永远不关。
    pub close_owned_on_exit: bool,
}

impl Default for X11Prefs {
    fn default() -> Self {
        Self {
            mode: DisplayMode::Auto,
            // 用户决定（2026-11-02）：默认随 ZeeTerm 一起关闭自己拉起的实例。
            close_owned_on_exit: true,
        }
    }
}

fn json_string(raw: Option<String>) -> Option<String> {
    match serde_json::from_str::<serde_json::Value>(&raw?) {
        Ok(serde_json::Value::String(text)) => Some(text),
        _ => None,
    }
}

/// 读设置。数据库读不了或值不对都按默认。
pub fn load_prefs(db: &Database) -> X11Prefs {
    let mut prefs = X11Prefs::default();
    let mode = json_string(db.setting_get(X11_DISPLAY_MODE_KEY).ok().flatten());
    if mode.as_deref() == Some("custom") {
        let custom = json_string(db.setting_get(X11_DISPLAY_CUSTOM_KEY).ok().flatten())
            .filter(|spec| parse_display(spec).is_ok())
            .unwrap_or_else(|| DEFAULT_CUSTOM_DISPLAY.to_owned());
        prefs.mode = DisplayMode::Custom(custom);
    }
    if let Some(Ok(serde_json::Value::Bool(close))) = db
        .setting_get(X11_CLOSE_OWNED_KEY)
        .ok()
        .flatten()
        .map(|raw| serde_json::from_str::<serde_json::Value>(&raw))
    {
        prefs.close_owned_on_exit = close;
    }
    prefs
}

/// 保存显示来源。指定显示的写法先过解析，不合法就拒绝（不落库）。
pub fn save_display_mode(db: &Database, mode: &DisplayMode) -> Result<(), ZsshError> {
    match mode {
        DisplayMode::Auto => db.setting_set(X11_DISPLAY_MODE_KEY, "\"auto\""),
        DisplayMode::Custom(spec) => {
            parse_display(spec).map_err(ZsshError::InvalidInput)?;
            let spec = serde_json::Value::String(spec.trim().to_owned()).to_string();
            db.setting_set(X11_DISPLAY_CUSTOM_KEY, &spec)?;
            db.setting_set(X11_DISPLAY_MODE_KEY, "\"custom\"")
        }
    }
}

/// 保存"退出时关闭自己拉起的 X 服务器"。
pub fn save_close_owned_on_exit(db: &Database, close: bool) -> Result<(), ZsshError> {
    db.setting_set(X11_CLOSE_OWNED_KEY, if close { "true" } else { "false" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TestDir;

    fn db() -> (TestDir, Database) {
        let dir = TestDir::new("x11-prefs");
        let db = Database::open(&dir.join("zssh.db")).expect("打开库");
        (dir, db)
    }

    #[test]
    fn defaults_are_auto_and_close_on_exit() {
        let (_dir, db) = db();
        assert_eq!(load_prefs(&db), X11Prefs::default());
        assert_eq!(X11Prefs::default().mode, DisplayMode::Auto);
        assert!(X11Prefs::default().close_owned_on_exit);
    }

    #[test]
    fn saved_values_round_trip() {
        let (_dir, db) = db();
        save_display_mode(&db, &DisplayMode::Custom(" localhost:3 ".into())).expect("存");
        save_close_owned_on_exit(&db, false).expect("存");
        let prefs = load_prefs(&db);
        assert_eq!(prefs.mode, DisplayMode::Custom("localhost:3".into()));
        assert!(!prefs.close_owned_on_exit);

        save_display_mode(&db, &DisplayMode::Auto).expect("改回自动");
        assert_eq!(load_prefs(&db).mode, DisplayMode::Auto);
    }

    #[test]
    fn an_invalid_custom_display_is_refused_and_not_saved() {
        let (_dir, db) = db();
        let err = save_display_mode(&db, &DisplayMode::Custom("/tmp/x".into())).unwrap_err();
        assert!(matches!(err, ZsshError::InvalidInput(_)), "{err}");
        assert_eq!(load_prefs(&db).mode, DisplayMode::Auto);
    }

    #[test]
    fn garbage_in_the_database_falls_back_to_defaults() {
        let (_dir, db) = db();
        db.setting_set(X11_DISPLAY_MODE_KEY, "\"custom\"").unwrap();
        db.setting_set(X11_DISPLAY_CUSTOM_KEY, "\"not a display\"")
            .unwrap();
        db.setting_set(X11_CLOSE_OWNED_KEY, "\"yes\"").unwrap();
        let prefs = load_prefs(&db);
        assert_eq!(
            prefs.mode,
            DisplayMode::Custom(DEFAULT_CUSTOM_DISPLAY.into())
        );
        assert!(prefs.close_owned_on_exit, "类型不对按默认");

        db.setting_set(X11_DISPLAY_MODE_KEY, "not json").unwrap();
        assert_eq!(load_prefs(&db).mode, DisplayMode::Auto);
    }
}
