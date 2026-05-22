//! OxiMux app library surface.
//!
//! Module declarations live here (rather than `main.rs`) so integration
//! tests under `tests/` can `use oximux_app::shell::*;`. The `oximux`
//! binary at `src/main.rs` imports from this library.

pub mod actions;
pub mod assets;
pub mod notifier;
pub mod persisted_terminals;
pub mod project_panes_factory;
pub mod relay_supervisor;
pub mod shell;
pub mod state;
pub mod workspace_root;
