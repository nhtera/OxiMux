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
//! Capabilities are **discovered** from the ACP handshake, not hardcoded: the
//! worker stashes `InitializeResponse` + `NewSessionResponse.modes`/`config_options`
//! into [`AcpState`], and [`AcpConnection::capabilities`] + the mode/config
//! accessors read that. An agent that advertises modes lights up the mode picker;
//! one that advertises nothing keeps every flag `false`. `supports_rewind` stays
//! `false` (no on-disk session log to truncate-fork).

mod approvals;
mod client_fs;
mod map;
mod worker;

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use agent_client_protocol::schema::v1::{
    CancelNotification, SessionConfigOption, SessionId, SessionModeState,
};
use agent_client_protocol::{Agent, ConnectionTo};
use anyhow::{Result, anyhow};
use futures::channel::mpsc as fmpsc;
use futures::channel::oneshot;
use serde_json::Value;

use super::connection::{AgentCapabilities, AgentConnection, ModeChoice};
use super::event::{ThreadEvent, TurnUsage};
use super::tool_call::PermissionDecision;

/// Commands the sync `AgentConnection` methods push to the async worker.
pub(crate) enum Outbound {
    /// Start a turn with this user text (`session/prompt`).
    Prompt(String),
    /// Switch the session's permission/edit mode at runtime (`session/set_mode`).
    /// ACP switches modes in-session, so this never respawns the child.
    SetMode(String),
    /// Set a session config option (`session/set_config_option`). `value` is a
    /// select value-id (string) or a boolean toggle.
    SetConfig { id: String, value: Value },
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
    /// Mode state advertised by the agent (`NewSessionResponse.modes`), stashed by
    /// the worker before `SessionInit`. Drives `permission_modes()`/`default_mode()`.
    /// `None` when the agent doesn't offer modes.
    pub modes: Option<SessionModeState>,
    /// Config options advertised by the agent (`NewSessionResponse.config_options`).
    /// Empty when the agent offers none.
    pub config_options: Vec<SessionConfigOption>,
    /// Capabilities resolved from the handshake; `capabilities()` reads this so the
    /// UI gates its affordances on what the live agent actually advertised.
    pub caps: AgentCapabilities,
    /// Latest `UsageUpdate` mapped to our usage shape, folded into the next
    /// `TurnEnded.usage` at turn end (ACP delivers usage out-of-band, not per-turn).
    pub last_usage: Option<TurnUsage>,
}

/// Derive `AgentCapabilities` from what the agent advertised at the ACP
/// handshake. `supports_modes`/`supports_config` come straight from the presence
/// of `modes`/`config_options`; `supports_slash`/`emits_usage` reflect that the
/// ACP adapter wires `AvailableCommands`/`Usage` session updates (empty until one
/// arrives, which harmlessly disables the affordance). `supports_rewind` is always
/// `false` — ACP has no on-disk session log to truncate-fork.
fn caps_from_handshake(
    modes: Option<&SessionModeState>,
    config_options: &[SessionConfigOption],
) -> AgentCapabilities {
    AgentCapabilities {
        supports_modes: modes.is_some(),
        supports_slash: true,
        supports_config: !config_options.is_empty(),
        emits_usage: true,
        supports_rewind: false,
    }
}

/// Map an ACP `SessionModeState` into the picker's `ModeChoice` vocabulary
/// (`SessionMode{id,name}` → `{wire,label}`).
fn modes_to_choices(modes: &SessionModeState) -> Vec<ModeChoice> {
    modes
        .available_modes
        .iter()
        .map(|mode| ModeChoice { wire: mode.id.0.to_string(), label: mode.name.clone() })
        .collect()
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
        // Read what the worker resolved from the ACP handshake. Before the
        // handshake completes (or if the lock is poisoned) fall back to all-false,
        // which is byte-identical to the pre-discovery behaviour.
        self.state.lock().map(|s| s.caps).unwrap_or_default()
    }

    fn permission_modes(&self) -> Vec<ModeChoice> {
        self.state
            .lock()
            .ok()
            .and_then(|s| s.modes.clone())
            .map(|m| modes_to_choices(&m))
            .unwrap_or_default()
    }

    fn default_mode(&self) -> Option<String> {
        self.state
            .lock()
            .ok()
            .and_then(|s| s.modes.as_ref().map(|m| m.current_mode_id.0.to_string()))
    }

    /// Switch the permission/edit mode in-session (`session/set_mode`). Routed
    /// through the worker (which owns `cx`) so the request is driven on the
    /// executor thread; queued behind an in-flight prompt if one is streaming.
    /// Returning `Ok` here is what tells the app layer to skip the Claude-style
    /// respawn (ACP switches modes live).
    fn set_mode(&self, mode: &str) -> Result<()> {
        self.outbound
            .unbounded_send(Outbound::SetMode(mode.to_string()))
            .map_err(|_| anyhow!("acp worker is gone"))
    }

    /// Set a session config option (`session/set_config_option`). Only meaningful
    /// when the agent advertised `config_options`; harmlessly ignored otherwise.
    fn set_config(&self, key: &str, value: Value) -> Result<()> {
        self.outbound
            .unbounded_send(Outbound::SetConfig { id: key.to_string(), value })
            .map_err(|_| anyhow!("acp worker is gone"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{SessionConfigOption, SessionMode, SessionModeState};

    fn mode_state() -> SessionModeState {
        SessionModeState::new(
            "default",
            vec![
                SessionMode::new("default", "Ask every time"),
                SessionMode::new("acceptEdits", "Accept edits"),
                SessionMode::new("bypassPermissions", "Bypass"),
            ],
        )
    }

    #[test]
    fn caps_derive_modes_and_config_from_handshake() {
        // An agent that advertises modes + config lights up those flags; slash +
        // usage are always on for ACP (empty until an update arrives); rewind off.
        let modes = mode_state();
        let config = vec![SessionConfigOption::boolean("reasoning", "Reasoning", true)];
        let caps = caps_from_handshake(Some(&modes), &config);
        assert!(caps.supports_modes);
        assert!(caps.supports_config);
        assert!(caps.supports_slash);
        assert!(caps.emits_usage);
        assert!(!caps.supports_rewind);
    }

    #[test]
    fn caps_all_false_when_agent_advertises_nothing_except_protocol_carriers() {
        // No modes, no config → those two flags stay false (no picker/config
        // control). Slash/usage remain on (protocol can carry them) but render
        // nothing until an update arrives, so the surface is unchanged.
        let caps = caps_from_handshake(None, &[]);
        assert!(!caps.supports_modes);
        assert!(!caps.supports_config);
        assert!(!caps.supports_rewind);
    }

    #[test]
    fn modes_map_to_picker_choices_in_order() {
        let choices = modes_to_choices(&mode_state());
        assert_eq!(choices.len(), 3);
        assert_eq!(choices[0], ModeChoice { wire: "default".into(), label: "Ask every time".into() });
        assert_eq!(choices[1], ModeChoice { wire: "acceptEdits".into(), label: "Accept edits".into() });
        assert_eq!(choices[2].wire, "bypassPermissions");
    }
}
