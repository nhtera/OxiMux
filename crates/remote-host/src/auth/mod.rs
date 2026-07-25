//! Two-key pairing/auth + a two-level ACL.
//!
//! - **Pairing** ([`pairing`]) proves a client scanned this host's QR: `Register`
//!   carries an `HMAC-SHA256(handshake_secret, app_pubkey || timestamp)` proof,
//!   checked inside a ±60s window. The secret never crosses the wire.
//! - **Identity** is the client's app-signing Ed25519 key (decoupled from the
//!   iroh transport key), which the host authorizes and can revoke.
//! - **Reconnect** ([`reconnect`]) is a `session_token` fast path, or an Ed25519
//!   challenge/nonce.
//! - **ACL** ([`acl`]): a global authorized set + a per-device scope. A one-time
//!   ticket bound to a session restricts that device to it; a static ticket
//!   grants full access (the confirmed default).
//!
//! Every check is a method on [`AuthStore`] so the dispatcher can re-run
//! authorization on **every** RPC — revocation must bite even a connection that
//! is already open.

mod acl;
mod pairing;
mod persistence;
mod reconnect;
#[cfg(test)]
mod tests;

pub use acl::DeviceInfo;
pub use persistence::{DeviceStore, StorageDeviceStore, StoredDevice};
// The registration proof is a wire invariant shared with the client, so it lives
// in `remote-proto`; re-exported here for the host's callers + tests.
pub use oximux_remote_proto::registration_proof;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use hmac::Hmac;
use rand::RngCore;
use rand::rngs::OsRng;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// A client's app-signing public key — the stable identity the host authorizes.
pub type AppPubkey = [u8; 32];

/// The maximum clock skew tolerated on a registration proof, each way.
const REGISTRATION_WINDOW_SECS: u64 = 60;

/// A QR pairing secret the host is currently advertising.
pub struct PairingSlot {
    pub secret: [u8; 16],
    /// A one-time ticket may bind pairing to a single session (scopes the device
    /// to it); `None` for a static/global full-access ticket.
    pub session_id: Option<String>,
    /// One-time tickets self-invalidate after a successful `Register`.
    pub one_time: bool,
    used: bool,
    /// Unix seconds after which the secret stops being redeemable, if the opener
    /// bounded it. `None` leaves it live until used or replaced.
    ///
    /// This is the *host's* copy of the window, not a display hint: a countdown
    /// that only ran in the UI would let a code keep working after the desktop
    /// said it had lapsed, which is precisely backwards for a bearer credential
    /// whose whole protection is being short-lived.
    expires_at: Option<u64>,
}

impl PairingSlot {
    pub fn new(secret: [u8; 16], session_id: Option<String>, one_time: bool) -> Self {
        Self { secret, session_id, one_time, used: false, expires_at: None }
    }

    /// A slot that stops being redeemable at `expires_at` (unix seconds).
    pub fn expiring(
        secret: [u8; 16],
        session_id: Option<String>,
        one_time: bool,
        expires_at: u64,
    ) -> Self {
        Self { expires_at: Some(expires_at), ..Self::new(secret, session_id, one_time) }
    }

    /// Spent — either redeemed (one-time) or past its window.
    fn dead_at(&self, now_secs: u64) -> bool {
        (self.one_time && self.used) || self.expires_at.is_some_and(|at| now_secs >= at)
    }
}

/// What a device is allowed to reach — the second ACL level, scoped to the
/// device (least privilege). A static/global pairing grants [`DeviceScope::Full`]
/// (the confirmed default); a one-time ticket bound to a session restricts the
/// device to only that session.
enum DeviceScope {
    Full,
    Sessions(HashSet<String>),
}

struct DeviceRecord {
    name: String,
    revoked: bool,
    scope: DeviceScope,
    /// The opt-down tier: reads are served, every state-changing RPC is refused.
    /// Orthogonal to `scope` — a device can be session-scoped AND read-only.
    read_only: bool,
    /// Unix seconds of the last successful authentication.
    last_seen: Option<u64>,
}

/// A device that just completed pairing. Published so the desktop can confirm it
/// to the user — pairing grants full access by default, so a silent one is exactly
/// the "race-won silent pairing" the red team flagged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedDevice {
    pub pubkey: AppPubkey,
    pub name: String,
}

#[derive(Default)]
struct AuthState {
    pairing: Option<PairingSlot>,
    devices: HashMap<AppPubkey, DeviceRecord>,
    tokens: HashMap<String, AppPubkey>,
}

/// The host's authorization state, shared behind a `Mutex` across connections.
/// The pairing / reconnect / ACL method sets live in the sibling submodules.
pub struct AuthStore {
    inner: Mutex<AuthState>,
    /// Optional durable sink: seeds the authorized set at construction and
    /// receives register/revoke write-throughs. `None` = in-memory only.
    store: Option<Arc<dyn DeviceStore>>,
    /// Announces completed pairings. Bounded and lossy by design — a missed
    /// notification must never stall `register`, which holds the auth path.
    paired_tx: broadcast::Sender<PairedDevice>,
}

impl Default for AuthStore {
    fn default() -> Self {
        Self {
            inner: Mutex::new(AuthState::default()),
            store: None,
            paired_tx: broadcast::channel(PAIRED_EVENT_CAPACITY).0,
        }
    }
}

/// Pairings are rare (a human scanning a code), so a small ring is ample.
const PAIRED_EVENT_CAPACITY: usize = 8;

impl AuthStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Watch for devices completing pairing.
    pub fn subscribe_pairings(&self) -> broadcast::Receiver<PairedDevice> {
        self.paired_tx.subscribe()
    }

    /// Announce a completed pairing. Send errors (no listeners) are ignored.
    pub(super) fn announce_paired(&self, device: PairedDevice) {
        let _ = self.paired_tx.send(device);
    }

    /// Is a pairing code currently usable? `false` once a one-time slot has been
    /// consumed or its window has lapsed, so the UI can stop showing a code that
    /// no longer works.
    pub fn pairing_open(&self) -> bool {
        self.pairing_open_at(now_secs())
    }

    /// [`pairing_open`](Self::pairing_open) against an explicit clock, so expiry
    /// is testable without sleeping.
    pub fn pairing_open_at(&self, now_secs: u64) -> bool {
        self.inner.lock().unwrap().pairing.as_ref().is_some_and(|slot| !slot.dead_at(now_secs))
    }

    /// Install (replace) the pairing secret the host is advertising via QR.
    pub fn set_pairing(&self, slot: PairingSlot) {
        self.inner.lock().unwrap().pairing = Some(slot);
    }

    /// Withdraw the advertised pairing secret, closing the window immediately.
    ///
    /// Leaving the view that opened a window is a statement that the user is done
    /// pairing, and honouring it beats waiting out the expiry: the shortest-lived
    /// credential is the one retired the moment nobody is looking at it.
    pub fn close_pairing(&self) {
        self.inner.lock().unwrap().pairing = None;
    }
}

/// Wall clock in unix seconds, saturating at the epoch on a clock set before it.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Mint a fresh 16-byte pairing secret from the OS CSPRNG. The enablement flow
/// hands this to both the advertised [`PairingSlot`] and the `PairingTicket` the
/// QR encodes — it is a bearer credential (one scan = full access by default) and
/// must never be logged.
pub fn mint_pairing_secret() -> [u8; 16] {
    let mut secret = [0u8; 16];
    OsRng.fill_bytes(&mut secret);
    secret
}

/// Mint a random reconnect token bound to `pubkey`.
fn issue_token(st: &mut AuthState, pubkey: AppPubkey) -> String {
    let mut raw = [0u8; 32];
    OsRng.fill_bytes(&mut raw);
    let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    st.tokens.insert(token.clone(), pubkey);
    token
}
