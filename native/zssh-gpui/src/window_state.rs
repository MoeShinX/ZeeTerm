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
    /// 设置页上次停在哪个分类（`general` / `appearance` / `terminal` / ...）；不认识的值读成 `None`（回默认的常规）。
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

/// 独立文件窗口的位置：读不懂就当没存过，不能让它连累整个文件（主窗口的位置也在里面）。
fn lenient_bounds<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<SavedBounds>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
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
    /// 独立「ZeeTerm 文件」窗口上次的大小和位置（用户手动调整的；最大化时是还原后的大小）。
    #[serde(deserialize_with = "lenient_bounds")]
    pub files: Option<SavedBounds>,
    /// 独立文件窗口上次是不是最大化着。
    pub files_maximized: bool,
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
}

/// 保存的位置要落在现有的某个显示器上才算数：标题栏那一条（窗口顶部 `STRIP` 高）和某个显示器
/// 至少重叠 `MIN_GRAB_WIDTH` x `MIN_GRAB_HEIGHT`，用户才抓得到它来拖。拔了副屏、换了分辨率之后
/// 上次的位置可能已经在屏幕外，那样就不能照着开（会开到看不见的地方）。
pub const TITLE_STRIP: f32 = 40.0;
const MIN_GRAB_WIDTH: f32 = 120.0;
const MIN_GRAB_HEIGHT: f32 = 24.0;

/// 窗口标题条在不在某个显示器上。`displays` 是各显示器的矩形（全局逻辑像素）。
pub fn on_a_display(bounds: SavedBounds, displays: &[SavedBounds]) -> bool {
    displays.iter().any(|display| {
        let left = bounds.x.max(display.x);
        let right = (bounds.x + bounds.width).min(display.x + display.width);
        let top = bounds.y.max(display.y);
        let bottom = (bounds.y + TITLE_STRIP.min(bounds.height)).min(display.y + display.height);
        right - left >= MIN_GRAB_WIDTH && bottom - top >= MIN_GRAB_HEIGHT
    })
}

/// 文件窗口开窗参数：用上次的大小位置（尺寸不低于窗口的最小尺寸）；没存过、值不合理、位置已经不在
/// 任何显示器上时返回 `None`，由调用方退回居中的默认大小。上次是最大化的就还是最大化（还原大小保留）。
pub fn restore_on_displays(
    bounds: Option<SavedBounds>,
    maximized: bool,
    displays: &[SavedBounds],
    min: (f32, f32),
) -> Option<WindowBounds> {
    let bounds = bounds?.sane()?;
    let bounds = SavedBounds {
        width: bounds.width.max(min.0),
        height: bounds.height.max(min.1),
        ..bounds
    };
    if !on_a_display(bounds, displays) {
        return None;
    }
    let rect = Bounds {
        origin: point(px(bounds.x), px(bounds.y)),
        size: size(px(bounds.width), px(bounds.height)),
    };
    Some(if maximized {
        WindowBounds::Maximized(rect)
    } else {
        WindowBounds::Windowed(rect)
    })
}

/// 主窗口的最小尺寸；记住的大小也不会低于它（和开窗时的 `window_min_size` 一致）。
pub const MAIN_MIN: (f32, f32) = (720.0, 480.0);
/// 主机库窗口没设系统最小尺寸，这里只保证记住的大小不小于 `sane` 的下限。
pub const HOSTS_MIN: (f32, f32) = (MIN_WIDTH, MIN_HEIGHT);

/// 主窗口上次的位置：不在任何显示器上（拔了副屏、换了分辨率）就返回 `None`，由调用方居中。
/// 主窗口不记最大化（`capture` 存的是还原后的矩形），所以总是普通窗口。
pub fn restore_main(state: &WindowState, displays: &[SavedBounds]) -> Option<WindowBounds> {
    restore_on_displays(state.main, false, displays, MAIN_MIN)
}

/// 主机库窗口上次的位置，规则同 [`restore_main`]。
pub fn restore_hosts(state: &WindowState, displays: &[SavedBounds]) -> Option<WindowBounds> {
    restore_on_displays(state.hosts, false, displays, HOSTS_MIN)
}

/// 现在所有显示器的矩形。
pub fn display_rects(cx: &::gpui::App) -> Vec<SavedBounds> {
    cx.displays()
        .iter()
        .map(|display| {
            let bounds = display.bounds();
            SavedBounds {
                x: f32::from(bounds.origin.x),
                y: f32::from(bounds.origin.y),
                width: f32::from(bounds.size.width),
                height: f32::from(bounds.size.height),
            }
        })
        .collect()
}

/// 记下独立文件窗口现在的大小位置和是否最大化。
pub fn save_files_window(window: &Window) {
    let mut state = WindowState::load();
    state.files = Some(WindowState::capture(window));
    state.files_maximized = window.is_maximized();
    state.save();
}

fn state_path() -> PathBuf {
    crate::settings::data_dir().join("window-state.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 随机唯一的临时目录；返回的 `TempDir` 要拿到用例结束，drop 时整棵删掉。
    fn temp_dir(name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("zssh-window-state-{name}-"))
            .tempdir()
            .unwrap();
        let path = dir.path().to_path_buf();
        (dir, path)
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
        let (_dir_guard, dir) = temp_dir("roundtrip");
        let path = dir.join("window-state.json");
        let state = WindowState {
            main: Some(SavedBounds {
                x: 10.0,
                y: 20.0,
                width: 1024.0,
                height: 768.0,
            }),
            hosts: None,
            files: Some(SavedBounds {
                x: -1500.0,
                y: 30.0,
                width: 900.0,
                height: 600.0,
            }),
            files_maximized: true,
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
        let (_dir_guard, dir) = temp_dir("oldfile");
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
        let (_dir_guard, dir) = temp_dir("sections");
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
    fn old_view_values_and_the_new_welcome_and_manage_ids_all_read_back() {
        let (_dir_guard, dir) = temp_dir("u3-ids");
        let path = dir.join("window-state.json");
        for (view, section) in [
            ("vault", "appearance"),
            ("terminal", "about"),
            ("settings", "sync"),
            ("welcome", "credentials"),
            ("settings", "quick-commands"),
            ("settings", "proxies"),
            ("settings", "forwards"),
        ] {
            std::fs::write(
                &path,
                format!(r#"{{"layout":{{"view":"{view}","settings_section":"{section}"}}}}"#),
            )
            .unwrap();
            let layout = WindowState::load_from(&path).unwrap().layout;
            assert_eq!(layout.view.as_deref(), Some(view));
            assert_eq!(layout.settings_section.as_deref(), Some(section));
        }
    }

    #[test]
    fn a_missing_section_opens_general_and_every_recorded_one_is_restored() {
        use crate::settings_nav::SettingsSection;
        let (_dir_guard, dir) = temp_dir("section-restore");
        let path = dir.join("window-state.json");
        // 没有记录（全新安装 / 旧文件里没有这个字段）：进「常规」。
        std::fs::write(&path, br#"{"layout":{"view":"settings"}}"#).unwrap();
        let layout = WindowState::load_from(&path).unwrap().layout;
        assert_eq!(
            SettingsSection::restored(layout.settings_section.as_deref()),
            SettingsSection::General
        );
        // 旧版本记下的分类 id 和新增的 general 都照常恢复，不会被默认值盖掉。
        for section in SettingsSection::ALL {
            std::fs::write(
                &path,
                format!(r#"{{"layout":{{"settings_section":"{}"}}}}"#, section.id()),
            )
            .unwrap();
            let layout = WindowState::load_from(&path).unwrap().layout;
            assert_eq!(
                SettingsSection::restored(layout.settings_section.as_deref()),
                section
            );
        }
    }

    #[test]
    fn a_bad_layout_field_only_loses_that_field() {
        let (_dir_guard, dir) = temp_dir("badlayout");
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

    fn display(x: f32, y: f32, w: f32, h: f32) -> SavedBounds {
        SavedBounds {
            x,
            y,
            width: w,
            height: h,
        }
    }

    const FILES_MIN: (f32, f32) = (640.0, 420.0);

    #[test]
    fn the_files_window_remembers_its_size_and_whether_it_was_maximized() {
        let (_dir_guard, dir) = temp_dir("files-roundtrip");
        let path = dir.join("window-state.json");
        let mut state = WindowState::default();
        assert_eq!(state.files, None);
        assert!(!state.files_maximized);
        state.files = Some(display(200.0, 100.0, 900.0, 600.0));
        state.files_maximized = true;
        state.save_to(&path).unwrap();
        let back = WindowState::load_from(&path).unwrap();
        assert_eq!(back.files, Some(display(200.0, 100.0, 900.0, 600.0)));
        assert!(back.files_maximized);
        // 老文件里没有这两项：读成没存过。
        std::fs::write(
            &path,
            br#"{"main":{"x":1.0,"y":2.0,"width":900.0,"height":700.0}}"#,
        )
        .unwrap();
        let old = WindowState::load_from(&path).unwrap();
        assert_eq!(old.files, None);
        assert!(!old.files_maximized);
    }

    #[test]
    fn a_garbled_files_entry_does_not_take_the_main_window_down_with_it() {
        let (_dir_guard, dir) = temp_dir("files-garbled");
        let path = dir.join("window-state.json");
        std::fs::write(
            &path,
            br#"{"main":{"x":1.0,"y":2.0,"width":900.0,"height":700.0},"files":"big"}"#,
        )
        .unwrap();
        let state = WindowState::load_from(&path).unwrap();
        assert!(state.main.is_some());
        assert_eq!(state.files, None);
    }

    #[test]
    fn a_saved_files_window_on_a_connected_screen_is_restored_as_it_was() {
        let screens = [display(0.0, 0.0, 1920.0, 1080.0)];
        let saved = display(300.0, 120.0, 900.0, 600.0);
        let WindowBounds::Windowed(rect) =
            restore_on_displays(Some(saved), false, &screens, FILES_MIN).unwrap()
        else {
            panic!("没最大化就该是普通窗口");
        };
        assert_eq!(f32::from(rect.origin.x), 300.0);
        assert_eq!(f32::from(rect.size.width), 900.0);
        assert_eq!(f32::from(rect.size.height), 600.0);
        // 最大化着关的：还是最大化，还原大小保留。
        assert!(matches!(
            restore_on_displays(Some(saved), true, &screens, FILES_MIN),
            Some(WindowBounds::Maximized(_))
        ));
    }

    #[test]
    fn a_window_saved_on_an_unplugged_screen_falls_back_to_the_default() {
        // 上次在左边的副屏（x 为负）上；现在只剩主屏。
        let only_main = [display(0.0, 0.0, 1920.0, 1080.0)];
        let on_second = display(-1500.0, 100.0, 900.0, 600.0);
        assert!(restore_on_displays(Some(on_second), false, &only_main, FILES_MIN).is_none());
        // 副屏还在就照旧。
        let both = [only_main[0], display(-1920.0, 0.0, 1920.0, 1080.0)];
        assert!(restore_on_displays(Some(on_second), false, &both, FILES_MIN).is_some());
        // 分辨率变小：窗口整个落在新屏幕外。
        let shrunk = [display(0.0, 0.0, 1280.0, 720.0)];
        let low = display(1500.0, 900.0, 900.0, 600.0);
        assert!(restore_on_displays(Some(low), false, &shrunk, FILES_MIN).is_none());
        // 没有任何显示器信息（取不到）：不敢照着开。
        assert!(restore_on_displays(Some(on_second), false, &[], FILES_MIN).is_none());
    }

    #[test]
    fn only_a_sliver_on_screen_is_not_enough_to_grab_the_title_bar() {
        let screen = [display(0.0, 0.0, 1920.0, 1080.0)];
        // 标题条只露出 30 像素宽。
        assert!(!on_a_display(display(1890.0, 100.0, 900.0, 600.0), &screen));
        // 露出 200 像素够抓。
        assert!(on_a_display(display(1720.0, 100.0, 900.0, 600.0), &screen));
        // 标题条在屏幕上方外面（y 为负很多）。
        assert!(!on_a_display(display(100.0, -500.0, 900.0, 600.0), &screen));
        // 窗口大半在屏幕下方外面但标题条在屏幕里：能抓，算数。
        assert!(on_a_display(display(100.0, 1000.0, 900.0, 600.0), &screen));
    }

    #[test]
    fn a_saved_size_below_the_window_minimum_is_raised_to_it() {
        let screens = [display(0.0, 0.0, 1920.0, 1080.0)];
        let WindowBounds::Windowed(rect) = restore_on_displays(
            Some(display(100.0, 100.0, 500.0, 330.0)),
            false,
            &screens,
            FILES_MIN,
        )
        .unwrap() else {
            panic!("应当是普通窗口");
        };
        assert_eq!(f32::from(rect.size.width), 640.0);
        assert_eq!(f32::from(rect.size.height), 420.0);
        // 明显坏的值（NaN、超大）直接丢。
        assert!(
            restore_on_displays(
                Some(display(f32::NAN, 0.0, 900.0, 600.0)),
                false,
                &screens,
                FILES_MIN
            )
            .is_none()
        );
        assert!(restore_on_displays(None, false, &screens, FILES_MIN).is_none());
    }

    #[test]
    fn main_and_hosts_windows_use_their_own_slot_and_minimum() {
        let screens = [display(0.0, 0.0, 1920.0, 1080.0)];
        let state = WindowState {
            main: Some(display(100.0, 100.0, 600.0, 400.0)),
            hosts: Some(display(200.0, 150.0, 500.0, 340.0)),
            ..WindowState::default()
        };
        let WindowBounds::Windowed(main) = restore_main(&state, &screens).unwrap() else {
            panic!("主窗口应当是普通窗口");
        };
        // 记住的大小低于主窗口最小尺寸：抬到最小尺寸，位置照旧。
        assert_eq!(f32::from(main.origin.x), 100.0);
        assert_eq!(f32::from(main.size.width), MAIN_MIN.0);
        assert_eq!(f32::from(main.size.height), MAIN_MIN.1);
        let WindowBounds::Windowed(hosts) = restore_hosts(&state, &screens).unwrap() else {
            panic!("主机库窗口应当是普通窗口");
        };
        assert_eq!(f32::from(hosts.origin.x), 200.0);
        assert_eq!(f32::from(hosts.size.width), 500.0);
        assert_eq!(f32::from(hosts.size.height), 340.0);
    }

    #[test]
    fn main_and_hosts_windows_on_an_unplugged_screen_fall_back_but_negative_screens_survive() {
        let state = WindowState {
            main: Some(display(-1500.0, 100.0, 900.0, 600.0)),
            hosts: Some(display(2100.0, 100.0, 820.0, 650.0)),
            ..WindowState::default()
        };
        // 主屏左边有副屏（负坐标）、右边有副屏：两个窗口都在，照着恢复。
        let all = [
            display(0.0, 0.0, 1920.0, 1080.0),
            display(-1920.0, 0.0, 1920.0, 1080.0),
            display(1920.0, 0.0, 1920.0, 1080.0),
        ];
        assert!(restore_main(&state, &all).is_some());
        assert!(restore_hosts(&state, &all).is_some());
        // 拔掉副屏只剩主屏：两个窗口都退回居中。
        let only_main = [all[0]];
        assert!(restore_main(&state, &only_main).is_none());
        assert!(restore_hosts(&state, &only_main).is_none());
        // 只拔右边那块：主窗口（在左副屏）保留，主机库（在右副屏）退回居中。
        let without_right = [all[0], all[1]];
        assert!(restore_main(&state, &without_right).is_some());
        assert!(restore_hosts(&state, &without_right).is_none());
        // 没存过：直接居中。
        assert!(restore_main(&WindowState::default(), &all).is_none());
    }

    #[test]
    fn missing_or_corrupt_state_falls_back() {
        let (_dir_guard, dir) = temp_dir("broken");
        assert_eq!(WindowState::load_from(&dir.join("none.json")), None);
        let path = dir.join("window-state.json");
        std::fs::write(&path, b"{ nope").unwrap();
        assert_eq!(WindowState::load_from(&path), None);
    }
}
