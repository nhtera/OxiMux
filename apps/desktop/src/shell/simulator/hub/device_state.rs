//! The hub's side of device state: applying a change (for an agent's verb,
//! once consent is settled, and for the panel's Device menu), erasing a
//! device — shut down, wiped, booted again through the registry so the panel
//! follows — and Android's demo-mode status bar, which must not outlive the
//! last worktree attached to the device.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use gpui::{Context, Task};
use oximux_simulator::android::adb::Adb;
use oximux_simulator::android::{Target, avd, devices};
use oximux_simulator::runner::Runner;
use oximux_simulator::state::StateChange;
use oximux_simulator::{DeviceId, Source, android, simctl_state};

use super::{NoticeKind, SimulatorHub};

/// One `adb` or `simctl` state call.
const STATE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long an erase waits for the device to shut down, and for `simctl
/// erase` itself.
const ERASE_WAIT: Duration = Duration::from_secs(60);

/// What the hub remembers about device state.
#[derive(Default)]
pub(crate) struct Memo {
    /// Emulators to boot with their data wiped (an erase in progress).
    wipe_next: HashSet<DeviceId>,
    /// Android devices showing demo mode's status bar, by serial: noted when
    /// it was turned on, as the device may have no session when it is left.
    demo: HashMap<DeviceId, String>,
    /// Devices being erased: nothing else boots them meanwhile.
    erasing: HashSet<DeviceId>,
}

impl SimulatorHub {
    /// Make `change` on `udid`, which streams (an Android device's serial is
    /// only known then). Resolves once it is done.
    pub(crate) fn apply_state(&mut self, udid: &DeviceId, change: StateChange, cx: &mut Context<Self>) -> Task<Result<(), String>> {
        let runner = self.runner.clone();
        let target = udid.clone();
        let job: Box<dyn FnOnce() -> Result<(), String> + Send> = match udid.source() {
            Source::Simctl => {
                let Some(sim) = udid.sim_udid().filter(|_| self.watch_gate().xcode_ok) else {
                    return Task::ready(Err("Xcode was not found".into()));
                };
                Box::new(move || simctl_state::apply(runner.as_ref(), &sim, &change, STATE_TIMEOUT).map_err(|e| e.to_string()))
            }
            Source::Adb => {
                let (Some(sdk), Some(android), Some(serial)) = (
                    self.android_sdk.clone(),
                    Target::from_id(udid),
                    self.session(udid).and_then(|s| s.android().map(|a| a.serial().to_owned())),
                ) else {
                    return Task::ready(Err("the device is not streaming yet".into()));
                };
                let demo = match &change {
                    StateChange::StatusBar { clean } => Some(*clean),
                    _ => None,
                };
                let adb_path = sdk.adb();
                let noted = serial.clone();
                let job = Box::new(move || {
                    let adb = Adb::new(runner.as_ref(), &adb_path);
                    android::state::apply(&adb, &serial, &android, &change, STATE_TIMEOUT).map_err(|e| e.to_string())
                });
                return cx.spawn(async move |this, cx| {
                    let done = cx.background_executor().spawn(async move { job() }).await;
                    if let (Ok(()), Some(on)) = (&done, demo) {
                        let _ = this.update(cx, |hub, _| {
                            if on {
                                hub.device_state.demo.insert(target, noted);
                            } else {
                                hub.device_state.demo.remove(&target);
                            }
                        });
                    }
                    done
                });
            }
            Source::Devicectl => return Task::ready(Err("a real iPhone's state is not changed from here".into())),
        };
        cx.background_executor().spawn(async move { job() })
    }

    /// [`Self::apply_state`] for the panel's Device menu: a failure is said
    /// in the panel.
    pub(crate) fn apply_state_from_panel(&mut self, udid: &DeviceId, change: StateChange, cx: &mut Context<Self>) {
        let applied = self.apply_state(udid, change, cx);
        let udid = udid.clone();
        cx.spawn(async move |this, cx| {
            if let Err(e) = applied.await {
                let _ = this.update(cx, |hub, cx| hub.notice(&udid, NoticeKind::Error, format!("The device did not change: {e}"), cx));
            }
        })
        .detach();
    }

    /// Erase `udid` and boot it again: shut down (after a recording is
    /// finalized), wipe, reconnect. Resolves once the wipe is done and the
    /// boot has started. A real device is never erased, and one erase at a
    /// time runs per device.
    pub(crate) fn erase(&mut self, udid: &DeviceId, cx: &mut Context<Self>) -> Task<Result<(), String>> {
        if udid.is_physical() {
            return Task::ready(Err("a real device is never erased by OxiMux".into()));
        }
        if self.device_state.erasing.contains(udid) {
            return Task::ready(Err("the device is already being erased".into()));
        }
        let runner = self.runner.clone();
        let wipe: Box<dyn FnOnce() -> Result<(), String> + Send> = match udid.source() {
            Source::Simctl => {
                let Some(sim) = udid.sim_udid().filter(|_| self.watch_gate().xcode_ok) else {
                    return Task::ready(Err("Xcode was not found".into()));
                };
                Box::new(move || simctl_state::erase(runner.as_ref(), &sim, ERASE_WAIT, ERASE_WAIT).map_err(|e| e.to_string()))
            }
            Source::Adb => {
                let (Some(sdk), Some(target)) = (self.android_sdk.clone(), Target::from_id(udid)) else {
                    return Task::ready(Err("the Android SDK was not found".into()));
                };
                // The emulator wipes itself as it boots (`-wipe-data`).
                Box::new(move || wait_stopped(runner.as_ref(), &sdk, &target))
            }
            Source::Devicectl => return Task::ready(Err("a real device is never erased by OxiMux".into())),
        };
        // One already off has nothing to shut down (and `simctl shutdown`
        // would fail on it).
        if self.watch.lock().unwrap().is_booted(udid) != Some(false) {
            self.shutdown_after_recording(udid, cx);
        }
        self.device_state.erasing.insert(udid.clone());
        let target = udid.clone();
        cx.spawn(async move |this, cx| {
            let wiped = cx.background_executor().spawn(async move { wipe() }).await;
            let done = this.update(cx, |hub, cx| {
                hub.device_state.erasing.remove(&target);
                hub.device_state.demo.remove(&target);
                let android = target.source() == Source::Adb;
                if wiped.is_ok() && android {
                    hub.device_state.wipe_next.insert(target.clone());
                }
                // Booted again either way: a failed wipe must not leave the
                // device off under its panel. After a wipe it is known to be
                // off, whatever the boot watcher last saw.
                match &wiped {
                    Ok(()) => hub.reconnect_as(&target, false, cx),
                    Err(_) => hub.reconnect(&target, cx),
                }
                // The boot that wipes an emulator takes the mark at once;
                // one still here had no boot (detached meanwhile, say), and
                // must not wipe some later one.
                let wiped = match wiped {
                    Ok(()) if android && hub.device_state.wipe_next.remove(&target) => {
                        Err("the emulator was not booted again, so it was not wiped".to_owned())
                    }
                    other => other,
                };
                if let Err(e) = &wiped {
                    hub.notice(&target, NoticeKind::Error, format!("The device was not erased: {e}"), cx);
                }
                wiped
            });
            done.unwrap_or_else(|_| Err("OxiMux closed during the erase".into()))
        })
    }

    /// Whether `udid` is being erased (a wake must not boot it meanwhile).
    pub(super) fn erasing(&self, udid: &DeviceId) -> bool {
        self.device_state.erasing.contains(udid)
    }

    /// Whether `udid`'s next boot wipes its data (taken once).
    pub(super) fn take_wipe(&mut self, udid: &DeviceId) -> bool {
        self.device_state.wipe_next.remove(udid)
    }

    /// Android devices in demo mode that no worktree is attached to any
    /// more (a detach, a switch to another device) get their real status bar
    /// back.
    pub(super) fn leave_unattached_demo_modes(&mut self, cx: &mut Context<Self>) {
        let attached = self.registry.attached_devices();
        let left: Vec<String> = self.device_state.demo.extract_if(|udid, _| !attached.contains(udid)).map(|(_, serial)| serial).collect();
        let Some(sdk) = self.android_sdk.clone().filter(|_| !left.is_empty()) else { return };
        let runner = self.runner.clone();
        cx.background_executor()
            .spawn(async move {
                let adb = Adb::new(runner.as_ref(), &sdk.adb());
                for serial in left {
                    if let Err(e) = android::state::exit_demo_mode(&adb, &serial, STATE_TIMEOUT) {
                        tracing::warn!("demo mode left on: {e}");
                    }
                }
            })
            .detach();
    }

    /// App quit: the serials of every device still in demo mode.
    pub(super) fn take_demo_serials(&mut self) -> Vec<String> {
        self.device_state.demo.drain().map(|(_, serial)| serial).collect()
    }
}

/// Until the emulator `target` has stopped (≤ [`ERASE_WAIT`]): gone from adb,
/// and its process gone too — one still saving its snapshot holds the AVD,
/// and the wiping boot would find it locked.
fn wait_stopped(runner: &dyn Runner, sdk: &android::sdk::Sdk, target: &Target) -> Result<(), String> {
    let deadline = Instant::now() + ERASE_WAIT;
    let held = |target: &Target| match (target, devices::avd_home()) {
        (Target::Avd(name), Some(home)) => avd::held(&home, name),
        _ => false,
    };
    while devices::serial_of(runner, sdk, target, Duration::from_secs(5)).map_err(|e| e.to_string())?.is_some() || held(target) {
        if Instant::now() >= deadline {
            return Err("the emulator did not shut down".into());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use oximux_simulator::registry::{Phase, WorktreeKey};
    use oximux_storage::SimApprovalRepo;

    use super::super::agent::Wake;
    use super::*;

    fn hub(cx: &mut gpui::TestAppContext) -> gpui::Entity<SimulatorHub> {
        let db = oximux_storage::open_memory().expect("db");
        cx.update(|cx| super::super::install_for_test(cx, oximux_storage::SettingsRepo::new(db.clone()), SimApprovalRepo::new(db)))
    }

    /// While a device is erased it is off on purpose: an agent's verb waits
    /// for the erase's own boot instead of booting it (which would make the
    /// wipe fail), and a second erase is refused.
    #[gpui::test]
    fn nothing_boots_a_device_while_it_is_erased(cx: &mut gpui::TestAppContext) {
        let hub = hub(cx);
        let udid = Target::Avd("Pixel".into()).id();
        hub.update(cx, |hub, cx| {
            drop(hub.registry.attach(WorktreeKey::from_path(Path::new("/nonexistent/w")), udid.clone(), true, Instant::now()));
            drop(hub.registry.device_shutdown(&udid));
            assert!(matches!(hub.registry.phase(&udid), Phase::Disconnected { .. }), "{:?}", hub.registry.phase(&udid));
            hub.device_state.erasing.insert(udid.clone());
            assert_eq!(hub.wake_for_agent(&udid, cx), Wake::Starting);
            assert!(matches!(hub.registry.phase(&udid), Phase::Disconnected { .. }), "not booted: {:?}", hub.registry.phase(&udid));
            let again = hub.erase(&udid, cx);
            assert_eq!(futures::FutureExt::now_or_never(again), Some(Err("the device is already being erased".into())));
        });
    }

    /// Demo mode is per device: it stays while any worktree is attached to
    /// the device, and is forgotten (left) with the last attachment.
    #[gpui::test]
    fn demo_mode_ends_with_the_last_attachment(cx: &mut gpui::TestAppContext) {
        let hub = hub(cx);
        let udid = DeviceId("adb:R58M123".into());
        let (a, b) = (Path::new("/nonexistent/a"), Path::new("/nonexistent/b"));
        hub.update(cx, |hub, cx| {
            for w in [a, b] {
                drop(hub.registry.attach(WorktreeKey::from_path(w), udid.clone(), true, Instant::now()));
            }
            hub.device_state.demo.insert(udid.clone(), "R58M123".into());
            hub.detach(a, cx);
            assert!(hub.device_state.demo.contains_key(&udid), "b is still attached");
            hub.detach(b, cx);
            assert!(hub.device_state.demo.is_empty());
        });
    }
}
