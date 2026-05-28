use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
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

// Per-session event buffer. Each TerminalView consumes only its own
// queue via `drain_events_for`, so panes can't steal each other's
// Output notifications. A shared global channel (as the old design
// used) made tick-time draining a race: whichever pane ticked first
// emptied the queue, leaving the actually-active pane with nothing to
// render.
type SessionEventQueues = Arc<Mutex<HashMap<TerminalSessionId, VecDeque<TerminalEvent>>>>;

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
    event_queues: SessionEventQueues,
}

impl RelayBackend {
    // `handle` MUST belong to a runtime that the *caller's* thread is
    // not a worker of — otherwise `Handle::block_on` panics. The app
    // wires this up by owning a dedicated tokio runtime for the relay
    // client and only calling sync methods from the GPUI render
    // thread (which is not a tokio worker).
    pub fn new(client: Arc<RelayClient>, handle: Handle) -> Self {
        Self {
            client,
            handle,
            sessions: Mutex::new(HashMap::new()),
            next_session_id: AtomicU64::new(1),
            event_queues: Arc::new(Mutex::new(HashMap::new())),
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
        let (replay, cols, rows) = match resp {
            Response::AttachOk { replay, cols, rows } => (replay, cols, rows),
            Response::Err { code, message } => bail!("attach: {code:?} — {message}"),
            other => bail!("unexpected attach response: {other:?}"),
        };
        // Build the local emulator at the daemon PTY's CURRENT dims, NOT a
        // hardcoded default. The replay bytes were produced by a process
        // drawing into a grid of this exact size — absolute-position CSI
        // sequences only land correctly when the receiving grid matches.
        // Replaying into the wrong size (then reflowing on the first
        // pane-driven resize) is what scrambled restored full-screen TUIs.
        // When the pane later resizes, `TerminalBackend::resize` resizes
        // the REAL daemon PTY, the live process repaints via SIGWINCH, and
        // those bytes arrive on the live stream — no static reflow.
        let cols = cols.max(1);
        let rows = rows.max(1);
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
        let queues = Arc::clone(&self.event_queues);
        self.handle.spawn(async move {
            while let Some(n) = notif_rx.recv().await {
                let event = match n {
                    Notification::Output { bytes, .. } => {
                        if let Ok(mut s) = state.lock() {
                            s.advance(&bytes);
                        }
                        TerminalEvent::Output { id, bytes }
                    }
                    Notification::Exit { code, .. } => {
                        push_event(&queues, id, TerminalEvent::Exit { id, code });
                        return;
                    }
                };
                push_event(&queues, id, event);
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
        // Hot path: called once per keystroke from the GPUI render/input
        // thread. `try_send_oneway` is fully synchronous — no tokio
        // Handle::block_on bridge, no future polling, no thread parking.
        // Send order is preserved by the writer task's single-consumer
        // mpsc; daemon-side write failures surface via the per-PTY
        // Output/Exit notification stream rather than a per-byte ack.
        self.client
            .try_send_oneway(Request::Write {
                pty_id,
                bytes: bytes.to_vec(),
            })
            .map_err(|e| anyhow!(e))?;
        Ok(())
    }

    fn resize(&mut self, id: TerminalSessionId, cols: u16, rows: u16) -> Result<()> {
        let pty_id = self.relay_pty_id_of(id)?;
        let resp = self.request(Request::Resize { pty_id, cols, rows })?;
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
                push_event(
                    &self.event_queues,
                    id,
                    TerminalEvent::Resize { id, cols, rows },
                );
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
            .map(|s| {
                // Dim-aware capture: prepend the OXBF header so a
                // matching prefill_grid can resize the receiver to the
                // captured dimensions before replay. Without this, an
                // 80-col capture replayed into a 200-col Term scrambles.
                oximux_pty::serialize_term_capped_with_dims(s.term_for_test(), max_bytes)
            })
            .unwrap_or_default()
    }

    fn prefill_grid(&mut self, id: TerminalSessionId, bytes: &[u8]) -> Result<()> {
        let sessions = self.sessions.lock().expect("sessions poisoned");
        let session = sessions
            .get(&id)
            .ok_or_else(|| anyhow!("unknown session {id:?}"))?;
        if let Ok(mut state) = session.state.lock() {
            // Match the portable backend: parse the dim header if
            // present, resize the dormant Term to match, then advance
            // the body. Legacy blobs (no header) replay as before.
            if let Some((cols, rows, payload)) = oximux_pty::parse_capture_header(bytes) {
                let cols = cols.clamp(1, 1024);
                let rows = rows.clamp(1, 512);
                state.resize(cols, rows);
                state.advance(payload);
            } else {
                state.advance(bytes);
            }
        }
        Ok(())
    }

    fn drain_events(&mut self) -> Vec<TerminalEvent> {
        // Drains every session's queue. Used by tests + cleanup paths;
        // the per-frame render path uses `drain_events_for` instead so
        // each pane only sees its own events.
        let mut map = self.event_queues.lock().expect("event queues poisoned");
        let mut out = Vec::new();
        for q in map.values_mut() {
            out.extend(q.drain(..));
        }
        out
    }

    fn drain_events_for(&mut self, id: TerminalSessionId) -> Vec<TerminalEvent> {
        let mut map = self.event_queues.lock().expect("event queues poisoned");
        match map.get_mut(&id) {
            Some(q) => q.drain(..).collect(),
            None => Vec::new(),
        }
    }

    fn close(&mut self, id: TerminalSessionId) -> Result<()> {
        let session = match self.sessions.lock().expect("sessions poisoned").remove(&id) {
            Some(s) => s,
            None => return Ok(()),
        };
        self.event_queues
            .lock()
            .expect("event queues poisoned")
            .remove(&id);
        self.client.unsubscribe_pty(&session.relay_pty_id);
        // Defer the synchronous relay Close round-trip to a detached
        // tokio task so the outer `Arc<Mutex<Box<dyn TerminalBackend>>>`
        // lock (held by `TerminalView::drop`'s spawned thread while
        // calling `close`) releases immediately. The next GPUI render
        // frame's `maybe_resize` re-acquires that mutex; without this
        // defer it blocks the main thread for the full `grace_ms`
        // window plus network RTT — visible as a "slow tab close".
        // Local state (sessions map, event_queues, unsubscribe) is
        // already cleaned up above; the remote request is the only
        // slow part left. Spawning on the tokio handle (not a raw OS
        // thread) keeps the request inside the existing runtime so
        // `block_on` semantics aren't needed.
        let client = self.client.clone();
        let pty_id = session.relay_pty_id.clone();
        self.handle.spawn(async move {
            match client.request(Request::Close {
                pty_id: pty_id.clone(),
                grace_ms: 500,
            }).await {
                Ok(Response::Ok) => {}
                // PtyNotFound on close is benign — the relay already
                // reaped it (e.g., from the child exiting first).
                Ok(Response::Err {
                    code: oximux_relay_proto::ErrCode::PtyNotFound,
                    ..
                }) => {}
                Ok(other) => {
                    tracing::warn!(?pty_id, ?other, "relay close: unexpected response");
                }
                Err(err) => {
                    tracing::warn!(?pty_id, ?err, "relay close: request failed");
                }
            }
        });
        Ok(())
    }
}

fn push_event(queues: &SessionEventQueues, id: TerminalSessionId, event: TerminalEvent) {
    if let Ok(mut map) = queues.lock() {
        map.entry(id).or_default().push_back(event);
    }
}
