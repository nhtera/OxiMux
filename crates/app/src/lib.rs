//! OxiMux app library surface.
//!
//! Module declarations live here (rather than `main.rs`) so integration
//! tests under `tests/` can `use oximux_app::shell::*;`. The `oximux`
//! binary at `src/main.rs` imports from this library.

pub mod actions;
pub mod assets;
pub mod commit_message_ai_settings;
pub mod keymap;
pub mod notifier;
pub mod persisted_terminals;
pub mod project_panes_factory;
pub mod relay_supervisor;
pub mod scm_layout_settings;
pub mod shell;
pub mod state;
pub mod terminal_settings;
pub mod window_factory;
pub mod window_registry;
pub mod workspace_root;
