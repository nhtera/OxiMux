//! Geometry + drag math for the terminal's overlay scrollbar.
//!
//! The terminal scrolls via alacritty's `display_offset` (lines back from the
//! live tail), not a GPUI pixel scroll handle, so the shared `vertical_scrollbar`
//! component doesn't apply. This module keeps the pure math (thumb size/position
//! and drag → offset mapping) separate and unit-tested; `terminal_view` owns the
//! element + event wiring.

/// Below this fraction of the track the thumb gets too small to grab, so it's
/// clamped up to stay usable on very deep scrollback.
const MIN_THUMB_FRACTION: f32 = 0.05;

/// Drag state for the scrollbar thumb. Captured on mouse-down, read on each
/// mouse-move until release.
#[derive(Clone, Copy)]
pub(crate) struct ScrollbarDrag {
    /// Global Y (px) where the drag began.
    pub start_y: f32,
    /// `display_offset` at drag start (lines back from the live tail).
    pub start_offset: usize,
}

/// Thumb position + size as fractions [0,1] of the track height: returns
/// `(top_fraction, height_fraction)`.
///
/// `visible_rows` is the viewport height in lines, `history_len` the off-screen
/// scrollback above it, `display_offset` how many lines back from the tail the
/// viewport currently sits (0 = pinned to the bottom). The full buffer is
/// `history_len + visible_rows` lines tall; the thumb covers the visible slice,
/// and its top is the viewport's top line measured from the buffer top.
pub(crate) fn thumb_geometry(
    history_len: usize,
    display_offset: usize,
    visible_rows: usize,
) -> (f32, f32) {
    let total = (history_len + visible_rows).max(1) as f32;
    let height = (visible_rows as f32 / total).clamp(MIN_THUMB_FRACTION, 1.0);
    let top_line = history_len.saturating_sub(display_offset) as f32;
    // Keep the thumb fully inside the track even after the min-height clamp.
    let top = (top_line / total).clamp(0.0, 1.0 - height);
    (top, height)
}

/// Map a thumb drag to a new absolute `display_offset`. `dy_px` is the cursor's
/// vertical travel from the drag start (down = positive); `track_px` the track's
/// pixel height. Dragging down moves toward the live tail, so the offset shrinks.
/// The result is clamped to `[0, history_len]`.
pub(crate) fn drag_to_offset(
    drag: ScrollbarDrag,
    dy_px: f32,
    track_px: f32,
    history_len: usize,
    visible_rows: usize,
) -> usize {
    if track_px <= 0.0 {
        return drag.start_offset.min(history_len);
    }
    let total = (history_len + visible_rows).max(1) as f32;
    let px_per_line = track_px / total;
    if px_per_line <= 0.0 {
        return drag.start_offset.min(history_len);
    }
    let lines = (dy_px / px_per_line).round() as i64;
    let new_offset = drag.start_offset as i64 - lines;
    new_offset.clamp(0, history_len as i64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumb_at_tail_sits_at_the_bottom() {
        // 100 history + 20 visible, pinned to tail (offset 0): thumb covers the
        // last 20/120 of the track and its top is at 100/120.
        let (top, height) = thumb_geometry(100, 0, 20);
        assert!((height - 20.0 / 120.0).abs() < 1e-4);
        assert!((top - 100.0 / 120.0).abs() < 1e-4);
        // top + height == 1.0 → flush with the track bottom.
        assert!((top + height - 1.0).abs() < 1e-4);
    }

    #[test]
    fn thumb_scrolled_to_top_sits_at_the_top() {
        // Fully scrolled back (offset == history_len): top fraction is 0.
        let (top, _h) = thumb_geometry(100, 100, 20);
        assert!(top.abs() < 1e-4);
    }

    #[test]
    fn deep_scrollback_clamps_thumb_to_min_height() {
        // 10_000 history, 20 visible → raw fraction ~0.002, clamped up.
        let (_top, height) = thumb_geometry(10_000, 0, 20);
        assert!((height - MIN_THUMB_FRACTION).abs() < 1e-4);
    }

    #[test]
    fn empty_history_is_safe() {
        // No scrollback: thumb fills the track, no div-by-zero.
        let (top, height) = thumb_geometry(0, 0, 20);
        assert_eq!(top, 0.0);
        assert!((height - 1.0).abs() < 1e-4);
    }

    #[test]
    fn drag_down_moves_toward_tail() {
        // 100 history + 20 visible over a 120px track = 1px/line. Start scrolled
        // back 50; drag down 30px → 30 lines toward the tail → offset 20.
        let drag = ScrollbarDrag { start_y: 0.0, start_offset: 50 };
        assert_eq!(drag_to_offset(drag, 30.0, 120.0, 100, 20), 20);
    }

    #[test]
    fn drag_up_moves_into_history_and_clamps() {
        // Drag up (negative) past the top clamps at history_len.
        let drag = ScrollbarDrag { start_y: 0.0, start_offset: 50 };
        assert_eq!(drag_to_offset(drag, -1000.0, 120.0, 100, 20), 100);
    }

    #[test]
    fn zero_track_is_a_noop() {
        let drag = ScrollbarDrag { start_y: 0.0, start_offset: 7 };
        assert_eq!(drag_to_offset(drag, 50.0, 0.0, 100, 20), 7);
    }
}
