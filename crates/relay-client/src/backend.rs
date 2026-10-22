use std::collections::{HashMap, VecDeque};
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use oximux_pty::{
    Cell, SpawnConfig, TerminalBackend, TerminalEvent, TerminalSessionId, TerminalSnapshot,
    TerminalState, apply_color_fg_bg, background_polarity, is_status_event, push_bounded_status,
};
use oximux_relay_proto::{Notification, Request, Response};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::attach::AttachCtx;
use crate::client::RelayClient;

// Match the in-process backend's scrollback so visual continuity is
// preserved when an app switches between in-process and relay
// backends mid-session (e.g., relay went down + supervisor restarted).
pub(crate) const SCROLLBACK_ROWS: usize = 5000;

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
pub(crate) struct EventQueues {
    renderer: HashMap<TerminalSessionId, VecDeque<TerminalEvent>>,
    status: HashMap<TerminalSessionId, VecDeque<TerminalEvent>>,
}

pub(crate) type SessionEventQueues = Arc<Mutex<EventQueues>>;

/// Per-session drain signals, shared with each pump.
pub(crate) type OutputWakers = Arc<Mutex<HashMap<TerminalSessionId, oximux_pty::OutputWaker>>>;

pub(crate) struct Session {
    pub(crate) relay_pty_id: String,
    // Daemon-minted handle for THIS attachment. Sent back on every
    // `Resize`/`Detach` so the daemon updates the right attachment's
    // requested size in its "smallest screen wins" `min` computation.
    pub(crate) attachment_id: u64,
    // This session's handle in the client's per-PTY subscriber fan-out.
    // Unique per attachment so teardown removes only OUR output stream and
    // never a sibling session that shares the same daemon PTY.
    pub(crate) sub_id: u64,
    pub(crate) state: Arc<Mutex<TerminalState>>,
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    // Reconnect guard: a monotonic attach-generation. The pump captures
    // the value at spawn time and stops touching `state`/emitting events once
    // it no longer matches — so a superseded pump (a future reconnect /
    // multi-window re-attach, or this session's own teardown) can't
    // double-drive the shared `TerminalState`. Bumped in `close` so the
    // pump stops draining into an orphaned state during teardown.
    pub(crate) generation: Arc<AtomicU64>,
    pub(crate) _pump: JoinHandle<()>,
}

/// Session ids for every `RelayBackend` in the process. One counter, not one
/// per backend: a replaced backend's views stay mounted holding their ids —
/// possibly for good, if bringing them back fails — so a successor starting
/// its own count could mint one of them again and cross-wire two panes.
pub(crate) static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

pub struct RelayBackend {
    client: Arc<RelayClient>,
    handle: Handle,
    sessions: Arc<Mutex<HashMap<TerminalSessionId, Session>>>,
    event_queues: SessionEventQueues,
    /// Per-session event-driven drain signals. The pump invokes the matching
    /// waker right after enqueuing output so the UI drains on arrival instead
    /// of polling a (throttled) timer. Shared with each pump task by Arc clone.
    output_wakers: OutputWakers,
    /// Session ids inherited from a predecessor backend that died with the
    /// old daemon (a crash or a restart). Each yields exactly one `DaemonLost`
    /// from `drain_events_for` (renderer) and from `drain_status_events_for`
    /// (status pollers), so its owner learns the process died with the daemon
    /// instead of draining nothing forever.
    inherited_dead_sessions: Mutex<std::collections::HashSet<TerminalSessionId>>,
    status_inherited_dead_sessions: Mutex<std::collections::HashSet<TerminalSessionId>>,
    /// The daemon-side id each inherited session had, so `external_id_of`
    /// still answers for it — recovery finds the lost session's checkpoint by
    /// it, even for a view that mounts the session only after the swap.
    inherited_external_ids: Mutex<HashMap<TerminalSessionId, String>>,
    /// Whether this client answers the OSC 7501 feature queries of the PTYs it
    /// shows. A current daemon answers its own; this is for one left over from
    /// an older version (kept after an update because it has sessions), which
    /// predates that and would otherwise leave every program unanswered.
    answers_program_status: bool,
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
            sessions: Arc::new(Mutex::new(HashMap::new())),
            event_queues: Arc::new(Mutex::new(EventQueues::default())),
            output_wakers: Arc::new(Mutex::new(HashMap::new())),
            inherited_dead_sessions: Mutex::new(std::collections::HashSet::new()),
            status_inherited_dead_sessions: Mutex::new(std::collections::HashSet::new()),
            inherited_external_ids: Mutex::new(HashMap::new()),
            answers_program_status: false,
        }
    }

    /// Answer program-status queries client-side — only for a daemon that does
    /// not answer them itself (see [`daemon_answers_program_status`]).
    pub fn with_program_status_replies(mut self, answers: bool) -> Self {
        self.answers_program_status = answers;
        self
    }

    /// Daemon-replacement seeding: take over the dead predecessor backend's
    /// sessions (`TerminalBackend::sessions_to_carry`) so each yields one
    /// `DaemonLost` on its next drain — the signal its owner uses to bring the
    /// session back on this backend — and keeps answering `external_id_of`.
    pub fn seed_daemon_losses(&self, sessions: Vec<(TerminalSessionId, Option<String>)>) {
        if sessions.is_empty() {
            return;
        }
        // Belt and braces: the counter is process-wide, so this only matters
        // for ids minted by some other backend type.
        let max_inherited = sessions.iter().map(|(id, _)| id.0).max().unwrap_or(0);
        NEXT_SESSION_ID.fetch_max(max_inherited + 1, Ordering::Relaxed);
        let ids: Vec<TerminalSessionId> = sessions.iter().map(|(id, _)| *id).collect();
        lock_recover(&self.inherited_dead_sessions, "inherited sessions")
            .extend(ids.iter().copied());
        lock_recover(
            &self.status_inherited_dead_sessions,
            "status inherited sessions",
        )
        .extend(ids);
        lock_recover(&self.inherited_external_ids, "inherited external ids").extend(
            sessions
                .into_iter()
                .filter_map(|(id, external)| external.map(|external| (id, external))),
        );
    }

    /// Forget an inherited loss: its view let go of the session.
    fn forget_inherited(&self, id: TerminalSessionId) {
        lock_recover(&self.inherited_dead_sessions, "inherited sessions").remove(&id);
        lock_recover(&self.status_inherited_dead_sessions, "status inherited sessions")
            .remove(&id);
        lock_recover(&self.inherited_external_ids, "inherited external ids").remove(&id);
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
        TerminalSessionId(NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed))
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
    // method forwards to this. See `AttachCtx::attach`.
    pub fn attach_relay_pty(&self, relay_pty_id: &str) -> Result<TerminalSessionId> {
        self.attach_ctx().attach(relay_pty_id)
    }

    /// Everything an attach or a pump needs, cloned out of the backend so the
    /// work can run without the caller holding it (see `begin_attach_existing`).
    pub(crate) fn attach_ctx(&self) -> AttachCtx {
        AttachCtx {
            client: Arc::clone(&self.client),
            handle: self.handle.clone(),
            sessions: Arc::clone(&self.sessions),
            queues: Arc::clone(&self.event_queues),
            wakers: Arc::clone(&self.output_wakers),
            answers_program_status: self.answers_program_status,
        }
    }
}

/// The first release whose daemon answers OSC 7501 program-status queries.
pub const PROGRAM_STATUS_REPLIES_SINCE: (u64, u64, u64) = (0, 1, 43);

/// Whether a daemon of `version` answers its PTYs' program-status queries, so
/// the client must not. A daemon kept after an update is merely *another*
/// version — possibly a newer one, which does answer — so the decision is the
/// version, not the mismatch. An unreadable version is taken as old: a second
/// reply is harmless to the program, none at all loses its status.
pub fn daemon_answers_program_status(version: &str) -> bool {
    let mut parts = version
        .split(['-', '+'])
        .next()
        .unwrap_or_default()
        .split('.')
        .map(str::parse::<u64>);
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Ok(major)), Some(Ok(minor)), Some(Ok(patch))) => {
            (major, minor, patch) >= PROGRAM_STATUS_REPLIES_SINCE
        }
        _ => false,
    }
}

/// Apply one relay notification to the local terminal state + event
/// queue, honoring the reconnect generation guard. Returns
/// `ControlFlow::Break` when the pump must stop — either the session was
/// superseded (`generation` moved past `my_generation`) or the PTY
/// exited. Extracted from the pump loop so the guard is unit-testable
/// without standing up a tokio task.
pub(crate) fn apply_relay_notification(
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
        // Explicit `oximux notify`: the same terminal-notification path as a
        // program's OSC 9 / 777, marked explicit (the user asked for it, so no
        // opt-in applies). An empty title means none was given.
        Notification::Attention { title, body, .. } => {
            let title = (!title.is_empty()).then_some(title);
            push_event(queues, id, TerminalEvent::Notify { id, title, body, explicit: true });
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

    fn begin_attach_existing(&mut self, external_id: &str) -> Option<oximux_pty::AttachJob> {
        let ctx = self.attach_ctx();
        let external_id = external_id.to_owned();
        Some(Box::new(move || ctx.attach(&external_id)))
    }

    fn set_output_waker(&mut self, id: TerminalSessionId, waker: oximux_pty::OutputWaker) {
        lock_recover(&self.output_wakers, "output wakers").insert(id, waker);
    }

    fn external_id_of(&self, id: TerminalSessionId) -> Option<String> {
        self.relay_pty_id_of_session(id).or_else(|| {
            lock_recover(&self.inherited_external_ids, "inherited external ids").get(&id).cloned()
        })
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
        self.spawn_prefilled(cfg, &[])
    }

    fn spawn_prefilled(&mut self, cfg: SpawnConfig, prefill: &[u8]) -> Result<TerminalSessionId> {
        // The daemon spawns the child but has no idea what the window looks
        // like — it is a detached process with no theme of its own. So the
        // polarity has to ride the wire in the environment the app sends
        // (see `oximux_pty::polarity`).
        let mut env = cfg.env;
        apply_color_fg_bg(&mut env, background_polarity());
        let resp = self.request(Request::Spawn {
            cwd: cfg.cwd.to_string_lossy().into_owned(),
            cols: cfg.cols,
            rows: cfg.rows,
            shell: Some(cfg.shell),
            args: cfg.args,
            env,
            prefill: prefill.to_vec(),
        })?;
        let (relay_pty_id, attachment_id) = match resp {
            Response::SpawnOk {
                pty_id,
                attachment_id,
            } => (pty_id, attachment_id),
            Response::Err { code, message } => bail!("spawn: {code:?} — {message}"),
            other => bail!("unexpected spawn response: {other:?}"),
        };

        // Subscribe first: spawn auto-attaches, so the child's output is already
        // on its way, and a notification that arrives before the subscription
        // exists is dropped. The channel queues what arrives meanwhile.
        let (sub_id, notif_rx) = self.client.subscribe_pty(&relay_pty_id);
        // Spawn auto-attaches, so this session owns an attachment from the
        // start — address its subscription the same way `attach_relay_pty` does.
        self.client.bind_attachment(&relay_pty_id, sub_id, attachment_id);
        // Restored history goes in before the pump starts draining that queue,
        // so none of the child's output can land ahead of it.
        let mut fresh = TerminalState::new(cfg.cols, cfg.rows, cfg.scrollback);
        if !prefill.is_empty() {
            fresh.prefill(prefill);
        }
        fresh.set_answers_program_status(self.answers_program_status);
        let state = Arc::new(Mutex::new(fresh));
        let id = self.mint_id();
        if cfg.capture_status_events {
            lock_recover(&self.event_queues, "event queues")
                .status
                .entry(id)
                .or_default();
        }
        let generation = Arc::new(AtomicU64::new(1));
        let pump = self.attach_ctx().spawn_pump(
            id,
            relay_pty_id.to_owned(),
            Arc::clone(&state),
            Arc::clone(&generation),
            1,
            attachment_id,
            notif_rx,
        );
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
            state.prefill(bytes);
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
        // Inherited losses are NOT flushed here: each is one view's one-shot
        // signal, and this drain runs on paths that serve no view in
        // particular (an agent cancel, on the backend every view shares) —
        // taking them would leave those views never learning they were lost.
        out
    }

    fn drain_events_for(&mut self, id: TerminalSessionId) -> Vec<TerminalEvent> {
        // Daemon replacement: an inherited dead session yields exactly one
        // `DaemonLost` so its owner (pane tick) learns the process died with
        // the old daemon — not on its own.
        if lock_recover(&self.inherited_dead_sessions, "inherited sessions").remove(&id) {
            return vec![TerminalEvent::DaemonLost { id }];
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
            return vec![TerminalEvent::DaemonLost { id }];
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

    fn sessions_to_carry(&self) -> Vec<(TerminalSessionId, Option<String>)> {
        // Live sessions, plus losses this backend inherited and has not yet
        // reported: replaced again before their views drained, they must
        // still hear about it from the next backend.
        let mut carry: Vec<(TerminalSessionId, Option<String>)> = self
            .live_session_ids()
            .into_iter()
            .map(|id| (id, self.relay_pty_id_of_session(id)))
            .collect();
        let external = lock_recover(&self.inherited_external_ids, "inherited external ids");
        carry.extend(
            lock_recover(&self.inherited_dead_sessions, "inherited sessions")
                .iter()
                .map(|id| (*id, external.get(id).cloned())),
        );
        carry
    }

    fn close(&mut self, id: TerminalSessionId) -> Result<()> {
        let session = match lock_recover(&self.sessions, "sessions").remove(&id) {
            Some(s) => s,
            // Not live here — perhaps a loss this backend inherited, whose
            // view is letting go of it now.
            None => {
                self.forget_inherited(id);
                return Ok(());
            }
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
            // Not live here — perhaps a loss this backend inherited, whose
            // view is letting go of it now.
            None => {
                self.forget_inherited(id);
                return Ok(());
            }
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
        .filter(|event| is_status_event(event))
        .cloned()
        .collect::<Vec<_>>();
    let mut status = VecDeque::new();
    for event in pending {
        push_bounded_status(&mut status, event);
    }
    queues.status.insert(id, status);
}

pub(crate) fn push_event(queues: &SessionEventQueues, id: TerminalSessionId, event: TerminalEvent) {
    let mut queues = lock_recover(queues, "event queues");
    if is_status_event(&event)
        && let Some(status) = queues.status.get_mut(&id)
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

/// Lock a mutex, recovering from poison instead of propagating the
/// panic. Every guarded value in this backend is a plain-data map or
/// per-session terminal grid that stays usable after another thread
/// panicked mid-access (the pump's panic boundary retires the affected
/// session with a synthetic exit), so recovery can never hand out state
/// a caller can't safely drop — whereas propagating would cascade one
/// bad session into whole-backend death.
pub(crate) fn lock_recover<'a, T: ?Sized>(
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
    use oximux_pty::STATUS_EVENT_CAPACITY;

    #[test]
    fn only_a_daemon_older_than_the_feature_leaves_replies_to_the_client() {
        assert!(!daemon_answers_program_status("0.1.42"));
        assert!(daemon_answers_program_status("0.1.43"));
        assert!(daemon_answers_program_status("0.1.44"));
        assert!(daemon_answers_program_status("0.2.0"));
        assert!(daemon_answers_program_status("0.1.43-beta.1"));
        assert!(!daemon_answers_program_status("garbage"));
        assert!(!daemon_answers_program_status(""));
    }

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
                attachment_id: 0,
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
                attachment_id: 0,
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
                attachment_id: 0,
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
                attachment_id: 0,
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
                attachment_id: 0,
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
                attachment_id: 0,
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
    fn attention_notification_emits_an_explicit_notify_and_continues() {
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
            matches!(
                &q[0],
                TerminalEvent::Notify { title: Some(t), body, explicit: true, .. }
                    if t == "Claude" && body == "needs you"
            ),
            "Attention → explicit Notify carrying its title and body"
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
                attachment_id: 0,
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
