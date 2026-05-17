//! Layout constants for the right sidebar and activity bar strip.

use gpui::Pixels;
use gpui::px;

/// Width of the vertical icon strip on the right edge of the sidebar.
pub const ACTIVITY_BAR_WIDTH: Pixels = px(40.);

/// Default panel content width (excludes the activity bar).
pub const DEFAULT_PANEL_WIDTH: Pixels = px(320.);

/// Minimum panel content width the user can resize down to.
pub const MIN_PANEL_WIDTH: Pixels = px(220.);

/// Reserved right-side space: window width minus this is the resize cap.
pub const MAX_PANEL_WIDTH_RESERVE: Pixels = px(320.);

/// Width of the 2px active-tab indicator drawn on the right edge of the icon.
pub const ACTIVE_INDICATOR_WIDTH: Pixels = px(2.);
