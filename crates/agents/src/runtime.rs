//! `AgentRuntime` trait — the seam every adapter implements.
//!
//! CLI adapters (Claude Code, Codex, Aider, custom-command) wrap one
//! PTY each. The future ACP runtime (v1.1) will wrap a JSON-RPC stream.
//! Both surface the same `AgentStatus` shape to the UI so the badge,
//! sidebar dot, and multi-agent dashboard don't care which runtime
//! produced them.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use oximux_core::{AgentAdapter, AgentSessionId, AgentStatus};

/// What to spawn for one agent session.
///
/// `prompt` is the initial user instruction. Adapters that accept stdin
/// pipe this in; adapters that take a `-p` arg (Claude Code) inject it
/// via `build_command`. `worktree_path` is the cwd — the worktree-per-
/// task workflow (Phase 2 git core) hands a fresh path in here.
#[derive(Debug, Clone)]
pub struct AgentSessionConfig {
    pub adapter: AgentAdapter,
    pub worktree_path: PathBuf,
    pub prompt: Option<String>,
    /// Optional model selector (e.g. `claude-opus-4-7`). Adapter ignores
    /// when the CLI has no `--model` flag.
    pub model: Option<String>,
    /// Optional reasoning effort hint (e.g. `high`). Adapter ignores when
    /// the CLI has no analog.
    pub effort: Option<String>,
    /// Extra env vars layered onto the PTY spawn env. Keys here override
    /// inherited env; do NOT inject secrets here — use the OS keychain
    /// path (Phase 4 storage) and have the adapter reference them.
    pub env: Vec<(String, String)>,
    /// Initial pane size in cells. Passed through to the PTY so the agent
    /// CLI sees a sane `$COLUMNS`/`$LINES` at first render.
    pub cols: u16,
    pub rows: u16,
}

/// Multi-consumer status subscription.
///
/// `tokio::sync::watch` lets pane-header badge + sidebar dot + dashboard
/// each hold their own `Receiver` over one `Sender`-owned `AgentStatus`.
/// The receiver always sees the latest value (no backpressure, no missed
/// updates between polls); intermediate transitions during a tick are
/// collapsed which is exactly the right semantic for status UI.
///
/// Raw byte / event streams are an internal runtime concern and not
/// exposed at this layer — adapters that need replay can plug their own
/// channel inside the impl.
pub type AgentStatusStream = tokio::sync::watch::Receiver<AgentStatus>;

/// The runtime trait. `Send + Sync + 'static` because the UI thread holds
/// a `Box<dyn AgentRuntime>` and the per-session reader tasks run on tokio.
///
/// `async-trait` is used to keep this `dyn`-compatible — native `async fn`
/// in traits is stable on the MSRV but yields a non-`dyn`-safe trait, and
/// the app layer needs `Box<dyn AgentRuntime>` (CLI vs ACP swap in v1.1).
#[async_trait]
pub trait AgentRuntime: Send + Sync + 'static {
    /// Start a new session. Returns a fresh `AgentSessionId`. The runtime
    /// is responsible for spawning the underlying PTY and starting the
    /// reader task before this resolves.
    async fn start_session(&self, cfg: AgentSessionConfig) -> Result<AgentSessionId>;

    /// Send user input to the session's stdin. For CLI adapters this is
    /// raw bytes written into the PTY (the agent CLI handles its own
    /// line discipline).
    async fn send_message(&self, id: AgentSessionId, msg: &str) -> Result<()>;

    /// Request graceful shutdown: SIGTERM → 5s grace → SIGKILL. Implementor
    /// MUST guarantee the process tree is reaped (zombie-free) before the
    /// returned future resolves.
    async fn cancel(&self, id: AgentSessionId) -> Result<()>;

    /// Subscribe to status updates for one session. `watch` semantics:
    /// every subscriber sees the latest value on first read and any
    /// subsequent transition. Callable any number of times; cheap clone.
    /// Returns `Err` only when `id` is unknown (already cancelled / never
    /// started).
    fn subscribe_status(&self, id: AgentSessionId) -> Result<AgentStatusStream>;

    /// Last-known status for one session (a `borrow()` shortcut for
    /// consumers that don't want to keep a subscription). Returns `Err`
    /// on unknown id.
    fn current_status(&self, id: AgentSessionId) -> Result<AgentStatus>;
}
