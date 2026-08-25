//! CLI agent adapters.
//!
//! The trait + helper types live here; concrete adapters
//! (claude_code / codex / pi / custom) land in later Phase 3 slices.

pub mod adapter;
pub mod claude_code;
pub mod codex;
pub mod custom;
pub(crate) mod detect;
pub mod omp;
pub mod pi;

pub use adapter::{CliAgentAdapter, CommandSpec, StatusPattern};
pub use detect::{program_for_spawn, resolve_on_path, resolve_on_path_blocking, which_on_path};
pub use claude_code::ClaudeCodeAdapter;
pub use codex::CodexAdapter;
pub use custom::CustomCommandAdapter;
pub use omp::OmpAdapter;
pub use pi::PiAdapter;
