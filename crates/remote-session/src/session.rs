//! The client session: the Register/Connect/AuthProve handshake and the one-shot
//! RPCs, driven over the `remote-proto` [`Transport`] seam. Pure Rust — the iroh
//! transport is injected as one `Transport` impl, and the in-memory loopback
//! drives the full-loop test against the real host dispatcher.
//!
//! Every RPC rides the concurrent [`demux`](crate::demux) pump, so a client can
//! issue requests *while* subscribed to a live session on the same connection: the
//! pump routes pushed `HostEvent`s to the event stream ([`Self::take_events`]) and
//! each reply to its waiting caller. The reconnect state machine is a later slice.

mod git;
mod handshake;
mod subscribe;
mod terminals;

use std::sync::{Arc, Mutex};

use futures::channel::oneshot;
use oximux_agent_core::thread::{AskQuestion, ChatImage, PermissionDecision, QuestionAnswers};
use oximux_remote_proto::messages::{
    AnswerQuestionReq, ResolvePermissionReq, SendPromptReq, SessionInfoWire, SessionSummary,
};
use oximux_remote_proto::proto::{Request, Response, RpcError, SessionChoices};
use oximux_remote_proto::{HostEvent, Transport};

use crate::demux::{Demux, DemuxPump, EventStream, TerminalStream, demux};
use crate::error::SessionError;
use crate::signer::ClientSigner;

type Result<T> = std::result::Result<T, SessionError>;

/// One client's connection to a host, over an abstract [`Transport`]. Owns the
/// app-signing identity, the demux RPC handle, and caches the reconnect token.
pub struct RemoteSession {
    demux: Arc<Demux>,
    signer: ClientSigner,
    /// The reconnect credential the host issues on Register/Connect. Cached in
    /// memory only — `mobile-core` may persist it, but losing it just forces the
    /// slower Ed25519 challenge on the next `connect`.
    token: Mutex<Option<String>>,
    /// The read-loop pump, taken once by the owner to drive (spawned in prod,
    /// joined in tests). Every RPC is dead in the water until it runs.
    pump: Mutex<Option<DemuxPump>>,
    /// The live event stream, taken once by the owner to consume.
    events: Mutex<Option<EventStream>>,
    /// The live terminal stream, taken once by the owner to consume. Separate
    /// from `events` because a client can watch a terminal without subscribing
    /// to any agent session, and vice versa.
    terminals: Mutex<Option<TerminalStream>>,
    /// Dropping this stops the pump — so the connection tears down when the
    /// session is dropped, no explicit close needed.
    _shutdown: oneshot::Sender<()>,
}

impl RemoteSession {
    pub fn new(transport: Arc<dyn Transport>, signer: ClientSigner) -> Self {
        let (handle, pump, events, terminals, shutdown) = demux(transport);
        Self {
            demux: handle,
            signer,
            token: Mutex::new(None),
            pump: Mutex::new(Some(pump)),
            events: Mutex::new(Some(events)),
            terminals: Mutex::new(Some(terminals)),
            _shutdown: shutdown,
        }
    }

    /// Take the read-loop pump to drive it — once. Spawn `pump.run()` onto an
    /// executor (prod) or join it (tests); RPCs only resolve while it runs.
    pub fn take_pump(&self) -> Option<DemuxPump> {
        self.pump.lock().unwrap().take()
    }

    /// Take the live event stream — once. Each pushed `HostEvent` is folded by a
    /// [`SessionSubscription`](crate::SessionSubscription).
    pub fn take_terminals(&self) -> Option<TerminalStream> {
        self.terminals.lock().unwrap().take()
    }

    /// Take the live event stream — once. Each pushed `HostEvent` is folded by a
    /// [`SessionSubscription`](crate::SessionSubscription).
    pub fn take_events(&self) -> Option<EventStream> {
        self.events.lock().unwrap().take()
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

    /// Answer an outstanding `AskUserQuestion`. Same idempotency contract as
    /// [`Self::resolve_permission`]: `Ok(false)` means someone answered first.
    pub async fn answer_question(
        &self,
        session_id: &str,
        request_id: &str,
        questions: &[AskQuestion],
        answers: &QuestionAnswers,
    ) -> Result<bool> {
        let payload = AnswerQuestionReq {
            session_id: session_id.to_string(),
            request_id: request_id.to_string(),
            questions: questions.to_vec(),
            answers: answers.clone(),
        };
        match self.call(Request::AnswerQuestion(payload)).await? {
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

    /// The models and permission modes this session's backend offers.
    ///
    /// Empty lists mean "nothing to choose from" — a dynamic-catalog backend
    /// before its handshake completes, or an agent with no mode options — not a
    /// failure.
    pub async fn list_choices(&self, session_id: &str) -> Result<SessionChoices> {
        let req = Request::ListChoices { session_id: session_id.to_string() };
        match self.call(req).await? {
            Response::Choices(choices) => Ok(choices),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "Choices" }),
        }
    }

    /// Switch the session's model.
    ///
    /// Errors when the backend fixes its model at spawn time; the host answers
    /// with a message saying so rather than accepting silently, so this is worth
    /// surfacing to the user instead of swallowing.
    pub async fn set_model(&self, session_id: &str, model: &str) -> Result<()> {
        let req = Request::SetModel {
            session_id: session_id.to_string(),
            model: model.to_string(),
        };
        self.expect_ack(req).await
    }

    /// Switch the session's permission mode. Same fix-at-spawn caveat as
    /// [`Self::set_model`].
    pub async fn set_permission_mode(&self, session_id: &str, mode: &str) -> Result<()> {
        let req = Request::SetPermissionMode {
            session_id: session_id.to_string(),
            mode: mode.to_string(),
        };
        self.expect_ack(req).await
    }

    /// Start a new agent session on the desktop, returning its id.
    ///
    /// The id comes back rather than being discovered by re-listing, so the
    /// caller can subscribe to the new session immediately instead of polling
    /// and guessing which row appeared.
    pub async fn create_session(&self, cwd: &str, agent_id: Option<&str>) -> Result<String> {
        let req = Request::CreateSession {
            cwd: cwd.to_string(),
            agent_id: agent_id.map(str::to_string),
        };
        match self.call(req).await? {
            Response::SessionCreated { session_id } => Ok(session_id),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "SessionCreated" }),
        }
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

    /// Send one request and await its reply. Rides the [`demux`](crate::demux)
    /// pump, which routes pushed `Response::Event` frames off to the event stream
    /// and this reply back here — so an RPC is safe to issue even while a live
    /// subscription is streaming on the same connection.
    async fn call(&self, req: Request) -> Result<Response> {
        self.demux.call(req).await
    }
}
