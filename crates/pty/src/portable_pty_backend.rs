//! `portable-pty` concrete `TerminalBackend`.
//!
//! Design:
//! - One `Session` per spawn. Owns master, writer, a clone-killer for the
//!   child, and a watcher thread join handle.
//! - The watcher thread emits `Output` chunks via a bounded
//!   `mpsc::sync_channel(256)`, fed by a reader thread and a reaper thread it
//!   spawns. A session ends on whichever lands first: the reader seeing EOF, or
//!   the child being reaped plus a short drain. Both routes emit exactly one
//!   `Exit`, after every `Output` that preceded it.
//! - Two routes rather than one because EOF is not universal: a ConPTY output
//!   pipe outlives its child, so on Windows a self-terminating shell produces no
//!   EOF at all. See `POST_EXIT_DRAIN`.
//! - `close` signals shutdown via the killer + drops master/writer, which
//!   closes file descriptors and lets the watcher exit cleanly. That drop is
//!   also what finally unblocks a reader thread parked on a ConPTY that its
//!   child has already left.
//!
//! No tokio runtime requirement — `std::thread` + `std::sync::mpsc::sync_channel`
//! gives us the same bounded-channel guarantee with one less moving part.
//! When the UI layer eventually needs async coordination, the bounded
//! receiver can be wrapped or replaced without changing the trait surface.

use crate::conpty_c0::ConptyC0Filter;
use anyhow::{Context, Result};
use oximux_shell_env::{clear_inherited_colour_suppression, seed_utf8_locale};
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::backend::{SpawnConfig, TerminalBackend, TerminalSessionId};
use crate::close_grace::{JoinHandleWatcher, close_with_grace, term_step};
use crate::events::TerminalEvent;
use crate::snapshot::{Cell, TerminalSnapshot};
use crate::state::TerminalState;

const EVENT_CHANNEL_CAPACITY: usize = 256;
const STATUS_EVENT_CAPACITY: usize = 256;
const READ_BUFFER_BYTES: usize = 4096;
const SCROLLBACK_ROWS: usize = 5_000;

/// Maximum time `close()` waits for the watcher thread to finish after
/// sending SIGTERM before falling back to SIGKILL. The trait contract
/// (`runtime.rs::AgentRuntime::cancel` doc) promises 5 s; agents that
/// flush logs and exit cleanly will land well inside this window.
const CANCEL_GRACE: Duration = Duration::from_secs(5);

/// Polling interval for `watcher.is_finished()` during the grace window.
/// 50 ms × 100 iterations = 5 s ceiling; sleep imprecision on loaded
/// hosts is acceptable since the grace is best-effort, not a hard SLA.
const KILL_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long the watcher keeps draining output after the child has been reaped,
/// before it declares the session finished and emits `Exit`.
///
/// This is what makes exit detection work on Windows, where waiting for the
/// reader to see EOF is not an option: a ConPTY's output pipe stays open for as
/// long as the pseudoconsole does, and the pseudoconsole belongs to the master,
/// which `close()` holds until the user closes the tab. So on Windows the reader
/// never reports EOF for a child that exited on its own, and `Exit` keyed off
/// EOF alone would simply never be published — terminals would sit there looking
/// alive, and agent sessions would never reach `Done`.
///
/// The window exists because reaping the child and reading the last of its
/// output are separate races: `child.wait()` can return while bytes it already
/// wrote are still in flight. 200 ms is far longer than that hand-off needs and
/// far shorter than a user notices, and it only ever delays a session that has
/// already ended.
///
/// Deliberately a deadline from the moment of reaping, not an idle timeout that
/// each new chunk pushes back. A detached grandchild can inherit the pty and keep
/// writing after the child this session owns has gone — `start /b` on Windows,
/// `nohup` off it — and an idle timeout would then never elapse, which lands back
/// on the exact bug this constant exists to fix. A fixed window means `Exit` is
/// published within a bounded time of the child dying, whatever else holds the
/// pty open.
const POST_EXIT_DRAIN: Duration = Duration::from_millis(200);

/// How often the watcher loop wakes to re-check the child while no output is
/// arriving. Also the granularity of [`POST_EXIT_DRAIN`].
const DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(20);

struct Session {
    /// PTY-bound handles. `None` for sessions in the dormant state
    /// (F3.4: restored sub-panes that hold a prefilled grid but haven't
    /// spawned a shell child yet). `promote_to_live` fills them in.
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    killer: Option<Box<dyn ChildKiller + Send + Sync>>,
    state: Arc<Mutex<TerminalState>>,
    watcher: Option<JoinHandle<()>>,
    cols: u16,
    rows: u16,
    /// PID captured before the child is moved into the watcher thread.
    /// Used as the signal target for the grace SIGTERM dance; portable-pty
    /// calls `setsid()` in the child's pre-exec hook so the PID equals
    /// the process-group id, and `kill(-pid, SIGTERM)` reaches every
    /// descendant the agent CLI spawned. `None` is a safe fallback —
    /// `close()` skips SIGTERM and goes straight to SIGKILL.
    pid: Option<u32>,
    /// F4.7: shell-emitted CWD via OSC 7. The watcher thread updates this
    /// every time the shell prints `ESC ] 7 ; file://host/path ST`; the
    /// UI reads it for cheap inherit-cwd on Cmd+D / Cmd+Shift+D. Falls
    /// back to libproc on a miss (first split before any prompt fires).
    cwd_hint: Arc<Mutex<Option<PathBuf>>>,
}

impl Session {
    /// True when this session has no live PTY child — only the grid
    /// emulator is populated (typically by `prefill_grid` during a
    /// restore from snapshot). Operations that touch the PTY return
    /// `Err` until `promote_to_live` runs.
    fn is_dormant(&self) -> bool {
        self.master.is_none()
    }
}

pub struct PortablePtyBackend {
    sessions: HashMap<TerminalSessionId, Session>,
    next_id: u64,
    event_tx: SyncSender<TerminalEvent>,
    event_rx: Receiver<TerminalEvent>,
    status_events: Arc<Mutex<HashMap<TerminalSessionId, VecDeque<TerminalEvent>>>>,
}

impl PortablePtyBackend {
    pub fn new() -> Self {
        let (event_tx, event_rx) = sync_channel(EVENT_CHANNEL_CAPACITY);
        Self {
            sessions: HashMap::new(),
            next_id: 1,
            event_tx,
            event_rx,
            status_events: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn mint_id(&mut self) -> TerminalSessionId {
        let id = TerminalSessionId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        id
    }
}

impl Default for PortablePtyBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalBackend for PortablePtyBackend {
    fn spawn(&mut self, cfg: SpawnConfig) -> Result<TerminalSessionId> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: cfg.rows,
                cols: cfg.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty failed")?;

        let mut command = CommandBuilder::new(&cfg.shell);
        command.args(&cfg.args);
        command.cwd(&cfg.cwd);
        // Terminal identity defaults so curses/TUI apps (clear, less, vim,
        // Claude Code) find a terminfo entry and get truecolor. Set before
        // the caller loop so an explicit caller-provided TERM still wins.
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        // Host-terminal identity so tools/agents can detect the emulator.
        command.env("TERM_PROGRAM", "oximux");
        seed_utf8_locale(&mut command);
        // After the identity defaults above and before the caller loop below:
        // an inherited `NO_COLOR` would otherwise contradict the `COLORTERM`
        // just set, and a caller that genuinely wants no colour can still say
        // so in `cfg.env`.
        clear_inherited_colour_suppression(&mut command);
        for (k, v) in &cfg.env {
            command.env(k, v);
        }
        let child = pair.slave.spawn_command(command).context("spawn shell")?;
        let killer = child.clone_killer();
        // Capture PID before the watcher thread consumes `child`. Used by
        // the grace SIGTERM path in `close()`; see the `pid` field doc.
        let pid = child.process_id();
        drop(pair.slave);

        let reader = pair.master.try_clone_reader().context("clone reader")?;
        let writer = pair.master.take_writer().context("take writer")?;

        let id = self.mint_id();
        if cfg.capture_status_events {
            self.status_events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .entry(id)
                .or_default();
        }
        let state = Arc::new(Mutex::new(TerminalState::new(
            cfg.cols,
            cfg.rows,
            cfg.scrollback,
        )));
        let cwd_hint: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
        let tx = self.event_tx.clone();
        let watcher_state = Arc::clone(&state);
        let watcher_cwd = Arc::clone(&cwd_hint);
        let status_events = Arc::clone(&self.status_events);
        let watcher = std::thread::spawn(move || {
            watch_session(
                id,
                reader,
                child,
                tx,
                watcher_state,
                watcher_cwd,
                status_events,
            )
        });

        self.sessions.insert(
            id,
            Session {
                master: Some(pair.master),
                writer: Some(writer),
                killer: Some(killer),
                state,
                watcher: Some(watcher),
                cols: cfg.cols,
                rows: cfg.rows,
                pid,
                cwd_hint,
            },
        );
        Ok(id)
    }

    fn spawn_dormant(&mut self, cols: u16, rows: u16) -> Result<TerminalSessionId> {
        // F3.4: register a session WITHOUT spawning a PTY child. The
        // grid emulator is wired up so `prefill_grid` + `snapshot` can
        // populate + render restored scrollback. `write` / `resize` /
        // `external_id_of` / `os_pid` all return `Err` until
        // `promote_to_live` swaps in a real PTY.
        let id = self.mint_id();
        let state = Arc::new(Mutex::new(TerminalState::new(cols, rows, SCROLLBACK_ROWS)));
        self.sessions.insert(
            id,
            Session {
                master: None,
                writer: None,
                killer: None,
                state,
                watcher: None,
                cols,
                rows,
                pid: None,
                cwd_hint: Arc::new(Mutex::new(None)),
            },
        );
        Ok(id)
    }

    fn promote_to_live(&mut self, id: TerminalSessionId, cfg: SpawnConfig) -> Result<()> {
        // F3.4: turn a dormant session into a live one by spawning a
        // PTY child + watcher. The session keeps its existing grid
        // emulator (with any prefilled scrollback intact), so the user
        // sees a continuous transition from "restored" view to a fresh
        // shell prompt.
        let session = self
            .sessions
            .get_mut(&id)
            .with_context(|| format!("unknown session {id:?}"))?;
        if !session.is_dormant() {
            return Err(anyhow::anyhow!("session {id:?} is already live"));
        }
        // Resize the grid emulator to match the requested geometry
        // BEFORE openpty so the child sees the right WINSIZE.
        let cols = cfg.cols;
        let rows = cfg.rows;
        if let Ok(mut state) = session.state.lock() {
            state.resize(cols, rows);
        }
        session.cols = cols;
        session.rows = rows;

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty failed")?;

        let mut command = CommandBuilder::new(&cfg.shell);
        command.args(&cfg.args);
        command.cwd(&cfg.cwd);
        // Terminal identity defaults so curses/TUI apps (clear, less, vim,
        // Claude Code) find a terminfo entry and get truecolor. Set before
        // the caller loop so an explicit caller-provided TERM still wins.
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        // Host-terminal identity so tools/agents can detect the emulator.
        command.env("TERM_PROGRAM", "oximux");
        seed_utf8_locale(&mut command);
        // After the identity defaults above and before the caller loop below:
        // an inherited `NO_COLOR` would otherwise contradict the `COLORTERM`
        // just set, and a caller that genuinely wants no colour can still say
        // so in `cfg.env`.
        clear_inherited_colour_suppression(&mut command);
        for (k, v) in &cfg.env {
            command.env(k, v);
        }
        let child = pair.slave.spawn_command(command).context("spawn shell")?;
        let killer = child.clone_killer();
        let pid = child.process_id();
        drop(pair.slave);

        let reader = pair.master.try_clone_reader().context("clone reader")?;
        let writer = pair.master.take_writer().context("take writer")?;
        let tx = self.event_tx.clone();
        if cfg.capture_status_events {
            self.status_events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .entry(id)
                .or_default();
        }
        let watcher_state = Arc::clone(&session.state);
        let watcher_cwd = Arc::clone(&session.cwd_hint);
        let status_events = Arc::clone(&self.status_events);
        let watcher = std::thread::spawn(move || {
            watch_session(
                id,
                reader,
                child,
                tx,
                watcher_state,
                watcher_cwd,
                status_events,
            )
        });

        session.master = Some(pair.master);
        session.writer = Some(writer);
        session.killer = Some(killer);
        session.watcher = Some(watcher);
        session.pid = pid;
        Ok(())
    }

    fn write(&mut self, id: TerminalSessionId, bytes: &[u8]) -> Result<()> {
        let session = self
            .sessions
            .get_mut(&id)
            .with_context(|| format!("unknown session {id:?}"))?;
        let writer = session
            .writer
            .as_mut()
            .with_context(|| format!("session {id:?} is dormant; promote_to_live first"))?;
        writer.write_all(bytes).context("pty write")?;
        writer.flush().context("pty flush")?;
        Ok(())
    }

    fn resize(&mut self, id: TerminalSessionId, cols: u16, rows: u16) -> Result<()> {
        let session = self
            .sessions
            .get_mut(&id)
            .with_context(|| format!("unknown session {id:?}"))?;
        // Dormant sessions: bookkeep cols/rows + grid resize only. No
        // PTY child to push the geometry to.
        if let Some(master) = session.master.as_ref() {
            master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .context("pty resize")?;
        }
        session.cols = cols;
        session.rows = rows;
        if let Ok(mut state) = session.state.lock() {
            state.resize(cols, rows);
        }
        let _ = self
            .event_tx
            .try_send(TerminalEvent::Resize { id, cols, rows });
        Ok(())
    }

    fn scroll(&mut self, id: TerminalSessionId, delta: i32) -> Result<()> {
        let session = self
            .sessions
            .get(&id)
            .with_context(|| format!("unknown session {id:?}"))?;
        if let Ok(mut state) = session.state.lock() {
            state.scroll_lines(delta);
        }
        Ok(())
    }

    fn scroll_to_bottom(&mut self, id: TerminalSessionId) -> Result<()> {
        let session = self
            .sessions
            .get(&id)
            .with_context(|| format!("unknown session {id:?}"))?;
        if let Ok(mut state) = session.state.lock() {
            state.scroll_to_bottom();
        }
        Ok(())
    }

    fn clear(&mut self, id: TerminalSessionId) -> Result<()> {
        let session = self
            .sessions
            .get(&id)
            .with_context(|| format!("unknown session {id:?}"))?;
        if let Ok(mut state) = session.state.lock() {
            state.clear();
        }
        Ok(())
    }

    fn snapshot(&self, id: TerminalSessionId) -> Result<TerminalSnapshot> {
        let session = self
            .sessions
            .get(&id)
            .with_context(|| format!("unknown session {id:?}"))?;
        let mut snap = TerminalSnapshot::empty(session.cols, session.rows);
        if let Ok(state) = session.state.lock() {
            state.fill_snapshot(&mut snap);
        }
        Ok(snap)
    }

    fn input_mode(&self, id: TerminalSessionId) -> crate::InputMode {
        self.sessions
            .get(&id)
            .and_then(|s| s.state.lock().ok().map(|st| st.input_mode()))
            .unwrap_or_default()
    }

    fn mouse_mode(&self, id: TerminalSessionId) -> crate::MouseMode {
        self.sessions
            .get(&id)
            .and_then(|s| s.state.lock().ok().map(|st| st.mouse_mode()))
            .unwrap_or_default()
    }

    fn bracketed_paste(&self, id: TerminalSessionId) -> Result<bool> {
        let session = self
            .sessions
            .get(&id)
            .with_context(|| format!("unknown session {id:?}"))?;
        let on = session
            .state
            .lock()
            .map(|s| s.is_bracketed_paste())
            .unwrap_or(false);
        Ok(on)
    }

    fn serialize_buffer(&self, id: TerminalSessionId, max_bytes: usize) -> Vec<u8> {
        let Some(session) = self.sessions.get(&id) else {
            return Vec::new();
        };
        session
            .state
            .lock()
            .map(|s| {
                // Use the with-dims variant so the consumer can resize
                // a fresh Term to match the captured grid BEFORE replay.
                // Without this, an 80-col capture replayed into a 200-col
                // Term (or vice versa) emits scrambled output because
                // absolute-position CSI sequences land in the wrong cells.
                crate::grid_serializer::serialize_term_capped_with_dims(
                    s.term_for_test(),
                    max_bytes,
                )
            })
            .unwrap_or_default()
    }

    fn prefill_grid(&mut self, id: TerminalSessionId, bytes: &[u8]) -> Result<()> {
        let session = self
            .sessions
            .get(&id)
            .with_context(|| format!("unknown session {id:?}"))?;
        if let Ok(mut state) = session.state.lock() {
            // New-format captures carry their grid dims as a 9-byte
            // header; resize the dormant Term to match BEFORE feeding
            // the body so absolute-position CSI sequences land in the
            // right cells. Legacy (pre-header) blobs replay against the
            // current grid as before.
            if let Some((cols, rows, payload)) = crate::grid_serializer::parse_capture_header(bytes)
            {
                // Defensive clamp: caps match the rest of the codebase
                // (DEFAULT_COLS/ROWS pattern in app/src/shell/terminal_view.rs
                // is 100×32; we accept up to 1024×512 for ultrawide users)
                // and prevent a corrupted header from triggering a huge
                // allocation inside alacritty's grid.
                let cols = cols.clamp(1, 1024);
                let rows = rows.clamp(1, 512);
                state.resize(cols, rows);
                state.advance(payload);
            } else {
                state.advance(bytes);
            }
            // Replay is historical: drop any title/clipboard/bell/color events
            // it fired so they don't leak into the live session's first frame.
            state.clear_collected();
        }
        Ok(())
    }

    fn write_output(&mut self, id: TerminalSessionId, bytes: &[u8]) -> Result<()> {
        let session = self
            .sessions
            .get(&id)
            .with_context(|| format!("unknown session {id:?}"))?;
        // Display-only invariant: the producer of these bytes must be
        // the sole writer to the grid. A live PTY session has a watcher
        // thread feeding the same parser concurrently — accepting
        // write_output there would race the watcher and scramble cells.
        if !session.is_dormant() {
            return Err(anyhow::anyhow!(
                "session {id:?} has a live PTY child; use write() instead"
            ));
        }
        if let Ok(mut state) = session.state.lock() {
            // Unlike prefill_grid (which clears collected title/bell/color
            // events as historical), write_output preserves them so the
            // live renderer surfaces them like a normal session would.
            state.advance(bytes);
        }
        Ok(())
    }

    fn search_grid(&self, id: TerminalSessionId) -> Vec<Vec<Cell>> {
        let Some(session) = self.sessions.get(&id) else {
            return Vec::new();
        };
        session
            .state
            .lock()
            .map(|s| s.fill_search_grid())
            .unwrap_or_default()
    }

    fn os_pid(&self, id: TerminalSessionId) -> Option<u32> {
        self.sessions.get(&id).and_then(|s| s.pid)
    }

    fn cwd_hint(&self, id: TerminalSessionId) -> Option<PathBuf> {
        // F4.7: cheap shell-emitted cwd lookup. None until the shell has
        // emitted at least one OSC 7 since spawn; the caller falls back
        // to libproc on miss. Poisoned mutex → None (the watcher panicked;
        // can't trust the cache).
        let session = self.sessions.get(&id)?;
        session.cwd_hint.lock().ok()?.clone()
    }

    fn drain_events(&mut self) -> Vec<TerminalEvent> {
        let mut out = Vec::new();
        while let Ok(event) = self.event_rx.try_recv() {
            out.push(event);
        }
        out
    }

    fn subscribe_status_events(&mut self, id: TerminalSessionId) -> Result<()> {
        if !self.sessions.contains_key(&id) {
            return Err(anyhow::anyhow!("unknown session {id:?}"));
        }
        self.status_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(id)
            .or_default();
        Ok(())
    }

    fn drain_status_events_for(&mut self, id: TerminalSessionId) -> Vec<TerminalEvent> {
        self.status_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_mut(&id)
            .map(|queue| queue.drain(..).collect())
            .unwrap_or_default()
    }

    fn unsubscribe_status_events(&mut self, id: TerminalSessionId) {
        self.status_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
    }

    fn close(&mut self, id: TerminalSessionId) -> Result<()> {
        let Some(mut session) = self.sessions.remove(&id) else {
            return Ok(());
        };
        // Dormant sessions have no PTY child / watcher / killer. Drop
        // them directly; the grid emulator state goes with the
        // `session` move and there's nothing to signal.
        if session.is_dormant() {
            return Ok(());
        }
        // Drop writer immediately so write() callers get EBADF rather than
        // racing with the pending kill.
        drop(session.writer.take());
        // Build the SIGTERM step. On Unix, target the whole process group
        // (negative pid) so children spawned by the agent CLI also receive
        // the signal. The negative-pid contract works because portable-pty
        // calls setsid() in the child's pre-exec hook, making pid == pgid.
        let pid = session.pid;
        let term_fn = move || term_step(pid);
        // The SIGKILL fallback uses the existing portable-pty killer.
        let mut killer = session
            .killer
            .take()
            .expect("live session must hold a killer");
        let kill_fn = move || {
            let _ = killer.kill();
        };
        // Drop master BEFORE the grace dance: closes the pty fd, which
        // unblocks the watcher's read() and lets it observe EOF + reap the
        // child. SIGTERM gives the agent a chance to exit cleanly first;
        // master-drop ensures even a stubborn agent's read loop unblocks.
        drop(session.master.take());
        // Defer the watcher join + SIGTERM/SIGKILL grace loop to a
        // detached thread. The caller (`TerminalView::drop`) holds the
        // shared `Arc<Mutex<Box<dyn TerminalBackend>>>` lock while
        // invoking `close`; without this defer, the lock stays held
        // through `close_with_grace`'s polling loop (up to CANCEL_GRACE,
        // currently 5s). The very next GPUI render frame after tab
        // close calls `maybe_resize` → `with_backend` → re-acquires that
        // mutex, blocking the main thread until grace completes —
        // visible to the user as a "slow tab close". Spawning here lets
        // the outer lock release as soon as we return; the watcher
        // handle + closures are all Send so the move is safe.
        if let Some(handle) = session.watcher.take() {
            std::thread::spawn(move || {
                close_with_grace(
                    JoinHandleWatcher(handle),
                    term_fn,
                    kill_fn,
                    CANCEL_GRACE,
                    KILL_POLL_INTERVAL,
                );
            });
        }
        Ok(())
    }
}
fn watch_session(
    id: TerminalSessionId,
    mut reader: Box<dyn Read + Send>,
    mut child: Box<dyn Child + Send + Sync>,
    tx: SyncSender<TerminalEvent>,
    state: Arc<Mutex<TerminalState>>,
    cwd_hint: Arc<Mutex<Option<PathBuf>>>,
    status_events: Arc<Mutex<HashMap<TerminalSessionId, VecDeque<TerminalEvent>>>>,
) {
    // Reap the child independently of the reader. The reader's renderer
    // notifications are nonblocking, so an absent renderer cannot stop the PTY
    // drain. Status Exit is published below only after all preceding Output,
    // preserving lifecycle ordering.
    let (exit_tx, exit_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let code = child.wait().ok().and_then(|status| {
            if status.success() {
                Some(0)
            } else {
                status.exit_code().try_into().ok()
            }
        });
        let _ = exit_tx.send(code);
    });

    // The blocking `read` gets a thread of its own so the loop below can watch
    // the child and the output at the same time. Keeping the read inline would
    // mean the only way out of this function is EOF, which is exactly the
    // assumption that does not hold on Windows (see `POST_EXIT_DRAIN`).
    //
    // Dropping `bytes_tx` on the way out is how the reader reports EOF, so the
    // unix path still ends the session the moment the pty closes, unchanged.
    let (bytes_tx, bytes_rx) = sync_channel::<Vec<u8>>(EVENT_CHANNEL_CAPACITY);
    std::thread::spawn(move || {
        let mut buf = [0u8; READ_BUFFER_BYTES];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    // Blocking send, unlike the renderer hand-off below: PTY
                    // bytes drive the grid, so dropping them on a full channel
                    // would corrupt terminal state rather than just skip a
                    // frame. Backpressure onto the pty is the correct response.
                    if bytes_tx.send(buf[..n].to_vec()).is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });

    let mut code = None;
    // `Some(_)` once the child has been reaped: the point after which output can
    // only be the tail of what it already wrote.
    let mut drain_deadline: Option<Instant> = None;
    let mut saw_eof = false;
    // Lives for the session, not the chunk: a sequence can be split by
    // wherever the pipe happened to break, so the phase has to carry over.
    let mut c0 = ConptyC0Filter::new();

    loop {
        match bytes_rx.recv_timeout(DRAIN_POLL_INTERVAL) {
            Ok(chunk) => {
                let chunk = c0.apply(&chunk);
                if !forward_chunk(id, &chunk, &tx, &state, &cwd_hint, &status_events) {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                saw_eof = true;
                break;
            }
        }

        if drain_deadline.is_none()
            && let Ok(reaped) = exit_rx.try_recv()
        {
            code = reaped;
            drain_deadline = Some(Instant::now() + POST_EXIT_DRAIN);
        }
        if let Some(deadline) = drain_deadline
            && Instant::now() >= deadline
        {
            break;
        }
    }

    // Whatever the reader already handed over outranks `Exit`, so take it before
    // publishing. Bounded by the channel, so this cannot spin.
    while let Ok(chunk) = bytes_rx.try_recv() {
        let chunk = c0.apply(&chunk);
        if !forward_chunk(id, &chunk, &tx, &state, &cwd_hint, &status_events) {
            return;
        }
    }

    // On EOF the exit code is worth blocking for: the pty is gone, so `wait`
    // is about to return. But only if the reaper has not been consulted yet —
    // when the child was reaped first (`drain_deadline` set) and EOF arrived
    // inside the drain window, the channel is already empty with its sender
    // gone, and a second recv would clobber the real code with `None`.
    if saw_eof && drain_deadline.is_none() {
        code = exit_rx.recv().ok().flatten();
    }

    let exit = TerminalEvent::Exit { id, code };
    push_status_event(&status_events, id, &exit);
    let _ = tx.send(exit);
}

/// Drive one chunk of PTY bytes into the grid and out to the renderer.
///
/// Returns `false` when the renderer is gone, which is the caller's signal to
/// abandon the session without publishing `Exit`.
fn forward_chunk(
    id: TerminalSessionId,
    chunk: &[u8],
    tx: &SyncSender<TerminalEvent>,
    state: &Arc<Mutex<TerminalState>>,
    cwd_hint: &Arc<Mutex<Option<PathBuf>>>,
    status_events: &Arc<Mutex<HashMap<TerminalSessionId, VecDeque<TerminalEvent>>>>,
) -> bool {
    // Drive the grid + collect derived events (bell, cwd, command marks,
    // progress, title, clipboard, device replies) in one locked pass. The OSC
    // scanner now lives in `TerminalState`.
    let derived = match state.lock() {
        Ok(mut s) => s.advance_collecting(id, chunk),
        Err(_) => Vec::new(),
    };
    // Forward derived events BEFORE the Output chunk so attention (bell) lands
    // ahead of the bytes that raised it. OSC 7 cwd is absorbed locally into the
    // cheap cwd cache rather than the event stream — `cwd_hint()` is the lookup
    // surface.
    for ev in derived {
        match ev {
            TerminalEvent::CwdChanged { path, .. } => {
                if let Ok(mut slot) = cwd_hint.lock() {
                    *slot = Some(path);
                }
            }
            other => {
                if !try_send_renderer_event(tx, other) {
                    return false;
                }
            }
        }
    }
    let output = TerminalEvent::Output {
        id,
        bytes: chunk.to_vec(),
    };
    push_status_event(status_events, id, &output);
    try_send_renderer_event(tx, output)
}

fn try_send_renderer_event(tx: &SyncSender<TerminalEvent>, event: TerminalEvent) -> bool {
    match tx.try_send(event) {
        Ok(()) | Err(TrySendError::Full(_)) => true,
        Err(TrySendError::Disconnected(_)) => false,
    }
}

fn push_status_event(
    queues: &Arc<Mutex<HashMap<TerminalSessionId, VecDeque<TerminalEvent>>>>,
    id: TerminalSessionId,
    event: &TerminalEvent,
) {
    let mut queues = queues
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(queue) = queues.get_mut(&id) else {
        return;
    };
    if queue.len() >= STATUS_EVENT_CAPACITY {
        if let Some(index) = queue
            .iter()
            .position(|queued| matches!(queued, TerminalEvent::Output { .. }))
        {
            queue.remove(index);
        } else if matches!(event, TerminalEvent::Output { .. }) {
            return;
        }
    }
    queue.push_back(event.clone());
}
