//! Right-sidebar widths for the simulator tab.
//!
//! Normal ("Split"): wide enough for a phone at full height beside the centre
//! panes — applied the first time the tab shows, never narrower than the
//! sidebar already is. "Fill" does not use a width: the sidebar takes the
//! centre's place (see `RightSidebar::fill`). Both are transient; only a
//! user's drag persists a width. Every width goes through
//! `clamp_panel_width`, so a narrow window keeps the centre's floor.

use crate::app_settings::scm_layout_settings::clamp_panel_width;

/// Vertical space the sidebar spends on things other than the phone: the tab
/// strip, the panel header, the toolbar pill and paddings.
const CHROME_H: f32 = 180.0;
/// Horizontal room around the phone (paddings, side buttons).
const MARGIN_W: f32 = 72.0;
/// Width ÷ height of the phone outline (screen plus bezel).
pub const PHONE_ASPECT: f32 = 0.49;
/// Never bump below this, even in a short window.
const MIN_SPLIT_WIDTH: f32 = 440.0;

/// The width that fits a full-height phone in a window of this size.
pub fn split_width(window_w: f32, window_h: f32) -> f32 {
    let phone_w = (window_h - CHROME_H).max(0.0) * PHONE_ASPECT;
    clamp_panel_width((phone_w + MARGIN_W).max(MIN_SPLIT_WIDTH), window_w)
}

/// The first-show width: fit the phone, but never shrink a wider sidebar.
pub fn first_select_width(current: f32, window_w: f32, window_h: f32) -> f32 {
    clamp_panel_width(current.max(split_width(window_w, window_h)), window_w)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_settings::scm_layout_settings::MAX_PANEL_WIDTH_RESERVE;

    #[test]
    fn split_fits_a_full_height_phone() {
        let w = split_width(3000.0, 1000.0);
        assert_eq!(w, (1000.0 - CHROME_H) * PHONE_ASPECT + MARGIN_W);
        // A short window never goes below the floor.
        assert_eq!(split_width(3000.0, 400.0), MIN_SPLIT_WIDTH);
    }

    #[test]
    fn first_select_never_shrinks_and_respects_the_window() {
        assert_eq!(first_select_width(900.0, 3000.0, 1000.0), 900.0);
        assert_eq!(first_select_width(0.0, 700.0, 1000.0), 700.0 - MAX_PANEL_WIDTH_RESERVE);
    }
}
