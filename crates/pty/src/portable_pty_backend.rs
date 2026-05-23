//! `portable-pty` concrete `TerminalBackend`.
//!
//! Design:
//! - One `Session` per spawn. Owns master, writer, a clone-killer for the
//!   child, and a watcher thread join handle.
//! - The watcher thread owns the reader + the child, reads to EOF, and
//!   emits `Output` chunks via a bounded `mpsc::sync_channel(256)`. When
//!   the reader sees EOF it waits the child and emits `Exit`.
//! - `close` signals shutdown via the killer + drops master/writer, which
//!   closes file descriptors and lets the watcher exit cleanly.
//!
//! No tokio runtime requirement — `std::thread` + `std::sync::mpsc::sync_channel`
//! gives us the same bounded-channel guarantee with one less moving part.
//! When the UI layer eventually needs async coordination, the bounded
//! receiver can be wrapped or replaced without changing the trait surface.

use anyhow::{Context, Result};
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::backend::{SpawnConfig, TerminalBackend, TerminalSessionId};
use crate::close_grace::{JoinHandleWatcher, close_with_grace, term_step};
use crate::events::TerminalEvent;
use crate::snapshot::{Cell, TerminalSnapshot};
use crate::state::TerminalState;

const EVENT_CHANNEL_CAPACITY: usize = 256;
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

struct Session {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
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
}

pub struct PortablePtyBackend {
    sessions: HashMap<TerminalSessionId, Session>,
    next_id: u64,
    event_tx: SyncSender<TerminalEvent>,
    event_rx: Receiver<TerminalEvent>,
}

impl PortablePtyBackend {
    pub fn new() -> Self {
        let (event_tx, event_rx) = sync_channel(EVENT_CHANNEL_CAPACITY);
        Self {
            sessions: HashMap::new(),
            next_id: 1,
            event_tx,
            event_rx,
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
        let state = Arc::new(Mutex::new(TerminalState::new(
            cfg.cols,
            cfg.rows,
            SCROLLBACK_ROWS,
        )));
        let tx = self.event_tx.clone();
        let watcher_state = Arc::clone(&state);
        let watcher =
            std::thread::spawn(move || watch_session(id, reader, child, tx, watcher_state));

        self.sessions.insert(
            id,
            Session {
                master: pair.master,
                writer,
                killer,
                state,
                watcher: Some(watcher),
                cols: cfg.cols,
                rows: cfg.rows,
                pid,
            },
        );
        Ok(id)
    }

    fn write(&mut self, id: TerminalSessionId, bytes: &[u8]) -> Result<()> {
        let session = self
            .sessions
            .get_mut(&id)
            .with_context(|| format!("unknown session {id:?}"))?;
        session.writer.write_all(bytes).context("pty write")?;
        session.writer.flush().context("pty flush")?;
        Ok(())
    }

    fn resize(&mut self, id: TerminalSessionId, cols: u16, rows: u16) -> Result<()> {
        let session = self
            .sessions
            .get_mut(&id)
            .with_context(|| format!("unknown session {id:?}"))?;
        session
            .master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("pty resize")?;
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
            .map(|s| crate::grid_serializer::serialize_term_capped(s.term_for_test(), max_bytes))
            .unwrap_or_default()
    }

    fn prefill_grid(&mut self, id: TerminalSessionId, bytes: &[u8]) -> Result<()> {
        let session = self
            .sessions
            .get(&id)
            .with_context(|| format!("unknown session {id:?}"))?;
        if let Ok(mut state) = session.state.lock() {
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

    fn drain_events(&mut self) -> Vec<TerminalEvent> {
        let mut out = Vec::new();
        while let Ok(event) = self.event_rx.try_recv() {
            out.push(event);
        }
        out
    }

    fn close(&mut self, id: TerminalSessionId) -> Result<()> {
        let Some(mut session) = self.sessions.remove(&id) else {
            return Ok(());
        };
        // Drop writer immediately so write() callers get EBADF rather than
        // racing with the pending kill.
        drop(session.writer);
        // Build the SIGTERM step. On Unix, target the whole process group
        // (negative pid) so children spawned by the agent CLI also receive
        // the signal. The negative-pid contract works because portable-pty
        // calls setsid() in the child's pre-exec hook, making pid == pgid.
        let pid = session.pid;
        let term_fn = move || term_step(pid);
        // The SIGKILL fallback uses the existing portable-pty killer.
        let mut killer = session.killer;
        let kill_fn = move || {
            let _ = killer.kill();
        };
        // Drop master BEFORE polling the watcher: closes the pty fd, which
        // unblocks the watcher's read() and lets it observe EOF + reap the
        // child. SIGTERM gives the agent a chance to exit cleanly first;
        // master-drop ensures even a stubborn agent's read loop unblocks.
        drop(session.master);
        if let Some(handle) = session.watcher.take() {
            close_with_grace(
                JoinHandleWatcher(handle),
                term_fn,
                kill_fn,
                CANCEL_GRACE,
                KILL_POLL_INTERVAL,
            );
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
) {
    let mut buf = [0u8; READ_BUFFER_BYTES];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let bytes_slice = &buf[..n];
                if let Ok(mut s) = state.lock() {
                    s.advance(bytes_slice);
                }
                let bytes = bytes_slice.to_vec();
                if tx.send(TerminalEvent::Output { id, bytes }).is_err() {
                    return;
                }
            }
            Err(_) => break,
        }
    }
    let code = child.wait().ok().and_then(|status| {
        if status.success() {
            Some(0)
        } else {
            status.exit_code().try_into().ok()
        }
    });
    let _ = tx.send(TerminalEvent::Exit { id, code });
}
