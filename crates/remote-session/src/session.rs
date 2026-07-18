//! The client session: the Register/Connect/AuthProve handshake and the one-shot
//! RPCs, driven over the `remote-proto` [`Transport`] seam. Pure Rust — the iroh
//! transport is injected as one `Transport` impl, and the in-memory loopback
//! drives the full-loop test against the real host dispatcher.
//!
//! Every call is strict request→response (`call`); the live subscription consumer
//! (folding `HostEvent`s into a `ChatThread`) and the reconnect state machine are
//! later slices layered on this.

mod handshake;

use std::sync::{Arc, Mutex};

use oximux_agent_core::thread::{ChatImage, PermissionDecision};
use oximux_remote_proto::messages::{
    ResolvePermissionReq, SendPromptReq, SessionInfoWire, SessionSummary,
};
use oximux_remote_proto::proto::{Request, Response, RpcError};
use oximux_remote_proto::{HostEvent, Transport};

use crate::error::SessionError;
use crate::signer::ClientSigner;

type Result<T> = std::result::Result<T, SessionError>;

/// One client's connection to a host, over an abstract [`Transport`]. Owns the
/// app-signing identity and caches the reconnect token.
pub struct RemoteSession {
    transport: Arc<dyn Transport>,
    signer: ClientSigner,
    /// The reconnect credential the host issues on Register/Connect. Cached in
    /// memory only — `mobile-core` may persist it, but losing it just forces the
    /// slower Ed25519 challenge on the next `connect`.
    token: Mutex<Option<String>>,
}

impl RemoteSession {
    pub fn new(transport: Arc<dyn Transport>, signer: ClientSigner) -> Self {
        Self { transport, signer, token: Mutex::new(None) }
    }

    /// The app-signing public key the host records for this device.
    pub fn public_key(&self) -> [u8; 32] {
        self.signer.public_key()
    }

    /// The cached reconnect token, if any (for the caller to persist).
    pub fn session_token(&self) -> Option<String> {
        self.token.lock().unwrap().clone()
    }

    /// Seed a persisted reconnect token so the next [`Self::connect`] can take the
    /// fast path.
    pub fn set_session_token(&self, token: Option<String>) {
        *self.token.lock().unwrap() = token;
    }

    // ---- one-shot session RPCs (the handshake lives in `handshake.rs`) ----

    /// Every session the host exposes to this device.
    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        match self.call(Request::ListSessions).await? {
            Response::Sessions(sessions) => Ok(sessions),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "Sessions" }),
        }
    }

    /// One session's detail + resume cursor.
    pub async fn session_info(&self, session_id: &str) -> Result<SessionInfoWire> {
        let req = Request::GetSessionInfo { session_id: session_id.to_string() };
        match self.call(req).await? {
            Response::SessionInfo(info) => Ok(info),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "SessionInfo" }),
        }
    }

    /// Send a user prompt into a session, starting a turn. `corr_id` lets the
    /// caller match the eventual echoed turn in the event stream.
    pub async fn send_prompt(
        &self,
        session_id: &str,
        text: &str,
        images: &[ChatImage],
        corr_id: u64,
    ) -> Result<()> {
        let req = Request::SendPrompt(SendPromptReq {
            session_id: session_id.to_string(),
            text: text.to_string(),
            images: images.to_vec(),
            corr_id,
        });
        self.expect_ack(req).await
    }

    /// Answer an outstanding permission request. `Ok(true)` = this call decided it;
    /// `Ok(false)` = it was already decided (idempotent — still a success).
    pub async fn resolve_permission(
        &self,
        session_id: &str,
        request_id: &str,
        decision: &PermissionDecision,
    ) -> Result<bool> {
        let payload = ResolvePermissionReq::new(session_id, request_id, decision)
            .map_err(|e| SessionError::Wire(e.to_string()))?;
        match self.call(Request::ResolvePermission(payload)).await? {
            Response::Ack => Ok(true),
            Response::Error(RpcError::AlreadyDecided) => Ok(false),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "Ack" }),
        }
    }

    /// Steer a mid-turn agent with extra guidance.
    pub async fn steer(&self, session_id: &str, text: &str) -> Result<()> {
        let req = Request::Steer { session_id: session_id.to_string(), text: text.to_string() };
        self.expect_ack(req).await
    }

    /// Cancel the session's in-flight turn.
    pub async fn cancel(&self, session_id: &str) -> Result<()> {
        self.expect_ack(Request::Cancel { session_id: session_id.to_string() }).await
    }

    /// Gap-fill: replay retained events after `after_seq` (the resync path when the
    /// live stream reports a `seq` jump).
    pub async fn events_since(&self, session_id: &str, after_seq: u64) -> Result<Vec<HostEvent>> {
        let req = Request::EventsSince { session_id: session_id.to_string(), after_seq };
        match self.call(req).await? {
            Response::Events(events) => Ok(events),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "Events" }),
        }
    }

    // ---- plumbing ----

    fn cache(&self, token: String) {
        *self.token.lock().unwrap() = Some(token);
    }

    /// A command whose only successful reply is `Ack`.
    async fn expect_ack(&self, req: Request) -> Result<()> {
        match self.call(req).await? {
            Response::Ack => Ok(()),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "Ack" }),
        }
    }

    /// Send one request frame and read exactly one response frame.
    ///
    /// Assumes strict one-request→one-response ordering on the transport. This
    /// holds today because this crate never subscribes — but the host pushes
    /// unsolicited [`Response::Event`] frames onto the *same* connection once a
    /// `Subscribe` is active. The future live-subscription consumer must therefore
    /// demux `Response::Event` off a single read loop (or a dedicated live
    /// channel) BEFORE routing replies here; it must NOT reuse this `call` while
    /// subscribed, or an `Event` frame would be mis-read as an RPC reply and shift
    /// the request/response alignment permanently.
    async fn call(&self, req: Request) -> Result<Response> {
        let bytes = req.to_bytes().map_err(|e| SessionError::Wire(e.to_string()))?;
        self.transport.send(bytes).await.map_err(|e| SessionError::Transport(e.to_string()))?;
        match self.transport.recv().await.map_err(|e| SessionError::Transport(e.to_string()))? {
            Some(frame) => {
                Response::from_bytes(&frame).map_err(|e| SessionError::Wire(e.to_string()))
            }
            None => Err(SessionError::Closed),
        }
    }
}
