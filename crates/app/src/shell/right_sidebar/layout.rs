//! Layout constants for the right sidebar (top tab bar + panel body).

use gpui::Pixels;
use gpui::px;

/// Height of the horizontal tab bar across the top of the right sidebar.
/// top bar (was a 40px vertical right-edge strip pre-refactor).
pub const ACTIVITY_BAR_HEIGHT: Pixels = px(36.);

/// Total width of the right sidebar column (tabs + body share this).
pub const DEFAULT_PANEL_WIDTH: Pixels = px(360.);

/// Minimum panel width the user can resize down to (Phase 4 settings layer).
pub const MIN_PANEL_WIDTH: Pixels = px(220.);

/// Reserved right-side space: window width minus this is the resize cap.
pub const MAX_PANEL_WIDTH_RESERVE: Pixels = px(320.);

/// Thickness of the 2px active-tab indicator (bottom border of the active tab).
pub const ACTIVE_INDICATOR_THICKNESS: Pixels = px(2.);

/// Width of each individual tab button inside the top tab bar.
pub const TAB_BUTTON_WIDTH: Pixels = px(36.);
