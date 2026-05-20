//! `CliAgentAdapter` — the per-CLI contract.
//!
//! One impl per agent binary (Claude Code, Codex, Aider, custom-command).
//! The adapter is pure metadata + arg-construction; the actual PTY spawn
//! lives in the runtime so all adapters share one zombie-prevention path.

use anyhow::Result;
use async_trait::async_trait;
use oximux_core::AgentStatus;
use regex::bytes::Regex;
use std::path::PathBuf;

use crate::runtime::AgentSessionConfig;

/// What to spawn in the PTY. The runtime takes this, layers in standard
/// PTY config (cols/rows from `AgentSessionConfig`, sandboxed env), and
/// hands it to `oximux-pty`.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    /// Absolute path or bare name resolvable on PATH.
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Adapter-specific env layered on top of the session env. Useful for
    /// things like `CLAUDE_CODE_DISABLE_TELEMETRY=1` that should apply
    /// regardless of user config.
    pub env: Vec<(String, String)>,
    /// Initial bytes to write to stdin after spawn (some adapters take
    /// the prompt that way instead of `-p`). The runtime writes these
    /// once the PTY is ready.
    pub stdin_seed: Option<Vec<u8>>,
}

/// One regex-driven transition rule. The status machine scans the recent
/// output ring for the first matching pattern; first match wins so order
/// in the adapter's `status_patterns()` slice is significant.
///
/// `regex::bytes::Regex` is used (not `regex::Regex`) because PTY output
/// is raw bytes — ANSI escapes, possibly invalid UTF-8 — so we match the
/// haystack as bytes and avoid the lossy decode on every chunk.
#[derive(Debug, Clone)]
pub struct StatusPattern {
    pub regex: Regex,
    pub transition: AgentStatus,
}

impl StatusPattern {
    /// Convenience: compile a pattern at construction time. Returns
    /// `Err` if the regex is invalid — adapters should propagate this
    /// up to `detect()` or surface a startup-time error rather than
    /// crashing mid-session.
    pub fn new(pattern: &str, transition: AgentStatus) -> Result<Self> {
        Ok(Self {
            regex: Regex::new(pattern)?,
            transition,
        })
    }
}

/// One CLI agent adapter. Stateless — the runtime instantiates once and
/// reuses across sessions. All session state lives in the runtime's
/// session table.
#[async_trait]
pub trait CliAgentAdapter: Send + Sync + 'static {
    /// Stable id for storage / config (`claude-code`, `codex`, `aider`,
    /// `custom`). Used as the SQLite discriminator in Phase 4.
    fn id(&self) -> &'static str;

    /// Human label for the launch dialog dropdown.
    fn name(&self) -> &'static str;

    /// Check whether this agent is installed. Implementations should be
    /// cheap (typically `which <binary>`) — startup `registry.rs` calls
    /// this for every known adapter on cold boot. Returning `Ok(false)`
    /// hides the adapter from the launch dialog; `Err` is logged and
    /// treated as unavailable.
    async fn detect(&self) -> Result<bool>;

    /// Build the spawn spec for one session. Pure: no I/O, no spawning.
    /// The runtime is responsible for the actual PTY launch.
    fn build_command(&self, cfg: &AgentSessionConfig) -> Result<CommandSpec>;

    /// Regex table for the status machine. Returned by reference so the
    /// machine can scan without cloning the patterns per session.
    fn status_patterns(&self) -> &[StatusPattern];
}
