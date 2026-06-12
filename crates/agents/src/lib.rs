//! oximux-agents
//!
//! `AgentRuntime` trait + CLI adapter trait + regex-based status machine.
//! Concrete adapters (Claude Code, Codex, Aider, custom) land in later
//! Phase 3 slices. The runtime owns one PTY per session via `oximux-pty`
//! and emits lifecycle events the UI badge + dashboard consume.

pub mod cli;
pub mod commit_message;
pub mod commit_message_heuristic;
pub mod registry;
pub mod runtime;
pub mod runtime_impl;
pub mod session_log;
pub mod status_machine;

pub use cli::{
    AiderAdapter, ClaudeCodeAdapter, CliAgentAdapter, CodexAdapter, CommandSpec,
    CustomCommandAdapter, StatusPattern,
};
pub use registry::{AdapterRegistry, RegistryEntry};
pub use runtime::{AgentRuntime, AgentSessionConfig, AgentStatusStream};
pub use runtime_impl::{CliRuntime, SharedBackend};
pub use status_machine::{StatusMachine, StatusTransition};
