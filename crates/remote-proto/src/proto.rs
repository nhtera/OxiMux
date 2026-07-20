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
/// appended `AnswerQuestion`.
///
/// Appending variants is *not* a breaking change — postcard ordinals of the
/// existing ones are untouched, and an older peer simply never sends or receives
/// the new calls. So this bumps while the transport ALPN
/// (`remote_iroh::OXIMUX_ALPN`) deliberately does not: that tracks breaking
/// changes only, and bumping it would refuse otherwise-compatible peers.
pub const PROTOCOL_VERSION: u32 = 5;

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
