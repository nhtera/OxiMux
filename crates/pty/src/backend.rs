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

/// Input-relevant terminal modes the keystroke encoder needs. Mirrors the
/// alacritty `TermMode` bits but lives in this crate so the app layer never
/// depends on vte/alacritty. `app_cursor` (DECCKM) switches cursor/Home/End
/// from CSI (`ESC [ A`) to SS3 (`ESC O A`) form — full-screen apps (vim,
/// less) set it and expect SS3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InputMode {
    pub app_cursor: bool,
}

/// Mouse-reporting modes an app has enabled, mirrored from alacritty
/// `TermMode` into this crate so the app layer never depends on vte. Drives
/// whether the renderer forwards mouse events to the child (vim, lazygit,
/// htop) instead of using them for local selection / scrollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MouseMode {
    /// Report button press/release (1000).
    pub report_click: bool,
    /// Report motion while a button is held (1002).
    pub report_drag: bool,
    /// Report any motion (1003).
    pub report_motion: bool,
    /// Encode reports in SGR form (1006) rather than the legacy X10 form.
    pub sgr: bool,
    /// Translate wheel to arrow keys on the alt-screen (1007).
    pub alternate_scroll: bool,
    /// The alt-screen is active (1049).
    pub alt_screen: bool,
}

impl MouseMode {
    /// True when the app wants any mouse events forwarded.
    pub fn any_reporting(&self) -> bool {
        self.report_click || self.report_drag || self.report_motion
    }
}

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
    /// Off-screen rows to retain. Sourced from `TerminalSettings` at spawn so
    /// the scrollback depth is user-tunable; defaults to the prior constant.
    pub scrollback: usize,
    /// Retain a second, bounded Output/Exit stream for lifecycle observers.
    /// The renderer remains the sole consumer of `drain_events_for`.
    pub capture_status_events: bool,
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
            scrollback: 5000,
            capture_status_events: false,
        }
    }
}

/// Callback a backend invokes — from whatever thread produces output — the
/// moment new output is enqueued for a session. The UI registers one of these
/// (via [`TerminalBackend::set_output_waker`]) to drain on arrival instead of
/// polling on a timer; the closure just nudges the UI's async drain task awake.
/// Kept as a bare `Fn` so this crate stays free of any async/channel dependency.
pub type OutputWaker = std::sync::Arc<dyn Fn() + Send + Sync>;

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
    fn spawn_dormant(&mut self, _cols: u16, _rows: u16) -> Result<TerminalSessionId> {
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
    fn promote_to_live(&mut self, _id: TerminalSessionId, _cfg: SpawnConfig) -> Result<()> {
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

    /// Register an event-driven drain signal for `session`: the backend calls
    /// `waker` whenever it enqueues output, so the UI can drain on arrival
    /// rather than waking on a fixed timer (which macOS throttles/coalesces
    /// when the run loop is idle between keystrokes — the cause of echo sitting
    /// undrained for 100ms–1s). Default is a no-op: backends that don't wire it
    /// leave the caller on its timer poll. Passing this lets relay-backed
    /// sessions match the responsiveness of an event-driven renderer.
    fn set_output_waker(&mut self, _id: TerminalSessionId, _waker: OutputWaker) {}

    /// Local session ids this backend currently tracks. Used by the
    /// daemon-crash recovery path: when a dead relay backend is swapped
    /// for a fresh one in place, the replacement is seeded with these
    /// ids so it can emit one synthetic `Exit` per orphaned session —
    /// otherwise consumers polling those ids (agent status machines)
    /// would drain nothing forever and report a live status for a dead
    /// process. Default returns empty — single-owner in-process
    /// backends are dropped with their view, never swapped under it.
    fn live_session_ids(&self) -> Vec<TerminalSessionId> {
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

    /// Scroll the displayed viewport by `delta` lines (positive = back into
    /// scrollback, negative = toward the live tail). Render-side only — the
    /// grid and PTY are untouched, so each attached client scrolls
    /// independently. Default impl is a no-op for backends without a local
    /// grid (fixture / replay).
    fn scroll(&mut self, _id: TerminalSessionId, _delta: i32) -> Result<()> {
        Ok(())
    }

    /// Snap the displayed viewport back to the live tail (offset 0). Default
    /// impl is a no-op for grid-less backends.
    fn scroll_to_bottom(&mut self, _id: TerminalSessionId) -> Result<()> {
        Ok(())
    }

    /// A renderable snapshot of the session's current state. Step 1-2 ships
    /// an empty snapshot — step 3 wires `alacritty_terminal` to produce
    /// real rows and a cursor position.
    fn snapshot(&self, id: TerminalSessionId) -> Result<TerminalSnapshot>;

    /// Input-relevant terminal modes (DECCKM today) read live so the
    /// keystroke encoder can emit the right cursor-key form. Default returns
    /// the all-off mode so fixture / replay backends don't have to care.
    fn input_mode(&self, _id: TerminalSessionId) -> InputMode {
        InputMode::default()
    }

    /// Mouse-reporting modes the app has enabled. Default all-off so the
    /// renderer keeps mouse events for local selection / scrollback.
    fn mouse_mode(&self, _id: TerminalSessionId) -> MouseMode {
        MouseMode::default()
    }

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

    /// Stream bytes produced by an external source (an agent CLI, a
    /// replayer, a fixture) into a session's grid emulator without going
    /// through a PTY child. Intended for "display-only" sessions
    /// registered via `spawn_dormant` that are never promoted — the
    /// caller acts as the byte producer and the renderer picks up the
    /// new state via the next `snapshot`. Distinct from `prefill_grid`,
    /// which is a one-shot capture-format restore that clears any
    /// title/bell/color side-effect events as historical; `write_output`
    /// preserves them so the live UI surfaces them normally.
    ///
    /// Default impl returns an error. Backends that support display-only
    /// dormant sessions override; backends that don't (replay / fixture /
    /// relay-bound) opt out by leaving the default.
    fn write_output(&mut self, _id: TerminalSessionId, _bytes: &[u8]) -> Result<()> {
        Err(anyhow::anyhow!(
            "this backend does not support write_output"
        ))
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

    /// Start independent lifecycle-event delivery for `id`. Registration is
    /// idempotent. Backends may backfill Output/Exit still pending for the
    /// renderer, but must never remove those renderer events.
    fn subscribe_status_events(&mut self, _id: TerminalSessionId) -> Result<()> {
        Err(anyhow::anyhow!(
            "this backend does not support status event subscriptions"
        ))
    }

    /// Drain the independent, per-session Output/Exit stream. This must not
    /// consume or otherwise alter events waiting for the renderer.
    fn drain_status_events_for(&mut self, _id: TerminalSessionId) -> Vec<TerminalEvent> {
        Vec::new()
    }

    /// Release lifecycle-observer state. Idempotent and independent of the
    /// renderer queue so a terminal may remain visible after observation ends.
    fn unsubscribe_status_events(&mut self, _id: TerminalSessionId) {}

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
