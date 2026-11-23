//! 字形只在自己的终端格子里整形：后备字体、粗体和连字都不能改变下一格的位置。
//! 每段只用一个 canvas；GPUI 缓存单格整形结果，不为每个字符建立布局元素。

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use gpui::{
    Bounds, FontFeatures, FontId, FontStyle, FontWeight, GlyphId, Hsla, IntoElement, Pixels,
    ShapedLine, SharedString, Styled as _, TextAlign, TextRun, canvas, fill, point, px, size,
};
use unicode_width::UnicodeWidthChar as _;
use zeeterm_terminal::{RowSnapshot, Snapshot};

use crate::highlight::Span;
use crate::links::LinkSpan;
use crate::settings::CursorShape;

pub(super) struct GridGlyph {
    pub col: usize,
    pub columns: usize,
    pub text: SharedString,
}

/// 常见可打印 ASCII（0x20..=0x7E）直接引用静态切片，避免每帧逐格堆分配 `String` + `Arc<str>`。
const ASCII_PRINTABLE: &str = " !\"#$%&'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~";

fn ascii_shared(ch: char) -> Option<SharedString> {
    if ch.is_ascii() {
        let code = ch as u8;
        if (0x20..=0x7e).contains(&code) {
            let ix = usize::from(code - 0x20);
            return Some(SharedString::new_static(&ASCII_PRINTABLE[ix..ix + 1]));
        }
    }
    None
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
        SharedString::new_static(" ")
    } else if cell.zerowidth.is_none()
        && let Some(shared) = ascii_shared(cell.ch)
    {
        shared
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

/// 单个窗格的网格排版缓存：内容、字体、配色与高亮不变时（拖选、光标闪烁、悬停），
/// 整屏行布局与单格 `ShapedLine` 直接复用，不再每帧重跑 `runs()` 和上万次 `shape_line()`。
#[derive(Default)]
pub(super) struct PanePaintCache {
    key: Option<PaintCacheKey>,
    glyph_cache_key: Option<(SharedString, u32)>,
    glyphs: HashMap<GlyphKey, Rc<ShapedLine>>,
    rows: Vec<CachedRow>,
}

const MAX_GLYPH_CACHE: usize = 4096;

#[derive(Clone, PartialEq, Eq)]
struct PaintCacheKey {
    revision: u64,
    mono: SharedString,
    font_size_bits: u32,
    cell_width_bits: u32,
    row_height_bits: u32,
    foreground: u32,
    background: u32,
    ansi: [u32; 16],
    highlights_hash: u64,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct GlyphKey {
    text: SharedString,
    bold: bool,
    italic: bool,
    color: u32,
}

struct BgRun {
    col: usize,
    columns: usize,
    color: Hsla,
}

struct DecoRun {
    col: usize,
    columns: usize,
    color: Hsla,
}

/// 预先烘焙好的单个字形：绘制阶段直接调 `window.paint_glyph` / `paint_emoji`，
/// 跳过 `ShapedLine::paint` 每次都会开启的 `window.paint_layer` 与装饰迭代开销（中英混排同样 ~3ms）。
struct BakedGlyph {
    dx: Pixels,
    dy: Pixels,
    font_id: FontId,
    glyph_id: GlyphId,
    color: Hsla,
    is_emoji: bool,
}

struct CachedRow {
    cols: usize,
    bg_runs: Vec<BgRun>,
    baked: Vec<BakedGlyph>,
    underlines: Vec<DecoRun>,
    strikethroughs: Vec<DecoRun>,
}

fn hsla_bits(c: Hsla) -> u32 {
    let rgba = c.to_rgb();
    let r = (rgba.r.clamp(0.0, 1.0) * 255.0).round() as u32;
    let g = (rgba.g.clamp(0.0, 1.0) * 255.0).round() as u32;
    let b = (rgba.b.clamp(0.0, 1.0) * 255.0).round() as u32;
    let a = (rgba.a.clamp(0.0, 1.0) * 255.0).round() as u32;
    (r << 24) | (g << 16) | (b << 8) | a
}

fn hash_highlights(highlights: &[Vec<Span>]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    highlights.len().hash(&mut hasher);
    for (row_ix, row) in highlights.iter().enumerate() {
        if row.is_empty() {
            continue;
        }
        row_ix.hash(&mut hasher);
        row.len().hash(&mut hasher);
        for span in row {
            span.start.hash(&mut hasher);
            span.end.hash(&mut hasher);
            span.fg.hash(&mut hasher);
            span.bg.hash(&mut hasher);
        }
    }
    hasher.finish()
}

pub(super) struct GridPaintArgs {
    pub snapshot: Rc<Snapshot>,
    pub plain_links: Rc<Vec<Vec<LinkSpan>>>,
    pub highlights: Vec<Vec<Span>>,
    pub cache: Rc<RefCell<PanePaintCache>>,
    pub selection: Option<((usize, usize), (usize, usize))>,
    pub cursor: Option<(usize, usize)>,
    pub cursor_shape: CursorShape,
    pub mono: SharedString,
    pub font_size: f32,
    pub cell_width: f32,
    pub row_height: f32,
    pub foreground: Hsla,
    pub background: Hsla,
    pub selection_color: Hsla,
    pub cursor_color: Hsla,
    pub ansi: [u32; 16],
}

struct CursorPaint {
    row: usize,
    col: usize,
    columns: usize,
    shape: CursorShape,
    block_glyph: Option<Rc<ShapedLine>>,
    underline: bool,
    strikethrough: bool,
}

#[derive(Clone, Copy)]
struct GlyphStyle {
    bold: bool,
    italic: bool,
    color: Hsla,
}

fn terminal_base_font(window: &gpui::Window, mono: &SharedString) -> gpui::Font {
    let mut font = window.text_style().font();
    // `canvas` 自身上的 `.font_family(mono)` 不会在 prepaint 阶段写入 `window.text_style()`，
    // 必须显式采用用户选的终端字体，同时保留窗口根元素注入的中文后备字体栈；
    // 禁用连字让正文与块光标解析到同一套 `FontId` 与等宽字形度量。
    font.family = mono.clone();
    font.features = FontFeatures::disable_ligatures();
    font
}

impl PanePaintCache {
    fn shape_cached(
        &mut self,
        window: &gpui::Window,
        base_font: &gpui::Font,
        font_size: f32,
        text: SharedString,
        style: GlyphStyle,
    ) -> Rc<ShapedLine> {
        let key = GlyphKey {
            text: text.clone(),
            bold: style.bold,
            italic: style.italic,
            color: hsla_bits(style.color),
        };
        if let Some(cached) = self.glyphs.get(&key) {
            return Rc::clone(cached);
        }
        if self.glyphs.len() >= MAX_GLYPH_CACHE {
            self.glyphs.clear();
        }
        let mut font = base_font.clone();
        font.weight = if style.bold {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        };
        font.style = if style.italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        };
        let run = TextRun {
            len: text.len(),
            font,
            color: style.color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let line = Rc::new(
            window
                .text_system()
                .shape_line(text, px(font_size), &[run], None),
        );
        self.glyphs.insert(key, Rc::clone(&line));
        line
    }

    fn ensure_rows(&mut self, args: &GridPaintArgs, window: &gpui::Window) {
        let key = PaintCacheKey {
            revision: args.snapshot.revision,
            mono: args.mono.clone(),
            font_size_bits: args.font_size.to_bits(),
            cell_width_bits: args.cell_width.to_bits(),
            row_height_bits: args.row_height.to_bits(),
            foreground: hsla_bits(args.foreground),
            background: hsla_bits(args.background),
            ansi: args.ansi,
            highlights_hash: hash_highlights(&args.highlights),
        };
        if self.key.as_ref() == Some(&key) {
            return;
        }
        let glyph_key = (args.mono.clone(), args.font_size.to_bits());
        if self.glyph_cache_key.as_ref() != Some(&glyph_key) {
            self.glyphs.clear();
            self.glyph_cache_key = Some(glyph_key);
        }
        let base_font = terminal_base_font(window, &args.mono);
        let row_h = px(args.row_height);
        let mut rows = Vec::with_capacity(args.snapshot.rows.len());
        for (row_ix, row) in args.snapshot.rows.iter().enumerate() {
            let plain_links = args
                .plain_links
                .get(row_ix)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let row_spans = args
                .highlights
                .get(row_ix)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let runs = super::runs(
                row,
                args.foreground,
                args.background,
                None,
                row_ix,
                None,
                plain_links,
                row_spans,
                &args.ansi,
            );
            let mut bg_runs = Vec::new();
            let mut baked = Vec::new();
            let mut underlines = Vec::new();
            let mut strikethroughs = Vec::new();

            for run in runs {
                if run.bg != args.background {
                    bg_runs.push(BgRun {
                        col: run.col,
                        columns: run.columns,
                        color: run.bg,
                    });
                }
                if run.underline {
                    underlines.push(DecoRun {
                        col: run.col,
                        columns: run.columns,
                        color: run.fg,
                    });
                }
                if run.strikethrough {
                    strikethroughs.push(DecoRun {
                        col: run.col,
                        columns: run.columns,
                        color: run.fg,
                    });
                }
                for glyph in run.glyphs {
                    if glyph.text.trim().is_empty() {
                        continue;
                    }
                    let line = self.shape_cached(
                        window,
                        &base_font,
                        args.font_size,
                        glyph.text,
                        GlyphStyle {
                            bold: run.bold,
                            italic: run.italic,
                            color: run.fg,
                        },
                    );
                    let cell_x = px(glyph.col as f32 * args.cell_width);
                    let padding_top = (row_h - line.ascent - line.descent) / 2.0;
                    let baseline_y = padding_top + line.ascent;
                    for shaped_run in &line.runs {
                        for sg in &shaped_run.glyphs {
                            baked.push(BakedGlyph {
                                dx: cell_x + sg.position.x,
                                dy: baseline_y + sg.position.y,
                                font_id: shaped_run.font_id,
                                glyph_id: sg.id,
                                color: run.fg,
                                is_emoji: sg.is_emoji,
                            });
                        }
                    }
                }
            }
            rows.push(CachedRow {
                cols: row.cells.len(),
                bg_runs,
                baked,
                underlines,
                strikethroughs,
            });
        }
        self.rows = rows;
        self.key = Some(key);
    }

    fn prepare_cursor(
        &mut self,
        args: &GridPaintArgs,
        window: &gpui::Window,
    ) -> Option<CursorPaint> {
        let (row_ix, cur_col) = args.cursor?;
        let row = args.snapshot.rows.get(row_ix)?;
        if cur_col >= row.cells.len() {
            return None;
        }
        let col = if row.cells[cur_col].wide_spacer && cur_col > 0 {
            cur_col - 1
        } else {
            cur_col
        };
        let cell = &row.cells[col];
        let glyph = grid_glyph(row, col);
        let block_glyph =
            if args.cursor_shape == CursorShape::Block && !glyph.text.trim().is_empty() {
                let base_font = terminal_base_font(window, &args.mono);
                Some(self.shape_cached(
                    window,
                    &base_font,
                    args.font_size,
                    glyph.text,
                    GlyphStyle {
                        bold: cell.bold,
                        italic: cell.italic,
                        color: args.background,
                    },
                ))
            } else {
                None
            };
        // 复用正文已经算好的下划线范围（含字符自带下划线、OSC 8 显式超链接与自动识别的纯文本网址）。
        let underline = self.rows.get(row_ix).is_some_and(|cached| {
            cached
                .underlines
                .iter()
                .any(|u| col >= u.col && col < u.col + u.columns)
        }) || cell.underline
            || cell.hyperlink.is_some();
        Some(CursorPaint {
            row: row_ix,
            col,
            columns: glyph.columns,
            shape: args.cursor_shape,
            block_glyph,
            underline,
            strikethrough: cell.strikethrough,
        })
    }
}

/// 整块终端网格用单个 canvas 绘制：内容不变时复用缓存的逐行布局与字形，
/// 拖选变化只重画选区矩形并复用已排好版的行，消除上千个 `div` + 重复 `shape_line` 开销。
pub(super) fn paint_pane_grid(args: GridPaintArgs) -> impl IntoElement {
    let mono = args.mono.clone();
    let font_size = args.font_size;
    let cache = Rc::clone(&args.cache);
    let selection = args.selection;
    let cell_width = args.cell_width;
    let row_height = args.row_height;
    let background = args.background;
    let selection_color = args.selection_color;
    let cursor_color = args.cursor_color;

    canvas(
        move |_, window, _| {
            let mut guard = args.cache.borrow_mut();
            guard.ensure_rows(&args, window);
            guard.prepare_cursor(&args, window)
        },
        move |bounds, cursor, window, cx| {
            let guard = cache.borrow();
            let row_h = px(row_height);
            let font_px = px(font_size);
            // 第一层：单元格底色 + 选区底纹 + 全部正文烘焙字形（同一层内 GPUI 先画矩形再画字形，正好底纹在下、文字在上）。
            window.paint_layer(bounds, |window| {
                for (row_ix, row) in guard.rows.iter().enumerate() {
                    let row_y = bounds.origin.y + px(row_ix as f32 * row_height);
                    let sel_span = super::selection_spans(selection, row_ix, row.cols)
                        .into_iter()
                        .next();

                    // 1. 单元格自定义底色（被选中的列让给选区底纹，避免盖住高亮）。
                    for bg in &row.bg_runs {
                        let run_start = bg.col;
                        let run_end = bg.col + bg.columns - 1;
                        match sel_span {
                            Some((sel_start, sel_end))
                                if sel_start <= run_end && sel_end >= run_start =>
                            {
                                if run_start < sel_start {
                                    let rect = Bounds::new(
                                        point(
                                            bounds.origin.x + px(run_start as f32 * cell_width),
                                            row_y,
                                        ),
                                        size(
                                            px((sel_start - run_start) as f32 * cell_width),
                                            row_h,
                                        ),
                                    );
                                    window.paint_quad(fill(rect, bg.color));
                                }
                                if run_end > sel_end {
                                    let rect = Bounds::new(
                                        point(
                                            bounds.origin.x + px((sel_end + 1) as f32 * cell_width),
                                            row_y,
                                        ),
                                        size(px((run_end - sel_end) as f32 * cell_width), row_h),
                                    );
                                    window.paint_quad(fill(rect, bg.color));
                                }
                            }
                            _ => {
                                let rect = Bounds::new(
                                    point(bounds.origin.x + px(bg.col as f32 * cell_width), row_y),
                                    size(px(bg.columns as f32 * cell_width), row_h),
                                );
                                window.paint_quad(fill(rect, bg.color));
                            }
                        }
                    }

                    // 2. 选区整块底纹。
                    if let Some((start, end)) = sel_span {
                        let rect = Bounds::new(
                            point(bounds.origin.x + px(start as f32 * cell_width), row_y),
                            size(px((end - start + 1) as f32 * cell_width), row_h),
                        );
                        window.paint_quad(fill(rect, selection_color));
                    }

                    // 3. 烘焙好的逐字形直画（ASCII 与中文/宽字符同速，每格位置严格固定）。
                    for g in &row.baked {
                        let origin = point(bounds.origin.x + g.dx, row_y + g.dy);
                        if g.is_emoji {
                            let _ = window.paint_emoji(origin, g.font_id, g.glyph_id, font_px);
                        } else {
                            let _ =
                                window.paint_glyph(origin, g.font_id, g.glyph_id, font_px, g.color);
                        }
                    }
                }
            });

            // 第二层（独立前景层）：下划线、删除线与光标必须盖在正文（含 `█` 等满格字符）之上；
            // GPUI 同一层内会先画矩形再画字形，因此本层内块光标矩形先于反色字形绘制，且整体晚于正文层。
            let has_foreground = cursor.is_some()
                || guard
                    .rows
                    .iter()
                    .any(|row| !row.underlines.is_empty() || !row.strikethroughs.is_empty());
            if has_foreground {
                window.paint_layer(bounds, |window| {
                    for (row_ix, row) in guard.rows.iter().enumerate() {
                        let row_y = bounds.origin.y + px(row_ix as f32 * row_height);
                        for u in &row.underlines {
                            let rect = Bounds::new(
                                point(
                                    bounds.origin.x + px(u.col as f32 * cell_width),
                                    row_y + row_h - px(1.0),
                                ),
                                size(px(u.columns as f32 * cell_width), px(1.0)),
                            );
                            window.paint_quad(fill(rect, u.color));
                        }
                        for s in &row.strikethroughs {
                            let rect = Bounds::new(
                                point(
                                    bounds.origin.x + px(s.col as f32 * cell_width),
                                    row_y + px(row_height * 0.5),
                                ),
                                size(px(s.columns as f32 * cell_width), px(1.0)),
                            );
                            window.paint_quad(fill(rect, s.color));
                        }
                    }

                    if let Some(c) = cursor.as_ref() {
                        let row_y = bounds.origin.y + px(c.row as f32 * row_height);
                        let x = bounds.origin.x + px(c.col as f32 * cell_width);
                        let w = px(c.columns as f32 * cell_width);
                        match c.shape {
                            CursorShape::Block => {
                                let cell_bounds = Bounds::new(point(x, row_y), size(w, row_h));
                                window.paint_quad(fill(cell_bounds, cursor_color));
                                if let Some(line) = &c.block_glyph {
                                    let _ = line.paint(
                                        point(x, row_y),
                                        row_h,
                                        TextAlign::Left,
                                        None,
                                        window,
                                        cx,
                                    );
                                }
                                if c.underline || c.strikethrough {
                                    window.paint_layer(cell_bounds, |window| {
                                        if c.underline {
                                            window.paint_quad(fill(
                                                Bounds::new(
                                                    point(x, row_y + row_h - px(1.0)),
                                                    size(w, px(1.0)),
                                                ),
                                                background,
                                            ));
                                        }
                                        if c.strikethrough {
                                            window.paint_quad(fill(
                                                Bounds::new(
                                                    point(x, row_y + px(row_height * 0.5)),
                                                    size(w, px(1.0)),
                                                ),
                                                background,
                                            ));
                                        }
                                    });
                                }
                            }
                            CursorShape::Underline => {
                                window.paint_quad(fill(
                                    Bounds::new(
                                        point(x, row_y + row_h - px(1.0)),
                                        size(w, px(1.0)),
                                    ),
                                    cursor_color,
                                ));
                            }
                            CursorShape::Bar => {
                                window.paint_quad(fill(
                                    Bounds::new(point(x, row_y), size(px(2.0), row_h)),
                                    cursor_color,
                                ));
                            }
                        }
                    }
                });
            }
        },
    )
    .flex_1()
    .w_full()
    .h_full()
    .min_w_0()
    .overflow_hidden()
    .font_family(mono)
    .text_size(px(font_size))
}

#[cfg(test)]
mod tests {
    use super::grid_glyph;
    use zeeterm_terminal::{Size, TerminalModel};

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

    #[gpui::test]
    fn grid_paint_uses_terminal_mono_font_instead_of_parent_ui_font(cx: &mut gpui::TestAppContext) {
        use gpui::{AppContext as _, ParentElement as _, Render, Styled as _, div};
        use std::cell::RefCell;
        use std::rc::Rc;

        let mut model = TerminalModel::new(Size { rows: 2, cols: 20 }, 10);
        model.feed(b"abc");
        let snapshot = Rc::new(model.snapshot());
        let cache = Rc::new(RefCell::new(super::PanePaintCache::default()));
        let seen_body_ok = Rc::new(RefCell::new(false));
        let seen_cursor_ok = Rc::new(RefCell::new(false));
        let seen_parent_family = Rc::new(RefCell::new(None));
        let seen_resolved_family = Rc::new(RefCell::new(None));

        struct Host {
            snapshot: Rc<zeeterm_terminal::Snapshot>,
            cache: Rc<RefCell<super::PanePaintCache>>,
            seen_body_ok: Rc<RefCell<bool>>,
            seen_cursor_ok: Rc<RefCell<bool>>,
            seen_parent_family: Rc<RefCell<Option<gpui::SharedString>>>,
            seen_resolved_family: Rc<RefCell<Option<gpui::SharedString>>>,
        }

        impl Render for Host {
            fn render(
                &mut self,
                _window: &mut gpui::Window,
                _cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                let mono = gpui::SharedString::new_static("Consolas");
                let args = super::GridPaintArgs {
                    snapshot: Rc::clone(&self.snapshot),
                    plain_links: Rc::new(vec![Vec::new(); self.snapshot.rows.len()]),
                    highlights: Vec::new(),
                    cache: Rc::clone(&self.cache),
                    selection: None,
                    cursor: Some((0, 0)),
                    cursor_shape: crate::settings::CursorShape::Block,
                    mono: mono.clone(),
                    font_size: 13.0,
                    cell_width: 8.0,
                    row_height: 18.0,
                    foreground: gpui::rgb(0xeeeeee).into(),
                    background: gpui::rgb(0x111111).into(),
                    selection_color: gpui::rgb(0x334466).into(),
                    cursor_color: gpui::rgb(0xcccccc).into(),
                    ansi: [0; 16],
                };
                let seen_body_ok = Rc::clone(&self.seen_body_ok);
                let seen_cursor_ok = Rc::clone(&self.seen_cursor_ok);
                let seen_parent_family = Rc::clone(&self.seen_parent_family);
                let seen_resolved_family = Rc::clone(&self.seen_resolved_family);
                let cache_check = Rc::clone(&self.cache);
                div()
                    .size_full()
                    .font_family("Segoe UI")
                    .child(super::paint_pane_grid(args))
                    .child(gpui::canvas(
                        move |_, window, _| {
                            *seen_parent_family.borrow_mut() =
                                Some(window.text_style().font().family);
                            let resolved = super::terminal_base_font(window, &mono);
                            *seen_resolved_family.borrow_mut() = Some(resolved.family.clone());
                            let guard = cache_check.borrow();
                            *seen_body_ok.borrow_mut() =
                                guard.rows.first().is_some_and(|r| !r.baked.is_empty());
                            *seen_cursor_ok.borrow_mut() = guard
                                .glyph_cache_key
                                .as_ref()
                                .is_some_and(|(family, _)| family == &mono)
                                && !guard.glyphs.is_empty();
                        },
                        |_, _, _, _| {},
                    ))
            }
        }

        let window = cx.add_window(|_, _| Host {
            snapshot,
            cache: Rc::clone(&cache),
            seen_body_ok: Rc::clone(&seen_body_ok),
            seen_cursor_ok: Rc::clone(&seen_cursor_ok),
            seen_parent_family: Rc::clone(&seen_parent_family),
            seen_resolved_family: Rc::clone(&seen_resolved_family),
        });
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .expect("draw");
        assert_eq!(
            seen_parent_family.borrow().as_deref(),
            Some("Segoe UI"),
            "前提：父容器使用的是界面字体"
        );
        assert_eq!(
            seen_resolved_family.borrow().as_deref(),
            Some("Consolas"),
            "终端排版应显式切换到终端字体而非继承父层 Segoe UI"
        );
        assert!(*seen_body_ok.borrow(), "正文应已用终端字体排版");
        assert!(*seen_cursor_ok.borrow(), "块光标字形应已按终端字体缓存");
    }
}
