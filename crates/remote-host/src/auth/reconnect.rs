//! Reconnect: the `session_token` fast path and the Ed25519 challenge.

use ed25519_dalek::{Signature, VerifyingKey};
use oximux_remote_proto::RpcError;

use super::{AppPubkey, AuthStore, issue_token};

impl AuthStore {
    /// Reconnect fast path: exchange a valid `session_token` for the device it
    /// belongs to (still-authorized only). Returns `None` to fall back to the
    /// challenge flow.
    pub fn authorize_token(&self, token: &str) -> Option<AppPubkey> {
        let st = self.inner.lock().unwrap();
        let pubkey = *st.tokens.get(token)?;
        st.devices.get(&pubkey).filter(|d| !d.revoked).map(|_| pubkey)
    }

    /// Verify an Ed25519 challenge answer, and on success mint a fresh token.
    pub fn verify_challenge(
        &self,
        pubkey: &AppPubkey,
        nonce: &[u8; 32],
        signature: &[u8],
    ) -> Result<String, RpcError> {
        let mut st = self.inner.lock().unwrap();
        if st.devices.get(pubkey).is_none_or(|d| d.revoked) {
            return Err(RpcError::Unauthorized);
        }
        let vk = VerifyingKey::from_bytes(pubkey).map_err(|_| RpcError::Unauthorized)?;
        let sig = Signature::from_slice(signature).map_err(|_| RpcError::BadRequest("bad signature".into()))?;
        vk.verify_strict(nonce, &sig).map_err(|_| RpcError::Unauthorized)?;
        Ok(issue_token(&mut st, *pubkey))
    }
}
