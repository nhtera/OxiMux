use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use oximux_pty::{
    Cell, SpawnConfig, TerminalBackend, TerminalEvent, TerminalSessionId, TerminalSnapshot,
    TerminalState,
};
use oximux_relay_proto::{Notification, Request, Response};
use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;

use crate::client::RelayClient;

// Match the in-process backend's scrollback so visual continuity is
// preserved when an app switches between in-process and relay
// backends mid-session (e.g., relay went down + supervisor restarted).
const SCROLLBACK_ROWS: usize = 5000;

struct Session {
    relay_pty_id: String,
    state: Arc<Mutex<TerminalState>>,
    cols: u16,
    rows: u16,
    _pump: JoinHandle<()>,
}

pub struct RelayBackend {
    client: Arc<RelayClient>,
    handle: Handle,
    sessions: Mutex<HashMap<TerminalSessionId, Session>>,
    next_session_id: AtomicU64,
    event_tx: Sender<TerminalEvent>,
    event_rx: Receiver<TerminalEvent>,
}

impl RelayBackend {
    // `handle` MUST belong to a runtime that the *caller's* thread is
    // not a worker of — otherwise `Handle::block_on` panics. The app
    // wires this up by owning a dedicated tokio runtime for the relay
    // client and only calling sync methods from the GPUI render
    // thread (which is not a tokio worker).
    pub fn new(client: Arc<RelayClient>, handle: Handle) -> Self {
        let (event_tx, event_rx) = channel();
        Self {
            client,
            handle,
            sessions: Mutex::new(HashMap::new()),
            next_session_id: AtomicU64::new(1),
            event_tx,
            event_rx,
        }
    }

    // Borrow the underlying client. Used by phase-06 reconciliation
    // (which needs `ListPtys` before any local session exists) and
    // by integration tests that need to query daemon-side state.
    pub fn client(&self) -> &Arc<RelayClient> {
        &self.client
    }

    // Daemon-side relay PTY id behind a local `TerminalSessionId`.
    // Phase 06 calls this at capture time (on app quit / project
    // switch) to persist `(project, ordinal) → relay_pty_id`.
    pub fn relay_pty_id_of_session(&self, id: TerminalSessionId) -> Option<String> {
        self.sessions
            .lock()
            .expect("sessions poisoned")
            .get(&id)
            .map(|s| s.relay_pty_id.clone())
    }

    fn mint_id(&self) -> TerminalSessionId {
        TerminalSessionId(self.next_session_id.fetch_add(1, Ordering::Relaxed))
    }

    fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        self.handle.block_on(fut)
    }

    fn request(&self, req: Request) -> Result<Response> {
        self.block_on(self.client.request(req))
            .map_err(|e| anyhow!(e))
    }

    fn relay_pty_id_of(&self, id: TerminalSessionId) -> Result<String> {
        let sessions = self.sessions.lock().expect("sessions poisoned");
        sessions
            .get(&id)
            .map(|s| s.relay_pty_id.clone())
            .ok_or_else(|| anyhow!("unknown session {id:?}"))
    }

    // Attach implementation. Public so callers holding a concrete
    // `RelayBackend` can skip the trait-method indirection; the trait
    // method forwards to this. Replays the daemon's buffered bytes
    // into the local TerminalState BEFORE the pump starts so the
    // first frame the renderer sees is the full prior screen.
    pub fn attach_relay_pty(&self, relay_pty_id: &str) -> Result<TerminalSessionId> {
        let resp = self.request(Request::Attach {
            pty_id: relay_pty_id.to_owned(),
        })?;
        let replay = match resp {
            Response::AttachOk { replay } => replay,
            Response::Err { code, message } => bail!("attach: {code:?} — {message}"),
            other => bail!("unexpected attach response: {other:?}"),
        };
        // Default grid size; caller can resize via TerminalBackend::resize
        // once it knows the actual pane dimensions.
        let cols = 80;
        let rows = 24;
        let state = Arc::new(Mutex::new(TerminalState::new(cols, rows, SCROLLBACK_ROWS)));
        state.lock().expect("state poisoned").advance(&replay);

        let id = self.mint_id();
        let notif_rx = self.client.subscribe_pty(relay_pty_id);
        let pump = self.spawn_pump(id, Arc::clone(&state), notif_rx);
        self.sessions.lock().expect("sessions poisoned").insert(
            id,
            Session {
                relay_pty_id: relay_pty_id.to_owned(),
                state,
                cols,
                rows,
                _pump: pump,
            },
        );
        Ok(id)
    }

    fn spawn_pump(
        &self,
        id: TerminalSessionId,
        state: Arc<Mutex<TerminalState>>,
        mut notif_rx: UnboundedReceiver<Notification>,
    ) -> JoinHandle<()> {
        let event_tx = self.event_tx.clone();
        self.handle.spawn(async move {
            while let Some(n) = notif_rx.recv().await {
                match n {
                    Notification::Output { bytes, .. } => {
                        if let Ok(mut s) = state.lock() {
                            s.advance(&bytes);
                        }
                        if event_tx
                            .send(TerminalEvent::Output {
                                id,
                                bytes: bytes.clone(),
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Notification::Exit { code, .. } => {
                        let _ = event_tx.send(TerminalEvent::Exit { id, code });
                        return;
                    }
                }
            }
        })
    }
}

impl TerminalBackend for RelayBackend {
    fn attach_existing(&mut self, external_id: &str) -> Result<TerminalSessionId> {
        self.attach_relay_pty(external_id)
    }

    fn external_id_of(&self, id: TerminalSessionId) -> Option<String> {
        self.relay_pty_id_of_session(id)
    }

    fn list_external_ids(&self) -> Vec<String> {
        match self.block_on(self.client.request(Request::ListPtys)) {
            Ok(Response::PtyList(items)) => items.into_iter().map(|d| d.pty_id).collect(),
            Ok(other) => {
                tracing::warn!(?other, "list_external_ids unexpected response");
                Vec::new()
            }
            Err(e) => {
                tracing::warn!(?e, "list_external_ids failed");
                Vec::new()
            }
        }
    }

    fn external_session_id(&self) -> Option<String> {
        Some(self.client.server_session_id().to_owned())
    }

    fn spawn(&mut self, cfg: SpawnConfig) -> Result<TerminalSessionId> {
        let resp = self.request(Request::Spawn {
            cwd: cfg.cwd.to_string_lossy().into_owned(),
            cols: cfg.cols,
            rows: cfg.rows,
            shell: Some(cfg.shell),
            env: cfg.env,
        })?;
        let relay_pty_id = match resp {
            Response::SpawnOk { pty_id } => pty_id,
            Response::Err { code, message } => bail!("spawn: {code:?} — {message}"),
            other => bail!("unexpected spawn response: {other:?}"),
        };

        let state = Arc::new(Mutex::new(TerminalState::new(
            cfg.cols,
            cfg.rows,
            SCROLLBACK_ROWS,
        )));
        let id = self.mint_id();
        let notif_rx = self.client.subscribe_pty(&relay_pty_id);
        let pump = self.spawn_pump(id, Arc::clone(&state), notif_rx);
        self.sessions.lock().expect("sessions poisoned").insert(
            id,
            Session {
                relay_pty_id,
                state,
                cols: cfg.cols,
                rows: cfg.rows,
                _pump: pump,
            },
        );
        Ok(id)
    }

    fn write(&mut self, id: TerminalSessionId, bytes: &[u8]) -> Result<()> {
        let pty_id = self.relay_pty_id_of(id)?;
        let resp = self.request(Request::Write {
            pty_id,
            bytes: bytes.to_vec(),
        })?;
        match resp {
            Response::Ok => Ok(()),
            other => Err(anyhow!("write: {other:?}")),
        }
    }

    fn resize(&mut self, id: TerminalSessionId, cols: u16, rows: u16) -> Result<()> {
        let pty_id = self.relay_pty_id_of(id)?;
        let resp = self.request(Request::Resize {
            pty_id,
            cols,
            rows,
        })?;
        match resp {
            Response::Ok => {
                let mut sessions = self.sessions.lock().expect("sessions poisoned");
                if let Some(s) = sessions.get_mut(&id) {
                    s.cols = cols;
                    s.rows = rows;
                    if let Ok(mut state) = s.state.lock() {
                        state.resize(cols, rows);
                    }
                }
                let _ = self
                    .event_tx
                    .send(TerminalEvent::Resize { id, cols, rows });
                Ok(())
            }
            other => Err(anyhow!("resize: {other:?}")),
        }
    }

    fn snapshot(&self, id: TerminalSessionId) -> Result<TerminalSnapshot> {
        let sessions = self.sessions.lock().expect("sessions poisoned");
        let session = sessions
            .get(&id)
            .ok_or_else(|| anyhow!("unknown session {id:?}"))?;
        let mut snap = TerminalSnapshot::empty(session.cols, session.rows);
        if let Ok(state) = session.state.lock() {
            state.fill_snapshot(&mut snap);
        }
        Ok(snap)
    }

    fn bracketed_paste(&self, id: TerminalSessionId) -> Result<bool> {
        let sessions = self.sessions.lock().expect("sessions poisoned");
        let session = sessions
            .get(&id)
            .ok_or_else(|| anyhow!("unknown session {id:?}"))?;
        Ok(session
            .state
            .lock()
            .map(|s| s.is_bracketed_paste())
            .unwrap_or(false))
    }

    fn search_grid(&self, id: TerminalSessionId) -> Vec<Vec<Cell>> {
        let sessions = self.sessions.lock().expect("sessions poisoned");
        let Some(session) = sessions.get(&id) else {
            return Vec::new();
        };
        session
            .state
            .lock()
            .map(|s| s.fill_search_grid())
            .unwrap_or_default()
    }

    fn serialize_buffer(&self, id: TerminalSessionId, max_bytes: usize) -> Vec<u8> {
        let sessions = self.sessions.lock().expect("sessions poisoned");
        let Some(session) = sessions.get(&id) else {
            return Vec::new();
        };
        session
            .state
            .lock()
            .map(|s| oximux_pty::serialize_term_capped(s.term_for_test(), max_bytes))
            .unwrap_or_default()
    }

    fn prefill_grid(&mut self, id: TerminalSessionId, bytes: &[u8]) -> Result<()> {
        let sessions = self.sessions.lock().expect("sessions poisoned");
        let session = sessions
            .get(&id)
            .ok_or_else(|| anyhow!("unknown session {id:?}"))?;
        if let Ok(mut state) = session.state.lock() {
            state.advance(bytes);
        }
        Ok(())
    }

    fn drain_events(&mut self) -> Vec<TerminalEvent> {
        let mut out = Vec::new();
        while let Ok(event) = self.event_rx.try_recv() {
            out.push(event);
        }
        out
    }

    fn close(&mut self, id: TerminalSessionId) -> Result<()> {
        let session = match self.sessions.lock().expect("sessions poisoned").remove(&id) {
            Some(s) => s,
            None => return Ok(()),
        };
        self.client.unsubscribe_pty(&session.relay_pty_id);
        let resp = self
            .request(Request::Close {
                pty_id: session.relay_pty_id.clone(),
                grace_ms: 500,
            })
            .map_err(|e| anyhow!(e))?;
        match resp {
            Response::Ok => Ok(()),
            // PtyNotFound on close is benign — the relay already
            // reaped it (e.g., from the child exiting first).
            Response::Err {
                code: oximux_relay_proto::ErrCode::PtyNotFound,
                ..
            } => Ok(()),
            other => Err(anyhow!("close: {other:?}")),
        }
    }
}

