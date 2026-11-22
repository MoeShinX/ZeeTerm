//! 终端窗格左侧可选的行号 / 时间戳栏（V2-U49）：栏宽怎么从文字区里扣、栏里写什么、怎么画。
//!
//! 栏宽永远是整数格（按一个字符格的宽度算），所以「栏开了列数少几列」是精确的整数减法，
//! 不会因为小数像素来回改变列数。栏里的数据来自终端模型（`zeeterm_terminal::TerminalModel::gutter_rows`），
//! 不在 Snapshot 里：两个开关都关着时这里一行都不会执行，终端几何和以前逐位一致。

use super::grid_paint::{GridGlyph, glyphs};
use super::*;
use zeeterm_terminal::GutterRow;

/// 行号最多留这么多位数字（超过 999999 的行号显示低 6 位）。
pub(super) const NUMBER_DIGITS: usize = 6;
/// 时间戳固定 `HH:MM:SS` 八个字符。
pub(super) const TIME_CHARS: usize = 8;
/// 文字区最少的列数：和没有栏时的下限一致（`render_in_area` 原来的 `.max(20)`）。
pub(super) const MIN_COLS: usize = 20;

/// 栏占几个字符格：各栏的字符数，栏与栏之间空一格，最后再留一格和文字区隔开（分隔线画在这一格里）。
/// 两个开关都关是 0。
pub(super) fn gutter_cells(line_numbers: bool, timestamps: bool) -> usize {
    let mut content = 0;
    let mut parts = 0;
    if timestamps {
        content += TIME_CHARS;
        parts += 1;
    }
    if line_numbers {
        content += NUMBER_DIGITS;
        parts += 1;
    }
    if parts == 0 {
        return 0;
    }
    content + (parts - 1) + 1
}

/// 按窗格内宽（扣掉四周内边距之后）算 `(文字区列数, 实际用上的栏宽格数)`。
///
/// 栏没开（`gutter == 0`）时这就是原来的 `floor(宽 / 格宽).max(20)`，一个字不差。
/// 窗格窄到扣掉栏之后不足 [`MIN_COLS`] 列时这个窗格不画栏（栏宽 0）：宁可没有栏，也不能把终端
/// 挤到比最小列数还窄、文字伸到窗格外面。
pub(super) fn split_columns(inner_w: f32, cell_width: f32, gutter: usize) -> (usize, usize) {
    let raw = (inner_w / cell_width).floor() as usize;
    if gutter > 0 && raw >= MIN_COLS + gutter {
        (raw - gutter, gutter)
    } else {
        (raw.max(MIN_COLS), 0)
    }
}

/// Unix 秒 → 本地 `HH:MM:SS`（`utc_offset` 是本机时区相对 UTC 的秒数）。
pub(super) fn format_time(unix_secs: i64, utc_offset: i64) -> String {
    let day_secs = unix_secs.saturating_add(utc_offset).rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02}",
        day_secs / 3600,
        day_secs % 3600 / 60,
        day_secs % 60
    )
}

/// 行号右对齐到 [`NUMBER_DIGITS`] 位；更大的行号只显示低 6 位（栏宽固定，不为大数字变宽）。
pub(super) fn format_number(number: u64) -> String {
    let limit = 10u64.pow(NUMBER_DIGITS as u32);
    format!("{:>width$}", number % limit, width = NUMBER_DIGITS)
}

/// 栏里一行的两段文字：`(时间戳, 行号)`，没开的那一栏是 `None`，没有内容的行是等宽空白。
pub(super) fn row_parts(
    row: &GutterRow,
    line_numbers: bool,
    timestamps: bool,
    utc_offset: i64,
) -> (Option<String>, Option<String>) {
    let time = timestamps.then(|| match row.time {
        Some(secs) => format_time(secs, utc_offset),
        None => " ".repeat(TIME_CHARS),
    });
    let number = line_numbers.then(|| match row.number {
        Some(n) => format_number(n),
        None => " ".repeat(NUMBER_DIGITS),
    });
    (time, number)
}

/// 画一个窗格的栏：一列和文字区同高同行距的小格子，淡一档的前景色，右边一条细分隔线。
#[allow(clippy::too_many_arguments)]
pub(super) fn gutter_column(
    rows: &[GutterRow],
    line_numbers: bool,
    timestamps: bool,
    cells: usize,
    cell_width: f32,
    row_height: f32,
    font_size: f32,
    mono: &::gpui::SharedString,
    foreground: Hsla,
    utc_offset: i64,
) -> AnyElement {
    let text_color = foreground.alpha(0.45);
    let line_color = foreground.alpha(0.2);
    let number_col = if timestamps { TIME_CHARS + 1 } else { 0 };
    div()
        .relative()
        .flex_shrink_0()
        .h_full()
        .w(::gpui::px(cells as f32 * cell_width))
        .overflow_hidden()
        .font_family(mono.clone())
        .text_size(::gpui::px(font_size))
        .text_color(text_color)
        .children(rows.iter().map(|row| {
            let (time, number) = row_parts(row, line_numbers, timestamps, utc_offset);
            let mut parts: Vec<GridGlyph> = Vec::new();
            if let Some(text) = time.filter(|text| !text.trim().is_empty()) {
                parts.push(GridGlyph {
                    col: 0,
                    columns: TIME_CHARS,
                    text: text.into(),
                });
            }
            if let Some(text) = number.filter(|text| !text.trim().is_empty()) {
                parts.push(GridGlyph {
                    col: number_col,
                    columns: NUMBER_DIGITS,
                    text: text.into(),
                });
            }
            div()
                .relative()
                .h(::gpui::px(row_height))
                .w_full()
                .flex_shrink_0()
                .child(glyphs(parts, 0, cell_width, font_size, row_height))
        }))
        // 分隔线：画在最后一格（文字区前的间隔格）正中，贯穿整列。
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(::gpui::px((cells as f32 - 0.5) * cell_width))
                .w(::gpui::px(1.0))
                .bg(line_color),
        )
        .into_any_element()
}

impl TerminalWorkspace {
    /// 光标所在格子的左上角，相对窗格的左上角（输入法预编辑文字、命令建议浮层的锚点都用它）。
    /// 行号 / 时间戳栏把文字区往右推；没开栏时栏宽是 0.0，结果和以前的 `PANE_PAD + 列 × 格宽` 逐位一致。
    pub(super) fn cursor_origin_in_pane(&self, index: usize, snapshot: &Snapshot) -> (f32, f32) {
        let gutter = self.pane_geom.get(index).map_or(0.0, |geom| geom.gutter);
        (
            PANE_PAD + gutter + snapshot.cursor_col as f32 * self.cell_width,
            PANE_PAD + snapshot.cursor_row as f32 * self.cell_height,
        )
    }
}
