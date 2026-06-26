//! Shell views — visual scaffolding only in Phase 0.
//!
//! Each child module is one zone of the cockpit. They take a `Theme + Density
//! + Typography` and return an `impl IntoElement` (RenderOnce). No state.

pub mod agent_presentation;
pub mod agent_session_persistence;
pub mod ambient_agent_scan;
pub mod ambient_state;
pub mod agents_dashboard;
pub mod add_project_dialog;
pub mod agent_status_badge;
pub mod agent_status_task;
pub mod agent_tab_label;
pub mod browser_view;
pub mod command_palette;
pub mod commit_dialog;
pub mod compose_bar;
pub mod confirm_dialog;
pub mod context_env;
pub mod cwd_resolver;
pub mod diff_view;
pub mod divider;
pub mod file_explorer;
pub mod forge;
pub mod file_tree_context_menu;
pub mod file_tree_view;
pub mod git_panel;
pub mod left_rail;
pub mod main_area;
pub mod openable_text_file;
pub mod pane_actions;
pub mod pane_content;
pub mod pane_group;
pub mod pane_group_manager;
pub mod pane_tree;
pub mod open_url;
pub mod pr_dialog;
pub mod project_panes;
pub mod project_picker;
pub mod rename_tab_dialog;
pub mod right_sidebar;
pub mod search_panel;
pub mod session_history;
pub mod session_live_store;
pub mod session_merge;
pub mod settings_modal;
pub mod source_control;
pub mod split_direction;
pub mod stash_panel;
pub mod status_bar;
pub mod tab_context_menu;
pub mod usage_meter;
#[cfg(target_os = "macos")]
pub mod usage_popover;
pub mod tasks_view;
pub mod toast;
pub mod top_bar;
pub mod welcome_actions;
pub mod welcome_flow;
pub mod welcome_view;
pub mod workspace_dialog;
pub mod workspace_ops;
pub mod worktree_panel;

// Terminal surface — clustered into shell/terminal/ for traversal. Re-exported
// here so existing `crate::shell::<name>::…` paths keep resolving unchanged.
pub mod terminal;

#[doc(inline)]
pub use terminal::{
    adapter_picker, box_drawing, cell_metrics, floating_terminal, floating_terminal_host,
    floating_terminal_persistence, key_input, mouse_report, terminal_canvas, terminal_links,
    terminal_palette, terminal_row, terminal_scrollbar, terminal_search, terminal_search_overlay,
    terminal_search_state, terminal_view,
};
