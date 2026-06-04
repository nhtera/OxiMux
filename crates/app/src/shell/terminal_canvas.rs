//! Canvas-based paint for `TerminalView` — replaces the per-cell flex-row
//! div stack with direct pixel painting via `paint_quad` + `shape_line`.
//!
//! Why a custom paint path:
//!
//! The previous renderer built one `div().flex_row()` per visible row with
//! a child `div(SharedString::from(run.text))` per styled run. GPUI then
//! shaped each span independently and packed them via the flex layout.
//! That worked, but had two real costs that became visible at scale:
//!
//!   1. **Font drift across rows.** Each span's painted width was whatever
//!      the text shaping returned (usually `glyph_advance * char_count`),
//!      which can be sub-pixel and accumulate. Over 80–200 cols, glyphs
//!      drifted off the cell grid, producing the "looks fuzzy / not crisp"
//!      complaint when the user compared OxiMux to native terminal apps.
//!
//!   2. **Resize jitter.** The flex layout solver runs at the parent
//!      column level, so during a window drag the spans briefly re-pack
//!      against a stale snapshot's column count — cells visibly slide
//!      before snapping back when the next snapshot lands.
//!
//! The canvas path locks every glyph to the grid via the third argument
//! of `text_system().shape_line(text, size, runs, force_width)`. Passing
//! `Some(cell_width)` makes the layout engine advance every glyph by
//! exactly `cell_width` regardless of the font's natural metric — the
//! standard trick GPU terminal renderers use to keep monospace cells
//! crisp.
//!
//! Background quads use `floor()` for the left edge and `ceil()` for the
//! width — the standard sub-pixel-gap fix. Without it, adjacent same-color
//! cells render with visible hairline seams on HiDPI displays.

use gpui::{
    App, Bounds, FontStyle, FontWeight, Hsla, Pixels, Point, SharedString, Size, StrikethroughStyle,
    TextAlign, TextRun, UnderlineStyle, Window, fill, point, px,
};
use oximux_pty::{Cell, CellColor, CursorShapeKind, TerminalSnapshot};
use oximux_settings::{Theme, Typography};

use crate::shell::box_drawing;
use crate::shell::cell_metrics::CellMetrics;
use crate::shell::terminal_palette::{ColorRole, resolve};
use crate::shell::terminal_search_state::{MatchHit, MatchKind};

/// Alpha multipliers from `TerminalSettings`, passed in per paint so they
/// live-reload. `dim` = SGR-2 faint text; `unfocused` = lighter text in an
/// inactive pane; `unfocused_cursor` = the faint ghost cursor block there.
/// `Default` mirrors `TerminalSettings` defaults (0.7 / 0.85 / 0.3).
#[derive(Clone, Copy)]
pub struct Alphas {
    pub dim: f32,
    pub unfocused: f32,
    pub unfocused_cursor: f32,
}

impl Default for Alphas {
    fn default() -> Self {
        Self {
            dim: 0.7,
            unfocused: 0.85,
            unfocused_cursor: 0.3,
        }
    }
}

/// Owned inputs for the canvas paint closure. `TerminalView::render`
/// builds one of these per frame and moves it into the closure so the
/// canvas API's `FnOnce + 'static` requirement is satisfied.
pub struct PaintParams {
    pub snapshot: TerminalSnapshot,
    pub theme: Theme,
    pub typography: Typography,
    /// `(row, col)` of the cursor. Set to `(usize::MAX, usize::MAX)` to
    /// suppress the cursor (off-blink phase) — out-of-grid indices fall
    /// through the per-row branch silently.
    pub cursor: (usize, usize),
    /// Shape to draw the cursor in. `Block` inverts the cell (existing
    /// path); `Bar`/`Underline` draw a thin overlay quad without inverting
    /// the glyph; `Hidden` draws nothing.
    pub cursor_shape: CursorShapeKind,
    /// Per-visible-row match highlights from the search overlay. `len`
    /// equals `snapshot.cells.len()`; rows without matches hold an
    /// empty vec.
    pub buckets: Vec<Vec<MatchHit>>,
    pub pane_focused: bool,
    pub pad: f32,
    /// `(row, col_start, col_end)` of the link the pointer is hovering with
    /// Cmd held — underlined so the user sees it's clickable. `None` = none.
    pub hovered_link: Option<(usize, usize, usize)>,
    /// Active text selection in cell coordinates: `(start_row, start_col,
    /// end_row, end_col)`. End is inclusive on both axes. Painted as a
    /// theme.selection bg overlay BEHIND text but ON TOP of cell bg so
    /// the user can still read the underlying styled content through it.
    pub selection: Option<(usize, usize, usize, usize)>,
    /// Shell-integration command-mark badges as `(screen_row, is_error)`. A
    /// thin bar in the left padding gutter of each prompt row — green for a
    /// zero exit, red for non-zero. Empty unless the shell emits OSC 133/633.
    pub command_badges: Vec<(usize, bool)>,
    /// Live alpha multipliers (dim / unfocused / unfocused-cursor) from
    /// `TerminalSettings`.
    pub alphas: Alphas,
}

/// Compute the `(cols, rows)` that fit in a canvas of `bounds`, given the
/// live cell metrics and the configured padding. Shared by the resize
/// path (so the PTY is told exactly the grid we paint) and any future
/// hit-testing. `pad` is subtracted on both axes because the grid origin
/// is inset by `pad` (see `paint_grid`).
pub fn grid_dims_for(bounds: Bounds<Pixels>, metrics: &CellMetrics, pad: f32) -> (u16, u16) {
    let inner_w = (f32::from(bounds.size.width) - pad * 2.0).max(metrics.cell_width);
    let inner_h = (f32::from(bounds.size.height) - pad * 2.0).max(metrics.line_height);
    (
        metrics.cols_in(inner_w).max(1),
        metrics.rows_in(inner_h).max(1),
    )
}

/// Inverse of `paint_grid`'s origin math: map a window-space pixel `pos` to
/// the `(row, col)` cell it falls in. `bounds` is the canvas's painted
/// bounds (window coords), `pad` the same inset used at paint time. Clamps
/// negative offsets to 0; callers clamp the high end against the live grid
/// since this function has no knowledge of the snapshot's dimensions.
pub fn point_to_cell(
    pos: Point<Pixels>,
    bounds: Bounds<Pixels>,
    metrics: &CellMetrics,
    pad: f32,
) -> (usize, usize) {
    let origin_x = f32::from(bounds.origin.x) + pad;
    let origin_y = f32::from(bounds.origin.y) + pad;
    let rel_x = (f32::from(pos.x) - origin_x).max(0.0);
    let rel_y = (f32::from(pos.y) - origin_y).max(0.0);
    let col = (rel_x / metrics.cell_width).floor() as usize;
    let row = (rel_y / metrics.line_height).floor() as usize;
    (row, col)
}

/// Paint the grid into `bounds`. Designed to be called from the paint
/// closure of `gpui::canvas`. Borrows `PaintParams` so the caller can
/// reuse it (e.g. to compute resize dims) before/after painting.
pub fn paint_grid(bounds: Bounds<Pixels>, p: &PaintParams, window: &mut Window, cx: &mut App) {
    // Live metrics so font/typography changes propagate next paint.
    // `CellMetrics::measure` shapes the `'m'` advance — adequate for
    // monospace; box-drawing fonts ship narrower glyphs for the same
    // advance, and `shape_line(force_width=Some(cell_w))` corrects for
    // any mismatch at paint time.
    let metrics = CellMetrics::measure(&p.typography, window);
    let cell_w = px(metrics.cell_width);
    let line_h = metrics.line_height_px();
    let font = p.typography.mono_font();
    let font_size = px(p.typography.t_body_lg);

    let origin = point(bounds.origin.x + px(p.pad), bounds.origin.y + px(p.pad));

    // Paint the full pane in theme.bg_base. Cells that need a different
    // background overdraw below; the default-default path then skips its
    // own quad emission for free.
    window.paint_quad(fill(bounds, p.theme.bg_base));

    // Selection rectangles, painted AFTER cell backgrounds (so colored
    // cells are visible through them as a tinted overlay) but BEFORE
    // text (so glyph color is unaffected). Computed once per row outside
    // the per-cell run loop to keep the hot path branchless.
    let selection_per_row: Option<Vec<(usize, usize)>> = p.selection.map(|sel| {
        // Normalize so swapping start/end doesn't matter — currently the
        // setter always emits start <= end but be defensive.
        let (mut r0, mut c0, mut r1, mut c1) = sel;
        if (r0, c0) > (r1, c1) {
            std::mem::swap(&mut r0, &mut r1);
            std::mem::swap(&mut c0, &mut c1);
        }
        let last_row = p.snapshot.cells.len().saturating_sub(1);
        let r0 = r0.min(last_row);
        let r1 = r1.min(last_row);
        let mut ranges = Vec::with_capacity(r1 - r0 + 1);
        for row_idx in 0..=last_row {
            let last_col = p
                .snapshot
                .cells
                .get(row_idx)
                .map(|r| r.len().saturating_sub(1))
                .unwrap_or(0);
            if row_idx < r0 || row_idx > r1 {
                continue;
            }
            let (cs, ce) = if r0 == r1 {
                (c0.min(last_col), c1.min(last_col))
            } else if row_idx == r0 {
                (c0.min(last_col), last_col)
            } else if row_idx == r1 {
                (0, c1.min(last_col))
            } else {
                (0, last_col)
            };
            // pad up to len matching row_idx so callers can index by
            // row_idx directly.
            while ranges.len() < row_idx + 1 {
                ranges.push((usize::MAX, usize::MAX));
            }
            ranges[row_idx] = (cs, ce);
        }
        ranges
    });

    for (row_idx, row) in p.snapshot.cells.iter().enumerate() {
        let row_y = origin.y + line_h * row_idx as f32;
        let cursor_here = p.cursor.0 == row_idx;
        // Only the block shape inverts the cell; bar/underline paint an
        // overlay after the text, and hidden draws nothing.
        let cursor_col = if cursor_here && p.cursor_shape == CursorShapeKind::Block {
            Some(p.cursor.1)
        } else {
            None
        };
        let matches = p.buckets.get(row_idx).map(|v| v.as_slice());

        let runs = group_runs(row, cursor_col, matches);
        if runs.is_empty() {
            continue;
        }

        // ── Background pass ────────────────────────────────────────────
        // `floor` on x and `ceil` on width: prevents sub-pixel seams
        // between adjacent same-color cells on HiDPI displays — the
        // standard quad-snapping pattern for GPU terminal renderers.
        let mut col: usize = 0;
        for run in &runs {
            let n_cells = run.cols;
            let (_fg, bg) = effective_colors(run, p.pane_focused, &p.theme, p.alphas);
            // Match highlight overrides bg unless the cursor is also on
            // this run (cursor inverse wins — keeps the cursor visible
            // on a matched cell).
            let bg = if run.inverse {
                bg
            } else {
                match run.match_kind {
                    Some(MatchKind::Current) => p.theme.match_bg_current,
                    Some(MatchKind::Other) => p.theme.match_bg_other,
                    None => bg,
                }
            };
            // Skip emit when the run is exactly the canvas bg AND has no
            // other reason to repaint (cursor / match). Saves a per-cell
            // quad in the common "shell prompt with no styled bg" case.
            let needs_bg = run.inverse || run.match_kind.is_some() || bg != p.theme.bg_base;
            if needs_bg {
                let x_left = (origin.x + cell_w * col as f32).floor();
                let width = (cell_w * n_cells as f32).ceil();
                let rect = Bounds {
                    origin: point(x_left, row_y),
                    size: Size {
                        width,
                        height: line_h,
                    },
                };
                window.paint_quad(fill(rect, bg));
            }
            col += n_cells;
        }

        // ── Selection overlay ─────────────────────────────────────────
        // One quad per row that intersects the selection rectangle.
        // Painted AFTER cell backgrounds so it tints them, BEFORE text
        // so glyphs render at full contrast on top. The fg is unchanged
        // — relying on theme.selection's alpha to let cell content read
        // through (charcoal theme defaults to ~30 % alpha amber).
        if let Some(ranges) = &selection_per_row
            && let Some(&(cs, ce)) = ranges.get(row_idx)
            && cs != usize::MAX
        {
            let x_left = (origin.x + cell_w * cs as f32).floor();
            let width = (cell_w * (ce + 1 - cs) as f32).ceil();
            let rect = Bounds {
                origin: point(x_left, row_y),
                size: Size {
                    width,
                    height: line_h,
                },
            };
            window.paint_quad(fill(rect, p.theme.selection));
        }

        // ── Text pass ──────────────────────────────────────────────────
        // Per-row `force_width`: when the row holds NO wide glyphs we keep
        // the original `Some(cell_w)` so fallback faces (box-drawing,
        // braille, symbol glyphs whose natural advance ≠ `cell_w`) can't
        // accumulate drift across the row. When a wide glyph IS present we
        // pass `None` so its natural 2-cell advance flows through —
        // forcing it to `cell_w` would squish it and leave a gap in the
        // spacer column. Spacers are dropped in `group_runs`, so the text
        // length matches the column count.
        let row_has_wide = row.iter().any(|c| c.wide);
        let force_width = if row_has_wide { None } else { Some(cell_w) };
        let mut text = String::with_capacity(row.len());
        let mut text_runs: Vec<TextRun> = Vec::with_capacity(runs.len());
        for run in &runs {
            let (fg, bg) = effective_colors(run, p.pane_focused, &p.theme, p.alphas);
            // Current-match fg uses theme.match_fg for legibility against
            // the bright amber match_bg_current. Inverse cells (incl. the
            // cursor) already have the inverted color from effective_colors;
            // don't double-flip.
            let fg = if !run.inverse && matches!(run.match_kind, Some(MatchKind::Current)) {
                p.theme.match_fg
            } else {
                fg
            };
            // SGR 8 (hidden): paint the glyph in its own background so the
            // text is invisible on screen but still extracted by selection.
            let fg = if run.hidden { bg } else { fg };
            // Per-run font: SGR 1 (bold) → heavier weight, SGR 3 (italic) →
            // slanted. GPUI synthesizes a faux cut when the face lacks a real
            // bold/oblique.
            let mut run_font = font.clone();
            run_font.weight = if run.bold {
                FontWeight::BOLD
            } else {
                FontWeight::NORMAL
            };
            run_font.style = if run.italic {
                FontStyle::Italic
            } else {
                FontStyle::Normal
            };
            let byte_len = run.text.len();
            text.push_str(&run.text);
            text_runs.push(TextRun {
                len: byte_len,
                font: run_font,
                color: fg,
                background_color: None,
                underline: run.underline.then_some(UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(fg),
                    wavy: false,
                }),
                strikethrough: run.strikethrough.then_some(StrikethroughStyle {
                    thickness: px(1.0),
                    color: Some(fg),
                }),
            });
        }
        if text.is_empty() {
            continue;
        }
        let shaped = window.text_system().shape_line(
            SharedString::from(text),
            font_size,
            &text_runs,
            force_width,
        );
        // Errors painting a single row are non-fatal — the rest of the
        // grid is still useful, and shaping errors usually mean a missing
        // glyph that GPUI will substitute with `.notdef` (visible to the
        // user as a tofu box, which is the correct UX).
        let _ = shaped.paint(
            point(origin.x, row_y),
            line_h,
            TextAlign::Left,
            None,
            window,
            cx,
        );

        // ── Box-drawing vector pass ───────────────────────────────────
        // Box chars are emitted as spaces above so the font glyph never
        // paints — here the vector stroke fills the cells. Per-row early
        // exit when no box char is present (the common case) keeps plain
        // rows free of the extra pass.
        paint_box_drawing_row(
            row,
            cursor_col,
            row_y,
            origin.x,
            cell_w,
            line_h,
            p.pane_focused,
            &p.theme,
            p.alphas,
            window,
        );

        // Bar / underline cursor: a thin quad on TOP of the glyph (the block
        // shape already inverted the cell above). Dimmed in an unfocused pane.
        if cursor_here && matches!(p.cursor_shape, CursorShapeKind::Bar | CursorShapeKind::Underline)
        {
            let x_left = (origin.x + cell_w * p.cursor.1 as f32).floor();
            let mut color = p.theme.fg_base;
            if !p.pane_focused {
                color.a *= p.alphas.unfocused_cursor;
            }
            let thickness = px(2.0);
            let rect = if p.cursor_shape == CursorShapeKind::Bar {
                Bounds {
                    origin: point(x_left, row_y),
                    size: Size {
                        width: thickness,
                        height: line_h,
                    },
                }
            } else {
                Bounds {
                    origin: point(x_left, row_y + line_h - thickness),
                    size: Size {
                        width: cell_w,
                        height: thickness,
                    },
                }
            };
            window.paint_quad(fill(rect, color));
        }

        // Cmd-hover link underline: a 1px rule under the link's cells so the
        // user sees the clickable span.
        if let Some((link_row, c0, c1)) = p.hovered_link
            && link_row == row_idx
        {
            let x_left = (origin.x + cell_w * c0 as f32).floor();
            let width = (cell_w * (c1 + 1 - c0) as f32).ceil();
            let thickness = px(1.0);
            let rect = Bounds {
                origin: point(x_left, row_y + line_h - thickness),
                size: Size {
                    width,
                    height: thickness,
                },
            };
            window.paint_quad(fill(rect, p.theme.fg_base));
        }

        // Shell-integration command-mark badge: a thin bar in the left padding
        // gutter of the prompt row. Green for a zero exit, red otherwise.
        for &(badge_row, is_error) in &p.command_badges {
            if badge_row == row_idx {
                let color = if is_error {
                    p.theme.status_error
                } else {
                    p.theme.status_ok
                };
                let rect = Bounds {
                    origin: point(bounds.origin.x + px(2.0), row_y),
                    size: Size {
                        width: px(2.0),
                        height: line_h,
                    },
                };
                window.paint_quad(fill(rect, color));
            }
        }
    }
}

/// One styled run of consecutive cells that share fg, bg, inverse, dim,
/// cursor-state, and match-state. Run boundary keys mirror the old
/// flex-paint grouping in `terminal_row::group_runs` (deleted along
/// with the row builder).
#[derive(Clone, Default)]
struct Run {
    text: String,
    fg: CellColor,
    bg: CellColor,
    inverse: bool,
    dim: bool,
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
    hidden: bool,
    /// True only when this run is exactly the cell under the cursor (not
    /// for SGR 7 inverse). Distinguishes the cursor's inverse from
    /// regular inverse so the unfocused-pane ghost cursor can dim ONLY
    /// the cursor cell, not arbitrary inverse runs.
    is_cursor: bool,
    match_kind: Option<MatchKind>,
    /// Total terminal columns this run spans. Narrow chars contribute 1,
    /// double-width (CJK) chars contribute 2 — diverging from
    /// `text.chars().count()` once a row mixes widths. The background and
    /// selection passes use this to size their quads; spacer cells are
    /// folded into the wide char's column count and contribute no chars.
    cols: usize,
}

fn group_runs(
    row: &[Cell],
    cursor_col: Option<usize>,
    match_cols: Option<&[MatchHit]>,
) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::with_capacity(row.len() / 4);
    for (col_idx, cell) in row.iter().enumerate() {
        // Spacer cells hold no glyph of their own — the adjacent wide cell
        // already owns this column. Drop them, but the cell that DID hold the
        // wide glyph already contributed `cols += 2`, so the row's column
        // accounting still totals correctly.
        if cell.wide_spacer {
            continue;
        }
        // Box-drawing characters with a vector implementation are emitted
        // into the text as spaces so the font glyph never paints; the
        // post-text vector pass strokes them directly. Diagonals
        // (U+2571..=U+2573) have no segment-model expression and fall
        // through to the font face instead of rendering blank.
        let ch = if cell.ch == '\0' || box_drawing::has_vector_impl(cell.ch) {
            ' '
        } else {
            cell.ch
        };
        let is_cursor = cursor_col == Some(col_idx);
        let inverse = cell.inverse || is_cursor;
        let match_kind = match_cols.and_then(|ranges| {
            ranges
                .iter()
                .find(|hit| col_idx >= hit.col_start && col_idx < hit.col_end)
                .map(|hit| hit.kind)
        });
        let advance = if cell.wide { 2 } else { 1 };
        match runs.last_mut() {
            Some(last)
                if last.fg == cell.fg
                    && last.bg == cell.bg
                    && last.inverse == inverse
                    && last.dim == cell.dim
                    && last.bold == cell.bold
                    && last.italic == cell.italic
                    && last.underline == cell.underline
                    && last.strikethrough == cell.strikethrough
                    && last.hidden == cell.hidden
                    && last.is_cursor == is_cursor
                    && last.match_kind == match_kind =>
            {
                last.text.push(ch);
                last.cols += advance;
            }
            _ => runs.push(Run {
                text: ch.to_string(),
                fg: cell.fg,
                bg: cell.bg,
                inverse,
                dim: cell.dim,
                bold: cell.bold,
                italic: cell.italic,
                underline: cell.underline,
                strikethrough: cell.strikethrough,
                hidden: cell.hidden,
                is_cursor,
                match_kind,
                cols: advance,
            }),
        }
    }
    runs
}

/// Resolve fg + bg as concrete Hsla, then swap if `inverse`. Swapping at
/// the Hsla layer (not the `CellColor` layer) makes inverse on a
/// Default/Default cell actually flip colors — if we swapped CellColors
/// first, `resolve(Default, Fg)` and `resolve(Default, Bg)` would still
/// split into fg_base / bg_base and the swap would null itself out.
///
/// Alpha multipliers compose: SGR-dim text in an unfocused pane gets
/// both `DIM_FG_ALPHA` and `UNFOCUSED_FG_ALPHA` (0.7 × 0.4 = 0.28).
/// Cursor cells get only `UNFOCUSED_CURSOR_ALPHA` on the inverted bg
/// — not the foreground multiplier, which would double-fade the glyph
/// inside the ghost block to invisibility.
fn effective_colors(run: &Run, pane_focused: bool, theme: &Theme, alphas: Alphas) -> (Hsla, Hsla) {
    let fg = resolve(run.fg, ColorRole::Fg, theme);
    let bg = resolve(run.bg, ColorRole::Bg, theme);
    let (mut fg, mut bg) = if run.inverse { (bg, fg) } else { (fg, bg) };
    if run.dim {
        fg.a *= alphas.dim;
    }
    if !pane_focused {
        if run.is_cursor {
            bg.a *= alphas.unfocused_cursor;
        } else {
            fg.a *= alphas.unfocused;
        }
    }
    (fg, bg)
}

/// Per-row vector pass for box-drawing characters. Bails out cheaply when
/// the row has none. Two sub-passes: (1) merge consecutive cells with the
/// same horizontal weight + same effective fg into ONE continuous stroke
/// (no inter-cell seams); (2) every other box-drawing cell paints
/// individually with a 1-px edge overlap. The cursor cell, when on a box
/// char, is drawn in the inverted color so it reads against the cursor
/// block.
#[allow(clippy::too_many_arguments)]
fn paint_box_drawing_row(
    row: &[Cell],
    cursor_col: Option<usize>,
    row_y: Pixels,
    origin_x: Pixels,
    cell_w: Pixels,
    line_h: Pixels,
    pane_focused: bool,
    theme: &Theme,
    alphas: Alphas,
    window: &mut Window,
) {
    // Map non-spacer cells to their starting column, mirroring `group_runs`'s
    // wide-cell accounting so positions stay grid-aligned in mixed CJK rows.
    let mut positions: Vec<(usize, &Cell)> = Vec::with_capacity(row.len());
    let mut col: usize = 0;
    for cell in row {
        if cell.wide_spacer {
            continue;
        }
        positions.push((col, cell));
        col += if cell.wide { 2 } else { 1 };
    }

    // Cheap presence gate — plain text rows pay only this scan. Use the
    // vector-impl predicate (not the wider box-drawing range) so a row of
    // only diagonals doesn't pay for an empty pass.
    if !positions
        .iter()
        .any(|(_, c)| box_drawing::has_vector_impl(c.ch))
    {
        return;
    }

    let cy = row_y + line_h / 2.0;
    // `Vec<bool>` keyed by column index is cheaper than a `HashSet` here
    // because the column count is small (≤ a few hundred) and lookups are
    // O(1) by indexing. The cell list ends at the last non-spacer column.
    let max_col = positions.last().map(|(c, _)| *c).unwrap_or(0);
    let mut processed_horizontal: Vec<bool> = vec![false; max_col + 1];

    // ── Pass 1: horizontal merge ──────────────────────────────────────
    let mut i = 0;
    while i < positions.len() {
        let (col_idx, cell) = positions[i];
        let ch = cell.ch;
        let Some(weight) = box_drawing::get_horizontal_weight(ch) else {
            i += 1;
            continue;
        };
        let is_cursor_here = cursor_col == Some(col_idx);
        // SGR 8 hidden — text path renders glyph in its own bg (invisible);
        // the vector pass mirrors this by skipping entirely, regardless of
        // inverse or cursor state.
        if cell.hidden {
            i += 1;
            continue;
        }
        let color = effective_fg_for_cell(cell, is_cursor_here, pane_focused, theme, alphas);
        let start_col = col_idx;
        let mut end_col = col_idx;
        let mut j = i + 1;
        // Greedily extend the span over neighbouring cells that share
        // weight, color, and are not hidden — the join becomes ONE stroke.
        while j < positions.len() {
            let (next_col, next_cell) = positions[j];
            if next_col != end_col + 1 {
                break;
            }
            if box_drawing::get_horizontal_weight(next_cell.ch) != Some(weight) {
                break;
            }
            if next_cell.hidden {
                break;
            }
            let next_is_cursor = cursor_col == Some(next_col);
            let next_color =
                effective_fg_for_cell(next_cell, next_is_cursor, pane_focused, theme, alphas);
            if next_color != color {
                break;
            }
            end_col = next_col;
            j += 1;
        }
        let start_x = origin_x + cell_w * start_col as f32;
        let end_x = origin_x + cell_w * (end_col + 1) as f32;
        box_drawing::draw_horizontal_span(start_x, end_x, cy, weight, cell_w, color, window);
        for c in start_col..=end_col {
            if c < processed_horizontal.len() {
                processed_horizontal[c] = true;
            }
        }
        i = j;
    }

    // ── Pass 2: vertical / standalone components ──────────────────────
    for &(col_idx, cell) in &positions {
        let ch = cell.ch;
        // Skip diagonals — they have no segment-model expression and the
        // group_runs replacement also left their font glyph intact.
        if !box_drawing::has_vector_impl(ch) {
            continue;
        }
        if cell.hidden {
            continue;
        }
        let is_cursor_here = cursor_col == Some(col_idx);
        let color = effective_fg_for_cell(cell, is_cursor_here, pane_focused, theme, alphas);
        let bounds = Bounds {
            origin: point(origin_x + cell_w * col_idx as f32, row_y),
            size: Size {
                width: cell_w,
                height: line_h,
            },
        };
        let already_h = processed_horizontal
            .get(col_idx)
            .copied()
            .unwrap_or(false);
        if already_h {
            // Horizontal already painted by the merge — only the vertical
            // axis of a T-junction / cross remains.
            box_drawing::draw_vertical_components(ch, bounds, color, cell_w, window);
        } else {
            box_drawing::draw_box_character(ch, bounds, color, cell_w, window);
        }
    }
}

/// Per-cell effective fg, mirroring `effective_colors` but without the run
/// abstraction. Used by the box-drawing pass which iterates cells directly
/// — the vector stroke is the "glyph", so it inherits the same fg the text
/// path would have computed for that cell.
fn effective_fg_for_cell(
    cell: &Cell,
    is_cursor: bool,
    pane_focused: bool,
    theme: &Theme,
    alphas: Alphas,
) -> Hsla {
    let fg = resolve(cell.fg, ColorRole::Fg, theme);
    let bg = resolve(cell.bg, ColorRole::Bg, theme);
    let inverse = cell.inverse || is_cursor;
    let mut fg = if inverse { bg } else { fg };
    if cell.dim {
        fg.a *= alphas.dim;
    }
    if !pane_focused && !is_cursor {
        fg.a *= alphas.unfocused;
    }
    fg
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_pty::{Cell, NamedColor16};

    fn cell(ch: char, fg: CellColor, bg: CellColor) -> Cell {
        Cell {
            ch,
            fg,
            bg,
            ..Cell::default()
        }
    }

    #[test]
    fn group_runs_merges_consecutive_same_style_cells() {
        let row = vec![
            cell('h', CellColor::Default, CellColor::Default),
            cell('i', CellColor::Default, CellColor::Default),
            cell('!', CellColor::Named(NamedColor16::Red), CellColor::Default),
        ];
        let runs = group_runs(&row, None, None);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].text, "hi");
        assert_eq!(runs[1].text, "!");
    }

    #[test]
    fn point_to_cell_maps_pixels_to_grid() {
        let metrics = CellMetrics {
            cell_width: 8.0,
            line_height: 16.0,
        };
        let bounds = Bounds {
            origin: point(px(10.0), px(20.0)),
            size: Size {
                width: px(800.0),
                height: px(600.0),
            },
        };
        let pad = 4.0;
        // Grid origin = (14, 24). A point 3 cells right + 2 rows down, mid-cell.
        let pos = point(px(14.0 + 8.0 * 3.0 + 2.0), px(24.0 + 16.0 * 2.0 + 1.0));
        assert_eq!(point_to_cell(pos, bounds, &metrics, pad), (2, 3));
        // Anything left/above the grid origin clamps to (0, 0).
        assert_eq!(
            point_to_cell(point(px(0.0), px(0.0)), bounds, &metrics, pad),
            (0, 0)
        );
    }

    #[test]
    fn group_runs_splits_on_sgr_attribute_boundary() {
        // Same colors, but the middle cell is bold → it must start its own run
        // so the renderer can switch font weight at the boundary.
        let mut bold_cell = cell('b', CellColor::Default, CellColor::Default);
        bold_cell.bold = true;
        let row = vec![
            cell('a', CellColor::Default, CellColor::Default),
            bold_cell,
            cell('c', CellColor::Default, CellColor::Default),
        ];
        let runs = group_runs(&row, None, None);
        assert_eq!(runs.len(), 3, "a bold cell between plain cells splits runs");
        assert!(runs[1].bold);
        assert_eq!(runs[1].text, "b");
    }

    #[test]
    fn group_runs_splits_on_cursor_boundary() {
        let row = vec![
            cell('a', CellColor::Default, CellColor::Default),
            cell('b', CellColor::Default, CellColor::Default),
            cell('c', CellColor::Default, CellColor::Default),
        ];
        let runs = group_runs(&row, Some(1), None);
        assert_eq!(runs.len(), 3, "cursor cell creates its own run");
        assert!(runs[1].is_cursor);
        assert!(runs[1].inverse);
    }

    #[test]
    fn group_runs_replaces_null_with_space() {
        let row = vec![cell('\0', CellColor::Default, CellColor::Default)];
        let runs = group_runs(&row, None, None);
        assert_eq!(runs[0].text, " ");
    }

    #[test]
    fn group_runs_replaces_box_drawing_with_space() {
        // U+2500..=U+257F with a vector implementation must be emitted as
        // ' ' so the font glyph never paints — the post-text vector pass
        // strokes them directly. Column accounting is unchanged. Cells
        // with the same style as their neighbours still merge into one run.
        let row = vec![
            cell('a', CellColor::Default, CellColor::Default),
            cell('\u{2500}', CellColor::Default, CellColor::Default), // ─
            cell('\u{253C}', CellColor::Default, CellColor::Default), // ┼
            cell('b', CellColor::Default, CellColor::Default),
        ];
        let runs = group_runs(&row, None, None);
        assert_eq!(runs.len(), 1, "same-style cells merge across the row");
        assert_eq!(runs[0].text, "a  b", "box chars become spaces in shaped text");
        assert_eq!(runs[0].cols, 4);
    }

    #[test]
    fn group_runs_keeps_diagonals_as_font_glyphs() {
        // Diagonals (U+2571..=U+2573) are inside the box-drawing block but
        // have no vector implementation. They must NOT be replaced with a
        // space — otherwise they would render blank (vector pass returns
        // false, no font glyph, nothing visible).
        let row = vec![
            cell('\u{2571}', CellColor::Default, CellColor::Default), // ╱
            cell('\u{2572}', CellColor::Default, CellColor::Default), // ╲
        ];
        let runs = group_runs(&row, None, None);
        assert_eq!(runs[0].text, "\u{2571}\u{2572}");
    }

    #[test]
    fn group_runs_splits_styled_box_drawing() {
        // A box char on a styled run still gets the space substitution and
        // still splits the run on its style boundary — the vector pass
        // paints it in the styled fg, so the boundary must be preserved.
        let mut red_box = cell(
            '\u{2500}',
            CellColor::Named(NamedColor16::Red),
            CellColor::Default,
        );
        red_box.bold = false; // explicit for clarity
        let row = vec![
            cell('a', CellColor::Default, CellColor::Default),
            red_box,
            cell('b', CellColor::Default, CellColor::Default),
        ];
        let runs = group_runs(&row, None, None);
        assert_eq!(runs.len(), 3, "differently styled box char splits runs");
        assert_eq!(runs[1].text, " ", "styled box char is still replaced");
        assert_eq!(runs[1].fg, CellColor::Named(NamedColor16::Red));
    }

    #[test]
    fn group_runs_skips_wide_spacer_and_counts_two_columns() {
        // Row: ASCII 'a', wide '你' (occupies cols 1 + 2), ASCII 'b'.
        let mut wide_cell = cell('你', CellColor::Default, CellColor::Default);
        wide_cell.wide = true;
        let mut spacer = cell(' ', CellColor::Default, CellColor::Default);
        spacer.wide_spacer = true;
        let row = vec![
            cell('a', CellColor::Default, CellColor::Default),
            wide_cell,
            spacer,
            cell('b', CellColor::Default, CellColor::Default),
        ];
        let runs = group_runs(&row, None, None);
        // All four cells share style → one merged run, but the spacer
        // contributes no character. Total column span is 4 (1 + 2 + 1).
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].text, "a你b", "spacer must NOT add a char");
        assert_eq!(
            runs[0].cols, 4,
            "wide glyph contributes 2 cols, spacer absorbed"
        );
    }

    #[test]
    fn effective_colors_dims_when_unfocused() {
        let theme = Theme::charcoal();
        let run = Run {
            text: "x".into(),
            fg: CellColor::Default,
            bg: CellColor::Default,
            inverse: false,
            dim: false,
            is_cursor: false,
            match_kind: None,
            ..Run::default()
        };
        let (fg_focused, _) = effective_colors(&run, true, &theme, Alphas::default());
        let (fg_unfocused, _) = effective_colors(&run, false, &theme, Alphas::default());
        assert!(fg_unfocused.a < fg_focused.a);
    }

    #[test]
    fn effective_colors_inverse_swaps_fg_and_bg() {
        let theme = Theme::charcoal();
        let run = Run {
            text: "x".into(),
            fg: CellColor::Default,
            bg: CellColor::Default,
            inverse: true,
            dim: false,
            is_cursor: true,
            match_kind: None,
            ..Run::default()
        };
        let (fg, bg) = effective_colors(&run, true, &theme, Alphas::default());
        // After swap, fg becomes the canvas bg and bg becomes the text fg.
        assert_eq!(fg, theme.bg_base);
        assert_eq!(bg, theme.fg_base);
    }
}
