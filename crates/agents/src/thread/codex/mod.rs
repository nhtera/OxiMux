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

mod map;
pub mod protocol;
pub mod transport;

use std::path::Path;
use std::process::Child;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use super::connection::{AgentConnection, AgentCapabilities};
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
    pub fn spawn(cwd: &Path, model: Option<&str>) -> Result<(Self, Receiver<ThreadEvent>)> {
        let (rpc, inbound_rx, child) = RpcClient::spawn(cwd)?;
        let (event_tx, event_rx) = mpsc::channel::<ThreadEvent>();
        let (out_tx, out_rx) = mpsc::channel::<Outbound>();
        let state = Arc::new(Mutex::new(CodexState::default()));

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
            thread::spawn(move || worker_loop(rpc, event_tx, state, out_rx, cwd, model))
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

    /// SIGTERM/kill + reap the child (and its sandbox helpers).
    fn reap(&self) {
        let mut child = match self.child.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
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

    fn resolve_permission(&self, _request_id: &str, _decision: PermissionDecision) -> Result<()> {
        // Phase 1 auto-declines approvals in the mapper, so no interactive
        // permission card is shown for Codex yet; a later phase wires this.
        anyhow::bail!("codex interactive approvals arrive in a later phase")
    }

    fn capabilities(&self) -> AgentCapabilities {
        // Phase 1: fixed on-request posture, no modes, no usage/vocab yet (a
        // later phase flips emits_usage + fills the model/effort vocab). No
        // ~/.claude JSONL → no rewind.
        AgentCapabilities::default()
    }

    /// Interrupt the in-flight turn (`turn/interrupt`). Fire-and-forget so the
    /// Stop button never blocks; no-op when nothing is in flight.
    fn cancel(&self) -> Result<()> {
        let (tid, turn) = match self.state.lock() {
            Ok(s) => (s.thread_id.clone(), s.current_turn_id.clone()),
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

/// The worker: async handshake, then forward prompts as `turn/start`.
fn worker_loop(
    rpc: RpcClient,
    event_tx: Sender<ThreadEvent>,
    state: Arc<Mutex<CodexState>>,
    out_rx: Receiver<Outbound>,
    cwd: std::path::PathBuf,
    model: Option<String>,
) {
    if let Err(e) = rpc.request(protocol::M_INITIALIZE, protocol::initialize_params(), HANDSHAKE_TIMEOUT) {
        let _ = event_tx.send(ThreadEvent::Error(format!("codex initialize failed: {e}")));
        return;
    }
    let _ = rpc.notify(protocol::N_INITIALIZED, Value::Null);
    match rpc.request(
        protocol::M_THREAD_START,
        protocol::thread_start_params(model.as_deref(), &cwd),
        HANDSHAKE_TIMEOUT,
    ) {
        Ok(res) => {
            let tid = protocol::thread_id_from_start_response(&res).unwrap_or_default();
            let resolved_model = protocol::model_from_start_response(&res)
                .or(model.clone())
                .unwrap_or_default();
            if let Ok(mut s) = state.lock() {
                s.thread_id = Some(tid.clone());
            }
            let _ = event_tx.send(ThreadEvent::SessionInit {
                session_id: tid,
                model: resolved_model,
                permission_mode: String::new(),
                slash_commands: Vec::new(),
            });
        }
        Err(e) => {
            let _ = event_tx.send(ThreadEvent::Error(format!("codex thread/start failed: {e}")));
            return;
        }
    }

    for cmd in out_rx {
        match cmd {
            Outbound::Prompt(text) => {
                let tid = state
                    .lock()
                    .ok()
                    .and_then(|s| s.thread_id.clone())
                    .unwrap_or_default();
                // Fire-and-forget: the turn's text + turnId arrive as notifications;
                // we don't block the worker on the turn's lifetime.
                if let Err(e) = rpc.fire(protocol::M_TURN_START, protocol::turn_start_params(&tid, &text)) {
                    let _ = event_tx.send(ThreadEvent::Error(format!("codex turn/start failed: {e}")));
                }
            }
            Outbound::Shutdown => break,
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
                let events = {
                    let mut st = match state.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    map::map_notification(&method, &params, &mut st)
                };
                for ev in events {
                    if event_tx.send(ev).is_err() {
                        return; // consumer gone
                    }
                }
            }
            Inbound::ServerRequest { id, .. } => {
                // Decline every approval so a turn that requests one doesn't
                // stall waiting on us. Phase 3 renders a real approval card.
                let _ = rpc.respond(id, json!({ "decision": "denied" }));
            }
        }
    }
}
