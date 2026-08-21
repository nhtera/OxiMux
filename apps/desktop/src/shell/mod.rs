//! Shell views — visual scaffolding only in Phase 0.
//!
//! Each child module is one zone of the cockpit. They take a `Theme + Density
//! + Typography` and return an `impl IntoElement` (RenderOnce). No state.

// Concern folders — one folder per cockpit domain (views + their helpers).
pub mod agent_chat;
pub mod agent_ui;
pub mod agents_dashboard;
pub mod automations_view;
pub mod browser_view;
pub mod chrome;
pub mod command_palette;
pub mod commit_dialog;
pub mod compose_bar;
pub mod diff_view;
#[cfg(target_os = "macos")]
pub mod driver_install;
pub mod file_explorer;
pub mod forge;
pub mod git_panel;
pub mod left_rail;
pub mod onboarding;
pub mod pane_group;
pub mod panes;
pub mod ports_panel;
pub mod pr_dialog;
pub mod project_panes;
pub mod right_sidebar;
pub mod search_panel;
pub mod session_history;
pub mod settings_modal;
pub mod source_control;
pub mod stash_panel;
pub mod tasks_view;
pub mod tool_paths;
pub mod usage;
pub mod welcome;
pub mod workspace;
pub mod worktree_panel;

// Cross-cutting glue — tiny, genuinely cross-domain modules kept loose at the
// shell/ root by design; foldering them into a concern folder buys nothing.
pub mod context_env;
pub mod cwd_resolver;
pub mod open_url;
pub mod openable_text_file;

// Re-exports — keep every existing `crate::shell::<name>` module path stable
// after folding loose modules into concern folders. Consumers (including
// external integration tests via `oximux_app::shell::<name>`) resolve unchanged.
pub use agent_ui::{
    agent_presentation, agent_process_scan, agent_session_persistence, agent_status_badge,
    agent_status_task, agent_tab_label, ambient_agent_scan, ambient_state, session_live_store,
};
pub use chrome::{divider, rename_tab_dialog, status_bar, tab_context_menu, toast, top_bar};
pub use file_explorer::{file_tree_context_menu, file_tree_view};
pub use panes::{
    main_area, pane_actions, pane_content, pane_group_manager, pane_tree, split_direction,
};
pub use usage::usage_meter;
#[cfg(target_os = "macos")]
pub use usage::usage_popover;
pub use welcome::{welcome_actions, welcome_flow, welcome_view};
pub use workspace::{
    add_project_dialog, project_picker, session_merge, workspace_dialog, workspace_ops,
};

// `confirm_dialog` is a generic, app-agnostic widget now living in oximux-ui.
// Re-exported here so existing `crate::shell::confirm_dialog::…` paths resolve.
pub use oximux_ui::confirm_dialog;

// Terminal surface — clustered into shell/terminal/ for traversal. Re-exported
// here so existing `crate::shell::<name>::…` paths keep resolving unchanged.
pub mod terminal;

#[doc(inline)]
pub use terminal::{
    adapter_picker, box_drawing, cell_metrics, floating_terminal, floating_terminal_host,
    floating_terminal_persistence, key_input, mouse_report, terminal_canvas, terminal_context_menu,
    terminal_links, terminal_palette, terminal_row, terminal_scrollbar, terminal_search,
    terminal_search_overlay, terminal_search_state, terminal_view,
};
