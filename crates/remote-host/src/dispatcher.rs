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

mod handlers;
mod handshake;
mod serve;
mod stream;

use std::sync::Arc;

use oximux_agents::session_registry::SessionRegistry;
use oximux_remote_proto::proto::{Request, Response, RpcError};

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

    /// Route one non-`Subscribe` request against the current connection state.
    /// Synchronous — the registry commands are non-blocking; only the transport
    /// I/O in [`serve`] is async.
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

    /// Route an authenticated request to its handler (in [`handlers`]). `Subscribe`
    /// never reaches here — the serve loop intercepts it to open the live stream.
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
            // Handshake variants (handled before auth) and `Subscribe` (intercepted
            // in `serve`) never reach here.
            _ => Response::Error(RpcError::BadRequest("unexpected request".into())),
        }
    }
}

/// The device this connection is authenticated as, if it is still authorized.
/// Returns `None` for an unauthenticated OR revoked connection — the per-RPC (and
/// per-live-frame) revocation gate. `pub(super)` so the serve loop shares it.
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
