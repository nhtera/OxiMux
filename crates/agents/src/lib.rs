//! oximux-agents
//!
//! `AgentRuntime` trait + CLI adapter trait + regex-based status machine.
//! Concrete adapters (Claude Code, Codex, Aider, custom) land in later
//! Phase 3 slices. The runtime owns one PTY per session via `oximux-pty`
//! and emits lifecycle events the UI badge + dashboard consume.

pub mod agent_process;
pub mod agent_title;
pub mod cli;
pub mod command_envelope;
pub mod commit_message;
pub mod commit_message_heuristic;
pub mod coord;
pub mod osc_sideband;
pub mod poll_helpers;
pub mod registry;
pub mod runtime;
pub mod schedule;
pub mod session_registry;
pub mod runtime_impl;
pub mod session_log;
pub mod status_machine;
pub mod tab_title;
pub mod team;
pub mod thread;

pub use agent_process::agent_label_for_process;
pub use agent_title::{agent_label_from_title, classify_agent_title};
pub use cli::{
    AiderAdapter, ClaudeCodeAdapter, CliAgentAdapter, CodexAdapter, CommandSpec,
    CustomCommandAdapter, StatusPattern,
};
pub use osc_sideband::AgentOscScanner;
pub use registry::{AdapterRegistry, RegistryEntry};
pub use runtime::{AgentRuntime, AgentSessionConfig, AgentStatusStream};
pub use runtime_impl::{CliRuntime, SharedBackend};
pub use status_machine::{StatusMachine, StatusTransition};
