//! CLI agent adapters.
//!
//! The trait + helper types live here; concrete adapters
//! (claude_code / codex / aider / custom) land in later Phase 3 slices.

pub mod adapter;
pub mod custom;

pub use adapter::{CliAgentAdapter, CommandSpec, StatusPattern};
pub use custom::CustomCommandAdapter;
