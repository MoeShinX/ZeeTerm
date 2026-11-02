//! 右侧面板的宽度规则（0.2.14 `ui/src/state/panelWidths.ts`）：纯函数，不依赖 GPUI。
//!
//! - 主机编辑 / 凭据 / 代理 / 转发这些侧栏（aside）默认 380、最小 320、最大窗口宽度的 50%；
//!   AI 面板默认 400、最大 60%；SFTP 停靠面板默认 420、最大 60%；
//! - 终端区域至少保留 360px：窗口缩小时面板跟着收窄，不能把终端挤没；
//! - 双击把手恢复默认宽度；宽度存进设置表（`layout.*`，本机布局，不同步）。
//!
//! G07c 的 AI / 快捷命令 / 文件面板也走这里，所以按「种类」参数化，不是只给 aside 用。

/// 主机编辑侧栏的默认宽度。
pub const ASIDE_DEFAULT: f32 = 380.0;
/// AI 面板的默认宽度。
pub const AI_DEFAULT: f32 = 400.0;
/// SFTP 停靠面板的默认宽度。
pub const SFTP_DEFAULT: f32 = 420.0;
/// 面板的最小宽度。
pub const PANEL_MIN: f32 = 320.0;
/// 终端区域至少保留的宽度。
pub const TERMINAL_MIN: f32 = 360.0;
const ASIDE_MAX_RATIO: f32 = 0.5;
const AI_MAX_RATIO: f32 = 0.6;
const SFTP_MAX_RATIO: f32 = 0.6;

/// 可调宽的右侧面板种类。`Ai` / `Sftp` 是 G07c 的 AI 面板和文件停靠面板用的，这一卡只有 `Aside`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum PanelKind {
    Aside,
    Ai,
    Sftp,
}

impl PanelKind {
    pub fn default_width(self) -> f32 {
        match self {
            Self::Aside => ASIDE_DEFAULT,
            Self::Ai => AI_DEFAULT,
            Self::Sftp => SFTP_DEFAULT,
        }
    }

    fn max_ratio(self) -> f32 {
        match self {
            Self::Aside => ASIDE_MAX_RATIO,
            Self::Ai => AI_MAX_RATIO,
            Self::Sftp => SFTP_MAX_RATIO,
        }
    }

    /// 宽度存在设置表里的键（和 0.2.14 一致）。
    pub fn setting_key(self) -> &'static str {
        match self {
            Self::Aside => "layout.aside_panel_width",
            Self::Ai => "layout.ai_panel_width",
            Self::Sftp => "layout.sftp_panel_width",
        }
    }
}

/// 宽度的上下限（跟随窗口宽度）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub min: f32,
    pub max: f32,
}

/// 上限 = min(比例上限, 窗口宽 - 终端最少保留 - 同屏另一个面板占的宽度)；窗口很小时先保证
/// 终端，面板可以低于 320（两个下限取更小的那个，保证 min <= max）。
pub fn bounds(kind: PanelKind, window_width: f32, reserved: f32) -> Bounds {
    let available = (window_width - TERMINAL_MIN - reserved.max(0.0))
        .floor()
        .max(0.0);
    let min = PANEL_MIN.min(available);
    let max = (window_width * kind.max_ratio())
        .min(available)
        .floor()
        .max(min);
    Bounds { min, max }
}

/// 把想要的宽度夹进合法区间。
pub fn clamp(kind: PanelKind, desired: f32, window_width: f32, reserved: f32) -> f32 {
    let Bounds { min, max } = bounds(kind, window_width, reserved);
    if !desired.is_finite() {
        return min;
    }
    desired.round().clamp(min, max)
}

/// 读回来的存量宽度做体检：不是有限数、负数、超大值一律回退到默认值。
pub fn sanitize_stored(raw: Option<f32>, fallback: f32) -> f32 {
    match raw {
        Some(value) if value.is_finite() && value > 0.0 && value <= 20_000.0 => value.round(),
        _ => fallback,
    }
}

/// 把设置表里的 JSON 数字读成宽度。
pub fn parse_stored(raw: Option<&str>) -> Option<f32> {
    raw.and_then(|text| serde_json::from_str::<serde_json::Value>(text.trim()).ok())
        .and_then(|value| value.as_f64())
        .map(|value| value as f32)
}

/// 左边缘的拖动把手：按下时记下指针和宽度，拖动时宽度 = 起始宽度 - 指针移动量（往左拖变宽）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DragStart {
    pub pointer_x: f32,
    pub width: f32,
}

pub fn dragged_width(
    kind: PanelKind,
    start: DragStart,
    pointer_x: f32,
    window_width: f32,
    reserved: f32,
) -> f32 {
    clamp(
        kind,
        start.width - (pointer_x - start.pointer_x),
        window_width,
        reserved,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aside_is_between_320_and_half_the_window_and_leaves_room_for_the_terminal() {
        let b = bounds(PanelKind::Aside, 1200.0, 0.0);
        assert_eq!(b.min, 320.0);
        assert_eq!(b.max, 600.0, "50% of 1200，终端还剩 600 > 360");
        // 窗口 800：50% = 400，但终端要留 360，只剩 440 可用 -> 400。
        assert_eq!(bounds(PanelKind::Aside, 800.0, 0.0).max, 400.0);
        // 窗口 720：可用 360，最大 = min(360, 360) = 360。
        assert_eq!(bounds(PanelKind::Aside, 720.0, 0.0).max, 360.0);
    }

    #[test]
    fn ai_and_sftp_may_take_sixty_percent_and_yield_to_each_other() {
        assert_eq!(bounds(PanelKind::Ai, 1500.0, 0.0).max, 900.0);
        assert_eq!(bounds(PanelKind::Sftp, 1500.0, 0.0).max, 900.0);
        // 同屏另一个面板占了 500：可用 = 1500 - 360 - 500 = 640。
        assert_eq!(bounds(PanelKind::Ai, 1500.0, 500.0).max, 640.0);
    }

    #[test]
    fn a_tiny_window_never_makes_an_impossible_range() {
        let b = bounds(PanelKind::Aside, 400.0, 0.0);
        assert!(b.min <= b.max, "{b:?}");
        assert_eq!(b.min, 40.0, "只剩 40 可用，下限跟着降");
        let zero = bounds(PanelKind::Aside, 100.0, 0.0);
        assert_eq!((zero.min, zero.max), (0.0, 0.0));
        let reserved = bounds(PanelKind::Ai, 900.0, 2000.0);
        assert!(reserved.min <= reserved.max);
    }

    #[test]
    fn clamping_rounds_and_survives_garbage() {
        assert_eq!(clamp(PanelKind::Aside, 100.0, 1200.0, 0.0), 320.0);
        assert_eq!(clamp(PanelKind::Aside, 9999.0, 1200.0, 0.0), 600.0);
        assert_eq!(clamp(PanelKind::Aside, 401.6, 1200.0, 0.0), 402.0);
        assert_eq!(clamp(PanelKind::Aside, f32::NAN, 1200.0, 0.0), 320.0);
        assert_eq!(clamp(PanelKind::Aside, f32::INFINITY, 1200.0, 0.0), 320.0);
    }

    #[test]
    fn stored_widths_are_sanity_checked() {
        assert_eq!(sanitize_stored(Some(412.4), 380.0), 412.0);
        for bad in [Some(f32::NAN), Some(-5.0), Some(0.0), Some(30_000.0), None] {
            assert_eq!(sanitize_stored(bad, 380.0), 380.0, "{bad:?}");
        }
        assert_eq!(parse_stored(Some("412")), Some(412.0));
        assert_eq!(parse_stored(Some("\"wide\"")), None);
        assert_eq!(parse_stored(Some("not json")), None);
        assert_eq!(parse_stored(None), None);
    }

    #[test]
    fn dragging_the_left_edge_left_makes_the_panel_wider() {
        let start = DragStart {
            pointer_x: 900.0,
            width: 380.0,
        };
        assert_eq!(
            dragged_width(PanelKind::Aside, start, 800.0, 1200.0, 0.0),
            480.0
        );
        assert_eq!(
            dragged_width(PanelKind::Aside, start, 1000.0, 1200.0, 0.0),
            320.0,
            "往右拖到下限停住"
        );
        assert_eq!(
            dragged_width(PanelKind::Aside, start, 0.0, 1200.0, 0.0),
            600.0,
            "拖到上限停住"
        );
    }

    #[test]
    fn each_kind_has_its_own_default_and_setting_key() {
        assert_eq!(PanelKind::Aside.default_width(), 380.0);
        assert_eq!(PanelKind::Ai.default_width(), 400.0);
        assert_eq!(PanelKind::Sftp.default_width(), 420.0);
        assert_eq!(PanelKind::Aside.setting_key(), "layout.aside_panel_width");
        assert_ne!(PanelKind::Ai.setting_key(), PanelKind::Sftp.setting_key());
    }
}
