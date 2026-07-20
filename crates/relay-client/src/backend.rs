use std::collections::{HashMap, VecDeque};
use std::ops::ControlFlow;
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
const STATUS_EVENT_CAPACITY: usize = 256;

/// Debug-only output-arrival probe, paired with the app-side input/echo trace.
/// Off unless `OXIMUX_INPUT_TRACE` is set; appends to the same log file so the
/// `send_bytes → output_arrived` gap (the remote program's own response time)
/// can be separated from `output_arrived → echo_render` (our drain/render).
fn arrival_trace(bytes: usize) {
    use std::io::Write as _;
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    if !*ENABLED.get_or_init(|| std::env::var_os("OXIMUX_INPUT_TRACE").is_some()) {
        return;
    }
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/oximux_input_trace.log")
    {
        let _ = writeln!(f, "{micros} output_arrived n={bytes}");
    }
}

// Per-session event buffer. Each TerminalView consumes only its own
// queue via `drain_events_for`, so panes can't steal each other's
// Output notifications. A shared global channel (as the old design
// used) made tick-time draining a race: whichever pane ticked first
// emptied the queue, leaving the actually-active pane with nothing to
// render.
#[derive(Default)]
struct EventQueues {
    renderer: HashMap<TerminalSessionId, VecDeque<TerminalEvent>>,
    status: HashMap<TerminalSessionId, VecDeque<TerminalEvent>>,
}

type SessionEventQueues = Arc<Mutex<EventQueues>>;

struct Session {
    relay_pty_id: String,
    // Daemon-minted handle for THIS attachment. Sent back on every
    // `Resize`/`Detach` so the daemon updates the right attachment's
    // requested size in its "smallest screen wins" `min` computation.
    attachment_id: u64,
    // This session's handle in the client's per-PTY subscriber fan-out.
    // Unique per attachment so teardown removes only OUR output stream and
    // never a sibling session that shares the same daemon PTY.
    sub_id: u64,
    state: Arc<Mutex<TerminalState>>,
    cols: u16,
    rows: u16,
    // Reconnect guard: a monotonic attach-generation. The pump captures
    // the value at spawn time and stops touching `state`/emitting events once
    // it no longer matches — so a superseded pump (a future reconnect /
    // multi-window re-attach, or this session's own teardown) can't
    // double-drive the shared `TerminalState`. Bumped in `close` so the
    // pump stops draining into an orphaned state during teardown.
    generation: Arc<AtomicU64>,
    _pump: JoinHandle<()>,
}

pub struct RelayBackend {
    client: Arc<RelayClient>,
    handle: Handle,
    sessions: Mutex<HashMap<TerminalSessionId, Session>>,
    next_session_id: AtomicU64,
    event_queues: SessionEventQueues,
    /// Per-session event-driven drain signals. The pump invokes the matching
    /// waker right after enqueuing output so the UI drains on arrival instead
    /// of polling a (throttled) timer. Shared with each pump task by Arc clone.
    output_wakers: Arc<Mutex<HashMap<TerminalSessionId, oximux_pty::OutputWaker>>>,
    /// Session ids inherited from a predecessor backend that died with
    /// the old daemon (crash-recovery swap). Each id yields exactly one
    /// synthetic `Exit { code: None }` from `drain_events[_for]`, so
    /// pollers of the orphaned sessions (agent status machines) learn
    /// the process is gone instead of draining nothing forever.
    inherited_dead_sessions: Mutex<std::collections::HashSet<TerminalSessionId>>,
    status_inherited_dead_sessions: Mutex<std::collections::HashSet<TerminalSessionId>>,
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
            event_queues: Arc::new(Mutex::new(EventQueues::default())),
            output_wakers: Arc::new(Mutex::new(HashMap::new())),
            inherited_dead_sessions: Mutex::new(std::collections::HashSet::new()),
            status_inherited_dead_sessions: Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// Crash-recovery seeding: record the dead predecessor backend's
    /// session ids so each yields one synthetic `Exit` on its next
    /// drain. Also start `next_session_id` past the inherited ids —
    /// the swapped-in backend must never mint an id that a live
    /// `TerminalView` still holds from the old backend, or the two
    /// would alias one event queue.
    pub fn seed_synthetic_exits(&self, ids: Vec<TerminalSessionId>) {
        if ids.is_empty() {
            return;
        }
        let max_inherited = ids.iter().map(|id| id.0).max().unwrap_or(0);
        // `fetch_max` keeps the floor monotonic even if seeding ever
        // raced a concurrent mint (it can't today — seeding happens
        // before the swap publishes the backend).
        self.next_session_id
            .fetch_max(max_inherited + 1, Ordering::Relaxed);
        lock_recover(&self.inherited_dead_sessions, "inherited sessions")
            .extend(ids.iter().copied());
        lock_recover(
            &self.status_inherited_dead_sessions,
            "status inherited sessions",
        )
        .extend(ids);
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
        lock_recover(&self.sessions, "sessions")
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
        let sessions = lock_recover(&self.sessions, "sessions");
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
        let (replay, cols, rows, attachment_id) = match resp {
            Response::AttachOk {
                replay,
                cols,
                rows,
                attachment_id,
            } => (replay, cols, rows, attachment_id),
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
        // NOTE: attach uses the const scrollback (not the user's
        // `scrollback_lines` setting) by design — the replay buffer must hold
        // the daemon's full retained output and the receiver grid has to be
        // sized to fit it, regardless of what the user picked for fresh spawns.
        let state = Arc::new(Mutex::new(TerminalState::new(cols, rows, SCROLLBACK_ROWS)));
        {
            let mut s = lock_recover(&state, "terminal state");
            s.advance(&replay);
            // Replay is historical: drop title/clipboard/bell/color events it
            // fired so they don't leak into the first live frame.
            s.clear_collected();
        }

        let id = self.mint_id();
        let generation = Arc::new(AtomicU64::new(1));
        let (sub_id, notif_rx) = self.client.subscribe_pty(relay_pty_id);
        let pump = self.spawn_pump(id, Arc::clone(&state), Arc::clone(&generation), 1, notif_rx);
        lock_recover(&self.sessions, "sessions").insert(
            id,
            Session {
                relay_pty_id: relay_pty_id.to_owned(),
                attachment_id,
                sub_id,
                state,
                cols,
                rows,
                generation,
                _pump: pump,
            },
        );
        Ok(id)
    }

    fn spawn_pump(
        &self,
        id: TerminalSessionId,
        state: Arc<Mutex<TerminalState>>,
        generation: Arc<AtomicU64>,
        my_generation: u64,
        mut notif_rx: UnboundedReceiver<Notification>,
    ) -> JoinHandle<()> {
        let queues = Arc::clone(&self.event_queues);
        let wakers = Arc::clone(&self.output_wakers);
        self.handle.spawn(async move {
            while let Some(n) = notif_rx.recv().await {
                let flow =
                    apply_relay_notification(&state, &queues, id, &generation, my_generation, n);
                // Event-driven drain: nudge the UI the instant output is queued
                // so it renders on arrival instead of waiting for the next
                // (macOS-throttled) poll tick. Fired for every notification —
                // an empty drain is cheap; a missed wake is felt lag.
                if let Some(waker) = lock_recover(&wakers, "output wakers").get(&id) {
                    waker();
                }
                if flow.is_break() {
                    return;
                }
            }
        })
    }
}

/// Apply one relay notification to the local terminal state + event
/// queue, honoring the reconnect generation guard. Returns
/// `ControlFlow::Break` when the pump must stop — either the session was
/// superseded (`generation` moved past `my_generation`) or the PTY
/// exited. Extracted from the pump loop so the guard is unit-testable
/// without standing up a tokio task.
fn apply_relay_notification(
    state: &Arc<Mutex<TerminalState>>,
    queues: &SessionEventQueues,
    id: TerminalSessionId,
    generation: &AtomicU64,
    my_generation: u64,
    n: Notification,
) -> ControlFlow<()> {
    // Generation guard: a superseded pump must not touch the shared
    // `TerminalState` or emit events. Checked BEFORE any `advance` so a
    // stale pump performs zero advances after being superseded.
    if generation.load(Ordering::Acquire) != my_generation {
        return ControlFlow::Break(());
    }
    match n {
        Notification::Output { bytes, .. } => {
            // Collect derived events (bell, command marks, progress, title,
            // clipboard, device/color replies) in the same locked pass that
            // advances the grid, then queue them AHEAD of this chunk's Output
            // so attention (bell) lands before the bytes that raised it.
            //
            // Containment boundary: a panic inside the emulator advance
            // (one bad byte stream) must kill only THIS session — surfaced
            // as a synthetic exit — never the whole backend. The poisoned
            // state mutex left behind is fine: every other accessor treats
            // a failed state lock as "skip", and the session is dead from
            // here on (same lifecycle as a real process exit; the view's
            // normal exit handling performs the close/cleanup). The event
            // `queues` mutex is a separate lock the panic never holds, so
            // the synthetic Exit below always delivers.
            let advanced = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                #[cfg(test)]
                PANIC_ON_ADVANCE.with(|f| {
                    if f.get() {
                        panic!("injected pump panic");
                    }
                });
                match state.lock() {
                    Ok(mut s) => s.advance_collecting(id, &bytes),
                    Err(_) => Vec::new(),
                }
            }));
            let derived = match advanced {
                Ok(d) => d,
                Err(payload) => {
                    tracing::error!(
                        ?id,
                        panic = panic_msg(payload.as_ref()),
                        "terminal emulator panicked; retiring session with synthetic exit"
                    );
                    push_event(queues, id, TerminalEvent::Exit { id, code: None });
                    return ControlFlow::Break(());
                }
            };
            for ev in derived {
                // The daemon owns cwd over the relay; drop OSC 7 here rather
                // than threading a cwd cache the relay backend doesn't keep.
                if matches!(ev, TerminalEvent::CwdChanged { .. }) {
                    continue;
                }
                push_event(queues, id, ev);
            }
            arrival_trace(bytes.len());
            push_event(queues, id, TerminalEvent::Output { id, bytes });
            ControlFlow::Continue(())
        }
        Notification::Exit { code, .. } => {
            push_event(queues, id, TerminalEvent::Exit { id, code });
            ControlFlow::Break(())
        }
        // Explicit `oximux notify` → raise the same pane attention as a
        // bell. (title/body are carried on the wire for a future OS-banner
        // surface; not consumed here yet.)
        Notification::Attention { .. } => {
            push_event(queues, id, TerminalEvent::Bell { id });
            ControlFlow::Continue(())
        }
        // The daemon discarded output for this subscriber because our queue was
        // full. The bytes survive in the session's replay ring, so recovery is a
        // re-attach — which this pump cannot perform: it holds no client handle,
        // and a fresh `Attach` would register a *second* attachment in the
        // daemon's smallest-screen-wins size calculation unless the old one is
        // released in the same step.
        //
        // So this arm is deliberately inert for now, and deliberately explicit
        // rather than a catch-all: the grid is known-stale from here, and that
        // fact should force a decision at this site rather than be swallowed by
        // a `_ =>`. Continue rather than Break — a terminal with a hole in it is
        // still more useful than one that abruptly dies.
        Notification::Gapped { .. } => {
            tracing::warn!(
                ?id,
                "daemon dropped output for this subscriber; terminal contents are stale \
                 until the session is re-attached"
            );
            ControlFlow::Continue(())
        }
    }
}

impl TerminalBackend for RelayBackend {
    fn attach_existing(&mut self, external_id: &str) -> Result<TerminalSessionId> {
        self.attach_relay_pty(external_id)
    }

    fn set_output_waker(&mut self, id: TerminalSessionId, waker: oximux_pty::OutputWaker) {
        lock_recover(&self.output_wakers, "output wakers").insert(id, waker);
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
            args: cfg.args,
            env: cfg.env,
        })?;
        let (relay_pty_id, attachment_id) = match resp {
            Response::SpawnOk {
                pty_id,
                attachment_id,
            } => (pty_id, attachment_id),
            Response::Err { code, message } => bail!("spawn: {code:?} — {message}"),
            other => bail!("unexpected spawn response: {other:?}"),
        };

        let state = Arc::new(Mutex::new(TerminalState::new(
            cfg.cols,
            cfg.rows,
            cfg.scrollback,
        )));
        let id = self.mint_id();
        if cfg.capture_status_events {
            lock_recover(&self.event_queues, "event queues")
                .status
                .entry(id)
                .or_default();
        }
        let generation = Arc::new(AtomicU64::new(1));
        let (sub_id, notif_rx) = self.client.subscribe_pty(&relay_pty_id);
        let pump = self.spawn_pump(id, Arc::clone(&state), Arc::clone(&generation), 1, notif_rx);
        lock_recover(&self.sessions, "sessions").insert(
            id,
            Session {
                relay_pty_id,
                attachment_id,
                sub_id,
                state,
                cols: cfg.cols,
                rows: cfg.rows,
                generation,
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
        // Called from the GPUI render thread whenever the pane bounds change.
        // It MUST NOT block on a daemon round-trip: a synchronous
        // `Handle::block_on` here parks the render loop (and, on macOS, lets
        // the system wedge the main thread in the App-Nap assertion path).
        //
        // Instead we update the local grid immediately — the daemon's ack
        // carries no data the renderer needs — and fire the daemon resize as
        // a detached task on the relay runtime. The live process repaints via
        // SIGWINCH on the output stream; a failed resize surfaces there, not
        // through a per-call ack. This mirrors how a terminal forwards a
        // resize to its PTY (a one-way ioctl) without waiting.
        let (pty_id, attachment_id) = {
            let mut sessions = lock_recover(&self.sessions, "sessions");
            let s = sessions
                .get_mut(&id)
                .ok_or_else(|| anyhow!("unknown session {id:?}"))?;
            s.cols = cols;
            s.rows = rows;
            if let Ok(mut state) = s.state.lock() {
                state.resize(cols, rows);
            }
            (s.relay_pty_id.clone(), s.attachment_id)
        };
        push_event(
            &self.event_queues,
            id,
            TerminalEvent::Resize { id, cols, rows },
        );
        let client = Arc::clone(&self.client);
        self.handle.spawn(async move {
            if let Err(err) = client
                .request(Request::Resize {
                    pty_id,
                    attachment_id,
                    cols,
                    rows,
                })
                .await
            {
                tracing::warn!(?err, "daemon resize request failed");
            }
        });
        Ok(())
    }

    fn snapshot(&self, id: TerminalSessionId) -> Result<TerminalSnapshot> {
        let sessions = lock_recover(&self.sessions, "sessions");
        let session = sessions
            .get(&id)
            .ok_or_else(|| anyhow!("unknown session {id:?}"))?;
        let mut snap = TerminalSnapshot::empty(session.cols, session.rows);
        if let Ok(state) = session.state.lock() {
            state.fill_snapshot(&mut snap);
        }
        Ok(snap)
    }

    fn input_mode(&self, id: TerminalSessionId) -> oximux_pty::InputMode {
        let sessions = lock_recover(&self.sessions, "sessions");
        sessions
            .get(&id)
            .and_then(|s| s.state.lock().ok().map(|st| st.input_mode()))
            .unwrap_or_default()
    }

    fn mouse_mode(&self, id: TerminalSessionId) -> oximux_pty::MouseMode {
        let sessions = lock_recover(&self.sessions, "sessions");
        sessions
            .get(&id)
            .and_then(|s| s.state.lock().ok().map(|st| st.mouse_mode()))
            .unwrap_or_default()
    }

    fn scroll(&mut self, id: TerminalSessionId, delta: i32) -> Result<()> {
        let sessions = lock_recover(&self.sessions, "sessions");
        let session = sessions
            .get(&id)
            .ok_or_else(|| anyhow!("unknown session {id:?}"))?;
        if let Ok(mut state) = session.state.lock() {
            state.scroll_lines(delta);
        }
        Ok(())
    }

    fn scroll_to_bottom(&mut self, id: TerminalSessionId) -> Result<()> {
        let sessions = lock_recover(&self.sessions, "sessions");
        let session = sessions
            .get(&id)
            .ok_or_else(|| anyhow!("unknown session {id:?}"))?;
        if let Ok(mut state) = session.state.lock() {
            state.scroll_to_bottom();
        }
        Ok(())
    }

    fn clear(&mut self, id: TerminalSessionId) -> Result<()> {
        // Render-side wipe of the LOCAL grid mirror only — the daemon PTY is
        // never told, so a cold-restore could replay the daemon's retained
        // scrollback. Acceptable for v1: Clear is a "tidy my screen now"
        // affordance, not a remote-history purge.
        let sessions = lock_recover(&self.sessions, "sessions");
        let session = sessions
            .get(&id)
            .ok_or_else(|| anyhow!("unknown session {id:?}"))?;
        if let Ok(mut state) = session.state.lock() {
            state.clear();
        }
        Ok(())
    }

    fn bracketed_paste(&self, id: TerminalSessionId) -> Result<bool> {
        let sessions = lock_recover(&self.sessions, "sessions");
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
        let sessions = lock_recover(&self.sessions, "sessions");
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
        let sessions = lock_recover(&self.sessions, "sessions");
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
        let sessions = lock_recover(&self.sessions, "sessions");
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
            state.clear_collected();
        }
        Ok(())
    }

    fn drain_events(&mut self) -> Vec<TerminalEvent> {
        // Drains every session's queue. Used by tests + cleanup paths;
        // the per-frame render path uses `drain_events_for` instead so
        // each pane only sees its own events.
        let mut queues = lock_recover(&self.event_queues, "event queues");
        let mut out = Vec::new();
        for q in queues.renderer.values_mut() {
            out.extend(q.drain(..));
        }
        // Crash-recovery: flush every inherited dead session as one
        // synthetic Exit each (see `seed_synthetic_exits`).
        let mut inherited = lock_recover(&self.inherited_dead_sessions, "inherited sessions");
        out.extend(
            inherited
                .drain()
                .map(|id| TerminalEvent::Exit { id, code: None }),
        );
        out
    }

    fn drain_events_for(&mut self, id: TerminalSessionId) -> Vec<TerminalEvent> {
        // Crash-recovery: an inherited dead session yields exactly one
        // synthetic Exit so its poller (agent status machine, pane tick)
        // learns the process died with the old daemon.
        if lock_recover(&self.inherited_dead_sessions, "inherited sessions").remove(&id) {
            return vec![TerminalEvent::Exit { id, code: None }];
        }
        let mut queues = lock_recover(&self.event_queues, "event queues");
        match queues.renderer.get_mut(&id) {
            Some(q) => q.drain(..).collect(),
            None => Vec::new(),
        }
    }

    fn subscribe_status_events(&mut self, id: TerminalSessionId) -> Result<()> {
        let known = lock_recover(&self.sessions, "sessions").contains_key(&id)
            || lock_recover(
                &self.status_inherited_dead_sessions,
                "status inherited sessions",
            )
            .contains(&id);
        if !known {
            return Err(anyhow!("unknown session {id:?}"));
        }
        let mut queues = lock_recover(&self.event_queues, "event queues");
        register_status_queue(&mut queues, id);
        Ok(())
    }

    fn drain_status_events_for(&mut self, id: TerminalSessionId) -> Vec<TerminalEvent> {
        if lock_recover(
            &self.status_inherited_dead_sessions,
            "status inherited sessions",
        )
        .remove(&id)
        {
            return vec![TerminalEvent::Exit { id, code: None }];
        }
        lock_recover(&self.event_queues, "event queues")
            .status
            .get_mut(&id)
            .map(|queue| queue.drain(..).collect())
            .unwrap_or_default()
    }

    fn unsubscribe_status_events(&mut self, id: TerminalSessionId) {
        lock_recover(&self.event_queues, "event queues")
            .status
            .remove(&id);
        lock_recover(
            &self.status_inherited_dead_sessions,
            "status inherited sessions",
        )
        .remove(&id);
    }

    fn live_session_ids(&self) -> Vec<TerminalSessionId> {
        lock_recover(&self.sessions, "sessions")
            .keys()
            .copied()
            .collect()
    }

    fn close(&mut self, id: TerminalSessionId) -> Result<()> {
        let session = match lock_recover(&self.sessions, "sessions").remove(&id) {
            Some(s) => s,
            None => return Ok(()),
        };
        push_status_event_only(
            &self.event_queues,
            id,
            TerminalEvent::Exit { id, code: None },
        );
        // Supersede the pump (generation guard): once bumped, the pump
        // stops advancing the now-orphaned `TerminalState` instead of
        // draining any still-buffered Output during teardown.
        session.generation.fetch_add(1, Ordering::Release);
        let mut queues = lock_recover(&self.event_queues, "event queues");
        queues.renderer.remove(&id);
        drop(queues);
        lock_recover(&self.output_wakers, "output wakers").remove(&id);
        self.client.unsubscribe_pty(&session.relay_pty_id, session.sub_id);
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
            match client
                .request(Request::Close {
                    pty_id: pty_id.clone(),
                    grace_ms: 500,
                })
                .await
            {
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

    fn detach(&mut self, id: TerminalSessionId) -> Result<()> {
        // Mirror `close`'s local teardown, but tell the daemon to DETACH this
        // attachment instead of killing the PTY — the tab is moving to another
        // window, which re-attaches by `relay_pty_id`. Because the local
        // session is removed here, the source `TerminalView`'s eventual
        // `Drop` → `close(id)` finds nothing and is a no-op, so the daemon PTY
        // survives the move.
        let session = match lock_recover(&self.sessions, "sessions").remove(&id) {
            Some(s) => s,
            None => return Ok(()),
        };
        push_status_event_only(
            &self.event_queues,
            id,
            TerminalEvent::Exit { id, code: None },
        );
        // Supersede the pump (generation guard) so it stops advancing the
        // now-detached `TerminalState` — the destination window mounts a fresh
        // session + pump against the same PTY.
        session.generation.fetch_add(1, Ordering::Release);
        let mut queues = lock_recover(&self.event_queues, "event queues");
        queues.renderer.remove(&id);
        drop(queues);
        lock_recover(&self.output_wakers, "output wakers").remove(&id);
        self.client.unsubscribe_pty(&session.relay_pty_id, session.sub_id);
        // Release this attachment in the daemon. Detach (not Close) keeps the
        // PTY alive; it also drops this attachment from the daemon's
        // smallest-screen-wins `min`, so the surviving / destination window
        // drives the real size. Deferred to a detached task for the same
        // reason as `close` (don't block the GPUI thread on the round-trip).
        let client = self.client.clone();
        let pty_id = session.relay_pty_id.clone();
        let attachment_id = session.attachment_id;
        self.handle.spawn(async move {
            match client
                .request(Request::Detach {
                    pty_id: pty_id.clone(),
                    attachment_id,
                })
                .await
            {
                Ok(Response::Ok) => {}
                // PtyNotFound is benign — the PTY was already reaped.
                Ok(Response::Err {
                    code: oximux_relay_proto::ErrCode::PtyNotFound,
                    ..
                }) => {}
                Ok(other) => {
                    tracing::warn!(?pty_id, ?other, "relay detach: unexpected response");
                }
                Err(err) => {
                    tracing::warn!(?pty_id, ?err, "relay detach: request failed");
                }
            }
        });
        Ok(())
    }
}

fn register_status_queue(queues: &mut EventQueues, id: TerminalSessionId) {
    if queues.status.contains_key(&id) {
        return;
    }
    let pending = queues
        .renderer
        .get(&id)
        .into_iter()
        .flat_map(|queue| queue.iter())
        .filter(|event| {
            matches!(
                event,
                TerminalEvent::Output { .. } | TerminalEvent::Exit { .. }
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut status = VecDeque::new();
    for event in pending {
        push_bounded_status(&mut status, event);
    }
    queues.status.insert(id, status);
}

fn push_event(queues: &SessionEventQueues, id: TerminalSessionId, event: TerminalEvent) {
    let mut queues = lock_recover(queues, "event queues");
    if matches!(
        event,
        TerminalEvent::Output { .. } | TerminalEvent::Exit { .. }
    ) && let Some(status) = queues.status.get_mut(&id)
    {
        push_bounded_status(status, event.clone());
    }
    queues.renderer.entry(id).or_default().push_back(event);
}

fn push_status_event_only(
    queues: &SessionEventQueues,
    id: TerminalSessionId,
    event: TerminalEvent,
) {
    if let Some(status) = lock_recover(queues, "event queues").status.get_mut(&id) {
        push_bounded_status(status, event);
    }
}

fn push_bounded_status(queue: &mut VecDeque<TerminalEvent>, event: TerminalEvent) {
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
    queue.push_back(event);
}

/// Lock a mutex, recovering from poison instead of propagating the
/// panic. Every guarded value in this backend is a plain-data map or
/// per-session terminal grid that stays usable after another thread
/// panicked mid-access (the pump's panic boundary retires the affected
/// session with a synthetic exit), so recovery can never hand out state
/// a caller can't safely drop — whereas propagating would cascade one
/// bad session into whole-backend death.
fn lock_recover<'a, T: ?Sized>(
    m: &'a Mutex<T>,
    what: &'static str,
) -> std::sync::MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|poisoned| {
        tracing::error!(what, "mutex poisoned; recovering");
        poisoned.into_inner()
    })
}

/// Best-effort text of a caught panic payload for the containment log.
fn panic_msg(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
}

// Test-only injection: makes the next emulator advance inside
// `apply_relay_notification` panic, exercising the pump's containment
// boundary without needing a byte sequence that crashes the real parser.
#[cfg(test)]
thread_local! {
    static PANIC_ON_ADVANCE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
mod tests {
    use super::*;

    // A panic while some thread held a backend lock must not take every
    // later backend call down with it: lock_recover hands back the
    // still-consistent map instead of propagating the poison.
    #[test]
    fn lock_recover_survives_poisoned_mutex() {
        let m = Arc::new(Mutex::new(vec![1u8]));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _guard = m2.lock().unwrap();
            panic!("poison the lock");
        })
        .join();
        assert!(m.lock().is_err(), "mutex must be poisoned by the panic");
        assert_eq!(*lock_recover(&m, "test"), vec![1u8]);
    }

    fn queue_len(queues: &SessionEventQueues, id: TerminalSessionId) -> usize {
        queues
            .lock()
            .unwrap()
            .renderer
            .get(&id)
            .map(|q| q.len())
            .unwrap_or(0)
    }

    // Containment boundary: a panic inside the emulator advance must
    // retire ONLY that session — one synthetic Exit event, pump stops —
    // and must not unwind into the caller (which would kill the backend).
    #[test]
    fn pump_panic_is_contained_as_synthetic_exit() {
        let id = TerminalSessionId(42);
        let state = Arc::new(Mutex::new(TerminalState::new(80, 24, 100)));
        let queues: SessionEventQueues = Arc::new(Mutex::new(EventQueues::default()));
        let generation = AtomicU64::new(1);

        PANIC_ON_ADVANCE.with(|f| f.set(true));
        let flow = apply_relay_notification(
            &state,
            &queues,
            id,
            &generation,
            1,
            Notification::Output {
                pty_id: "p".into(),
                bytes: b"boom".to_vec(),
            },
        );
        PANIC_ON_ADVANCE.with(|f| f.set(false));

        assert!(flow.is_break(), "panicked session's pump must stop");
        let guard = queues.lock().unwrap();
        let q = guard.renderer.get(&id).expect("synthetic exit queued");
        assert_eq!(q.len(), 1, "exactly one synthetic event");
        assert!(
            matches!(q[0], TerminalEvent::Exit { code: None, .. }),
            "the synthetic event is Exit {{ code: None }}"
        );
        drop(guard);

        // The backend itself survives: another session keeps pumping
        // normally after the contained panic.
        let other = TerminalSessionId(43);
        let other_state = Arc::new(Mutex::new(TerminalState::new(80, 24, 100)));
        let flow = apply_relay_notification(
            &other_state,
            &queues,
            other,
            &generation,
            1,
            Notification::Output {
                pty_id: "q".into(),
                bytes: b"hi".to_vec(),
            },
        );
        assert!(flow.is_continue(), "other sessions are unaffected");
        assert_eq!(queue_len(&queues, other), 1);
    }

    // The reconnect generation guard: once a pump is superseded (the
    // shared generation moves past the value it captured), it must stop
    // touching the terminal state AND stop emitting events — zero
    // advances after supersession via a monotonic attach-generation guard.
    #[test]
    fn generation_guard_stops_superseded_pump() {
        let id = TerminalSessionId(1);
        let state = Arc::new(Mutex::new(TerminalState::new(80, 24, 100)));
        let queues: SessionEventQueues = Arc::new(Mutex::new(EventQueues::default()));
        let generation = AtomicU64::new(1);

        // Matching generation → applies and emits an Output event.
        let flow = apply_relay_notification(
            &state,
            &queues,
            id,
            &generation,
            1,
            Notification::Output {
                pty_id: "p".into(),
                bytes: b"hi".to_vec(),
            },
        );
        assert!(flow.is_continue(), "live pump keeps going");
        assert_eq!(queue_len(&queues, id), 1, "live pump emits Output");

        // A newer attach bumps the generation, superseding this pump.
        generation.store(2, Ordering::Release);
        let flow = apply_relay_notification(
            &state,
            &queues,
            id,
            &generation,
            1,
            Notification::Output {
                pty_id: "p".into(),
                bytes: b"XX".to_vec(),
            },
        );
        assert!(flow.is_break(), "superseded pump must stop");
        assert_eq!(
            queue_len(&queues, id),
            1,
            "superseded pump performs zero advance + emits nothing"
        );
    }

    // A live pump (matching generation) receiving a BEL byte must surface
    // attention: a Bell event queued AHEAD of the Output chunk that carried
    // it, so a background tab lights up. Order matters — the tab strip reads
    // the Bell before painting the output.
    #[test]
    fn bell_byte_in_output_emits_bell_then_output() {
        let id = TerminalSessionId(7);
        let state = Arc::new(Mutex::new(TerminalState::new(80, 24, 100)));
        let queues: SessionEventQueues = Arc::new(Mutex::new(EventQueues::default()));
        let generation = AtomicU64::new(1);

        let flow = apply_relay_notification(
            &state,
            &queues,
            id,
            &generation,
            1,
            Notification::Output {
                pty_id: "p".into(),
                bytes: vec![0x07], // BEL
            },
        );
        assert!(flow.is_continue(), "live pump keeps going after a bell");

        let guard = queues.lock().unwrap();
        let q = guard.renderer.get(&id).expect("events queued");
        assert_eq!(q.len(), 2, "a BEL chunk emits Bell + Output");
        assert!(matches!(q[0], TerminalEvent::Bell { .. }), "Bell first");
        assert!(
            matches!(q[1], TerminalEvent::Output { .. }),
            "Output second"
        );
    }

    // An Exit notification (matching generation) must emit an Exit event AND
    // stop the pump — the PTY is gone, so there is nothing more to read.
    #[test]
    fn exit_notification_emits_exit_and_breaks() {
        let id = TerminalSessionId(8);
        let state = Arc::new(Mutex::new(TerminalState::new(80, 24, 100)));
        let queues: SessionEventQueues = Arc::new(Mutex::new(EventQueues::default()));
        let generation = AtomicU64::new(1);

        let flow = apply_relay_notification(
            &state,
            &queues,
            id,
            &generation,
            1,
            Notification::Exit {
                pty_id: "p".into(),
                code: Some(0),
            },
        );
        assert!(flow.is_break(), "Exit stops the pump");
        let guard = queues.lock().unwrap();
        let q = guard.renderer.get(&id).expect("events queued");
        assert_eq!(q.len(), 1);
        assert!(
            matches!(q[0], TerminalEvent::Exit { .. }),
            "Exit event emitted"
        );
    }

    // An explicit `oximux notify` Attention notification raises the same
    // pane attention as a bell and keeps the pump running.
    #[test]
    fn attention_notification_emits_bell_and_continues() {
        let id = TerminalSessionId(9);
        let state = Arc::new(Mutex::new(TerminalState::new(80, 24, 100)));
        let queues: SessionEventQueues = Arc::new(Mutex::new(EventQueues::default()));
        let generation = AtomicU64::new(1);

        let flow = apply_relay_notification(
            &state,
            &queues,
            id,
            &generation,
            1,
            Notification::Attention {
                pty_id: "p".into(),
                title: "Claude".into(),
                body: "needs you".into(),
            },
        );
        assert!(flow.is_continue(), "Attention keeps the pump running");
        let guard = queues.lock().unwrap();
        let q = guard.renderer.get(&id).expect("events queued");
        assert_eq!(q.len(), 1);
        assert!(
            matches!(q[0], TerminalEvent::Bell { .. }),
            "Attention → Bell"
        );
    }

    // The guard's core anti-scramble guarantee: a SUPERSEDED pump must not
    // advance the shared `TerminalState`. Feed a BEL byte under a stale
    // generation; because the guard breaks before `advance`, the bell flag
    // is never set on the grid — `take_bell` stays false. This pins "zero
    // advance after supersession" directly on the state, not just the event
    // queue (a stale pump scribbling on the reattached grid is exactly what
    // scrambled restored full-screen TUIs).
    #[test]
    fn superseded_pump_leaves_state_unadvanced() {
        let id = TerminalSessionId(10);
        let state = Arc::new(Mutex::new(TerminalState::new(80, 24, 100)));
        let queues: SessionEventQueues = Arc::new(Mutex::new(EventQueues::default()));
        let generation = AtomicU64::new(2); // a newer attach already moved on

        let flow = apply_relay_notification(
            &state,
            &queues,
            id,
            &generation,
            1, // this pump captured the old generation
            Notification::Output {
                pty_id: "p".into(),
                bytes: vec![0x07], // BEL — would set the bell flag IF advanced
            },
        );
        assert!(flow.is_break(), "superseded pump stops");
        assert_eq!(queue_len(&queues, id), 0, "no events from a stale pump");
        assert!(
            !state.lock().unwrap().take_bell(),
            "stale pump must not advance the grid (BEL never reached state)"
        );
    }

    #[test]
    fn late_status_registration_backfills_without_consuming_renderer_events() {
        let id = TerminalSessionId(20);
        let queues: SessionEventQueues = Arc::new(Mutex::new(EventQueues::default()));
        push_event(
            &queues,
            id,
            TerminalEvent::Output {
                id,
                bytes: b"STATUS_MARKER".to_vec(),
            },
        );
        push_event(&queues, id, TerminalEvent::Bell { id });
        push_event(&queues, id, TerminalEvent::Exit { id, code: Some(7) });

        let mut guard = queues.lock().unwrap();
        register_status_queue(&mut guard, id);
        let status = guard
            .status
            .get_mut(&id)
            .unwrap()
            .drain(..)
            .collect::<Vec<_>>();
        let renderer = guard
            .renderer
            .get_mut(&id)
            .unwrap()
            .drain(..)
            .collect::<Vec<_>>();

        assert_eq!(status.len(), 2, "status receives only Output and Exit");
        assert!(matches!(status[0], TerminalEvent::Output { .. }));
        assert!(matches!(
            status[1],
            TerminalEvent::Exit { code: Some(7), .. }
        ));
        assert_eq!(renderer.len(), 3, "backfill leaves renderer queue intact");
        assert!(matches!(renderer[1], TerminalEvent::Bell { .. }));
    }

    #[test]
    fn bounded_status_queue_preserves_exit_under_output_pressure() {
        let id = TerminalSessionId(21);
        let mut queue = VecDeque::new();
        for byte in 0..=STATUS_EVENT_CAPACITY {
            push_bounded_status(
                &mut queue,
                TerminalEvent::Output {
                    id,
                    bytes: vec![byte as u8],
                },
            );
        }
        push_bounded_status(&mut queue, TerminalEvent::Exit { id, code: Some(7) });

        assert_eq!(queue.len(), STATUS_EVENT_CAPACITY);
        assert!(matches!(
            queue.back(),
            Some(TerminalEvent::Exit { code: Some(7), .. })
        ));
    }
}
