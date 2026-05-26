//! Shell views — visual scaffolding only in Phase 0.
//!
//! Each child module is one zone of the cockpit. They take a `Theme + Density
//! + Typography` and return an `impl IntoElement` (RenderOnce). No state.

pub mod adapter_picker;
pub mod add_project_dialog;
pub mod agent_status_badge;
pub mod agent_status_task;
pub mod agent_tab_label;
pub mod cell_metrics;
pub mod command_palette;
pub mod commit_dialog;
pub mod confirm_dialog;
pub mod cwd_resolver;
pub mod diff_view;
pub mod file_explorer;
pub mod file_tree_context_menu;
pub mod file_tree_view;
pub mod git_panel;
pub mod key_input;
pub mod left_rail;
pub mod main_area;
pub mod openable_text_file;
pub mod pane_actions;
pub mod pane_content;
pub mod pane_group;
pub mod pane_group_manager;
pub mod pane_tree;
pub mod project_panes;
pub mod project_picker;
pub mod rename_tab_dialog;
pub mod right_sidebar;
pub mod search_panel;
pub mod source_control;
pub mod split_direction;
pub mod stash_panel;
pub mod status_bar;
pub mod tab_context_menu;
pub mod terminal_palette;
pub mod terminal_row;
pub mod terminal_search;
pub mod terminal_search_overlay;
pub mod terminal_search_state;
pub mod terminal_view;
pub mod top_bar;
pub mod welcome_view;
pub mod workspace_dialog;
pub mod workspace_ops;
pub mod worktree_panel;
