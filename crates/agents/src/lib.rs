//! oximux-agents
//!
//! `AgentRuntime` trait + CLI adapter trait + regex-based status machine.
//! Concrete adapters (Claude Code, Codex, Aider, custom) land in later
//! Phase 3 slices. The runtime owns one PTY per session via `oximux-pty`
//! and emits lifecycle events the UI badge + dashboard consume.

pub mod cli;
pub mod runtime;
pub mod status_machine;

pub use cli::{CliAgentAdapter, CommandSpec, StatusPattern};
pub use runtime::{AgentRuntime, AgentSessionConfig, AgentStatusStream};
pub use status_machine::{StatusMachine, StatusTransition};
