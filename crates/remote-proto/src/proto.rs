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
/// subscription & gap-fill).
pub const PROTOCOL_VERSION: u32 = 1;

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
    /// Reply to [`Request::EventsSince`] — the replayed backlog.
    Events(Vec<HostEvent>),
    /// The request failed at the protocol level.
    Error(RpcError),
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
