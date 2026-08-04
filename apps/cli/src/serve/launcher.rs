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

/// How long to wait for a fresh agent's `SessionInit` (which carries the
/// session id this RPC must return). Generous — a cold agent CLI can take a
/// while to boot — but bounded so a wedged one cannot park the RPC forever.
const INIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

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

/// Spawn the agent and wait for its `SessionInit`. Blocking (process spawn +
/// a bounded recv loop) — run under `spawn_blocking`.
fn spawn_and_wait_for_init(spec: ConnectSpec) -> Result<Spawned, LaunchError> {
    let (conn, events) = connect(spec).map_err(|err| {
        // Spawn errors routinely carry host paths; log, return the category.
        tracing::warn!(%err, "headless agent spawn failed");
        LaunchError::Failed
    })?;
    let deadline = std::time::Instant::now() + INIT_TIMEOUT;
    let mut buffered = Vec::new();
    loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .ok_or(LaunchError::Failed)?;
        match events.recv_timeout(remaining) {
            Ok(event) => {
                let init_id = match &event {
                    ThreadEvent::SessionInit { session_id, .. } => Some(session_id.clone()),
                    ThreadEvent::Error(err) => {
                        tracing::warn!(%err, "agent errored before session init");
                        None
                    }
                    _ => None,
                };
                buffered.push(event);
                if let Some(session_id) = init_id
                    && !session_id.is_empty()
                {
                    return Ok(Spawned { conn, events, buffered, session_id });
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                tracing::warn!("agent did not reach session init in time");
                return Err(LaunchError::Failed);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                tracing::warn!("agent exited before session init");
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
        // Granted before the child exists, so the very first `oximux` call it
        // makes is already confined. The handle is scoped to nothing until the
        // rebind below — a spawn that dies before `SessionInit` therefore
        // leaves a credential that reaches no session at all.
        let label = credential_label();
        let secret = self.local.grant_session(&label);
        spec.env = credential_env(&label, &secret);

        let spawned = match tokio::task::spawn_blocking(move || spawn_and_wait_for_init(spec)).await
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
