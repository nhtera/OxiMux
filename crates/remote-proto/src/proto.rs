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
/// the `Choices` reply).
///
/// Appending variants is *not* a breaking change — postcard ordinals of the
/// existing ones are untouched, and an older peer simply never sends or receives
/// the new calls. So this bumps while the transport ALPN
/// (`remote_iroh::OXIMUX_ALPN`) deliberately does not: that tracks breaking
/// changes only, and bumping it would refuse otherwise-compatible peers.
pub const PROTOCOL_VERSION: u32 = 7;

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
/// (`claude-opus-4-8` against "Opus 4.8"), and `description` carries the
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
