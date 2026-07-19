//! The pre-auth handshake handlers: `Register` (first-time pairing), `Connect`
//! (token fast path or challenge), and `AuthProve` (Ed25519 nonce signature).
//! Each transitions the connection's [`ConnAuthn`] state; the authenticated
//! session RPCs live in [`super::handlers`].

use oximux_remote_proto::messages::{ConnectReq, HelloAckWire, HelloReq, RegisterReq};
use oximux_remote_proto::proto::{
    MIN_COMPATIBLE_VERSION, PROTOCOL_VERSION, Response, RpcError, is_compatible,
};
use rand::RngCore;
use rand::rngs::OsRng;

use super::{ConnAuthn, ConnState, Dispatcher};

impl Dispatcher {
    /// Record the client's declared version and answer with this host's range.
    ///
    /// An incompatible client is told so **explicitly** rather than having its
    /// connection dropped: a bare disconnect is indistinguishable from a network
    /// failure, and would send someone debugging their wifi when the real fix is
    /// updating the app. The reply still carries the host's numbers so the client
    /// can say which side is behind.
    pub(super) fn handle_hello(&self, state: &mut ConnState, req: HelloReq) -> Response {
        state.peer_version = req.protocol_version;
        if !is_compatible(req.protocol_version) {
            return Response::Error(RpcError::IncompatibleVersion {
                host_version: PROTOCOL_VERSION,
                host_min_compatible: MIN_COMPATIBLE_VERSION,
            });
        }
        Response::HelloAck(HelloAckWire {
            protocol_version: PROTOCOL_VERSION,
            min_compatible: MIN_COMPATIBLE_VERSION,
        })
    }

    pub(super) fn handle_register(&self, state: &mut ConnAuthn, req: RegisterReq) -> Response {
        match self.auth.register(&req, (self.now_secs)()) {
            Ok(token) => {
                *state = ConnAuthn::Authed { app_pubkey: req.app_pubkey };
                Response::Registered { session_token: token }
            }
            Err(e) => Response::Error(e),
        }
    }

    pub(super) fn handle_connect(&self, state: &mut ConnAuthn, req: ConnectReq) -> Response {
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

    pub(super) fn handle_auth_prove(&self, state: &mut ConnAuthn, signature: &[u8]) -> Response {
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
}
