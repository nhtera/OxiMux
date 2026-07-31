//! CLI agent adapters.
//!
//! The trait + helper types live here; concrete adapters
//! (claude_code / codex / aider / custom) land in later Phase 3 slices.

pub mod adapter;
pub mod aider;
pub mod claude_code;
pub mod codex;
pub mod custom;
pub(crate) mod detect;
pub mod pi;

pub use adapter::{CliAgentAdapter, CommandSpec, StatusPattern};
pub use detect::{resolve_on_path, which_on_path};
pub use aider::AiderAdapter;
pub use claude_code::ClaudeCodeAdapter;
pub use codex::CodexAdapter;
pub use custom::CustomCommandAdapter;
pub use pi::PiAdapter;
