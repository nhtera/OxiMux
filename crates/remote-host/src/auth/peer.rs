//! Who a session RPC is served to — the caller vocabulary above the device
//! store.
//!
//! Remote callers carry the [`AppPubkey`] the host paired and can revoke. Local
//! callers (the CLI over the desktop's owner-only socket) have no device record:
//! their authority comes from holding the local bearer token, and their scope
//! from how the connection announced itself. The two are one type here so every
//! ACL predicate answers for both through a single gate — a handler cannot
//! forget to consider the local case, because the local case is not a separate
//! code path.

use std::sync::Arc;

use super::AppPubkey;

/// What a local caller may reach.
///
/// `Full` is an operator at their own keyboard — the same person the desktop
/// already obeys. `Session` is a caller that announced itself as belonging to
/// one agent session (the CLI presents its `OXIMUX_SESSION_ID`), and is
/// confined to it exactly as a session-scoped paired device would be: it may
/// read and write that conversation, and nothing else — no other sessions, no
/// terminals, no schedules, no creating its way out of the box.
///
/// `Arc<str>` rather than `String` because a [`Peer`] is re-derived from this
/// on every RPC *and* every forwarded live frame, and each of those cloned the
/// scope. On a busy session that was an allocation per streamed event on the
/// forwarding path; a refcount bump costs nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalScope {
    Full,
    Session(Arc<str>),
}

impl LocalScope {
    /// Does this scope admit `session_id`? The local mirror of a device's
    /// `DeviceScope` check.
    pub(super) fn allows(&self, session_id: &str) -> bool {
        match self {
            LocalScope::Full => true,
            LocalScope::Session(id) => &**id == session_id,
        }
    }

    /// Full-host surfaces (terminals, schedules, session creation, project
    /// enumeration): only the operator tier. A session-confined caller is
    /// refused outright, for the same reason a session-scoped device is —
    /// serving a filtered view would silently widen the narrowing.
    pub(super) fn is_full(&self) -> bool {
        matches!(self, LocalScope::Full)
    }
}

/// One authenticated caller of the session-RPC surface.
///
/// The inner enum is private on purpose: `Peer::remote` is free to mint (a
/// pubkey proves nothing by itself — every predicate still consults the device
/// store), but a local peer asserts "the desktop's own listener authenticated
/// this connection against the local token", and only this crate's serve path
/// may claim that. No remote handshake can construct it, which is asserted by
/// test as well as by visibility.
#[derive(Debug, Clone)]
pub struct Peer(PeerKind);

#[derive(Debug, Clone)]
pub(super) enum PeerKind {
    Local(LocalScope),
    Remote(AppPubkey),
}

impl Peer {
    /// The two-armed view the ACL predicates match on. `pub(super)` keeps the
    /// exhaustive match inside the auth module — everyone else goes through
    /// the predicates.
    pub(super) fn kind(&self) -> &PeerKind {
        &self.0
    }

    /// A paired remote device, identified by its app-signing key.
    pub fn remote(pubkey: AppPubkey) -> Self {
        Self(PeerKind::Remote(pubkey))
    }

    /// A caller on the desktop's local socket. `pub(crate)`: only the host's
    /// own serve path mints local authority.
    pub(crate) fn local(scope: LocalScope) -> Self {
        Self(PeerKind::Local(scope))
    }

    /// The paired-device identity, when this caller is remote. Handlers that
    /// act on the device record (`Unpair`) have nothing to act on for a local
    /// caller.
    pub(crate) fn remote_pubkey(&self) -> Option<&AppPubkey> {
        match &self.0 {
            PeerKind::Remote(pubkey) => Some(pubkey),
            PeerKind::Local(_) => None,
        }
    }
}
