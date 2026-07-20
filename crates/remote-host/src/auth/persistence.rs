//! Durable device persistence for the [`AuthStore`](super::AuthStore).
//!
//! The store is kept authoritative in memory; a [`DeviceStore`] is an optional
//! write-through/seed sink so the authorized set + revocations survive a restart.
//! `AuthStore` depends only on the trait (mockable, no DB in unit tests);
//! [`StorageDeviceStore`] is the concrete `oximux-storage` binding.
//!
//! Reconnect `session_token`s are deliberately NOT persisted — they are an
//! ephemeral fast path; after a restart a client simply falls back to the
//! Ed25519 challenge.

use std::sync::Arc;

use oximux_storage::{RemoteDeviceRepo, RemoteScope};

use super::{AppPubkey, AuthStore, DeviceRecord, DeviceScope};

/// A device as it crosses the persistence boundary — only public/simple types,
/// so the trait never leaks the private `DeviceScope`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDevice {
    pub pubkey: AppPubkey,
    pub name: String,
    /// `None` = full access; `Some(list)` = restricted to those sessions.
    pub sessions: Option<Vec<String>>,
    pub revoked: bool,
    /// The opt-down tier: reads served, state-changing RPCs refused.
    pub read_only: bool,
    /// Unix seconds of the device's last successful authentication, if it has
    /// ever connected. `None` for a device that paired but never came back.
    pub last_seen: Option<u64>,
}

/// A durable sink for the authorized-device set. Methods are best-effort
/// (return `()`): a persistence failure must never break in-memory auth for the
/// live session — the impl logs and drops.
pub trait DeviceStore: Send + Sync {
    /// Load every recorded device (including revoked ones) to seed the store.
    fn load(&self) -> Vec<StoredDevice>;
    /// Persist a newly-registered device.
    fn save(&self, device: &StoredDevice);
    /// Persist a revoked-flag change.
    fn set_revoked(&self, pubkey: &AppPubkey, revoked: bool);
    /// Persist a read-only-tier change.
    fn set_read_only(&self, pubkey: &AppPubkey, read_only: bool);
    /// Erase a device's record entirely, so its key is no longer *known*.
    ///
    /// Distinct from [`set_revoked`](Self::set_revoked): a revoked device stays
    /// recorded on purpose, so proof of a pairing secret can never resurrect it.
    /// Erasing is the deliberate host-side undo of that, and is the only way a
    /// device that has already paired can pair again.
    fn remove(&self, pubkey: &AppPubkey);
    /// Record that a device just authenticated. Called once per connection, not
    /// per RPC — the per-RPC recheck runs on the hot path and must not touch the
    /// database.
    fn touch_last_seen(&self, pubkey: &AppPubkey);
}

impl AuthStore {
    /// Build a store backed by durable device persistence: seed the authorized
    /// set from `store` and write every register/revoke through to it.
    pub fn with_store(store: Arc<dyn DeviceStore>) -> Self {
        let mut state = super::AuthState::default();
        for d in store.load() {
            let scope = match d.sessions {
                Some(sessions) => DeviceScope::Sessions(sessions.into_iter().collect()),
                None => DeviceScope::Full,
            };
            state
                .devices
                .insert(
                    d.pubkey,
                    DeviceRecord {
                        name: d.name,
                        revoked: d.revoked,
                        scope,
                        read_only: d.read_only,
                        last_seen: d.last_seen,
                    },
                );
        }
        Self { store: Some(store), inner: std::sync::Mutex::new(state), ..Default::default() }
    }

    /// Write a newly-registered device through to durable storage (if any). Call
    /// **outside** the `inner` lock — the store may do blocking I/O.
    pub(super) fn persist_saved(&self, device: &StoredDevice) {
        if let Some(store) = &self.store {
            store.save(device);
        }
    }

    /// Write a revocation through to durable storage (if any). Call outside the lock.
    pub(super) fn persist_revoked(&self, pubkey: &AppPubkey, revoked: bool) {
        if let Some(store) = &self.store {
            store.set_revoked(pubkey, revoked);
        }
    }

    /// Erase a device from durable storage (if any). Call outside the lock.
    pub(super) fn persist_removed(&self, pubkey: &AppPubkey) {
        if let Some(store) = &self.store {
            store.remove(pubkey);
        }
    }

    /// Write a read-only-tier change through to durable storage (if any).
    pub(super) fn persist_read_only(&self, pubkey: &AppPubkey, read_only: bool) {
        if let Some(store) = &self.store {
            store.set_read_only(pubkey, read_only);
        }
    }

    /// Record a successful authentication, in memory and durably.
    ///
    /// Called once per connection from the handshake, never from the per-RPC
    /// recheck — that runs on every request and a database write there would put
    /// disk I/O on the hot path.
    pub fn touch_last_seen(&self, pubkey: &AppPubkey, now_secs: u64) {
        {
            let mut st = self.inner.lock().unwrap();
            if let Some(d) = st.devices.get_mut(pubkey) {
                d.last_seen = Some(now_secs);
            }
        }
        if let Some(store) = &self.store {
            store.touch_last_seen(pubkey);
        }
    }
}

/// The `oximux-storage`-backed [`DeviceStore`]. Maps `StoredDevice` ↔ the repo's
/// `RemoteScope`/rows and hex-encodes the 32-byte pubkey for the TEXT key.
pub struct StorageDeviceStore {
    repo: RemoteDeviceRepo,
}

impl StorageDeviceStore {
    pub fn new(repo: RemoteDeviceRepo) -> Self {
        Self { repo }
    }
}

impl DeviceStore for StorageDeviceStore {
    fn load(&self) -> Vec<StoredDevice> {
        match self.repo.list_all() {
            Ok(rows) => rows
                .into_iter()
                .filter_map(|row| {
                    Some(StoredDevice {
                        pubkey: hex_to_pubkey(&row.pubkey)?,
                        name: row.name,
                        sessions: match row.scope {
                            RemoteScope::Full => None,
                            RemoteScope::Sessions(s) => Some(s),
                        },
                        revoked: row.revoked,
                        read_only: row.read_only,
                        last_seen: row.last_seen.map(|t| t as u64),
                    })
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "loading persisted devices failed; starting empty");
                Vec::new()
            }
        }
    }

    fn save(&self, device: &StoredDevice) {
        let scope = match &device.sessions {
            None => RemoteScope::Full,
            Some(sessions) => RemoteScope::Sessions(sessions.clone()),
        };
        if let Err(e) =
            self.repo.upsert(&pubkey_to_hex(&device.pubkey), &device.name, &scope, device.revoked)
        {
            tracing::warn!(error = %e, "persisting paired device failed");
        }
    }

    fn set_revoked(&self, pubkey: &AppPubkey, revoked: bool) {
        if let Err(e) = self.repo.set_revoked(&pubkey_to_hex(pubkey), revoked) {
            tracing::warn!(error = %e, "persisting device revocation failed");
        }
    }

    fn set_read_only(&self, pubkey: &AppPubkey, read_only: bool) {
        if let Err(e) = self.repo.set_read_only(&pubkey_to_hex(pubkey), read_only) {
            tracing::warn!(error = %e, "persisting device read-only tier failed");
        }
    }

    fn remove(&self, pubkey: &AppPubkey) {
        if let Err(e) = self.repo.remove(&pubkey_to_hex(pubkey)) {
            tracing::warn!(error = %e, "erasing paired device failed");
        }
    }

    fn touch_last_seen(&self, pubkey: &AppPubkey) {
        if let Err(e) = self.repo.touch_last_seen(&pubkey_to_hex(pubkey)) {
            tracing::warn!(error = %e, "persisting device last-seen failed");
        }
    }
}

fn pubkey_to_hex(pubkey: &AppPubkey) -> String {
    pubkey.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_to_pubkey(hex: &str) -> Option<AppPubkey> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registration_proof;
    use ed25519_dalek::SigningKey;
    use oximux_remote_proto::messages::RegisterReq;
    use oximux_storage::open_memory;
    use std::sync::Mutex;

    const SECRET: [u8; 16] = [0x22; 16];
    const NOW: u64 = 1_700_000_000;

    fn vk(seed: u8) -> AppPubkey {
        SigningKey::from_bytes(&[seed; 32]).verifying_key().to_bytes()
    }

    fn reg(pubkey: AppPubkey, session: Option<&str>) -> RegisterReq {
        RegisterReq {
            app_pubkey: pubkey,
            device_name: "phone".into(),
            proof: registration_proof(&SECRET, &pubkey, NOW),
            timestamp_secs: NOW,
            session_id: session.map(Into::into),
        }
    }

    /// A record-only in-memory store, to assert the write-through hooks fire.
    #[derive(Default)]
    struct RecordingStore {
        saved: Mutex<Vec<StoredDevice>>,
        revoked: Mutex<Vec<(AppPubkey, bool)>>,
        read_only: Mutex<Vec<(AppPubkey, bool)>>,
        seen: Mutex<Vec<AppPubkey>>,
        removed: Mutex<Vec<AppPubkey>>,
        seed: Vec<StoredDevice>,
    }
    impl DeviceStore for RecordingStore {
        fn load(&self) -> Vec<StoredDevice> {
            self.seed.clone()
        }
        fn save(&self, device: &StoredDevice) {
            self.saved.lock().unwrap().push(device.clone());
        }
        fn set_revoked(&self, pubkey: &AppPubkey, revoked: bool) {
            self.revoked.lock().unwrap().push((*pubkey, revoked));
        }
        fn set_read_only(&self, pubkey: &AppPubkey, read_only: bool) {
            self.read_only.lock().unwrap().push((*pubkey, read_only));
        }
        fn remove(&self, pubkey: &AppPubkey) {
            self.removed.lock().unwrap().push(*pubkey);
        }
        fn touch_last_seen(&self, pubkey: &AppPubkey) {
            self.seen.lock().unwrap().push(*pubkey);
        }
    }

    /// The pair of properties that make revoke and forget different actions
    /// rather than two names for one: a revoked key stays known (so proof of a
    /// pairing secret can never resurrect a lost phone over the wire), and a
    /// forgotten one does not (so the user can deliberately let a device back
    /// in). Collapsing either direction reintroduces a real bug.
    #[test]
    fn revoking_blocks_re_registration_but_forgetting_allows_it() {
        let store = Arc::new(RecordingStore::default());
        let auth = AuthStore::with_store(store.clone());
        let pubkey = vk(0x44);

        auth.set_pairing(super::super::PairingSlot::new(SECRET, None, false));
        auth.register(&reg(pubkey, None), NOW).expect("first pairing");

        auth.revoke(&pubkey);
        auth.register(&reg(pubkey, None), NOW)
            .expect_err("a revoked device must not re-pair with only the secret");

        auth.forget(&pubkey);
        assert_eq!(
            store.removed.lock().unwrap().as_slice(),
            &[pubkey],
            "the erasure is written through, so it survives a restart",
        );
        auth.register(&reg(pubkey, None), NOW)
            .expect("a forgotten device is no longer known, so it may pair again");
    }

    /// Forgetting must cut a *live* device off as hard as revoking does — its
    /// cached token is what would otherwise keep an open connection working
    /// against a record that no longer exists.
    #[test]
    fn forgetting_drops_the_devices_live_tokens() {
        let store = Arc::new(RecordingStore::default());
        let auth = AuthStore::with_store(store.clone());
        let pubkey = vk(0x55);

        auth.set_pairing(super::super::PairingSlot::new(SECRET, None, false));
        let token = auth.register(&reg(pubkey, None), NOW).expect("pair");
        assert_eq!(auth.authorize_token(&token), Some(pubkey), "token works while paired");

        auth.forget(&pubkey);
        assert_eq!(auth.authorize_token(&token), None, "the token dies with the record");
        assert!(!auth.is_authorized(&pubkey), "and the device is no longer authorized");
    }

    #[test]
    fn register_and_revoke_write_through_to_the_store() {
        let store = Arc::new(RecordingStore::default());
        let auth = AuthStore::with_store(store.clone());
        auth.set_pairing(super::super::PairingSlot::new(SECRET, None, false));
        let pubkey = vk(0x33);

        auth.register(&reg(pubkey, None), NOW).expect("register");
        assert_eq!(store.saved.lock().unwrap().len(), 1, "register persisted");
        assert_eq!(store.saved.lock().unwrap()[0].pubkey, pubkey);

        auth.revoke(&pubkey);
        assert_eq!(store.revoked.lock().unwrap().as_slice(), &[(pubkey, true)], "revoke persisted");
    }

    #[test]
    fn seed_restores_authorized_devices() {
        let pubkey = vk(0x44);
        let store = Arc::new(RecordingStore {
            seed: vec![StoredDevice {
                pubkey,
                name: "seeded".into(),
                sessions: Some(vec!["sess-1".into()]),
                revoked: false,
                read_only: false,
                last_seen: None,
            }],
            ..Default::default()
        });
        let auth = AuthStore::with_store(store);
        assert!(auth.is_authorized(&pubkey), "seeded device is authorized");
        assert!(auth.is_allowed_for(&pubkey, "sess-1"), "seeded scope restored");
        assert!(!auth.is_allowed_for(&pubkey, "sess-2"), "seeded scope is not Full");
    }

    #[test]
    fn storage_backed_store_survives_a_restart() {
        let db = open_memory().expect("open_memory");
        let repo = RemoteDeviceRepo::new(db);
        let pubkey = vk(0x55);

        // Session A: pair a device, then revoke a different one.
        {
            let auth = AuthStore::with_store(Arc::new(StorageDeviceStore::new(repo.clone())));
            auth.set_pairing(super::super::PairingSlot::new(SECRET, Some("sess-1".into()), false));
            auth.register(&reg(pubkey, Some("sess-1")), NOW).expect("register");
        }

        // Session B: a fresh AuthStore over the SAME repo re-seeds the device.
        {
            let auth = AuthStore::with_store(Arc::new(StorageDeviceStore::new(repo.clone())));
            assert!(auth.is_authorized(&pubkey), "device survived the restart");
            assert!(auth.is_allowed_for(&pubkey, "sess-1"), "scope survived");
            assert!(!auth.is_allowed_for(&pubkey, "sess-2"));
            // A revoked device stays known → re-Register is still blocked post-restart.
            auth.revoke(&pubkey);
        }
        {
            let auth = AuthStore::with_store(Arc::new(StorageDeviceStore::new(repo.clone())));
            assert!(!auth.is_authorized(&pubkey), "revocation survived the restart");
            auth.set_pairing(super::super::PairingSlot::new(SECRET, None, false));
            assert_eq!(
                auth.register(&reg(pubkey, None), NOW),
                Err(oximux_remote_proto::RpcError::Unauthorized),
                "a revoked-and-persisted device cannot re-register"
            );
        }
    }
}
