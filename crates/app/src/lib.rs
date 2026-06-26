//! OxiMux app library surface.
//!
//! Module declarations live here (rather than `main.rs`) so integration
//! tests under `tests/` can `use oximux_app::shell::*;`. The `oximux`
//! binary at `src/main.rs` imports from this library.

pub mod actions;
pub mod assets;
pub mod keymap_registry;
pub mod left_rail_layout;
pub mod notifier;
pub mod project_panes_factory;
pub mod shell;
pub mod state;
pub mod ui;
pub mod workspace_root;

// Still-loose modules (folded into groups in later Tier-1 reorg commits).
pub mod agent_awake;
pub mod agent_hooks_global;
pub mod agent_status_hooks;
pub mod app_nap;
pub mod browser_profiles;
pub mod custom_commands_loader;
pub mod file_http_client;
pub mod git_state_cache;
pub mod menu;
pub mod persisted_terminals;
pub mod project_scripts_loader;
pub mod relay_cold_restore;
pub mod relay_supervisor;
pub(crate) mod restore_fallback;
pub mod single_instance;
pub mod window_factory;
pub mod window_registry;

// --- Grouped module folders (Tier-1 reorg) ---------------------------------
// Files are foldered one level deep for traversal; each submodule is
// re-exported below so existing `crate::<name>::…` / `oximux_app::<name>::…`
// call sites keep resolving unchanged.
pub mod app_settings;

#[doc(inline)]
pub use app_settings::{
    agent_launch_settings, commit_message_ai_settings, keybindings_settings, motion_settings,
    scm_layout_settings, terminal_settings,
};
