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
/// the host settles on the one it picked at spawn — for a backend the host
/// **can** name.
///
/// Deliberately short, because it does not bound "how long may an agent take to
/// boot": `--session-id` already told this backend what it is called, so the
/// announcement is a confirmation and the host loses nothing by not waiting for
/// it. It bounds only "might this backend name itself instead".
///
/// It was sixty seconds, sized for a cold CLI boot, and against Claude Code
/// that was a deadlock rather than a slow path: the CLI emits no `system/init`
/// until it has been given a first prompt, and this host sent no prompt until
/// it had seen the init. Every launch spent the full minute and then failed.
const ANNOUNCE_GRACE_TOLD: std::time::Duration = std::time::Duration::from_millis(1500);

/// The same wait for a backend the host **cannot** name — where the agent's own
/// word is the only truth there is.
///
/// `ConnectSpec::fresh_session_id` is read by the `StreamJson` arm alone
/// (`thread/connect.rs`), so Codex and pi are never told an id. Timing out on
/// them does not degrade to a slower path; it produces a session registered
/// under an id its own backend has never heard of. `resume` then hands that id
/// back as a thread id the backend cannot resolve, and Codex's resume
/// "degrades to a fresh start" — a new thread behind a transcript still showing
/// the old conversation. Silent history loss, so this must not be a race.
///
/// Sized above the backends' own `HANDSHAKE_TIMEOUT` (30s in both
/// `thread/codex` and `thread/pi`) so their timeout is the one that fires: they
/// announce from the handshake, with no prompt needed, and a handshake that
/// never completes ends the process — which arrives here as `Disconnected`
/// immediately, not as a wait. Nothing hangs for this long that was ever going
/// to work.
const ANNOUNCE_GRACE_SELF_NAMING: std::time::Duration = std::time::Duration::from_secs(35);

/// Which of the two a transport gets.
///
/// The split *is* the fix: one number cannot serve both "may never announce, do
/// not wait" and "will announce, must be heard".
fn announce_grace(transport: Transport) -> std::time::Duration {
    match transport {
        // Told its id via `--session-id`, and silent until prompted.
        Transport::StreamJson => ANNOUNCE_GRACE_TOLD,
        // Cannot be told; announces at handshake.
        _ => ANNOUNCE_GRACE_SELF_NAMING,
    }
}

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
/// backend announces one of its own within `grace` — see [`announce_grace`] for
/// why that is two different numbers. An announcement wins where it happens:
/// not every transport can be told an id, and for those the agent's own word is
/// the only truth. Claude, which can be told, echoes `chosen` straight back — so
/// the two agree rather than compete.
///
/// Blocking (process spawn + a bounded recv loop) — run under `spawn_blocking`.
fn spawn_and_settle_id(
    spec: ConnectSpec,
    chosen: String,
    grace: std::time::Duration,
) -> Result<Spawned, LaunchError> {
    let (conn, events) = connect(spec).map_err(|err| {
        // Spawn errors routinely carry host paths; log, return the category.
        tracing::warn!(%err, "headless agent spawn failed");
        LaunchError::Failed
    })?;
    let deadline = std::time::Instant::now() + grace;
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
            //
            // Reaching this on a self-naming backend means the handshake
            // outlived its own 30s timeout without the process dying, which
            // should not happen. Say so: the session that follows carries an id
            // its backend does not know, and a later resume will quietly start
            // a fresh thread instead.
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if grace != ANNOUNCE_GRACE_TOLD {
                    tracing::warn!(
                        session_id = %chosen,
                        "agent never announced a session id; \
                         resuming this session will start a fresh conversation"
                    );
                }
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

        let grace = announce_grace(transport);
        let spawned = match tokio::task::spawn_blocking(move || {
            spawn_and_settle_id(spec, chosen, grace)
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The split, stated as the property rather than as two numbers: a backend the
    /// host can name may be waited on briefly, and one it cannot must be waited on
    /// properly.
    ///
    /// Collapsing these back into one constant is the regression. Whichever value
    /// survived would be wrong for the other half — short strands codex and pi under
    /// an id their own backend never heard of, long reintroduces the Claude deadlock
    /// this grace was shortened to escape.
    #[test]
    fn a_backend_that_cannot_be_told_its_id_is_waited_on_far_longer() {
        assert_eq!(announce_grace(Transport::StreamJson), ANNOUNCE_GRACE_TOLD);
        for cannot_be_told in [Transport::AppServer, Transport::Rpc] {
            assert_eq!(
                announce_grace(cannot_be_told),
                ANNOUNCE_GRACE_SELF_NAMING,
                "{cannot_be_told:?} is never handed `--session-id`, so its own \
                 announcement is the only id there is",
            );
        }
        assert!(ANNOUNCE_GRACE_SELF_NAMING > ANNOUNCE_GRACE_TOLD);
    }

    /// And it clears the handshake those backends are themselves allowed.
    ///
    /// Codex announces only after `initialize` → `model/list` → `thread/start`, each
    /// budgeted `HANDSHAKE_TIMEOUT` (30s, in both `thread/codex` and `thread/pi`).
    /// A grace under that budget is a race with the child's own startup, decided by
    /// how loaded the box is — which is exactly the bug: the loser is a session
    /// registered under an id its backend cannot resolve, and a later resume then
    /// silently starts a fresh conversation.
    ///
    /// Hard-coded rather than imported: those constants are private to their
    /// modules, and this asserting against a copy is the point — if theirs grows,
    /// this fails and someone has to look.
    #[test]
    fn the_self_naming_grace_outlasts_the_backend_handshake_it_waits_for() {
        const BACKEND_HANDSHAKE_TIMEOUT: std::time::Duration =
            std::time::Duration::from_secs(30);
        assert!(
            ANNOUNCE_GRACE_SELF_NAMING > BACKEND_HANDSHAKE_TIMEOUT,
            "the child's own timeout must fire first, so a handshake that will \
             never complete ends the process — which arrives as a disconnect, not \
             as a wait",
        );
    }
}
