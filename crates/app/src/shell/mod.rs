//! Shell views — visual scaffolding only in Phase 0.
//!
//! Each child module is one zone of the cockpit. They take a `Theme + Density
//! + Typography` and return an `impl IntoElement` (RenderOnce). No state.

pub mod cell_metrics;
pub mod commit_dialog;
pub mod confirm_dialog;
pub mod diff_view;
pub mod git_panel;
pub mod key_input;
pub mod left_rail;
pub mod main_area;
pub mod main_pane;
pub mod pane_layout;
pub mod pane_tree;
pub mod right_sidebar;
pub mod stash_panel;
pub mod status_bar;
pub mod tabbed_pane;
pub mod terminal_palette;
pub mod terminal_row;
pub mod terminal_search;
pub mod terminal_search_overlay;
pub mod terminal_search_state;
pub mod terminal_view;
pub mod top_bar;
pub mod worktree_panel;
