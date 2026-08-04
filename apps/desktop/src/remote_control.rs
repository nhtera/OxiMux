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

pub mod launch_bridge;
pub mod local_listener;
pub mod project_provider;
pub mod rewind_bridge;
pub mod session_catalog;
pub mod relay_terminals;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use gpui::Global;

/// Settings key for the master switch. Persisted so remote access survives a
/// relaunch — without it a paired phone silently cannot reach a restarted
/// desktop until someone opens Settings and flips the toggle by hand.
pub const ENABLED_SETTING: &str = "remote.enabled";

/// Settings key for the keep-awake companion. Separate from the agents-running
/// preference on purpose: they answer different questions, and one silently
/// governing the other would make turning off a notification setting stop a
/// paired phone from reaching this Mac.
pub const KEEP_AWAKE_SETTING: &str = "remote.keep_awake";

/// Settings key for local CLI access — the owner-only control socket the
/// `oximux` CLI dials. Its own switch, independent of remote in both
/// directions: pairing a phone must not open a local control surface, and
/// turning local access off must not cut a paired phone.
///
/// **Default ON** — see [`local_access_enabled`]. A CLI that does nothing until
/// someone finds a settings toggle is a CLI most people never discover works.
pub const LOCAL_ENABLED_SETTING: &str = "local.enabled";

/// Should local CLI access be running, given whatever is stored under
/// [`LOCAL_ENABLED_SETTING`]?
///
/// Absent means ON. This is a *default*, not a coercion: an explicit `"false"`
/// keeps it off across restarts, so a user who turns it off stays turned off.
///
/// What the default admits, stated where the decision is made: the socket is a
/// 0700 directory and a 0600 node, and a caller must still prove a token it can
/// only have by reading that file. But any program running as this user can read
/// it — including the agents this desktop spawns — so on-by-default means an
/// agent that goes looking can drive every session and terminal. The per-session
/// credential in `oximux-remote-local` exists to close exactly that and is not
/// yet wired at spawn; until it is, the toggle's own copy is what tells the user.
pub fn local_access_enabled(stored: Option<&str>) -> bool {
    stored.is_none_or(|v| v == "true")
}

/// How long an opened pairing window stays redeemable.
///
/// Long enough to walk to the phone, unlock it, and open the app; short enough
/// that a code left on an unattended screen stops working before the desk does.
pub const PAIRING_WINDOW_SECS: u64 = 5 * 60;

/// Wall clock in unix seconds, saturating at the epoch on a clock set before it.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
use futures::channel::mpsc;
use oximux_agents::session_registry::{
    RemoteChoice, RemotePrompt, SessionHandle, SessionMeta, SessionRegistry,
};
use oximux_agents::thread::AgentConnection;
use oximux_remote_host::AudioTranscriber;
use oximux_remote_host::ProjectProvider;
use oximux_remote_host::TerminalSource;
use oximux_remote_host::RewindService;
use oximux_remote_host::{SessionLauncher, 
    AppPubkey, AuthStore, DeviceInfo, DeviceStore, Dispatcher, PairingEvent, PairingSlot,
    mint_pairing_secret,
};
use tokio::sync::broadcast;
use oximux_remote_iroh::HostHandle;
use oximux_remote_proto::PairingTicket;

/// Monotonic source of *placeholder* remote session ids, for a chat that has not
/// yet been given an agent session id of its own.
///
/// Positional and per-process, so it names a view's build order rather than the
/// conversation — worthless to a client once the desktop restarts. Everything with
/// a real id uses that instead ([`remote_session_id_for`]); this only has to be
/// collision-free for the run it exists in.
static REMOTE_SEQ: AtomicU64 = AtomicU64::new(1);

/// Mint the next placeholder remote session id (`"agent-N"`), for a chat with no
/// agent session id yet. Prefer [`remote_session_id_for`], which falls back here.
pub fn next_remote_session_id() -> String {
    format!("agent-{}", REMOTE_SEQ.fetch_add(1, Ordering::Relaxed))
}

/// The id a session is known by remotely.
///
/// Derived from the agent's own session id whenever there is one, because a
/// remote client's reference to a session has to survive a desktop restart. The
/// counter cannot promise that: it numbers views in the order they happen to be
/// built, so `agent-3` is a different conversation on every launch — and after a
/// restart a phone holding one is pointing at whichever chat was third this time.
/// The agent's id is minted once, keys the persisted transcript, and drives
/// `--resume`, so it already names the conversation rather than its position.
///
/// A chat that has never completed a turn has no such id yet and falls back to
/// the counter. It picks up a stable one the moment the agent mints it (see
/// `AgentChatView::rekey_remote_session`), which is also the first moment it has
/// any history worth reaching from another device.
pub fn remote_session_id_for(agent_session_id: Option<&str>) -> String {
    match agent_session_id {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => next_remote_session_id(),
    }
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

    /// Publish the folded transcript so a remote client opening this session sees
    /// its full history — including one restored from disk after a restart, which
    /// never entered the event ring. `entries_json` is the folded `Vec<ThreadEntry>`
    /// as JSON; the registry pairs it with the current seq for the client's resume.
    pub fn publish_transcript(&self, entries_json: String, model: Option<String>) {
        self.handle.publish_transcript(entries_json, model);
    }

    /// Register the sink that relays remotely-injected prompts (phone sends) back to
    /// the view, so the desktop renders the user's own bubble instead of a reply to
    /// a prompt it never showed.
    pub fn set_prompt_sink(&self, tx: mpsc::UnboundedSender<RemotePrompt>) {
        self.handle.set_remote_prompt_sink(tx);
    }

    /// Register the sink that carries a model/permission-mode change the backend
    /// refused in-session to the view, which completes it by respawning the child
    /// resumed on the new pick — the same recovery the desktop's own picker does.
    /// Without this a remote picker could not switch the model of a Claude or
    /// Codex session at all, since both fix it at spawn.
    pub fn set_choice_sink(&self, tx: mpsc::UnboundedSender<RemoteChoice>) {
        self.handle.set_remote_choice_sink(tx);
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
    /// The desktop's terminals, when a relay client is available to serve them.
    ///
    /// `None` leaves every terminal RPC answering `Unauthorized`, which is the
    /// correct degradation: a desktop running without the PTY daemon has no
    /// terminals to expose, and saying so distinctly would let an unauthorized
    /// client probe the host's configuration.
    terminals: Option<Arc<dyn TerminalSource>>,
    /// The desktop's session-launch path, when one is installed. `None` answers
    /// `CreateSession` with `Unauthorized` for the same reason `terminals` does:
    /// whether this desktop can start sessions is not a fact an unauthorized
    /// client should be able to establish.
    launcher: Option<Arc<dyn SessionLauncher>>,
    /// The desktop's rewind path, when one is installed. `None` answers
    /// `RewindSession` with `Unauthorized`, as `launcher` does.
    rewinder: Option<Arc<dyn RewindService>>,
    /// The desktop's schedule store, when one is installed. `None` answers the
    /// schedule RPCs with `Unauthorized`, as `launcher` does — whether this
    /// desktop keeps schedules is not a fact an unauthorized client should be able
    /// to establish. Shares the app's one SQLite connection, so a schedule the
    /// phone creates is the same row the desktop's ticker fires and its Settings
    /// pane lists.
    schedules: Option<Arc<oximux_agents::schedule::ScheduleStore>>,
    /// The desktop's speech-to-text engine, when one is installed. `None` answers
    /// `TranscribeAudio` with `Unauthorized`, as `launcher` does. Shares the
    /// composer's own model manager, so a model downloaded in Settings › Voice is
    /// immediately usable from the phone, and a phone dictation runs through the
    /// exact engine a desktop one does.
    transcriber: Option<Arc<dyn AudioTranscriber>>,
    /// The desktop's project list, when the host exposes it. `None` serves an empty
    /// list — an authorized client simply sees no quick-start projects, which is
    /// honest for a desktop with none; the create path stays gated regardless.
    projects: Option<Arc<dyn ProjectProvider>>,
    /// The index of persisted-but-unbuilt sessions, so a client is not limited to
    /// the projects the desktop has happened to show this run.
    catalog: Option<Arc<dyn oximux_remote_host::catalog::SessionCatalog>>,
    /// The live host's auth store while one is bound, so the paired-devices UI can
    /// revoke against the *running* host (the dispatcher rechecks authorization on
    /// every RPC, so a revoke lands mid-session). Cleared on stop — with no host, the
    /// durable store is authoritative.
    auth: Mutex<Option<Arc<AuthStore>>>,
    /// Prevents idle sleep while the host is bound — a phone can only reach a Mac
    /// that is awake, so without this the feature simply stops working a few
    /// minutes after the user walks away. Held for the host's lifetime and
    /// released with it; gated by its own setting, so it does not disturb (or get
    /// disturbed by) the keep-awake-while-agents-run preference.
    awake: Mutex<Option<crate::agent_awake::AwakeHold>>,
    /// Seeds the iroh endpoint key so the host keeps ONE identity across binds and
    /// restarts. Without it iroh mints a random key per bind, changing the endpoint
    /// id — and since a paired phone dials by that id, every restart would silently
    /// invalidate every pairing. `None` = ephemeral identity (tests).
    endpoint_secret: Option<[u8; 32]>,
    /// Local CLI access — a second, independent switch beside `enabled`. Either
    /// one populates the session registry; both off keeps the tested
    /// `disabled_binds_nothing` invariant: no registry rows, no socket, no token.
    local_enabled: AtomicBool,
    /// The running local listener, once bound. Dropping the handle aborts the
    /// accept loop AND every in-flight CLI connection — the local analogue of
    /// dropping the iroh `HostHandle`.
    local: Mutex<Option<local_listener::LocalHandle>>,
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
            terminals: None,
            launcher: None,
            rewinder: None,
            schedules: None,
            transcriber: None,
            projects: None,
            catalog: None,
            auth: Mutex::new(None),
            awake: Mutex::new(None),
            endpoint_secret: None,
            local_enabled: AtomicBool::new(false),
            local: Mutex::new(None),
        }
    }

    /// Pin the host's endpoint identity from the persistent host key, so a device
    /// paired in an earlier run can still reach it.
    pub fn with_endpoint_secret(mut self, secret: [u8; 32]) -> Self {
        self.endpoint_secret = Some(secret);
        self
    }

    /// The boot constructor: same as [`new`](Self::new) but backed by durable
    /// paired-device storage, so a phone paired in an earlier run stays authorized
    /// across restarts and toggle cycles.
    pub fn with_devices(devices: Arc<dyn DeviceStore>) -> Self {
        Self { devices: Some(devices), ..Self::new() }
    }

    /// Install the terminal source the host serves. Called once at boot, after
    /// the relay client comes up.
    pub fn set_launcher(&mut self, launcher: Arc<dyn SessionLauncher>) {
        self.launcher = Some(launcher);
    }

    /// Install the rewind service the host serves. Called once at boot.
    pub fn set_rewinder(&mut self, rewinder: Arc<dyn RewindService>) {
        self.rewinder = Some(rewinder);
    }

    /// Install the schedule store the host serves. Called once at boot with the
    /// same store the desktop's scheduler ticks, so both surfaces read and write
    /// the same rows.
    pub fn set_schedule_store(&mut self, schedules: Arc<oximux_agents::schedule::ScheduleStore>) {
        self.schedules = Some(schedules);
    }

    /// Install the transcriber the host serves. Called once at boot with a
    /// transcriber that shares the composer's model manager, so both dictation
    /// surfaces decode with the same model.
    pub fn set_transcriber(&mut self, transcriber: Arc<dyn AudioTranscriber>) {
        self.transcriber = Some(transcriber);
    }

    pub fn set_terminals(&mut self, terminals: Arc<dyn TerminalSource>) {
        self.terminals = Some(terminals);
    }

    /// Install the project provider the host serves. Called once at boot, backed
    /// by the same project store the desktop sidebar lists — so the phone's
    /// quick-start projects match what the desktop shows.
    pub fn set_project_provider(&mut self, projects: Arc<dyn ProjectProvider>) {
        self.projects = Some(projects);
    }

    /// Let the host see and open sessions whose views have not been built.
    pub fn set_session_catalog(&mut self, catalog: Arc<dyn oximux_remote_host::catalog::SessionCatalog>) {
        self.catalog = Some(catalog);
    }

    /// The shared session registry (the host serves from this same instance).
    pub fn registry(&self) -> Arc<SessionRegistry> {
        self.registry.clone()
    }

    /// Drop a session from the registry by id.
    ///
    /// For the one case a [`RemoteBinding`] cannot cover: a view re-keying itself
    /// onto the agent's own session id has to remove the entry it was registered
    /// under before, and by then its binding is already pointing at the new one.
    /// Ordinary teardown goes through the binding, which carries the registry
    /// with it precisely so `Drop` needs no context.
    pub fn unregister(&self, id: &str) {
        self.registry.unregister(id);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Release);
    }

    pub fn local_enabled(&self) -> bool {
        self.local_enabled.load(Ordering::Acquire)
    }

    pub fn set_local_enabled(&self, on: bool) {
        self.local_enabled.store(on, Ordering::Release);
    }

    /// Assemble a fresh dispatcher + CSPRNG seed for a host bind. Serves
    /// **already-paired** devices and opens no pairing window.
    ///
    /// Binding never admits a new device on its own, whether the user just flipped
    /// the toggle or the app restored remote access at launch. Opening a window
    /// implicitly would leave the desktop claimable by anyone who can reach the
    /// code without the user ever asking to pair — and it is why adding a second
    /// device used to mean toggling off and on, which tears the endpoint down
    /// under the devices already connected through it. Pairing is its own explicit
    /// act: see [`open_pairing`](Self::open_pairing).
    ///
    /// Existing devices authenticate against the durable store by pubkey and never
    /// need the pairing secret, so they reconnect while the window stays shut. A
    /// fresh [`AuthStore`] per bind is still seeded from that durable store, so
    /// they survive toggle cycles and restarts.
    ///
    /// The secret returned is what `start_host` mints its ticket from; with no slot
    /// seeded it redeems to nothing, and `open_pairing` supplies the live one.
    pub fn prepare_host(&self) -> (Arc<Dispatcher>, [u8; 16]) {
        let auth = self.seeded_auth();
        (Arc::new(self.dispatcher(auth)), mint_pairing_secret())
    }

    /// Open a pairing window on the **running** host and return the ticket to show.
    /// `None` if no host is bound yet.
    ///
    /// Deliberately does not touch the endpoint. The endpoint id is pinned to the
    /// durable host identity and the secret is just a value in the live
    /// [`AuthStore`], so a new code costs a mutex rather than a rebind — devices
    /// already connected keep their sessions while a new one is admitted.
    pub fn open_pairing(&self) -> Option<PairingTicket> {
        // Both locks are taken and released one at a time; holding either across
        // the other would pair this path with `set_host`/`stop_host` for no reason.
        let auth = self.auth.lock().unwrap().as_ref().cloned()?;
        let endpoint_id = self.host.lock().unwrap().as_ref()?.ticket().endpoint_id;
        let secret = mint_pairing_secret();
        // ONE-TIME and short-lived: the code dies on the first scan, and expires
        // unscanned so a QR left on screen stops being an open invitation.
        auth.set_pairing(PairingSlot::expiring(secret, None, true, now_secs() + PAIRING_WINDOW_SECS));
        Some(PairingTicket { endpoint_id, handshake_secret: secret, session_id: None })
    }

    /// Withdraw the pairing window. Idempotent, and a no-op with no host bound.
    pub fn close_pairing(&self) {
        if let Some(auth) = self.auth.lock().unwrap().as_ref() {
            auth.close_pairing();
        }
    }

    /// Build a dispatcher over the shared registry, exposing terminals and the
    /// session launcher when they are installed. Both host-preparation paths go
    /// through here so a resumed host cannot quietly serve a different surface
    /// than a fresh one.
    fn dispatcher(&self, auth: Arc<AuthStore>) -> Dispatcher {
        let mut dispatcher = Dispatcher::new(self.registry.clone(), auth);
        if let Some(terminals) = &self.terminals {
            dispatcher = dispatcher.with_terminals(Arc::clone(terminals));
        }
        if let Some(launcher) = &self.launcher {
            dispatcher = dispatcher.with_launcher(Arc::clone(launcher));
        }
        if let Some(rewinder) = &self.rewinder {
            dispatcher = dispatcher.with_rewinder(Arc::clone(rewinder));
        }
        if let Some(schedules) = &self.schedules {
            dispatcher = dispatcher.with_schedule_store(Arc::clone(schedules));
        }
        if let Some(transcriber) = &self.transcriber {
            dispatcher = dispatcher.with_transcriber(Arc::clone(transcriber));
        }
        if let Some(projects) = &self.projects {
            dispatcher = dispatcher.with_projects(Arc::clone(projects));
        }
        if let Some(catalog) = &self.catalog {
            dispatcher = dispatcher.with_catalog(Arc::clone(catalog));
        }
        dispatcher
    }

    /// A fresh auth store seeded from the durable device list, retained so the
    /// paired-devices UI can revoke against the live host.
    fn seeded_auth(&self) -> Arc<AuthStore> {
        let auth = Arc::new(match &self.devices {
            Some(devices) => AuthStore::with_store(devices.clone()),
            None => AuthStore::new(),
        });
        *self.auth.lock().unwrap() = Some(auth.clone());
        auth
    }

    /// Paired devices to list in the UI, revoked ones excluded. Reads the live auth
    /// store while a host is bound (so the list matches exactly what that host will
    /// accept) and falls back to durable storage when remote is off — you can review,
    /// revoke, and re-tier a device without turning remote access on.
    pub fn paired_devices(&self) -> Vec<DeviceInfo> {
        if let Some(auth) = self.auth.lock().unwrap().as_ref() {
            return auth.devices();
        }
        match &self.devices {
            // Revoked devices are listed here too, matching the live store: the
            // record is what a forget acts on, so hiding it would put the device
            // beyond reach of the only action that can undo the revocation.
            Some(store) => store
                .load()
                .into_iter()
                .map(|d| DeviceInfo {
                    pubkey: d.pubkey,
                    name: d.name,
                    read_only: d.read_only,
                    last_seen: d.last_seen,
                    revoked: d.revoked,
                })
                .collect(),
            None => Vec::new(),
        }
    }

    /// Move a device between the read-write and read-only tiers. Against a live host
    /// this takes effect on its next RPC (the dispatcher rechecks per call, so an
    /// open connection is downgraded mid-session); with no host bound it goes
    /// straight to durable storage so the next bind seeds it.
    pub fn set_device_read_only(&self, pubkey: &AppPubkey, read_only: bool) {
        if let Some(auth) = self.auth.lock().unwrap().as_ref() {
            auth.set_read_only(pubkey, read_only);
            return;
        }
        if let Some(store) = &self.devices {
            store.set_read_only(pubkey, read_only);
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

    /// Erase a device's record, cutting it off now and letting it pair again.
    ///
    /// The host refuses to `Register` a key it already knows — including a revoked
    /// one — so without this a device could never be re-paired: revoking a phone
    /// and then wanting it back was a dead end, and so was a phone that dropped
    /// its own side of the pairing. Erasing the record is the deliberate host-side
    /// undo that check is written to require.
    pub fn forget_device(&self, pubkey: &AppPubkey) {
        if let Some(auth) = self.auth.lock().unwrap().as_ref() {
            auth.forget(pubkey);
            return;
        }
        if let Some(store) = &self.devices {
            store.remove(pubkey);
        }
    }

    /// Is the displayed pairing code still redeemable? `false` once a device has
    /// used it, so the UI can stop showing a dead code.
    pub fn pairing_open(&self) -> bool {
        self.auth.lock().unwrap().as_ref().is_some_and(|auth| auth.pairing_open())
    }

    /// Watch for devices completing pairing, so the desktop can confirm each one.
    /// `None` when no host has been prepared yet.
    pub fn subscribe_pairings(&self) -> Option<broadcast::Receiver<PairingEvent>> {
        self.auth.lock().unwrap().as_ref().map(|auth| auth.subscribe_pairings())
    }

    /// The seed that pins the host's endpoint id, if one was installed at boot.
    pub fn endpoint_secret(&self) -> Option<[u8; 32]> {
        self.endpoint_secret
    }

    /// Store the freshly-bound host, stopping and replacing any prior one.
    ///
    /// Taking the keep-awake hold here rather than at the toggle ties it to the
    /// host actually being bound: a bind that fails leaves no hold to leak, and
    /// the assertion never outlives the thing it exists to keep reachable.
    pub fn set_host(&self, host: HostHandle) {
        *self.host.lock().unwrap() = Some(host);
        *self.awake.lock().unwrap() = Some(crate::agent_awake::global().acquire_remote());
    }

    /// Stop and forget the running host (dropping the handle signals its accept loop
    /// to shut down and closes the endpoint). Idempotent.
    pub fn stop_host(&self) {
        self.host.lock().unwrap().take();
        // With no host, the durable store is authoritative for the devices UI.
        self.auth.lock().unwrap().take();
        // Nothing left to stay awake for; the guard releases on drop.
        self.awake.lock().unwrap().take();
    }

    /// The pairing ticket to encode as the QR, once the host has bound. `None` while
    /// disabled or during the brief async bind before the endpoint is ready.
    pub fn pairing_ticket(&self) -> Option<PairingTicket> {
        self.host.lock().unwrap().as_ref().map(|h| h.ticket().clone())
    }

    /// Restore remote access at launch: flip the flag on and bind the iroh
    /// endpoint off-thread, so already-paired phones can dial a freshly-started
    /// desktop.
    ///
    /// Flag and bind travel together deliberately. Setting the flag alone would
    /// leave the UI showing remote as on while nothing was listening; binding
    /// alone would listen while [`Self::bind`] refused to register a single
    /// session. Either half on its own is the bug this exists to prevent.
    ///
    /// A missing tokio runtime is a no-op rather than a panic: iroh needs one to
    /// bind, and the app is still perfectly usable without remote access.
    pub fn resume_at_launch(cx: &mut gpui::App) {
        let Some(rc) = cx.try_global::<RemoteControl>() else {
            return;
        };
        rc.set_enabled(true);
        let (dispatcher, secret) = rc.prepare_host();
        let endpoint_secret = rc.endpoint_secret();
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            tracing::warn!("no tokio runtime; remote host not bound");
            return;
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        rt.spawn(async move {
            let _ = tx.send(oximux_remote_iroh::start_host(dispatcher, secret, endpoint_secret).await);
        });
        cx.spawn(async move |cx| {
            match rx.await {
                Ok(Ok(host)) => cx.update(|cx| {
                    if let Some(rc) = cx.try_global::<RemoteControl>() {
                        rc.set_host(host);
                    }
                }),
                // Say why rather than leaving the pane spinning on
                // "Starting the pairing host…" forever.
                Ok(Err(err)) => tracing::warn!(%err, "remote host failed to bind"),
                Err(_) => tracing::warn!("remote host bind task dropped"),
            }
        })
        .detach();
    }

    /// Register `id`→`conn` and return the binding **iff some control surface
    /// is enabled** — remote, local CLI, or both. `None` with both off, so the
    /// caller does no work and holds no binding: population is gated, never
    /// unconditional, which is what keeps `disabled_binds_nothing` true for
    /// users who enabled neither.
    pub fn bind(&self, id: &str, conn: Arc<dyn AgentConnection>) -> Option<RemoteBinding> {
        if !(self.enabled() || self.local_enabled()) {
            return None;
        }
        let handle = self.registry.register(id.to_string(), conn);
        Some(RemoteBinding { registry: self.registry.clone(), handle })
    }

    /// Assemble the dispatcher a local listener serves. Shares the live host's
    /// auth store when one is bound (so the paired-device ACL the UI edits is
    /// the one a hypothetical remote check consults), else builds one seeded
    /// from durable storage WITHOUT retaining it — `self.auth` means "the live
    /// remote host's store" to the pairing UI, and the local listener must not
    /// impersonate that.
    fn prepare_local(&self) -> Arc<Dispatcher> {
        let auth = self.auth.lock().unwrap().clone().unwrap_or_else(|| {
            Arc::new(match &self.devices {
                Some(devices) => AuthStore::with_store(devices.clone()),
                None => AuthStore::new(),
            })
        });
        Arc::new(self.dispatcher(auth))
    }

    /// Flip local CLI access on and bind the control socket. The flag and the
    /// bind travel together for the reason [`resume_at_launch`] documents; a
    /// missing tokio runtime or data dir is a warn-and-degrade, not a panic.
    ///
    /// [`resume_at_launch`]: Self::resume_at_launch
    pub fn start_local(cx: &mut gpui::App) {
        let Some(rc) = cx.try_global::<RemoteControl>() else {
            return;
        };
        rc.set_local_enabled(true);
        let dispatcher = rc.prepare_local();
        let Some(dir) = crate::app_paths::data_dir() else {
            tracing::warn!("no data dir; local control socket not bound");
            return;
        };
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            tracing::warn!("no tokio runtime; local control socket not bound");
            return;
        };
        match local_listener::start(dispatcher, dir, rt) {
            Ok(handle) => *rc.local.lock().unwrap() = Some(handle),
            // The flag stays on so the UI reflects the user's choice, but with
            // nothing bound the CLI reports the host unreachable — which is
            // true, and better than a toggle that looks on and answers nothing.
            Err(err) => tracing::warn!(%err, "local control socket failed to bind"),
        }
    }

    /// Mint a credential confining one agent process to `session_id`, for the
    /// spawner to inject into that process's environment. `None` when local
    /// access is off — there is no listener to honor it, and an agent that
    /// receives no credential simply cannot reach the control socket.
    ///
    /// **Nothing calls this yet, and the confinement is therefore not in force:**
    /// an agent that runs `oximux` reads the operator token file (it is the same
    /// OS user) and is served full scope. Wiring it is not a matter of finding
    /// the spawn site. A chat's `remote_session_id` starts as a placeholder and
    /// is REKEYED once the agent reports its own id
    /// (`rekey_remote_session_if_needed`), while a child's environment is fixed
    /// at spawn and cannot be revised afterwards — so either the credential is
    /// minted against a spawn-time identity the listener then re-maps on rekey,
    /// or the id has to be known before the process exists. That choice belongs
    /// with the phase that wires it, not here.
    #[allow(dead_code, reason = "the agent spawner does not inject credentials yet")]
    pub fn grant_agent_credential(&self, session_id: &str) -> Option<String> {
        self.local.lock().unwrap().as_ref().map(|h| h.grant_session(session_id))
    }

    /// Drop an agent's credential when its session ends. Unused for as long as
    /// [`grant_agent_credential`](Self::grant_agent_credential) is.
    #[allow(dead_code, reason = "the agent spawner does not inject credentials yet")]
    pub fn revoke_agent_credential(&self, session_id: &str) {
        if let Some(handle) = self.local.lock().unwrap().as_ref() {
            handle.revoke_session(session_id);
        }
    }

    /// Flip local CLI access off: the listener, its socket file, and every
    /// in-flight CLI connection go down with the handle. Idempotent.
    pub fn stop_local(&self) {
        self.set_local_enabled(false);
        self.local.lock().unwrap().take();
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
        read_only: Mutex<Vec<(AppPubkey, bool)>>,
        removed: Mutex<Vec<AppPubkey>>,
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
        fn set_read_only(&self, pubkey: &AppPubkey, read_only: bool) {
            self.read_only.lock().unwrap().push((*pubkey, read_only));
        }
        fn remove(&self, pubkey: &AppPubkey) {
            self.removed.lock().unwrap().push(*pubkey);
        }
        fn touch_last_seen(&self, _pubkey: &[u8; 32]) {}
    }

    fn a_device(byte: u8, name: &str, revoked: bool) -> StoredDevice {
        StoredDevice {
            pubkey: [byte; 32],
            name: name.into(),
            sessions: None,
            revoked,
            read_only: false,
            last_seen: None,
        }
    }

    /// Every bind seeds its auth store from durable device persistence.
    #[test]
    fn prepare_host_seeds_auth_from_the_durable_device_store() {
        let store = Arc::new(RecordingStore::default());
        let rc = RemoteControl::with_devices(store.clone());

        let _ = rc.prepare_host();

        assert_eq!(store.loads.load(Ordering::Relaxed), 1, "seeded from the durable devices");
        assert!(!rc.pairing_open(), "binding alone never admits a new device");
    }

    /// Asking to pair with no host bound must open nothing. The pane shows a
    /// "starting" state and re-asks once the endpoint is up; a window minted here
    /// would be a live code with no endpoint able to serve it.
    #[test]
    fn opening_a_pairing_window_needs_a_bound_host() {
        let rc = RemoteControl::with_devices(Arc::new(RecordingStore::default()));
        let _ = rc.prepare_host();

        assert!(rc.open_pairing().is_none(), "no host, no ticket");
        assert!(!rc.pairing_open(), "and no window was left open behind it");
    }

    /// Binding a host must serve already-paired devices without opening a pairing
    /// window — otherwise merely turning remote access on (or relaunching) would
    /// silently leave the desktop claimable by anyone who can reach the code.
    #[test]
    fn a_bound_host_serves_paired_devices_but_opens_no_pairing_window() {
        let store = Arc::new(RecordingStore::default());
        store.devices.lock().unwrap().push(a_device(0x11, "phone", false));
        let rc = RemoteControl::with_devices(store.clone());

        let _ = rc.prepare_host();

        assert!(!rc.pairing_open(), "no pairing window from a bind");
        assert_eq!(store.loads.load(Ordering::Relaxed), 1, "still seeded from durable devices");
        assert_eq!(
            rc.paired_devices().len(),
            1,
            "the already-paired phone is still authorized and can reconnect",
        );
    }

    /// With no host bound, the devices list still reads durable storage — so a
    /// device can be reviewed and cut off without turning remote access on.
    ///
    /// Revoked devices are listed, flagged rather than hidden. This deliberately
    /// reverses the earlier "revoked devices are not offered": the host refuses to
    /// register a key it already knows, revoked included, so hiding the row put
    /// the device beyond reach of the only action that can undo the revocation and
    /// made revoking silently permanent.
    #[test]
    fn paired_devices_reads_durable_storage_when_no_host_is_bound() {
        let store = Arc::new(RecordingStore::default());
        store.devices.lock().unwrap().extend([
            a_device(0x11, "phone", false),
            a_device(0x22, "old-tablet", true),
        ]);
        let rc = RemoteControl::with_devices(store);

        let listed = rc.paired_devices();

        assert_eq!(listed.len(), 2, "revoked devices stay listed so they can be forgotten");
        let phone = listed.iter().find(|d| d.name == "phone").expect("active device listed");
        assert!(!phone.revoked, "the active device is not flagged");
        assert!(!phone.read_only, "a paired device starts read-write");
        let tablet =
            listed.iter().find(|d| d.name == "old-tablet").expect("revoked device listed");
        assert!(tablet.revoked, "and the revoked one is flagged, not omitted");
    }

    /// Forgetting with no host bound erases the record durably, which is what lets
    /// that device pair again — a revocation alone leaves the key known forever.
    #[test]
    fn forget_without_a_host_erases_the_record() {
        let store = Arc::new(RecordingStore::default());
        store.devices.lock().unwrap().push(a_device(0x11, "phone", false));
        let rc = RemoteControl::with_devices(store.clone());

        rc.forget_device(&[0x11; 32]);

        assert_eq!(
            store.removed.lock().unwrap().as_slice(),
            &[[0x11; 32]],
            "the erasure reaches storage, so it survives into the next bind",
        );
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

    /// Re-tiering with no host bound still persists, so the next bind seeds it.
    #[test]
    fn set_device_read_only_without_a_host_writes_through_to_storage() {
        let store = Arc::new(RecordingStore::default());
        store.devices.lock().unwrap().push(a_device(0x11, "phone", false));
        let rc = RemoteControl::with_devices(store.clone());

        rc.set_device_read_only(&[0x11; 32], true);

        assert_eq!(
            store.read_only.lock().unwrap().as_slice(),
            &[([0x11; 32], true)],
            "the opt-down is persisted even with remote off",
        );
    }

    /// Re-enabling rotates the pairing secret, so a QR captured earlier stops working.
    #[test]
    fn each_prepare_mints_a_fresh_pairing_secret() {
        let rc = RemoteControl::new();

        let (_, first) = rc.prepare_host();
        let (_, second) = rc.prepare_host();

        assert_ne!(first, second, "a stale pairing code must not stay valid across enables");
    }

    /// Local CLI access is on unless the user turned it off.
    ///
    /// The absent case is the one that matters: a fresh install has no stored
    /// value and must still bind, or the CLI silently does nothing on the
    /// machine it was installed for. The explicit `"false"` case matters just as
    /// much in the other direction — a default that overrode a user's choice
    /// would turn the surface back on at every launch.
    #[test]
    fn local_access_defaults_on_but_respects_an_explicit_no() {
        assert!(local_access_enabled(None), "a fresh install runs the CLI socket");
        assert!(local_access_enabled(Some("true")));
        assert!(!local_access_enabled(Some("false")), "an explicit no must survive a restart");
        // Anything else is not a yes. The value is written by this app, so a
        // stray one means corruption or a hand edit, and the safe reading of
        // "I cannot tell" for a control surface is off.
        assert!(!local_access_enabled(Some("")), "an empty value is not consent");
        assert!(!local_access_enabled(Some("TRUE")), "only the value this app writes counts");
    }

    /// Disabled is the struct-level default and binds nothing — the per-event
    /// path stays free. Note this is about a freshly constructed
    /// `RemoteControl`, NOT about what a real launch does: the app turns local
    /// access on from `local_access_enabled` before any session binds.
    #[test]
    fn disabled_binds_nothing() {
        let rc = RemoteControl::new();
        assert!(!rc.enabled());
        assert!(!rc.local_enabled(), "local CLI access defaults off too");
        assert!(rc.bind("agent-1", a_conn()).is_none());
        assert!(rc.registry().is_empty(), "no session registered while disabled");
    }

    /// Local CLI access alone populates the registry — the gate is widened to
    /// either switch, not moved: with remote still off, sessions register for
    /// the local listener to serve.
    #[test]
    fn local_access_alone_binds_sessions() {
        let rc = RemoteControl::new();
        rc.set_local_enabled(true);
        assert!(!rc.enabled(), "remote stays off");
        assert!(rc.bind("agent-1", a_conn()).is_some());
        assert!(!rc.registry().is_empty());
        // And turning it off again restores the empty-gate behavior for NEW
        // sessions (live bindings are cut by the listener teardown, not here).
        rc.stop_local();
        assert!(rc.bind("agent-2", a_conn()).is_none());
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
