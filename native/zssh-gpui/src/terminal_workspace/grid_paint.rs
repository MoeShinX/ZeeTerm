//! 字形只在自己的终端格子里整形：后备字体、粗体和连字都不能改变下一格的位置。
//! 每段只用一个 canvas；GPUI 缓存单格整形结果，不为每个字符建立布局元素。

use gpui::{IntoElement, SharedString, Styled as _, TextAlign, TextRun, canvas, point, px};
use unicode_width::UnicodeWidthChar as _;
use zssh_terminal::RowSnapshot;

pub(super) struct GridGlyph {
    pub col: usize,
    pub columns: usize,
    pub text: SharedString,
}

pub(super) fn grid_glyph(row: &RowSnapshot, col: usize) -> GridGlyph {
    let cell = &row.cells[col];
    // 行尾放不下宽字时也会有 spacer，不能让它前面的普通字符误占两格。
    let columns =
        if cell.ch.width() == Some(2) && row.cells.get(col + 1).is_some_and(|next| next.spacer) {
            2
        } else {
            1
        };
    let text = if cell.hidden || (cell.ch == ' ' && cell.zerowidth.is_none()) {
        SharedString::from(" ")
    } else {
        let mut text = String::from(cell.ch);
        if let Some(marks) = &cell.zerowidth {
            text.extend(marks.iter());
        }
        text.into()
    };
    GridGlyph { col, columns, text }
}

pub(super) fn glyphs(
    glyphs: Vec<GridGlyph>,
    start_col: usize,
    cell_width: f32,
    font_size: f32,
    row_height: f32,
) -> impl IntoElement {
    canvas(
        move |_, window, _| {
            let style = window.text_style();
            let font = style.font();
            glyphs
                .into_iter()
                .filter(|glyph| !glyph.text.trim().is_empty())
                .map(|glyph| {
                    let run = TextRun {
                        len: glyph.text.len(),
                        font: font.clone(),
                        color: style.color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    let line =
                        window
                            .text_system()
                            .shape_line(glyph.text, px(font_size), &[run], None);
                    (px((glyph.col - start_col) as f32 * cell_width), line)
                })
                .collect::<Vec<_>>()
        },
        move |bounds, lines, window, cx| {
            for (x, line) in lines {
                let _ = line.paint(
                    bounds.origin + point(x, px(0.0)),
                    px(row_height),
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
        },
    )
    .size_full()
}

#[cfg(test)]
mod tests {
    use super::grid_glyph;
    use zssh_terminal::{Size, TerminalModel};

    #[test]
    fn chinese_combining_marks_and_emoji_keep_the_models_columns() {
        let mut model = TerminalModel::new(Size { rows: 3, cols: 30 }, 100);
        model.feed("中e\u{301}😀x".as_bytes());
        let snapshot = model.snapshot();
        let row = &snapshot.rows[0];
        let glyphs: Vec<_> = (0..6)
            .filter(|&col| !row.cells[col].spacer)
            .map(|col| {
                let glyph = grid_glyph(row, col);
                (glyph.text.to_string(), glyph.col, glyph.columns)
            })
            .collect();
        assert_eq!(
            glyphs,
            vec![
                ("中".into(), 0, 2),
                ("e\u{301}".into(), 2, 1),
                ("😀".into(), 3, 2),
                ("x".into(), 5, 1)
            ]
        );
    }

    #[test]
    fn a_wrapped_wide_character_does_not_widen_the_ascii_before_its_spacer() {
        let mut model = TerminalModel::new(Size { rows: 3, cols: 3 }, 100);
        model.feed("ab界".as_bytes());
        let snapshot = model.snapshot();
        assert!(snapshot.rows[0].cells[2].spacer);
        assert_eq!(grid_glyph(&snapshot.rows[0], 1).columns, 1);
        assert_eq!(grid_glyph(&snapshot.rows[1], 0).columns, 2);
    }

    #[test]
    fn concealed_wide_text_keeps_its_two_cells_without_drawing_the_secret() {
        let mut model = TerminalModel::new(Size { rows: 3, cols: 10 }, 100);
        model.feed("\x1b[8m界\x1b[0mx".as_bytes());
        let snapshot = model.snapshot();
        let hidden = grid_glyph(&snapshot.rows[0], 0);
        assert_eq!(hidden.text.as_ref(), " ");
        assert_eq!(hidden.columns, 2);
        assert_eq!(grid_glyph(&snapshot.rows[0], 2).text.as_ref(), "x");
    }

    #[test]
    fn styled_runs_preserve_the_url_start_after_a_chinese_prefix() {
        let url = "https://example.com/r/Case09XY";
        let text = format!("\x1b[1m测试完成，\x1b[3m点击查看你的结果链接：\x1b[0m\x1b[34m{url}");
        let mut model = TerminalModel::new(Size { rows: 3, cols: 100 }, 100);
        model.feed(text.as_bytes());
        let snapshot = model.snapshot();
        let row = &snapshot.rows[0];
        let foreground = gpui::rgb(0x111111).into();
        let background = gpui::rgb(0xffffff).into();
        let runs = super::super::runs(
            row,
            foreground,
            background,
            None,
            0,
            None,
            &[],
            &[],
            &[0; 16],
        );
        let glyphs: Vec<_> = runs.iter().flat_map(|run| run.glyphs.iter()).collect();
        let start = glyphs
            .iter()
            .find(|glyph| glyph.col == 32)
            .expect("URL start");
        assert_eq!(start.text.as_ref(), "h");
        assert_eq!(model.text_between((0, 32), (0, 32 + url.len() - 1)), url);
        for run in runs {
            let last = run.glyphs.last().expect("nonempty run");
            assert_eq!(run.col + run.columns, last.col + last.columns);
        }
    }
}
