//! The headless [`SessionLauncher`]: spawn an agent, learn its session id from
//! `SessionInit`, register it, and hand the stream to a pump. No tab, no view
//! — the pump is the whole "UI".
//!
//! **Every agent this host spawns is confined to its own session.** Before the
//! child exists it is granted a local-control credential and handed it in its
//! environment, so the `oximux` CLI it runs reaches that one conversation
//! rather than the whole host. The ordering is the awkward part: a session's id
//! arrives with the agent's own `SessionInit`, *after* the environment is
//! fixed, so the credential is minted under an opaque handle and re-pointed at
//! the real id the moment it lands. The window in between is fail-closed — the
//! handle matches no session, so a call made in it reaches nothing.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use oximux_agents::session_registry::SessionRegistry;
use oximux_agents::thread::{ConnectSpec, ThreadEvent, Transport, connect};
use oximux_remote_host::{LaunchError, SessionLauncher};
use oximux_remote_local::{
    LocalControlListener, SESSION_ENV_VAR, SESSION_TOKEN_ENV_VAR, generate_token,
};
use oximux_storage::SettingsRepo;

use super::blob::ChatBlob;
use super::catalog::SessionIndex;
use super::pump::{self, PumpSet, PumpSpec};

/// How long a fresh spawn is given to announce a session id of its own before
/// the host settles on the one it picked at spawn.
///
/// Deliberately short, because it no longer bounds "how long may an agent take
/// to boot" — the host names the session itself now (see `create` below), so
/// there is nothing this wait is required to produce. It bounds only "might
/// this backend name itself instead", and one that does says so in its first
/// line, immediately.
///
/// It was sixty seconds, sized for a cold CLI boot, and against Claude Code
/// that was a deadlock rather than a slow path: the CLI emits no `system/init`
/// until it has been given a first prompt, and this host sent no prompt until
/// it had seen the init. Every launch spent the full minute and then failed.
const ANNOUNCE_GRACE: std::time::Duration = std::time::Duration::from_millis(1500);

/// Map a requested agent id onto a transport this headless host can spawn.
///
/// The desktop resolves ids against its configured roster; serve keeps a
/// fixed vocabulary of the built-in adapters. An unknown id is refused rather
/// than defaulted — starting a *different* agent than the one asked for is
/// worse than starting none. ACP agents need a configured command the
/// headless host does not carry yet, so they are refused too.
fn transport_for(agent_id: Option<&str>) -> Result<Transport, LaunchError> {
    match agent_id {
        None | Some("claude") => Ok(Transport::StreamJson),
        Some("codex") => Ok(Transport::AppServer),
        Some("pi") => Ok(Transport::Rpc),
        Some(_) => Err(LaunchError::Failed),
    }
}

/// Whether `cwd` is a directory this host can open a session in — the same
/// validation the desktop applies, for the same reason: the dispatcher checks
/// authorization, not filesystem reality.
fn usable_working_directory(cwd: &str) -> Result<PathBuf, LaunchError> {
    let resolved = std::path::Path::new(cwd)
        .canonicalize()
        .map_err(|_| LaunchError::BadWorkingDirectory)?;
    if !resolved.is_dir() {
        return Err(LaunchError::BadWorkingDirectory);
    }
    Ok(resolved)
}

pub struct HeadlessLauncher {
    registry: Arc<SessionRegistry>,
    settings: SettingsRepo,
    pumps: Arc<PumpSet>,
    /// Set at drain: a host that is shutting down refuses new work.
    draining: Arc<AtomicBool>,
    /// The shared list-row index — a minted session is noted immediately, so
    /// it lists even if its agent dies before the pump's first persist.
    index: Arc<SessionIndex>,
    /// The local socket, for minting each agent's own confined credential.
    local: Arc<LocalControlListener>,
}

/// The environment an agent carries: which credential it holds, and the secret
/// proving it. Both or neither — a token with no label names nothing to look
/// up, and a label with no token proves nothing.
pub fn credential_env(label: &str, secret: &str) -> Vec<(String, String)> {
    vec![
        (SESSION_ENV_VAR.to_string(), label.to_string()),
        (SESSION_TOKEN_ENV_VAR.to_string(), secret.to_string()),
    ]
}

/// An opaque handle naming one spawn's credential.
///
/// Minted with the credential generator even though a label is not itself a
/// secret: it is the lookup key a caller must name to be *offered* the proof,
/// and a predictable key would let any same-user process aim the handshake at
/// a specific agent's credential instead of having to guess both halves.
fn credential_label() -> String {
    format!("agent-{}", generate_token())
}

impl HeadlessLauncher {
    pub fn new(
        registry: Arc<SessionRegistry>,
        settings: SettingsRepo,
        pumps: Arc<PumpSet>,
        draining: Arc<AtomicBool>,
        index: Arc<SessionIndex>,
        local: Arc<LocalControlListener>,
    ) -> Self {
        Self { registry, settings, pumps, draining, index, local }
    }
}

/// What the blocking spawn half hands back to the async half.
struct Spawned {
    conn: Arc<dyn oximux_agents::thread::AgentConnection>,
    events: std::sync::mpsc::Receiver<ThreadEvent>,
    /// Everything read while waiting for init, `SessionInit` included, in order.
    buffered: Vec<ThreadEvent>,
    session_id: String,
}

/// Spawn the agent and settle which id its session is registered under.
///
/// `chosen` is the id handed to the agent at spawn, and the answer unless the
/// backend announces one of its own within [`ANNOUNCE_GRACE`]. An announcement
/// wins where it happens: not every transport can be told an id, and for those
/// the agent's own word is the only truth. Claude, which can be told, echoes
/// `chosen` straight back — so the two agree rather than compete.
///
/// Blocking (process spawn + a bounded recv loop) — run under `spawn_blocking`.
fn spawn_and_settle_id(spec: ConnectSpec, chosen: String) -> Result<Spawned, LaunchError> {
    let (conn, events) = connect(spec).map_err(|err| {
        // Spawn errors routinely carry host paths; log, return the category.
        tracing::warn!(%err, "headless agent spawn failed");
        LaunchError::Failed
    })?;
    let deadline = std::time::Instant::now() + ANNOUNCE_GRACE;
    let mut buffered = Vec::new();
    loop {
        let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
            return Ok(Spawned { conn, events, buffered, session_id: chosen });
        };
        match events.recv_timeout(remaining) {
            Ok(event) => {
                let announced = match &event {
                    ThreadEvent::SessionInit { session_id, .. } => Some(session_id.clone()),
                    ThreadEvent::Error(err) => {
                        tracing::warn!(%err, "agent errored before it was registered");
                        None
                    }
                    _ => None,
                };
                buffered.push(event);
                if let Some(session_id) = announced.filter(|s| !s.is_empty()) {
                    return Ok(Spawned { conn, events, buffered, session_id });
                }
            }
            // The quiet path, and the normal one for Claude Code: nothing to
            // announce until the agent is asked something. The id chosen at
            // spawn stands, and the prompt that follows is what draws out the
            // `SessionInit` — which the pump folds like any other event.
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                return Ok(Spawned { conn, events, buffered, session_id: chosen });
            }
            // Distinct from quiet: the process is gone. Registering a session
            // whose agent has already exited would hand back an id that can
            // never answer.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                tracing::warn!("agent exited before it could be registered");
                return Err(LaunchError::Failed);
            }
        }
    }
}

#[async_trait::async_trait]
impl SessionLauncher for HeadlessLauncher {
    async fn create(&self, cwd: &str, agent_id: Option<&str>) -> Result<String, LaunchError> {
        if self.draining.load(Ordering::SeqCst) {
            return Err(LaunchError::Unavailable);
        }
        let cwd = usable_working_directory(cwd)?;
        let transport = transport_for(agent_id)?;
        let mut spec = ConnectSpec::for_backend(
            &oximux_agents::thread::ChatBackend::from(transport),
            cwd.clone(),
            None,
            None,
            None,
            None,
        );
        // Named here rather than waited for. A headless host has no one to ask:
        // it must hand this RPC a session id, and the agent it is about to spawn
        // will not volunteer one until it has a prompt to answer — which cannot
        // be sent until this call returns. Choosing the id breaks that circle,
        // and `--session-id` means the agent adopts it rather than being renamed
        // behind its back.
        let chosen = uuid::Uuid::new_v4().to_string();
        spec.fresh_session_id = Some(chosen.clone());
        // Granted before the child exists, so the very first `oximux` call it
        // makes is already confined. The handle is scoped to nothing until the
        // rebind below — a spawn that dies before it is registered therefore
        // leaves a credential that reaches no session at all.
        let label = credential_label();
        let secret = self.local.grant_session(&label);
        spec.env = credential_env(&label, &secret);

        let spawned = match tokio::task::spawn_blocking(move || spawn_and_settle_id(spec, chosen))
            .await
        {
            Ok(Ok(spawned)) => spawned,
            // Nothing will ever bind or revoke this credential, so drop it here
            // rather than leaking a live secret for a process that never
            // started.
            Ok(Err(err)) => {
                self.local.revoke_session(&label);
                return Err(err);
            }
            Err(_) => {
                self.local.revoke_session(&label);
                return Err(LaunchError::Failed);
            }
        };
        self.local.bind_session(&label, &spawned.session_id);

        let handle = self.registry.register(spawned.session_id.clone(), spawned.conn);
        self.index.note(
            &spawned.session_id,
            None,
            None,
            Some(cwd.clone()),
        );
        let mut seed = ChatBlob::new(spawned.session_id.clone());
        seed.provider = transport;
        seed.session_meta.cwd = Some(cwd.to_string_lossy().into_owned());
        // The fold re-derives model/meta from the buffered `SessionInit`; the
        // seed only has to carry what init does not — the provider and cwd
        // that a later resume needs.
        pump::start(
            PumpSpec {
                session_id: spawned.session_id.clone(),
                handle: handle.clone(),
                events: spawned.events,
                buffered: spawned.buffered,
                seed,
                settings: self.settings.clone(),
                registry: self.registry.clone(),
                index: self.index.clone(),
                on_end: Some({
                    let local = self.local.clone();
                    Box::new(move || local.revoke_session(&label))
                }),
            },
            self.pumps.clone(),
        );
        Ok(spawned.session_id)
    }
}
