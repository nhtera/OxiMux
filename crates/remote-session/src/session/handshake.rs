//! The client half of the auth handshake: `Register` (first-time pairing) and
//! `Connect`→(`Challenge`→`AuthProve`) reconnect. Each caches the reconnect token
//! the host issues. The transport plumbing (`call`) + token cache live on
//! [`RemoteSession`] in the parent module.

use oximux_remote_proto::messages::{ConnectReq, RegisterReq};
use oximux_remote_proto::pairing::PairingTicket;
use oximux_remote_proto::proto::{Request, Response};
use oximux_remote_proto::{AuthProveReq, registration_proof};

use super::{RemoteSession, Result};
use crate::error::SessionError;

impl RemoteSession {
    /// First-time pairing: prove possession of the QR's `handshake_secret` and, on
    /// success, cache the reconnect token. `now_secs` is the client's Unix clock —
    /// the host accepts it within a ±skew window.
    pub async fn pair(&self, ticket: &PairingTicket, device_name: &str, now_secs: u64) -> Result<()> {
        let app_pubkey = self.signer.public_key();
        let proof = registration_proof(&ticket.handshake_secret, &app_pubkey, now_secs);
        let req = Request::Register(RegisterReq {
            app_pubkey,
            device_name: device_name.to_string(),
            proof,
            timestamp_secs: now_secs,
            session_id: ticket.session_id.clone(),
        });
        match self.call(req).await? {
            Response::Registered { session_token } => {
                self.cache(session_token);
                Ok(())
            }
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "Registered" }),
        }
    }

    /// Reconnect: try the cached token fast path; if absent/rejected the host
    /// challenges, and we sign the nonce with the app key.
    pub async fn connect(&self) -> Result<()> {
        let app_pubkey = self.signer.public_key();
        let session_token = self.token.lock().unwrap().clone();
        match self.call(Request::Connect(ConnectReq { app_pubkey, session_token })).await? {
            Response::Connected { session_token } => {
                self.cache(session_token);
                Ok(())
            }
            Response::Challenge { nonce } => self.answer_challenge(&nonce).await,
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "Connected or Challenge" }),
        }
    }

    async fn answer_challenge(&self, nonce: &[u8; 32]) -> Result<()> {
        let signature = self.signer.sign(nonce).to_vec();
        match self.call(Request::AuthProve(AuthProveReq { signature })).await? {
            Response::Connected { session_token } => {
                self.cache(session_token);
                Ok(())
            }
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "Connected" }),
        }
    }
}
