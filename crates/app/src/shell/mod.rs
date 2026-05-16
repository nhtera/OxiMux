//! Shell views — visual scaffolding only in Phase 0.
//!
//! Each child module is one zone of the cockpit. They take a `Theme + Density
//! + Typography` and return an `impl IntoElement` (RenderOnce). No state.

pub mod cell_metrics;
pub mod key_input;
pub mod main_area;
pub mod main_pane;
pub mod pane_layout;
pub mod pane_tree;
pub mod sidebar;
pub mod status_bar;
pub mod tabbed_pane;
pub mod terminal_palette;
pub mod terminal_row;
pub mod terminal_search;
pub mod terminal_search_overlay;
pub mod terminal_search_state;
pub mod terminal_view;
pub mod top_bar;
