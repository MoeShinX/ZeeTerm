//! 窗口尺寸/位置持久化，以及主窗口的布局（当前页、主机树宽度/折叠/展开的分组）。
//!
//! 单独存 `window-state.json`，不和 `ui-settings.json` 抢写：界面设置由终端视图
//! 保存，窗口状态由开窗/关窗时保存，互不覆盖。位置明显不合理（离屏、尺寸过小）
//! 时直接丢弃，回到居中打开；布局里哪个字段读不懂就只丢那一个字段，不连累别的。

use std::io::Write;
use std::path::{Path, PathBuf};

use ::gpui::{Bounds, Window, WindowBounds, point, px, size};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

pub const MIN_WIDTH: f32 = 480.0;
pub const MIN_HEIGHT: f32 = 320.0;
pub const MAX_SIDE: f32 = 12_000.0;
pub const MAX_OFFSET: f32 = 20_000.0;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedBounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl SavedBounds {
    /// 明显不合理的值（NaN、太小、离屏太远）返回 `None`。
    pub fn sane(self) -> Option<Self> {
        let finite = [self.x, self.y, self.width, self.height]
            .into_iter()
            .all(f32::is_finite);
        if !finite {
            return None;
        }
        if !(MIN_WIDTH..=MAX_SIDE).contains(&self.width) {
            return None;
        }
        if !(MIN_HEIGHT..=MAX_SIDE).contains(&self.height) {
            return None;
        }
        if self.x.abs() > MAX_OFFSET || self.y.abs() > MAX_OFFSET {
            return None;
        }
        Some(self)
    }
}

/// 主机树宽度：默认、下限、上限（和 0.2.14 的 `HOST_TREE_*` 一致）以及占窗口宽度的上限比例。
pub const TREE_DEFAULT_WIDTH: f32 = 224.0;
pub const TREE_MIN_WIDTH: f32 = 180.0;
pub const TREE_MAX_WIDTH: f32 = 480.0;
pub const TREE_MAX_RATIO: f32 = 0.4;

/// 当前窗口宽度下主机树宽度的取值范围：窗口很窄时上限跟着缩，但不低于下限。
pub fn tree_width_bounds(window_width: f32) -> (f32, f32) {
    let max = TREE_MAX_WIDTH.min(window_width * TREE_MAX_RATIO);
    (TREE_MIN_WIDTH, max.max(TREE_MIN_WIDTH))
}

/// 把一个宽度收进范围；不是有限数就回默认值。
pub fn clamp_tree_width(width: f32, window_width: f32) -> f32 {
    if !width.is_finite() {
        return TREE_DEFAULT_WIDTH;
    }
    let (min, max) = tree_width_bounds(window_width);
    width.clamp(min, max)
}

/// 主窗口的布局状态。
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MainLayout {
    /// 上次退出时在哪一页（`vault` / `terminal` / `settings`）；不认识的值读成 `None`。
    pub view: Option<String>,
    /// 设置页上次停在哪个分类（`appearance` / `terminal` / ...）；不认识的值读成 `None`（回默认的外观）。
    pub settings_section: Option<String>,
    pub tree_width: f32,
    pub tree_collapsed: bool,
    /// 展开的分组键（`g<id>` / `ungrouped`）。
    pub tree_expanded: Vec<String>,
}

impl Default for MainLayout {
    fn default() -> Self {
        Self {
            view: None,
            settings_section: None,
            tree_width: TREE_DEFAULT_WIDTH,
            tree_collapsed: false,
            tree_expanded: Vec::new(),
        }
    }
}

impl MainLayout {
    /// 逐个字段宽容地读：哪个字段缺了、类型不对、值不合理，就只那个字段用默认值。
    pub fn from_value(value: &Value) -> Self {
        let mut layout = Self::default();
        let Some(map) = value.as_object() else {
            return layout;
        };
        if let Some(view) = map.get("view").and_then(Value::as_str)
            && crate::main_window::nav::MainView::parse(view).is_some()
        {
            layout.view = Some(view.to_owned());
        }
        if let Some(section) = map.get("settings_section").and_then(Value::as_str)
            && crate::settings_nav::SettingsSection::parse(section).is_some()
        {
            layout.settings_section = Some(section.to_owned());
        }
        if let Some(width) = map.get("tree_width").and_then(Value::as_f64) {
            let width = width as f32;
            if width.is_finite() && (TREE_MIN_WIDTH..=TREE_MAX_WIDTH).contains(&width) {
                layout.tree_width = width;
            }
        }
        if let Some(collapsed) = map.get("tree_collapsed").and_then(Value::as_bool) {
            layout.tree_collapsed = collapsed;
        }
        if let Some(items) = map.get("tree_expanded").and_then(Value::as_array) {
            layout.tree_expanded = items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
        }
        layout
    }
}

fn lenient_layout<'de, D: Deserializer<'de>>(deserializer: D) -> Result<MainLayout, D::Error> {
    let value = Value::deserialize(deserializer)?;
    Ok(MainLayout::from_value(&value))
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowState {
    pub main: Option<SavedBounds>,
    pub hosts: Option<SavedBounds>,
    #[serde(deserialize_with = "lenient_layout")]
    pub layout: MainLayout,
}

impl WindowState {
    pub fn load() -> Self {
        Self::load_from(&state_path()).unwrap_or_default()
    }

    pub fn load_from(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn save(&self) {
        if let Err(err) = self.save_to(&state_path()) {
            tracing::warn!(%err, "保存窗口状态失败");
        }
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

    /// 当前窗口的尺寸/位置（用可恢复的 `window_bounds`，避免标题栏/边框误差）。
    pub fn capture(window: &Window) -> SavedBounds {
        let bounds = match window.window_bounds() {
            WindowBounds::Windowed(bounds)
            | WindowBounds::Maximized(bounds)
            | WindowBounds::Fullscreen(bounds) => bounds,
        };
        SavedBounds {
            x: f32::from(bounds.origin.x),
            y: f32::from(bounds.origin.y),
            width: f32::from(bounds.size.width),
            height: f32::from(bounds.size.height),
        }
    }

    /// 恢复成开窗参数；不合理时返回 `None`，由调用方退回居中。
    pub fn restore(bounds: Option<SavedBounds>) -> Option<WindowBounds> {
        let bounds = bounds?.sane()?;
        Some(WindowBounds::Windowed(Bounds {
            origin: point(px(bounds.x), px(bounds.y)),
            size: size(px(bounds.width), px(bounds.height)),
        }))
    }
}

fn state_path() -> PathBuf {
    crate::settings::data_dir().join("window-state.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zssh-window-state-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sane_bounds_pass_and_bad_ones_are_dropped() {
        let good = SavedBounds {
            x: 120.0,
            y: 80.0,
            width: 1100.0,
            height: 760.0,
        };
        assert_eq!(good.sane(), Some(good));

        assert!(
            SavedBounds {
                width: 10.0,
                ..good
            }
            .sane()
            .is_none()
        );
        assert!(
            SavedBounds {
                height: 10.0,
                ..good
            }
            .sane()
            .is_none()
        );
        assert!(
            SavedBounds {
                x: 99_999.0,
                ..good
            }
            .sane()
            .is_none()
        );
        assert!(
            SavedBounds {
                width: f32::NAN,
                ..good
            }
            .sane()
            .is_none()
        );
    }

    #[test]
    fn state_round_trips() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("window-state.json");
        let state = WindowState {
            main: Some(SavedBounds {
                x: 10.0,
                y: 20.0,
                width: 1024.0,
                height: 768.0,
            }),
            hosts: None,
            layout: MainLayout {
                view: Some("settings".to_owned()),
                settings_section: Some("sync".to_owned()),
                tree_width: 300.0,
                tree_collapsed: true,
                tree_expanded: vec!["g1".to_owned(), "ungrouped".to_owned()],
            },
        };
        state.save_to(&path).unwrap();
        assert_eq!(WindowState::load_from(&path), Some(state));
    }

    #[test]
    fn an_old_file_without_a_layout_still_loads_with_defaults() {
        let dir = temp_dir("oldfile");
        let path = dir.join("window-state.json");
        std::fs::write(
            &path,
            br#"{"main":{"x":1.0,"y":2.0,"width":900.0,"height":700.0}}"#,
        )
        .unwrap();
        let state = WindowState::load_from(&path).unwrap();
        assert_eq!(state.main.map(|b| b.width), Some(900.0));
        assert_eq!(state.layout, MainLayout::default());
    }

    #[test]
    fn the_settings_section_is_kept_only_when_it_is_a_known_one() {
        let dir = temp_dir("sections");
        let path = dir.join("window-state.json");
        std::fs::write(&path, br#"{"layout":{"settings_section":"shortcuts"}}"#).unwrap();
        assert_eq!(
            WindowState::load_from(&path)
                .unwrap()
                .layout
                .settings_section
                .as_deref(),
            Some("shortcuts")
        );
        for bad in [r#""nowhere""#, "7", "null"] {
            std::fs::write(
                &path,
                format!(r#"{{"layout":{{"settings_section":{bad}}}}}"#),
            )
            .unwrap();
            assert_eq!(
                WindowState::load_from(&path)
                    .unwrap()
                    .layout
                    .settings_section,
                None,
                "{bad} 不该被当成分类"
            );
        }
    }

    #[test]
    fn a_bad_layout_field_only_loses_that_field() {
        let dir = temp_dir("badlayout");
        let path = dir.join("window-state.json");
        std::fs::write(
            &path,
            br#"{"main":{"x":1.0,"y":2.0,"width":900.0,"height":700.0},
                 "layout":{"view":"nowhere","tree_width":"wide","tree_collapsed":true,
                           "tree_expanded":["g3", 7, "ungrouped"]}}"#,
        )
        .unwrap();
        let state = WindowState::load_from(&path).unwrap();
        assert!(state.main.is_some(), "窗口尺寸不能被布局里的坏字段连累");
        assert_eq!(state.layout.view, None);
        assert_eq!(state.layout.settings_section, None);
        assert_eq!(state.layout.tree_width, TREE_DEFAULT_WIDTH);
        assert!(state.layout.tree_collapsed);
        assert_eq!(state.layout.tree_expanded, ["g3", "ungrouped"]);

        // 整个 layout 不是对象也不崩。
        std::fs::write(&path, br#"{"layout": 5}"#).unwrap();
        assert_eq!(
            WindowState::load_from(&path).unwrap().layout,
            MainLayout::default()
        );
        // 宽度超出范围按默认。
        std::fs::write(&path, br#"{"layout":{"tree_width": 9999}}"#).unwrap();
        assert_eq!(
            WindowState::load_from(&path).unwrap().layout.tree_width,
            TREE_DEFAULT_WIDTH
        );
    }

    #[test]
    fn tree_width_is_clamped_to_the_window() {
        assert_eq!(clamp_tree_width(f32::NAN, 1200.0), TREE_DEFAULT_WIDTH);
        assert_eq!(clamp_tree_width(10.0, 1200.0), TREE_MIN_WIDTH);
        assert_eq!(clamp_tree_width(900.0, 1600.0), TREE_MAX_WIDTH);
        // 窄窗口：上限按 40% 缩，但不低于下限。
        assert_eq!(clamp_tree_width(400.0, 800.0), 320.0);
        assert_eq!(clamp_tree_width(400.0, 300.0), TREE_MIN_WIDTH);
        assert_eq!(clamp_tree_width(250.0, 1200.0), 250.0);
    }

    #[test]
    fn missing_or_corrupt_state_falls_back() {
        let dir = temp_dir("broken");
        assert_eq!(WindowState::load_from(&dir.join("none.json")), None);
        let path = dir.join("window-state.json");
        std::fs::write(&path, b"{ nope").unwrap();
        assert_eq!(WindowState::load_from(&path), None);
    }
}
