//! `AcpConnection` — drives an external agent that speaks the **Agent Client
//! Protocol** (Gemini via `gemini --experimental-acp`) and surfaces decoded
//! [`ThreadEvent`]s on a channel, so an ACP agent lights up the same chat UI as
//! Claude/Codex with no view changes.
//!
//! Shape (a dual-channel actor, like the Codex connection):
//! - a **worker thread** ([`worker`]) runs the `agent-client-protocol` client
//!   under `block_on`: it `Initialize`s, opens a session, then loops turning
//!   queued [`Outbound::Prompt`]s into `session/prompt` requests;
//! - the client's **notification handler** maps each `session/update` into
//!   `ThreadEvent`s (via [`map`]) and pushes them on the same `mpsc` the app
//!   drains; a **request handler** answers `session/request_permission`.
//!
//! Phase 1 maps only the text round-trip and declines every permission; later
//! phases complete the mapping, make approvals interactive, and add slash/mode/
//! usage capabilities. Gemini keeps `emits_usage = false` and
//! `supports_rewind = false` (no `~/.claude` session log to truncate-fork).

mod approvals;
mod map;
mod worker;

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use agent_client_protocol::schema::v1::{CancelNotification, SessionId};
use agent_client_protocol::{Agent, ConnectionTo};
use anyhow::{Result, anyhow};
use futures::channel::mpsc as fmpsc;
use futures::channel::oneshot;

use super::connection::{AgentCapabilities, AgentConnection};
use super::event::ThreadEvent;
use super::tool_call::PermissionDecision;

/// Commands the sync `AgentConnection` methods push to the async worker.
pub(crate) enum Outbound {
    /// Start a turn with this user text (`session/prompt`).
    Prompt(String),
    /// Break the worker loop → the connection drops → the child exits on EOF.
    Shutdown,
}

/// Holds a clone of the live connection + session id so [`AcpConnection::cancel`]
/// can fire `session/cancel` while a prompt future is parked on the worker. (The
/// notification→ThreadEvent mapping is stateless; the `ChatThread` accumulates
/// the streamed chunks itself.)
#[derive(Default)]
pub(crate) struct AcpState {
    /// Set once the session is open; the cancel target.
    pub session_id: Option<SessionId>,
    /// A clone of the live connection (thread-safe: `send_notification` is sync
    /// and the client's driver flushes it), so cancel works off the worker.
    pub connection: Option<ConnectionTo<Agent>>,
    /// Permission requests awaiting the user's decision, keyed by the card's
    /// `request_id`. The async request handler parks on the receiver; the sync
    /// `resolve_permission` sends the decision so the handler can answer the agent.
    pub pending: HashMap<String, oneshot::Sender<PermissionDecision>>,
}

/// A live chat connection to an ACP agent.
pub struct AcpConnection {
    outbound: fmpsc::UnboundedSender<Outbound>,
    state: Arc<Mutex<AcpState>>,
    _worker: JoinHandle<()>,
}

impl AcpConnection {
    /// Spawn `command args…` (e.g. `gemini --experimental-acp`) in `cwd` and start
    /// streaming decoded events. Returns immediately; the ACP handshake runs on
    /// the worker thread and emits [`ThreadEvent::SessionInit`] once the session
    /// opens (or [`ThreadEvent::Error`] if the agent can't be spawned).
    pub fn spawn(command: &str, args: &[String], cwd: &Path) -> Result<(Self, Receiver<ThreadEvent>)> {
        let (event_tx, event_rx) = mpsc::channel::<ThreadEvent>();
        let (out_tx, out_rx) = fmpsc::unbounded::<Outbound>();
        let state = Arc::new(Mutex::new(AcpState::default()));

        let worker = {
            let command = command.to_string();
            let args = args.to_vec();
            let cwd = cwd.to_path_buf();
            let state = state.clone();
            thread::spawn(move || worker::run(command, args, cwd, event_tx, out_rx, state))
        };

        Ok((Self { outbound: out_tx, state, _worker: worker }, event_rx))
    }
}

impl AgentConnection for AcpConnection {
    fn send_user_message(&self, text: &str) -> Result<()> {
        self.outbound
            .unbounded_send(Outbound::Prompt(text.to_string()))
            .map_err(|_| anyhow!("acp worker is gone"))
    }

    fn resolve_permission(&self, request_id: &str, decision: PermissionDecision) -> Result<()> {
        // Wake the parked request handler with the user's decision; it translates
        // to the agent's option and answers. A no-op if already resolved / gone.
        let tx = self
            .state
            .lock()
            .map_err(|_| anyhow!("acp state poisoned"))?
            .pending
            .remove(request_id);
        if let Some(tx) = tx {
            let _ = tx.send(decision);
        }
        Ok(())
    }

    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            supports_modes: false,   // session modes wired in a later phase
            supports_slash: false,   // available-commands mapping is a later phase
            supports_config: false,  // Gemini exposes no reasoning-effort control
            emits_usage: false,      // no per-turn token usage surfaced yet
            supports_rewind: false,  // no on-disk session log; rewind stays Claude-only
        }
    }

    /// Interrupt the in-flight turn (`session/cancel`). Fire-and-forget: the
    /// worker is parked on the prompt future, so we signal the agent directly via
    /// the stashed connection and let the prompt resolve.
    fn cancel(&self) -> Result<()> {
        if let Ok(s) = self.state.lock()
            && let (Some(conn), Some(sid)) = (s.connection.as_ref(), s.session_id.as_ref())
        {
            let _ = conn.send_notification(CancelNotification::new(sid.clone()));
        }
        Ok(())
    }

    fn shutdown(&self) {
        let _ = self.outbound.unbounded_send(Outbound::Shutdown);
    }
}

impl Drop for AcpConnection {
    fn drop(&mut self) {
        // Dropping the sender also ends the worker's prompt loop, but signal
        // explicitly so the child exits promptly.
        let _ = self.outbound.unbounded_send(Outbound::Shutdown);
    }
}
