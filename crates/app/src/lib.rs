//! OxiMux app library surface.
//!
//! Module declarations live here (rather than `main.rs`) so integration
//! tests under `tests/` can `use oximux_app::shell::*;`. The `oximux`
//! binary at `src/main.rs` imports from this library.

pub mod actions;
pub mod assets;
pub mod notifier;
pub mod shell;
pub mod workspace_root;
