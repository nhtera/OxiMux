//! Live cell metrics for the terminal grid.
//!
//! Centralises the `cell_width` / `line_height` math that used to live as
//! magic-number constants (8.4 × 17 px) in the legacy pane host. Measuring per render
//! via `text_system().advance(font_id, font_size, 'm').width` lets the grid
//! adapt when the user changes font or size in Typography, and removes the
//! drift risk where `terminal_view` thought a row was 17 px tall while
//! the host carved cells out of viewport space using a stale constant.
//!
//! Cost: one `resolve_font` + one `advance('m')` per `PaneGroup` render.
//! Both are cached inside GPUI's text system (LRU on `Font`), so steady-
//! state cost is a hash lookup. Measured live in Zed's terminal pipeline
//! the same way (`crates/terminal_view/src/terminal_element.rs:974–979`).

use gpui::{Pixels, Window, px};
use oximux_settings::Typography;

/// Extra pixels added on top of `t_body_lg` to derive line-height. Tuned
/// to Menlo's em-square (~16.9 px at 14 pt) plus a sliver of safety so
/// half-block glyphs (▀ ▄ █) tile cleanly. Anything looser leaves
/// vertical padding and breaks pixel-art mascots.
pub const LINE_HEIGHT_EXTRA: f32 = 3.0;

/// Fallback used when font resolution fails — e.g. headless tests with no
/// real text system, or a Typography that points at a missing primary
/// family before GPUI's fallback chain has cached anything. Roughly the
/// 'm' advance for Menlo 14 pt.
const FALLBACK_CELL_WIDTH: f32 = 8.4;

#[derive(Debug, Clone, Copy)]
pub struct CellMetrics {
    pub cell_width: f32,
    pub line_height: f32,
}

impl CellMetrics {
    /// Measure live against the active text system. `window` is taken by
    /// `&Window` (not `&mut`) so callers can use this from any read path
    /// — `PaneGroup`'s grid dispatch already only has `&Window`.
    pub fn measure(typography: &Typography, window: &Window) -> Self {
        let font_size = px(typography.t_body_lg);
        let line_height = typography.t_body_lg + LINE_HEIGHT_EXTRA;
        let font = typography.mono_font();
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(&font);
        let cell_width = text_system
            .advance(font_id, font_size, 'm')
            .ok()
            .map(|size| f32::from(size.width))
            .unwrap_or(FALLBACK_CELL_WIDTH);
        Self {
            cell_width,
            line_height,
        }
    }

    pub fn line_height_px(&self) -> Pixels {
        px(self.line_height)
    }

    /// Columns that fit in `width` px. Uses `next_up().floor()` instead of
    /// plain `floor()` so a width that's exactly `N * cell_width` doesn't
    /// drop to `N-1` due to f32 round-off — borrowed from Zed's
    /// `TerminalBounds::num_columns` (terminal.rs:220–223), which has 20
    /// regression tests for this exact class of bug.
    pub fn cols_in(&self, width: f32) -> u16 {
        ((width / self.cell_width).next_up().floor() as i32).max(0) as u16
    }

    /// Rows that fit in `height` px. Same precision trick as `cols_in`.
    pub fn rows_in(&self, height: f32) -> u16 {
        ((height / self.line_height).next_up().floor() as i32).max(0) as u16
    }
}
