//! The per-connection RPC dispatcher — transport-agnostic.
//!
//! It owns one connection's auth state, decodes each `Request` frame, and routes
//! it to the [`SessionRegistry`]. It depends only on the `remote-proto`
//! [`Transport`] seam, so the iroh endpoint is one driver and the in-memory
//! loopback drives tests with no network.
//!
//! The connection loop lives in [`serve`]: it multiplexes incoming client
//! requests against the live events of any active `Subscribe`, so a live event is
//! pushed the moment it is produced. The forwarding invariants live in [`stream`];
//! the handshake in [`handshake`]; the authenticated session RPCs in [`handlers`].
//!
//! **Authorization is re-checked on every session RPC *and* every live frame**,
//! not just at connect: a device revoked mid-connection (Phase 7) must stop being
//! served even though its transport is still open.

mod forge;
mod git;
mod handlers;
mod handshake;
mod schedules;
mod serve;
mod stream;
mod transcribe;

use std::sync::Arc;

use oximux_agents::session_registry::SessionRegistry;
use oximux_remote_proto::proto::{
    ASSUMED_VERSION_WHEN_SILENT, MIN_COMPATIBLE_VERSION, PROTOCOL_VERSION, Request, Response,
    RpcError, is_compatible,
};

use crate::auth::{AppPubkey, AuthStore};

/// One connection's mutable state: what it has proven, and which protocol
/// version it speaks.
///
/// The version lives here rather than being checked once and discarded because
/// the gate must also catch a peer that never sent `Hello` at all — silence is
/// itself a version claim (see [`ASSUMED_VERSION_WHEN_SILENT`]), and it can only
/// be acted on when a later request arrives.
struct ConnState {
    authn: ConnAuthn,
    peer_version: u32,
}

impl Default for ConnState {
    fn default() -> Self {
        Self { authn: ConnAuthn::Unauth, peer_version: ASSUMED_VERSION_WHEN_SILENT }
    }
}

/// One connection's authentication state.
enum ConnAuthn {
    /// Nothing proven yet — only handshake RPCs + `Ping` are allowed.
    Unauth,
    /// A `Connect` with no token asked for a challenge; awaiting `AuthProve`.
    PendingChallenge { app_pubkey: AppPubkey, nonce: [u8; 32] },
    /// Authenticated as this device.
    Authed { app_pubkey: AppPubkey },
}

/// Serves RPCs for connections, backed by one [`SessionRegistry`] + [`AuthStore`].
pub struct Dispatcher {
    registry: Arc<SessionRegistry>,
    auth: Arc<AuthStore>,
    /// The desktop's terminals, when the host exposes them.
    ///
    /// `None` is a real configuration, not merely an untouched default: a host
    /// built without a PTY layer answers every terminal RPC with
    /// `Unauthorized`. That is the same answer a device lacking the scope gets,
    /// deliberately — "this host has no terminals" and "you may not see them"
    /// are not distinctions worth leaking to a client.
    terminals: Option<Arc<dyn crate::terminals::TerminalSource>>,
    /// The desktop's session-launch path, when the host exposes it. `None`
    /// answers `Unauthorized` for the same reason `terminals` does — whether a
    /// host can start sessions is not something an unauthorized client should be
    /// able to probe.
    launcher: Option<Arc<dyn crate::launcher::SessionLauncher>>,
    /// The desktop's project list, when the host exposes it. `None` answers with
    /// an empty list rather than `Unauthorized`: unlike `launcher`, an authorized
    /// client asking for projects on a host that simply has none should see "no
    /// projects", not a capability probe — the create path is still gated.
    projects: Option<Arc<dyn crate::projects::ProjectProvider>>,
    /// The desktop's rewind path, when the host exposes it. `None` answers
    /// `Unauthorized`, as `terminals` and `launcher` do.
    rewinder: Option<Arc<dyn crate::rewind::RewindService>>,
    /// The desktop's schedule store, when the host exposes it. `None` answers
    /// `Unauthorized` for the same reason the other optional surfaces do —
    /// whether this desktop keeps schedules is not something an unauthorized
    /// client should be able to probe. Held as the concrete store rather than a
    /// trait because it is already gpui-free and process-spawn-free (it only
    /// reads and writes SQLite rows), so no view-layer seam is needed the way
    /// `launcher` and `rewinder` needed one.
    schedules: Option<Arc<oximux_agents::schedule::ScheduleStore>>,
    /// The desktop's speech-to-text engine, when the host exposes it. `None`
    /// answers `Unauthorized` for the same reason the other optional surfaces do
    /// — whether this desktop can transcribe is not something an unauthorized
    /// client should probe. A trait rather than a concrete type because the
    /// decode loads an ONNX graph and runs on the desktop's own model manager,
    /// which lives above this crate (the same reason `launcher`/`rewinder` are
    /// seams and `schedules` is not).
    transcriber: Option<Arc<dyn crate::transcribe::AudioTranscriber>>,
    /// Wall clock (Unix seconds), injectable so tests are deterministic.
    now_secs: fn() -> u64,
}

impl Dispatcher {
    pub fn new(registry: Arc<SessionRegistry>, auth: Arc<AuthStore>) -> Self {
        Self {
            registry,
            auth,
            terminals: None,
            launcher: None,
            projects: None,
            rewinder: None,
            schedules: None,
            transcriber: None,
            now_secs: system_now_secs,
        }
    }

    /// Expose the desktop's terminals over this dispatcher.
    pub fn with_terminals(mut self, terminals: Arc<dyn crate::terminals::TerminalSource>) -> Self {
        self.terminals = Some(terminals);
        self
    }

    /// Let this dispatcher start new sessions on the desktop.
    pub fn with_launcher(mut self, launcher: Arc<dyn crate::launcher::SessionLauncher>) -> Self {
        self.launcher = Some(launcher);
        self
    }

    /// Let this dispatcher list the desktop's projects as new-session targets.
    pub fn with_projects(mut self, projects: Arc<dyn crate::projects::ProjectProvider>) -> Self {
        self.projects = Some(projects);
        self
    }

    /// Let this dispatcher rewind sessions on the desktop.
    pub fn with_rewinder(mut self, rewinder: Arc<dyn crate::rewind::RewindService>) -> Self {
        self.rewinder = Some(rewinder);
        self
    }

    /// Expose the desktop's schedules over this dispatcher.
    pub fn with_schedule_store(
        mut self,
        schedules: Arc<oximux_agents::schedule::ScheduleStore>,
    ) -> Self {
        self.schedules = Some(schedules);
        self
    }

    /// Let this dispatcher transcribe voice clips with the desktop's engine.
    pub fn with_transcriber(
        mut self,
        transcriber: Arc<dyn crate::transcribe::AudioTranscriber>,
    ) -> Self {
        self.transcriber = Some(transcriber);
        self
    }

    /// Override the clock (tests).
    pub fn with_clock(mut self, now_secs: fn() -> u64) -> Self {
        self.now_secs = now_secs;
        self
    }

    /// Route one non-`Subscribe` request against the current connection state.
    /// Synchronous — the registry commands are non-blocking; only the transport
    /// I/O in [`serve`] is async.
    fn dispatch(&self, state: &mut ConnState, req: Request) -> Response {
        // The version gate precedes everything, including the handshake: an
        // incompatible peer must be turned away before it offers a credential,
        // not after.
        if !is_compatible(state.peer_version) {
            return Response::Error(RpcError::IncompatibleVersion {
                host_version: PROTOCOL_VERSION,
                host_min_compatible: MIN_COMPATIBLE_VERSION,
            });
        }
        match req {
            Request::Hello(h) => self.handle_hello(state, h),
            Request::Ping => Response::Pong,
            Request::Register(r) => self.handle_register(&mut state.authn, r),
            Request::Connect(c) => self.handle_connect(&mut state.authn, c),
            Request::AuthProve(a) => self.handle_auth_prove(&mut state.authn, &a.signature),
            // Everything below requires an authenticated, still-authorized device.
            other => match authorized_pubkey(&state.authn, &self.auth) {
                Some(pubkey) => self.handle_session_rpc(&pubkey, other),
                None => Response::Error(RpcError::Unauthorized),
            },
        }
    }

    /// Route an authenticated request to its handler (in [`handlers`]). `Subscribe`
    /// never reaches here — the serve loop intercepts it to open the live stream.
    fn handle_session_rpc(&self, pubkey: &AppPubkey, req: Request) -> Response {
        match req {
            Request::ListSessions => self.list_sessions(pubkey),
            Request::GetSessionInfo { session_id } => self.session_info(pubkey, &session_id),
            Request::FetchTranscript { session_id } => self.fetch_transcript(pubkey, &session_id),
            Request::SendPrompt(r) => self.send_prompt(pubkey, r),
            Request::ResolvePermission(r) => self.resolve_permission(pubkey, r),
            Request::AnswerQuestion(r) => self.answer_question(pubkey, r),
            Request::Steer { session_id, text } => {
                self.scoped(pubkey, &session_id, |h| h.steer(&text))
            }
            Request::Cancel { session_id } => self.scoped(pubkey, &session_id, |h| h.cancel()),
            Request::ListChoices { session_id } => self.list_choices(pubkey, &session_id),
            Request::EventsSince { session_id, after_seq } => {
                self.events_since(pubkey, &session_id, after_seq)
            }
            Request::ListSchedules => self.list_schedules(pubkey),
            Request::CreateSchedule { name, cwd, prompt, agent_id, recurrence } => {
                self.create_schedule(pubkey, name, cwd, prompt, agent_id, recurrence)
            }
            Request::DeleteSchedule { id } => self.delete_schedule(pubkey, &id),
            Request::SetScheduleEnabled { id, enabled } => {
                self.set_schedule_enabled(pubkey, &id, enabled)
            }
            Request::GetScheduleRuns { schedule_id, limit } => {
                self.schedule_runs(pubkey, &schedule_id, limit)
            }
            // Handshake variants (handled before auth) and `Subscribe` (intercepted
            // in `serve`) never reach here.
            _ => Response::Error(RpcError::BadRequest("unexpected request".into())),
        }
    }
}

/// The device this connection is authenticated as, if it is still authorized.
/// Returns `None` for an unauthenticated OR revoked connection — the per-RPC (and
/// per-live-frame) revocation gate. `pub(super)` so the serve loop shares it.
fn authorized_pubkey(state: &ConnAuthn, auth: &AuthStore) -> Option<AppPubkey> {
    match state {
        ConnAuthn::Authed { app_pubkey } if auth.is_authorized(app_pubkey) => Some(*app_pubkey),
        _ => None,
    }
}

fn system_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
