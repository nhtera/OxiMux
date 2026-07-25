//! The client half of the auth handshake: `Register` (first-time pairing) and
//! `Connect`→(`Challenge`→`AuthProve`) reconnect. Each caches the reconnect token
//! the host issues. The transport plumbing (`call`) + token cache live on
//! [`RemoteSession`] in the parent module.

use oximux_remote_proto::messages::{ConnectReq, HelloReq, RegisterReq};
use oximux_remote_proto::pairing::PairingTicket;
use oximux_remote_proto::proto::{
    ASSUMED_VERSION_WHEN_SILENT, PROTOCOL_VERSION, Request, Response, RpcError, is_compatible,
};
use oximux_remote_proto::{AuthProveReq, registration_proof};

use super::{RemoteSession, Result};
use crate::error::SessionError;

impl RemoteSession {
    /// Exchange protocol versions before offering any credential, so an
    /// unusable pairing fails with "update the app" rather than a mystery
    /// rejection three RPCs later.
    ///
    /// **A host that errors on `Hello` is treated as a pre-handshake host, not as
    /// a failure.** A v1/v2 host cannot even decode this variant — it answers
    /// `BadRequest("undecodable request frame")` — and refusing to talk to it
    /// would mean this compatibility feature was itself the thing that broke
    /// compatibility. The one error that *is* fatal is an explicit
    /// [`RpcError::IncompatibleVersion`]: that is a host which understood us and
    /// said no.
    async fn hello(&self) -> Result<()> {
        let req = Request::Hello(HelloReq { protocol_version: PROTOCOL_VERSION });
        let host_version = match self.call(req).await? {
            Response::HelloAck(ack) => {
                // The host's floor governs whether it will serve us; its version
                // governs whether we can understand it.
                if PROTOCOL_VERSION < ack.min_compatible {
                    return Err(SessionError::IncompatibleVersion {
                        ours: PROTOCOL_VERSION,
                        theirs: ack.protocol_version,
                    });
                }
                ack.protocol_version
            }
            Response::Error(e @ RpcError::IncompatibleVersion { .. }) => {
                return Err(SessionError::Rpc(e));
            }
            // Any other error, or an off-contract reply: a host too old to know
            // `Hello`. Read its silence as v1 and keep going.
            _ => ASSUMED_VERSION_WHEN_SILENT,
        };
        if !is_compatible(host_version) {
            return Err(SessionError::IncompatibleVersion {
                ours: PROTOCOL_VERSION,
                theirs: host_version,
            });
        }
        Ok(())
    }

    /// First-time pairing: prove possession of the QR's `handshake_secret` and, on
    /// success, cache the reconnect token. `now_secs` is the client's Unix clock —
    /// the host accepts it within a ±skew window.
    pub async fn pair(&self, ticket: &PairingTicket, device_name: &str, now_secs: u64) -> Result<()> {
        self.hello().await?;
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
        self.hello().await?;
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
