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
//! same trick crate-reorg-terminal/element.rs and zed-industries/zed's terminal_element
//! both use to keep monospace cells crisp.
//!
//! Background quads use `floor()` for the left edge and `ceil()` for the
//! width — this is the standard sub-pixel-gap fix borrowed from crate-reorg's
//! `LayoutRect::paint`. Without it, adjacent same-color cells render with
//! visible hairline seams on HiDPI displays.

use gpui::{
    App, Bounds, Hsla, Pixels, SharedString, Size, TextAlign, TextRun, Window, fill, point, px,
};
use oximux_pty::{Cell, CellColor, TerminalSnapshot};
use oximux_settings::{Theme, Typography};

use crate::shell::cell_metrics::CellMetrics;
use crate::shell::terminal_palette::{ColorRole, resolve};
use crate::shell::terminal_search_state::{MatchHit, MatchKind};

/// Multiplier on fg alpha when SGR 2 ("faint") is set. Sits between
/// the reference terminal 0.69 / Alacritty 0.66 / Kitty 0.75 — picked to land in the
/// middle so dim text (zsh autosuggest, fish, prompt paths) reads the
/// way users expect across terminals.
const DIM_FG_ALPHA: f32 = 0.7;

/// Multiplier on fg alpha for cells in an unfocused pane. Replaces the
/// older translucent-veil approach with the "lighter text on same bg"
/// treatment the reference terminal / iTerm2 use — the focused pane reads as the active
/// one without any extra chrome.
const UNFOCUSED_FG_ALPHA: f32 = 0.4;

/// Multiplier on the inverted cursor-cell bg when its pane is unfocused.
/// Renders a faint ghost block so the user can still see where the
/// shell's caret sits in the inactive pane.
const UNFOCUSED_CURSOR_ALPHA: f32 = 0.3;

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
    /// Per-visible-row match highlights from the search overlay. `len`
    /// equals `snapshot.cells.len()`; rows without matches hold an
    /// empty vec.
    pub buckets: Vec<Vec<MatchHit>>,
    pub pane_focused: bool,
    pub pad: f32,
    /// Active text selection in cell coordinates: `(start_row, start_col,
    /// end_row, end_col)`. End is inclusive on both axes. Painted as a
    /// theme.selection bg overlay BEHIND text but ON TOP of cell bg so
    /// the user can still read the underlying styled content through it.
    pub selection: Option<(usize, usize, usize, usize)>,
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
        let cursor_col = if p.cursor.0 == row_idx {
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
        // between adjacent same-color cells on HiDPI displays. Same
        // pattern crate-reorg-terminal/element.rs uses in `LayoutRect::paint`.
        let mut col: usize = 0;
        for run in &runs {
            let n_cells = run.text.chars().count();
            let (_fg, bg) = effective_colors(run, p.pane_focused, &p.theme);
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
        // One ShapedLine per row with `force_width = Some(cell_w)` so
        // every glyph advance is exactly cell_w — no drift across the
        // row regardless of font shaping. TextRuns carry per-cell color.
        let mut text = String::with_capacity(row.len());
        let mut text_runs: Vec<TextRun> = Vec::with_capacity(runs.len());
        for run in &runs {
            let (fg, _bg) = effective_colors(run, p.pane_focused, &p.theme);
            // Current-match fg uses theme.match_fg for legibility against
            // the bright amber match_bg_current. Inverse cells (incl. the
            // cursor) already have the inverted color from effective_colors;
            // don't double-flip.
            let fg = if !run.inverse && matches!(run.match_kind, Some(MatchKind::Current)) {
                p.theme.match_fg
            } else {
                fg
            };
            let byte_len = run.text.len();
            text.push_str(&run.text);
            text_runs.push(TextRun {
                len: byte_len,
                font: font.clone(),
                color: fg,
                background_color: None,
                underline: None,
                strikethrough: None,
            });
        }
        if text.is_empty() {
            continue;
        }
        let shaped = window.text_system().shape_line(
            SharedString::from(text),
            font_size,
            &text_runs,
            Some(cell_w),
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
    }
}

/// One styled run of consecutive cells that share fg, bg, inverse, dim,
/// cursor-state, and match-state. Run boundary keys mirror the old
/// flex-paint grouping in `terminal_row::group_runs` (deleted along
/// with the row builder).
#[derive(Clone)]
struct Run {
    text: String,
    fg: CellColor,
    bg: CellColor,
    inverse: bool,
    dim: bool,
    /// True only when this run is exactly the cell under the cursor (not
    /// for SGR 7 inverse). Distinguishes the cursor's inverse from
    /// regular inverse so the unfocused-pane ghost cursor can dim ONLY
    /// the cursor cell, not arbitrary inverse runs.
    is_cursor: bool,
    match_kind: Option<MatchKind>,
}

fn group_runs(
    row: &[Cell],
    cursor_col: Option<usize>,
    match_cols: Option<&[MatchHit]>,
) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::with_capacity(row.len() / 4);
    for (col_idx, cell) in row.iter().enumerate() {
        let ch = if cell.ch == '\0' { ' ' } else { cell.ch };
        let is_cursor = cursor_col == Some(col_idx);
        let inverse = cell.inverse || is_cursor;
        let match_kind = match_cols.and_then(|ranges| {
            ranges
                .iter()
                .find(|hit| col_idx >= hit.col_start && col_idx < hit.col_end)
                .map(|hit| hit.kind)
        });
        match runs.last_mut() {
            Some(last)
                if last.fg == cell.fg
                    && last.bg == cell.bg
                    && last.inverse == inverse
                    && last.dim == cell.dim
                    && last.is_cursor == is_cursor
                    && last.match_kind == match_kind =>
            {
                last.text.push(ch);
            }
            _ => runs.push(Run {
                text: ch.to_string(),
                fg: cell.fg,
                bg: cell.bg,
                inverse,
                dim: cell.dim,
                is_cursor,
                match_kind,
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
fn effective_colors(run: &Run, pane_focused: bool, theme: &Theme) -> (Hsla, Hsla) {
    let fg = resolve(run.fg, ColorRole::Fg, theme);
    let bg = resolve(run.bg, ColorRole::Bg, theme);
    let (mut fg, mut bg) = if run.inverse { (bg, fg) } else { (fg, bg) };
    if run.dim {
        fg.a *= DIM_FG_ALPHA;
    }
    if !pane_focused {
        if run.is_cursor {
            bg.a *= UNFOCUSED_CURSOR_ALPHA;
        } else {
            fg.a *= UNFOCUSED_FG_ALPHA;
        }
    }
    (fg, bg)
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
            inverse: false,
            dim: false,
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
        };
        let (fg_focused, _) = effective_colors(&run, true, &theme);
        let (fg_unfocused, _) = effective_colors(&run, false, &theme);
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
        };
        let (fg, bg) = effective_colors(&run, true, &theme);
        // After swap, fg becomes the canvas bg and bg becomes the text fg.
        assert_eq!(fg, theme.bg_base);
        assert_eq!(bg, theme.fg_base);
    }
}
