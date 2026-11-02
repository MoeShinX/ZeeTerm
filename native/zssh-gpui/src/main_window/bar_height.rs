//! 终端底部两条栏和 AI 输入框的高度规则（0.2.14 的 `quickBarHeight.ts`、`ai/inputHeight.ts`）：
//! 纯函数，不依赖 GPUI。高度和别的布局一样存进设置表（`layout.quick_bar_height`、
//! `layout.compose_bar_height`、`layout.ai_input_height`，本机布局、不同步）。

/// 快捷命令栏的一行高度（含上边框 1px），也是最小值和双击恢复的值。
pub const QUICK_DEFAULT: f32 = 28.0;
/// 多行时每一行占的高度：按钮 20px + 上下各 2px 外边距。
pub const QUICK_ROW: f32 = 24.0;
/// 最多显示的行数（超过的在栏内纵向滚动）。
pub const QUICK_MAX_ROWS: f32 = 5.0;
/// 最大高度不超过终端区域高度的这个比例。
pub const QUICK_MAX_RATIO: f32 = 0.4;
/// 存量值的硬顶：再大的当脏数据。
const STORED_HARD_MAX: f32 = 20_000.0;

/// 当前终端区域高度下允许的最大高度：min(5 行, 区域的 40%)，至少是默认高度。
pub fn quick_max(area: f32) -> f32 {
    let rows_max = QUICK_DEFAULT + (QUICK_MAX_ROWS - 1.0) * QUICK_ROW;
    if !area.is_finite() || area <= 0.0 {
        return rows_max;
    }
    QUICK_DEFAULT.max(rows_max.min((area * QUICK_MAX_RATIO).floor()))
}

pub fn clamp_quick(desired: f32, area: f32) -> f32 {
    if !desired.is_finite() {
        return QUICK_DEFAULT;
    }
    desired.round().max(QUICK_DEFAULT).min(quick_max(area))
}

/// 拖动中的高度：拖动条在栏的上沿，往上拖（y 变小）栏变高。
pub fn drag_quick(start_height: f32, start_y: f32, y: f32, area: f32) -> f32 {
    clamp_quick(start_height + (start_y - y), area)
}

/// 读回来的存量高度做体检：不是有限数、比默认小、大得离谱的一律回到默认。
pub fn sanitize_quick(raw: Option<f32>) -> f32 {
    match raw {
        Some(v) if v.is_finite() && (QUICK_DEFAULT..=STORED_HARD_MAX).contains(&v) => v.round(),
        _ => QUICK_DEFAULT,
    }
}

// ---- 撰写栏 ----

/// 输入框一行的内容高度（`leading-[18px]`），最多长到这么多行，再多在框内滚动。
pub const COMPOSE_LINE: f32 = 18.0;
pub const COMPOSE_MAX_ROWS: f32 = 5.0;
/// 输入框上下内边距加边框，一共多出来的高度。
pub const COMPOSE_INPUT_CHROME: f32 = 6.0;
/// 自动增高时输入框高度不超过终端区域高度的这个比例。
pub const COMPOSE_AUTO_RATIO: f32 = 0.3;
pub const COMPOSE_INPUT_MIN: f32 = COMPOSE_LINE + COMPOSE_INPUT_CHROME;
/// 栏比输入框多出来的高度：上边框 1px + 上下内边距各 2px。
pub const COMPOSE_BAR_CHROME: f32 = 5.0;
/// 撰写栏只有一行时的高度，也是固定高度的最小值。
pub const COMPOSE_BAR_MIN: f32 = COMPOSE_INPUT_MIN + COMPOSE_BAR_CHROME;
/// 固定高度最大不超过终端区域高度的这个比例（比自动增高宽松，这是「自由调」）。
pub const COMPOSE_BAR_MAX_RATIO: f32 = 0.6;
/// 设置值 0 = 自动增高（默认）。
pub const COMPOSE_AUTO: f32 = 0.0;
const COMPOSE_FALLBACK_MAX: f32 = 480.0;

/// 自动增高时输入框最多长到多高：min(5 行, 区域的 30%)，至少一行。
pub fn compose_input_max(area: f32) -> f32 {
    let rows_max = COMPOSE_LINE * COMPOSE_MAX_ROWS + COMPOSE_INPUT_CHROME;
    if !area.is_finite() || area <= 0.0 {
        return rows_max;
    }
    COMPOSE_INPUT_MIN.max(rows_max.min((area * COMPOSE_AUTO_RATIO).floor()))
}

/// 按内容行数算自动模式下输入框的高度（`lines` 至少 1 行）。
pub fn compose_auto_input_height(lines: usize, area: f32) -> f32 {
    let natural = COMPOSE_LINE * lines.max(1) as f32 + COMPOSE_INPUT_CHROME;
    natural.min(compose_input_max(area))
}

pub fn compose_bar_max(area: f32) -> f32 {
    if !area.is_finite() || area <= 0.0 {
        return COMPOSE_FALLBACK_MAX;
    }
    COMPOSE_BAR_MIN.max((area * COMPOSE_BAR_MAX_RATIO).floor())
}

pub fn clamp_compose(desired: f32, area: f32) -> f32 {
    if !desired.is_finite() {
        return COMPOSE_BAR_MIN;
    }
    desired
        .round()
        .max(COMPOSE_BAR_MIN)
        .min(compose_bar_max(area))
}

pub fn drag_compose(start_height: f32, start_y: f32, y: f32, area: f32) -> f32 {
    clamp_compose(start_height + (start_y - y), area)
}

/// 读回来的存量值做体检：不是有限数、负数、0、小于一行的都当 0（自动）；其余取整。
pub fn sanitize_compose(raw: Option<f32>) -> f32 {
    match raw {
        Some(v) if v.is_finite() && v >= COMPOSE_BAR_MIN => v.round(),
        _ => COMPOSE_AUTO,
    }
}

// ---- AI 输入框 ----

/// AI 输入框的默认（也是最小）高度，约三行。
pub const AI_INPUT_DEFAULT: f32 = 75.0;

/// 当前面板高度下允许的最大高度（面板高度的一半，至少是默认高度）。
pub fn ai_input_max(panel_height: f32) -> f32 {
    if !panel_height.is_finite() || panel_height <= 0.0 {
        return AI_INPUT_DEFAULT;
    }
    AI_INPUT_DEFAULT.max((panel_height / 2.0).floor())
}

pub fn clamp_ai_input(desired: f32, panel_height: f32) -> f32 {
    if !desired.is_finite() {
        return AI_INPUT_DEFAULT;
    }
    desired
        .round()
        .max(AI_INPUT_DEFAULT)
        .min(ai_input_max(panel_height))
}

pub fn drag_ai_input(start_height: f32, start_y: f32, y: f32, panel_height: f32) -> f32 {
    clamp_ai_input(start_height + (start_y - y), panel_height)
}

pub fn sanitize_ai_input(raw: Option<f32>) -> f32 {
    match raw {
        Some(v) if v.is_finite() && (AI_INPUT_DEFAULT..=STORED_HARD_MAX).contains(&v) => v.round(),
        _ => AI_INPUT_DEFAULT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_bar_is_one_row_up_to_five_rows_or_forty_percent() {
        assert_eq!(quick_max(0.0), 124.0, "量不到区域时按 5 行");
        assert_eq!(quick_max(1000.0), 124.0);
        assert_eq!(quick_max(200.0), 80.0, "区域的 40%");
        assert_eq!(quick_max(40.0), QUICK_DEFAULT, "再矮也至少一行");
        assert_eq!(clamp_quick(5.0, 1000.0), 28.0);
        assert_eq!(clamp_quick(500.0, 1000.0), 124.0);
        assert_eq!(clamp_quick(f32::NAN, 1000.0), 28.0);
    }

    #[test]
    fn dragging_the_top_edge_up_makes_the_bar_taller() {
        assert_eq!(drag_quick(28.0, 500.0, 452.0, 1000.0), 76.0);
        assert_eq!(drag_quick(76.0, 500.0, 600.0, 1000.0), 28.0, "往下拖到下限");
    }

    #[test]
    fn stored_quick_height_is_sanity_checked() {
        assert_eq!(sanitize_quick(Some(60.4)), 60.0);
        for bad in [None, Some(10.0), Some(f32::NAN), Some(99_999.0)] {
            assert_eq!(sanitize_quick(bad), QUICK_DEFAULT, "{bad:?}");
        }
    }

    #[test]
    fn compose_auto_grows_to_five_rows_then_scrolls() {
        assert_eq!(compose_auto_input_height(1, 1000.0), 24.0);
        assert_eq!(compose_auto_input_height(3, 1000.0), 60.0);
        assert_eq!(compose_auto_input_height(9, 1000.0), 96.0, "5 行封顶");
        assert_eq!(compose_auto_input_height(9, 200.0), 60.0, "区域 30% 更紧");
        assert_eq!(compose_auto_input_height(0, 1000.0), 24.0);
    }

    #[test]
    fn compose_fixed_height_is_free_up_to_sixty_percent() {
        assert_eq!(compose_bar_max(1000.0), 600.0);
        assert_eq!(compose_bar_max(0.0), 480.0);
        assert_eq!(clamp_compose(5.0, 1000.0), COMPOSE_BAR_MIN);
        assert_eq!(clamp_compose(5000.0, 1000.0), 600.0);
        assert_eq!(drag_compose(COMPOSE_BAR_MIN, 500.0, 400.0, 1000.0), 129.0);
    }

    #[test]
    fn stored_compose_height_zero_means_auto() {
        assert_eq!(sanitize_compose(Some(0.0)), COMPOSE_AUTO);
        assert_eq!(sanitize_compose(Some(-4.0)), COMPOSE_AUTO);
        assert_eq!(sanitize_compose(Some(COMPOSE_BAR_MIN - 1.0)), COMPOSE_AUTO);
        assert_eq!(sanitize_compose(Some(120.6)), 121.0);
        assert_eq!(sanitize_compose(None), COMPOSE_AUTO);
        assert_eq!(sanitize_compose(Some(f32::NAN)), COMPOSE_AUTO);
    }

    #[test]
    fn ai_input_is_between_default_and_half_the_panel() {
        assert_eq!(ai_input_max(0.0), 75.0);
        assert_eq!(ai_input_max(600.0), 300.0);
        assert_eq!(ai_input_max(100.0), 75.0, "面板很矮也至少默认高度");
        assert_eq!(clamp_ai_input(10.0, 600.0), 75.0);
        assert_eq!(clamp_ai_input(900.0, 600.0), 300.0);
        assert_eq!(drag_ai_input(75.0, 500.0, 400.0, 800.0), 175.0);
        assert_eq!(sanitize_ai_input(Some(50.0)), 75.0, "旧版存的 50 按默认");
        assert_eq!(sanitize_ai_input(Some(180.0)), 180.0);
    }
}
