//! Payload structs carried by the [`Request`](crate::proto::Request) /
//! [`Response`](crate::proto::Response) envelope, plus the streamed
//! [`HostEvent`] frame.
//!
//! `ThreadEvent` and `PermissionDecision` cross the wire only as `serde_json`
//! strings ([`HostEvent::event_json`], [`ResolvePermissionReq::decision_json`]),
//! both private so a payload can only be built through the encoding constructor
//! — see the crate-level note for why they are not native postcard.

use oximux_agent_core::thread::{
    AskQuestion, ChatImage, PermissionDecision, QuestionAnswers, SessionMeta, ThreadEvent,
};
use serde::{Deserialize, Serialize};

use crate::proto::WireError;

/// [`Request::Hello`](crate::proto::Request::Hello) payload — the version
/// declaration a client opens with, before any credential is offered.
///
/// Deliberately a **new variant** rather than a field on [`RegisterReq`] /
/// [`ConnectReq`]: postcard encodes struct fields positionally, so adding a field
/// to an existing payload would silently change how every older peer decodes it —
/// the exact class of breakage this handshake exists to catch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloReq {
    /// The version the client was built against
    /// ([`PROTOCOL_VERSION`](crate::proto::PROTOCOL_VERSION)).
    pub protocol_version: u32,
}

/// [`Response::HelloAck`](crate::proto::Response::HelloAck) payload — the host's
/// half of the version exchange, so *each* side can refuse a peer it cannot
/// understand rather than only the host policing the client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloAckWire {
    /// The host's own [`PROTOCOL_VERSION`](crate::proto::PROTOCOL_VERSION).
    pub protocol_version: u32,
    /// The oldest peer version this host still speaks
    /// ([`MIN_COMPATIBLE_VERSION`](crate::proto::MIN_COMPATIBLE_VERSION)).
    pub min_compatible: u32,
}

/// [`Request::Register`](crate::proto::Request::Register) payload — the QR-pairing proof.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegisterReq {
    /// The client's app-signing Ed25519 public key (decoupled from the iroh
    /// transport key), the stable identity the host authorizes and can revoke.
    pub app_pubkey: [u8; 32],
    /// Human label for the paired-devices list.
    pub device_name: String,
    /// `HMAC-SHA256(handshake_secret, app_pubkey || timestamp_secs)` — proves the
    /// client scanned this host's QR without ever sending the secret itself. A
    /// one-time proof, but still a credential: the host must never log it.
    pub proof: [u8; 32],
    /// Unix seconds; the host accepts a ±60s window to bound replay.
    pub timestamp_secs: u64,
    /// A one-time ticket may bind pairing to a single session; `None` for a
    /// static/global ticket.
    pub session_id: Option<String>,
}

/// [`Request::Connect`](crate::proto::Request::Connect) payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectReq {
    pub app_pubkey: [u8; 32],
    /// Present → fast reconnect; absent → the host issues a challenge. A bearer
    /// credential (see [`Response::Connected`](crate::proto::Response::Connected))
    /// — the host must never log it.
    pub session_token: Option<String>,
}

/// [`Request::AuthProve`](crate::proto::Request::AuthProve) payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthProveReq {
    /// Ed25519 signature (64 bytes) over the host's challenge nonce. A `Vec`
    /// rather than `[u8; 64]` because serde derives `Deserialize` only for
    /// arrays up to length 32; the host validates the length.
    pub signature: Vec<u8>,
}

/// [`Request::SendPrompt`](crate::proto::Request::SendPrompt) payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SendPromptReq {
    pub session_id: String,
    pub text: String,
    /// Image attachments (base64 in `ChatImage`, so postcard-safe as-is).
    pub images: Vec<ChatImage>,
    /// Client-minted correlation id so the client can match the eventual turn.
    pub corr_id: u64,
}

/// [`Request::ResolvePermission`](crate::proto::Request::ResolvePermission)
/// payload. The decision rides as a `serde_json` string — [`PermissionDecision`]
/// carries `serde_json::Value`, which postcard cannot decode (see the
/// crate-level note).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvePermissionReq {
    pub session_id: String,
    pub request_id: String,
    /// Private so a request can only be built via [`ResolvePermissionReq::new`],
    /// which guarantees the JSON is a real encoded `PermissionDecision` — mirrors
    /// [`HostEvent`]'s encapsulation of `event_json`.
    decision_json: String,
}

impl ResolvePermissionReq {
    /// Encode a decision into its wire form.
    pub fn new(
        session_id: impl Into<String>,
        request_id: impl Into<String>,
        decision: &PermissionDecision,
    ) -> Result<Self, WireError> {
        Ok(Self {
            session_id: session_id.into(),
            request_id: request_id.into(),
            decision_json: serde_json::to_string(decision)?,
        })
    }

    /// Decode the carried decision.
    pub fn decision(&self) -> Result<PermissionDecision, WireError> {
        Ok(serde_json::from_str(&self.decision_json)?)
    }
}

/// [`Request::AnswerQuestion`](crate::proto::Request::AnswerQuestion) payload —
/// the answer to an outstanding `AskUserQuestion`.
///
/// Unlike [`ResolvePermissionReq`] these ride as native postcard rather than a
/// JSON string: every field of `AskQuestion` / `QuestionAnswers` is a plain
/// `String`, `Vec`, `bool` or map, with no `serde_json::Value` anywhere, so the
/// encapsulation that decision payloads need does not apply here.
///
/// `questions` is echoed back by the client rather than looked up host-side, and
/// that is forced rather than chosen: building the backend payload needs the
/// question text (answers are keyed by it), but the questions live in the
/// desktop's `ChatThread`, which the session registry cannot reach. The client
/// is quoting the host's own `QuestionAsked` event back at it, and it is an
/// authenticated paired device that could already send prompts and approve
/// tools, so this widens no boundary that matters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnswerQuestionReq {
    pub session_id: String,
    pub request_id: String,
    pub questions: Vec<AskQuestion>,
    pub answers: QuestionAnswers,
}

/// Coarse per-session status, piggybacked on every [`HostEvent`] so a client can
/// keep its session-list badges live without a separate poll.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatusWire {
    pub last_seq: u64,
    pub awaiting_permission: bool,
}

/// One row of [`Response::Sessions`](crate::proto::Response::Sessions).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub title: String,
    pub model: Option<String>,
    pub last_seq: u64,
    pub awaiting_permission: bool,
}

/// One row of [`Response::Projects`](crate::proto::Response::Projects): a project
/// the host knows, offered as a quick-start target for a new session. `path` is
/// the absolute host path a client passes back to
/// [`Request::CreateSession`](crate::proto::Request::CreateSession); `name` is the
/// display label (typically the folder's final component).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectSummaryWire {
    pub name: String,
    pub path: String,
}

/// A session's folded-transcript snapshot — the
/// [`Response::SessionTranscript`](crate::proto::Response::SessionTranscript)
/// payload.
///
/// `entries_json` is the folded `Vec<ThreadEntry>` as JSON (same reasoning as
/// [`HostEvent`]: the entry tree is deep and evolving, so it crosses as a string
/// rather than a mirrored postcard record). `seq` is the fold cursor the entries
/// reflect — the client resumes the live stream from it so subsequent events
/// extend the snapshot with neither a gap nor a duplicate. An empty transcript is
/// `entries_json: "[]"`, `seq: 0`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTranscriptWire {
    pub session_id: String,
    pub seq: u64,
    pub entries_json: String,
    pub model: Option<String>,
}

/// One page of a folded transcript — the
/// [`Response::TranscriptPage`](crate::proto::Response::TranscriptPage) payload.
///
/// `entries_json` is a JSON **array slice** of the folded `Vec<ThreadEntry>` —
/// the entries `[cursor, next_cursor)` of the full snapshot, each identical to
/// what [`SessionTranscriptWire`] would have carried at that position. The
/// client concatenates page arrays in order to rebuild the full fold.
/// `next_cursor` is the index to request next, `None` on the last page; `total`
/// is the snapshot's full entry count, constant across its pages. `seq` is the
/// fold cursor of the whole snapshot (not the page), so live resume works from
/// any completed paging pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptPageWire {
    pub session_id: String,
    pub seq: u64,
    pub entries_json: String,
    pub next_cursor: Option<u64>,
    pub total: u64,
    pub model: Option<String>,
}

/// One worktree (workspace) row — the
/// [`Response::WorktreeCreated`](crate::proto::Response::WorktreeCreated) /
/// [`Response::Worktrees`](crate::proto::Response::Worktrees) payload.
///
/// `id` is the stable handle
/// [`Request::RemoveWorktree`](crate::proto::Request::RemoveWorktree) takes —
/// removal is by id, never by path. `path` is the worktree's absolute host
/// path, exposed for the same reason [`ProjectSummaryWire::path`] is: it is
/// what a client hands to
/// [`Request::CreateSession`](crate::proto::Request::CreateSession), and the
/// surface is gated on full scope so a confined device never sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeWire {
    pub id: String,
    /// The owning project's root path, matching a
    /// [`ProjectSummaryWire::path`].
    pub project_path: String,
    /// Human label (the desktop's workspace name).
    pub name: String,
    pub slug: String,
    /// The branch the worktree was created on (`oximux/<slug>` for rows the
    /// desktop minted).
    pub branch: String,
    /// Absolute host path of the worktree directory.
    pub path: String,
}

/// A freshly-minted pairing window — the
/// [`Response::PairingIssued`](crate::proto::Response::PairingIssued) payload.
///
/// `ticket` is the [`PairingTicket`](crate::pairing::PairingTicket) in its
/// canonical `base64url` form — the exact string a QR encodes — so the CLI
/// renders it without re-deriving the encoding. A bearer credential with a
/// short life: `expires_at` (unix seconds) is the host's own deadline, and the
/// window is one-time — the first successful registration spends it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingIssuedWire {
    pub ticket: String,
    pub expires_at: u64,
    /// Whether the enrollment this ticket mints is read-only (the opt-down) —
    /// echoed so the operator sees which tier they just offered.
    pub read_only: bool,
}

/// One enrolled device — the
/// [`Response::PairedDeviceList`](crate::proto::Response::PairedDeviceList)
/// payload row. Mirrors what the desktop's paired-devices pane shows, tier
/// included, so a mistaken full-write pairing is visible from the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedDeviceWire {
    pub pubkey: [u8; 32],
    pub name: String,
    pub read_only: bool,
    /// Tombstoned but still listed — erasing it is the only way that device
    /// can ever pair again.
    pub revoked: bool,
    /// Unix seconds of the last successful authentication; `None` for a device
    /// that paired but never reconnected.
    pub last_seen: Option<u64>,
}

/// One terminal the phone can list and attach to.
///
/// `cwd` is the terminal's working directory as a display string, not a path to
/// act on: nothing in the terminal RPCs takes a path, so this never becomes a
/// filesystem-probe surface the way the git `path` argument does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalSummary {
    pub pty_id: String,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
}

/// [`Response::SessionInfo`](crate::proto::Response::SessionInfo) payload — a
/// summary plus the session's advertised inventory ([`SessionMeta`] is already
/// serde + postcard-safe: all `String`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfoWire {
    pub summary: SessionSummary,
    pub meta: SessionMeta,
}

/// A streamed event frame: a `(seq, ThreadEvent)` pair plus the session's coarse
/// status. The `ThreadEvent` is carried as a `serde_json` string — see the
/// crate-level note on why it is not native postcard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostEvent {
    pub session_id: String,
    pub seq: u64,
    pub status: SessionStatusWire,
    event_json: String,
}

impl HostEvent {
    /// Build a frame, encoding the event into its JSON payload.
    pub fn new(
        session_id: impl Into<String>,
        seq: u64,
        event: &ThreadEvent,
        status: SessionStatusWire,
    ) -> Result<Self, WireError> {
        Ok(Self {
            session_id: session_id.into(),
            seq,
            status,
            event_json: serde_json::to_string(event)?,
        })
    }

    /// Decode the carried event.
    pub fn event(&self) -> Result<ThreadEvent, WireError> {
        Ok(serde_json::from_str(&self.event_json)?)
    }
}

/// Index-side (staged) status of a path, mirroring porcelain v2's X column.
/// Re-declared here rather than reusing `oximux_core` so the wire crate stays
/// dependency-minimal (the mobile core links it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexStatusWire {
    Unmodified,
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    Untracked,
    Ignored,
    Unmerged,
}

/// Worktree-side (unstaged) status of a path — porcelain v2's Y column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorktreeStatusWire {
    Unmodified,
    Modified,
    Deleted,
    Renamed,
    Untracked,
    Ignored,
    Unmerged,
}

/// One changed/untracked path in a [`GitStatusWire`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitFileWire {
    /// Repository-relative path, as git reports it. Always relative — a client
    /// echoes this back on a diff request, and the host contains it again there.
    pub path: String,
    pub index: IndexStatusWire,
    pub worktree: WorktreeStatusWire,
    /// Unstaged `(added, removed)` line counts, when git produced them.
    pub unstaged_lines: Option<(u32, u32)>,
    /// Staged `(added, removed)` line counts, when git produced them.
    pub staged_lines: Option<(u32, u32)>,
}

/// [`Response::GitStatus`](crate::proto::Response::GitStatus) payload — the
/// working-tree state of the repository a session lives in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitStatusWire {
    /// Current branch; `None` when HEAD is detached.
    pub branch: Option<String>,
    /// Configured upstream tracking ref, e.g. `origin/main`.
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub files: Vec<GitFileWire>,
}

/// How a path changed, mirroring the desktop's `DiffStatus`. Rename/copy origins
/// cross as strings (the wire crate stays `PathBuf`-free).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffStatusWire {
    Added,
    Modified,
    Deleted,
    Renamed { from: String, similarity: u8 },
    Copied { from: String, similarity: u8 },
    ModeChanged { old_mode: u32, new_mode: u32 },
    Binary,
}

/// One line inside a hunk body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffLineKindWire {
    Context,
    Added,
    Removed,
    /// The `\ No newline at end of file` marker, kept inline next to the line it
    /// qualifies so a renderer can show the hint where it belongs.
    NoNewlineHint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLineWire {
    pub kind: DiffLineKindWire,
    /// Body text minus the leading `+`/`-`/space marker.
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunkWire {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    /// Free text after the second `@@` (function name etc.), verbatim.
    pub header_suffix: String,
    pub lines: Vec<DiffLineWire>,
}

/// One file's diff — the [`Response::GitDiff`](crate::proto::Response::GitDiff)
/// payload is a list of these (a rename can yield more than one).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiffWire {
    pub path: String,
    pub status: DiffStatusWire,
    pub hunks: Vec<DiffHunkWire>,
    /// The rendered line count exceeded the desktop's large-diff threshold, so a
    /// client may want to collapse it. Hunks are still complete — never truncated.
    pub large: bool,
}

/// One issue or pull request, mirroring `oximux_git::gh::ForgeItem`.
///
/// Re-declared here rather than reusing the git crate's type, for the same
/// reason [`IndexStatusWire`] is: this crate is linked into the mobile core, and
/// depending on `oximux-git` would drag the whole git layer — and its CLI
/// shell-outs — into a phone build that can never use them.
///
/// The source type is `Deserialize`-only (it parses forge-CLI JSON), so it could
/// not cross this wire even if the dependency were acceptable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeItemWire {
    pub number: u64,
    pub title: String,
    /// `OPEN` / `CLOSED` / `MERGED`, forwarded verbatim.
    ///
    /// Not narrowed to an enum: the per-provider strings vary more than a fixed
    /// set can absorb, and an unrecognised value would have to become a lossy
    /// "other" — the same reasoning `CheckRun.bucket` follows upstream.
    pub state: String,
    pub url: String,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    /// Login of whoever opened it. Empty when the source omits it (a deleted
    /// account), which renders as no attribution rather than a blank name.
    pub author: String,
    /// RFC-3339 last-update timestamp, or empty when the source omitted it.
    /// Formatted client-side — the host does not know the reader's locale.
    pub updated_at: String,
}

/// One CI check run, mirroring `oximux_git::gh::CheckRun`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRunWire {
    pub name: String,
    /// `pass` / `fail` / `pending` / `skipping` / `cancel`, forwarded verbatim
    /// from the forge CLI's own bucketing rather than re-derived here.
    pub bucket: String,
    pub link: String,
    /// Short human blurb (e.g. "Successful in 2m"). Empty when absent.
    pub description: String,
}

/// Body + author of one issue/PR, mirroring `oximux_git::gh::ItemDetail`.
///
/// Fetched separately from the listing because the body is markdown that can run
/// to kilobytes; sending it for every row would make a 50-item list far more
/// expensive than the list a phone actually renders.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ForgeItemDetailWire {
    pub body: String,
    pub author: String,
}

/// Whether to list issues or pull requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForgeItemKindWire {
    Issue,
    /// A GitHub pull request or GitLab merge request.
    Pull,
}

/// Which items to list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForgeStateWire {
    Open,
    Closed,
    All,
}

/// How often a schedule repeats, mirroring `oximux_agents::schedule::Recurrence`.
///
/// Re-declared here rather than reused for the same reason [`ForgeItemWire`] is:
/// this crate links into the mobile core, and depending on `oximux-agents` would
/// pull the session registry and its process-spawn machinery into a phone build
/// that only needs the three shapes.
///
/// **Not cron.** The desktop deliberately models recurrence as a closed set of
/// three cases so a phone can drive it with pickers that cannot produce an
/// invalid value; this wire type carries that same closed set. The host still
/// re-validates on receipt (an interval under the floor, an impossible time)
/// through the desktop's own constructors — the enum shape prevents *most*
/// nonsense, the constructor prevents the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecurrenceWire {
    /// Every N minutes, measured from the previous fire.
    EveryMinutes { minutes: u32 },
    /// Every day at a wall-clock time.
    DailyAt { hour: u8, minute: u8 },
    /// Every week on one weekday at a wall-clock time. `weekday` is 0=Monday.
    WeeklyAt { weekday: u8, hour: u8, minute: u8 },
}

/// A stored schedule, mirroring `oximux_agents::schedule::Schedule`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleWire {
    pub id: String,
    pub name: String,
    /// Working directory the run's session opens in — a project the desktop
    /// already has, the same containment `CreateSession`'s cwd follows.
    pub cwd: String,
    pub prompt: String,
    pub agent_id: Option<String>,
    pub recurrence: RecurrenceWire,
    pub enabled: bool,
    /// RFC-3339 next-fire instant in the **desktop's** local zone. Formatted
    /// client-side against the reader's own clock — the host does not know the
    /// phone's locale.
    pub next_fire_at: String,
    /// The desktop's own human phrasing of the recurrence (e.g. "Weekdays at
    /// 09:00"). Carried rather than re-derived on the phone so both surfaces read
    /// identically — the same reason the fold runs once, on the desktop.
    pub summary: String,
}

/// One recorded fire, mirroring `oximux_agents::schedule::ScheduleRun`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleRunWire {
    pub schedule_id: String,
    /// RFC-3339 **scheduled** instant, not when the tick noticed it.
    pub fired_at: String,
    pub outcome: RunOutcomeWire,
    /// The session the fire opened, when it opened one. `None` on a failure that
    /// never got that far.
    pub session_id: Option<String>,
    /// A short human note on a failure, absent on success.
    pub detail: Option<String>,
}

/// How one fire turned out, mirroring `oximux_agents::schedule::RunOutcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunOutcomeWire {
    Ok,
    Failed,
}

// ---- v18: automation primitives ----

/// Arm a heartbeat — a recurring wake-up inside a session that already exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateHeartbeatReq {
    /// The session to wake, or `None` for "the one this connection IS".
    ///
    /// `None` is the confined-agent case and the reason this is optional at
    /// all: an agent knows it wants to wake *itself* but does not necessarily
    /// know its own session id (its credential names a handle, not an id). The
    /// host resolves it from the connection's proven scope, which is the only
    /// value it could honestly supply.
    pub session_id: Option<String>,
    /// A name for the wake-up, shown in listings and quoted in the preamble the
    /// agent receives, so a fired heartbeat says which one it was.
    pub name: String,
    /// What to send when it fires. Delivered under a preamble marking it as a
    /// timer rather than a person, so the agent acts instead of replying.
    pub prompt: String,
    pub recurrence: RecurrenceWire,
}

/// One armed heartbeat.
///
/// Its own type rather than a field appended to [`ScheduleWire`]: postcard
/// structs are positional, so appending there would misparse every later
/// element of a `Vec<ScheduleWire>` on any client built before the change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatWire {
    pub id: String,
    /// The session this wakes.
    pub session_id: String,
    pub name: String,
    pub prompt: String,
    pub recurrence: RecurrenceWire,
    pub enabled: bool,
    /// RFC-3339 next-fire instant in the **host's** local zone.
    pub next_fire_at: String,
    /// The host's own human phrasing of the recurrence, carried rather than
    /// re-derived so every surface reads identically.
    pub summary: String,
}

/// Open a team run: one session per role, all in one call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamRunCreateReq {
    /// A name for the run, shown in listings.
    pub name: String,
    /// The project root every role's session opens in (or the base for its
    /// worktree). Validated by the host exactly as `CreateSession`'s cwd is.
    pub cwd: String,
    /// Which configured agent runs every role. `None` = the host's default.
    pub agent_id: Option<String>,
    /// Give each role its own worktree under the project, so roles editing the
    /// same files do not collide. The host derives each path from the run and
    /// role names — never the client.
    pub worktree_each: bool,
    /// The roles, in order. At least one; the host caps how many.
    pub roles: Vec<TeamRoleSpecWire>,
}

/// One role's name and opening instruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamRoleSpecWire {
    pub name: String,
    pub prompt: String,
}

/// A team run and every role in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamRunWire {
    pub id: String,
    pub name: String,
    pub cwd: String,
    /// RFC-3339 creation instant, host-local.
    pub created_at: String,
    /// `true` once every role has reported. Derived host-side so two clients
    /// cannot disagree about whether a run is finished.
    pub closed: bool,
    pub roles: Vec<TeamRoleWire>,
}

/// One role's live state within a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamRoleWire {
    pub name: String,
    /// The session working this role, when one started. `None` means the
    /// session could not be opened — the role's status says why.
    pub session_id: Option<String>,
    pub status: TeamRoleStatusWire,
    /// What the role reported, or why it could not start.
    pub summary: Option<String>,
    /// RFC-3339 instant the role last changed state, host-local.
    pub updated_at: String,
}

/// Where a role stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TeamRoleStatusWire {
    /// Session open, no report yet.
    Running,
    /// The role reported success.
    Done,
    /// The role reported failure, or never started.
    Failed,
}

/// Write one coordination key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateSetReq {
    pub key: String,
    /// The value, as a JSON string. A string rather than a `Value` because the
    /// postcard envelope carries no self-describing types — the same convention
    /// the transcript and event payloads already follow.
    pub value_json: String,
    /// Optimistic concurrency: write only if the stored version is exactly
    /// this. `Some(0)` means "only if absent". `None` overwrites.
    pub if_version: Option<u64>,
}

/// One coordination entry as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateEntryWire {
    pub key: String,
    pub value_json: String,
    /// Incremented on every write. A caller passes the version it read back as
    /// `if_version` to make its next write conditional on nothing having
    /// changed underneath it.
    pub version: u64,
    /// RFC-3339 last-write instant, host-local.
    pub updated_at: String,
}

/// One coordination change, carrying the cursor position that produced it.
///
/// The `seq` is a host-wide counter over *changes*, not the per-key `version`
/// in [`StateEntryWire`]: a watcher needs one ordering across every key it
/// watches, and per-key versions give no way to tell whether something it never
/// saw happened in between.
///
/// It counts from the host's boot, and a restart resets it. That is why a
/// resume answers with a fresh baseline whenever the cursor cannot be honoured
/// rather than trusting the number — a stale cursor from a previous boot would
/// otherwise silently look like a valid recent one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateChangeWire {
    pub seq: u64,
    pub key: String,
    /// `None` is a delete — the key no longer exists.
    pub entry: Option<StateEntryWire>,
}

/// What a cursor-aware `StateWatchFrom` starts a watcher with.
///
/// Exactly one of `baseline`/`replay` carries anything, and which one is the
/// answer to "did I miss something?": a `baseline` means the watcher's cursor
/// could not be honoured and it is being resynced from scratch, a `replay`
/// means the gap was covered exactly. That distinction is the whole point of
/// the cursor — the pre-v19 `StateWatch` returned the board either way, so a
/// watcher could not tell a clean reconnect from a lossy one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateWatchStartedWire {
    /// The cursor to resume from next time: the newest change the host has
    /// issued, whether or not this watcher received it.
    pub seq: u64,
    /// The board as it stands — sent for a fresh watch, and for a resume whose
    /// cursor had aged out of the host's ring (or predates its boot).
    pub baseline: Option<Vec<StateEntryWire>>,
    /// The changes since the caller's cursor, when the ring still covered them.
    /// Empty is a normal answer: nothing happened while the watcher was away.
    pub replay: Vec<StateChangeWire>,
}

/// A role settling its own row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamReportReq {
    pub run_id: String,
    /// Which role is reporting. Named rather than inferred from the calling
    /// session so an operator can settle a role whose agent died — otherwise a
    /// crashed role would hold its run open with nothing able to close it.
    pub role: String,
    /// `true` for done, `false` for failed. A bool rather than the status enum
    /// because `Running` is not something a report can set: a role reporting is
    /// by definition finished.
    pub ok: bool,
    pub summary: Option<String>,
}
