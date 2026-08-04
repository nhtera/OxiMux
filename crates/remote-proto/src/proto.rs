//! The append-only RPC envelope (`Request`/`Response`) and its postcard codec.
//! The payload structs each variant carries live in [`crate::messages`] and are
//! re-exported here.
//!
//! **Append-only discipline** (mirrors `oximux-relay-proto`'s `PROTOCOL_VERSION`):
//! postcard encodes an enum by the ordinal of its variant, so the wire meaning
//! of `Request`/`Response` is positional. New calls are added by **appending**
//! variants; existing variants are never reordered, removed, or have their
//! payload shape changed. Bump [`PROTOCOL_VERSION`] on every such change and
//! surface a mismatch at the handshake so an old client can't misread a newer
//! host.

use serde::{Deserialize, Serialize};

// The payload structs each variant carries live in `crate::messages`; re-export
// them so callers reach them as `proto::RegisterReq` etc. and `proto::tests`
// sees them via `super`.
pub use crate::messages::*;

/// Bumped whenever the wire schema changes. v1: initial remote-control surface
/// (handshake + session list/info + prompt/resolve/steer/cancel + event
/// subscription & gap-fill). v2: appended the git surface (`GitStatus`,
/// `GitDiff`). v3: appended the version handshake (`Hello`/`HelloAck`). v4:
/// appended the git write surface (`GitStage`, `GitUnstage`, `GitCommit`). v5:
/// appended `AnswerQuestion`. v6: appended the terminal surface
/// (`ListTerminals`, `TermAttach`/`TermInput`/`TermResize`/`TermDetach`, and the
/// pushed `TermOutput`/`TermGapped`/`TermExited` frames). v7: appended the
/// session-control surface (`ListChoices` + `SetModel`/`SetPermissionMode`, and
/// the `Choices` reply, plus `CreateSession`/`SessionCreated`). v8: appended
/// `RewindSession`. v9: appended the read-only forge surface
/// (`ListForgeItems`, `GetForgeItemDetail`, `ListForgeChecks`). v10: appended the
/// schedule surface (`ListSchedules`, `CreateSchedule`, `DeleteSchedule`,
/// `SetScheduleEnabled`, `GetScheduleRuns`, and the `Schedules`/`ScheduleCreated`/
/// `ScheduleRuns` replies). v11: appended the voice-dictation surface
/// (`TranscribeAudio` and its `Transcript` reply) — the phone records a clip and
/// the desktop's existing speech engine decodes it. v12: appended the pushed
/// session-list subscription (`SubscribeSessions` and its `SessionsChanged`
/// frame) — the host streams the session list so the phone need not poll it.
/// v13: appended `FetchTranscript` and its `SessionTranscript` reply — an
/// authoritative folded-transcript snapshot so a client opens a session with its
/// full history even after a host restart (the restored transcript never enters
/// the live event ring). v14: appended `ListProjects` and its `Projects` reply —
/// the host's projects (name + path) so a client can start a session in one
/// without typing its path. v15: appended `Unpair` — a device drops its own
/// enrollment, so forgetting a desktop on the phone also clears the phone from
/// the desktop's paired-devices list. v16: appended the CLI working set — the
/// paginated transcript fetch (`FetchTranscriptPage` + its `TranscriptPage`
/// reply; the v13 `FetchTranscript` is untouched as the legacy unpaginated
/// path), the worktree surface (`CreateWorktree`/`ListWorktrees`/
/// `RemoveWorktree` and the `WorktreeCreated`/`Worktrees` replies), and
/// [`RpcError::Unsupported`] so a host without an optional capability can say
/// so to an *authorized* caller instead of miscategorizing it as a refusal.
/// v16 also carries the local-operator pairing administration a headless host
/// needs (`PairNew`/`PairList`/`PairRemove` and the `PairingIssued`/
/// `PairedDeviceList` replies) — runtime commands, never boot flags, so a
/// bearer ticket is minted on demand instead of reprinted into a journal.
/// v17: appended `RunScheduleNow` (a manual fire that never advances cadence
/// accounting) with its `ScheduleRunRecorded` reply, and the
/// `ScheduleRunsChanged` push — a recorded schedule run delivered to
/// session-list subscribers so run results arrive without polling. The push is
/// gated host-side on the peer having declared ≥ v17
/// ([`SCHEDULE_PUSH_MIN_VERSION`]): unlike a new *reply* (only sent to a peer
/// that asked), a push reaches peers that never opted in, and an older decoder
/// meeting the unknown ordinal would drop the whole connection.
///
/// v18: appended the automation surface — **heartbeats** (`CreateHeartbeat`,
/// `ListHeartbeats`, `DeleteHeartbeat` and the `Heartbeats` reply), a session's
/// own recurring wake-ups, which are schedules aimed at an existing session
/// rather than at a fresh spawn; **team runs** (`TeamRunCreate`, `TeamReport`,
/// `TeamStatus`, `TeamList` and the `TeamRun`/`TeamRuns` replies), a restart-
/// surviving record of a multi-role fan-out; and the **coordination state KV**
/// (`StateGet`, `StateSet`, `StateDelete`, `StateWatch` with the `StateValue`/
/// `StateSnapshot`/`StateChanged`/`StateConflict` frames), a small versioned
/// blackboard agents share without going through a transcript.
///
/// Heartbeats get their own list verb and their own `HeartbeatWire` rather than
/// riding [`Response::Schedules`]: `ScheduleWire` is a postcard struct, so it
/// is as positional as an enum and a field appended to it would misparse every
/// element after the first on an older client. The same reasoning keeps
/// heartbeats out of `ListSchedules` entirely — a v17 phone must not receive
/// rows whose target it has no field to see.
///
/// Appending variants is *not* a breaking change — postcard ordinals of the
/// existing ones are untouched, and an older peer simply never sends or receives
/// the new calls. So this bumps while the transport ALPN
/// (`remote_iroh::OXIMUX_ALPN`) deliberately does not: that tracks breaking
/// changes only, and bumping it would refuse otherwise-compatible peers.
pub const PROTOCOL_VERSION: u32 = 18;

/// The oldest peer that can decode [`Response::StateChanged`]. Like
/// [`SCHEDULE_PUSH_MIN_VERSION`], this exists because a push reaches a peer
/// that never asked for it — but unlike that one, a peer only ever receives
/// `StateChanged` after sending [`Request::StateWatch`], which a pre-v18 peer
/// cannot do. Stated anyway so the gate is a property of the push rather than
/// an inference about who could have subscribed.
pub const STATE_PUSH_MIN_VERSION: u32 = 18;

/// The oldest peer that can decode [`Response::ScheduleRunsChanged`]. Hosts
/// must not push it to a connection whose declared version is older — see the
/// v17 note above for why pushes, uniquely, need this gate.
pub const SCHEDULE_PUSH_MIN_VERSION: u32 = 17;

/// The oldest peer version this build still speaks. **Raise this only on a
/// genuinely breaking change** — a reordered/removed variant or an altered
/// payload shape — never merely because [`PROTOCOL_VERSION`] moved.
///
/// The two constants exist separately because equality is the *wrong* test for
/// an append-only wire: a v1 client and a v3 host understand each other
/// perfectly, since the client simply never sends the appended calls. Rejecting
/// on `!=` (as the unrelated relay protocol does, where it is correct) would
/// break every already-paired phone the moment the desktop shipped an appended
/// RPC — turning a compatible upgrade into a fleet-wide outage.
pub const MIN_COMPATIBLE_VERSION: u32 = 1;

/// The version assumed for a peer that never sent [`Request::Hello`]. Clients
/// predating the version handshake are exactly the v1 clients, so treating
/// silence as v1 is not a fallback — it is the correct reading, and it keeps the
/// compatibility gate meaningful for peers that cannot declare themselves.
pub const ASSUMED_VERSION_WHEN_SILENT: u32 = 1;

/// Whether this build can serve a peer that speaks `peer_version`.
///
/// Asymmetric by design: a peer **older** than [`MIN_COMPATIBLE_VERSION`] is
/// refused, while a **newer** peer is accepted — it knows this version is older
/// and is responsible for confining itself to the calls this build understands.
/// Refusing the newer side too would make every upgrade require both ends to
/// move in lock-step, which is precisely what a negotiated handshake is meant to
/// avoid.
pub fn is_compatible(peer_version: u32) -> bool {
    peer_version >= MIN_COMPATIBLE_VERSION
}

/// A local codec failure (encode/decode), never sent on the wire. Protocol-level
/// failures the host reports to a client are [`RpcError`], carried in
/// [`Response::Error`].
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("postcard envelope codec error: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("json payload codec error: {0}")]
    Json(#[from] serde_json::Error),
}

/// A protocol-level failure the host reports back to a client. Serializable (it
/// crosses the wire), unlike [`WireError`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RpcError {
    /// The client is not (or no longer) authorized for this call — covers both
    /// an unauthenticated connection and a revoked device caught by the
    /// per-RPC recheck.
    Unauthorized,
    /// No session with that id is registered.
    UnknownSession,
    /// A `ResolvePermission` lost the race — the request was already decided.
    /// Idempotent: the client treats this as success, not an error.
    AlreadyDecided,
    /// The request was malformed for the current state (e.g. `AuthProve` with no
    /// challenge outstanding).
    BadRequest(String),
    /// The host hit an internal error handling an otherwise-valid request.
    Internal(String),
    /// The peer's protocol version is too old for this build to serve. Carries
    /// both ends of the host's range so the client can tell the user *what* to
    /// do (upgrade the phone) instead of reporting a bare connection failure.
    /// Appended last to keep the enum's ordinal encoding append-only.
    IncompatibleVersion { host_version: u32, host_min_compatible: u32 },
    /// The host understood the call but does not offer the capability — a
    /// headless host with no speech engine, a build without worktrees. Distinct
    /// from [`RpcError::Unauthorized`] **only for an authorized caller**: the
    /// unauthorized answer stays `Unauthorized` so a capability cannot be probed
    /// without a credential. Clients render "this host cannot do that" rather
    /// than "you may not do that", which would send the user to the wrong fix.
    /// Appended last to keep the enum's ordinal encoding append-only (v16).
    Unsupported,
}

/// Client → host. Append-only; see the module note.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    // ---- handshake ----
    /// First-time pairing: prove possession of the QR's `handshake_secret`.
    Register(RegisterReq),
    /// Reconnect: a fast path with a prior `session_token`, else the challenge
    /// flow (host replies [`Response::Challenge`], client answers [`AuthProve`]).
    Connect(ConnectReq),
    /// Answer a [`Response::Challenge`] by signing the nonce with the app key.
    AuthProve(AuthProveReq),
    /// Liveness probe.
    Ping,
    // ---- session control ----
    /// Bootstrap: every session the host is willing to expose.
    ListSessions,
    /// One session's detail + its current `last_seq` (a resume cursor).
    GetSessionInfo { session_id: String },
    /// Send a user prompt (with optional image attachments) into a session.
    SendPrompt(SendPromptReq),
    /// Answer an outstanding permission request. Idempotent host-side.
    ResolvePermission(ResolvePermissionReq),
    /// Steer a mid-turn agent with additional guidance.
    Steer { session_id: String, text: String },
    /// Cancel the session's in-flight turn.
    Cancel { session_id: String },
    /// Subscribe to a session's live event stream, optionally replaying the
    /// backlog after `after_seq` first (gap-free resume).
    Subscribe { session_id: String, after_seq: Option<u64> },
    /// One-shot backlog replay (gap-fill) for events after `after_seq`.
    EventsSince { session_id: String, after_seq: u64 },
    /// Working-tree status of the repository the session lives in. Scoped by
    /// session so git access inherits the device's existing session ACL — a
    /// session-scoped device cannot browse another project's repository.
    GitStatus { session_id: String },
    /// Diff for one path in the session's repository. `path` is echoed back from
    /// a [`Response::GitStatus`] listing and is **re-contained host-side** — a
    /// client cannot reach outside the repository with it. `untracked` selects the
    /// read-off-disk codepath git itself won't diff; `staged` picks index-vs-HEAD.
    GitDiff { session_id: String, path: String, staged: bool, untracked: bool },
    /// Declare the client's protocol version. Sent first, **before** any
    /// credential, so an incompatible peer is turned away without a secret ever
    /// crossing the wire. Optional on the wire (a v1 client never sends it and is
    /// read as [`ASSUMED_VERSION_WHEN_SILENT`]); appended last to keep the enum's
    /// ordinal encoding append-only.
    Hello(HelloReq),
    /// Stage paths into the session repository's index. Paths are echoed from a
    /// [`Response::GitStatus`] listing and each is **re-contained host-side**.
    /// State-changing, so a read-only device is refused.
    GitStage { session_id: String, paths: Vec<String> },
    /// Remove paths from the index, leaving the worktree untouched.
    GitUnstage { session_id: String, paths: Vec<String> },
    /// Commit what is already staged. Deliberately carries no paths: the
    /// path-taking git variant pre-stages, which would silently overwrite
    /// hunk-level partial staging the remote client cannot see.
    GitCommit { session_id: String, message: String },
    /// Answer an outstanding `AskUserQuestion`. Idempotent host-side, sharing the
    /// same decided-once gate as [`Request::ResolvePermission`]. State-changing —
    /// answering releases a blocked turn — so a read-only device is refused.
    AnswerQuestion(AnswerQuestionReq),
    /// Every terminal the host is willing to expose.
    ///
    /// Not session-scoped, unlike the git RPCs: a terminal is not owned by an
    /// agent session, so there is no `cwd` to inherit an ACL from. A
    /// session-scoped device is therefore refused outright rather than shown a
    /// filtered list — narrowing "this one session" to "these terminals" has no
    /// defensible mapping, and guessing one would silently widen the scope the
    /// desktop user chose.
    ListTerminals,
    /// Attach to a terminal: replies with the replay ring and the dims it was
    /// drawn at, then streams [`Response::TermOutput`] frames live.
    ///
    /// **Read access, not write.** Attaching shows the screen; it cannot type.
    TermAttach { pty_id: String },
    /// Send keystrokes to a terminal.
    ///
    /// **The most dangerous RPC on this protocol**: bytes into a live shell is
    /// arbitrary code execution on the desktop. Gated on full write scope, so a
    /// device downgraded to read-only can watch a terminal it cannot drive.
    TermInput { pty_id: String, bytes: Vec<u8> },
    /// Resize a terminal's grid.
    ///
    /// State-changing and *shared*: the daemon drives the PTY at the smallest
    /// requested size across all attachments, so a phone resizing can reflow
    /// the desktop user's own window. Write-gated for that reason, not because
    /// resizing is dangerous on its own.
    TermResize { pty_id: String, cols: u16, rows: u16 },
    /// Stop streaming a terminal to this connection. Idempotent: detaching one
    /// that was never attached is not an error.
    TermDetach { pty_id: String },
    /// The models and permission modes this session's backend offers, for the
    /// phone's pickers.
    ///
    /// Session-scoped rather than global because the answer depends on which
    /// backend is bound: two open sessions can be running different agents with
    /// different catalogs. Read-only — listing what is available changes nothing.
    ListChoices { session_id: String },
    /// Switch the session's model.
    ///
    /// **Not guaranteed to succeed.** Some backends fix the model at spawn time
    /// and the desktop falls back to respawning the child; that fallback is not
    /// reachable from here yet, so those backends answer with an error rather
    /// than appearing to work. Failing loudly beats a control that silently
    /// does nothing.
    SetModel { session_id: String, model: String },
    /// Switch the session's permission mode. Same fix-at-spawn caveat as
    /// [`Request::SetModel`].
    SetPermissionMode { session_id: String, mode: String },
    /// Start a new agent session on the desktop.
    ///
    /// **The only RPC that creates rather than drives**, and so the
    /// highest-privilege one here: it spawns a process on the developer's
    /// machine. Write-gated, so a read-only device is refused.
    ///
    /// `cwd` is whatever the client sends, by an explicit product decision. That
    /// is not a new capability — a paired device already has shell access via
    /// [`Request::TermInput`] and could `cd` anywhere and launch an agent itself
    /// — but the host still validates the path is a real directory rather than
    /// trusting it.
    ///
    /// `agent_id` picks which configured agent to start; `None` takes the
    /// desktop's default. An unknown id is refused rather than silently
    /// defaulted: starting a *different* agent than the one asked for is worse
    /// than starting none.
    CreateSession { cwd: String, agent_id: Option<String> },
    /// Rewind a session to an earlier turn: drop the user message at `ordinal`
    /// (0-based, counting only user entries) and everything after it.
    ///
    /// **Destructive and not undoable from the client.** The host forks the
    /// conversation to a new backing session rather than editing the original,
    /// so the desktop retains a recovery path — but nothing on this protocol
    /// exposes it. Write-gated.
    ///
    /// `ordinal` rather than an entry index because ordinals count only user
    /// entries, so client and host agree even when their folds disagree about
    /// how many rows a turn produced. The host re-validates it against its own
    /// transcript and refuses a mismatch rather than truncating at the wrong
    /// point.
    ///
    /// `include_files` additionally restores the working tree to the turn's
    /// checkpoint. That is a **destructive filesystem write** — it discards
    /// uncommitted work, including edits made outside this session and by the
    /// person sitting at the desktop. It is accepted on the wire from this
    /// version so enabling it later needs no new variant, but the host may
    /// refuse it; a client must treat refusal as normal, not as an error to
    /// retry.
    RewindSession { session_id: String, ordinal: u32, include_files: bool },
    /// Issues or pull requests for the session's repository.
    ///
    /// A **read** — gated on `is_allowed_for`, not `may_write`. The repository
    /// is resolved from the session's own `cwd`, exactly as the git RPCs do, so
    /// a session-scoped device can only ever reach the project it was scoped
    /// to.
    ///
    /// No credential crosses this wire in either direction. The desktop runs
    /// the forge CLI that is already signed in there; the phone never holds a
    /// token.
    ///
    /// **An empty list is a normal answer, not a failure**: a repo hosted
    /// nowhere relevant, a CLI that is absent or signed out, or simply no
    /// matching items all resolve to empty. A client must render "nothing
    /// here", never an error — the host cannot distinguish these cases and does
    /// not pretend to.
    ListForgeItems { session_id: String, kind: ForgeItemKindWire, state: ForgeStateWire, mine: bool },
    /// Body + author of one issue/PR — the lazy companion to
    /// [`Request::ListForgeItems`], whose rows omit the body so a 50-item list
    /// stays small.
    GetForgeItemDetail { session_id: String, kind: ForgeItemKindWire, number: u64 },
    /// CI check runs for the current branch's pull request.
    ///
    /// Empty when there is no PR, no checks, or the forge does not report any —
    /// including every GitLab repo, which has no pipeline mapping wired.
    ListForgeChecks { session_id: String },
    /// Every schedule the desktop holds, with its next fire and human summary.
    ///
    /// A **global read** — it names no session, so there is nothing for a
    /// session-scoped device to be narrowed to. It is therefore gated on full
    /// scope, not `is_allowed_for`: a schedule can target any project, and its
    /// prompt and cwd would leak work outside a confined device's one
    /// conversation. A read-only full device may still list — seeing standing
    /// schedules changes nothing.
    ListSchedules,
    /// Create a schedule that fires a canned prompt on a recurrence.
    ///
    /// **A standing grant to run an agent unattended**, so it is the
    /// highest-privilege schedule RPC: gated exactly as [`Request::CreateSession`]
    /// is (full scope **and** not read-only), because a schedule is a deferred
    /// session spawn and a session-scoped device could otherwise plant one that
    /// runs outside its own confinement.
    ///
    /// `recurrence` is validated host-side through the same constructors the
    /// desktop uses — an interval under the floor, or an impossible time, is
    /// refused rather than stored, so the phone cannot smuggle in a recurrence the
    /// desktop's own UI could never produce.
    CreateSchedule {
        name: String,
        cwd: String,
        prompt: String,
        agent_id: Option<String>,
        recurrence: RecurrenceWire,
    },
    /// Delete a schedule by id. Idempotent: deleting one already gone is not an
    /// error. Write-gated like [`Request::CreateSchedule`]. In-flight runs are
    /// unaffected — this stops *future* fires, it does not reach into a run
    /// already started.
    DeleteSchedule { id: String },
    /// Enable or disable a schedule without deleting it. Same write gate as
    /// creating one — a disabled schedule that a lost phone re-enables is the same
    /// standing grant as a freshly created one.
    SetScheduleEnabled { id: String, enabled: bool },
    /// The recent run history for one schedule (most recent first, capped at
    /// `limit`). A **read**, gated like [`Request::ListSchedules`]: run rows carry
    /// the same cross-project detail a schedule row does.
    GetScheduleRuns { schedule_id: String, limit: u32 },
    /// Decode one voice clip to text with the desktop's speech engine.
    ///
    /// `audio_base64` is a standard-base64 WAV (16 kHz mono PCM16 is the
    /// phone's contract; the host reads the real rate from the header and
    /// resamples when it differs). `sample_rate` is the phone's declared capture
    /// rate, used only as a fallback for a headerless raw-PCM payload.
    ///
    /// **Mutates nothing** — a composer utility, not a session command — so it is
    /// gated on the authenticated-connection requirement alone, not on write
    /// scope: any paired device may dictate, exactly as any of them may type. No
    /// audio is retained past the decode call.
    TranscribeAudio { audio_base64: String, sample_rate: u32 },
    /// Subscribe to the live session list. The immediate reply is a plain
    /// [`Response::Sessions`] snapshot — the same per-device-filtered list
    /// [`Request::ListSessions`] returns — after which the host **pushes** a fresh
    /// [`Response::SessionsChanged`] whenever a session opens, closes, or its title/
    /// model/status changes, so a subscribed client never polls. The immediate reply
    /// and the pushes use different variants for the same reason `Subscribe` replies
    /// with `Events` but pushes `Event`: the client demux tells a solicited reply
    /// from an unsolicited push by variant. Read-only: gated on the authenticated-
    /// connection requirement, like `ListSessions`.
    SubscribeSessions,
    /// Fetch a session's folded transcript as an authoritative snapshot, so a
    /// client opening a session sees its full history — including a transcript
    /// restored from disk after a host restart, which never entered the live event
    /// ring. The reply ([`Response::SessionTranscript`]) carries the folded entries
    /// plus the `seq` they reflect; the client rehydrates its fold from them and
    /// then subscribes from that `seq` so live events extend the snapshot without a
    /// gap or a duplicate. Read-only; scope-checked like the other session RPCs.
    FetchTranscript { session_id: String },
    /// List the projects (workspaces) the host knows, so a client can start a new
    /// session in one without typing its path. The reply ([`Response::Projects`])
    /// carries each project's display name and absolute host path; the client hands
    /// the chosen path straight to [`Request::CreateSession`]. Scope-checked like
    /// `CreateSession` — the list is only useful for creating, and it exposes host
    /// paths, so a device that may not create sessions may not enumerate them.
    ListProjects,
    /// Drop **this** device's enrollment, so a user who forgets the desktop on
    /// their phone also disappears from the desktop's paired-devices list. Replies
    /// [`Response::Ack`]; the connection is unauthorized from the next RPC on.
    ///
    /// Carries no pubkey: it acts on the calling connection's own identity, which
    /// the host already authenticated. A device that could name its subject would
    /// be a remote un-enrollment of *other* devices, which is a desktop-only act.
    ///
    /// Erases the record rather than tombstoning it, so the phone may pair again
    /// with a fresh code — the same asymmetry the desktop's own Forget has against
    /// Revoke. Revocation stays out of reach here because a revoked device fails
    /// the authorization gate before this handler runs, so it cannot clear its own
    /// tombstone and re-pair.
    Unpair,
    /// One page of a session's folded transcript — the paginated successor to
    /// [`Request::FetchTranscript`], which is **left untouched** as the legacy
    /// unpaginated path (changing its reply shape would break every v15 phone;
    /// postcard payloads are positional and may never be reshaped).
    ///
    /// `cursor` is the folded-entry index to start from (0 for the first page);
    /// `limit` caps how many entries this page may carry. The host additionally
    /// enforces a byte budget so a page always fits one transport frame — a
    /// client must treat a short page as normal and keep paging by
    /// `next_cursor`, never assume `limit` entries arrived. Read-only;
    /// scope-checked like the other session RPCs. Appended for v16.
    FetchTranscriptPage { session_id: String, cursor: u64, limit: u32 },
    /// Create a git worktree (a workspace) under a project.
    ///
    /// **The target path is host-derived, never client-supplied**: the host
    /// composes it from its own data directory, the project's id, and the
    /// sanitized slug — exactly as the desktop's own New-Worktree flow does. The
    /// client only picks *which* project, by the absolute root path a
    /// [`Response::Projects`] row already handed it; a path matching no known
    /// project is refused, so this cannot be aimed at an arbitrary repository.
    ///
    /// A **write to the filesystem and the repository** (new worktree + new
    /// branch), and it names no session — so it is gated on a dedicated
    /// full-scope, non-read-only check, the same shape as
    /// [`Request::CreateSession`]: a session-scoped device could otherwise mint
    /// itself a directory outside its confinement.
    CreateWorktree { project_path: String, slug: String },
    /// List a project's worktrees (or every project's, when `project_path` is
    /// `None`). A **read**, but a full-scope one: worktree rows carry host
    /// paths and branch names across all projects, which a session-scoped
    /// device must not enumerate. A read-only full device may list — seeing
    /// worktrees changes nothing.
    ListWorktrees { project_path: Option<String> },
    /// Remove a worktree by the id a [`Response::Worktrees`] row carried —
    /// **never by path**, so there is no path for a client to aim. Destructive
    /// (deletes the worktree directory and its branch), so it shares
    /// [`Request::CreateWorktree`]'s gate. Removing one already gone is not an
    /// error.
    RemoveWorktree { id: String },
    /// Open a pairing window and mint its ticket — the runtime `pair-new`
    /// command a headless host takes instead of a boot flag (a flag would
    /// reprint the bearer ticket into the journal on every restart).
    ///
    /// **Local-operator only** — the strictest gate on this protocol. A paired
    /// device that could mint tickets could enroll further devices (lateral
    /// movement), so even full-scope remote devices are refused; only a caller
    /// on the host's own owner-only socket may ask. The reply carries a bearer
    /// credential: the CLI prints it to an interactive TTY only, and the host
    /// must never log it. Tickets are one-time and short-lived by construction.
    ///
    /// `read_only` opts the resulting enrollment down to the read tier;
    /// the default mints full write, per the recorded product decision.
    PairNew { read_only: bool },
    /// The host's paired devices — tier, revocation, last-seen — so a mistaken
    /// enrollment is visible. Same local-operator gate as
    /// [`Request::PairNew`]: the device list is admin surface, not device
    /// surface.
    PairList,
    /// Erase one device's enrollment by pubkey (the host-side counterpart of
    /// the desktop's Forget). Idempotent; same local-operator gate. Erasure
    /// rather than revocation, so the device may pair again with a fresh
    /// ticket — revocation stays a desktop-UI act.
    PairRemove { pubkey: [u8; 32] },
    /// Fire one schedule immediately, recording the run **without advancing
    /// cadence accounting** — `next_fire_at` is untouched and the scheduled
    /// occurrence still fires on time. Works on paused schedules too: an
    /// explicit run-now outranks the pause, which only silences the clock.
    ///
    /// Same write gate as [`Request::CreateSchedule`] (full scope, not
    /// read-only): this spawns a session. The reply is
    /// [`Response::ScheduleRunRecorded`] with the settled run — the RPC waits
    /// for the fire to start (or fail to), not for the agent turn to finish.
    /// A host whose scheduling is owned by another process (the ticker lock
    /// lost) answers [`RpcError::Unsupported`].
    RunScheduleNow { schedule_id: String },

    // ---- v18: automation primitives ----
    /// Arm a recurring wake-up **inside an existing session** — a heartbeat.
    ///
    /// Unlike [`Request::CreateSchedule`], which spawns a fresh session per
    /// fire, this delivers its prompt into a conversation that is already open,
    /// with all the context that conversation has built. That is what makes it
    /// useful to the agent living there, and it is why the gate is
    /// **session-scoped, not full-scope**: a heartbeat is a deferred
    /// `SendPrompt` into one session, so anyone who may prompt that session
    /// may arm one. A read-only device may not; a session-confined agent may,
    /// for its own session only.
    ///
    /// `session_id` is `None` for a caller that IS a session (the confined
    /// agent case) — the host resolves it from the connection's own scope,
    /// which is the only value it could legitimately name. An operator must
    /// name one.
    ///
    /// Capped per session host-side; the overflow refusal is a
    /// [`RpcError::BadRequest`] naming the limit.
    CreateHeartbeat(CreateHeartbeatReq),
    /// The heartbeats armed for a session. `None` means the calling session,
    /// as in [`Request::CreateHeartbeat`]. A read (scope-checked, read-only
    /// devices welcome).
    ListHeartbeats { session_id: Option<String> },
    /// Disarm one heartbeat by id. Idempotent — deleting one already gone is
    /// success. Same write gate as arming it, checked against the session the
    /// heartbeat actually targets rather than one the caller names, so a
    /// confined agent cannot delete another session's wake-ups.
    DeleteHeartbeat { id: String },
    /// Open a team run: one row per role, each with a session started for it.
    ///
    /// The run outlives the process that created it — a host restart
    /// re-associates live sessions to open runs rather than losing them, which
    /// is the whole reason this is host state and not a client-side script's
    /// bookkeeping. Full write scope, like [`Request::CreateSession`]: it
    /// starts sessions, several at once.
    TeamRunCreate(TeamRunCreateReq),
    /// A role reporting its own outcome — called by the agent working that
    /// role, from inside its session. Scope-checked against **that role's
    /// session**, so a confined agent can settle its own row and no other.
    TeamReport(TeamReportReq),
    /// One run's roles and their statuses. A read; full scope, since a run
    /// spans sessions and a confined caller has no defensible narrowing.
    TeamStatus { run_id: String },
    /// Every team run this host holds, newest first. Same read gate as
    /// [`Request::TeamStatus`].
    TeamList,
    /// Read one coordination key. Absent is a normal answer
    /// ([`Response::StateValue`] with `entry: None`), not an error.
    StateGet { key: String },
    /// Write one coordination key.
    ///
    /// `if_version` is optimistic concurrency: `Some(v)` writes only if the
    /// stored version is exactly `v` (`Some(0)` means "only if absent"), and a
    /// mismatch is refused with the *current* entry so the caller can merge and
    /// retry rather than guess. `None` overwrites unconditionally.
    StateSet(StateSetReq),
    /// Delete one coordination key. Idempotent.
    StateDelete { key: String },
    /// Subscribe to coordination-state changes, optionally narrowed to keys
    /// with `prefix`. The reply is the matching entries as they stand now;
    /// [`Response::StateChanged`] frames follow for every later write or
    /// delete, so a watcher never has to poll and never misses the baseline.
    StateWatch { prefix: Option<String> },
}

/// Host → client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Response {
    /// Registration accepted; `session_token` is the reconnect credential — a
    /// bearer secret the host must never log.
    Registered { session_token: String },
    /// Challenge flow: sign this nonce and reply with [`Request::AuthProve`].
    Challenge { nonce: [u8; 32] },
    /// Authenticated; `session_token` refreshes the reconnect credential — a
    /// bearer secret the host must never log.
    Connected { session_token: String },
    /// Reply to [`Request::Ping`].
    Pong,
    /// Reply to [`Request::ListSessions`].
    Sessions(Vec<SessionSummary>),
    /// Reply to [`Request::GetSessionInfo`].
    SessionInfo(SessionInfoWire),
    /// Generic success for a command with no payload (prompt/steer/cancel/resolve).
    Ack,
    /// Reply to [`Request::EventsSince`] — the replayed backlog. Also the immediate
    /// reply to [`Request::Subscribe`]: the backlog after `after_seq`, before the
    /// live [`Response::Event`] frames begin.
    Events(Vec<HostEvent>),
    /// The request failed at the protocol level.
    Error(RpcError),
    /// A single live event **pushed** to a subscriber, unsolicited (no matching
    /// request frame) — the live edge that follows the [`Response::Events`]
    /// backlog once a [`Request::Subscribe`] is accepted. Appended last to keep
    /// the enum's ordinal encoding append-only.
    ///
    /// **Gap contract:** live frames carry a monotonically increasing `seq` per
    /// session, but the stream is *lossy under lag* — if the host's bounded live
    /// ring laps before a slow subscriber reads it, the skipped span is dropped
    /// silently (not re-sent here). A client detects this as a **jump in `seq`**
    /// between consecutive `Event` frames for a session and resynchronizes with
    /// [`Request::EventsSince`] `{ after_seq: last_seq_seen }`. `HostEvent.status`
    /// is the session's status *at forward time*, not as of `seq`, so it can lead
    /// `seq` even with no loss — it is a freshness hint, never the gap signal.
    Event(HostEvent),
    /// Reply to [`Request::GitStatus`]. Appended last to keep the enum's ordinal
    /// encoding append-only.
    GitStatus(GitStatusWire),
    /// Reply to [`Request::GitDiff`] — one entry per file the diff covers.
    GitDiff(Vec<FileDiffWire>),
    /// Reply to [`Request::Hello`] — the host's version and its oldest supported
    /// peer, so the client can refuse a host it cannot understand. Appended last
    /// to keep the enum's ordinal encoding append-only.
    HelloAck(HelloAckWire),
    /// Reply to [`Request::GitCommit`] — the new HEAD sha.
    GitCommitted { sha: String },
    /// Reply to [`Request::ListTerminals`].
    Terminals(Vec<TerminalSummary>),
    /// Reply to [`Request::TermAttach`]: the replay ring plus the dims it was
    /// drawn at.
    ///
    /// A client MUST build its emulator at exactly these dims before feeding it
    /// `replay` — the bytes were produced by a process drawing into a grid of
    /// this size, and absolute-position sequences land in the wrong cells in any
    /// other. This mirrors the desktop's own attach contract rather than
    /// inventing a second one.
    TermAttached { replay: Vec<u8>, cols: u16, rows: u16 },
    /// Terminal bytes **pushed** to an attached client, unsolicited.
    ///
    /// **Lossy under lag, and deliberately not sequenced.** The relay drops
    /// output for a subscriber that falls behind and tells it so; this bridge
    /// forwards that signal as [`Response::TermGapped`] rather than papering
    /// over it. Do not read a continuous `TermOutput` stream as a guarantee that
    /// no bytes were skipped — the gap notice is the signal, not the byte count.
    TermOutput { pty_id: String, bytes: Vec<u8> },
    /// The host dropped terminal output destined for this client.
    ///
    /// Recovery is to re-issue [`Request::TermAttach`], which returns a fresh
    /// replay snapshot. Forwarded rather than swallowed because a client that
    /// keeps rendering after a gap is drawing a screen with a hole in it, and
    /// nothing downstream can detect that on its own.
    TermGapped { pty_id: String },
    /// A terminal ended. The client should stop rendering it as live.
    TermExited { pty_id: String, code: Option<i32> },
    /// Reply to [`Request::ListChoices`].
    Choices(SessionChoices),
    /// Reply to [`Request::CreateSession`] — the id the new session is
    /// registered under, so the client can subscribe to it directly rather than
    /// re-listing and guessing which row is new.
    SessionCreated { session_id: String },
    /// Reply to [`Request::ListForgeItems`]. Empty is a normal answer.
    ForgeItems(Vec<ForgeItemWire>),
    /// Reply to [`Request::GetForgeItemDetail`]. `None` when the forge CLI
    /// could not supply it — a distinct answer from an empty body, which is a
    /// real item that simply has no description.
    ForgeItemDetail(Option<ForgeItemDetailWire>),
    /// Reply to [`Request::ListForgeChecks`]. Empty is a normal answer.
    ForgeChecks(Vec<CheckRunWire>),
    /// Reply to [`Request::ListSchedules`]. Empty is a normal answer — a desktop
    /// with no schedules is the common case, not a failure.
    Schedules(Vec<ScheduleWire>),
    /// Reply to [`Request::CreateSchedule`] — the stored row, so the client can
    /// insert it without re-listing. Carries the derived id and first `next_fire`,
    /// which the client could not have computed itself.
    ScheduleCreated(ScheduleWire),
    /// Reply to [`Request::GetScheduleRuns`]. Empty means the schedule has never
    /// fired yet, a normal state for a freshly created one.
    ScheduleRuns(Vec<ScheduleRunWire>),
    /// Reply to [`Request::TranscribeAudio`] — the decoded transcript. An empty
    /// string is a normal answer (the clip was silence, or only filler the engine
    /// dropped), not a failure: the client inserts it as-is, which is a no-op.
    Transcript(String),
    /// The session list, **pushed** on every change to a
    /// [`Request::SubscribeSessions`] subscriber (open/close/rename/permission). A
    /// distinct variant from [`Response::Sessions`] — which is both the
    /// `ListSessions` reply and `SubscribeSessions`'s immediate snapshot reply — so
    /// the client demux routes these unsolicited pushes to the subscription stream
    /// rather than an RPC reply slot, exactly as `Event` is distinct from the
    /// `Events` reply. Appended last to keep the ordinal encoding append-only.
    SessionsChanged(Vec<SessionSummary>),
    /// A session's folded transcript snapshot — the reply to
    /// [`Request::FetchTranscript`]. Carries the folded entries (as JSON, mirroring
    /// [`HostEvent`]'s reason for not being native postcard: the entry tree is deep
    /// and still evolving) plus the `seq` they reflect, so the client rehydrates its
    /// fold and resumes the live stream from that cursor.
    SessionTranscript(SessionTranscriptWire),
    /// The host's known projects — the reply to [`Request::ListProjects`]. Each
    /// carries a display name and the absolute host path a client passes to
    /// [`Request::CreateSession`]. May be empty (no projects, or the host exposes
    /// none).
    Projects(Vec<ProjectSummaryWire>),
    /// One page of a folded transcript — the reply to
    /// [`Request::FetchTranscriptPage`]. Carries the page's entries plus the
    /// cursor to continue from (`None` when this was the last page). The `seq`
    /// is the fold cursor of the **whole** snapshot, identical on every page,
    /// so a client subscribes from it after the final page exactly as it would
    /// after a [`Response::SessionTranscript`]. Appended for v16.
    TranscriptPage(TranscriptPageWire),
    /// Reply to [`Request::CreateWorktree`] — the created row, so the client
    /// can start a session in it (`path` feeds
    /// [`Request::CreateSession`]'s cwd) without re-listing.
    WorktreeCreated(WorktreeWire),
    /// Reply to [`Request::ListWorktrees`]. Empty is a normal answer — a
    /// project with no worktrees is the common case, not a failure.
    Worktrees(Vec<WorktreeWire>),
    /// Reply to [`Request::PairNew`] — the encoded ticket and its window.
    /// **Contains a bearer credential**: hosts never log it, and the CLI
    /// refuses to print it anywhere but an interactive terminal.
    PairingIssued(PairingIssuedWire),
    /// Reply to [`Request::PairList`].
    PairedDeviceList(Vec<PairedDeviceWire>),
    /// Reply to [`Request::RunScheduleNow`] — the manual run as recorded,
    /// success or failure (the failure detail rides in the run itself; an
    /// `Error` reply is reserved for refusals, not fires that ran and failed).
    ScheduleRunRecorded(ScheduleRunWire),
    /// A schedule run **pushed** to session-list subscribers the moment it is
    /// recorded, unsolicited — scheduled and manual fires alike, so a phone or
    /// CLI watching the host learns a run's outcome without polling
    /// [`Request::GetScheduleRuns`]. Only sent to peers that declared
    /// ≥ [`SCHEDULE_PUSH_MIN_VERSION`] in their `Hello`.
    ScheduleRunsChanged(ScheduleRunWire),

    // ---- v18: automation primitives ----
    /// Reply to [`Request::CreateHeartbeat`] — the armed wake-up.
    HeartbeatCreated(HeartbeatWire),
    /// Reply to [`Request::ListHeartbeats`]. Empty is normal.
    Heartbeats(Vec<HeartbeatWire>),
    /// Reply to [`Request::TeamRunCreate`] and [`Request::TeamStatus`] — one
    /// run with every role's current state.
    TeamRun(TeamRunWire),
    /// Reply to [`Request::TeamList`], newest run first.
    TeamRuns(Vec<TeamRunWire>),
    /// Reply to [`Request::StateGet`], and to a [`Request::StateSet`] that
    /// **wrote**: the entry as it now stands, or `None` for a key that does not
    /// exist. A refused conditional write answers
    /// [`Response::StateConflict`] instead.
    StateValue(Option<StateEntryWire>),
    /// Reply to [`Request::StateWatch`] — the entries matching the prefix as
    /// they stand at subscribe time, before any pushed change.
    StateSnapshot(Vec<StateEntryWire>),
    /// One coordination-state write or delete **pushed** to a watcher,
    /// unsolicited. A delete carries `entry: None` under the deleted key.
    /// Never sent to a peer below [`STATE_PUSH_MIN_VERSION`].
    StateChanged { key: String, entry: Option<StateEntryWire> },
    /// A conditional [`Request::StateSet`] lost: the stored version was not the
    /// one the caller expected. Carries the entry it lost to (`None` when the
    /// caller expected a value and the key is absent), so a losing writer can
    /// merge and retry without a second round trip.
    ///
    /// Its own variant rather than a `StateValue` carrying the current entry:
    /// those two are indistinguishable whenever the winning write happens to
    /// leave the value the loser was trying to store, and a caller that
    /// mistakes a refusal for a success is exactly the lost update this whole
    /// mechanism exists to prevent. Not an [`RpcError`] either — nothing is
    /// wrong, the caller simply lost a race it asked to be told about.
    StateConflict(Option<StateEntryWire>),
}

/// What a session's backend offers for its model and permission-mode pickers.
///
/// Both lists can legitimately be **empty**: a dynamic-catalog backend reports
/// nothing until its handshake completes, and an agent may expose no mode
/// choices at all. An empty list means "no choice to offer", never an error —
/// the phone hides the picker rather than showing a broken one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionChoices {
    pub models: Vec<Choice>,
    pub modes: Vec<Choice>,
    /// What is active now, so the picker can mark it without a second round trip.
    pub current_model: Option<String>,
    pub current_mode: Option<String>,
}

/// One selectable option.
///
/// `id` is what goes back over the wire; `label` is what a person reads. They
/// are kept separate because a backend's identifier is often not presentable
/// (`claude-opus-5` against "Opus 5"), and `description` carries the
/// second line the desktop's own picker shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

impl Request {
    /// Postcard-encode this request (the envelope has no `Value`, so this is
    /// pure postcard).
    pub fn to_bytes(&self) -> Result<Vec<u8>, WireError> {
        Ok(postcard::to_stdvec(self)?)
    }
    /// Decode a request from postcard bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, WireError> {
        Ok(postcard::from_bytes(bytes)?)
    }
}

impl Response {
    /// Postcard-encode this response.
    pub fn to_bytes(&self) -> Result<Vec<u8>, WireError> {
        Ok(postcard::to_stdvec(self)?)
    }
    /// Decode a response from postcard bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, WireError> {
        Ok(postcard::from_bytes(bytes)?)
    }
}

#[cfg(test)]
mod tests;
