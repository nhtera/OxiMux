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
mod auth;
mod client_fs;
mod client_terminal;
mod map;
mod worker;

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};

use agent_client_protocol::schema::v1::{
    CancelNotification, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelect, SessionConfigSelectOption, SessionConfigSelectOptions, SessionId,
    SessionModeState,
};
use agent_client_protocol::{Agent, ConnectionTo};
use anyhow::{Result, anyhow};
use futures::channel::mpsc as fmpsc;
use futures::channel::oneshot;
use serde_json::Value;

use super::connection::{
    AgentCapabilities, AgentConnection, EffortChoice, FeatureControl, FeatureKind,
    FeatureSelectOption, FeatureValue, ModeChoice, ModelChoice,
};
use super::entry::ChatImage;
use super::event::{ThreadEvent, TurnUsage};
use super::tool_call::PermissionDecision;

pub use client_terminal::{
    AcpTerminalHost, TerminalExitLite, TerminalOutputLite, TerminalSpawnSpec,
};

/// Process-wide backing for ACP embedded terminals, installed once by the app at
/// boot (mirrors the app's `SHARED_BACKEND` terminal global). When unset the ACP
/// client advertises `terminal:false` and the `terminal/*` handlers reject, so a
/// headless/test build has no terminal surface.
static TERMINAL_HOST: OnceLock<Arc<dyn AcpTerminalHost>> = OnceLock::new();

/// Install the app's embedded-terminal host so ACP agents can drive live inline
/// terminals. Call once at app boot, before any ACP chat connects; a later call
/// is ignored (the first host wins), matching the `OnceLock` install idiom.
pub fn install_terminal_host(host: Arc<dyn AcpTerminalHost>) {
    let _ = TERMINAL_HOST.set(host);
}

/// The installed embedded-terminal host, if any. Read by the worker to gate the
/// `terminal` capability at the handshake and to serve the `terminal/*` methods.
pub(crate) fn terminal_host() -> Option<Arc<dyn AcpTerminalHost>> {
    TERMINAL_HOST.get().cloned()
}

/// Commands the sync `AgentConnection` methods push to the async worker.
pub(crate) enum Outbound {
    /// Start a turn with this user text plus any attached images (`session/prompt`).
    /// Images are dropped by the worker when the agent didn't advertise
    /// `prompt_capabilities.image`, so a text-only agent sees an unchanged request.
    Prompt { text: String, images: Vec<ChatImage> },
    /// Switch the session's permission/edit mode at runtime (`session/set_mode`).
    /// ACP switches modes in-session, so this never respawns the child.
    SetMode(String),
    /// Set a session config option (`session/set_config_option`). `value` is a
    /// select value-id (string) or a boolean toggle.
    SetConfig { id: String, value: Value },
    /// Authenticate with the method the user picked from the auth card, then retry
    /// opening the session on the same connection (`authenticate`).
    Authenticate(String),
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
    /// Auth methods the agent advertised at `initialize` (mirrors how modes/config
    /// are stashed). Read by the worker's auth loop to map a picked method id to
    /// its kind (e.g. a terminal method's login args). Empty when the agent needs
    /// no login.
    pub auth_methods: Vec<super::event::AuthMethodInfo>,
    /// Set while a `session/load` is replaying the agent's history: per spec the
    /// agent re-sends its full transcript as ordinary `session/update`
    /// notifications BEFORE the load response resolves. OxiMux already repaints
    /// its own persisted blob, so the notification handler drops those
    /// transcript-bearing replays while this is set (control updates still pass).
    pub replaying: bool,
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

/// Drain every parked permission responder out of the state, returning the
/// senders. Dropping them resolves each handler's `orx.await` to `Err`, which the
/// permission handler answers as `Cancelled` — the mechanism [`AcpConnection::cancel`]
/// uses to unwedge an agent's outstanding `session/request_permission` on Stop.
fn drain_pending(state: &Mutex<AcpState>) -> Vec<oneshot::Sender<PermissionDecision>> {
    state.lock().map(|mut s| s.pending.drain().map(|(_, tx)| tx).collect()).unwrap_or_default()
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

/// The agent's `Select` config option tagged with `category`, if it advertised
/// one. ACP has no first-class model/effort concept; by convention it exposes
/// each as a `Select` config option tagged with a semantic category
/// (`SessionConfigOptionCategory`). Returns the option (for its id, the
/// `set_config` key) + its select payload (for the choices + current value).
fn select_for(
    options: &[SessionConfigOption],
    category: SessionConfigOptionCategory,
) -> Option<(&SessionConfigOption, &SessionConfigSelect)> {
    options.iter().find_map(|opt| {
        if opt.category.as_ref() != Some(&category) {
            return None;
        }
        match &opt.kind {
            SessionConfigKind::Select(select) => Some((opt, select)),
            // Boolean (and any future non-select kind) can't be a picker list.
            _ => None,
        }
    })
}

/// The model selector (a `Model`-category select).
fn model_select(options: &[SessionConfigOption]) -> Option<(&SessionConfigOption, &SessionConfigSelect)> {
    select_for(options, SessionConfigOptionCategory::Model)
}

/// The reasoning-effort selector (a `ThoughtLevel`-category select). ACP maps the
/// effort picker to this; empty for an agent that advertises no reasoning levels.
fn thought_level_select(
    options: &[SessionConfigOption],
) -> Option<(&SessionConfigOption, &SessionConfigSelect)> {
    select_for(options, SessionConfigOptionCategory::ThoughtLevel)
}

/// The effort-picker vocabulary from the agent's `ThoughtLevel` select, in
/// advertised order (`value` → `wire`, `name` → `label`). Empty when the agent
/// advertises no reasoning-level selector (the picker then stays hidden).
fn effort_choices(options: &[SessionConfigOption]) -> Vec<EffortChoice> {
    match thought_level_select(options) {
        Some((_opt, select)) => flatten_select(&select.options)
            .into_iter()
            .map(|opt| EffortChoice { wire: opt.value.0.to_string(), label: opt.name.clone() })
            .collect(),
        None => Vec::new(),
    }
}

/// Flatten a select's options — grouped or ungrouped — into `&SessionConfigSelectOption`
/// in advertised order (the order the agent listed them).
fn flatten_select(options: &SessionConfigSelectOptions) -> Vec<&SessionConfigSelectOption> {
    match options {
        SessionConfigSelectOptions::Ungrouped(opts) => opts.iter().collect(),
        SessionConfigSelectOptions::Grouped(groups) => {
            groups.iter().flat_map(|g| g.options.iter()).collect()
        }
        // A future option-list shape we don't understand yields no choices.
        _ => Vec::new(),
    }
}

/// The model picker vocabulary from the agent's `Model` select option, in
/// advertised order. `wire` is the value id the agent expects back via
/// `set_config`; `label` is the option's human-readable `name` (what the picker
/// shows). Empty when the agent advertises no model selector (the picker then
/// stays hidden — honest).
fn model_choices(options: &[SessionConfigOption]) -> Vec<ModelChoice> {
    match model_select(options) {
        Some((_opt, select)) => flatten_select(&select.options)
            .into_iter()
            .map(|opt| ModelChoice {
                wire: opt.value.0.to_string(),
                label: opt.name.clone(),
                description: opt.description.clone(),
            })
            .collect(),
        None => Vec::new(),
    }
}

/// A semantic icon hint for a config-option surfaced as a generic feature — a
/// keyword match on its id+name (kept UI-asset-free; the view maps the hint to a
/// glyph). Plan / fast / auto-accept / agent-profile get distinct hints; the
/// rest fall back to a neutral settings glyph.
fn feature_icon_hint(id: &str, name: &str) -> &'static str {
    let h = format!("{id} {name}").to_lowercase();
    if h.contains("plan") {
        "plan"
    } else if h.contains("fast") || h.contains("turbo") {
        "zap"
    } else if h.contains("accept") || h.contains("auto") || h.contains("yolo") {
        "check"
    } else if h.contains("agent") || h.contains("profile") {
        "bot"
    } else {
        "settings"
    }
}

/// Derive the generic feature-control cluster from the agent's config options.
/// Every option that is NOT the dedicated model/effort selector becomes a
/// [`FeatureControl`]: a `Boolean` option → a toggle; any other `Select` (a
/// `Mode`-category profile, a `ModelConfig`, or a custom `_`-category option) → a
/// labeled select. `Model` and `ThoughtLevel` keep their dedicated pickers and
/// are skipped here so they aren't double-surfaced. The option `id` is the
/// `set_config` key echoed back on change.
fn feature_controls(options: &[SessionConfigOption]) -> Vec<FeatureControl> {
    options
        .iter()
        .filter(|opt| {
            !matches!(
                opt.category,
                Some(SessionConfigOptionCategory::Model)
                    | Some(SessionConfigOptionCategory::ThoughtLevel)
            )
        })
        .filter_map(|opt| {
            let kind = match &opt.kind {
                SessionConfigKind::Select(select) => FeatureKind::Select {
                    options: flatten_select(&select.options)
                        .into_iter()
                        .map(|o| FeatureSelectOption {
                            wire: o.value.0.to_string(),
                            label: o.name.clone(),
                            description: o.description.clone(),
                        })
                        .collect(),
                    selected: Some(select.current_value.0.to_string()),
                },
                SessionConfigKind::Boolean(b) => FeatureKind::Toggle { on: b.current_value },
                // A future option kind we don't understand is silently skipped.
                _ => return None,
            };
            let id = opt.id.0.to_string();
            let label = opt.name.clone();
            let icon = Some(feature_icon_hint(&id, &label).to_string());
            Some(FeatureControl { id, label, description: opt.description.clone(), icon, kind })
        })
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
    ///
    /// `resume_session_id` continues a persisted session: when the agent
    /// advertises `loadSession` the worker sends `session/load` with that id
    /// instead of `session/new`; when it doesn't (or the load fails), the worker
    /// falls back to a fresh session and emits a one-line notice. `None` always
    /// opens a fresh session.
    pub fn spawn(
        command: &str,
        args: &[String],
        cwd: &Path,
        resume_session_id: Option<String>,
    ) -> Result<(Self, Receiver<ThreadEvent>)> {
        Self::spawn_with_env(command, args, cwd, resume_session_id, Vec::new(), None)
    }

    /// Like [`Self::spawn`], but seeds the subprocess with extra `env` overrides
    /// and (optionally) an `auth_method` to `authenticate` once if the freshly
    /// spawned agent still reports `AuthRequired`. This backs the EnvVar-auth
    /// respawn: the user's typed credentials ride in `env` (delivered to the child
    /// as real environment, never argv), and `auth_method` closes the "set env,
    /// then authenticate" loop without re-prompting. `spawn` is this with no
    /// extra env and no auto-authenticate, so every existing caller is unchanged.
    pub fn spawn_with_env(
        command: &str,
        args: &[String],
        cwd: &Path,
        resume_session_id: Option<String>,
        env: Vec<(String, String)>,
        auth_method: Option<String>,
    ) -> Result<(Self, Receiver<ThreadEvent>)> {
        let (event_tx, event_rx) = mpsc::channel::<ThreadEvent>();
        let (out_tx, out_rx) = fmpsc::unbounded::<Outbound>();
        let state = Arc::new(Mutex::new(AcpState::default()));

        let worker = {
            let command = command.to_string();
            let args = args.to_vec();
            let cwd = cwd.to_path_buf();
            let state = state.clone();
            thread::spawn(move || {
                worker::run(
                    command,
                    args,
                    cwd,
                    resume_session_id,
                    env,
                    auth_method,
                    event_tx,
                    out_rx,
                    state,
                )
            })
        };

        Ok((Self { outbound: out_tx, state, _worker: worker }, event_rx))
    }
}

impl AgentConnection for AcpConnection {
    fn send_user_message(&self, text: &str) -> Result<()> {
        self.outbound
            .unbounded_send(Outbound::Prompt { text: text.to_string(), images: Vec::new() })
            .map_err(|_| anyhow!("acp worker is gone"))
    }

    /// Send a prompt carrying attached images. The worker maps each [`ChatImage`]
    /// to a base64 `ContentBlock::Image` alongside the text block — gated on the
    /// agent's `prompt_capabilities.image` so a text-only agent falls back to
    /// text (mirroring the trait default) rather than erroring the turn.
    fn send_user_message_with_images(&self, text: &str, images: &[ChatImage]) -> Result<()> {
        self.outbound
            .unbounded_send(Outbound::Prompt { text: text.to_string(), images: images.to_vec() })
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

    /// Authenticate with the picked method from the auth card. Routed to the
    /// worker (which owns `cx` and the parked handshake), which runs
    /// `authenticate` and retries the session open on the same connection.
    fn authenticate(&self, method_id: &str) -> Result<()> {
        self.outbound
            .unbounded_send(Outbound::Authenticate(method_id.to_string()))
            .map_err(|_| anyhow!("acp worker is gone"))
    }

    /// The model choices the agent advertised via its `Model`-category select
    /// config option. Empty until the handshake stashes `config_options` (and
    /// empty forever for an agent that offers no model selector) — the picker
    /// then stays hidden, which is honest for ACP.
    fn models(&self) -> Vec<ModelChoice> {
        self.state.lock().map(|s| model_choices(&s.config_options)).unwrap_or_default()
    }

    /// The agent's currently-selected model (the `Model` select's `current_value`),
    /// shown as current when the user hasn't picked one this session.
    fn default_model(&self) -> Option<String> {
        self.state
            .lock()
            .ok()
            .and_then(|s| model_select(&s.config_options).map(|(_o, sel)| sel.current_value.0.to_string()))
    }

    /// Switch the model in-session by writing the picked value to the agent's
    /// `Model` select config option (`session/set_config_option`). Returning `Ok`
    /// tells the app to skip the Claude-style respawn (which would drop the live
    /// ACP session). Bails when the agent exposes no model selector — but then
    /// `models()` is empty and the picker is hidden, so this is unreachable in
    /// practice.
    fn set_model(&self, model: &str) -> Result<()> {
        let option_id = self
            .state
            .lock()
            .ok()
            .and_then(|s| model_select(&s.config_options).map(|(opt, _sel)| opt.id.0.to_string()));
        match option_id {
            Some(id) => self.set_config(&id, Value::String(model.to_string())),
            None => Err(anyhow!("this ACP agent does not expose a model selector")),
        }
    }

    /// The reasoning-effort levels the agent advertised via its `ThoughtLevel`
    /// select config option. Empty until the handshake stashes `config_options`
    /// (and empty forever for an agent with no reasoning selector) — the picker
    /// then stays hidden, which is honest for ACP.
    fn efforts(&self) -> Vec<EffortChoice> {
        self.state.lock().map(|s| effort_choices(&s.config_options)).unwrap_or_default()
    }

    /// The agent's currently-selected reasoning level (the `ThoughtLevel` select's
    /// `current_value`), shown as current when the user hasn't picked one.
    fn default_effort(&self) -> Option<String> {
        self.state.lock().ok().and_then(|s| {
            thought_level_select(&s.config_options).map(|(_o, sel)| sel.current_value.0.to_string())
        })
    }

    /// Switch the reasoning effort in-session by writing the picked value to the
    /// agent's `ThoughtLevel` select config option (`session/set_config_option`).
    /// Returning `Ok` tells the app to skip the Claude-style respawn (which would
    /// drop the live ACP session). Bails when the agent exposes no reasoning
    /// selector — but then `efforts()` is empty and the picker is hidden, so this
    /// is unreachable in practice.
    fn set_effort(&self, effort: &str) -> Result<()> {
        let option_id = self.state.lock().ok().and_then(|s| {
            thought_level_select(&s.config_options).map(|(opt, _sel)| opt.id.0.to_string())
        });
        match option_id {
            Some(id) => self.set_config(&id, Value::String(effort.to_string())),
            None => Err(anyhow!("this ACP agent does not expose a reasoning-effort selector")),
        }
    }

    /// The generic feature controls the agent advertised via its config options
    /// (every option that isn't the dedicated model/effort selector). Empty until
    /// the handshake stashes `config_options`.
    fn features(&self) -> Vec<FeatureControl> {
        self.state.lock().map(|s| feature_controls(&s.config_options)).unwrap_or_default()
    }

    /// Apply a feature-control change in-session by writing it to the matching
    /// config option (`session/set_config_option`) — a bool for a toggle, the
    /// chosen value id for a select. Returning `Ok` tells the app to skip the
    /// respawn (ACP applies config live).
    fn set_feature(&self, id: &str, value: FeatureValue) -> Result<()> {
        let json = match value {
            FeatureValue::Bool(b) => Value::Bool(b),
            FeatureValue::Choice(s) => Value::String(s),
        };
        self.set_config(id, json)
    }

    /// Interrupt the in-flight turn (`session/cancel`). Fire-and-forget: the
    /// worker is parked on the prompt future, so we signal the agent directly via
    /// the stashed connection and let the prompt resolve.
    ///
    /// Also resolves every parked permission responder: the ACP child is NOT
    /// respawned on Stop, so a spec-imperfect agent that left a
    /// `session/request_permission` outstanding could otherwise wedge its next
    /// turn awaiting a decision that will never come. Dropping the drained senders
    /// resolves each handler's `orx.await` to `Err`, which it already answers as
    /// `Cancelled` on the wire (`worker.rs`).
    fn cancel(&self) -> Result<()> {
        if let Ok(s) = self.state.lock()
            && let (Some(conn), Some(sid)) = (s.connection.as_ref(), s.session_id.as_ref())
        {
            let _ = conn.send_notification(CancelNotification::new(sid.clone()));
        }
        // Drop the senders OUTSIDE the state lock: dropping wakes each parked
        // permission handler, which then answers `Cancelled` — the same drop-to-
        // resolve trick Zed uses.
        drop(drain_pending(&self.state));
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
    use agent_client_protocol::schema::v1::{
        SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption, SessionMode,
        SessionModeState,
    };

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

    #[test]
    fn model_choices_read_the_model_category_select_in_advertised_order() {
        // ACP has no model primitive: a model list is a `Model`-category select
        // config option. Its option values become the picker `wire`s (in order),
        // its `current_value` is the default, and its `id` is the `set_config` key.
        // A sibling mode-select + boolean toggle must be ignored by the lookup.
        let mode_select = SessionConfigOption::select(
            "mode",
            "Mode",
            "ask",
            vec![SessionConfigSelectOption::new("ask", "Ask")],
        )
        .category(SessionConfigOptionCategory::Mode);
        let model_select_opt = SessionConfigOption::select(
            "model",
            "Model",
            "sonnet",
            vec![
                SessionConfigSelectOption::new("opus", "Claude Opus"),
                SessionConfigSelectOption::new("sonnet", "Claude Sonnet"),
            ],
        )
        .category(SessionConfigOptionCategory::Model);
        let toggle = SessionConfigOption::boolean("reasoning", "Reasoning", true);
        let options = vec![toggle, mode_select, model_select_opt];

        let choices = model_choices(&options);
        assert_eq!(choices.len(), 2);
        assert_eq!(choices[0].wire, "opus");
        assert_eq!(choices[1].wire, "sonnet");
        // `label` carries the option's human-readable `name` (what the picker shows).
        assert_eq!(choices[0].label, "Claude Opus");
        assert_eq!(choices[1].label, "Claude Sonnet");

        let (opt, select) = model_select(&options).expect("model select present");
        assert_eq!(opt.id.0.as_ref(), "model");
        assert_eq!(select.current_value.0.as_ref(), "sonnet");
    }

    #[test]
    fn feature_controls_surface_non_model_effort_options() {
        // A Mode-category select and a Boolean toggle both become generic feature
        // controls; the Model and ThoughtLevel selectors keep their dedicated
        // pickers and are excluded here (no double-surfacing).
        let mode_select = SessionConfigOption::select(
            "permission",
            "Permission",
            "ask",
            vec![
                SessionConfigSelectOption::new("ask", "Ask"),
                SessionConfigSelectOption::new("auto", "Auto"),
            ],
        )
        .category(SessionConfigOptionCategory::Mode);
        let model = SessionConfigOption::select(
            "model",
            "Model",
            "sonnet",
            vec![SessionConfigSelectOption::new("sonnet", "Sonnet")],
        )
        .category(SessionConfigOptionCategory::Model);
        let thought = SessionConfigOption::select(
            "thought",
            "Thinking",
            "high",
            vec![SessionConfigSelectOption::new("high", "High")],
        )
        .category(SessionConfigOptionCategory::ThoughtLevel);
        let toggle = SessionConfigOption::boolean("auto_accept", "Auto accept", true);
        let options = vec![model, thought, mode_select, toggle];

        let feats = feature_controls(&options);
        assert_eq!(feats.len(), 2, "only the mode-select + toggle surface as features");

        let perm = feats.iter().find(|f| f.id == "permission").expect("mode select present");
        match &perm.kind {
            FeatureKind::Select { options, selected } => {
                assert_eq!(options.len(), 2);
                assert_eq!(options[0].wire, "ask");
                assert_eq!(selected.as_deref(), Some("ask"));
            }
            _ => panic!("expected a select feature"),
        }

        let auto = feats.iter().find(|f| f.id == "auto_accept").expect("toggle present");
        assert!(matches!(auto.kind, FeatureKind::Toggle { on: true }));
    }

    #[test]
    fn drain_pending_resolves_parked_responders_to_err() {
        // A parked permission responder awaits its oneshot; draining (what
        // `cancel()` does on Stop) drops the sender, resolving the receiver to
        // `Err` — the worker's handler maps that to a `Cancelled` outcome, so the
        // agent's request is answered rather than left hanging.
        let state = Mutex::new(AcpState::default());
        let (tx, rx) = oneshot::channel::<PermissionDecision>();
        state.lock().unwrap().pending.insert("rid-1".to_string(), tx);

        let drained = drain_pending(&state);
        assert_eq!(drained.len(), 1);
        assert!(state.lock().unwrap().pending.is_empty(), "pending is emptied by the drain");
        drop(drained);
        // The receiver now resolves to Err (sender dropped) rather than pending.
        assert!(futures::executor::block_on(rx).is_err());
    }

    #[test]
    fn effort_choices_read_the_thought_level_select() {
        // A ThoughtLevel-category select becomes the effort picker vocabulary
        // (value → wire, name → label); a sibling Model select is ignored.
        let model = SessionConfigOption::select(
            "model",
            "Model",
            "sonnet",
            vec![SessionConfigSelectOption::new("sonnet", "Sonnet")],
        )
        .category(SessionConfigOptionCategory::Model);
        let thought = SessionConfigOption::select(
            "reasoning",
            "Reasoning",
            "medium",
            vec![
                SessionConfigSelectOption::new("low", "Low"),
                SessionConfigSelectOption::new("medium", "Medium"),
                SessionConfigSelectOption::new("high", "High"),
            ],
        )
        .category(SessionConfigOptionCategory::ThoughtLevel);
        let options = vec![model, thought];

        let efforts = effort_choices(&options);
        assert_eq!(efforts.len(), 3);
        assert_eq!(efforts[0].wire, "low");
        assert_eq!(efforts[1].label, "Medium");
        let (opt, sel) = thought_level_select(&options).expect("thought select present");
        assert_eq!(opt.id.0.as_ref(), "reasoning");
        assert_eq!(sel.current_value.0.as_ref(), "medium");
        // No ThoughtLevel select → empty picker.
        assert!(effort_choices(&[]).is_empty());
    }

    #[test]
    fn model_choices_empty_when_no_model_selector() {
        // An agent that advertises only a boolean toggle has no model list → the
        // picker stays hidden (empty choices, no select).
        let toggle = SessionConfigOption::boolean("reasoning", "Reasoning", false);
        assert!(model_choices(&[toggle]).is_empty());
        assert!(model_select(&[]).is_none());
    }
}
