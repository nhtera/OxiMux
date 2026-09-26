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
use oximux_simulator::registry::{self, BootResult, Effect, Generation, Phase, Registry, WorktreeKey};
use oximux_simulator::runner::SystemRunner;
use oximux_simulator::session::{SessionEvent, StreamSession};
use oximux_simulator::{DeviceId, DeviceInfo, DeviceState, SimError, simctl};
use oximux_storage::SettingsRepo;

use crate::app_settings::sim_state_keys;

/// `simctl` call timeouts (boot has its own bounded poll inside).
const SIMCTL_TIMEOUT: Duration = Duration::from_secs(30);
/// How often devices are checked for parking and the idle shutdown (so a
/// hidden device parks 60–75 s after it was hidden).
const TICK: Duration = Duration::from_secs(15);

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
    /// Booted devices a window already took for auto-attach (so one boot is
    /// attached once, not by every window); dropped when the device shuts down.
    boot_claims: HashSet<DeviceId>,
}

impl EventEmitter<HubEvent> for SimulatorHub {}

mod agent;
mod capture;
mod lifecycle;

pub use agent::Wake;
pub use capture::NoticeKind;
pub(crate) use capture::{CaptureKind, capture_dir, capture_path, home_button, paste_now, stamp};

pub use lifecycle::{install, on_quit};
pub(crate) use lifecycle::{is_udid, simulator_dir};
#[cfg(test)]
pub(crate) use lifecycle::install_for_test;

/// The global handle to the one hub.
pub struct SimulatorService(pub Entity<SimulatorHub>);

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
        self.registry.attached_devices().iter().find_map(|udid| self.session(udid)).map(|s| s.hello().version.clone())
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

    /// Re-list devices in the background (never on the UI thread; never
    /// without a resolvable Xcode, which would pop the tools dialog).
    pub fn refresh_devices(&mut self, cx: &mut Context<Self>) {
        if !self.watch_gate().xcode_ok {
            return;
        }
        let runner = self.runner.clone();
        cx.spawn(async move |this, cx| {
            let listed = cx.background_executor().spawn(async move { simctl::list_devices(runner.as_ref(), SIMCTL_TIMEOUT) }).await;
            let _ = this.update(cx, |hub, cx| match listed {
                Ok(devices) => {
                    hub.devices = devices;
                    hub.devices_listed = true;
                    cx.emit(HubEvent::Devices);
                }
                Err(e) => tracing::debug!("simulator device listing failed: {e}"),
            });
        })
        .detach();
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

    /// Paste `text` into `udid`, in the background: onto the device's
    /// clipboard with `simctl pbcopy`, then a HID ⌘V (the spike's verified
    /// route for any Unicode). If `pbcopy` fails, short ASCII is typed out
    /// instead; anything else is reported as a notice. Pastes run one at a
    /// time, in order.
    pub fn paste(&self, udid: &DeviceId, text: String, cx: &mut Context<Self>) {
        let Some(session) = self.session(udid) else { return };
        if text.is_empty() || !self.watch_gate().xcode_ok {
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
    /// sees it go and the panel shows Disconnected with Reconnect.
    pub fn shutdown_device(&self, udid: &DeviceId, cx: &mut Context<Self>) {
        if !self.watch_gate().xcode_ok {
            return;
        }
        let (runner, udid) = (self.runner.clone(), udid.clone());
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = simctl::shutdown(runner.as_ref(), udid.as_str(), SIMCTL_TIMEOUT) {
                    tracing::warn!(%udid, "simulator shutdown failed: {e}");
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
                .spawn(async move { availability::check(runner.as_ref(), SIMCTL_TIMEOUT, &availability::default_helper_probe) })
                .await;
            let _ = this.update(cx, |hub, cx| {
                hub.availability_in_flight = false;
                let old = hub.availability.as_ref().map(|a| a.xcode.clone());
                let changed = old.is_some_and(|old| old != fresh.xcode);
                let xcode_found = matches!(fresh.xcode, availability::Xcode::Found { .. });
                hub.availability = Some(fresh);
                // The device menu's first listing waits on this check (no
                // `xcrun` before Xcode is known); run it now that it is.
                if xcode_found && !hub.devices_listed {
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
        let runner = self.runner.clone();
        cx.spawn(async move |this, cx| {
            let listed = cx.background_executor().spawn(async move { simctl::list_devices(runner.as_ref(), SIMCTL_TIMEOUT) }).await;
            let _ = this.update(cx, |hub, cx| {
                if hub.attach_seq.get(&key) != Some(&seq) {
                    return; // superseded by a newer attach for this worktree
                }
                if let Err(why) = hub.attach_listed(&key, listed, device.as_ref(), preferred.as_ref(), cx) {
                    tracing::warn!("simulator attach: {why}");
                    cx.emit(HubEvent::AttachFailed(key.path().to_path_buf(), why));
                }
            });
        })
        .detach();
    }

    /// Start an attach for `worktree`: supersede any older one in flight.
    /// Returns its sequence number.
    fn begin_attach(&mut self, worktree: &Path, cx: &mut Context<Self>) -> u64 {
        self.mark_used(cx);
        self.next_attach += 1;
        self.attach_seq.insert(WorktreeKey::from_path(worktree), self.next_attach);
        self.next_attach
    }

    /// Finish an attach with a device listing taken for it: pick `device` (or
    /// the automatic choice) and attach. Returns the device, or why not.
    pub(crate) fn attach_listed(
        &mut self,
        key: &WorktreeKey,
        listed: Result<Vec<DeviceInfo>, SimError>,
        device: Option<&DeviceId>,
        preferred: Option<&DeviceId>,
        cx: &mut Context<Self>,
    ) -> Result<DeviceInfo, String> {
        let devices = listed.map_err(|e| format!("could not list simulators: {e}"))?;
        self.devices = devices;
        self.devices_listed = true;
        cx.emit(HubEvent::Devices);
        let pick = match device {
            Some(udid) => self.devices.iter().find(|d| &d.udid == udid).map(|d| (d, d.state == DeviceState::Booted)),
            None => registry::auto_pick(&self.devices, preferred),
        };
        let Some((info, booted)) = pick else {
            return Err("No usable iOS simulator. Install an iOS runtime in Xcode › Settings › Components.".into());
        };
        let info = info.clone();
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

    /// An agent's attach: the listing is the agent's own, so it resolves a
    /// device name first. Supersedes any attach in flight for `worktree`.
    pub(crate) fn attach_for_agent(
        &mut self,
        worktree: &Path,
        listed: Result<Vec<DeviceInfo>, SimError>,
        device: Option<&DeviceId>,
        preferred: Option<&DeviceId>,
        cx: &mut Context<Self>,
    ) -> Result<DeviceInfo, String> {
        self.begin_attach(worktree, cx);
        self.attach_listed(&WorktreeKey::from_path(worktree), listed, device, preferred, cx)
    }

    pub fn detach(&mut self, worktree: &Path, cx: &mut Context<Self>) {
        let key = WorktreeKey::from_path(worktree);
        self.attach_seq.remove(&key); // a pending attach must not land after this
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
                Effect::ShutdownDevice { udid } => {
                    // Never `xcrun` without a resolvable Xcode (the CLT dialog).
                    if !self.watch_gate().xcode_ok {
                        continue;
                    }
                    let runner = self.runner.clone();
                    cx.background_executor()
                        .spawn(async move {
                            if let Err(e) = simctl::shutdown(runner.as_ref(), udid.as_str(), SIMCTL_TIMEOUT) {
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
        let runner = self.runner.clone();
        cx.spawn(async move |this, cx| {
            let target = udid.clone();
            let result = match cx
                .background_executor()
                .spawn(async move { simctl::boot(runner.as_ref(), target.as_str(), SIMCTL_TIMEOUT, &cancel) })
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
                    StreamSession::start(&path, &target, &opts).map_err(|e| {
                        // Shut down between the watcher's polls (e.g. while
                        // parked): that is a disconnect, not a failure.
                        let gone = matches!(e, SimError::DeviceNotBooted);
                        (e.to_string(), gone)
                    })
                })
                .await;
            let _ = this.update(cx, |hub, cx| {
                if let Ok(session) = &result {
                    hub.listen(udid.clone(), generation, session, cx);
                }
                // Only for the attempt still current: a late answer must not
                // stop a newer session or clear ownership.
                if matches!(result, Err((_, true))) && hub.registry.phase(&udid) == (Phase::Starting { generation }) {
                    let effects = hub.registry.device_shutdown(&udid);
                    hub.run(effects, cx);
                }
                let result = result.map_err(|(e, _)| e);
                let effects = hub.registry.session_started(&udid, generation, result);
                hub.run(effects, cx);
                cx.emit(HubEvent::Changed(udid));
            });
        })
        .detach();
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
                            let still_booted = hub.watch.lock().unwrap().is_booted(&udid).unwrap_or(true);
                            let reason = fatal.clone().unwrap_or_else(|| format!("The simulator helper exited (code {code:?})."));
                            let effects = hub.registry.session_exited(&udid, generation, still_booted, reason);
                            hub.run(effects, cx);
                            cx.emit(HubEvent::Changed(udid.clone()));
                        }
                        if current {
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

