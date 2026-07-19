//! The desktop's remote-control state: a process-wide [`SessionRegistry`] that the
//! in-app iroh host serves, plus an `enabled` flag that gates whether live agent
//! sessions are fanned into it at all, and the running [`HostHandle`] itself.
//!
//! Held as a [`gpui::Global`] so any [`AgentChatView`] can bind its session into the
//! registry on connect and tee its `ThreadEvent`s in — but only while remote control
//! is enabled, so a disabled desktop pays **zero** per-event cost (no clone, no
//! registration). The registry itself is `gpui`-free and lives behind an `Arc`, so
//! the network layer subscribes and commands sessions off the UI thread. The
//! enablement toggle binds/stops the iroh host through the shared `&Global` (the
//! async bind runs on the tokio runtime; the resulting handle folds back here).
//!
//! [`AgentChatView`]: crate::shell::agent_chat

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use gpui::Global;
use oximux_agents::session_registry::{SessionHandle, SessionMeta, SessionRegistry};
use oximux_agents::thread::AgentConnection;
use oximux_remote_host::{
    AppPubkey, AuthStore, DeviceStore, Dispatcher, PairingSlot, mint_pairing_secret,
};
use oximux_remote_iroh::HostHandle;
use oximux_remote_proto::PairingTicket;

/// Monotonic source of stable per-view remote session ids. Decoupled from an
/// agent's own (often not-yet-known-at-connect) session id: the phone only needs a
/// key that stays stable for a desktop session's lifetime, and a fresh chat has no
/// id until its subprocess assigns one. Process-wide, so ids never collide.
static REMOTE_SEQ: AtomicU64 = AtomicU64::new(1);

/// Mint the next stable remote session id (`"agent-N"`). Human-readable so it reads
/// sensibly in the phone's session list.
pub fn next_remote_session_id() -> String {
    format!("agent-{}", REMOTE_SEQ.fetch_add(1, Ordering::Relaxed))
}

/// A view's live tie into the registry: the handle to tee events through, plus the
/// registry itself so teardown can `unregister` without a `gpui` context (a `Drop`
/// has none). Held `Option`ally on the view — `None` when remote is disabled.
pub struct RemoteBinding {
    registry: Arc<SessionRegistry>,
    handle: Arc<SessionHandle>,
}

impl RemoteBinding {
    /// Fan one backend event into the bound session (assign seq, store, broadcast).
    pub fn ingest(&self, event: oximux_agents::thread::ThreadEvent) {
        self.handle.ingest(event);
    }

    /// Publish the session's display metadata (title/model) so a remote client's
    /// session list shows what the desktop shows instead of the raw session id.
    pub fn set_meta(&self, meta: SessionMeta) {
        self.handle.set_meta(meta);
    }

    /// Remove the session from the registry. The map holds its own `Arc`, so this
    /// explicit call is required — dropping the view's handle alone won't evict it.
    pub fn unregister(self, id: &str) {
        self.registry.unregister(id);
    }
}

/// Process-wide remote-control state, installed once at boot as a `gpui::Global`.
pub struct RemoteControl {
    registry: Arc<SessionRegistry>,
    /// `AtomicBool` (not `bool`) so the Settings toggle can flip it through the
    /// shared `&Global` reference without needing `&mut` access to the global.
    enabled: AtomicBool,
    /// The running iroh host, once the endpoint has bound. A `Mutex` (not the
    /// `enabled` atomic style) because a [`HostHandle`] isn't a primitive; guarded
    /// behind the shared `&Global` so the toggle starts/stops it without `&mut`
    /// global access. Dropping the handle stops the accept loop + closes the endpoint.
    host: Mutex<Option<HostHandle>>,
    /// Durable paired-device persistence, installed at boot. Every host bind seeds a
    /// fresh [`AuthStore`] from it, so devices paired in an earlier run (or before the
    /// last toggle-off) stay authorized. `None` = in-memory only (tests).
    devices: Option<Arc<dyn DeviceStore>>,
    /// The live host's auth store while one is bound, so the paired-devices UI can
    /// revoke against the *running* host (the dispatcher rechecks authorization on
    /// every RPC, so a revoke lands mid-session). Cleared on stop — with no host, the
    /// durable store is authoritative.
    auth: Mutex<Option<Arc<AuthStore>>>,
}

impl Global for RemoteControl {}

impl Default for RemoteControl {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteControl {
    /// A fresh, **disabled** remote-control state — no sessions are fanned in and no
    /// host is bound until the enablement toggle turns it on.
    pub fn new() -> Self {
        Self {
            registry: Arc::new(SessionRegistry::new()),
            enabled: AtomicBool::new(false),
            host: Mutex::new(None),
            devices: None,
            auth: Mutex::new(None),
        }
    }

    /// The boot constructor: same as [`new`](Self::new) but backed by durable
    /// paired-device storage, so a phone paired in an earlier run stays authorized
    /// across restarts and toggle cycles.
    pub fn with_devices(devices: Arc<dyn DeviceStore>) -> Self {
        Self { devices: Some(devices), ..Self::new() }
    }

    /// The shared session registry (the host serves from this same instance).
    pub fn registry(&self) -> Arc<SessionRegistry> {
        self.registry.clone()
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Release);
    }

    /// Assemble a fresh dispatcher + one-time CSPRNG pairing secret for a host bind.
    /// The secret seeds both the auth store's advertised pairing slot and the
    /// `PairingTicket` that [`start_host`](oximux_remote_iroh::start_host) mints, so
    /// the QR and the host agree. A fresh [`AuthStore`] per bind means toggling remote
    /// off then on rotates the **pairing secret** (the old QR stops working) — but the
    /// store it is seeded from is durable, so already-paired devices stay authorized
    /// and reconnect without re-scanning.
    pub fn prepare_host(&self) -> (Arc<Dispatcher>, [u8; 16]) {
        let secret = mint_pairing_secret();
        let auth = Arc::new(match &self.devices {
            Some(devices) => AuthStore::with_store(devices.clone()),
            None => AuthStore::new(),
        });
        auth.set_pairing(PairingSlot::new(secret, None, false));
        // Keep a handle so the paired-devices UI can revoke against the live host.
        *self.auth.lock().unwrap() = Some(auth.clone());
        let dispatcher = Arc::new(Dispatcher::new(self.registry.clone(), auth));
        (dispatcher, secret)
    }

    /// Paired devices to list in the UI as `(pubkey, name)`, revoked ones excluded.
    /// Reads the live auth store while a host is bound (so the list matches exactly
    /// what that host will accept) and falls back to durable storage when remote is
    /// off — you can review and revoke a device without turning remote access on.
    pub fn paired_devices(&self) -> Vec<(AppPubkey, String)> {
        if let Some(auth) = self.auth.lock().unwrap().as_ref() {
            return auth.devices();
        }
        match &self.devices {
            Some(store) => store
                .load()
                .into_iter()
                .filter(|d| !d.revoked)
                .map(|d| (d.pubkey, d.name))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Revoke a paired device. Against a live host this drops its tokens and fails
    /// its next per-RPC authorization recheck (so an in-flight session dies), and
    /// write-through keeps it revoked across restarts. With no host bound, the
    /// revocation goes straight to durable storage.
    pub fn revoke_device(&self, pubkey: &AppPubkey) {
        if let Some(auth) = self.auth.lock().unwrap().as_ref() {
            auth.revoke(pubkey);
            return;
        }
        if let Some(store) = &self.devices {
            store.set_revoked(pubkey, true);
        }
    }

    /// Store the freshly-bound host, stopping and replacing any prior one.
    pub fn set_host(&self, host: HostHandle) {
        *self.host.lock().unwrap() = Some(host);
    }

    /// Stop and forget the running host (dropping the handle signals its accept loop
    /// to shut down and closes the endpoint). Idempotent.
    pub fn stop_host(&self) {
        self.host.lock().unwrap().take();
        // With no host, the durable store is authoritative for the devices UI.
        self.auth.lock().unwrap().take();
    }

    /// The pairing ticket to encode as the QR, once the host has bound. `None` while
    /// disabled or during the brief async bind before the endpoint is ready.
    pub fn pairing_ticket(&self) -> Option<PairingTicket> {
        self.host.lock().unwrap().as_ref().map(|h| h.ticket().clone())
    }

    /// Register `id`→`conn` and return the binding **iff remote is enabled**;
    /// `None` when disabled, so the caller does no work and holds no binding.
    pub fn bind(&self, id: &str, conn: Arc<dyn AgentConnection>) -> Option<RemoteBinding> {
        if !self.enabled() {
            return None;
        }
        let handle = self.registry.register(id.to_string(), conn);
        Some(RemoteBinding { registry: self.registry.clone(), handle })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use oximux_agents::thread::{StubConnection, ThreadEvent};
    use oximux_remote_host::{AppPubkey, StoredDevice};

    use super::*;

    fn a_conn() -> Arc<dyn AgentConnection> {
        Arc::new(StubConnection::default())
    }

    /// Counts `load` calls so the *wiring* is covered, not just `remote-host`'s
    /// adapter: if `prepare_host` ever stops seeding from the durable store, a phone
    /// paired in an earlier run would silently lose authorization. Also records
    /// revocations so the no-host revoke path is checked.
    #[derive(Default)]
    struct RecordingStore {
        loads: AtomicUsize,
        devices: Mutex<Vec<StoredDevice>>,
        revoked: Mutex<Vec<AppPubkey>>,
    }

    impl DeviceStore for RecordingStore {
        fn load(&self) -> Vec<StoredDevice> {
            self.loads.fetch_add(1, Ordering::Relaxed);
            self.devices.lock().unwrap().clone()
        }
        fn save(&self, _device: &StoredDevice) {}
        fn set_revoked(&self, pubkey: &AppPubkey, revoked: bool) {
            if revoked {
                self.revoked.lock().unwrap().push(*pubkey);
            }
        }
    }

    fn a_device(byte: u8, name: &str, revoked: bool) -> StoredDevice {
        StoredDevice { pubkey: [byte; 32], name: name.into(), sessions: None, revoked }
    }

    /// Every bind seeds its auth store from durable device persistence.
    #[test]
    fn prepare_host_seeds_auth_from_the_durable_device_store() {
        let store = Arc::new(RecordingStore::default());
        let rc = RemoteControl::with_devices(store.clone());

        let _ = rc.prepare_host();

        assert_eq!(store.loads.load(Ordering::Relaxed), 1, "seeded from the durable devices");
    }

    /// With no host bound, the devices list still reads durable storage (so a device
    /// can be reviewed/cut off without turning remote access on) and hides revoked ones.
    #[test]
    fn paired_devices_reads_durable_storage_when_no_host_is_bound() {
        let store = Arc::new(RecordingStore::default());
        store.devices.lock().unwrap().extend([
            a_device(0x11, "phone", false),
            a_device(0x22, "old-tablet", true),
        ]);
        let rc = RemoteControl::with_devices(store);

        let listed = rc.paired_devices();

        assert_eq!(listed.len(), 1, "revoked devices are not offered");
        assert_eq!(listed[0].1, "phone");
    }

    /// Revoking with no host bound still persists, so it survives into the next bind.
    #[test]
    fn revoke_without_a_host_writes_through_to_storage() {
        let store = Arc::new(RecordingStore::default());
        store.devices.lock().unwrap().push(a_device(0x11, "phone", false));
        let rc = RemoteControl::with_devices(store.clone());

        rc.revoke_device(&[0x11; 32]);

        assert_eq!(store.revoked.lock().unwrap().as_slice(), &[[0x11; 32]], "revocation persisted");
    }

    /// Re-enabling rotates the pairing secret, so a QR captured earlier stops working.
    #[test]
    fn each_prepare_mints_a_fresh_pairing_secret() {
        let rc = RemoteControl::new();

        let (_, first) = rc.prepare_host();
        let (_, second) = rc.prepare_host();

        assert_ne!(first, second, "a stale pairing code must not stay valid across enables");
    }

    /// Disabled is the default and binds nothing — the per-event path stays free.
    #[test]
    fn disabled_binds_nothing() {
        let rc = RemoteControl::new();
        assert!(!rc.enabled());
        assert!(rc.bind("agent-1", a_conn()).is_none());
        assert!(rc.registry().is_empty(), "no session registered while disabled");
    }

    /// Enabled binds a session whose teed events reach a live subscriber in order.
    #[test]
    fn enabled_binds_and_tees_in_order() {
        let rc = RemoteControl::new();
        rc.set_enabled(true);

        let binding = rc.bind("agent-1", a_conn()).expect("bound while enabled");
        let mut rx = rc.registry().subscribe("agent-1").expect("registered");

        binding.ingest(ThreadEvent::AssistantText("hi".into()));
        binding.ingest(ThreadEvent::AssistantText(" there".into()));

        let (seq1, ev1) = rx.try_recv().expect("first teed event");
        assert_eq!(seq1, 1);
        assert_eq!(ev1, ThreadEvent::AssistantText("hi".into()));
        assert_eq!(rx.try_recv().expect("second teed event").0, 2, "seq advances");

        binding.unregister("agent-1");
        assert!(rc.registry().get("agent-1").is_none(), "unregister evicts the session");
    }
}
