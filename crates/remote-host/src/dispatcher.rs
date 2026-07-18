//! The per-connection RPC dispatcher — transport-agnostic.
//!
//! It owns one connection's auth state, decodes each `Request` frame, and routes
//! it to the [`SessionRegistry`]. It depends only on the `remote-proto`
//! [`Transport`] seam, so the iroh endpoint is one driver and the in-memory
//! loopback drives tests with no network.
//!
//! **Authorization is re-checked on every session RPC**, not just at connect: a
//! device revoked mid-connection (Phase 7) must fail its next call even though
//! its transport is still open. The authenticated handlers live in [`handlers`].

mod handlers;

use std::sync::Arc;

use oximux_agents::session_registry::SessionRegistry;
use oximux_remote_proto::messages::{ConnectReq, RegisterReq};
use oximux_remote_proto::proto::{Request, Response, RpcError};
use oximux_remote_proto::Transport;
use rand::RngCore;
use rand::rngs::OsRng;

use crate::auth::{AppPubkey, AuthStore};

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
    /// Wall clock (Unix seconds), injectable so tests are deterministic.
    now_secs: fn() -> u64,
}

impl Dispatcher {
    pub fn new(registry: Arc<SessionRegistry>, auth: Arc<AuthStore>) -> Self {
        Self { registry, auth, now_secs: system_now_secs }
    }

    /// Override the clock (tests).
    pub fn with_clock(mut self, now_secs: fn() -> u64) -> Self {
        self.now_secs = now_secs;
        self
    }

    /// Serve one connection until the peer closes or a send fails. Each request
    /// yields exactly one response frame.
    pub async fn serve(&self, transport: &dyn Transport) {
        let mut state = ConnAuthn::Unauth;
        loop {
            let frame = match transport.recv().await {
                Ok(Some(frame)) => frame,
                Ok(None) | Err(_) => break,
            };
            let response = match Request::from_bytes(&frame) {
                Ok(req) => self.dispatch(&mut state, req),
                Err(_) => Response::Error(RpcError::BadRequest("undecodable request frame".into())),
            };
            // `Response` carries no `serde_json::Value`, so it always postcard-encodes.
            let bytes = response.to_bytes().expect("response is always encodable");
            if transport.send(bytes).await.is_err() {
                break;
            }
        }
    }

    /// Route one request against the current connection state. Synchronous — the
    /// registry commands are non-blocking; only the transport I/O in `serve` is
    /// async.
    fn dispatch(&self, state: &mut ConnAuthn, req: Request) -> Response {
        match req {
            Request::Ping => Response::Pong,
            Request::Register(r) => self.handle_register(state, r),
            Request::Connect(c) => self.handle_connect(state, c),
            Request::AuthProve(a) => self.handle_auth_prove(state, &a.signature),
            // Everything below requires an authenticated, still-authorized device.
            other => match authorized_pubkey(state, &self.auth) {
                Some(pubkey) => self.handle_session_rpc(&pubkey, other),
                None => Response::Error(RpcError::Unauthorized),
            },
        }
    }

    fn handle_register(&self, state: &mut ConnAuthn, req: RegisterReq) -> Response {
        match self.auth.register(&req, (self.now_secs)()) {
            Ok(token) => {
                *state = ConnAuthn::Authed { app_pubkey: req.app_pubkey };
                Response::Registered { session_token: token }
            }
            Err(e) => Response::Error(e),
        }
    }

    fn handle_connect(&self, state: &mut ConnAuthn, req: ConnectReq) -> Response {
        if let Some(token) = req.session_token.as_deref()
            && let Some(pubkey) = self.auth.authorize_token(token)
        {
            *state = ConnAuthn::Authed { app_pubkey: pubkey };
            return Response::Connected { session_token: token.to_string() };
        }
        // No token / invalid token → challenge the app key.
        let mut nonce = [0u8; 32];
        OsRng.fill_bytes(&mut nonce);
        *state = ConnAuthn::PendingChallenge { app_pubkey: req.app_pubkey, nonce };
        Response::Challenge { nonce }
    }

    fn handle_auth_prove(&self, state: &mut ConnAuthn, signature: &[u8]) -> Response {
        let ConnAuthn::PendingChallenge { app_pubkey, nonce } = state else {
            return Response::Error(RpcError::BadRequest("no challenge outstanding".into()));
        };
        let (app_pubkey, nonce) = (*app_pubkey, *nonce);
        match self.auth.verify_challenge(&app_pubkey, &nonce, signature) {
            Ok(token) => {
                *state = ConnAuthn::Authed { app_pubkey };
                Response::Connected { session_token: token }
            }
            Err(e) => Response::Error(e),
        }
    }

    /// Route an authenticated request to its handler (in [`handlers`]).
    fn handle_session_rpc(&self, pubkey: &AppPubkey, req: Request) -> Response {
        match req {
            Request::ListSessions => self.list_sessions(pubkey),
            Request::GetSessionInfo { session_id } => self.session_info(pubkey, &session_id),
            Request::SendPrompt(r) => self.send_prompt(pubkey, r),
            Request::ResolvePermission(r) => self.resolve_permission(pubkey, r),
            Request::Steer { session_id, text } => {
                self.scoped(pubkey, &session_id, |h| h.steer(&text))
            }
            Request::Cancel { session_id } => self.scoped(pubkey, &session_id, |h| h.cancel()),
            Request::EventsSince { session_id, after_seq } => {
                self.events_since(pubkey, &session_id, after_seq)
            }
            // Live streaming keeps the connection open and pushes many frames — it
            // needs the runtime transport layer (iroh slice). Until then, reply
            // with the same backlog replay `EventsSince` gives, so a client can at
            // least catch up.
            Request::Subscribe { session_id, after_seq } => {
                self.events_since(pubkey, &session_id, after_seq.unwrap_or(0))
            }
            // Handshake variants never reach here (handled before auth).
            _ => Response::Error(RpcError::BadRequest("unexpected request".into())),
        }
    }
}

/// The device this connection is authenticated as, if it is still authorized.
/// Returns `None` for an unauthenticated OR revoked connection — the per-RPC
/// revocation gate.
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
