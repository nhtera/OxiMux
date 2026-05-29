//! Vector rendering for Unicode box-drawing glyphs (U+2500–U+257F).
//!
//! ## Why a custom path
//!
//! Box-drawing is where the font path falls apart. Each cell ships its
//! glyph with the font's own metrics — typically a tiny gap on the right
//! or bottom edge so the glyph doesn't bleed into the next cell — and
//! the cell-to-cell join becomes a visible hairline when many cells line
//! up to form a TUI border (vim splits, lazygit panes, htop frames). On
//! HiDPI the gap is a sub-pixel artifact that fluctuates with the
//! window's position. The fix is to draw box-drawing characters
//! programmatically as path strokes that ignore the font advance
//! entirely.
//!
//! ## Integration
//!
//! `terminal_canvas::paint_grid` runs a per-row scan that bails out for
//! rows with no box-drawing characters (the common case) so plain text
//! pays nothing. Rows that do contain box chars get two passes:
//!
//! 1. **Horizontal merge** — consecutive cells with the same horizontal
//!    weight + same fg color collapse into ONE `paint::draw_horizontal_span`
//!    call. The span is a single `PathBuilder` stroke from the leftmost
//!    cell's left edge to the rightmost cell's right edge — no joints,
//!    no seams.
//! 2. **Standalone** — every other box-drawing cell paints via
//!    `paint::draw_box_character` with a 1-px overlap on each edge so
//!    adjacent cells fuse without a hairline gap.
//!
//! The text pass in `paint_grid` replaces every box-drawing char with a
//! space before shaping so the font glyph never paints. Result: only the
//! vector stroke is visible.

mod paint;
mod segments;

pub use paint::{draw_box_character, draw_horizontal_span, draw_vertical_components};
pub use segments::{get_horizontal_weight, has_vector_impl, is_box_drawing_char};
