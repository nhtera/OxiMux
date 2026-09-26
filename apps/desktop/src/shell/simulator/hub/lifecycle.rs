//! Startup, quit and the hub's background loops (idle-shutdown tick, gated
//! device watcher). A child of `hub` so it can reach the hub's state.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use gpui::{App, AppContext};
use oximux_simulator::boot_watch::{self, BootWatch};
use oximux_simulator::child_ledger::{self, Ledger};
use oximux_simulator::registry::Registry;
use oximux_simulator::runner::SystemRunner;
use oximux_simulator::DeviceId;
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
        devices: Vec::new(),
        devices_listed: false,
        availability_in_flight: false,
        recordings: HashMap::new(),
        recording_starts: Default::default(),
        simctl: None,
        paste_lock: Default::default(),
        agent: super::agent::AgentState::load(Some(approvals)),
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
                let effects = hub.registry.tick(Instant::now());
                hub.run(effects, cx);
                hub.reap_recordings(cx);
                hub.expire_consent(cx);
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
