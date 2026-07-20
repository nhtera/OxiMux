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
    /// Unix seconds of the device's last successful authentication. `None` means
    /// it paired but has never reconnected — worth surfacing, since an entry that
    /// has never been used is a plausible sign of a pairing the user does not
    /// recognize.
    pub last_seen: Option<u64>,
    /// Cut off, but still recorded — the tombstone that stops a pairing code
    /// from resurrecting it. Listed rather than hidden so the record remains
    /// reachable: erasing it is the only way that device can ever pair again.
    pub revoked: bool,
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

    /// May this device see and attach to terminals?
    ///
    /// Requires [`DeviceScope::Full`], not merely "authorized". A terminal has
    /// no owning agent session, so a session-scoped device has no defensible
    /// mapping onto one — and inventing a mapping ("the terminal whose cwd
    /// matches") would silently widen the scope the desktop user chose, on the
    /// single highest-risk surface in this protocol. A narrowed device is
    /// therefore refused outright rather than shown a filtered list.
    ///
    /// This is read access: it shows the screen, it does not type. Typing is
    /// [`may_drive_terminals`](Self::may_drive_terminals).
    pub fn may_use_terminals(&self, pubkey: &AppPubkey) -> bool {
        let st = self.inner.lock().unwrap();
        matches!(
            st.devices.get(pubkey),
            Some(d) if !d.revoked && matches!(d.scope, DeviceScope::Full)
        )
    }

    /// May this device type into a terminal, or resize one?
    ///
    /// The most consequential permission the protocol grants: bytes into a live
    /// shell is arbitrary code execution on the desktop. Read-only is the
    /// opt-down that separates watching a terminal from driving it.
    ///
    /// Resize rides the same gate despite being harmless in isolation, because
    /// it is *shared* — the daemon runs the PTY at the smallest size any
    /// attachment asks for, so a phone can reflow the desktop user's window.
    pub fn may_drive_terminals(&self, pubkey: &AppPubkey) -> bool {
        if !self.may_use_terminals(pubkey) {
            return false;
        }
        !self.inner.lock().unwrap().devices.get(pubkey).is_some_and(|d| d.read_only)
    }

    /// Every recorded device — the data behind the paired-devices list, its
    /// revoke/forget actions, and its read-only toggle.
    ///
    /// Revoked devices are **included**, flagged rather than filtered. Hiding
    /// them made the record unreachable from the UI while `register` still
    /// refused the key, so revoking a phone quietly became permanent: nothing
    /// on screen could undo it.
    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.inner
            .lock()
            .unwrap()
            .devices
            .iter()
            .map(|(pubkey, d)| DeviceInfo {
                pubkey: *pubkey,
                name: d.name.clone(),
                read_only: d.read_only,
                last_seen: d.last_seen,
                revoked: d.revoked,
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

    /// Forget a device entirely: same immediate cut-off as [`revoke`](Self::revoke),
    /// but the record is erased rather than tombstoned, so the key stops being
    /// *known* and the device may pair again with a fresh code.
    ///
    /// This is the explicit host-side undo that [`register`](Self::register)'s
    /// already-known check requires. Keeping it separate from `revoke` is the
    /// point: revoking a lost phone must stay permanent, because otherwise anyone
    /// holding a pairing code could undo it over the wire. Forgetting is only
    /// reachable from the desktop, by someone already at the machine.
    pub fn forget(&self, pubkey: &AppPubkey) {
        {
            let mut st = self.inner.lock().unwrap();
            st.devices.remove(pubkey);
            // Drop live tokens in the same critical section as the record, or an
            // in-flight connection would keep passing the per-RPC recheck against
            // a device that no longer exists.
            st.tokens.retain(|_, owner| owner != pubkey);
        }
        // Write through outside the lock (the store may block on I/O).
        self.persist_removed(pubkey);
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
