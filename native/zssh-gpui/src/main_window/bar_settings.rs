//! 终端底部两条栏、AI 面板、文件面板用到的设置（和 0.2.14 同键同格式）。
//!
//! | 键 | 内容 | 同步 |
//! |---|---|---|
//! | `quick_commands` | 快捷命令列表（JSON 数组序列化成的字符串） | 同步 |
//! | `quick_commands_bar` / `compose_bar` | 命令栏 / 撰写栏开着没有 | 本机 |
//! | `quick_commands_group` | 命令栏当前分组 | 本机 |
//! | `layout.quick_bar_height` / `layout.compose_bar_height` | 栏高（撰写栏 0 = 自动） | 本机 |
//! | `layout.ai_panel_width` / `layout.sftp_panel_width` / `layout.overview_width` / `layout.ai_input_height` | 面板宽度、AI 输入框高度 | 本机 |
//!
//! 读不懂的值回到默认，不崩；写失败把原因交给调用方显示。

use zssh_core::store::Database;

use super::bar_height;
use super::page_logic::{setting_json, setting_text};
use super::panel_width::{self, PanelKind};
use super::quick_commands::{self, QuickCommand};

pub const QUICK_COMMANDS: &str = "quick_commands";
pub const QUICK_BAR: &str = "quick_commands_bar";
pub const QUICK_GROUP: &str = "quick_commands_group";
pub const COMPOSE_BAR: &str = "compose_bar";
pub const QUICK_HEIGHT: &str = "layout.quick_bar_height";
pub const COMPOSE_HEIGHT: &str = "layout.compose_bar_height";
pub const AI_INPUT_HEIGHT: &str = "layout.ai_input_height";

#[derive(Clone, Debug, PartialEq)]
pub struct BarSettings {
    pub commands: Vec<QuickCommand>,
    pub quick_open: bool,
    pub compose_open: bool,
    pub group: String,
    pub quick_height: f32,
    /// 0 = 自动增高。
    pub compose_height: f32,
    pub ai_input_height: f32,
    pub ai_width: f32,
    pub sftp_width: f32,
    /// 服务器概览面板的宽度。
    pub overview_width: f32,
}

impl Default for BarSettings {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            quick_open: false,
            compose_open: false,
            group: String::new(),
            quick_height: bar_height::QUICK_DEFAULT,
            compose_height: bar_height::COMPOSE_AUTO,
            ai_input_height: bar_height::AI_INPUT_DEFAULT,
            ai_width: panel_width::AI_DEFAULT,
            sftp_width: panel_width::SFTP_DEFAULT,
            overview_width: panel_width::OVERVIEW_DEFAULT,
        }
    }
}

/// 设置表里的 JSON 布尔；读不懂按默认。
pub fn parse_bool(raw: Option<&str>, default: bool) -> bool {
    match raw.map(str::trim) {
        Some("true") => true,
        Some("false") => false,
        _ => default,
    }
}

fn read(db: &Database, key: &str) -> Option<String> {
    db.setting_get(key).ok().flatten()
}

impl BarSettings {
    pub fn load(db: &Database) -> Self {
        let number = |key: &str| panel_width::parse_stored(read(db, key).as_deref());
        Self {
            commands: quick_commands::parse_commands(
                &setting_text(read(db, QUICK_COMMANDS).as_deref()).unwrap_or_default(),
            ),
            quick_open: parse_bool(read(db, QUICK_BAR).as_deref(), false),
            compose_open: parse_bool(read(db, COMPOSE_BAR).as_deref(), false),
            group: setting_text(read(db, QUICK_GROUP).as_deref()).unwrap_or_default(),
            quick_height: bar_height::sanitize_quick(number(QUICK_HEIGHT)),
            compose_height: bar_height::sanitize_compose(number(COMPOSE_HEIGHT)),
            ai_input_height: bar_height::sanitize_ai_input(number(AI_INPUT_HEIGHT)),
            ai_width: panel_width::sanitize_stored(
                number(PanelKind::Ai.setting_key()),
                panel_width::AI_DEFAULT,
            ),
            sftp_width: panel_width::sanitize_stored(
                number(PanelKind::Sftp.setting_key()),
                panel_width::SFTP_DEFAULT,
            ),
            overview_width: panel_width::sanitize_stored(
                number(PanelKind::Overview.setting_key()),
                panel_width::OVERVIEW_DEFAULT,
            ),
        }
    }
}

pub fn write_bool(db: &Database, key: &str, value: bool) -> Result<(), String> {
    db.setting_set(key, if value { "true" } else { "false" })
        .map_err(|err| crate::error_text::error_text(&err))
}

pub fn write_number(db: &Database, key: &str, value: f32) -> Result<(), String> {
    db.setting_set(key, &format!("{}", value.round() as i64))
        .map_err(|err| crate::error_text::error_text(&err))
}

pub fn write_text(db: &Database, key: &str, value: &str) -> Result<(), String> {
    db.setting_set(key, &setting_json(value))
        .map_err(|err| crate::error_text::error_text(&err))
}

pub fn write_commands(db: &Database, list: &[QuickCommand]) -> Result<(), String> {
    write_text(
        db,
        QUICK_COMMANDS,
        &quick_commands::serialize_commands(list),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Database {
        Database::open_in_memory().expect("内存库")
    }

    #[test]
    fn an_empty_database_gives_the_defaults() {
        assert_eq!(BarSettings::load(&db()), BarSettings::default());
    }

    #[test]
    fn values_round_trip_in_the_same_format_as_the_front_end() {
        let db = db();
        let list = vec![QuickCommand {
            id: "qc-1".into(),
            name: "端口".into(),
            command: "ss -tlnp".into(),
            group: Some("排查".into()),
            enter: true,
            color: Some("blue".into()),
        }];
        write_commands(&db, &list).expect("写命令");
        write_bool(&db, QUICK_BAR, true).expect("写开关");
        write_bool(&db, COMPOSE_BAR, true).expect("写开关");
        write_text(&db, QUICK_GROUP, "排查").expect("写分组");
        write_number(&db, QUICK_HEIGHT, 76.0).expect("写高度");
        write_number(&db, COMPOSE_HEIGHT, 120.4).expect("写高度");
        write_number(&db, PanelKind::Ai.setting_key(), 512.0).expect("写宽度");
        write_number(&db, PanelKind::Overview.setting_key(), 433.0).expect("写宽度");

        // 设置表里是 0.2.14 能读的写法：命令是「JSON 字符串里装着 JSON 数组」，布尔和数字是裸 JSON。
        assert_eq!(read(&db, QUICK_BAR).as_deref(), Some("true"));
        assert_eq!(read(&db, QUICK_HEIGHT).as_deref(), Some("76"));
        let raw = read(&db, QUICK_COMMANDS).expect("有值");
        let inner: String = serde_json::from_str(&raw).expect("外层是 JSON 字符串");
        assert!(inner.starts_with("[{"), "{inner}");

        let loaded = BarSettings::load(&db);
        assert_eq!(loaded.commands, list);
        assert!(loaded.quick_open && loaded.compose_open);
        assert_eq!(loaded.group, "排查");
        assert_eq!(loaded.quick_height, 76.0);
        assert_eq!(loaded.compose_height, 120.0);
        assert_eq!(loaded.ai_width, 512.0);
        assert_eq!(loaded.overview_width, 433.0);
    }

    #[test]
    fn garbage_falls_back_to_the_defaults() {
        let db = db();
        db.setting_set(QUICK_COMMANDS, "\"not json\"").expect("写");
        db.setting_set(QUICK_BAR, "\"yes\"").expect("写");
        db.setting_set(QUICK_HEIGHT, "\"tall\"").expect("写");
        db.setting_set(COMPOSE_HEIGHT, "-3").expect("写");
        let loaded = BarSettings::load(&db);
        assert!(loaded.commands.is_empty());
        assert!(!loaded.quick_open);
        assert_eq!(loaded.quick_height, bar_height::QUICK_DEFAULT);
        assert_eq!(loaded.compose_height, bar_height::COMPOSE_AUTO);
    }

    #[test]
    fn bool_parsing_is_strict() {
        assert!(parse_bool(Some(" true "), false));
        assert!(!parse_bool(Some("1"), false));
        assert!(parse_bool(None, true));
    }
}
