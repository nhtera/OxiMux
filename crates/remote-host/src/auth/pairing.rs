//! First-time pairing: the `Register` HMAC proof path.

use std::collections::HashSet;

use ed25519_dalek::VerifyingKey;
use hmac::Mac;
use oximux_remote_proto::RpcError;
use oximux_remote_proto::messages::RegisterReq;

use super::{
    AuthStore, DeviceRecord, DeviceScope, HmacSha256, REGISTRATION_WINDOW_SECS, StoredDevice,
    issue_token,
};

impl AuthStore {
    /// Verify a `Register` proof and, on success, authorize the device. Returns a
    /// fresh reconnect token.
    pub fn register(&self, req: &RegisterReq, now_secs: u64) -> Result<String, RpcError> {
        // Do all in-memory work under the lock, then write through to durable
        // storage OUTSIDE it (the store may block on I/O).
        let (token, stored) = {
            let mut st = self.inner.lock().unwrap();
            let slot = st.pairing.as_ref().ok_or(RpcError::Unauthorized)?;
            if slot.one_time && slot.used {
                return Err(RpcError::Unauthorized);
            }
            if now_secs.abs_diff(req.timestamp_secs) > REGISTRATION_WINDOW_SECS {
                return Err(RpcError::BadRequest("registration timestamp outside window".into()));
            }
            // Constant-time verify against the canonical proof.
            let mut mac =
                HmacSha256::new_from_slice(&slot.secret).expect("HMAC accepts any key length");
            mac.update(&req.app_pubkey);
            mac.update(&req.timestamp_secs.to_le_bytes());
            mac.verify_slice(&req.proof).map_err(|_| RpcError::Unauthorized)?;

            // Reject a structurally invalid pubkey now, not at first use.
            VerifyingKey::from_bytes(&req.app_pubkey).map_err(|_| RpcError::Unauthorized)?;

            // A device that is already known must NOT re-pair over the wire — proof
            // of the pairing secret alone can't resurrect a *revoked* device (that
            // requires an explicit host-side un-revoke), nor silently rewrite an
            // active device's scope/name. An already-paired device reconnects via
            // `Connect`, not `Register`.
            if st.devices.contains_key(&req.app_pubkey) {
                return Err(RpcError::Unauthorized);
            }

            // A session-bound ticket scopes the DEVICE to that one session; a
            // static/global ticket grants full access.
            let bound = slot.session_id.clone();
            let scope = match &bound {
                Some(session_id) => DeviceScope::Sessions(HashSet::from([session_id.clone()])),
                None => DeviceScope::Full,
            };
            if slot.one_time {
                st.pairing.as_mut().expect("checked above").used = true;
            }
            st.devices.insert(
                req.app_pubkey,
                DeviceRecord {
                    name: req.device_name.clone(),
                    revoked: false,
                    scope,
                    // Pairing grants full access (the confirmed default);
                    // read-only is an explicit opt-down afterwards.
                    read_only: false,
                    // A device that has just paired has, by definition, just
                    // authenticated.
                    last_seen: Some(now_secs),
                },
            );
            let token = issue_token(&mut st, req.app_pubkey);
            let stored = StoredDevice {
                pubkey: req.app_pubkey,
                name: req.device_name.clone(),
                sessions: bound.map(|s| vec![s]),
                revoked: false,
                read_only: false,
                last_seen: Some(now_secs),
            };
            (token, stored)
        };
        self.persist_saved(&stored);
        // Announce AFTER the durable write, so a listener that reacts by reading
        // the device list can't observe a device the store doesn't have yet.
        self.announce_paired(super::PairedDevice {
            pubkey: req.app_pubkey,
            name: req.device_name.clone(),
        });
        Ok(token)
    }
}
