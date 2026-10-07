//! Startup, quit and the hub's background loops (idle-shutdown tick, gated
//! device watcher). A child of `hub` so it can reach the hub's state.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use gpui::{App, AppContext};
use oximux_simulator::boot_watch::{self, BootWatch, WatchGate};
use oximux_simulator::child_ledger::{self, Ledger};
use oximux_simulator::registry::Registry;
use oximux_simulator::runner::SystemRunner;
use oximux_simulator::{DeviceId, Platform, Source};
use oximux_storage::{SettingsRepo, SimApprovalRepo};

use super::{HubEvent, SimulatorHub, SimulatorService, TICK, hub};
use crate::app_settings::sim_state_keys;

pub(crate) fn simulator_dir() -> PathBuf {
    crate::app_paths::data_dir().unwrap_or_else(std::env::temp_dir).join("simulator")
}

/// Create the hub, reap a previous run's orphans in the background, and start
/// the idle-shutdown tick and the (gated) device watcher. Call once at startup.
pub fn install(cx: &mut App, repo: SettingsRepo, approvals: SimApprovalRepo) {
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
    let feature_used = sim_state_keys::feature_used(&repo);
    let hub = new_hub(cx, repo, approvals, ledger, reaped);
    cx.set_global(SimulatorService(hub.clone()));
    if feature_used {
        hub.update(cx, |hub, cx| hub.refresh_availability(cx));
    }
    // A filesystem lookup only: never runs adb.
    hub.update(cx, |hub, cx| hub.refresh_android_sdk(cx));
    spawn_tick(cx, hub.downgrade());
    spawn_watch(cx, hub.downgrade());
}

/// A hub for tests: no child ledger (it lives in the real app data dir), no
/// background loops, the startup reap already done.
#[cfg(test)]
pub(crate) fn install_for_test(cx: &mut App, repo: SettingsRepo, approvals: SimApprovalRepo) -> gpui::Entity<SimulatorHub> {
    let hub = new_hub(cx, repo, approvals, None, Arc::new((Mutex::new(true), Condvar::new())));
    cx.set_global(SimulatorService(hub.clone()));
    hub
}

fn new_hub(
    cx: &mut App,
    repo: SettingsRepo,
    approvals: SimApprovalRepo,
    ledger: Option<Arc<Ledger>>,
    reaped: Arc<(Mutex<bool>, Condvar)>,
) -> gpui::Entity<SimulatorHub> {
    let snapshot = sim_state_keys::load_snapshot(&repo);
    let feature_used = sim_state_keys::feature_used(&repo);
    sim_state_keys::revoke_physical_approvals_once(&repo, &approvals);
    // Which Wi-Fi transports may join a phone's row (see `wifi`).
    *oximux_simulator::android::devices::paired_phones().lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
        sim_state_keys::load_wifi_paired(&repo);
    let mut agent = super::agent::AgentState::load(Some(approvals));
    agent.stopped = sim_state_keys::load_stopped(&repo);
    cx.new(|_| SimulatorHub {
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
        devices: Vec::new(),
        devices_listed: false,
        ios_listed: false,
        list_seq: 0,
        landed_seq: 0,
        availability_in_flight: false,
        recordings: HashMap::new(),
        recording_starts: Default::default(),
        simctl: None,
        paste_lock: Default::default(),
        agent,
        boot_claims: HashSet::new(),
        android_sdk: None,
        phone_watch_until: None,
        screen_off: HashSet::new(),
        android_phones_only: false,
        android_sdk_lost: false,
        wifi: Default::default(),
        physical_used: false,
        phone_states: None,
        iphone_states: None,
        capture_starts: Default::default(),
    })
}

struct OpenOnDrop(Arc<(Mutex<bool>, Condvar)>);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        let (done, cvar) = &*self.0;
        *done.lock().unwrap_or_else(|p| p.into_inner()) = true;
        cvar.notify_all();
    }
}

/// App quit (bounded by GPUI's shutdown grace): finalize screen recordings
/// (the only wait: at most `record::FINALIZE_GRACE`, all in parallel, and
/// only while one is running), close every helper's stdin, and hand the
/// owned devices to a detached `simctl shutdown` that runs after we are gone.
pub fn on_quit(cx: &mut App) {
    let Some(hub) = hub(cx) else { return };
    hub.update(cx, |hub, _| {
        // Movies first: a recording killed by the device shutdown below would
        // be unplayable.
        hub.stop_recordings_blocking();
        let (sessions, owned) = hub.registry.quit();
        // Owned emulators by serial: the live sessions know theirs, a parked
        // one was seen by the watcher. No adb call on the quit path.
        let serials: Vec<String> = owned
            .iter()
            .filter_map(|id| match oximux_simulator::android::Target::from_id(id)? {
                oximux_simulator::android::Target::Avd(name) => sessions
                    .iter()
                    .filter_map(|s| s.android())
                    .find(|a| a.id() == id)
                    .map(|a| a.serial().to_owned())
                    .or_else(|| oximux_simulator::android::devices::known_serial(&name)),
                oximux_simulator::android::Target::Serial(_) => None,
            })
            .collect();
        for session in sessions {
            session.shutdown();
        }
        // `quit` already released ownership of what the script shuts down.
        sim_state_keys::save_snapshot(&hub.repo, &hub.registry.snapshot());
        spawn_detached_shutdown(&owned);
        if let Some(sdk) = &hub.android_sdk {
            spawn_detached_emulator_shutdown(&sdk.adb(), &serials);
        }
    });
}

/// `adb -s <serial> emu kill` for each emulator OxiMux booted, in a process
/// of its own that outlives the app — the graceful console stop, which keeps
/// the quick-boot snapshot. Serials are positional arguments, and only
/// well-formed ones.
fn spawn_detached_emulator_shutdown(adb: &std::path::Path, serials: &[String]) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let safe = |s: &&String| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '-' | '_'));
        let serials: Vec<&String> = serials.iter().filter(safe).collect();
        if serials.is_empty() {
            return;
        }
        let spawned = std::process::Command::new("/bin/sh")
            .args(["-c", r#"adb=$1; shift; sleep 1; for s in "$@"; do "$adb" -s "$s" emu kill; done"#, "sh"])
            .arg(adb)
            .args(serials)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn();
        if let Err(e) = spawned {
            tracing::warn!("could not schedule shutdown of owned emulators: {e}");
        }
    }
    #[cfg(not(unix))]
    let _ = (adb, serials);
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
pub(crate) fn is_udid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && groups.iter().zip([8, 4, 4, 4, 12]).all(|(g, n)| g.len() == n && g.chars().all(|c| c.is_ascii_hexdigit()))
}

fn spawn_tick(cx: &mut App, hub: gpui::WeakEntity<SimulatorHub>) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(TICK).await;
            let alive = hub.update(cx, |hub, cx| {
                let minutes = crate::shell::simulator::panel::settings(cx).idle_shutdown_minutes;
                hub.registry.set_idle_shutdown((minutes > 0).then(|| Duration::from_secs(u64::from(minutes) * 60)));
                let effects = hub.registry.tick(Instant::now());
                hub.run(effects, cx);
                hub.reap_recordings(cx);
                hub.expire_consent(cx);
                hub.recheck_android_sdk(cx);
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
            let gate = hub.update(cx, |hub, cx| {
                let xcode_ok = hub.watch_gate().xcode_ok;
                // Android devices are watched with no Xcode at all.
                let gate = WatchGate {
                    enabled: crate::shell::simulator::panel::settings(cx).enabled,
                    xcode_ok: xcode_ok || hub.android_sdk.is_some(),
                    ..hub.watch_gate()
                };
                if !gate.should_poll() {
                    // Start from a fresh baseline when polling resumes: devices
                    // booted meanwhile are not news, and must not all fire at once.
                    hub.watch.lock().unwrap().forget();
                    return None;
                }
                // Android is polled only while an Android device is attached,
                // or is one OxiMux booted and must still shut down when idle:
                // an iOS-only user with Android Studio installed gets no adb.
                let android_in_use = hub
                    .registry
                    .attached_devices()
                    .into_iter()
                    .chain(hub.registry.owned_devices())
                    .any(|d| d.platform() == Platform::Android);
                // Phones are watched (every adb state) only while they matter:
                // a device menu is open, or a real device was attached.
                let phones = hub.android_sdk.clone().filter(|_| hub.watching_phones());
                // So are real iPhones (`devicectl`, ~30 ms), with Xcode.
                let iphones = xcode_ok && hub.watching_phones();
                // A streaming phone's screen: a sleeping one sends nothing.
                let screens = hub.android_sdk.clone().map(|sdk| (sdk, hub.screens_to_watch()));
                let sdk = hub.android_sdk.clone().filter(|_| android_in_use);
                Some((hub.runner.clone(), hub.registry.generation(), xcode_ok, sdk, (phones, screens, iphones)))
            });
            let (runner, listed_at, xcode_ok, sdk, (phones, screens, iphones)) = match gate {
                Ok(Some(gate)) => gate,
                Ok(None) => continue,
                Err(_) => return, // the hub is gone
            };
            // What this round lists: only those sources' devices can be read
            // as shut down. Real iPhones join below, only if `devicectl`
            // answered: a round it missed says nothing about them.
            let mut sources: Vec<Source> = [xcode_ok.then_some(Source::Simctl), sdk.is_some().then_some(Source::Adb)].into_iter().flatten().collect();
            // The `simctl list` runs with no lock held: the UI thread reads
            // the watch state (reconnect, helper exit) and must never wait on
            // CoreSimulator.
            let (listed, phones, screens, iphones) = cx
                .background_executor()
                .spawn(async move {
                    let phones = phones.and_then(|sdk| super::android::phone_states(runner.as_ref(), &sdk));
                    let screens = screens.map(|(sdk, watched)| super::android::screens_awake(runner.as_ref(), &sdk, watched));
                    // Beside the rest: a slow CoreDevice must not hold up
                    // simulator and unplug detection.
                    std::thread::scope(|scope| {
                        let iphones = scope.spawn(|| iphones.then(|| super::iphone::list(runner.as_ref())).flatten());
                        let booted = super::android::booted_all(runner.as_ref(), xcode_ok, sdk.as_ref());
                        (booted, phones, screens, iphones.join().unwrap_or(None))
                    })
                })
                .await;
            let alive = hub.update(cx, |hub, cx| {
                if let Some(phones) = phones {
                    hub.observe_phones(phones, cx);
                }
                hub.observe_screens(screens.unwrap_or_default(), cx);
                if let Some(iphones) = &iphones {
                    hub.observe_iphones(super::iphone::states(iphones), cx);
                }
            });
            if alive.is_err() {
                return;
            }
            let mut booted = match listed {
                Ok(booted) => booted,
                Err(e) => {
                    tracing::debug!("simulator device watch: {e}");
                    continue;
                }
            };
            // A cabled, trusting iPhone is "booted"; one only on Wi-Fi is not
            // (devicectl keeps listing a paired phone after it is unplugged).
            if let Some(iphones) = &iphones {
                sources.push(Source::Devicectl);
                booted.extend(oximux_simulator::devicectl::connected(iphones).cloned());
            }
            let alive = hub.update(cx, |hub, cx| {
                let (events, newly_listed) = {
                    let mut watch = hub.watch.lock().unwrap();
                    if watch.listed() != sources.as_slice() {
                        tracing::info!(?sources, booted = booted.len(), "simulator watcher: listing changed");
                    }
                    // Sources this round starts from a baseline for: every
                    // one on the first poll, else those the last did not list.
                    let newly_listed: Vec<Source> = sources
                        .iter()
                        .copied()
                        .filter(|p| !watch.has_baseline() || !watch.listed().contains(p))
                        .collect();
                    (watch.observe_listed(booted.clone(), &sources), newly_listed)
                };
                // A baseline reports no boots, but a latched device already up
                // (booted while OxiMux was closed, or while its source was not
                // polled) is not "stopped by the user" any more.
                for udid in booted.iter().filter(|u| newly_listed.contains(&u.source())) {
                    hub.clear_stopped_by_user(udid);
                }
                // A device OxiMux attached is already somebody's; one booted
                // elsewhere may be an agent's, for windows to pick up.
                let attached = hub.registry.attached_devices();
                let mut fresh = Vec::new();
                for event in events {
                    match event {
                        boot_watch::WatchEvent::Booted(udid) => {
                            // Up again, by whoever: the power button's "stop"
                            // is over.
                            hub.clear_stopped_by_user(&udid);
                            // A real device plugged in is never taken over:
                            // it attaches only when asked for.
                            if !attached.contains(&udid) && !udid.is_physical() {
                                fresh.push(udid);
                            }
                        }
                        boot_watch::WatchEvent::Shutdown(udid) => {
                            hub.boot_claims.remove(&udid);
                        }
                    }
                }
                if !fresh.is_empty() {
                    tracing::info!(devices = fresh.len(), "simulator watcher: devices booted elsewhere");
                    cx.emit(HubEvent::DeviceBooted(fresh));
                }
                // A boot or start finished while we were listing: the set may
                // predate it and read a fresh boot as a shutdown. Skip; the
                // next poll is three seconds away.
                if hub.registry.generation() != listed_at {
                    return;
                }
                let changed = hub.registry.attached_devices();
                let effects = hub.registry.reconcile_booted(&booted, &sources);
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
