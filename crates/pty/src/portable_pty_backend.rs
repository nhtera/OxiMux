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

use crate::backend::{SpawnConfig, TerminalBackend, TerminalSessionId};
use crate::events::TerminalEvent;
use crate::snapshot::TerminalSnapshot;
use crate::state::TerminalState;

const EVENT_CHANNEL_CAPACITY: usize = 256;
const READ_BUFFER_BYTES: usize = 4096;
const SCROLLBACK_ROWS: usize = 5_000;

struct Session {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    state: Arc<Mutex<TerminalState>>,
    watcher: Option<JoinHandle<()>>,
    cols: u16,
    rows: u16,
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
        let _ = self.event_tx.try_send(TerminalEvent::Resize { id, cols, rows });
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
        let _ = session.killer.kill();
        // Dropping master + writer closes the fds; watcher hits EOF and exits.
        drop(session.writer);
        drop(session.master);
        if let Some(handle) = session.watcher.take() {
            let _ = handle.join();
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
