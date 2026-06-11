//! OxiMux app library surface.
//!
//! Module declarations live here (rather than `main.rs`) so integration
//! tests under `tests/` can `use oximux_app::shell::*;`. The `oximux`
//! binary at `src/main.rs` imports from this library.

pub mod actions;
pub mod app_nap;
pub mod assets;
pub mod commit_message_ai_settings;
pub mod custom_commands_loader;
pub mod git_state_cache;
pub mod keymap;
pub mod left_rail_layout;
pub mod menu;
pub mod motion_settings;
pub mod notifier;
pub mod persisted_terminals;
pub mod project_panes_factory;
pub mod project_scripts_loader;
pub mod relay_cold_restore;
pub mod relay_supervisor;
pub(crate) mod restore_fallback;
pub mod scm_layout_settings;
pub mod shell;
pub mod state;
pub mod terminal_settings;
pub mod ui;
pub mod window_factory;
pub mod window_registry;
pub mod workspace_root;
