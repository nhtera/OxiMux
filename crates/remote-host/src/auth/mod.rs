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

pub use persistence::{DeviceStore, StorageDeviceStore, StoredDevice};

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use hmac::{Hmac, Mac};
use rand::RngCore;
use rand::rngs::OsRng;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// A client's app-signing public key — the stable identity the host authorizes.
pub type AppPubkey = [u8; 32];

/// The maximum clock skew tolerated on a registration proof, each way.
const REGISTRATION_WINDOW_SECS: u64 = 60;

/// The canonical registration proof: `HMAC-SHA256(secret, app_pubkey || ts_le)`.
/// Shared shape — the client (`remote-session`/`mobile-core`) computes the same;
/// factor into a shared crate when that client lands.
pub fn registration_proof(secret: &[u8; 16], app_pubkey: &AppPubkey, timestamp_secs: u64) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(app_pubkey);
    mac.update(&timestamp_secs.to_le_bytes());
    mac.finalize().into_bytes().into()
}

/// A QR pairing secret the host is currently advertising.
pub struct PairingSlot {
    pub secret: [u8; 16],
    /// A one-time ticket may bind pairing to a single session (scopes the device
    /// to it); `None` for a static/global full-access ticket.
    pub session_id: Option<String>,
    /// One-time tickets self-invalidate after a successful `Register`.
    pub one_time: bool,
    used: bool,
}

impl PairingSlot {
    pub fn new(secret: [u8; 16], session_id: Option<String>, one_time: bool) -> Self {
        Self { secret, session_id, one_time, used: false }
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
}

#[derive(Default)]
struct AuthState {
    pairing: Option<PairingSlot>,
    devices: HashMap<AppPubkey, DeviceRecord>,
    tokens: HashMap<String, AppPubkey>,
}

/// The host's authorization state, shared behind a `Mutex` across connections.
/// The pairing / reconnect / ACL method sets live in the sibling submodules.
#[derive(Default)]
pub struct AuthStore {
    inner: Mutex<AuthState>,
    /// Optional durable sink: seeds the authorized set at construction and
    /// receives register/revoke write-throughs. `None` = in-memory only.
    store: Option<Arc<dyn DeviceStore>>,
}

impl AuthStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install (replace) the pairing secret the host is advertising via QR.
    pub fn set_pairing(&self, slot: PairingSlot) {
        self.inner.lock().unwrap().pairing = Some(slot);
    }

    /// Stop advertising any pairing secret. The enablement UI calls this once
    /// pairing is done (or the Allow-connections toggle goes off) so a static
    /// secret can't be used to `Register` indefinitely.
    pub fn clear_pairing(&self) {
        self.inner.lock().unwrap().pairing = None;
    }
}

/// Mint a random reconnect token bound to `pubkey`.
fn issue_token(st: &mut AuthState, pubkey: AppPubkey) -> String {
    let mut raw = [0u8; 32];
    OsRng.fill_bytes(&mut raw);
    let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    st.tokens.insert(token.clone(), pubkey);
    token
}
