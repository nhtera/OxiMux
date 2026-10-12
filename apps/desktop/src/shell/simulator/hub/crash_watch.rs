//! The panel's crash watch: while a panel shows a streaming device, its new
//! crashes are looked for every [`POLL`] — a simulator's new `.ips` reports,
//! an Android device's crash buffer — and each is announced once
//! ([`HubEvent::Crash`]).
//!
//! The first look at a device is a baseline: a crash from before the panel
//! showed it never raises a row. A device no panel shows any more is
//! forgotten, so showing it again starts from a fresh baseline. Read-only:
//! it never touches the device.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{App, WeakEntity};
use oximux_simulator::android::adb::Adb;
use oximux_simulator::diag::Crash;
use oximux_simulator::diag::crashes::{BufferScan, ReportScan};
use oximux_simulator::runner::SystemRunner;
use oximux_simulator::{DeviceId, Source};

use super::{HubEvent, SimulatorHub};

/// How often a shown device is looked at.
const POLL: Duration = Duration::from_secs(5);

/// One device's watch.
enum Scan {
    Sim(ReportScan),
    Android(BufferScan),
}

/// The watched devices (see the module docs).
#[derive(Default)]
pub(super) struct CrashWatch {
    scans: HashMap<DeviceId, Arc<Mutex<Scan>>>,
}

/// One device to look at, off the UI thread.
struct Job {
    udid: DeviceId,
    scan: Arc<Mutex<Scan>>,
    /// An Android device's `adb` and serial.
    adb: Option<(PathBuf, String)>,
}

impl SimulatorHub {
    /// The devices to look at now; the rest are forgotten.
    fn crash_jobs(&mut self) -> Vec<Job> {
        let shown = self.registry.shown_live();
        self.crash_watch.scans.retain(|udid, _| shown.contains(udid));
        let adb = self.android_sdk.as_ref().map(|sdk| sdk.adb());
        shown
            .into_iter()
            .filter_map(|udid| {
                let android = match udid.source() {
                    Source::Simctl => None,
                    Source::Adb => {
                        let serial = self.session(&udid)?.android()?.serial().to_owned();
                        Some((adb.clone()?, serial))
                    }
                    // A real iPhone's crashes are not readable from here.
                    Source::Devicectl => return None,
                };
                let scan = self.crash_watch.scans.entry(udid.clone()).or_insert_with(|| {
                    Arc::new(Mutex::new(if android.is_some() { Scan::Android(BufferScan::default()) } else { Scan::Sim(ReportScan::default()) }))
                });
                Some(Job { udid, scan: scan.clone(), adb: android })
            })
            .collect()
    }
}

/// Look at `job`'s device: its crashes since the last look.
fn look(job: &Job, home: &std::path::Path) -> Vec<Crash> {
    let mut scan = job.scan.lock().unwrap_or_else(|e| e.into_inner());
    match (&mut *scan, &job.adb) {
        (Scan::Sim(scan), _) => job.udid.sim_udid().map(|sim| scan.poll(home, sim.as_str())).unwrap_or_default(),
        (Scan::Android(scan), Some((adb, serial))) => match scan.poll(&Adb::new(&SystemRunner, adb), serial) {
            Ok(crashes) => crashes,
            Err(e) => {
                tracing::debug!(udid = %job.udid, "crash buffer unreadable: {e}");
                Vec::new()
            }
        },
        (Scan::Android(_), None) => Vec::new(),
    }
}

pub(super) fn spawn_crash_watch(cx: &mut App, hub: WeakEntity<SimulatorHub>) {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(POLL).await;
            let Ok(jobs) = hub.update(cx, |hub, _| hub.crash_jobs()) else { return };
            if jobs.is_empty() {
                continue;
            }
            let home = home.clone();
            let found = cx
                .background_executor()
                .spawn(async move { jobs.iter().flat_map(|job| look(job, &home).into_iter().map(|c| (job.udid.clone(), c))).collect::<Vec<_>>() })
                .await;
            let alive = hub.update(cx, |_, cx| {
                for (udid, crash) in found {
                    cx.emit(HubEvent::Crash(udid, crash));
                }
            });
            if alive.is_err() {
                return;
            }
        }
    })
    .detach();
}
