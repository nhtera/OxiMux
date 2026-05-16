//! Row builder + run grouping for `TerminalView` render.
//!
//! Lifted out of `terminal_view.rs` to keep that file under the 500-LOC
//! warn cap and to isolate the pure rendering math (cursor + match
//! highlighting) from the entity's mutable state. Nothing in this module
//! touches GPUI listeners or focus — it's a leaf-level pixel function.

use gpui::{ParentElement, Pixels, SharedString, Styled, div};
use oximux_pty::{Cell, CellColor};
use oximux_settings::Theme;

use crate::shell::terminal_palette::{ColorRole, resolve};

/// Build one row's `Div`. Caller is responsible for stacking rows into the
/// terminal grid and for computing `match_cols` from search state.
///
/// `cursor` carries the (row, col) of the cursor for the full snapshot; when
/// `cursor.0 != row_idx`, the cursor branch goes silent (the cell at
/// `(cursor.0, cursor.1)` is the one that gets `inverse`).
///
/// `match_cols` is `None` when no search is active — zero-cost path, no
/// per-cell range check.
pub fn build_row(
    row: &[Cell],
    row_idx: usize,
    cursor: (usize, usize),
    match_cols: Option<&[(usize, usize)]>,
    theme: &Theme,
    line_height: Pixels,
) -> gpui::Div {
    let mut row_div = div().flex().flex_row().h(line_height).whitespace_nowrap();
    let cursor_col = if cursor.0 == row_idx {
        Some(cursor.1)
    } else {
        None
    };
    for run in group_runs(row, cursor_col, match_cols) {
        let (fg, bg) = effective_colors(&run, theme);
        let mut span = div().child(SharedString::from(run.text)).text_color(fg);
        // Match highlight: paint `theme.selection` as bg unless the cursor
        // (`inverse`) is on this run — cursor wins so it stays visible on a
        // matched cell.
        if run.is_match && !run.inverse {
            span = span.bg(theme.selection);
        } else if !run.inverse && run.bg == CellColor::Default {
            // Skip painting the canvas — saves one rect per default-bg run.
        } else {
            span = span.bg(bg);
        }
        row_div = row_div.child(span);
    }
    row_div
}

struct Run {
    text: String,
    fg: CellColor,
    bg: CellColor,
    inverse: bool,
    is_match: bool,
}

fn group_runs(
    row: &[Cell],
    cursor_col: Option<usize>,
    match_cols: Option<&[(usize, usize)]>,
) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
    for (col_idx, cell) in row.iter().enumerate() {
        let ch = if cell.ch == '\0' { ' ' } else { cell.ch };
        let inverse = cell.inverse || cursor_col == Some(col_idx);
        let is_match = match_cols.is_some_and(|ranges| {
            ranges
                .iter()
                .any(|&(start, end)| col_idx >= start && col_idx < end)
        });
        match runs.last_mut() {
            Some(last)
                if last.fg == cell.fg
                    && last.bg == cell.bg
                    && last.inverse == inverse
                    && last.is_match == is_match =>
            {
                last.text.push(ch);
            }
            _ => runs.push(Run {
                text: ch.to_string(),
                fg: cell.fg,
                bg: cell.bg,
                inverse,
                is_match,
            }),
        }
    }
    runs
}

/// Resolve fg + bg as concrete Hsla, then swap if `inverse`. Swapping at the
/// Hsla layer (not the `CellColor` layer) is what makes inverse on a
/// Default/Default cell actually flip the colors — if we swapped the
/// `CellColor`s first, `resolve(Default, Fg)` and `resolve(Default, Bg)`
/// would still split into fg_base / bg_base, but a Default/Default swap
/// would land back on fg_base / bg_base again and the cursor would be
/// invisible. With this version, the resolved fg_base (white) and bg_base
/// (charcoal) swap cleanly: cursor renders as a charcoal glyph on a white
/// block.
fn effective_colors(run: &Run, theme: &Theme) -> (gpui::Hsla, gpui::Hsla) {
    let fg = resolve(run.fg, ColorRole::Fg, theme);
    let bg = resolve(run.bg, ColorRole::Bg, theme);
    if run.inverse { (bg, fg) } else { (fg, bg) }
}
