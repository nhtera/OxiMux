//! The `TerminalBackend` contract.
//!
//! One trait object per concrete source (real PTY, fixture replay, ACP).
//! The UI layer in `crates/app` owns a `Box<dyn TerminalBackend>` and
//! polls it via `drain_events` once per frame.

use anyhow::Result;
use std::path::PathBuf;

use crate::events::TerminalEvent;
use crate::snapshot::TerminalSnapshot;

/// Opaque handle to one running PTY session. Backends mint these monotonically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TerminalSessionId(pub u64);

/// What to spawn. The caller picks shell + cwd + env + initial size; the
/// backend is responsible for honoring all four exactly.
#[derive(Debug, Clone)]
pub struct SpawnConfig {
    pub shell: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
}

impl Default for SpawnConfig {
    fn default() -> Self {
        Self {
            shell: std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into()),
            args: Vec::new(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            env: Vec::new(),
            cols: 80,
            rows: 24,
        }
    }
}

/// Trait every terminal source implements.
///
/// `Send + 'static` because the UI thread holds the handle and the reader
/// task runs on a tokio thread; ownership must move freely between them.
pub trait TerminalBackend: Send + 'static {
    /// Spawn a new session. Returns a fresh `TerminalSessionId`.
    fn spawn(&mut self, cfg: SpawnConfig) -> Result<TerminalSessionId>;

    /// Write user input (keypresses, paste, programmatic input) to the session.
    fn write(&mut self, id: TerminalSessionId, bytes: &[u8]) -> Result<()>;

    /// Resize the PTY when the rendering area changes.
    fn resize(&mut self, id: TerminalSessionId, cols: u16, rows: u16) -> Result<()>;

    /// A renderable snapshot of the session's current state. Step 1-2 ships
    /// an empty snapshot — step 3 wires `alacritty_terminal` to produce
    /// real rows and a cursor position.
    fn snapshot(&self, id: TerminalSessionId) -> Result<TerminalSnapshot>;

    /// True when the session has DECSET 2004 (bracketed paste) enabled.
    /// Callers use this to decide whether pasted clipboard text should be
    /// wrapped with `\e[200~` / `\e[201~`. Default impl returns `false` so
    /// fixture / replay backends don't have to care.
    fn bracketed_paste(&self, _id: TerminalSessionId) -> Result<bool> {
        Ok(false)
    }

    /// Drain accumulated events without blocking. Returns empty when idle.
    fn drain_events(&mut self) -> Vec<TerminalEvent>;

    /// Tear down a session. Idempotent — safe to call on an already-closed id.
    fn close(&mut self, id: TerminalSessionId) -> Result<()>;
}
