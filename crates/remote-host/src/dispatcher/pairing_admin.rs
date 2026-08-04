//! The local-operator pairing administration (v16): mint a pairing window,
//! list enrollments, erase one. The runtime commands a headless host takes
//! instead of a `--pair` boot flag — a flag would reprint the bearer ticket
//! into the journal on every restart.
//!
//! Every arm is behind [`may_administer_pairing`], the strictest gate on the
//! protocol: local operator only, so a paired device can never mint further
//! enrollments. The minted ticket crosses exactly one hop — the owner-only
//! local socket — and is never logged here.
//!
//! [`may_administer_pairing`]: crate::auth::AuthStore::may_administer_pairing

use oximux_remote_proto::messages::{PairedDeviceWire, PairingIssuedWire};
use oximux_remote_proto::pairing::PairingTicket;
use oximux_remote_proto::proto::{Response, RpcError};

use super::Dispatcher;
use crate::auth::{PAIRING_WINDOW_SECS, PairingSlot, Peer, mint_pairing_secret};

impl Dispatcher {
    /// Open a one-time, short-lived pairing window and answer with its ticket.
    ///
    /// One live window at a time (a new mint replaces the previous slot,
    /// exactly as the desktop's pairing pane does), one-time, and expiring:
    /// those three properties are load-bearing for the write-by-default
    /// enrollment tier and must not be relaxed here.
    pub(super) fn pair_new(&self, peer: &Peer, read_only: bool) -> Response {
        if !self.auth.may_administer_pairing(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        // A ticket names the endpoint a device dials; a host that has not
        // bound one (remote access off) has nothing redeemable to offer.
        let Some(endpoint_id) = self.pairing_endpoint else {
            return Response::Error(RpcError::Unsupported);
        };
        let secret = mint_pairing_secret();
        let expires_at = (self.now_secs)() + PAIRING_WINDOW_SECS;
        self.auth.set_pairing(
            PairingSlot::expiring(secret, None, true, expires_at).with_read_only(read_only),
        );
        let ticket =
            PairingTicket { endpoint_id, handshake_secret: secret, session_id: None };
        match ticket.encode() {
            Ok(ticket) => {
                Response::PairingIssued(PairingIssuedWire { ticket, expires_at, read_only })
            }
            Err(err) => {
                // Withdraw the window we just opened: a slot nobody received
                // the secret for is pure attack surface.
                self.auth.close_pairing();
                tracing::warn!(%err, "pairing ticket encode failed");
                Response::Error(RpcError::Internal("pairing ticket encode failed".into()))
            }
        }
    }

    /// The enrollment list, tier and tombstones included.
    pub(super) fn pair_list(&self, peer: &Peer) -> Response {
        if !self.auth.may_administer_pairing(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        let rows = self
            .auth
            .devices()
            .into_iter()
            .map(|d| PairedDeviceWire {
                pubkey: d.pubkey,
                name: d.name,
                read_only: d.read_only,
                revoked: d.revoked,
                last_seen: d.last_seen,
            })
            .collect();
        Response::PairedDeviceList(rows)
    }

    /// Erase one enrollment. Idempotent — removing an unknown key is the state
    /// the caller wanted.
    pub(super) fn pair_remove(&self, peer: &Peer, pubkey: &[u8; 32]) -> Response {
        if !self.auth.may_administer_pairing(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        self.auth.forget(pubkey);
        Response::Ack
    }
}
