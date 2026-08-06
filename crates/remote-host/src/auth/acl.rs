//! The authorization gates the dispatcher re-runs on every RPC: global
//! authorization (revocation), per-device scope, the device listing, and revoke.

use super::peer::PeerKind;
use super::{AppPubkey, AuthStore, DeviceScope, Peer};

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

    /// May this caller act on `session_id`? A remote device must be authorized
    /// (not revoked) AND have the session in scope — `Full` for a static
    /// pairing, or the session listed in a session-bound one. A local caller
    /// answers from its [`LocalScope`](super::LocalScope) alone: it has no
    /// device record to revoke, and its lifetime is the desktop's local-access
    /// toggle.
    pub fn is_allowed_for(&self, peer: &Peer, session_id: &str) -> bool {
        match peer.kind() {
            PeerKind::Local(scope) => scope.allows(session_id),
            PeerKind::Remote(pubkey) => {
                let st = self.inner.lock().unwrap();
                match st.devices.get(pubkey) {
                    Some(d) if !d.revoked => match &d.scope {
                        DeviceScope::Full => true,
                        DeviceScope::Sessions(sessions) => sessions.contains(session_id),
                    },
                    _ => false,
                }
            }
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
    pub fn may_use_terminals(&self, peer: &Peer) -> bool {
        match peer.kind() {
            PeerKind::Local(scope) => scope.is_full(),
            PeerKind::Remote(pubkey) => {
                let st = self.inner.lock().unwrap();
                matches!(
                    st.devices.get(pubkey),
                    Some(d) if !d.revoked && matches!(d.scope, DeviceScope::Full)
                )
            }
        }
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
    pub fn may_drive_terminals(&self, peer: &Peer) -> bool {
        if !self.may_use_terminals(peer) {
            return false;
        }
        match peer.kind() {
            // A full-scope local caller is the operator; there is no read-only
            // tier to consult for it.
            PeerKind::Local(_) => true,
            PeerKind::Remote(pubkey) => {
                !self.inner.lock().unwrap().devices.get(pubkey).is_some_and(|d| d.read_only)
            }
        }
    }

    /// May this device start a **new** agent session?
    ///
    /// Two gates in one, because creating a session is the only RPC with no
    /// session id to scope against:
    ///
    /// - **Full scope required.** A session-scoped device confined to one
    ///   conversation could otherwise create a second one and simply walk out of
    ///   its own scope, which would make the narrowing decorative. So it is
    ///   refused outright, the same reasoning as
    ///   [`may_use_terminals`](Self::may_use_terminals).
    /// - **Not read-only.** Spawning a process is as state-changing as it gets.
    ///
    /// Written as its own check rather than `may_write(pubkey, "")`. That
    /// expression happens to behave identically today — an empty id matches no
    /// scoped device — but only by accident of the sentinel, and a reader cannot
    /// tell whether the empty string is load-bearing or a placeholder. This says
    /// what it means.
    pub fn may_create_sessions(&self, peer: &Peer) -> bool {
        match peer.kind() {
            PeerKind::Local(scope) => scope.is_full(),
            PeerKind::Remote(pubkey) => {
                let st = self.inner.lock().unwrap();
                matches!(
                    st.devices.get(pubkey),
                    Some(d) if !d.revoked && !d.read_only && matches!(d.scope, DeviceScope::Full)
                )
            }
        }
    }

    /// May this device read the desktop's schedules and their run history?
    ///
    /// Full scope, for the same reason [`may_use_terminals`](Self::may_use_terminals)
    /// requires it: a schedule names no session, so a session-scoped device has
    /// nothing to be narrowed against, and a schedule's cwd and prompt describe
    /// work that may lie entirely outside the one conversation such a device was
    /// confined to. Reading changes nothing, so a read-only full device is
    /// admitted — it can watch the schedule list it cannot edit.
    pub fn may_read_schedules(&self, peer: &Peer) -> bool {
        match peer.kind() {
            PeerKind::Local(scope) => scope.is_full(),
            PeerKind::Remote(pubkey) => {
                let st = self.inner.lock().unwrap();
                matches!(
                    st.devices.get(pubkey),
                    Some(d) if !d.revoked && matches!(d.scope, DeviceScope::Full)
                )
            }
        }
    }

    /// May this device create, delete, or toggle a schedule?
    ///
    /// The same two gates as [`may_create_sessions`](Self::may_create_sessions),
    /// and for the same reason: a schedule is a *deferred* session spawn, so
    /// planting one is at least as privileged as starting a session now. Full
    /// scope stops a session-scoped device from planting a run outside its
    /// confinement; not-read-only stops a watcher from arming one at all.
    pub fn may_manage_schedules(&self, peer: &Peer) -> bool {
        match peer.kind() {
            PeerKind::Local(scope) => scope.is_full(),
            PeerKind::Remote(pubkey) => {
                let st = self.inner.lock().unwrap();
                matches!(
                    st.devices.get(pubkey),
                    Some(d) if !d.revoked && !d.read_only && matches!(d.scope, DeviceScope::Full)
                )
            }
        }
    }

    /// May this device list a project's worktrees?
    ///
    /// Full scope, for the reason [`may_read_schedules`](Self::may_read_schedules)
    /// requires it: a worktree names no session, so a session-scoped device has
    /// nothing to be narrowed against, and worktree rows carry host paths and
    /// branch names across every project — precisely what a confined device must
    /// not enumerate. Reading changes nothing, so a read-only full device is
    /// admitted.
    pub fn may_read_worktrees(&self, peer: &Peer) -> bool {
        match peer.kind() {
            PeerKind::Local(scope) => scope.is_full(),
            PeerKind::Remote(pubkey) => {
                let st = self.inner.lock().unwrap();
                matches!(
                    st.devices.get(pubkey),
                    Some(d) if !d.revoked && matches!(d.scope, DeviceScope::Full)
                )
            }
        }
    }

    /// May this device create or remove a worktree?
    ///
    /// The same two gates as [`may_create_sessions`](Self::may_create_sessions),
    /// and deliberately **never** routed through [`may_write`](Self::may_write):
    /// these RPCs name no session, so the session-scoped gate would have nothing
    /// to narrow against and the check would hold only by accident of a
    /// sentinel. Creating a worktree writes the filesystem and the repository
    /// (a directory plus a branch), and removing one is destructive — full
    /// scope stops a session-scoped device from minting itself a directory
    /// outside its confinement; not-read-only stops a watcher from writing.
    pub fn may_manage_worktrees(&self, peer: &Peer) -> bool {
        match peer.kind() {
            PeerKind::Local(scope) => scope.is_full(),
            PeerKind::Remote(pubkey) => {
                let st = self.inner.lock().unwrap();
                matches!(
                    st.devices.get(pubkey),
                    Some(d) if !d.revoked && !d.read_only && matches!(d.scope, DeviceScope::Full)
                )
            }
        }
    }

    /// May this caller administer pairing — mint tickets, list enrollments,
    /// erase one?
    ///
    /// **The strictest gate on this protocol: local operator only.** A paired
    /// device that could mint tickets could enroll further devices — lateral
    /// movement from one compromised phone to a standing fleet — so even a
    /// full-scope, write-capable remote device is refused. Only a caller the
    /// host's own owner-only socket authenticated at full scope qualifies,
    /// which is the same person the desktop's pairing pane already obeys.
    pub fn may_administer_pairing(&self, peer: &Peer) -> bool {
        match peer.kind() {
            PeerKind::Local(scope) => scope.is_full(),
            PeerKind::Remote(_) => false,
        }
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

    /// May this caller perform a **state-changing** RPC on `session_id`?
    ///
    /// Strictly narrower than [`is_allowed_for`](Self::is_allowed_for): the device
    /// must be authorized and in scope AND not marked read-only. Every RPC that
    /// drives an agent, decides a permission, writes the repository, or types into
    /// a terminal goes through this — a read-only device can watch, never act.
    /// A local caller has no read-only tier: within its scope it acts with the
    /// operator's own authority.
    pub fn may_write(&self, peer: &Peer, session_id: &str) -> bool {
        if !self.is_allowed_for(peer, session_id) {
            return false;
        }
        match peer.kind() {
            PeerKind::Local(_) => true,
            PeerKind::Remote(pubkey) => {
                !self.inner.lock().unwrap().devices.get(pubkey).is_some_and(|d| d.read_only)
            }
        }
    }

    /// May this caller arm or disarm a heartbeat on `session_id`?
    ///
    /// Deliberately [`may_write`](Self::may_write) and **not** the full-scope
    /// `may_manage_schedules` gate its sibling verbs use. A heartbeat is a
    /// deferred `SendPrompt` into one already-open session, so it grants exactly
    /// what the caller can already do by hand — whereas an ordinary schedule is
    /// a deferred *spawn*, which a session-scoped caller could use to create its
    /// way out of the box. The narrowing that stops a confined agent reaching
    /// another session is the same one that stops it prompting one.
    pub fn may_manage_heartbeats(&self, peer: &Peer, session_id: &str) -> bool {
        self.may_write(peer, session_id)
    }

    /// May this caller see the heartbeats armed on `session_id`? Reading
    /// changes nothing, so this is the plain session scope — a read-only device
    /// watching a session may see its wake-ups.
    pub fn may_read_heartbeats(&self, peer: &Peer, session_id: &str) -> bool {
        self.is_allowed_for(peer, session_id)
    }

    /// May this caller attempt a heartbeat write *before* a target is known?
    ///
    /// The coarse gate an **idempotent** delete needs. Deleting a heartbeat is
    /// authorized against the session it targets, read from the store — but a
    /// row that does not exist names no session, and answering "already gone"
    /// without any check at all would let a read-only device skip the tier
    /// check entirely by naming an id that never existed. So the tier is
    /// consulted first, and the per-target scope check still runs the moment a
    /// row IS found.
    ///
    /// Deliberately coarse: it must admit a session-confined caller, since
    /// deleting its own wake-up is exactly what that caller is for.
    pub fn may_attempt_heartbeat_writes(&self, peer: &Peer) -> bool {
        match peer.kind() {
            PeerKind::Local(_) => true,
            PeerKind::Remote(pubkey) => {
                let st = self.inner.lock().unwrap();
                matches!(st.devices.get(pubkey), Some(d) if !d.revoked && !d.read_only)
            }
        }
    }

    /// May this caller open a team run?
    ///
    /// The same two gates as [`may_create_sessions`](Self::may_create_sessions),
    /// because that is literally what it does — several times over. A
    /// session-scoped caller is refused outright: a run spans sessions by
    /// definition, so serving a narrowed one would hand it the escape hatch the
    /// narrowing exists to close.
    pub fn may_manage_teams(&self, peer: &Peer) -> bool {
        self.may_create_sessions(peer)
    }

    /// May this caller read a team run whose roles occupy `sessions`?
    ///
    /// Full scope reads any run. A session-confined caller reads only a run it
    /// is *in* — which is what lets a role check whether its teammates have
    /// finished without being able to enumerate the host's other work.
    pub fn may_read_team_run(&self, peer: &Peer, sessions: &[String]) -> bool {
        match peer.kind() {
            PeerKind::Local(scope) => {
                scope.is_full() || sessions.iter().any(|s| scope.allows(s))
            }
            PeerKind::Remote(pubkey) => {
                let st = self.inner.lock().unwrap();
                match st.devices.get(pubkey) {
                    Some(d) if !d.revoked => match &d.scope {
                        DeviceScope::Full => true,
                        DeviceScope::Sessions(scoped) => {
                            sessions.iter().any(|s| scoped.contains(s))
                        }
                    },
                    _ => false,
                }
            }
        }
    }

    /// May this caller read the coordination blackboard?
    ///
    /// Any authorized caller, at any scope. **This is the one surface a
    /// session-confined caller shares host-wide**, and it is deliberate: a
    /// blackboard narrowed to one session coordinates nothing, which is the
    /// entire feature. What keeps that defensible is that the board holds only
    /// what agents deliberately put on it — no host paths, no credentials, no
    /// session contents — so it is a channel between agents rather than a
    /// window into the host.
    pub fn may_read_state(&self, peer: &Peer) -> bool {
        match peer.kind() {
            PeerKind::Local(_) => true,
            PeerKind::Remote(pubkey) => self.is_authorized(pubkey),
        }
    }

    /// May this caller write the coordination blackboard? Same reach as
    /// [`may_read_state`](Self::may_read_state), minus the read-only tier: a
    /// device that may only watch must not be able to steer agents by editing
    /// what they coordinate on.
    pub fn may_write_state(&self, peer: &Peer) -> bool {
        match peer.kind() {
            PeerKind::Local(_) => true,
            PeerKind::Remote(pubkey) => {
                let st = self.inner.lock().unwrap();
                matches!(st.devices.get(pubkey), Some(d) if !d.revoked && !d.read_only)
            }
        }
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
    /// holding a pairing code could undo it over the wire.
    ///
    /// Reachable two ways, and only two: from the desktop, by someone already at
    /// the machine, and from a device dropping *itself*
    /// ([`forget_self`](Self::forget_self)). No path lets one device forget
    /// another.
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

    /// [`forget`](Self::forget) a device at its own request, announcing it so the
    /// desktop's list stops showing a device that has walked away.
    ///
    /// Announced where the desktop-initiated `forget` is not: that one is already
    /// a direct response to a click, and the list it refreshes is the one the
    /// clicker is looking at. This one arrives with nothing on screen having
    /// caused it, so the pane would otherwise keep listing a phone that had
    /// dropped the pairing — precisely the stale row that made the phone's
    /// "Forget this desktop" look like it had done nothing.
    ///
    /// The name is read before the record goes, since afterwards there is nothing
    /// left to name it with.
    pub fn forget_self(&self, pubkey: &AppPubkey) {
        let name = self
            .inner
            .lock()
            .unwrap()
            .devices
            .get(pubkey)
            .map(|d| d.name.clone())
            .unwrap_or_default();
        self.forget(pubkey);
        self.announce_paired(super::PairingEvent::Unpaired(super::PairedDevice {
            pubkey: *pubkey,
            name,
        }));
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
