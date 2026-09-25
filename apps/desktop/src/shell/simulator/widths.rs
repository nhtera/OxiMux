//! Right-sidebar widths for the simulator tab.
//!
//! A phone needs more room than the sidebar's 360 default, so the first time
//! the tab is shown the width bumps to [`FIRST_SELECT_WIDTH`], and ⤢ widens it
//! to everything but the centre panes' reserve. Both are *transient*: only a
//! user's drag persists a width, so there is one width source of truth and
//! "restore" returns to it. Every value goes through `clamp_panel_width`, so a
//! narrow window never squeezes the centre panes below their floor.

use crate::app_settings::scm_layout_settings::{MAX_PANEL_WIDTH_RESERVE, clamp_panel_width};

/// Width the sidebar grows to (at least) when the simulator tab first opens.
pub const FIRST_SELECT_WIDTH: f32 = 440.0;

/// The width for the first select: never narrower than it already is.
pub fn first_select_width(current: f32, window_width: f32) -> f32 {
    clamp_panel_width(current.max(FIRST_SELECT_WIDTH), window_width)
}

/// The maximized width: the window minus the centre panes' reserve.
pub fn maximized_width(window_width: f32) -> f32 {
    clamp_panel_width(window_width - MAX_PANEL_WIDTH_RESERVE, window_width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_settings::scm_layout_settings::MIN_PANEL_WIDTH;

    #[test]
    fn first_select_bumps_but_never_shrinks_and_respects_the_window() {
        assert_eq!(first_select_width(360.0, 1600.0), FIRST_SELECT_WIDTH);
        assert_eq!(first_select_width(600.0, 1600.0), 600.0);
        // A narrow window caps it at its ceiling.
        assert_eq!(first_select_width(360.0, 700.0), 700.0 - MAX_PANEL_WIDTH_RESERVE);
    }

    #[test]
    fn maximize_leaves_the_centre_reserve_and_never_goes_below_the_floor() {
        assert_eq!(maximized_width(1600.0), 1600.0 - MAX_PANEL_WIDTH_RESERVE);
        assert_eq!(maximized_width(400.0), MIN_PANEL_WIDTH);
    }
}
