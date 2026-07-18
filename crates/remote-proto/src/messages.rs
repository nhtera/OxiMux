//! Payload structs carried by the [`Request`](crate::proto::Request) /
//! [`Response`](crate::proto::Response) envelope, plus the streamed
//! [`HostEvent`] frame.
//!
//! `ThreadEvent` and `PermissionDecision` cross the wire only as `serde_json`
//! strings ([`HostEvent::event_json`], [`ResolvePermissionReq::decision_json`]),
//! both private so a payload can only be built through the encoding constructor
//! — see the crate-level note for why they are not native postcard.

use oximux_agent_core::thread::{ChatImage, PermissionDecision, SessionMeta, ThreadEvent};
use serde::{Deserialize, Serialize};

use crate::proto::WireError;

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
