//! oximux-agents
//!
//! `AgentRuntime` trait + CLI adapters for Claude Code, Codex, and Aider.
//! Each adapter spawns the CLI binary inside a PTY (provided by `oximux-pty`)
//! and detects lifecycle events (approval prompts, completion, error) by
//! pattern-matching the terminal stream. ACP runtime deferred to v1.1.
//!
//! Phase 0 = skeleton; adapters land in Phase 3.

use anyhow::Result;
use oximux_core::AgentAdapter;

/// Lifecycle event surfaced to the UI cockpit. The dashboard in Phase 7
/// aggregates these across all live sessions.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    Started,
    AwaitingApproval(String),
    Output(String),
    Completed,
    Errored(String),
}

/// Runtime trait every adapter implements. CLI adapters wrap a PTY; the
/// future ACP runtime will wrap a JSON-RPC stream over stdio/socket.
pub trait AgentRuntime: Send + Sync {
    fn adapter(&self) -> AgentAdapter;
    fn start(&mut self) -> Result<()>;
    fn send_input(&mut self, _input: &str) -> Result<()>;
    fn poll(&mut self) -> Result<Vec<AgentEvent>>;
}
