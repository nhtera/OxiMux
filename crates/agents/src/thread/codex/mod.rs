//! `CodexAppServerConnection` — drives a `codex app-server` subprocess over its
//! native JSON-RPC (v2) and surfaces decoded [`ThreadEvent`]s on a channel, so
//! Codex lights up the same chat UI as Claude with no view changes.
//!
//! Shape (mirrors the Claude connection's ownership, but with a handshake):
//! - a **reader thread** (in [`transport`]) owns stdout, routes responses to the
//!   pending-request map, and forwards notifications / server-requests as [`Inbound`];
//! - a **mapper thread** turns `Inbound` into `ThreadEvent`s (and answers the
//!   Phase-1 approval stub);
//! - a **worker thread** runs the async handshake (`initialize` → `initialized` →
//!   `thread/start`) so `spawn` never blocks the UI, then forwards prompts as
//!   `turn/start`. Interrupts go direct (bypassing the worker queue).
//!
//! Phase 1 maps only the text-round-trip slice (`item/agentMessage/delta`,
//! `turn/started`, `turn/completed`, `error`); Phase 2 completes the mapping and
//! Phase 3 makes approvals/usage/pickers real. Fixed posture: `on-request`
//! approvals + `workspace-write` sandbox (`supports_modes = false`).

mod approvals;
mod map;
pub mod protocol;
pub mod transport;

use std::collections::HashMap;
use std::path::Path;
use std::process::Child;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use approvals::ServerRequestAction;
use super::connection::{AgentCapabilities, AgentConnection, EffortChoice, ModelChoice};
use super::event::{ThreadEvent, TurnUsage};
use super::tool_call::PermissionDecision;
use transport::{Inbound, RpcClient};

/// Handshake round-trips (`initialize`, `thread/start`) block the worker; a
/// generous ceiling so a cold `codex` start (auth/network) still connects.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Commands the sync `AgentConnection` methods push to the worker thread.
enum Outbound {
    Prompt(String),
    Shutdown,
}

/// Session state shared reader(mapper)↔worker↔interrupt: the thread + in-flight
/// turn ids, and the latest token usage (attached to the next `TurnEnded`).
#[derive(Default)]
struct CodexState {
    thread_id: Option<String>,
    current_turn_id: Option<String>,
    last_usage: Option<TurnUsage>,
    /// Tool items (commandExecution / fileChange) by itemId, so a later approval
    /// request — which carries only the itemId — can show the command / changes.
    cmd_items: HashMap<String, Value>,
    /// Pending approvals: our permission-card `request_id` → the Codex JSON-RPC
    /// request id we must answer with a `{decision}` once the user chooses.
    pending_approvals: HashMap<String, Value>,
    /// Codex's model catalog (from `model/list`), cached after the handshake so
    /// the pickers render it.
    models: Vec<protocol::CodexModel>,
    /// The model/effort applied to each `turn/start` (a switch respawns with
    /// these seeded). `None` = Codex's own default.
    current_model: Option<String>,
    current_effort: Option<String>,
    /// Set when Stop was pressed before the turn id was known (the `turn/started`
    /// notification hadn't landed yet). The mapper interrupts as soon as the id
    /// arrives, so a fast Send→Stop doesn't silently no-op.
    cancel_requested: bool,
}

pub struct CodexAppServerConnection {
    outbound: Sender<Outbound>,
    rpc: RpcClient,
    state: Arc<Mutex<CodexState>>,
    child: Arc<Mutex<Child>>,
    _worker: JoinHandle<()>,
    _mapper: JoinHandle<()>,
}

impl CodexAppServerConnection {
    /// Spawn `codex app-server` in `cwd` and start streaming decoded events.
    /// Returns immediately; the handshake runs on the worker thread and emits
    /// [`ThreadEvent::SessionInit`] once `thread/start` resolves.
    pub fn spawn(
        cwd: &Path,
        model: Option<&str>,
        resume_session_id: Option<&str>,
        effort: Option<&str>,
    ) -> Result<(Self, Receiver<ThreadEvent>)> {
        let (rpc, inbound_rx, child) = RpcClient::spawn(cwd)?;
        let (event_tx, event_rx) = mpsc::channel::<ThreadEvent>();
        let (out_tx, out_rx) = mpsc::channel::<Outbound>();
        let state = Arc::new(Mutex::new(CodexState::default()));
        // Seed the current model/effort so the first turn — and the picker
        // labels — reflect the tab's choice.
        if let Ok(mut s) = state.lock() {
            s.current_model = model.map(str::to_string);
            s.current_effort = effort.map(str::to_string);
        }

        let mapper = {
            let event_tx = event_tx.clone();
            let state = state.clone();
            let rpc = rpc.clone();
            thread::spawn(move || map_inbound(inbound_rx, event_tx, state, rpc))
        };
        let worker = {
            let rpc = rpc.clone();
            let state = state.clone();
            let cwd = cwd.to_path_buf();
            let model = model.map(str::to_string);
            let resume = resume_session_id.map(str::to_string);
            thread::spawn(move || worker_loop(rpc, event_tx, state, out_rx, cwd, model, resume))
        };

        Ok((
            Self {
                outbound: out_tx,
                rpc,
                state,
                child: Arc::new(Mutex::new(child)),
                _worker: worker,
                _mapper: mapper,
            },
            event_rx,
        ))
    }

    /// Kill + reap the codex process group — the direct child AND its sandbox
    /// helper processes (spawned in the same group, see `spawn_command`).
    fn reap(&self) {
        let mut child = match self.child.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        #[cfg(unix)]
        {
            // SAFETY: `pgid` is our own child's process group (it leads the group
            // via `process_group(0)`); `kill(2)` with a negative pid signals the
            // whole group. Best-effort — a gone group returns ESRCH, ignored.
            let pgid = child.id() as libc::pid_t;
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl AgentConnection for CodexAppServerConnection {
    fn send_user_message(&self, text: &str) -> Result<()> {
        self.outbound
            .send(Outbound::Prompt(text.to_string()))
            .map_err(|_| anyhow!("codex worker is gone"))
    }

    fn resolve_permission(&self, request_id: &str, decision: PermissionDecision) -> Result<()> {
        // Look up the stashed Codex JSON-RPC request id and answer it directly
        // (the reader never blocked on the decision). A no-op if already answered.
        let id = self
            .state
            .lock()
            .map_err(|_| anyhow!("codex state poisoned"))?
            .pending_approvals
            .remove(request_id);
        match id {
            Some(id) => self
                .rpc
                .respond(id, json!({ "decision": approvals::to_codex_decision(&decision) })),
            None => Ok(()),
        }
    }

    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            supports_modes: false,  // fixed on-request posture in P1 (no mode picker)
            supports_slash: false,  // no slash-command palette wired for Codex yet
            supports_config: true,  // reasoning-effort picker (from model/list)
            emits_usage: true,      // thread/tokenUsage/updated → the usage footer
            supports_rewind: false, // no ~/.claude JSONL; rewind stays Claude-only
        }
    }

    fn models(&self) -> Vec<ModelChoice> {
        self.state
            .lock()
            .map(|s| {
                s.models
                    .iter()
                    .map(|m| ModelChoice {
                        wire: m.wire.clone(),
                        label: m.display.clone(),
                        description: m.description.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn efforts(&self) -> Vec<EffortChoice> {
        self.state
            .lock()
            .map(|s| {
                current_or_default_model(&s)
                    .map(|m| {
                        m.efforts
                            .iter()
                            .map(|e| EffortChoice { wire: e.clone(), label: title_case(e) })
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    fn default_model(&self) -> Option<String> {
        let s = self.state.lock().ok()?;
        s.current_model
            .clone()
            .or_else(|| current_or_default_model(&s).map(|m| m.wire.clone()))
    }

    fn default_effort(&self) -> Option<String> {
        let s = self.state.lock().ok()?;
        s.current_effort
            .clone()
            .or_else(|| current_or_default_model(&s).map(|m| m.default_effort.clone()))
    }

    /// Interrupt the in-flight turn (`turn/interrupt`). Fire-and-forget so the
    /// Stop button never blocks; no-op when nothing is in flight.
    fn cancel(&self) -> Result<()> {
        let (tid, turn) = match self.state.lock() {
            Ok(mut s) => {
                // Turn id not known yet (turn/started still in flight) → remember
                // the request so the mapper interrupts the moment it arrives.
                if s.current_turn_id.is_none() {
                    s.cancel_requested = true;
                }
                (s.thread_id.clone(), s.current_turn_id.clone())
            }
            Err(_) => return Ok(()),
        };
        match (tid, turn) {
            (Some(tid), Some(turn)) => self
                .rpc
                .fire(protocol::M_TURN_INTERRUPT, protocol::turn_interrupt_params(&tid, &turn)),
            _ => Ok(()),
        }
    }

    fn shutdown(&self) {
        let _ = self.outbound.send(Outbound::Shutdown);
        self.reap();
    }
}

impl Drop for CodexAppServerConnection {
    fn drop(&mut self) {
        // Guard against orphaned `codex` + sandbox helpers if the connection is
        // dropped without an explicit shutdown().
        let _ = self.outbound.send(Outbound::Shutdown);
        self.reap();
    }
}

/// The current model (if the picker chose one), else the catalog default, else
/// the first entry.
fn current_or_default_model(st: &CodexState) -> Option<&protocol::CodexModel> {
    let cur = st.current_model.as_deref();
    st.models
        .iter()
        .find(|m| Some(m.wire.as_str()) == cur)
        .or_else(|| st.models.iter().find(|m| m.is_default))
        .or_else(|| st.models.first())
}

/// Capitalize the first letter for an effort label ("low" → "Low").
fn title_case(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// The worker: async handshake (`initialize` → cache `model/list` → `initialized`
/// → `thread/resume` or `thread/start`), then forward prompts as `turn/start`.
fn worker_loop(
    rpc: RpcClient,
    event_tx: Sender<ThreadEvent>,
    state: Arc<Mutex<CodexState>>,
    out_rx: Receiver<Outbound>,
    cwd: std::path::PathBuf,
    model: Option<String>,
    resume: Option<String>,
) {
    if let Err(e) = rpc.request(protocol::M_INITIALIZE, protocol::initialize_params(), HANDSHAKE_TIMEOUT) {
        let _ = event_tx.send(ThreadEvent::Error(format!("codex initialize failed: {e}")));
        return;
    }
    // Fetch the model catalog BEFORE emitting SessionInit, so the composer's
    // controls seed (which runs on that event) already has the picker vocab.
    if let Ok(res) = rpc.request(protocol::M_MODEL_LIST, json!({}), HANDSHAKE_TIMEOUT) {
        let models = protocol::parse_model_list(&res);
        if let Ok(mut s) = state.lock() {
            s.models = models;
        }
    }
    let _ = rpc.notify(protocol::N_INITIALIZED, Value::Null);

    // Resume the persisted thread when restoring; else start fresh. A failed
    // resume (thread gone / experimental) degrades to a fresh start.
    let resume_id = resume.as_deref().filter(|s| !s.is_empty());
    let started = match resume_id {
        Some(tid) => rpc.request(
            protocol::M_THREAD_RESUME,
            protocol::thread_resume_params(tid, model.as_deref()),
            HANDSHAKE_TIMEOUT,
        ),
        None => rpc.request(
            protocol::M_THREAD_START,
            protocol::thread_start_params(model.as_deref(), &cwd),
            HANDSHAKE_TIMEOUT,
        ),
    };
    let res = match started {
        Ok(r) => Some(r),
        Err(e) if resume_id.is_some() => {
            tracing::warn!(error = %e, "codex thread/resume failed; starting a fresh thread");
            rpc.request(
                protocol::M_THREAD_START,
                protocol::thread_start_params(model.as_deref(), &cwd),
                HANDSHAKE_TIMEOUT,
            )
            .ok()
        }
        Err(e) => {
            let _ = event_tx.send(ThreadEvent::Error(format!("codex thread/start failed: {e}")));
            return;
        }
    };
    let Some(res) = res else {
        let _ = event_tx.send(ThreadEvent::Error("codex thread/start failed after resume".into()));
        return;
    };
    // A handshake with no thread id is a hard failure — every later turn/cancel
    // would target an empty id. Surface it rather than pretending to connect.
    let Some(tid) = protocol::thread_id_from_start_response(&res).filter(|t| !t.is_empty()) else {
        let _ = event_tx.send(ThreadEvent::Error("codex handshake returned no thread id".into()));
        return;
    };
    let resolved_model = protocol::model_from_start_response(&res).or(model.clone()).unwrap_or_default();
    if let Ok(mut s) = state.lock() {
        s.thread_id = Some(tid.clone());
    }
    let _ = event_tx.send(ThreadEvent::SessionInit {
        session_id: tid,
        model: resolved_model,
        permission_mode: String::new(),
        slash_commands: Vec::new(),
    });

    // Poll for commands, but wake periodically to notice a subprocess crash:
    // if the child died, break so `event_tx` drops and the app's disconnect
    // handler fires (a plain blocking `recv` would park here forever on a crash).
    loop {
        match out_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Outbound::Prompt(text)) => {
                let (tid, model, effort) = match state.lock() {
                    Ok(s) => (
                        s.thread_id.clone().unwrap_or_default(),
                        s.current_model.clone(),
                        s.current_effort.clone(),
                    ),
                    Err(_) => (String::new(), None, None),
                };
                // Fire-and-forget: the turn's text + turnId arrive as notifications;
                // we don't block the worker on the turn's lifetime.
                let params = protocol::turn_start_params(&tid, &text, model.as_deref(), effort.as_deref());
                if let Err(e) = rpc.fire(protocol::M_TURN_START, params) {
                    let _ = event_tx.send(ThreadEvent::Error(format!("codex turn/start failed: {e}")));
                }
            }
            Ok(Outbound::Shutdown) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !rpc.is_alive() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// The mapper: `Inbound` → `ThreadEvent` via [`map::map_notification`], plus the
/// Phase-1 approval auto-decline (Phase 3 makes approvals interactive).
fn map_inbound(
    rx: Receiver<Inbound>,
    event_tx: Sender<ThreadEvent>,
    state: Arc<Mutex<CodexState>>,
    rpc: RpcClient,
) {
    for inbound in rx {
        match inbound {
            Inbound::Notification { method, params } => {
                let (events, deferred_interrupt) = {
                    let mut st = match state.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    let events = map::map_notification(&method, &params, &mut st);
                    // Honor a Stop pressed before the turn id was known, now that
                    // this notification (turn/started) may have set it.
                    let deferred = if st.cancel_requested {
                        match (st.thread_id.clone(), st.current_turn_id.clone()) {
                            (Some(tid), Some(turn)) => {
                                st.cancel_requested = false;
                                Some((tid, turn))
                            }
                            _ => None,
                        }
                    } else {
                        None
                    };
                    (events, deferred)
                };
                if let Some((tid, turn)) = deferred_interrupt {
                    let _ = rpc.fire(protocol::M_TURN_INTERRUPT, protocol::turn_interrupt_params(&tid, &turn));
                }
                for ev in events {
                    if event_tx.send(ev).is_err() {
                        return; // consumer gone
                    }
                }
            }
            Inbound::ServerRequest { id, method, params } => {
                let action = {
                    let mut st = match state.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    approvals::map_server_request(&id, &method, &params, &mut st)
                };
                match action {
                    // A permission card was emitted; the reply follows the user's
                    // decision (via resolve_permission) — don't answer here.
                    ServerRequestAction::Emit(events) => {
                        for ev in events {
                            if event_tx.send(ev).is_err() {
                                return;
                            }
                        }
                    }
                    ServerRequestAction::AutoRespond(result) => {
                        let _ = rpc.respond(id, result);
                    }
                }
            }
        }
    }
}
