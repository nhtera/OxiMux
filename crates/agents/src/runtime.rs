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
use oximux_core::{AgentAdapter, AgentSessionId, AgentSnapshot, AgentStatus};

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
    /// Optional model selector (e.g. `claude-opus-5`). Adapter ignores
    /// when the CLI has no `--model` flag.
    pub model: Option<String>,
    /// Optional reasoning effort hint (e.g. `high`). Adapter ignores when
    /// the CLI has no analog.
    pub effort: Option<String>,
    /// User-configured extra CLI flags (from `agent_launch.toml`), already
    /// split into argv tokens. Each adapter appends these after its own
    /// model/effort flags and before any positional prompt, so a launch
    /// default like a skip-permissions flag reaches the spawned binary.
    pub extra_args: Vec<String>,
    /// Extra env vars layered onto the PTY spawn env. Keys here override
    /// inherited env; do NOT inject secrets here — use the OS keychain
    /// path (Phase 4 storage) and have the adapter reference them.
    pub env: Vec<(String, String)>,
    /// Initial pane size in cells. Passed through to the PTY so the agent
    /// CLI sees a sane `$COLUMNS`/`$LINES` at first render.
    pub cols: u16,
    pub rows: u16,
    /// Custom-adapter input: `(program, args)`. Only the `Custom` adapter
    /// reads this; other adapters hard-code their binary in `build_command`
    /// and ignore the field. The launch dialog (step 10) populates it when
    /// the user picks "Custom command".
    pub custom_command: Option<(String, Vec<String>)>,
    /// Whether this spawn resumes / forks a prior session. `None` for a fresh
    /// launch; each adapter's `build_command` maps the variant to its own
    /// resume/fork CLI shape.
    pub resumption: oximux_core::SessionResumption,
}

/// Multi-consumer status subscription.
///
/// `tokio::sync::watch` lets pane-header badge + sidebar dot + dashboard
/// each hold their own `Receiver` over one `Sender`-owned `AgentSnapshot`.
/// The receiver always sees the latest value (no backpressure, no missed
/// updates between polls); intermediate transitions during a tick are
/// collapsed which is exactly the right semantic for status UI.
///
/// The payload is an `AgentSnapshot` (status + optional OSC-9999 sideband
/// `detail`), not a bare `AgentStatus`, so sideband-fed consumers can read
/// the live tool step / message off the same channel. Consumers that only
/// care about the lifecycle read `snapshot.status`; `current_status()` still
/// hands back a bare `AgentStatus` for that common case.
///
/// Raw byte / event streams are an internal runtime concern and not
/// exposed at this layer — adapters that need replay can plug their own
/// channel inside the impl.
pub type AgentStatusStream = tokio::sync::watch::Receiver<AgentSnapshot>;

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

    /// Request graceful shutdown: SIGTERM → 5 s grace → SIGKILL. Implementor
    /// MUST guarantee the process tree is reaped (zombie-free) before the
    /// returned future resolves. `CliRuntime` honors this via
    /// `PortablePtyBackend::close()`, which signals the child's process
    /// group then joins the watcher thread that calls `child.wait()`.
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
