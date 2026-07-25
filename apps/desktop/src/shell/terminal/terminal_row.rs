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
use crate::shell::terminal_search_state::{MatchHit, MatchKind};

/// Build one row's `Div`. Caller is responsible for stacking rows into the
/// terminal grid and for computing `match_cols` from search state.
///
/// `cursor` carries the (row, col) of the cursor for the full snapshot; when
/// `cursor.0 != row_idx`, the cursor branch goes silent (the cell at
/// `(cursor.0, cursor.1)` is the one that gets `inverse`).
///
/// `match_cols` is `None` when no search is active — zero-cost path, no
/// per-cell range check. When `Some`, each `MatchHit` carries a `kind`
/// (Current = bright amber + dark fg; Other = dim amber, default fg).
pub fn build_row(
    row: &[Cell],
    row_idx: usize,
    cursor: (usize, usize),
    match_cols: Option<&[MatchHit]>,
    theme: &Theme,
    line_height: Pixels,
    pane_focused: bool,
) -> gpui::Div {
    // `.line_height(line_height)` is load-bearing: GPUI's default text
    // line-height is `phi()` = 1.618 × font_size (~22.65 px for our 14 px
    // body), which makes glyphs float in a tall line-box with vertical
    // padding above and below. Half-block characters (▀ U+2580, ▄ U+2584,
    // █ U+2588) rely on glyphs touching with zero gap so ▄-then-▀ across
    // adjacent rows joins into a solid pixel column. Caller passes a
    // line_height tuned to Menlo's em-square (~17 px at 14 pt) so the
    // padding collapses; Claude Code's mascot is the canonical regression
    // case and should now render with the same pixel resolution as
    // mainstream GPU-accelerated terminals.
    let mut row_div = div()
        .flex()
        .flex_row()
        .h(line_height)
        .line_height(line_height)
        .whitespace_nowrap();
    let cursor_col = if cursor.0 == row_idx {
        Some(cursor.1)
    } else {
        None
    };
    for run in group_runs(row, cursor_col, match_cols) {
        let (fg, bg) = effective_colors(&run, pane_focused, theme);
        // `.h_full()` ensures the match/cursor bg covers the full row
        // height — otherwise the span shrinks to glyph height and
        // highlights render as skinny blocks instead of full strips.
        // `.flex_none()` keeps the span at its content width so consecutive
        // runs butt against each other without any flex-growth distortion.
        let mut span = div().h_full().child(SharedString::from(run.text));
        // Match highlight: cursor wins over match (so the inverted cursor
        // block stays visible on a matched cell). Otherwise default to
        // painting the resolved bg unconditionally — earlier we skipped
        // `CellColor::Default` runs as an optimisation, but that left the
        // bottom half of half-block glyphs (▀) showing whatever ancestor
        // paints first instead of the theme canvas, which produced visible
        // seams in pixel-art mascots (Claude Code). Always paint the run's
        // own bg so each cell renders against a known canvas.
        span = if run.inverse {
            span.text_color(fg).bg(bg)
        } else if let Some(MatchKind::Current) = run.match_kind {
            span.text_color(theme.match_fg).bg(theme.match_bg_current)
        } else if let Some(MatchKind::Other) = run.match_kind {
            span.text_color(fg).bg(theme.match_bg_other)
        } else {
            span.text_color(fg).bg(bg)
        };
        row_div = row_div.child(span);
    }
    row_div
}

struct Run {
    text: String,
    fg: CellColor,
    bg: CellColor,
    inverse: bool,
    dim: bool,
    /// True when this run is the cell currently under the cursor (and only
    /// then — `cell.inverse` set via SGR 7 does not set this flag). Used to
    /// distinguish "cursor inverse" from "ordinary inverse" so the unfocused
    /// pane can render a dim ghost cursor without dimming inverse text runs.
    is_cursor: bool,
    match_kind: Option<MatchKind>,
}

fn group_runs(
    row: &[Cell],
    cursor_col: Option<usize>,
    match_cols: Option<&[MatchHit]>,
) -> Vec<Run> {
    let mut runs: Vec<Run> = Vec::new();
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

/// Multiplier applied to fg alpha when the cell carries `Flags::DIM`
/// (SGR 2 / "faint"). 0.7 lands in the middle of the ~0.66–0.75 faint-text
/// opacity range common across mainstream terminals, so shell prompts that
/// rely on dim text (zsh autosuggestions, fish, oh-my-zsh paths) read the
/// way users expect.
const DIM_FG_ALPHA: f32 = 0.7;

/// Multiplier applied to fg alpha when the *pane* isn't focused. This
/// replaces the old absolute black overlay (22 % alpha veil) with the
/// "just lighter text" treatment common multiplexers use: the bg
/// stays unchanged so split borders read as crisp, and inactive text
/// fades back so the focused pane's content reads as the active one
/// without any additional chrome.
///
/// 0.4 is pronounced enough that the eye lands on the focused pane
/// immediately at a glance.
/// Earlier 0.55 attempt was too subtle on charcoal bg (~#0E0F11): even
/// reduced alpha left high enough luminance that focus state was
/// ambiguous from a step back.
const UNFOCUSED_FG_ALPHA: f32 = 0.4;

/// Multiplier applied to the cursor-cell bg (the inverted color) when the
/// pane isn't focused. Renders a "ghost" cursor block — visible enough that
/// the user knows where the shell's caret is in the inactive pane, faint
/// enough to read as inactive. 0.3 is a common opacity for unfocused
/// cursors.
const UNFOCUSED_CURSOR_ALPHA: f32 = 0.3;

/// Resolve fg + bg as concrete Hsla, then swap if `inverse`. Swapping at the
/// Hsla layer (not the `CellColor` layer) is what makes inverse on a
/// Default/Default cell actually flip the colors — if we swapped the
/// `CellColor`s first, `resolve(Default, Fg)` and `resolve(Default, Bg)`
/// would still split into fg_base / bg_base, but a Default/Default swap
/// would land back on fg_base / bg_base again and the cursor would be
/// invisible. With this version, the resolved fg_base (white) and bg_base
/// (charcoal) swap cleanly: cursor renders as a charcoal glyph on a white
/// block.
///
/// Alpha multipliers compose: SGR-dim text in an unfocused pane gets both
/// `DIM_FG_ALPHA` and `UNFOCUSED_FG_ALPHA` (0.7 × 0.4 = 0.28) — intentional.
/// Dim text in a background pane should fade further than dim text in the
/// focused pane.
///
/// Cursor cells in an unfocused pane get a separate `UNFOCUSED_CURSOR_ALPHA`
/// on the inverted bg so the ghost cursor reads as faint-but-present. We do
/// NOT also apply `UNFOCUSED_FG_ALPHA` to the cursor cell's fg (the
/// character under the cursor), because that double-fades the glyph to
/// near-invisible inside the ghost block.
fn effective_colors(run: &Run, pane_focused: bool, theme: &Theme) -> (gpui::Hsla, gpui::Hsla) {
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
