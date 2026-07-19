//! The authorization gates the dispatcher re-runs on every RPC: global
//! authorization (revocation), per-device scope, the device listing, and revoke.

use super::{AppPubkey, AuthStore, DeviceScope};

/// One paired device as the desktop UI shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub pubkey: AppPubkey,
    pub name: String,
    /// The opt-down tier: this device may read but not act.
    pub read_only: bool,
}

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

    /// The currently-authorized (non-revoked) devices — the data behind the
    /// paired-devices list, its revoke action, and its read-only toggle.
    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.inner
            .lock()
            .unwrap()
            .devices
            .iter()
            .filter(|(_, d)| !d.revoked)
            .map(|(pubkey, d)| DeviceInfo {
                pubkey: *pubkey,
                name: d.name.clone(),
                read_only: d.read_only,
            })
            .collect()
    }

    /// May this device perform a **state-changing** RPC on `session_id`?
    ///
    /// Strictly narrower than [`is_allowed_for`](Self::is_allowed_for): the device
    /// must be authorized and in scope AND not marked read-only. Every RPC that
    /// drives an agent, decides a permission, writes the repository, or types into
    /// a terminal goes through this — a read-only device can watch, never act.
    pub fn may_write(&self, pubkey: &AppPubkey, session_id: &str) -> bool {
        if !self.is_allowed_for(pubkey, session_id) {
            return false;
        }
        !self.inner.lock().unwrap().devices.get(pubkey).is_some_and(|d| d.read_only)
    }

    /// Is this device marked read-only (the opt-down tier)? For the paired-device
    /// UI; authorization decisions use [`may_write`](Self::may_write).
    pub fn is_read_only(&self, pubkey: &AppPubkey) -> bool {
        self.inner.lock().unwrap().devices.get(pubkey).is_some_and(|d| d.read_only)
    }

    /// Move a device between the read-write and read-only tiers. Takes effect on
    /// the next RPC (the dispatcher rechecks per call, so an open connection is
    /// downgraded mid-session) and is written through to durable storage.
    pub fn set_read_only(&self, pubkey: &AppPubkey, read_only: bool) {
        {
            let mut st = self.inner.lock().unwrap();
            if let Some(d) = st.devices.get_mut(pubkey) {
                d.read_only = read_only;
            }
        }
        // Write through outside the lock (the store may block on I/O).
        self.persist_read_only(pubkey, read_only);
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
