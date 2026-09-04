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
mod heartbeats;
mod pairing_admin;
mod schedules;
mod serve;
mod state;
mod stream;
mod team;
mod transcribe;
mod transcript_page;
mod worktree_rpcs;

use std::sync::Arc;

use oximux_agents::session_registry::SessionRegistry;
use oximux_remote_proto::proto::{
    ASSUMED_VERSION_WHEN_SILENT, MIN_COMPATIBLE_VERSION, PROTOCOL_VERSION, Request, Response,
    RpcError, is_compatible,
};

use crate::auth::{AppPubkey, AuthStore, LocalScope, Peer};

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
    /// A caller on the desktop's local socket, authenticated by the listener
    /// against the local bearer token **before** this connection reached the
    /// dispatcher. Enters only through [`Dispatcher::serve_local`] — no wire
    /// request constructs it, which is what keeps local authority unreachable
    /// from any remote transport.
    LocalAuthed { scope: LocalScope },
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
    /// The desktop's index of persisted-but-unopened sessions, when the host
    /// exposes it.
    ///
    /// `None` restricts every surface to the registry, which means a client sees
    /// only the sessions whose projects the desktop has already shown this run.
    /// That is a degradation rather than a refusal — the sessions it can see all
    /// work — so unlike `launcher` this has no `Unauthorized` answer.
    catalog: Option<Arc<dyn crate::catalog::SessionCatalog>>,
    /// The host's worktree management, when it exposes one. `None` answers an
    /// **authorized** full-scope caller with `Unsupported` — a host that cannot
    /// manage worktrees should say so rather than masquerade as a refusal —
    /// while an unauthorized or under-scoped caller still gets `Unauthorized`
    /// first, so the capability is not probeable without the scope to use it.
    worktrees: Option<Arc<dyn crate::worktrees::WorktreeService>>,
    /// The endpoint id pairing tickets name, when this host has one bound. A
    /// `PairNew` on a host with no endpoint answers `Unsupported` — a ticket
    /// nobody can dial is not worth minting.
    pairing_endpoint: Option<[u8; 32]>,
    /// The host's manual-fire path, when this process owns the ticker.
    /// `None` answers an **authorized** caller with `Unsupported` — on a
    /// desktop+serve box only the ticker-lock holder can fire, and the loser
    /// saying so beats it pretending.
    schedule_runner: Option<Arc<dyn crate::schedule_runner::ScheduleRunner>>,
    /// Recorded schedule runs, fanned out to session-list subscribers as
    /// [`Response::ScheduleRunsChanged`] pushes. The channel is created by the
    /// host and shared with its ticker's recorded-run hook; `None` simply
    /// pushes nothing.
    schedule_events: Option<tokio::sync::broadcast::Sender<
        oximux_remote_proto::messages::ScheduleRunWire,
    >>,
    /// The host's team runs, when it keeps them. `None` answers an
    /// **authorized** caller `Unsupported`, like `worktrees` — a host without
    /// the table should say so rather than look like a refusal.
    teams: Option<Arc<oximux_agents::team::TeamStore>>,
    /// The coordination blackboard, when the host keeps one. Same
    /// `Unsupported`-for-authorized shape as `teams`.
    coord: Option<Arc<oximux_agents::coord::CoordStore>>,
    /// Coordination writes, fanned out to `StateWatch` subscribers. The
    /// dispatcher owns this one (unlike `schedule_events`, which the host's
    /// ticker feeds) because every writer goes through these handlers.
    state_events: Option<
        tokio::sync::broadcast::Sender<oximux_remote_proto::messages::StateChangeWire>,
    >,
    /// Recent coordination changes, so a `StateWatchFrom` can replay a gap
    /// instead of resyncing. Always present: it is a bounded in-memory ring, so
    /// a host with no watchers pays a `VecDeque` for it and nothing else.
    state_log: state::StateLog,
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
            catalog: None,
            worktrees: None,
            pairing_endpoint: None,
            schedule_runner: None,
            schedule_events: None,
            teams: None,
            coord: None,
            state_events: None,
            state_log: state::StateLog::default(),
            now_secs: system_now_secs,
        }
    }

    /// Let this dispatcher see and open sessions the desktop has persisted but
    /// not yet built views for.
    pub fn with_catalog(mut self, catalog: Arc<dyn crate::catalog::SessionCatalog>) -> Self {
        self.catalog = Some(catalog);
        self
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

    /// Let this dispatcher manage the host's worktrees.
    pub fn with_worktrees(
        mut self,
        worktrees: Arc<dyn crate::worktrees::WorktreeService>,
    ) -> Self {
        self.worktrees = Some(worktrees);
        self
    }

    /// Name the endpoint pairing tickets dial, enabling `PairNew`.
    pub fn with_pairing_endpoint(mut self, endpoint_id: [u8; 32]) -> Self {
        self.pairing_endpoint = Some(endpoint_id);
        self
    }

    /// Let this dispatcher fire schedules on demand — only the process that
    /// owns the ticker lock installs one.
    pub fn with_schedule_runner(
        mut self,
        runner: Arc<dyn crate::schedule_runner::ScheduleRunner>,
    ) -> Self {
        self.schedule_runner = Some(runner);
        self
    }

    /// Fan recorded schedule runs out to session-list subscribers. The host
    /// keeps the sender and feeds it from its ticker's recorded-run hook.
    pub fn with_schedule_events(
        mut self,
        events: tokio::sync::broadcast::Sender<oximux_remote_proto::messages::ScheduleRunWire>,
    ) -> Self {
        self.schedule_events = Some(events);
        self
    }

    /// Expose the host's team runs over this dispatcher.
    pub fn with_team_store(mut self, teams: Arc<oximux_agents::team::TeamStore>) -> Self {
        self.teams = Some(teams);
        self
    }

    /// Expose the coordination blackboard, and open the channel its watchers
    /// ride. Created here rather than passed in: every writer is a handler on
    /// this dispatcher, so nothing outside it has a reason to hold the sender.
    pub fn with_coord_store(mut self, coord: Arc<oximux_agents::coord::CoordStore>) -> Self {
        self.coord = Some(coord);
        let (tx, _) = tokio::sync::broadcast::channel(64);
        self.state_events = Some(tx);
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
            // A local connection is already authenticated; letting the remote
            // handshake run on it would overwrite `LocalAuthed` with whatever
            // state the handshake reaches — a downgrade at best, and at worst a
            // path for a local caller to smuggle itself into the paired-device
            // world. Refused outright.
            Request::Register(_) | Request::Connect(_) | Request::AuthProve(_)
                if matches!(state.authn, ConnAuthn::LocalAuthed { .. }) =>
            {
                Response::Error(RpcError::BadRequest(
                    "a local connection does not pair; it is already authenticated".into(),
                ))
            }
            Request::Register(r) => self.handle_register(&mut state.authn, r),
            Request::Connect(c) => self.handle_connect(&mut state.authn, c),
            Request::AuthProve(a) => self.handle_auth_prove(&mut state.authn, &a.signature),
            // Everything below requires an authenticated, still-authorized caller.
            other => match authorized_peer(&state.authn, &self.auth) {
                Some(peer) => self.handle_session_rpc(&peer, other, state.peer_version),
                None => Response::Error(RpcError::Unauthorized),
            },
        }
    }

    /// Route an authenticated request to its handler (in [`handlers`]). `Subscribe`
    /// never reaches here — the serve loop intercepts it to open the live stream.
    fn handle_session_rpc(&self, peer: &Peer, req: Request, peer_version: u32) -> Response {
        match req {
            Request::ListSessions => self.list_sessions(peer),
            Request::GetSessionInfo { session_id } => self.session_info(peer, &session_id),
            Request::FetchTranscript { session_id } => self.fetch_transcript(peer, &session_id),
            Request::FetchTranscriptPage { session_id, cursor, limit } => {
                self.fetch_transcript_page(peer, &session_id, cursor, limit)
            }
            Request::SendPrompt(r) => self.send_prompt(peer, r),
            Request::ResolvePermission(r) => self.resolve_permission(peer, r),
            Request::AnswerQuestion(r) => self.answer_question(peer, r),
            Request::Steer { session_id, text } => self.steer(peer, &session_id, &text),
            Request::Cancel { session_id } => self.scoped(peer, &session_id, |h| h.cancel()),
            Request::ListChoices { session_id } => self.list_choices(peer, &session_id),
            Request::EventsSince { session_id, after_seq } => {
                self.events_since(peer, &session_id, after_seq, peer_version)
            }
            Request::ListSchedules => self.list_schedules(peer),
            Request::ListSchedulesV2 => self.list_schedules_v2(peer),
            Request::CreateSchedule { name, cwd, prompt, agent_id, recurrence } => {
                self.create_schedule(peer, name, cwd, prompt, agent_id, recurrence)
            }
            Request::CreateScheduleV2 { name, cwd, prompt, agent_id, recurrence } => {
                self.create_schedule_v2(peer, name, cwd, prompt, agent_id, recurrence)
            }
            Request::DeleteSchedule { id } => self.delete_schedule(peer, &id),
            Request::SetScheduleEnabled { id, enabled } => {
                self.set_schedule_enabled(peer, &id, enabled)
            }
            Request::GetScheduleRuns { schedule_id, limit } => {
                self.schedule_runs(peer, &schedule_id, limit)
            }
            // Un-enrolling itself. Gated on the authenticated connection alone: it
            // only ever narrows the caller's own access, so neither full scope nor
            // write access is the right thing to ask for — a session-scoped,
            // read-only device may still stop being paired. A local caller has no
            // enrollment to drop; its lifetime is the desktop's toggle, so it is
            // pointed there instead of being told a no-op succeeded.
            Request::Unpair => match peer.remote_pubkey() {
                Some(pubkey) => {
                    self.auth.forget_self(pubkey);
                    Response::Ack
                }
                None => Response::Error(RpcError::BadRequest(
                    "a local connection is not paired; disable local CLI access in the desktop settings instead".into(),
                )),
            },
            Request::PairNew { read_only } => self.pair_new(peer, read_only),
            Request::PairList => self.pair_list(peer),
            Request::PairRemove { pubkey } => self.pair_remove(peer, &pubkey),
            // v18: the automation surface. `TeamRunCreate` is async (it starts
            // sessions) and is intercepted in `serve`; the rest are store reads
            // and writes, synchronous like the schedule verbs.
            Request::CreateHeartbeat(r) => self.create_heartbeat(peer, r),
            Request::ListHeartbeats { session_id } => {
                self.list_heartbeats(peer, session_id.as_deref())
            }
            Request::DeleteHeartbeat { id } => self.delete_heartbeat(peer, &id),
            Request::TeamReport(r) => self.team_report(peer, r),
            Request::TeamStatus { run_id } => self.team_status(peer, &run_id),
            Request::TeamList => self.team_list(peer),
            // v22: the same board, one shape wider. `TeamRunCreateV2` is async
            // for the same reason its v18 sibling is and is intercepted beside
            // it in `serve`.
            Request::TeamStatusV2 { run_id } => self.team_status_v2(peer, &run_id),
            Request::StateGet { key } => self.state_get(peer, &key),
            Request::StateSet(r) => self.state_set(peer, r),
            Request::StateDelete { key } => self.state_delete(peer, &key),
            // Handshake variants (handled before auth) and `Subscribe` (intercepted
            // in `serve`) never reach here.
            _ => Response::Error(RpcError::BadRequest("unexpected request".into())),
        }
    }
}

/// The caller this connection is authenticated as, if it is still authorized.
/// Returns `None` for an unauthenticated OR revoked connection — the per-RPC (and
/// per-live-frame) revocation gate. A local connection was authenticated by the
/// listener when it accepted the socket; its "revocation" is the desktop tearing
/// the listener (and every open local connection) down when the toggle goes off.
/// `pub(super)` so the serve loop shares it.
fn authorized_peer(state: &ConnAuthn, auth: &AuthStore) -> Option<Peer> {
    match state {
        ConnAuthn::Authed { app_pubkey } if auth.is_authorized(app_pubkey) => {
            Some(Peer::remote(*app_pubkey))
        }
        ConnAuthn::LocalAuthed { scope } => Some(Peer::local(scope.clone())),
        _ => None,
    }
}

fn system_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
