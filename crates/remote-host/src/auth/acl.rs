//! The authorization gates the dispatcher re-runs on every RPC: global
//! authorization (revocation), per-device scope, the device listing, and revoke.

use super::{AppPubkey, AuthStore, DeviceScope};

impl AuthStore {
    /// Is this device still globally authorized (i.e. not revoked)? The per-RPC
    /// revocation gate.
    pub fn is_authorized(&self, pubkey: &AppPubkey) -> bool {
        self.inner.lock().unwrap().devices.get(pubkey).is_some_and(|d| !d.revoked)
    }

    /// May this device act on `session_id`? Authorized (not revoked) AND its scope
    /// allows the session — `Full` for a static pairing, or the session listed in
    /// a session-bound one.
    pub fn is_allowed_for(&self, pubkey: &AppPubkey, session_id: &str) -> bool {
        let st = self.inner.lock().unwrap();
        match st.devices.get(pubkey) {
            Some(d) if !d.revoked => match &d.scope {
                DeviceScope::Full => true,
                DeviceScope::Sessions(sessions) => sessions.contains(session_id),
            },
            _ => false,
        }
    }

    /// The currently-authorized (non-revoked) devices as `(pubkey, name)` — the
    /// data behind the paired-devices list + revoke UI.
    pub fn devices(&self) -> Vec<(AppPubkey, String)> {
        self.inner
            .lock()
            .unwrap()
            .devices
            .iter()
            .filter(|(_, d)| !d.revoked)
            .map(|(pubkey, d)| (*pubkey, d.name.clone()))
            .collect()
    }

    /// Revoke a device: it fails the next per-RPC recheck and its tokens die.
    pub fn revoke(&self, pubkey: &AppPubkey) {
        {
            let mut st = self.inner.lock().unwrap();
            if let Some(d) = st.devices.get_mut(pubkey) {
                d.revoked = true;
            }
            st.tokens.retain(|_, owner| owner != pubkey);
        }
        // Write through outside the lock (the store may block on I/O).
        self.persist_revoked(pubkey, true);
    }
}
