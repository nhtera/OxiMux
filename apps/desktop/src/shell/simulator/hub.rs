//! [`SimulatorHub`]: the app-wide executor around `oximux_simulator::Registry`.
//!
//! The registry decides; the hub does. Every [`Effect`] runs here — boots,
//! helper spawns and `simctl` calls on the background executor, never the UI
//! thread — and its result is fed back into the registry, whose next effects
//! run in turn. Panels subscribe to [`HubEvent`]s and read state by pull.
//!
//! The hub is the **only** consumer of each session's wake callback and event
//! receiver (a `StreamSession` has one of each); it fans them out as
//! [`HubEvent::Frame`] / [`HubEvent::Session`]. Viewers read frames with
//! `StreamSession::latest_frame`, which any number of them can call.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::{App, Context, Entity, EventEmitter, Global};
use oximux_simulator::availability::{self, Availability, HelperStatus};
use oximux_simulator::boot_watch::{BootWatch, WatchGate};
use oximux_simulator::child_ledger::Ledger;
use oximux_simulator::helper::HelperOptions;
use oximux_simulator::protocol::StreamFormat;
use oximux_simulator::registry::{self, BootResult, Effect, Generation, Phase, Registry, WorktreeKey};
use oximux_simulator::runner::SystemRunner;
use oximux_simulator::session::{HelperSession, SessionEvent, StreamSession};
use oximux_simulator::{DeviceId, DeviceInfo, DeviceState, SimError, Source, simctl};
use oximux_storage::SettingsRepo;

use crate::app_settings::sim_state_keys;

/// `simctl` call timeouts (boot has its own bounded poll inside).
const SIMCTL_TIMEOUT: Duration = Duration::from_secs(30);
/// How often devices are checked for parking and the idle shutdown (so a
/// hidden device parks 60–75 s after it was hidden).
const TICK: Duration = Duration::from_secs(15);
/// How long an opened device menu keeps phones watched. A lease, not a
/// count: gpui-kit does not report a menu closed when its button goes away
/// (an attach landing under an open empty-state menu), and a count stuck
/// above zero would poll `adb` until quit.
const MENU_PHONE_WATCH: Duration = Duration::from_secs(120);

/// What panels hear about.
#[derive(Clone, Debug, PartialEq)]
pub enum HubEvent {
    /// A device's [`Phase`] or attachments changed.
    Changed(DeviceId),
    /// New frame(s) for a device: repaint its viewers.
    Frame(DeviceId),
    /// A helper event (size, orientation, error, exit).
    Session(DeviceId, SessionEvent),
    /// The availability check finished (or was refreshed).
    Availability,
    /// The device list (for the device menu) was refreshed.
    Devices,
    /// An attach for this worktree found no device to use (the panel leaves
    /// its "Attaching…" state and shows why).
    AttachFailed(PathBuf, String),
    /// Something the user should hear about for this device (a saved
    /// screenshot, a failed paste): the window shows it as a toast.
    Notice(DeviceId, NoticeKind, String),
    /// A consent request was raised, answered or dropped: panels re-read
    /// [`SimulatorHub::consent_request`].
    Consent,
    /// The "Agent is using this device" badge came on or went off.
    AgentActivity(DeviceId),
    /// The device watcher saw devices boot that no worktree has attached (an
    /// agent's `simctl boot` or build, Simulator.app): a window may attach one
    /// where an agent is working, after claiming it (P9 auto-open).
    DeviceBooted(Vec<DeviceId>),
    /// A phone was plugged in, unplugged, or approved this Mac (the device
    /// list is being refreshed): a row may enable, a Reconnect may apply.
    PhysicalChanged,
}

pub struct SimulatorHub {
    registry: Registry<StreamSession>,
    repo: SettingsRepo,
    runner: Arc<SystemRunner>,
    ledger: Option<Arc<Ledger>>,
    /// Opens once startup's `reap_stale` is done: a session must not start
    /// before it, or its fresh ledger entry could be taken for an orphan.
    reaped: Arc<(Mutex<bool>, Condvar)>,
    watch: Arc<Mutex<BootWatch>>,
    availability: Option<Availability>,
    feature_used: bool,
    /// Latest attach request per worktree: an older request whose device
    /// listing lands later must not win over a newer one.
    attach_seq: HashMap<WorktreeKey, u64>,
    next_attach: u64,
    /// Last device listing, for the device menu.
    devices: Vec<DeviceInfo>,
    devices_listed: bool,
    /// Whether that listing asked `simctl`: false when it ran before Xcode
    /// was known (the Android SDK is often found first).
    ios_listed: bool,
    /// Listings are numbered as they start ([`Stamp`]); one that started
    /// before the listing already shown never replaces it, so an older
    /// Android-only listing cannot hide the iOS devices of a newer full one.
    list_seq: u64,
    landed_seq: u64,
    availability_in_flight: bool,
    /// Screen recordings in progress, one per device.
    recordings: HashMap<DeviceId, oximux_simulator::record::Recording>,
    /// Devices whose recording is starting (a second click is ignored).
    recording_starts: HashSet<DeviceId>,
    /// `simctl`'s path, once resolved (recordings spawn it directly).
    simctl: Option<PathBuf>,
    paste_lock: capture::PasteLock,
    /// Consent, the agent badge and device scales (see `agent`).
    agent: agent::AgentState,
    /// The Android SDK, once found (see `android`).
    android_sdk: Option<oximux_simulator::android::sdk::Sdk>,
    /// Booted devices a window already took for auto-attach (so one boot is
    /// attached once, not by every window); dropped when the device shuts down.
    boot_claims: HashSet<DeviceId>,
    /// Until when an opened device menu has phones watched, so a row enables
    /// when its phone approves this Mac (see [`MENU_PHONE_WATCH`]).
    phone_watch_until: Option<Instant>,
    /// A real device was attached this run: phones stay watched.
    physical_used: bool,
    /// The SDK in use has no emulator (a standalone `adb`): phones only.
    android_phones_only: bool,
    /// An SDK was found before and is gone now (a `brew upgrade` mid-way):
    /// the tick keeps looking.
    android_sdk_lost: bool,
    /// Attached phones whose screen is off (they stream nothing).
    screen_off: HashSet<DeviceId>,
    /// The phones (and their adb states) the last phone watch saw.
    phone_states: Option<std::collections::BTreeMap<String, oximux_simulator::android::adb::AdbState>>,
}

impl EventEmitter<HubEvent> for SimulatorHub {}

mod agent;
mod android;
mod capture;
mod lifecycle;

pub use agent::Wake;
pub(crate) use agent::InstallAnswer;
pub(crate) use android::list_all;
pub use capture::NoticeKind;
pub(crate) use capture::{CaptureKind, capture_dir, capture_path, home_button, paste_now, stamp};

pub use lifecycle::{install, on_quit};
pub(crate) use lifecycle::{is_udid, simulator_dir};
#[cfg(test)]
pub(crate) use lifecycle::install_for_test;

/// The global handle to the one hub.
pub struct SimulatorService(pub Entity<SimulatorHub>);

/// When a device listing started, and whether it asked `simctl` (see
/// [`SimulatorHub::begin_listing`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Stamp {
    seq: u64,
    ios: bool,
}

impl Global for SimulatorService {}

/// The hub, when installed (it is not in `oximux serve`, which has no UI).
pub fn hub(cx: &App) -> Option<Entity<SimulatorHub>> {
    cx.try_global::<SimulatorService>().map(|s| s.0.clone())
}

impl SimulatorHub {
    /// Claim a freshly booted device for auto-attach: true for the first
    /// caller only.
    pub fn claim_booted(&mut self, udid: &DeviceId) -> bool {
        self.boot_claims.insert(udid.clone())
    }

    /// The helper's version, from any live stream's `hello` (`None` until a
    /// device streams this run).
    pub fn helper_version(&self) -> Option<String> {
        self.registry.attached_devices().iter().find_map(|udid| self.session(udid)).and_then(|s| s.hello().map(|h| h.version.clone()))
    }

    pub fn availability(&self) -> Option<&Availability> {
        self.availability.as_ref()
    }

    pub fn device_for(&self, worktree: &Path) -> Option<DeviceId> {
        self.registry.device_for(&WorktreeKey::from_path(worktree)).cloned()
    }

    pub fn phase(&self, udid: &DeviceId) -> Phase {
        self.registry.phase(udid)
    }

    /// The live session for `udid` (viewers call `latest_frame` on it).
    pub fn session(&self, udid: &DeviceId) -> Option<StreamSession> {
        self.registry.session(udid).cloned()
    }

    /// The last device listing (see [`Self::refresh_devices`]).
    pub fn devices(&self) -> &[DeviceInfo] {
        &self.devices
    }

    /// The device attached to `worktree`, with its listing when known.
    pub fn attached_info(&self, worktree: &Path) -> Option<(DeviceId, Option<DeviceInfo>)> {
        let udid = self.device_for(worktree)?;
        let info = self.devices.iter().find(|d| d.udid == udid).cloned();
        Some((udid, info))
    }

    /// Re-list devices in the background (never on the UI thread; iOS never
    /// without a resolvable Xcode, which would pop the tools dialog).
    pub fn refresh_devices(&mut self, cx: &mut Context<Self>) {
        let (xcode_ok, sdk) = (self.watch_gate().xcode_ok, self.android_sdk.clone());
        if !xcode_ok && sdk.is_none() {
            // Nothing can be listed any more (the SDK was cleared on a Mac
            // without Xcode): what an earlier listing showed is gone too.
            let stamp = self.begin_listing(false);
            if self.devices_listed {
                self.land_listing(stamp, Vec::new(), cx);
            } else {
                // None shown yet: still turn away one in flight from before.
                self.landed_seq = stamp.seq;
            }
            return;
        }
        let stamp = self.begin_listing(xcode_ok);
        let runner = self.runner.clone();
        cx.spawn(async move |this, cx| {
            let listed = cx
                .background_executor()
                .spawn(async move { android::list_sides(runner.as_ref(), xcode_ok, sdk.as_ref(), SIMCTL_TIMEOUT) })
                .await;
            let _ = this.update(cx, |hub, cx| match listed {
                Ok((devices, ios_ok)) => {
                    if hub.land_listing(stamp, devices, cx) && ios_ok {
                        hub.forget_deleted_simulators(cx);
                    }
                }
                Err(e) => tracing::debug!("simulator device listing failed: {e}"),
            });
        })
        .detach();
    }

    /// A device menu opened or closed. While one is open (for at most
    /// [`MENU_PHONE_WATCH`]) the watcher also watches phones (`adb devices`),
    /// so an approval shows without reopening it.
    pub fn set_device_menu_open(&mut self, open: bool) {
        self.phone_watch_until = open.then(|| Instant::now() + MENU_PHONE_WATCH);
    }

    /// Whether the watcher should watch phones now: a menu is open, or a real
    /// device was attached this run (to see it unplugged and back).
    pub(crate) fn watching_phones(&self) -> bool {
        self.phone_watch_until.is_some_and(|until| Instant::now() < until)
            || self.physical_used
            || self.registry.attached_devices().iter().any(DeviceId::is_physical)
    }

    /// Number a device listing as it starts; `ios`: it asks `simctl`.
    pub(crate) fn begin_listing(&mut self, ios: bool) -> Stamp {
        self.list_seq += 1;
        Stamp { seq: self.list_seq, ios }
    }

    /// Show a finished listing in the device menu, unless one that started
    /// later is already shown. Returns whether it landed.
    fn land_listing(&mut self, stamp: Stamp, devices: Vec<DeviceInfo>, cx: &mut Context<Self>) -> bool {
        if stamp.seq <= self.landed_seq {
            return false;
        }
        self.landed_seq = stamp.seq;
        self.devices = devices;
        self.devices_listed = true;
        self.ios_listed = stamp.ios;
        cx.emit(HubEvent::Devices);
        true
    }

    /// Apply stream settings to `udid`'s live session, in the background
    /// (`configure` waits for the helper's reply).
    pub fn configure_stream(&self, udid: &DeviceId, scale: f64, fps: f64, cx: &mut Context<Self>) {
        let Some(session) = self.session(udid) else { return };
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = session.configure(Some(scale), Some(fps), None, Duration::from_secs(5)) {
                    tracing::debug!("simulator stream configure failed: {e}");
                }
            })
            .detach();
    }

    /// Switch `udid`'s live stream between JPEG and H.264 (iOS helpers that
    /// have H.264; a no-op otherwise).
    pub fn set_stream_format(&self, udid: &DeviceId, format: StreamFormat, cx: &mut Context<Self>) {
        let Some(session) = self.session(udid) else { return };
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = session.set_format(format, Duration::from_secs(5)) {
                    tracing::debug!("simulator stream encoding change failed: {e}");
                }
            })
            .detach();
    }

    /// Paste `text` into `udid`, in the background: onto the device's
    /// clipboard with `simctl pbcopy`, then a HID ⌘V (the spike's verified
    /// route for any Unicode). If `pbcopy` fails, short ASCII is typed out
    /// instead; anything else is reported as a notice. Pastes run one at a
    /// time, in order.
    pub fn paste(&self, udid: &DeviceId, text: String, cx: &mut Context<Self>) {
        let Some(session) = self.session(udid) else { return };
        if text.is_empty() {
            return;
        }
        // Android takes text as text (any Unicode), no clipboard round trip.
        if let Some(android) = session.android().cloned() {
            let lock = self.paste_lock.clone();
            cx.background_executor()
                .spawn(async move {
                    let _turn = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Err(e) = android.type_text(&text) {
                        tracing::debug!("android paste failed: {e}");
                    }
                })
                .detach();
            return;
        }
        if !self.watch_gate().xcode_ok {
            return;
        }
        let (target, lock) = (udid.clone(), self.paste_lock.clone());
        let udid = udid.clone();
        cx.spawn(async move |this, cx| {
            let failed = cx.background_executor().spawn(async move { paste_now(&session, &target, &text, &lock).err() }).await;
            if let Some(e) = failed {
                let _ = this.update(cx, |_, cx| {
                    cx.emit(HubEvent::Notice(udid, NoticeKind::Error, format!("Paste into the simulator failed: {e}")));
                });
            }
        })
        .detach();
    }

    /// The lock that keeps pastes in order (agents paste through it too).
    pub(crate) fn paste_lock(&self) -> capture::PasteLock {
        self.paste_lock.clone()
    }

    /// OxiMux booted `udid` (so it may shut it down without asking).
    pub fn is_owned(&self, udid: &DeviceId) -> bool {
        self.registry.is_owned(udid)
    }

    /// Rotate `udid` a quarter turn (Simulator.app's "Rotate Right" when
    /// `clockwise`, else "Rotate Left"), in the background: the helper replies
    /// once the device turned. False without a live stream.
    pub fn rotate(&self, udid: &DeviceId, clockwise: bool, cx: &mut Context<Self>) -> bool {
        let Some(session) = self.session(udid) else { return false };
        let now = session.orientation();
        let next = if clockwise { now.rotated_right() } else { now.rotated_left() };
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = session.configure(None, None, Some(next), Duration::from_secs(5)) {
                    tracing::debug!("simulator rotate failed: {e}");
                }
            })
            .detach();
        true
    }

    /// Shut `udid` down (the toolbar's power button). The device watcher then
    /// sees it go and the panel shows Disconnected with Reconnect. A real
    /// device is never shut down.
    pub fn shutdown_device(&mut self, udid: &DeviceId, cx: &mut Context<Self>) {
        if udid.is_physical() {
            tracing::warn!(%udid, "refused to shut down a real device");
            return;
        }
        if udid.source() == Source::Adb {
            self.shutdown_android(udid, cx);
            return;
        }
        let Some(sim) = udid.sim_udid() else { return };
        let (runner, udid) = (self.runner.clone(), udid.clone());
        let xcode_ok = self.watch_gate().xcode_ok;
        cx.spawn(async move |this, cx| {
            // Never `xcrun` without a resolvable Xcode (the CLT dialog).
            let result = if xcode_ok {
                cx.background_executor()
                    .spawn(async move { simctl::shutdown(runner.as_ref(), &sim, SIMCTL_TIMEOUT) })
                    .await
                    .map_err(|e| e.to_string())
            } else {
                Err("Xcode is not available".into())
            };
            if let Err(e) = result {
                tracing::warn!(%udid, "simulator shutdown failed: {e}");
                // Still running: say so, and let agents use it again (the
                // latch means "the user stopped it", which did not happen).
                let _ = this.update(cx, |hub, cx| {
                    hub.clear_stopped_by_user(&udid);
                    cx.emit(HubEvent::Notice(udid, NoticeKind::Error, format!("The simulator did not shut down: {e}")));
                });
            }
        })
        .detach();
    }

    /// The panel was opened: from now on the device watcher may poll.
    /// Returns whether this was the first use (which also ran the first
    /// availability check).
    pub fn mark_used(&mut self, cx: &mut Context<Self>) -> bool {
        if self.feature_used {
            return false;
        }
        self.feature_used = true;
        sim_state_keys::mark_feature_used(&self.repo);
        self.refresh_availability(cx);
        true
    }

    /// Whether a device listing has landed yet (so "none booted" means it).
    pub fn devices_listed(&self) -> bool {
        self.devices_listed
    }

    /// Re-check Xcode, the runtime and the helper in the background. A changed
    /// developer dir restarts every helper: they loaded the old Xcode's
    /// private frameworks.
    pub fn refresh_availability(&mut self, cx: &mut Context<Self>) {
        // One check at a time: a slow `xcodebuild` must not stack up polls
        // that then land out of order.
        if self.availability_in_flight {
            return;
        }
        self.availability_in_flight = true;
        let runner = self.runner.clone();
        cx.spawn(async move |this, cx| {
            let fresh = cx
                .background_executor()
                .spawn(async move {
                    availability::check(
                        runner.as_ref(),
                        SIMCTL_TIMEOUT,
                        &availability::default_helper_probe,
                        &oximux_simulator::xcode_app::installed_xcode_apps,
                    )
                })
                .await;
            let _ = this.update(cx, |hub, cx| {
                hub.availability_in_flight = false;
                // Only the *selected* Xcode matters (path and version): an
                // Xcode.app appearing on disk while none is selected must not
                // restart every attached (e.g. Android) stream.
                let old = hub.availability.as_ref().map(|a| a.xcode.selected().cloned());
                let changed = old.is_some_and(|old| old.as_ref() != fresh.xcode.selected());
                let xcode_found = matches!(fresh.xcode, availability::Xcode::Found { .. });
                // Once per change of verdict, not per poll: which check
                // decided, against which developer dir and version.
                let verdict = |a: &availability::Availability| (a.xcode.clone(), a.support.clone(), a.blocking_reason());
                if hub.availability.as_ref().map(verdict) != Some(verdict(&fresh)) {
                    tracing::info!(
                        xcode = ?fresh.xcode,
                        support = ?fresh.support,
                        blocking = ?fresh.blocking_reason(),
                        "simulator availability"
                    );
                }
                hub.availability = Some(fresh);
                // A listing never runs `xcrun` before Xcode is known: list now
                // that it is (again, when an Android-only listing beat this
                // check), or for Android alone when there is no Xcode.
                if android::listing_due(xcode_found, hub.ios_listed, hub.android_sdk.is_some(), hub.devices_listed) {
                    hub.refresh_devices(cx);
                }
                if changed {
                    hub.simctl = None; // inside the old Xcode
                    for udid in hub.registry.attached_devices() {
                        let effects = hub.registry.reconnect(&udid, true);
                        hub.run(effects, cx);
                    }
                }
                cx.emit(HubEvent::Availability);
            });
        })
        .detach();
    }

    /// Attach `worktree` to `device`, or to an automatically picked one.
    /// Lists devices in the background first (to know whether it is booted).
    pub fn attach(&mut self, worktree: &Path, device: Option<DeviceId>, preferred: Option<DeviceId>, cx: &mut Context<Self>) {
        let seq = self.begin_attach(worktree, cx);
        let key = WorktreeKey::from_path(worktree);
        let (runner, xcode_ok, sdk) = (self.runner.clone(), self.watch_gate().xcode_ok, self.android_sdk.clone());
        let stamp = self.begin_listing(xcode_ok);
        cx.spawn(async move |this, cx| {
            let listed = cx
                .background_executor()
                .spawn(async move { android::list_all(runner.as_ref(), xcode_ok, sdk.as_ref(), SIMCTL_TIMEOUT) })
                .await;
            let _ = this.update(cx, |hub, cx| {
                if hub.attach_seq.get(&key) != Some(&seq) {
                    return; // superseded by a newer attach for this worktree
                }
                if let Err(why) = hub.attach_listed(&key, listed, Some(stamp), device.as_ref(), preferred.as_ref(), cx) {
                    tracing::warn!("simulator attach: {why}");
                    cx.emit(HubEvent::AttachFailed(key.path().to_path_buf(), why));
                }
            });
        })
        .detach();
    }

    /// Attach `worktree` to `udid`, which the device watcher just saw boot.
    /// The watcher's own listing already said it is booted, so a device the
    /// menu has listed before attaches now, without a second `simctl list`
    /// (trigger 2's latency); one it has not falls back to [`Self::attach`].
    pub fn attach_booted(&mut self, worktree: &Path, udid: DeviceId, cx: &mut Context<Self>) {
        let Some(at) = self.devices.iter().position(|d| d.udid == udid) else {
            return self.attach(worktree, Some(udid), None, cx);
        };
        self.devices[at].state = DeviceState::Booted;
        let listed = Ok(self.devices.clone());
        self.begin_attach(worktree, cx);
        if let Err(why) = self.attach_listed(&WorktreeKey::from_path(worktree), listed, None, Some(&udid), None, cx) {
            tracing::warn!("simulator attach: {why}");
            cx.emit(HubEvent::AttachFailed(worktree.to_path_buf(), why));
        }
        // Only this device's dot was updated: the others' states (what else
        // booted or shut down since) come from a fresh listing.
        self.refresh_devices(cx);
    }

    /// Start an attach for `worktree`: supersede any older one in flight.
    /// Returns its sequence number.
    pub(crate) fn begin_attach(&mut self, worktree: &Path, cx: &mut Context<Self>) -> u64 {
        self.mark_used(cx);
        self.next_attach += 1;
        self.attach_seq.insert(WorktreeKey::from_path(worktree), self.next_attach);
        self.next_attach
    }

    /// Finish an attach with a device listing taken for it: pick `device` (or
    /// the automatic choice) and attach. Returns the device, or why not. The
    /// listing also refreshes the device menu when `stamp` says it is the
    /// newest (`None`: the menu's own list, already updated).
    pub(crate) fn attach_listed(
        &mut self,
        key: &WorktreeKey,
        listed: Result<Vec<DeviceInfo>, SimError>,
        stamp: Option<Stamp>,
        device: Option<&DeviceId>,
        preferred: Option<&DeviceId>,
        cx: &mut Context<Self>,
    ) -> Result<DeviceInfo, String> {
        let devices = listed.map_err(|e| format!("could not list simulators: {e}"))?;
        match stamp {
            Some(stamp) => {
                self.land_listing(stamp, devices.clone(), cx);
            }
            None => cx.emit(HubEvent::Devices),
        }
        let pick = match device {
            Some(udid) => devices.iter().find(|d| &d.udid == udid).map(|d| (d, d.state == DeviceState::Booted)),
            None => registry::auto_pick(&devices, preferred),
        };
        let Some((info, booted)) = pick else {
            return Err("No usable device. Install an iOS runtime in Xcode › Settings › Components, or create an Android emulator.".into());
        };
        let info = info.clone();
        self.physical_used |= info.udid.is_physical();
        // An attach (the user's pick, `sim attach`) boots it on purpose.
        self.clear_stopped_by_user(&info.udid);
        if self.registry.device_for(key) != Some(&info.udid) {
            // Questions about the device it is leaving are moot.
            self.forget_consent_requests(key.path(), cx);
        }
        let effects = self.registry.attach(key.clone(), info.udid.clone(), booted, Instant::now());
        self.run(effects, cx);
        // Switching device may have left the old one unattached.
        self.stop_unattached_recordings(cx);
        cx.emit(HubEvent::Changed(info.udid.clone()));
        Ok(info)
    }

    /// The worktree's attach number now, noted by an agent's attach before
    /// it lists devices (see [`Self::attach_for_agent`]). Noting changes
    /// nothing: a failed agent attach leaves a pending one of the user's.
    pub(crate) fn attach_generation(&self, worktree: &Path) -> Option<u64> {
        self.attach_seq.get(&WorktreeKey::from_path(worktree)).copied()
    }

    /// An agent's attach: the listing is the agent's own, so it resolves a
    /// device name first. `seen` is [`Self::attach_generation`] from before
    /// that listing: if the user detached or picked another device while it
    /// ran, their choice stands and this attach is refused. Otherwise it
    /// supersedes any attach in flight for `worktree`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn attach_for_agent(
        &mut self,
        worktree: &Path,
        seen: Option<u64>,
        listed: Result<Vec<DeviceInfo>, SimError>,
        stamp: Stamp,
        device: Option<&DeviceId>,
        preferred: Option<&DeviceId>,
        cx: &mut Context<Self>,
    ) -> Result<DeviceInfo, String> {
        let key = WorktreeKey::from_path(worktree);
        if self.attach_seq.get(&key).copied() != seen {
            return Err("the user changed this worktree's device while the attach was listing devices; run `oximux sim status`".into());
        }
        self.begin_attach(worktree, cx);
        self.attach_listed(&key, listed, Some(stamp), device, preferred, cx)
    }

    pub fn detach(&mut self, worktree: &Path, cx: &mut Context<Self>) {
        let key = WorktreeKey::from_path(worktree);
        // A new number, not a removal: a pending attach must not land after
        // this, and an agent's attach listing meanwhile must see it happened.
        self.next_attach += 1;
        self.attach_seq.insert(key.clone(), self.next_attach);
        let udid = self.registry.device_for(&key).cloned();
        self.forget_consent_requests(worktree, cx);
        let effects = self.registry.detach(&key, Instant::now());
        self.run(effects, cx);
        // A recording belongs to the device: it ends with its last attachment.
        self.stop_unattached_recordings(cx);
        if let Some(udid) = udid {
            cx.emit(HubEvent::Changed(udid));
        }
    }

    /// A worktree's panel was shown or hidden (pauses the helper when no
    /// viewer of its device is visible).
    pub fn set_visible(&mut self, worktree: &Path, visible: bool, cx: &mut Context<Self>) {
        let effects = self.registry.set_visible(&WorktreeKey::from_path(worktree), visible, Instant::now());
        self.run(effects, cx);
    }

    pub fn reconnect(&mut self, udid: &DeviceId, cx: &mut Context<Self>) {
        let booted = self.watch.lock().unwrap().is_booted(udid).unwrap_or(true);
        let effects = self.registry.reconnect(udid, booted);
        self.run(effects, cx);
        cx.emit(HubEvent::Changed(udid.clone()));
    }

    /// Carry out effects; results come back through the registry.
    fn run(&mut self, effects: Vec<Effect<StreamSession>>, cx: &mut Context<Self>) {
        for effect in effects {
            match effect {
                Effect::Boot { udid, generation, cancel } => self.boot(udid, generation, cancel, cx),
                Effect::StartSession { udid, generation } => self.start_session(udid, generation, cx),
                Effect::StopSession { session, .. } => session.shutdown(),
                Effect::Pause(session) => drop(session.pause()),
                Effect::Resume(session) => drop(session.resume()),
                // The registry never asks this of a real device; refused here
                // too, whatever asked.
                Effect::ShutdownDevice { udid } if udid.is_physical() => {
                    tracing::warn!(%udid, "refused to shut down a real device");
                }
                Effect::ShutdownDevice { udid } if udid.source() == Source::Adb => self.shutdown_android(&udid, cx),
                Effect::ShutdownDevice { udid } => {
                    // Never `xcrun` without a resolvable Xcode (the CLT dialog).
                    let Some(sim) = udid.sim_udid().filter(|_| self.watch_gate().xcode_ok) else { continue };
                    let runner = self.runner.clone();
                    cx.background_executor()
                        .spawn(async move {
                            if let Err(e) = simctl::shutdown(runner.as_ref(), &sim, SIMCTL_TIMEOUT) {
                                tracing::warn!(%udid, "idle simulator shutdown failed: {e}");
                            }
                        })
                        .detach();
                }
                Effect::Persist => sim_state_keys::save_snapshot(&self.repo, &self.registry.snapshot()),
            }
        }
    }

    fn boot(&mut self, udid: DeviceId, generation: Generation, cancel: Arc<AtomicBool>, cx: &mut Context<Self>) {
        // The registry never boots a real device; refused here too.
        if udid.is_physical() {
            tracing::warn!(%udid, "refused to boot a real device");
            return self.finish_boot(udid, generation, BootResult::Failed("A real device is never booted by OxiMux.".into()), cx);
        }
        if udid.source() == Source::Adb {
            return self.boot_android(udid, generation, cancel, cx);
        }
        let Some(sim) = udid.sim_udid() else {
            return self.finish_boot(udid, generation, BootResult::Failed("not a simulator".into()), cx);
        };
        let runner = self.runner.clone();
        cx.spawn(async move |this, cx| {
            let result = match cx
                .background_executor()
                .spawn(async move { simctl::boot(runner.as_ref(), &sim, SIMCTL_TIMEOUT, &cancel) })
                .await
            {
                Ok(simctl::BootOutcome::Booted) => BootResult::Booted,
                Ok(simctl::BootOutcome::AlreadyBooted) => BootResult::AlreadyBooted,
                Err(SimError::Cancelled) => BootResult::Cancelled,
                Err(e) => BootResult::Failed(e.to_string()),
            };
            let _ = this.update(cx, |hub, cx| {
                let effects = hub.registry.boot_finished(&udid, generation, result);
                hub.run(effects, cx);
                // The device menu's state dots come from the listing: refresh
                // it so the booted device stops showing as shut down.
                hub.refresh_devices(cx);
                cx.emit(HubEvent::Changed(udid));
            });
        })
        .detach();
    }

    fn start_session(&mut self, udid: DeviceId, generation: Generation, cx: &mut Context<Self>) {
        match udid.source() {
            Source::Adb => return self.start_android_session(udid, generation, cx),
            // Never the simulator helper: it would ask CoreSimulator for an
            // id it has never heard of.
            Source::Devicectl => {
                let why = "Streaming a real iPhone is not available in this version of OxiMux.".to_owned();
                return self.finish_start(udid, generation, Err((why, false)), cx);
            }
            Source::Simctl => {}
        }
        let helper = match self.availability.as_ref().map(|a| &a.helper) {
            Some(HelperStatus::Found(path)) => Ok(path.clone()),
            Some(HelperStatus::Missing(why)) => Err(why.clone()),
            // Not checked yet: resolve it the same way, right here.
            None => match availability::default_helper_probe() {
                HelperStatus::Found(path) => Ok(path),
                HelperStatus::Missing(why) => Err(why),
            },
        };
        let stream = cx
            .try_global::<crate::app_settings::simulator_settings::SimulatorSettings>()
            .map(|s| s.stream)
            .unwrap_or_default();
        let opts = HelperOptions {
            scale: f64::from(stream.effective_scale()),
            fps: f64::from(stream.fps),
            format: stream.encoding.format(),
            log_path: Some(simulator_dir().join("logs").join(format!("helper-{udid}.log"))),
            ledger: self.ledger.clone(),
            ..HelperOptions::default()
        };
        let reaped = self.reaped.clone();
        cx.spawn(async move |this, cx| {
            let target = udid.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let path = helper.map_err(|e| (e, false))?;
                    let (done, cvar) = &*reaped;
                    let _unused = cvar.wait_while(done.lock().unwrap(), |done| !*done).unwrap();
                    HelperSession::start(&path, &target, &opts).map(StreamSession::from).map_err(|e| {
                        // Shut down between the watcher's polls (e.g. while
                        // parked): that is a disconnect, not a failure.
                        let gone = matches!(e, SimError::DeviceNotBooted);
                        (e.to_string(), gone)
                    })
                })
                .await;
            let _ = this.update(cx, |hub, cx| hub.finish_start(udid, generation, result, cx));
        })
        .detach();
    }

    /// A session start for `udid` ended: listen to it, or record why not
    /// (`true` with the error: the device was gone — a disconnect, not a
    /// failure).
    fn finish_start(
        &mut self,
        udid: DeviceId,
        generation: Generation,
        result: Result<StreamSession, (String, bool)>,
        cx: &mut Context<Self>,
    ) {
        if let Ok(session) = &result {
            self.listen(udid.clone(), generation, session, cx);
        }
        // Only for the attempt still current: a late answer must not
        // stop a newer session or clear ownership.
        if matches!(result, Err((_, true))) && self.starting(&udid, generation) {
            let effects = self.registry.device_shutdown(&udid);
            self.run(effects, cx);
        }
        let result = result.map_err(|(e, _)| e);
        let effects = self.registry.session_started(&udid, generation, result);
        self.run(effects, cx);
        cx.emit(HubEvent::Changed(udid));
    }

    /// Become the session's sole wake/event consumer and fan out. The wake
    /// closure owns only a channel (never the session), so it cannot keep
    /// the session alive; bursts of frames coalesce into one wake.
    fn listen(&mut self, udid: DeviceId, generation: Generation, session: &StreamSession, cx: &mut Context<Self>) {
        let Some(events) = session.take_events() else { return };
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
        let pending = Arc::new(AtomicBool::new(false));
        let flag = pending.clone();
        let kick = tx.clone();
        session.set_wake(move || {
            if !flag.swap(true, Ordering::AcqRel) {
                let _ = tx.unbounded_send(());
            }
        });
        // Anything that arrived before the wake was installed (a helper that
        // died during the hop to this thread) is drained on this first pass.
        let _ = kick.unbounded_send(());
        drop(kick);
        cx.spawn(async move |this, cx| {
            while rx.next().await.is_some() {
                pending.store(false, Ordering::Release);
                let drained: Vec<SessionEvent> = events.try_iter().collect();
                let alive = this.update(cx, |hub, cx| {
                    // A superseded session's events must not reach the viewers
                    // of the current one (its Exited still reaches the
                    // registry, which ignores it as stale).
                    let current = matches!(hub.registry.phase(&udid),
                        Phase::Live { generation: g } | Phase::Starting { generation: g } if g == generation);
                    for event in drained {
                        if let SessionEvent::Exited { code, fatal } = &event {
                            if let Some(why) = fatal {
                                tracing::info!(%udid, "stream ended: {why}");
                            }
                            let still_booted = hub.watch.lock().unwrap().is_booted(&udid).unwrap_or(true);
                            let reason = fatal.clone().unwrap_or_else(|| format!("The stream helper exited{}.", oximux_simulator::exit_code_suffix(*code)));
                            let effects = hub.registry.session_exited(&udid, generation, still_booted, reason);
                            hub.run(effects, cx);
                            cx.emit(HubEvent::Changed(udid.clone()));
                        }
                        if current {
                            // Visible, or the Encoding menu would say H.264
                            // over a JPEG stream with no hint why.
                            if let SessionEvent::EncodingFallback(why) = &event {
                                let text = format!("{why}. Pick H.264 in the stream row to try again.");
                                cx.emit(HubEvent::Notice(udid.clone(), NoticeKind::Error, text));
                            }
                            // A phone that refuses injected input: once per session.
                            if let SessionEvent::Error(why) = &event
                                && why == oximux_simulator::android::server_log::INPUT_BLOCKED
                            {
                                cx.emit(HubEvent::Notice(udid.clone(), NoticeKind::Error, why.clone()));
                            }
                            cx.emit(HubEvent::Session(udid.clone(), event));
                        }
                    }
                    if current {
                        cx.emit(HubEvent::Frame(udid.clone()));
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    fn watch_gate(&self) -> WatchGate {
        WatchGate {
            xcode_ok: self.availability.as_ref().is_some_and(|a| matches!(a.xcode, availability::Xcode::Found { .. })),
            // The watcher fills this in from Settings, which the hub cannot
            // read without an `App`; the other callers only need `xcode_ok`.
            enabled: true,
            feature_used: self.feature_used,
        }
    }
}

