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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::{App, AppContext, Context, Entity, EventEmitter, Global};
use oximux_simulator::availability::{self, Availability, HelperStatus};
use oximux_simulator::boot_watch::{self, BootWatch, WatchGate};
use oximux_simulator::child_ledger::{self, Ledger};
use oximux_simulator::helper::HelperOptions;
use oximux_simulator::registry::{self, BootResult, Effect, Generation, Phase, Registry, WorktreeKey};
use oximux_simulator::runner::SystemRunner;
use oximux_simulator::session::{SessionEvent, StreamSession};
use oximux_simulator::{DeviceId, DeviceState, SimError, simctl};
use oximux_storage::SettingsRepo;

use crate::app_settings::sim_state_keys;

/// `simctl` call timeouts (boot has its own bounded poll inside).
const SIMCTL_TIMEOUT: Duration = Duration::from_secs(30);
/// How often owned devices are checked for the idle shutdown.
const TICK: Duration = Duration::from_secs(30);

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
    /// An attach for this worktree found no device to use (the panel leaves
    /// its "Attaching…" state and shows why).
    AttachFailed(PathBuf, String),
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
}

impl EventEmitter<HubEvent> for SimulatorHub {}

/// The global handle to the one hub.
pub struct SimulatorService(pub Entity<SimulatorHub>);

impl Global for SimulatorService {}

/// The hub, when installed (it is not in `oximux serve`, which has no UI).
pub fn hub(cx: &App) -> Option<Entity<SimulatorHub>> {
    cx.try_global::<SimulatorService>().map(|s| s.0.clone())
}

fn simulator_dir() -> PathBuf {
    crate::app_paths::data_dir().unwrap_or_else(std::env::temp_dir).join("simulator")
}

/// Create the hub, reap a previous run's orphans in the background, and start
/// the idle-shutdown tick and the (gated) device watcher. Call once at startup.
pub fn install(cx: &mut App, repo: SettingsRepo) {
    let ledger = Ledger::open(simulator_dir().join("children.json"))
        .map(Arc::new)
        .inspect_err(|e| tracing::warn!("simulator child ledger unavailable: {e}"))
        .ok();
    let reaped = Arc::new((Mutex::new(false), Condvar::new()));
    {
        let (ledger, reaped) = (ledger.clone(), reaped.clone());
        cx.background_executor()
            .spawn(async move {
                // Open the gate however this ends (a panic included), or every
                // session start would wait forever.
                let _open = OpenOnDrop(reaped);
                if let Some(ledger) = ledger {
                    let report = child_ledger::reap_stale(&ledger);
                    if !report.killed.is_empty() {
                        tracing::info!(killed = ?report.killed, "reaped orphaned simulator children");
                    }
                }
            })
            .detach();
    }
    let snapshot = sim_state_keys::load_snapshot(&repo);
    let feature_used = sim_state_keys::feature_used(&repo);
    let hub = cx.new(|_| SimulatorHub {
        registry: Registry::restore(snapshot, Instant::now()),
        repo,
        runner: Arc::new(SystemRunner),
        ledger,
        reaped,
        watch: Arc::new(Mutex::new(BootWatch::default())),
        availability: None,
        feature_used,
        attach_seq: HashMap::new(),
        next_attach: 0,
    });
    cx.set_global(SimulatorService(hub.clone()));
    if feature_used {
        hub.update(cx, |hub, cx| hub.refresh_availability(cx));
    }
    spawn_tick(cx, hub.downgrade());
    spawn_watch(cx, hub.downgrade());
}

struct OpenOnDrop(Arc<(Mutex<bool>, Condvar)>);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        let (done, cvar) = &*self.0;
        *done.lock().unwrap_or_else(|p| p.into_inner()) = true;
        cvar.notify_all();
    }
}

/// App quit (bounded by GPUI's shutdown grace): close every helper's stdin
/// and hand the owned devices to a detached `simctl shutdown` that runs after
/// we are gone. Never waits.
pub fn on_quit(cx: &mut App) {
    let Some(hub) = hub(cx) else { return };
    hub.update(cx, |hub, _| {
        let (sessions, owned) = hub.registry.quit();
        for session in sessions {
            session.shutdown();
        }
        // `quit` already released ownership of what the script shuts down.
        sim_state_keys::save_snapshot(&hub.repo, &hub.registry.snapshot());
        spawn_detached_shutdown(&owned);
    });
}

/// `sleep 1; xcrun simctl shutdown …` in its own process group, so it
/// outlives the app and never holds quit up. A relaunch within that second
/// sees the device "Shutting Down", which the registry treats as not booted.
fn spawn_detached_shutdown(owned: &[DeviceId]) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        if owned.is_empty() {
            return;
        }
        // Positional arguments, never interpolation: these ids come back from
        // the settings DB, and only well-formed UUIDs are passed at all.
        let udids: Vec<&str> = owned.iter().map(DeviceId::as_str).filter(|u| is_udid(u)).collect();
        if udids.is_empty() {
            return;
        }
        let spawned = std::process::Command::new("/bin/sh")
            .args(["-c", r#"sleep 1; for u in "$@"; do xcrun simctl shutdown "$u"; done"#, "sh"])
            .args(&udids)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn();
        if let Err(e) = spawned {
            tracing::warn!("could not schedule shutdown of owned simulators: {e}");
        }
    }
    #[cfg(not(unix))]
    let _ = owned;
}

/// A simulator UDID: 8-4-4-4-12 hex digits.
fn is_udid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && groups.iter().zip([8, 4, 4, 4, 12]).all(|(g, n)| g.len() == n && g.chars().all(|c| c.is_ascii_hexdigit()))
}

impl SimulatorHub {
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

    /// The panel was opened: from now on the device watcher may poll.
    pub fn mark_used(&mut self, cx: &mut Context<Self>) {
        if !self.feature_used {
            self.feature_used = true;
            sim_state_keys::mark_feature_used(&self.repo);
            self.refresh_availability(cx);
        }
    }

    /// Re-check Xcode, the runtime and the helper in the background. A changed
    /// developer dir restarts every helper: they loaded the old Xcode's
    /// private frameworks.
    pub fn refresh_availability(&mut self, cx: &mut Context<Self>) {
        let runner = self.runner.clone();
        cx.spawn(async move |this, cx| {
            let fresh = cx
                .background_executor()
                .spawn(async move { availability::check(runner.as_ref(), SIMCTL_TIMEOUT, &availability::default_helper_probe) })
                .await;
            let _ = this.update(cx, |hub, cx| {
                let old = hub.availability.as_ref().map(|a| a.xcode.clone());
                let changed = old.is_some_and(|old| old != fresh.xcode);
                hub.availability = Some(fresh);
                if changed {
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
        self.mark_used(cx);
        let key = WorktreeKey::from_path(worktree);
        self.next_attach += 1;
        let seq = self.next_attach;
        self.attach_seq.insert(key.clone(), seq);
        let runner = self.runner.clone();
        cx.spawn(async move |this, cx| {
            let listed = cx.background_executor().spawn(async move { simctl::list_devices(runner.as_ref(), SIMCTL_TIMEOUT) }).await;
            let _ = this.update(cx, |hub, cx| {
                if hub.attach_seq.get(&key) != Some(&seq) {
                    return; // superseded by a newer attach for this worktree
                }
                let fail = |cx: &mut Context<SimulatorHub>, why: String| {
                    tracing::warn!("simulator attach: {why}");
                    cx.emit(HubEvent::AttachFailed(key.path().to_path_buf(), why));
                };
                let devices = match listed {
                    Ok(devices) => devices,
                    Err(e) => return fail(cx, format!("could not list simulators: {e}")),
                };
                let pick = match &device {
                    Some(udid) => devices.iter().find(|d| &d.udid == udid).map(|d| (d, d.state == DeviceState::Booted)),
                    None => registry::auto_pick(&devices, preferred.as_ref()),
                };
                let Some((info, booted)) = pick else {
                    return fail(cx, "No usable iOS simulator. Install an iOS runtime in Xcode › Settings › Components.".into());
                };
                let udid = info.udid.clone();
                let effects = hub.registry.attach(key, udid.clone(), booted, Instant::now());
                hub.run(effects, cx);
                cx.emit(HubEvent::Changed(udid));
            });
        })
        .detach();
    }

    pub fn detach(&mut self, worktree: &Path, cx: &mut Context<Self>) {
        let key = WorktreeKey::from_path(worktree);
        self.attach_seq.remove(&key); // a pending attach must not land after this
        let udid = self.registry.device_for(&key).cloned();
        let effects = self.registry.detach(&key, Instant::now());
        self.run(effects, cx);
        if let Some(udid) = udid {
            cx.emit(HubEvent::Changed(udid));
        }
    }

    /// A worktree's panel was shown or hidden (pauses the helper when no
    /// viewer of its device is visible).
    pub fn set_visible(&mut self, worktree: &Path, visible: bool, cx: &mut Context<Self>) {
        let effects = self.registry.set_visible(&WorktreeKey::from_path(worktree), visible);
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
        let opts = HelperOptions {
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
                    let path = helper?;
                    let (done, cvar) = &*reaped;
                    let _unused = cvar.wait_while(done.lock().unwrap(), |done| !*done).unwrap();
                    StreamSession::start(&path, &target, &opts).map_err(|e| e.to_string())
                })
                .await;
            let _ = this.update(cx, |hub, cx| {
                if let Ok(session) = &result {
                    hub.listen(udid.clone(), generation, session, cx);
                }
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
            // Settings (P9) will own this; the feature is on by default.
            enabled: true,
            feature_used: self.feature_used,
        }
    }
}

fn spawn_tick(cx: &mut App, hub: gpui::WeakEntity<SimulatorHub>) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(TICK).await;
            let alive = hub.update(cx, |hub, cx| {
                let effects = hub.registry.tick(Instant::now());
                hub.run(effects, cx);
            });
            if alive.is_err() {
                return;
            }
        }
    })
    .detach();
}

fn spawn_watch(cx: &mut App, hub: gpui::WeakEntity<SimulatorHub>) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(boot_watch::POLL_INTERVAL).await;
            let gate = hub.update(cx, |hub, _| {
                hub.watch_gate().should_poll().then(|| (hub.runner.clone(), hub.registry.generation()))
            });
            let (runner, listed_at) = match gate {
                Ok(Some(gate)) => gate,
                Ok(None) => continue,
                Err(_) => return, // the hub is gone
            };
            // The `simctl list` runs with no lock held: the UI thread reads
            // the watch state (reconnect, helper exit) and must never wait on
            // CoreSimulator.
            let listed = cx.background_executor().spawn(async move { boot_watch::list_booted(runner.as_ref()) }).await;
            let booted = match listed {
                Ok(booted) => booted,
                Err(e) => {
                    tracing::debug!("simulator device watch: {e}");
                    continue;
                }
            };
            let alive = hub.update(cx, |hub, cx| {
                hub.watch.lock().unwrap().observe(booted.clone());
                // A boot or start finished while we were listing: the set may
                // predate it and read a fresh boot as a shutdown. Skip; the
                // next poll is three seconds away.
                if hub.registry.generation() != listed_at {
                    return;
                }
                let changed = hub.registry.attached_devices();
                let effects = hub.registry.reconcile_booted(&booted);
                if !effects.is_empty() {
                    hub.run(effects, cx);
                    for udid in changed {
                        cx.emit(HubEvent::Changed(udid));
                    }
                }
            });
            if alive.is_err() {
                return;
            }
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::is_udid;

    #[test]
    fn only_well_formed_udids_reach_the_shutdown_script() {
        assert!(is_udid("81CE1BE8-E38A-4BA8-8AAB-5DACA07576B3"));
        for bad in ["", "U", "81CE1BE8-E38A-4BA8-8AAB-5DACA07576B", "x; rm -rf ~", "81CE1BE8-E38A-4BA8-8AAB-5DACA07576B3 extra", "ZZZZZZZZ-E38A-4BA8-8AAB-5DACA07576B3"] {
            assert!(!is_udid(bad), "{bad}");
        }
    }
}
