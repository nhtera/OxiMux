//! The `TerminalBackend` contract.
//!
//! One trait object per concrete source (real PTY, fixture replay, ACP).
//! The UI layer in `crates/app` owns a `Box<dyn TerminalBackend>` and
//! polls it via `drain_events` once per frame.

use anyhow::Result;
use std::path::PathBuf;

use crate::events::TerminalEvent;
use crate::snapshot::{Cell, TerminalSnapshot};

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

    /// F3.4: register a session that has a grid emulator but NO live
    /// PTY child yet. The caller usually follows up with `prefill_grid`
    /// to populate restored scrollback and renders the snapshot. Until
    /// `promote_to_live` runs, `write` / `resize` (on the PTY side) /
    /// `os_pid` / `external_id_of` all surface "dormant" errors.
    ///
    /// Default impl returns an error — only backends that implement
    /// dormancy override (currently the in-process portable-pty backend).
    fn spawn_dormant(
        &mut self,
        _cols: u16,
        _rows: u16,
    ) -> Result<TerminalSessionId> {
        Err(anyhow::anyhow!(
            "this backend does not support dormant sessions"
        ))
    }

    /// F3.4: promote a dormant session to live by spawning a PTY child
    /// and wiring read/write. The grid emulator + any prefilled
    /// scrollback survive intact; the user sees a continuous transition
    /// from the "restored" view into a fresh prompt at `cfg.cwd`.
    ///
    /// Default impl returns an error — only backends that implement
    /// dormancy override.
    fn promote_to_live(
        &mut self,
        _id: TerminalSessionId,
        _cfg: SpawnConfig,
    ) -> Result<()> {
        Err(anyhow::anyhow!(
            "this backend does not support promote_to_live"
        ))
    }

    /// Attach to a session that already exists in this backend's
    /// underlying source (e.g., a daemon-owned PTY that outlived the
    /// previous app launch). The default implementation returns an
    /// error — only relay-backed backends meaningfully implement it.
    /// Phase 06 reconciliation calls this before falling back to a
    /// fresh `spawn` + visual prefill.
    fn attach_existing(&mut self, _external_id: &str) -> Result<TerminalSessionId> {
        Err(anyhow::anyhow!(
            "this backend does not support attach_existing"
        ))
    }

    /// Local-side identifier this backend would use to persist the
    /// given session for cross-restart re-attach (e.g., the relay
    /// daemon's PTY id). Default returns `None` — backends without
    /// cross-restart semantics opt out by leaving the default.
    fn external_id_of(&self, _id: TerminalSessionId) -> Option<String> {
        None
    }

    /// Every external id this backend currently knows about on its
    /// remote source (e.g., live relay PTYs). Phase 06 reconciliation
    /// calls this once per project switch to learn which persisted
    /// ids are still attachable. Default returns empty — in-process
    /// backends have no external namespace.
    fn list_external_ids(&self) -> Vec<String> {
        Vec::new()
    }

    /// Opaque session-identity string for the backend's remote source
    /// (e.g., the relay daemon's `HelloAck.session_id`). Used by phase
    /// 06 to detect "remote restarted, persisted ids are stale." None
    /// for backends without a remote source.
    fn external_session_id(&self) -> Option<String> {
        None
    }

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

    /// Full row-major cell grid (history + visible) for substring search
    /// (Phase 1 step 8). Default impl returns an empty grid so fixture /
    /// replay backends that don't retain scrollback can opt out without
    /// implementing it. Real PTY backends override to expose scrollback.
    fn search_grid(&self, _id: TerminalSessionId) -> Vec<Vec<Cell>> {
        Vec::new()
    }

    /// Snapshot the session's grid + scrollback as ANSI bytes suitable for
    /// replaying into a fresh PTY's grid (Phase 4 step 16). `max_bytes`
    /// caps output size; backends binary-search the largest scrollback
    /// that fits. Default impl returns empty so fixture / replay backends
    /// without a live grid can opt out.
    fn serialize_buffer(&self, _id: TerminalSessionId, _max_bytes: usize) -> Vec<u8> {
        Vec::new()
    }

    /// Feed bytes directly into the session's grid without writing them to
    /// the PTY. Used at restore time to repopulate the visible grid +
    /// scrollback from a previous session's `serialize_buffer` capture
    /// BEFORE the live shell starts producing output. Default impl is a
    /// no-op for backends without a live grid.
    fn prefill_grid(&mut self, _id: TerminalSessionId, _bytes: &[u8]) -> Result<()> {
        Ok(())
    }

    /// Drain accumulated events without blocking. Returns empty when idle.
    fn drain_events(&mut self) -> Vec<TerminalEvent>;

    /// Drain events for a single session. Critical when one backend
    /// instance is shared across multiple panes (e.g., the relay
    /// backend): without this, every pane's tick races on the same
    /// global channel and steals other panes' events. Default impl
    /// filters `drain_events` by id — correct when the backend has only
    /// one session, wasteful but still correct otherwise.
    fn drain_events_for(&mut self, id: TerminalSessionId) -> Vec<TerminalEvent> {
        self.drain_events()
            .into_iter()
            .filter(|e| e.session_id() == id)
            .collect()
    }

    /// OS pid of the shell child for this session, when the backend
    /// spawned one locally. Returns `None` for remote/relay backends
    /// where the child runs on another host, or when the child has
    /// already exited. Used by the app crate's libproc-based CWD
    /// lookup so sub-pane splits can inherit the current shell's
    /// working directory.
    fn os_pid(&self, _id: TerminalSessionId) -> Option<u32> {
        None
    }

    /// Best-known current working directory for this session, populated
    /// by the OSC 7 (`file://`) tracker if the shell emits it. Returns
    /// `None` when no OSC 7 has been seen yet (most likely a fresh
    /// spawn before the first prompt) — callers fall back to a libproc
    /// query on `os_pid` in that case.
    ///
    /// Reading the cached value is lock-free amortized and ~100x faster
    /// than `proc_pidinfo`; rapid Cmd+D chains benefit the most.
    fn cwd_hint(&self, _id: TerminalSessionId) -> Option<PathBuf> {
        None
    }

    /// Tear down a session. Idempotent — safe to call on an already-closed id.
    fn close(&mut self, id: TerminalSessionId) -> Result<()>;

    /// Release this session's attachment to a relay-owned PTY WITHOUT
    /// killing the PTY, so the SAME daemon PTY can be re-attached elsewhere
    /// — e.g. a tab torn off into another window via `attach_existing`. After
    /// detach the local session is gone, so a later `close(id)` is a no-op
    /// and the daemon PTY survives.
    ///
    /// Default impl falls back to `close`: backends without a detachable
    /// remote (the in-process portable PTY, whose child dies with the
    /// process) have nothing to preserve, so a "move" there degrades to a
    /// normal teardown. Only relay-backed backends override.
    fn detach(&mut self, id: TerminalSessionId) -> Result<()> {
        self.close(id)
    }
}
