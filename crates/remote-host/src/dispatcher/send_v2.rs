//! `SendV2` (v37): a prompt sent at most once per idempotency key.
//!
//! The key is reserved in the host's receipts (as a hash) before the prompt is
//! written or queued; a key seen before sends nothing and answers where the
//! first send stands. The prompt goes out under a uuid derived from the
//! key alone (it survives the session moving to its agent's id), so the
//! agent's acceptance of it is recorded (by whichever view drives the
//! session) under that uuid: the key's row is the reservation,
//! the uuid's row is the proof.

use oximux_agents::send_receipts::{KeyKind, ReceiptState, key_hash};
use oximux_remote_proto::delivery::{MAX_IDEMPOTENCY_KEY_BYTES, ReceiptStateWire, SendOutcomeWire, SendV2Req};
use oximux_remote_proto::proto::{Response, RpcError};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::Dispatcher;
use crate::auth::Peer;

impl Dispatcher {
    pub(super) fn send_v2(&self, peer: &Peer, req: SendV2Req) -> Response {
        let key = req.idempotency_key.as_str();
        if key.trim().is_empty() || key.len() > MAX_IDEMPOTENCY_KEY_BYTES {
            return Response::Error(RpcError::BadRequest(format!(
                "idempotency_key must be 1 to {MAX_IDEMPOTENCY_KEY_BYTES} bytes"
            )));
        }
        if req.text.trim().is_empty() && req.images.is_empty() {
            return Response::Error(RpcError::BadRequest("an empty prompt".into()));
        }
        // Authorized before the key is looked at, so a peer cannot probe
        // another session's keys.
        if !self.may_drive_session(peer, &req.session_id) {
            return Response::Error(RpcError::Unauthorized);
        }
        if let Some(refused) = self.refuse_ended_helper(peer, &req.session_id) {
            return refused;
        }
        // The receipts live with the host core; without one there is nothing
        // to keep the promise with (a caller falls back to `SendPrompt`).
        let Some(core) = self.host_core.as_ref().filter(|core| core.is_owner()) else {
            return Response::Error(RpcError::Unsupported);
        };
        let Some(handle) = self.registry.get(&req.session_id) else {
            return Response::Error(RpcError::UnknownSession);
        };
        let source = if peer.own_session().is_some() {
            oximux_agents::send_gate::SendSource::Agent
        } else {
            oximux_agents::send_gate::SendSource::Remote
        };
        let receipts = core.lineage().receipts();
        let (session, reserved) = (req.session_id.as_str(), key_hash(KeyKind::V2, key));
        let uuid = send_uuid(key);
        match receipts.reserve(session, &reserved, core.instance(), chrono::Local::now()) {
            Err(err) => {
                tracing::warn!(%err, session, "send receipt reserve failed");
                Response::Error(RpcError::Internal("send receipt failed".into()))
            }
            Ok(Some(state)) => {
                let line = key_hash(KeyKind::Gate, &uuid.to_string());
                let mut state = receipts.state(session, &line).ok().flatten().unwrap_or(state);
                // Refused, but the host's own retry will send it again: to the
                // caller it is still on its way, not something to resend.
                let retrying = || core.lineage().retries().live(session).ok().flatten().is_some_and(|r| r.turn_key == line);
                if state == ReceiptState::Rejected && retrying() {
                    state = ReceiptState::Pending;
                }
                Response::SendOutcome(SendOutcomeWire::Duplicate(wire(state)))
            }
            Ok(None) => match handle.send_prompt_tagged(&req.text, &req.images, source, Some(uuid)) {
                Ok(()) => Response::SendOutcome(SendOutcomeWire::Queued),
                Err(err) => {
                    // Nothing reached the agent: the same key may try again.
                    let _ = receipts.forget_pending(session, &reserved);
                    tracing::warn!(error = %err, session, "send_v2 failed");
                    Response::Error(RpcError::Internal("session command failed".into()))
                }
            },
        }
    }
}

/// The uuid a keyed send goes out under: the same for every retry of the key,
/// and unchanged when the session moves to its agent's id.
fn send_uuid(key: &str) -> Uuid {
    let digest = Sha256::digest(format!("send-v2\0{key}").as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    uuid::Builder::from_random_bytes(bytes).into_uuid()
}

fn wire(state: ReceiptState) -> ReceiptStateWire {
    match state {
        ReceiptState::Pending => ReceiptStateWire::Pending,
        ReceiptState::Accepted => ReceiptStateWire::Accepted,
        ReceiptState::Rejected => ReceiptStateWire::Rejected,
        ReceiptState::Unknown => ReceiptStateWire::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_maps_to_one_uuid() {
        assert_eq!(send_uuid("k"), send_uuid("k"));
        assert_ne!(send_uuid("k"), send_uuid("k2"));
    }
}
